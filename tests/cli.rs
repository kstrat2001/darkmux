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
        .stdout(predicate::str::contains("machine"))
        .stdout(predicate::str::contains("profile"))
        .stdout(predicate::str::contains("lab"));
}

// (#1426) The top-level `profiles` verb retired into `profile list`.
#[test]
fn profile_list_lists_from_explicit_config() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path().join("profiles.json");
    fs::write(&p, fixture_json()).unwrap();
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.args(["profile", "list", "--profiles-file", p.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("fast"))
        .stdout(predicate::str::contains("deep"))
        .stdout(predicate::str::contains("(default)"));
}

#[test]
fn profile_list_errors_when_config_missing() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.args(["profile", "list", "--profiles-file", "/no/such/path.json"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("registry not found")
                .or(predicate::str::contains("no profile registry")),
        );
}

// (#1426) The retired top-level spellings now fail with an unknown-subcommand
// error (no compat alias — pre-2.0 clean removal).
#[test]
fn retired_top_level_profiles_verb_is_unknown() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.arg("profiles")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand").or(
            predicate::str::contains("unexpected argument"),
        ));
}

#[test]
fn retired_top_level_scan_verb_is_unknown() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.arg("scan")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand").or(
            predicate::str::contains("unexpected argument"),
        ));
}

#[test]
fn retired_top_level_pr_review_verb_is_unknown() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.arg("pr-review")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand").or(
            predicate::str::contains("unexpected argument"),
        ));
}

#[test]
fn retired_top_level_notebook_verb_is_unknown() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.arg("notebook")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand").or(
            predicate::str::contains("unexpected argument"),
        ));
}

/// (#1426 phase 2) The `skills` top-level verb retired — `init` is the one
/// setup/refresh verb (it refreshes the bundled darkmux-* skills on re-run,
/// and `darkmux doctor` flags stale ones). The spelling has NO compat alias,
/// so clap rejects it as an unknown subcommand.
#[test]
fn retired_top_level_skills_verb_is_unknown() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.arg("skills")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand").or(
            predicate::str::contains("unexpected argument"),
        ));
}

/// (#1426 ship-2) The `crew` family retired ENTIRELY: phase 2 promoted
/// `dispatch` to a top-level verb, and the crew REGISTRY dissolved (a crew is
/// a derived view of a mission's resourcing), taking the registry-read verbs
/// (`crew list`/`show`/`index`) with it. Every crew spelling — the bare family
/// and each old sub-verb — is now an unknown TOP-LEVEL verb with no compat
/// alias (pre-2.0 clean removal).
#[test]
fn retired_crew_family_is_unknown_entirely() {
    for args in [
        vec!["crew"],
        vec!["crew", "dispatch", "code-reviewer"],
        vec!["crew", "list"],
        vec!["crew", "show", "review-deep"],
        vec!["crew", "index", "status"],
    ] {
        let mut cmd = Command::cargo_bin("darkmux").unwrap();
        cmd.args(&args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("unrecognized subcommand").or(
                predicate::str::contains("unexpected argument"),
            ));
    }
}

/// (#1426, decision 17) The `lessons` top-level verb retired into the `memory`
/// family — every spelling, the bare family and each old sub-verb, is now an
/// unknown TOP-LEVEL verb with no compat alias (pre-2.0 clean removal). The
/// surface moved to `memory lesson <sub>`; see the companion test below.
#[test]
fn retired_lessons_family_is_unknown_entirely() {
    for args in [
        vec!["lessons"],
        vec!["lessons", "list"],
        vec!["lessons", "add", "--title", "t", "--body", "b"],
        vec!["lessons", "recall", "--term", "x"],
        vec!["lessons", "export"],
    ] {
        let mut cmd = Command::cargo_bin("darkmux").unwrap();
        cmd.args(&args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("unrecognized subcommand").or(
                predicate::str::contains("unexpected argument"),
            ));
    }
}

/// (#1426, decision 17) The replacement surface EXISTS: `memory` carries both
/// kinds, `memory lesson` keeps all seven of the retired family's sub-verbs
/// (behavior + flags unchanged — the verb moved, nothing else), and
/// `memory correction` is read-only (a `list` and no write verb, since
/// corrections are recorded by the review path, never hand-authored).
/// The retirement test above only proves the OLD spelling is gone; this proves
/// the new one landed, so a rename that dropped a sub-verb can't pass both.
#[test]
fn memory_family_carries_both_kinds() {
    let help = |args: &[&str]| -> String {
        let out = Command::cargo_bin("darkmux")
            .unwrap()
            .args(args)
            .arg("--help")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    let memory = help(&["memory"]);
    for kind in ["lesson", "correction"] {
        assert!(memory.contains(kind), "memory --help lists `{kind}`: {memory}");
    }

    let lesson = help(&["memory", "lesson"]);
    for sub in ["add", "list", "edit", "remove", "export", "import", "recall"] {
        assert!(
            lesson.contains(sub),
            "memory lesson --help keeps the retired family's `{sub}`: {lesson}"
        );
    }

    let correction = help(&["memory", "correction"]);
    assert!(correction.contains("list"), "{correction}");
    // Read-only by construction (#849): the review path records corrections as
    // flow notes, so a write verb here would be inventing a surface.
    for write_verb in ["add", "edit", "remove", "import"] {
        assert!(
            !correction.contains(&format!("  {write_verb} ")),
            "memory correction stays read-only — no `{write_verb}`: {correction}"
        );
    }
}

/// (#1465) The `lab` second-level surface regrouped: the flat plural-noun
/// leaves (`lab runs`/`lab workloads`/`lab fixtures`), the flat run leaves
/// (`lab inspect`/`lab compare`), the flat fixture-mutation leaves
/// (`lab register`/`lab unregister`), and the role-scoped snowflake
/// (`lab review-bench`) all retired into kind-families (`lab run {list,
/// inspect,compare}`, `lab workload list`, `lab fixture {list,register,
/// unregister}`) and the generalized `lab eval`. `lab` survives, so each is an
/// unknown SUB-verb within the surviving family. No compat alias (pre-2.0
/// clean removal).
#[test]
fn retired_lab_flat_subverbs_are_unknown() {
    for args in [
        vec!["lab", "runs"],
        vec!["lab", "workloads"],
        vec!["lab", "fixtures"],
        vec!["lab", "inspect", "some-run"],
        vec!["lab", "compare", "a", "b"],
        vec!["lab", "register", "/some/path"],
        vec!["lab", "unregister", "some-name"],
        vec!["lab", "review-bench"],
    ] {
        let mut cmd = Command::cargo_bin("darkmux").unwrap();
        cmd.args(&args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("unrecognized subcommand").or(
                predicate::str::contains("unexpected argument"),
            ));
    }
}

/// (#1465) The `--crew` flag on the review-eval path retired with the crew
/// family (#1426); it is now `--roster-profile`. clap rejects the old flag as
/// an unexpected argument (no compat alias).
#[test]
fn retired_crew_flag_on_lab_eval_is_unknown() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.args(["lab", "eval", "--funnel", "--crew", "review-funnel"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

/// (#1465) The replacement `lab` surface EXISTS: `lab --help` lists the new
/// kind-families, and each family's `--help` keeps its members. The retirement
/// test above only proves the OLD spellings are gone; this proves the new ones
/// landed, so a regroup that dropped a member can't pass both.
#[test]
fn lab_kind_families_carry_their_members() {
    let help = |args: &[&str]| -> String {
        let out = Command::cargo_bin("darkmux")
            .unwrap()
            .args(args)
            .arg("--help")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    let lab = help(&["lab"]);
    for family in ["run", "workload", "fixture", "notebook", "eval"] {
        assert!(lab.contains(family), "lab --help lists `{family}`: {lab}");
    }

    // `lab run` carries the recorded-run sub-verbs AND still takes a workload.
    let run = help(&["lab", "run"]);
    for sub in ["list", "inspect", "compare"] {
        assert!(run.contains(sub), "lab run --help keeps `{sub}`: {run}");
    }
    assert!(
        run.to_lowercase().contains("workload"),
        "lab run --help still names the workload positional: {run}"
    );

    let workload = help(&["lab", "workload"]);
    assert!(workload.contains("list"), "lab workload --help has `list`: {workload}");

    let fixture = help(&["lab", "fixture"]);
    for sub in ["list", "register", "unregister"] {
        assert!(fixture.contains(sub), "lab fixture --help keeps `{sub}`: {fixture}");
    }

    // `lab eval` takes a role positional (default pr-reviewer) and the renamed
    // roster flag.
    let eval = help(&["lab", "eval"]);
    assert!(eval.to_lowercase().contains("role"), "lab eval --help names the role positional: {eval}");
    assert!(eval.contains("--roster-profile"), "lab eval --help has --roster-profile: {eval}");
    // The retired `--crew` flag must be gone. The word may still appear in the
    // `--roster-profile` doc's "renamed from `--crew`" note, so assert the
    // FLAG-DEFINITION form (`--crew <`) is absent, not the bare substring.
    assert!(!eval.contains("--crew <"), "lab eval --help must not define the retired --crew flag: {eval}");
}

/// (#1426 ship-4) `mission run` retired — the coder pipeline runs through
/// `mission launch coder-phase`. `mission` survives (launch/finalize/abort/…),
/// so the error is an unknown SUB-verb WITHIN the surviving family. No compat
/// alias (pre-2.0 clean removal).
#[test]
fn retired_mission_run_subverb_is_unknown() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.args(["mission", "run", "some-mission"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand").or(
            predicate::str::contains("unexpected argument"),
        ));
}

/// (#1463) The `phase` top-level verb family retired ENTIRELY: `estimate` +
/// `review` + the `start`/`complete`/`abandon` lifecycle trio. Every spelling —
/// the bare family and each old sub-verb — is now an unknown TOP-LEVEL verb with
/// no compat alias (pre-2.0 clean removal). (`mission add-phase` is a DIFFERENT,
/// surviving verb — it is NOT `darkmux phase`; see the mission-surface test.)
#[test]
fn retired_phase_family_is_unknown_entirely() {
    for args in [
        vec!["phase"],
        vec!["phase", "estimate", "spec.json"],
        vec!["phase", "review"],
        vec!["phase", "start", "s1"],
        vec!["phase", "complete", "s1"],
        vec!["phase", "abandon", "s1"],
    ] {
        let mut cmd = Command::cargo_bin("darkmux").unwrap();
        cmd.args(&args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("unrecognized subcommand").or(
                predicate::str::contains("unexpected argument"),
            ));
    }
}

/// (#1463) `mission ship` retired (the frontier does git/gh by hand, then
/// `mission finalize`) and `mission close` renamed to `mission finalize`. Both
/// old spellings are now unknown SUB-verbs within the surviving `mission`
/// family. No compat alias (pre-2.0 clean removal).
#[test]
fn retired_mission_ship_and_close_subverbs_are_unknown() {
    for args in [
        vec!["mission", "ship", "some-mission"],
        vec!["mission", "close", "some-mission"],
    ] {
        let mut cmd = Command::cargo_bin("darkmux").unwrap();
        cmd.args(&args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("unrecognized subcommand").or(
                predicate::str::contains("unexpected argument"),
            ));
    }
}

/// (#1463) The replacement surface EXISTS: the `mission` family lists `finalize`
/// and `abort` (the two whole-mission terminals) and keeps `add-phase`, while
/// `ship`/`close` are gone. Proves the rename landed — a change that dropped
/// `finalize` or re-added `ship`/`close` can't pass both this and the
/// retirement test above.
#[test]
fn mission_family_has_finalize_abort_addphase_but_not_ship_close() {
    let out = Command::cargo_bin("darkmux")
        .unwrap()
        .args(["mission", "--help"])
        .output()
        .expect("mission --help runs");
    let help = String::from_utf8_lossy(&out.stdout);
    let mut verbs: Vec<String> = Vec::new();
    let mut in_commands = false;
    for line in help.lines() {
        if line.trim_start().starts_with("Commands:") {
            in_commands = true;
            continue;
        }
        if !in_commands {
            continue;
        }
        if line.trim().is_empty() || line.starts_with("Options:") {
            break;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 || indent > 3 {
            continue; // section header or a wrapped description line
        }
        if let Some(tok) = line.split_whitespace().next() {
            if tok.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
                verbs.push(tok.to_string());
            }
        }
    }
    for present in ["finalize", "abort", "add-phase"] {
        assert!(
            verbs.iter().any(|v| v == present),
            "mission help must list `{present}` (#1463); parsed verbs: {verbs:?}"
        );
    }
    for gone in ["ship", "close"] {
        assert!(
            !verbs.iter().any(|v| v == gone),
            "the `mission {gone}` verb must stay retired (#1463); parsed verbs: {verbs:?}"
        );
    }
}

/// (#1426 ship-4) VERBPAT drift guard: the `mission` family exposes NO `run`
/// subcommand after the collapse, but DOES keep `launch`. Anchored on the exact
/// command-column token so it never false-matches `mission launch` (the
/// two-word `mission run` anchor the ship-4 coverage directive names). Re-adding
/// `MissionCmd::Run` would list `run` in the help and fail this.
#[test]
fn mission_run_verb_absent_from_help_but_launch_present() {
    let out = Command::cargo_bin("darkmux")
        .unwrap()
        .args(["mission", "--help"])
        .output()
        .expect("mission --help runs");
    let help = String::from_utf8_lossy(&out.stdout);
    // Collect the command-name column under the "Commands:" section — the
    // verb token sits at a shallow (<=3 space) indent; wrapped description
    // lines sit deeper and are skipped, so we never read a description word.
    let mut in_commands = false;
    let mut verbs: Vec<String> = Vec::new();
    for line in help.lines() {
        if line.trim_start().starts_with("Commands:") {
            in_commands = true;
            continue;
        }
        if !in_commands {
            continue;
        }
        if line.trim().is_empty() || line.starts_with("Options:") {
            break;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 || indent > 3 {
            continue; // section header or a wrapped description line
        }
        if let Some(tok) = line.split_whitespace().next() {
            if tok.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
                verbs.push(tok.to_string());
            }
        }
    }
    assert!(
        verbs.iter().any(|v| v == "launch"),
        "mission help must list `launch`; parsed verbs: {verbs:?}"
    );
    assert!(
        !verbs.iter().any(|v| v == "run"),
        "the `mission run` verb must stay retired (#1426 ship-4); parsed verbs: {verbs:?}"
    );
}

// (#1860) `mission config list`/`show` wiring — help-level presence plus one
// real end-to-end invocation of each, isolated via `DARKMUX_CREW_DIR` so the
// user tier is empty and deterministic (the on-disk `templates/builtin/`
// tier still resolves from cwd, and the two embedded built-ins always
// resolve regardless of either).

#[test]
fn mission_help_lists_config() {
    let out = Command::cargo_bin("darkmux")
        .unwrap()
        .args(["mission", "--help"])
        .output()
        .expect("mission --help runs");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.lines().any(|l| l.trim_start().starts_with("config")),
        "mission help must list `config` (#1860); got:\n{help}"
    );
}

#[test]
fn mission_config_help_lists_list_and_show() {
    let out = Command::cargo_bin("darkmux")
        .unwrap()
        .args(["mission", "config", "--help"])
        .output()
        .expect("mission config --help runs");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("list"), "got:\n{help}");
    assert!(help.contains("show"), "got:\n{help}");
}

#[test]
fn mission_config_list_json_includes_the_two_embedded_builtins() {
    let tmp = TempDir::new().unwrap();
    let out = Command::cargo_bin("darkmux")
        .unwrap()
        // (merge-gate CONSIDER 5) `DARKMUX_HOME` isolates config.json +
        // profiles.json + mission-configs all at once (never the
        // operator's real `~/.darkmux`); `DARKMUX_LMS_BIN=/usr/bin/true`
        // means `lms ps --json` never shells to a real LMStudio.
        .env("DARKMUX_HOME", tmp.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "config", "list", "--json"])
        .output()
        .expect("mission config list --json runs");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let configs = v["configs"].as_array().expect("configs is an array");
    let ids: Vec<&str> = configs.iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"review"), "got ids: {ids:?}");
    assert!(ids.contains(&"coder-phase"), "got ids: {ids:?}");
    for c in configs {
        assert!(c.get("error").is_some(), "every row must carry an `error` key even when null");
    }
}

#[test]
fn mission_config_show_review_names_every_phase_and_flags_unconstructible_kinds() {
    let tmp = TempDir::new().unwrap();
    let out = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", tmp.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "config", "show", "review", "--json"])
        .output()
        .expect("mission config show review --json runs");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(v["id"], "review");
    let phase_ids: Vec<&str> =
        v["phases"].as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap()).collect();
    assert_eq!(phase_ids, vec!["investigate", "adjudicate", "report"]);
    // Every step review.json declares is a real registered kind (Tier 1 +
    // the review Tier 3 kinds) — none should be flagged unconstructible.
    for phase in v["phases"].as_array().unwrap() {
        for task in phase["tasks"].as_array().unwrap() {
            for step in task["steps"].as_array().unwrap() {
                assert_eq!(
                    step["constructible"], true,
                    "step {:?} in the built-in review config must be constructible",
                    step
                );
            }
        }
    }
}

#[test]
fn mission_config_show_unknown_id_exits_nonzero_with_hint() {
    let tmp = TempDir::new().unwrap();
    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", tmp.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "config", "show", "totally-not-a-real-config"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
}

/// (merge-gate MUST-FIX 3, end to end) `--param` applies as a launch
/// binding on the review route: with a `role_profiles` map naming a REAL
/// profile for `review-judge`, `show`'s JSON must report `provenance:
/// "role_profiles map"` (unmapped roles still resolve too) — this
/// specific test overrides via `--param` and checks the override wins.
#[test]
fn mission_config_show_review_param_override_applies_with_launch_override_provenance() {
    let tmp = TempDir::new().unwrap();
    let profiles_path = tmp.path().join("profiles.json");
    fs::write(
        &profiles_path,
        r#"{"profiles":{"deep":{"models":[{"id":"m-deep","n_ctx":8000}]}},"default_profile":"deep"}"#,
    )
    .unwrap();
    let out = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", tmp.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args([
            "mission",
            "config",
            "show",
            "review",
            "--json",
            "--profiles-file",
            profiles_path.to_str().unwrap(),
            "--param",
            "review-judge=deep",
        ])
        .output()
        .expect("runs");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["warnings"].as_array().unwrap().len(), 0, "a role the review config declares must not warn");
    let judge_role = v["phases"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["tasks"].as_array().unwrap())
        .find_map(|t| {
            let r = &t["role"];
            (r["role_id"] == "review-judge").then(|| r.clone())
        })
        .expect("review-judge task must be present");
    assert_eq!(judge_role["provenance"], "launch override (--param)");
    assert_eq!(judge_role["profile"], "deep");
}

/// (merge-gate MUST-FIX 3, end to end) `mission launch` never converts
/// `--param <role>=<profile>` into a binding for a NON-review-route config
/// (coder-phase's `--param role=<id>` is a different knob entirely). `show`
/// must neuter the override and warn, not silently apply it.
#[test]
fn mission_config_show_coder_phase_param_is_neutered_with_warning() {
    let tmp = TempDir::new().unwrap();
    let out = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", tmp.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args([
            "mission",
            "config",
            "show",
            "coder-phase",
            "--json",
            "--param",
            "coder=deep",
        ])
        .output()
        .expect("runs");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let warnings = v["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w.as_str().unwrap().contains("ignored")),
        "got warnings: {warnings:?}"
    );
    let coder_role = v["phases"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["tasks"].as_array().unwrap())
        .find_map(|t| {
            let r = &t["role"];
            (r["role_id"] == "coder").then(|| r.clone())
        })
        .expect("coder task must be present");
    assert_ne!(
        coder_role["provenance"], "launch override (--param)",
        "a non-review-route config must never claim the launch-override provenance from --param"
    );
}

/// (merge-gate CONSIDER 7) Mirrors `machine_status_explicit_bad_profiles_file_errors_loudly`:
/// an EXPLICIT `--profiles-file` that fails to load errors loudly. Only
/// the no-arg default degrades.
#[test]
fn mission_config_show_explicit_bad_profiles_file_errors_loudly() {
    let tmp = TempDir::new().unwrap();
    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", tmp.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args([
            "mission",
            "config",
            "show",
            "review",
            "--profiles-file",
            "/no/such/path.json",
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("registry not found")
                .or(predicate::str::contains("no profile registry"))
                .or(predicate::str::contains("profiles-file")),
        );
}

// (#1426 phase 3) `swap`, `status`, `model`, `fleet`, and `recommendations`
// all retired as top-level verbs with NO compat alias (pre-2.0 clean removal).
// `swap` (the second residency writer) is gone entirely; `status`/`model`/
// `fleet` folded into the `machine` family.
#[test]
fn retired_top_level_swap_verb_is_unknown() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.arg("swap").assert().failure().stderr(
        predicate::str::contains("unrecognized subcommand")
            .or(predicate::str::contains("unexpected argument")),
    );
}

#[test]
fn retired_top_level_status_verb_is_unknown() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.arg("status").assert().failure().stderr(
        predicate::str::contains("unrecognized subcommand")
            .or(predicate::str::contains("unexpected argument")),
    );
}

#[test]
fn retired_top_level_model_verb_is_unknown() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.arg("model").assert().failure().stderr(
        predicate::str::contains("unrecognized subcommand")
            .or(predicate::str::contains("unexpected argument")),
    );
}

#[test]
fn retired_top_level_fleet_verb_is_unknown() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.arg("fleet").assert().failure().stderr(
        predicate::str::contains("unrecognized subcommand")
            .or(predicate::str::contains("unexpected argument")),
    );
}

#[test]
fn retired_top_level_recommendations_verb_is_unknown() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.arg("recommendations").assert().failure().stderr(
        predicate::str::contains("unrecognized subcommand")
            .or(predicate::str::contains("unexpected argument")),
    );
}

// (#1426) The `machine` family is present. `machine status` (absorbs the
// retired `status`) shows the matching-profile line; bare `machine` routes to
// `machine status` (one code path, no separate overview render).
#[test]
fn machine_status_runs_with_explicit_profiles() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path().join("profiles.json");
    fs::write(&p, fixture_json()).unwrap();
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.env("DARKMUX_LMS_BIN", "/usr/bin/true");
    cmd.args(["machine", "status", "--profiles-file", p.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("darkmux-managed"))
        .stdout(predicate::str::contains("matches"));
}

#[test]
fn bare_machine_routes_to_status() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.env("DARKMUX_LMS_BIN", "/usr/bin/true");
    cmd.arg("machine")
        .assert()
        .success()
        .stdout(predicate::str::contains("darkmux-managed"));
}

/// (#1426) `machine status --json` emits the machine-readable shape the
/// frontier orchestrator parses: ownership groups plus the absorbed `status`
/// verb's `matching_profiles` + `registry` provenance keys.
#[test]
fn machine_status_json_carries_matching_profiles_and_registry_keys() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path().join("profiles.json");
    fs::write(&p, fixture_json()).unwrap();
    let out = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args([
            "machine",
            "status",
            "--json",
            "--profiles-file",
            p.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    for key in ["managed", "user_state", "matching_profiles", "registry"] {
        assert!(json.get(key).is_some(), "missing `{key}` in: {json}");
    }
    assert!(json["matching_profiles"].is_array());
}

/// (#1426 gate fix) An EXPLICIT `--profiles-file` that doesn't load errors
/// loudly — the retired `status` verb's behavior. Only the no-arg default
/// degrades to residents-without-match.
#[test]
fn machine_status_explicit_bad_profiles_file_errors_loudly() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.env("DARKMUX_LMS_BIN", "/usr/bin/true");
    cmd.args(["machine", "status", "--profiles-file", "/no/such/path.json"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("registry not found")
                .or(predicate::str::contains("no profile registry"))
                .or(predicate::str::contains("profiles-file")),
        );
}

/// Serve `count` canned HTTP responses on an ephemeral loopback port,
/// then stop. Returns the bound `host:port`.
fn canned_http_peer(status_line: &'static str, body: &'static str, count: usize) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for _ in 0..count {
            let Ok((mut stream, _)) = listener.accept() else { break };
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    addr
}

/// Register `id` at `addr` in a roster under a per-test DARKMUX_FLEET_FILE,
/// via the real `machine add` verb. Returns the roster file path.
fn roster_with_peer(tmp: &TempDir, id: &str, addr: &str) -> std::path::PathBuf {
    let fleet_file = tmp.path().join("fleet.json");
    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_FLEET_FILE", &fleet_file)
        .args(["machine", "add", id, "--address", addr])
        .assert()
        .success();
    fleet_file
}

/// (#1426 gate fix) A peer whose daemon answers but can't reach LMStudio
/// (`lms_unreachable: true`) must NOT render as a healthy-empty machine —
/// residents are UNKNOWN, not zero. Loud message, exit 2.
#[test]
fn machine_status_remote_degraded_peer_is_not_healthy_empty() {
    let addr = canned_http_peer("200 OK", r#"{"models":[],"lms_unreachable":true,"generated_at_ms":1}"#, 1);
    let tmp = TempDir::new().unwrap();
    let fleet_file = roster_with_peer(&tmp, "peer1", &addr);
    let out = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_FLEET_FILE", &fleet_file)
        .args(["machine", "status", "peer1"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "degraded peer must exit 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("UNKNOWN"), "must say residents unknown: {stderr}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("exclusively darkmux's"),
        "must NOT render the healthy-empty view: {stdout}"
    );
}

/// (#1426) A healthy peer read renders its residents partitioned by
/// ownership; `--json` carries the `machine_id` provenance key.
#[test]
fn machine_status_remote_happy_path_json_carries_machine_id() {
    let body = r#"{"models":[{"identifier":"darkmux:qwen-x","model":"qwen-x","status":"loaded","size":"4 GB","context":32000}],"lms_unreachable":false,"generated_at_ms":1}"#;
    let addr = canned_http_peer("200 OK", body, 1);
    let tmp = TempDir::new().unwrap();
    let fleet_file = roster_with_peer(&tmp, "peer1", &addr);
    let out = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_FLEET_FILE", &fleet_file)
        .args(["machine", "status", "peer1", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["machine_id"], "peer1");
    assert_eq!(json["managed"][0]["identifier"], "darkmux:qwen-x");
}

/// (#1426) A peer payload whose `models` doesn't parse (older/newer daemon
/// shape) falls back to a raw JSON print — never a fabricated-empty render.
#[test]
fn machine_status_remote_shape_mismatch_prints_raw_json() {
    let addr = canned_http_peer("200 OK", r#"{"future_shape":{"models_v2":[]}}"#, 1);
    let tmp = TempDir::new().unwrap();
    let fleet_file = roster_with_peer(&tmp, "peer1", &addr);
    let out = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_FLEET_FILE", &fleet_file)
        .args(["machine", "status", "peer1"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("future_shape"), "raw payload passthrough: {stdout}");
    assert!(!stdout.contains("darkmux-managed"), "no fabricated render: {stdout}");
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
/// **Requires Docker** (#1405 removed the legacy openclaw shell-out runtime,
/// which this test previously mocked via `--runtime-cmd /usr/bin/true` to
/// avoid needing a real backend in CI). The internal runtime is now the only
/// dispatch path and it always spawns a real container, so this test needs
/// `darkmux-runtime:latest` built locally — matches the
/// `mock_dispatch_proof` test's "Docker required → `#[ignore]`d by default"
/// convention so `cargo test --workspace` never requires Docker. Run
/// explicitly with:
///
/// ```sh
/// cargo test --test cli lab_run_quick_q_from_clean_cwd_uses_embedded_workload -- --ignored
/// ```
#[test]
#[ignore]
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
        .arg("lab")
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
        .arg("lab")
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
        .arg("lab")
        .arg("notebook")
        .arg("list")
        .arg("--machine")
        .arg("nonexistent")
        .assert()
        .success()
        .stdout(predicate::str::contains("no notebook entries found"));
}

/// (#895) `lab notebook list` with an absent notebook dir exits 0 — "nothing
/// to list" is success (fresh user / chaining), not an error. (#1426 — the
/// notebook family folded into `lab`.)
#[test]
fn notebook_list_no_dir() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.arg("lab")
        .arg("notebook")
        .arg("list")
        .env("DARKMUX_NOTEBOOK_DIR", "/no/such/path/xyz")
        .assert()
        .success()
        .stdout(predicate::str::contains("no notebook directory yet"));
}

/// (#1426) `external` retired entirely — the pipe is the interface (any text
/// on stdin into `mission propose`). The old top-level verb now fails with an
/// unknown-subcommand error (no compat alias — pre-2.0 clean removal).
#[test]
fn retired_top_level_external_verb_is_unknown() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.arg("external")
        .arg("pull")
        .arg("--stdin")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand").or(
            predicate::str::contains("unexpected argument"),
        ));
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
            "lab",
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
        "lab",
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

// ─── (#491) Phase 4 lab CLI verbs: register / unregister / fixtures / doctor ──

/// Operator runs `dm lab fixture register <path>` against a fixture dir with
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
    cmd.args(["lab", "fixture", "register", fixture_dir.to_str().unwrap()])
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
        .args(["lab", "fixture", "register", fixture_dir.to_str().unwrap()])
        .assert()
        .success();

    // Now list.
    Command::cargo_bin("darkmux")
        .unwrap()
        .current_dir(tmp.path())
        .args(["lab", "fixture", "list"])
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
        .args(["lab", "fixture", "register", fixture_dir.to_str().unwrap()])
        .assert()
        .success();

    Command::cargo_bin("darkmux")
        .unwrap()
        .current_dir(tmp.path())
        .args(["lab", "fixture", "unregister", "demo"])
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
    assert!(stdout.contains("lab-init.sh") || stdout.contains("dm lab fixture register"), "got: {stdout}");
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
        .args(["lab", "fixture", "register", fixture_dir.to_str().unwrap()])
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
        .args(["lab", "fixture", "register", fixture_dir.to_str().unwrap()])
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
        stdout.contains("dm lab fixture register --force"),
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
        .args(["lab", "fixture", "register", &fixture_path])
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
        .args(["lab", "fixture", "register", &fixture_path])
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

/// (#1426 / #386) `darkmux dispatch <role> [MESSAGE]` sources the message
/// from a positional argument, `--message-from-file`, or stdin. The positional
/// and the file flag are mutually exclusive, and the file is resolved at the
/// top of the handler — all before any container work, so these need no
/// docker / model.
#[test]
fn dispatch_message_source_contract() {
    // Mutual exclusion: positional MESSAGE AND --message-from-file → clap
    // rejects. (Proves the positional exists and conflicts with the file flag.)
    Command::cargo_bin("darkmux")
        .unwrap()
        .args([
            "dispatch",
            "code-reviewer",
            "inline",
            "--message-from-file",
            "/tmp/whatever",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));

    // Missing --message-from-file → resolved early, fails loud BEFORE any
    // dispatch setup (the message is resolved at the top of the handler, ahead
    // of out-dir creation / container spawn).
    Command::cargo_bin("darkmux")
        .unwrap()
        .args([
            "dispatch",
            "code-reviewer",
            "--message-from-file",
            "/nonexistent/darkmux-1426/brief.md",
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("message-from-file")
                .and(predicate::str::contains("out-dir").not())
                .and(predicate::str::contains("spawning").not()),
        );
}

/// (#2265) `dispatch --finding <key>` refuses a key that addresses no stored
/// finding, BEFORE any dispatch setup — a silently missing brief would send
/// the role to work on an observation it never saw. The refusal names the
/// second producer that can fill the store.
#[test]
fn dispatch_finding_refuses_a_key_with_no_stored_record() {
    let store = TempDir::new().unwrap(); // empty store
    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_FINDINGS_DIR", store.path())
        .args(["dispatch", "health-research", "--finding", "sess-x/9", "smoke"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("no finding sess-x/9")
                .and(predicate::str::contains("darkmux finding sync"))
                // Refused ahead of the dispatch: the ACK gate never ran, and
                // nothing reached docker.
                .and(predicate::str::contains("requires operator acknowledgment").not())
                .and(predicate::str::contains("docker").not()),
        );

    // A key of the wrong SHAPE is refused with the form it should have.
    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_FINDINGS_DIR", store.path())
        .args(["dispatch", "health-research", "--finding", "not-a-key", "smoke"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("<dispatch>/<seq>"));
}

/// (#2295) The same refusal rule for the second record kind: `dispatch --mod
/// <key>` with a key that addresses no stored mod refuses BEFORE any dispatch
/// setup. Proven by ABSENCE — the ACK gate this role would otherwise hit, and
/// any docker work, must both be unreached.
#[test]
fn dispatch_mod_refuses_a_key_with_no_stored_record() {
    let store = TempDir::new().unwrap(); // empty store
    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_MODS_DIR", store.path())
        .args(["dispatch", "health-research", "--mod", "mod-1-nope", "smoke"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("no mod mod-1-nope")
                .and(predicate::str::contains("darkmux mod list"))
                .and(predicate::str::contains("requires operator acknowledgment").not())
                .and(predicate::str::contains("docker").not()),
        );

    // A key that could escape the store is refused as a key, never read.
    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_MODS_DIR", store.path())
        .args(["dispatch", "health-research", "--mod", "../etc", "smoke"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("is not a mod key"));
}

/// (#2265) A key that DOES address a stored finding is loaded and the dispatch
/// proceeds — proven at the ACK gate, which bails before any Docker work.
/// `--finding` is repeatable, and both keys resolve.
#[test]
fn dispatch_finding_loads_a_stored_record_and_proceeds() {
    let store = TempDir::new().unwrap();
    for (dispatch, seq) in [("sess-x", 1u64), ("sess-y", 2)] {
        let dir = store.path().join(dispatch).join(seq.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("finding.json"),
            serde_json::json!({
                "key": format!("{dispatch}/{seq}"),
                "dispatch": dispatch,
                "seq": seq,
                "ts": "2026-09-03T00:00:00Z",
                "tool_name": "create_finding",
                "proposer": {"handle": "crawler", "model": "m"},
                "context": {"unit": "u7"},
                "emitted": {"file": "src/x.ts", "line": 82, "why": "three unnamed operands"},
                "schema_version": "1"
            })
            .to_string(),
        )
        .unwrap();
    }
    let ack_dir = TempDir::new().unwrap();
    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_FINDINGS_DIR", store.path())
        .env("DARKMUX_ACK_DIR", ack_dir.path())
        .args([
            "dispatch", "health-research", "--finding", "sess-x/1", "--finding", "sess-y/2",
            "smoke",
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("requires operator acknowledgment")
                .and(predicate::str::contains("no finding").not()),
        );
}

/// (#2265 review, IMPORTANT 4 + CRITICAL 8) The END-TO-END pin for
/// `--finding`, through the path a real `darkmux dispatch` takes: the CLI ->
/// the crew-of-one graph (`dispatch_as_crew_of_one::build_graph`) -> the step
/// kind -> `dispatch_internal`, with the assertions made on the `dispatch
/// start` FLOW RECORD that dispatch actually wrote.
///
/// Both halves matter and neither was covered before. The earlier CLI tests
/// asserted only that a good key reaches the ACK gate and a bad one is refused
/// — nothing about the brief or the record — so the append could be deleted
/// and they stayed green, and the crew-of-one graph could drop the keys (it
/// did) with nothing to catch it. Here: `prompt_chars` must exceed the
/// operator's own message, proving the finding block was appended, and
/// `brief_refs` must name the records, proving the hand-off survived every
/// hop.
///
/// (#2295) Extended to BOTH record kinds in one dispatch: a finding and a mod,
/// in the order given, with the mod's attachment named by the container path
/// it is mounted at.
#[test]
fn dispatch_finding_reaches_the_flow_record_with_the_brief_and_the_keys() {
    let stub = RespondingStubServer::start();
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let findings = TempDir::new().unwrap();
    let mods = TempDir::new().unwrap();

    let profiles_path = home.path().join("profiles.json");
    fs::write(&profiles_path, responding_endpoint_profiles_json(stub.port)).unwrap();

    // A finding in the store, with a distinctive marker the brief must carry.
    let dir = findings.path().join("sess-pin").join("4");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("finding.json"),
        serde_json::json!({
            "key": "sess-pin/4", "dispatch": "sess-pin", "seq": 4,
            "ts": "2026-09-03T00:00:00Z", "tool_name": "create_finding",
            "proposer": {"handle": "crawler", "model": "m"},
            "context": {"unit": "u7"},
            "emitted": {"file": "src/x.ts", "line": 82, "why": "MARKER-three-unnamed-operands"},
            "schema_version": "1"
        })
        .to_string(),
    )
    .unwrap();

    // A mod in the store, with its own distinctive kit and one attachment.
    let mod_dir = mods.path().join("mod-9-pin");
    fs::create_dir_all(mod_dir.join("attachments")).unwrap();
    fs::write(mod_dir.join("attachments").join("fix.patch"), b"body").unwrap();
    fs::write(
        mod_dir.join("mod.json"),
        serde_json::json!({
            "key": "mod-9-pin", "ts": "2026-09-04T00:00:00Z", "by": "sonnet",
            "for": ["sess-pin/4"],
            "kit": "MARKER-name-the-three-operands",
            "kit_looks_json": false,
            "attachments": ["fix.patch"],
            "context": {"findings": []},
            "schema_version": "1"
        })
        .to_string(),
    )
    .unwrap();

    let message = "fix it";
    // `review-judge` is TOOL-LESS, so this takes the light single-shot hosted
    // path (a host `curl` to the stub) rather than a `darkmux-runtime`
    // container — no Docker, no image, no model.
    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_PROFILES", &profiles_path)
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_FINDINGS_DIR", findings.path())
        .env("DARKMUX_MODS_DIR", mods.path())
        .env("DARKMUX_REDIS_URL", "")
        .args([
            "dispatch", "review-judge", "--finding", "sess-pin/4", "--mod", "mod-9-pin",
            "--skip-preflight", message,
        ])
        .assert()
        .success();

    let mut start: Option<serde_json::Value> = None;
    for entry in fs::read_dir(flows.path()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        for line in fs::read_to_string(&path).unwrap().lines() {
            let Ok(rec) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            if rec["action"] == "dispatch start" {
                start = Some(rec);
            }
        }
    }
    let start = start.expect("a `dispatch start` flow record");
    let payload = &start["payload"];
    assert_eq!(
        payload["brief_refs"],
        serde_json::json!([
            {"kind": "finding", "key": "sess-pin/4"},
            {"kind": "mod", "key": "mod-9-pin"},
        ]),
        "both refs must survive the CLI -> crew-of-one -> step-kind hand-off, \
         in the order given: {start}"
    );
    let prompt_chars = payload["prompt_chars"].as_u64().expect("prompt_chars");
    assert!(
        prompt_chars > message.chars().count() as u64 + 200,
        "the finding block must be IN the brief (prompt_chars {prompt_chars} is barely \
         longer than the operator's own {} chars): {start}",
        message.chars().count()
    );
    let prompt = payload["prompt"].as_str().unwrap_or_default();
    assert!(
        prompt.contains("MARKER-three-unnamed-operands"),
        "the record's own prompt carries the finding's emission verbatim: {start}"
    );
    assert!(
        prompt.contains("MARKER-name-the-three-operands"),
        "and the mod's kit, byte-exact: {start}"
    );
    assert!(
        prompt.find("MARKER-three-unnamed-operands") < prompt.find("MARKER-name-the-three-operands"),
        "the blocks follow the order the refs were given: {start}"
    );
    assert!(
        prompt.contains("/darkmux-mods/mod-9-pin/attachments/fix.patch"),
        "the mod block names its attachment by the container path it is mounted at: {start}"
    );
    // (#2295 review, CRITICAL 1) EXACTLY once. Resolution moved from the CLI
    // down to the step kind so a mission graph gets its blocks too; if the CLI
    // kept appending as well, a `darkmux dispatch --finding` would send the
    // model the same record twice and nothing above would notice.
    assert_eq!(
        prompt.matches("MARKER-three-unnamed-operands").count(),
        1,
        "the finding block is appended exactly once: {start}"
    );
    assert_eq!(
        prompt.matches("MARKER-name-the-three-operands").count(),
        1,
        "and the mod block exactly once: {start}"
    );
    assert_eq!(
        prompt.matches("<mod key=\"mod-9-pin\">").count(),
        1,
        "one mod block, not two: {start}"
    );
}

/// (#2295 review, CRITICAL 1) The refs cannot ride the fleet work queue — its
/// job shape has no field for them and the peer's stores are its own — so a
/// remote `--machine` dispatch that names one is refused rather than routed
/// with its blocks silently missing. Refused BEFORE the ack gate, like every
/// other brief-ref refusal.
#[test]
fn dispatch_refuses_a_record_ref_routed_to_another_machine() {
    let store = TempDir::new().unwrap();
    let dir = store.path().join("sess-r").join("1");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("finding.json"),
        serde_json::json!({
            "key": "sess-r/1", "dispatch": "sess-r", "seq": 1,
            "ts": "2026-09-04T00:00:00Z", "tool_name": "create_finding",
            "proposer": {"handle": "h", "model": "m"},
            "context": {}, "emitted": {"why": "x"}, "schema_version": "1"
        })
        .to_string(),
    )
    .unwrap();

    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_FINDINGS_DIR", store.path())
        .env("DARKMUX_MACHINE_ID", "this-one")
        .args([
            "dispatch", "health-research", "--finding", "sess-r/1", "--machine", "some-other-mac",
            "smoke",
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("cannot be routed to another machine")
                .and(predicate::str::contains("requires operator acknowledgment").not()),
        );
}

/// (#1426) The POSITIONAL message reaches the dispatch path. `health-research`
/// is licensed-adjacent, so its ACK gate bails BEFORE any Docker work — a
/// CI-safe way to prove the positional message was accepted and routed without
/// a real model.
#[test]
fn dispatch_positional_message_reaches_ack_gate() {
    let ack_dir = TempDir::new().unwrap(); // empty — no prior ack on file
    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_ACK_DIR", ack_dir.path())
        .args(["dispatch", "health-research", "smoke"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("requires operator acknowledgment")
                // The positional was consumed — no "no message given" guard,
                // no docker.
                .and(predicate::str::contains("no message given").not())
                .and(predicate::str::contains("runtime=internal").not())
                .and(predicate::str::contains("docker").not()),
        );
}

/// (#1426) When the positional MESSAGE is omitted, the message is read from
/// stdin (pipe composition: `git diff | darkmux dispatch pr-reviewer`). Piping
/// a message to `health-research` proves the stdin channel drives the message
/// (no TTY-absent error fires) and the dispatch reaches the ACK gate, which
/// bails before Docker. CI-safe: `write_stdin` makes stdin a non-TTY pipe, the
/// path the byte-faithful `read_to_string` consumes.
#[test]
fn dispatch_stdin_message_reaches_ack_gate() {
    let ack_dir = TempDir::new().unwrap();
    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_ACK_DIR", ack_dir.path())
        .args(["dispatch", "health-research"])
        .write_stdin("smoke from stdin")
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("requires operator acknowledgment")
                // The TTY-absent guard did NOT fire (stdin was a pipe with
                // content), and no docker work happened.
                .and(predicate::str::contains("no message given").not())
                .and(predicate::str::contains("runtime=internal").not())
                .and(predicate::str::contains("docker").not()),
        );
}

/// (#1426) Empty piped stdin bails LOUDLY with its own error — distinct from
/// the terminal-guard's "no message given" text — instead of dispatching a
/// blank brief (an empty `git diff |` is the most common accident). The
/// dispatch never starts: no ACK-gate text, no docker.
#[test]
fn dispatch_empty_stdin_bails_loudly() {
    let ack_dir = TempDir::new().unwrap();
    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_ACK_DIR", ack_dir.path())
        .args(["dispatch", "health-research"])
        .write_stdin("") // empty pipe → loud bail, not a blank dispatch
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("stdin was empty")
                // Distinct from the TTY-absent guard's error.
                .and(predicate::str::contains("no message given").not())
                // Bailed before any dispatch machinery.
                .and(predicate::str::contains("requires operator acknowledgment").not())
                .and(predicate::str::contains("docker").not()),
        );
}

/// (#1426) A whitespace-only pipe (`echo |` produces a lone "\n" — the second
/// most common accident) gets the same loud empty-stdin bail. The emptiness
/// check trims for the CHECK only; a message with real content is still
/// delivered byte-faithfully (covered by dispatch_stdin_message_reaches_ack_gate).
#[test]
fn dispatch_whitespace_only_stdin_bails_loudly() {
    let ack_dir = TempDir::new().unwrap();
    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_ACK_DIR", ack_dir.path())
        .args(["dispatch", "health-research"])
        .write_stdin("\n")
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("stdin was empty")
                .and(predicate::str::contains("requires operator acknowledgment").not()),
        );
}

/// (#1426) An empty (or whitespace-only) --message-from-file gets the same
/// trim-empty bail for consistency, with a distinct error naming the file
/// path — resolved at the top of the handler, before any dispatch setup.
#[test]
fn dispatch_empty_message_file_bails_loudly() {
    let tmp = TempDir::new().unwrap();
    let brief = tmp.path().join("blank-brief.md");
    fs::write(&brief, "  \n\n").unwrap();
    Command::cargo_bin("darkmux")
        .unwrap()
        .args([
            "dispatch",
            "code-reviewer",
            "--message-from-file",
            brief.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("is empty")
                .and(predicate::str::contains("blank-brief.md"))
                .and(predicate::str::contains("docker").not()),
        );
}

/// (#1405 gate remediation, relocated to top-level `dispatch` in #1426) The
/// licensed-adjacent ACK gate fires on the internal dispatch path BEFORE any
/// Docker work. This pins the moved-but-unwired regression class structurally:
/// a non-TTY dispatch of `health-research` with no prior ack must bail at the
/// gate — no Docker preflight, no container spawn — so the test needs no
/// Docker and is CI-safe.
#[test]
fn dispatch_licensed_adjacent_role_bails_at_ack_gate_before_docker() {
    let ack_dir = TempDir::new().unwrap(); // empty — no prior ack on file
    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_ACK_DIR", ack_dir.path())
        .args(["dispatch", "health-research", "smoke"])
        // assert_cmd pipes stdin (not a TTY), so the gate's non-interactive
        // arm bails rather than prompting for ACKNOWLEDGE.
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("requires operator acknowledgment")
                // Bailed BEFORE the Docker preflight / container spawn: the
                // "runtime=internal — image:" line prints only after the
                // gate, and no docker error can have surfaced.
                .and(predicate::str::contains("runtime=internal").not())
                .and(predicate::str::contains("docker").not()),
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
    // (#1521-adjacent UX) The local-judge marker is no longer repeated on
    // each finding's own comment — it renders once, on the review's
    // top-level body, alongside the verdict line.
    let top_body = v["review"]["body"].as_str().unwrap();
    assert!(
        top_body.contains("needs frontier verification"),
        "the review header carries the local-judge marker once: {top_body}"
    );
}

// (#2310 P0) Render-side half of the review conformance harness — see
// `crates/darkmux-lab/tests/review_conformance.rs`'s module doc for the
// pipeline-side half. This exercises `pr_review::synthesize_review` at the
// CLI boundary (`mission launch review --param from_envelope=... --param
// diff_file=... --param emit=-` — the CI-testable, zero-model-call,
// zero-bundling synthesis-only path; there is no bare `pr-review render`
// top-level verb — see `retired_top_level_pr_review_verb_is_unknown` above,
// that spelling was retired and this is the real one) against the SAME
// `ReviewEnvelope` the conformance harness's graph run produced and pinned
// as its own golden — read directly from
// `crates/darkmux-lab/tests/golden/review-conformance/envelope.json` (Hole
// 5, #2336 review: this used to be a byte-copy at
// `tests/fixtures/review-conformance/envelope.json` with nothing asserting
// the two stayed in sync; single-sourced now, one file on disk, not two).
// Together the two goldens (this render golden + the crate's pipeline
// golden) cover both halves of the bespoke `src/mission_launch_review.rs`
// launcher #2310 P4 will retire: build+run the graph (pipeline golden) and
// render the resulting envelope into a postable payload (this golden).
mod review_conformance {
    use super::*;
    use std::path::PathBuf;

    /// (Hole 5, #2336 review) The CLI-only fixtures (the render golden —
    /// no crate-side equivalent exists for it).
    fn fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/review-conformance")
    }

    /// (Hole 5, #2336 review) `envelope.json` and `diff.patch` used to be
    /// byte-copies duplicated under `tests/fixtures/review-conformance/`,
    /// with nothing asserting the two copies stayed in sync — a hand-edit
    /// to one drifts from the other silently. Single-sourced now: this
    /// reads the SAME files `crates/darkmux-lab/tests/review_conformance.rs`
    /// pins as its own golden/fixture, so there is exactly one envelope and
    /// one diff on disk, not two that happen to match today.
    fn crate_fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("crates/darkmux-lab/tests/fixtures/review-conformance")
    }

    fn crate_golden_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("crates/darkmux-lab/tests/golden/review-conformance")
    }

    /// Run the CLI's `from_envelope` synthesis path over the committed
    /// envelope + diff fixtures and return the parsed `{mode, review,
    /// comment}` stdout payload. (Hole 5, #2336 review) `DARKMUX_HOME`
    /// scopes to a per-call tempdir — mirrors the 26+ other `cli.rs` tests
    /// that set it — so a user-tier `~/.darkmux/mission-configs/review.json`
    /// on the box running this test cannot change what gets rendered.
    fn render_conformance_envelope() -> serde_json::Value {
        let home = TempDir::new().unwrap();
        let output = Command::cargo_bin("darkmux")
            .unwrap()
            .env("DARKMUX_HOME", home.path())
            .args([
                "mission",
                "launch",
                "review",
                "--param",
                &format!("from_envelope={}", crate_golden_dir().join("envelope.json").to_str().unwrap()),
                "--param",
                &format!("diff_file={}", crate_fixture_dir().join("diff.patch").to_str().unwrap()),
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
        serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {stdout}"))
    }

    fn golden_path() -> PathBuf {
        fixture_dir().join("rendered.golden.json")
    }

    /// The render golden itself: the conformance envelope, rendered,
    /// pinned byte-for-byte. To regenerate after a deliberate, reviewed
    /// rendering change: `DARKMUX_REVIEW_CONFORMANCE_UPDATE_GOLDEN=1 cargo
    /// test --test cli review_conformance::` then review the diff before
    /// committing.
    #[test]
    fn rendered_payload_matches_the_committed_golden() {
        let v = render_conformance_envelope();
        let pretty = serde_json::to_string_pretty(&v).unwrap();

        if std::env::var("DARKMUX_REVIEW_CONFORMANCE_UPDATE_GOLDEN").is_ok() {
            fs::write(golden_path(), format!("{pretty}\n")).unwrap();
            return;
        }
        let expected = fs::read_to_string(golden_path()).unwrap_or_else(|_| {
            panic!(
                "missing golden at {} — run with DARKMUX_REVIEW_CONFORMANCE_UPDATE_GOLDEN=1 to generate it",
                golden_path().display()
            )
        });
        assert_eq!(
            pretty.trim_end(),
            expected.trim_end(),
            "the review conformance envelope's RENDERED payload drifted from the committed golden \
             at {} — this is the #2310 refactor's regression net; if the drift is intended, \
             regenerate with DARKMUX_REVIEW_CONFORMANCE_UPDATE_GOLDEN=1 cargo test --test cli \
             review_conformance:: then review the diff before committing.",
            golden_path().display()
        );
    }

    /// Sanity checks on the render's SHAPE (not just the opaque golden
    /// diff) — fails loud + legibly if the fixture stops exercising what
    /// it claims to. Mirrors `bundle_golden.rs`'s convention of pairing a
    /// byte-golden with independent shape assertions.
    #[test]
    fn rendered_payload_has_the_expected_shape() {
        let v = render_conformance_envelope();
        assert_eq!(v["mode"], "review");
        assert_eq!(v["review"]["event"], "COMMENT", "advisory by default — the fixture's `request_changes` is false");
        let comments = v["review"]["comments"].as_array().expect("comments array");
        assert_eq!(comments.len(), 1, "exactly the one CONFIRMED (billing.ts) finding posts a comment");
        assert_eq!(comments[0]["path"], "src/billing.ts");
        let body = comments[0]["body"].as_str().unwrap();
        assert!(body.contains("numeric-add bug"), "{body}");
    }

    /// (Hole 5, #2336 review) Positive proof the `DARKMUX_HOME` scoping in
    /// `render_conformance_envelope` is real containment, not decoration.
    /// `mission launch review`'s liveness floor
    /// (`darkmux_types::dispatch_liveness::liveness_dir`) writes a heartbeat
    /// file straight to `<DARKMUX_HOME>/liveness/<pid>.log` with NO config
    /// load in between (its own doc: "resolves the darkmux home WITHOUT
    /// touching config resolution"), and falls back to the real `~/.darkmux`
    /// when `DARKMUX_HOME` is unset. Asserting the heartbeat file lands
    /// inside OUR tempdir is the closest this suite can get to red-proving
    /// the scoping without ever actually letting an unscoped run touch the
    /// operator's real home (which this harness's own rules forbid) —
    /// dropping `.env("DARKMUX_HOME", ...)` here would make this assertion
    /// fail (no `liveness/` dir under the tempdir at all) while silently
    /// writing the heartbeat to the real `~/.darkmux/liveness/` instead.
    #[test]
    fn render_conformance_envelope_confines_its_liveness_heartbeat_to_the_scoped_home() {
        let home = TempDir::new().unwrap();
        let output = Command::cargo_bin("darkmux")
            .unwrap()
            .env("DARKMUX_HOME", home.path())
            .args([
                "mission",
                "launch",
                "review",
                "--param",
                &format!("from_envelope={}", crate_golden_dir().join("envelope.json").to_str().unwrap()),
                "--param",
                &format!("diff_file={}", crate_fixture_dir().join("diff.patch").to_str().unwrap()),
                "--param",
                "emit=-",
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "stderr={}", String::from_utf8_lossy(&output.stderr));
        let liveness_dir = home.path().join("liveness");
        let entries: Vec<_> = fs::read_dir(&liveness_dir)
            .unwrap_or_else(|e| panic!("expected a liveness heartbeat under the scoped DARKMUX_HOME at {}: {e}", liveness_dir.display()))
            .collect();
        assert!(!entries.is_empty(), "the liveness heartbeat file must exist under the scoped home");
    }
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

/// (#1475 packet 2) A real (non `from_envelope`) run on a registry with a
/// profile but NO `default_profile`, and no role→profile bindings (a bare
/// DARKMUX_HOME with no `role_profiles` config), fails loud at role→profile
/// resolution: an UNMAPPED review role has no `default_profile` floor to fall
/// back to. Loud + named, before any bundling/dispatch happens.
#[test]
fn pr_review_run_no_profile_binding_or_default_errors_loudly() {
    let tmp = TempDir::new().unwrap();
    let diff_path = tmp.path().join("pr.diff");
    fs::write(&diff_path, pr_review_run_diff()).unwrap();
    let profiles = tmp.path().join("profiles.json");
    // A valid registry with a profile but NO default_profile — and no
    // role_profiles bindings, so every review role is unmapped with no floor.
    fs::write(
        &profiles,
        r#"{"profiles":{"fast":{"models":[{"id":"a","n_ctx":32000}]}}}"#,
    )
    .unwrap();

    Command::cargo_bin("darkmux")
        .unwrap()
        // Isolate the config root so no operator `role_profiles` bindings leak
        // in — a bare DARKMUX_HOME (no config.json) means every review role is
        // unmapped, exercising the no-binding-and-no-default path deterministically.
        .env("DARKMUX_HOME", tmp.path())
        .args([
            "mission",
            "launch",
            "review",
            "--param",
            &format!("worktree={}", tmp.path().to_str().unwrap()),
            "--param",
            &format!("diff_file={}", diff_path.to_str().unwrap()),
            "--param",
            &format!("profiles={}", profiles.to_str().unwrap()),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("default_profile"));
}

/// (#2345 C1) `review-report-step`'s task `depends_on: ["review-synthesis-
/// task"]` — an upstream step erroring leaves it `NodeStatus::Planned`
/// forever, so before this fix an errored graph run rendered nothing at
/// all: exit 0, empty stdout. Forces a hermetic, no-network step error via
/// a `--bundler` pointed at a nonexistent executable — `review-bundle-
/// step`'s own `run_streaming` propagates that spawn failure as a genuine
/// `NodeStatus::Error`, which blocks every downstream task (probe/dedup/
/// judge/verify/synthesis/report) from ever reaching `Complete` — no real
/// LMStudio/Docker/network needed, matching this suite's hermeticity rules
/// (the graph never gets far enough to dispatch a single model call).
#[test]
fn pr_review_run_errored_graph_still_emits_a_degraded_payload() {
    let tmp = TempDir::new().unwrap();
    let diff_path = tmp.path().join("pr.diff");
    fs::write(&diff_path, pr_review_run_diff()).unwrap();
    let profiles = tmp.path().join("profiles.json");
    fs::write(
        &profiles,
        r#"{"profiles":{"fast":{"models":[{"id":"a","n_ctx":32000}]}},"default_profile":"fast"}"#,
    )
    .unwrap();

    let output = Command::cargo_bin("darkmux")
        .unwrap()
        // Isolate the config root — same hermeticity discipline as every
        // other graph-path test in this file.
        .env("DARKMUX_HOME", tmp.path())
        .args([
            "mission",
            "launch",
            "review",
            "--param",
            &format!("worktree={}", tmp.path().to_str().unwrap()),
            "--param",
            &format!("diff_file={}", diff_path.to_str().unwrap()),
            "--param",
            &format!("profiles={}", profiles.to_str().unwrap()),
            "--param",
            "bundler=/definitely/not/a/real/darkmux-bundler-xyz",
            "--param",
            "emit=-",
        ])
        .output()
        .unwrap();

    // (#2345 C1) `run_dispatch` never turns a step-level error into a hard
    // process `Err` — the exit code stays 0 on any produced review output,
    // matching `mission_launch_review::launch`'s own documented contract.
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("stdout was not JSON — the errored graph run rendered nothing ({e}): {stdout}")
    });
    assert_eq!(v["mode"], "degraded", "an errored graph run must still render as degraded, not silence");
}

/// (#1311, restored #2345 I2) The GRAPH path's own `synthesis`/`done`
/// liveness markers, dropped when #2310 P3 moved the render itself into
/// `review-report-step` without carrying the bracket along — the
/// `from_envelope` path (`pr_review_run_emits_liveness_floor_markers_in_order`)
/// never lost them, since it renders inline in the launcher and always
/// has. Reuses the same hermetic errored-graph fixture as the C1 test
/// above (the fallback path is exactly where these markers were missing).
#[test]
fn pr_review_run_errored_graph_still_emits_synthesis_and_done_liveness_markers() {
    let tmp = TempDir::new().unwrap();
    let diff_path = tmp.path().join("pr.diff");
    fs::write(&diff_path, pr_review_run_diff()).unwrap();
    let profiles = tmp.path().join("profiles.json");
    fs::write(
        &profiles,
        r#"{"profiles":{"fast":{"models":[{"id":"a","n_ctx":32000}]}},"default_profile":"fast"}"#,
    )
    .unwrap();

    let output = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", tmp.path())
        .args([
            "mission",
            "launch",
            "review",
            "--param",
            &format!("worktree={}", tmp.path().to_str().unwrap()),
            "--param",
            &format!("diff_file={}", diff_path.to_str().unwrap()),
            "--param",
            &format!("profiles={}", profiles.to_str().unwrap()),
            "--param",
            "bundler=/definitely/not/a/real/darkmux-bundler-xyz",
            "--param",
            "emit=-",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr={}", String::from_utf8_lossy(&output.stderr));

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_in_order(&stderr, &["process-start", "synthesis", "done"], "stderr");

    let liveness_dir = tmp.path().join("liveness");
    let mut logs: Vec<_> = fs::read_dir(&liveness_dir)
        .expect("liveness dir should exist")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "log"))
        .collect();
    assert_eq!(logs.len(), 1, "expected one heartbeat file, got {logs:?}");
    let file_body = fs::read_to_string(logs.pop().unwrap()).unwrap();
    assert_in_order(&file_body, &["process-start", "synthesis", "done"], "heartbeat file");
}

/// (#2345 I3) `run_dispatch` used to REPLACE `review-report-step`'s whole
/// `config` with only the launch's own `emit`/`envelope_out`/`attribution`
/// — silently discarding whatever the document itself already declared
/// there (a user-tier config may pin a fixed `attribution`). It now MERGES,
/// launcher values winning only for keys the launch actually supplied.
/// Proven with a launch that passes NO `--param attribution=` of its own:
/// the document's own stamped attribution must still appear in the
/// rendered footer. An empty diff + empty worktree (no `--param bundler`)
/// exercises the WHOLE graph — bundle -> probe -> dedup -> judge -> verify
/// -> synthesis -> report — hermetically: the built-in bundler yields zero
/// bundles for an empty diff, so every downstream stage completes
/// trivially with zero model calls, and `review-report-step` itself runs
/// and renders (unlike the C1/I2 tests above, which force an upstream
/// ERROR so the step never runs at all — exactly the case this test needs
/// to AVOID, since the step's own config merge is what's under test).
#[test]
fn pr_review_run_document_stamped_attribution_survives_the_launcher_merge() {
    let tmp = TempDir::new().unwrap();
    let diff_path = tmp.path().join("empty.diff");
    fs::write(&diff_path, "").unwrap();
    let profiles = tmp.path().join("profiles.json");
    fs::write(
        &profiles,
        r#"{"profiles":{"fast":{"models":[{"id":"a","n_ctx":32000}]}},"default_profile":"fast"}"#,
    )
    .unwrap();

    let builtin_path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("templates/builtin/mission-configs/review.json");
    let mut doc: serde_json::Value = serde_json::from_str(&fs::read_to_string(&builtin_path).unwrap()).unwrap();
    doc["id"] = serde_json::json!("review-attrib-test");
    let report_phase = doc["phases"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|p| p["id"] == "report")
        .expect("review.json declares a report phase");
    let report_task = report_phase["tasks"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|t| t["id"] == "review-report-task")
        .expect("review.json declares review-report-task");
    let report_step = report_task["steps"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|s| s["id"] == "review-report-step")
        .expect("review.json declares review-report-step");
    report_step["config"] = serde_json::json!({ "attribution": "custom-doc-attribution-marker" });

    let config_dir = tmp.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("review-attrib-test.json"), serde_json::to_string(&doc).unwrap()).unwrap();

    let output = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", tmp.path())
        .args([
            "mission",
            "launch",
            "review-attrib-test",
            "--param",
            &format!("worktree={}", tmp.path().to_str().unwrap()),
            "--param",
            &format!("diff_file={}", diff_path.to_str().unwrap()),
            "--param",
            &format!("profiles={}", profiles.to_str().unwrap()),
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
    assert!(
        stdout.contains("custom-doc-attribution-marker"),
        "the document's own stamped attribution must survive the launcher's config merge: {stdout}"
    );

    // (#2345 CONSIDER-3, round 2) The (#1311/I2) synthesis/done liveness
    // bracket fires on the CLEAN path too (`review-report-step` actually
    // ran here, never the fallback) — pin the ordering here as well, not
    // just on the errored-graph variant.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_in_order(&stderr, &["process-start", "synthesis", "done"], "stderr");
}

/// (#2345 CONSIDER-1, round 2) The errored-run FALLBACK used to honor a
/// merged `attribution` but still pass the raw launch-only `emit`/
/// `envelope_out` — so a document-stamped `emit: <file>` was written by a
/// CLEAN run (the step reads its own merged config) but silently dumped to
/// STDOUT by an errored run (the fallback ignored the merge for those two
/// fields). Forces the fallback via the same hermetic bogus-`--bundler`
/// error the C1 test uses, over a document that stamps a fixed `emit` file
/// path on `review-report-step` with NO `--param emit=` of its own —
/// exactly the pairing CONSIDER-1 named.
#[test]
fn pr_review_run_errored_graph_honors_the_documents_stamped_emit_path_in_the_fallback() {
    let tmp = TempDir::new().unwrap();
    let diff_path = tmp.path().join("pr.diff");
    fs::write(&diff_path, pr_review_run_diff()).unwrap();
    let profiles = tmp.path().join("profiles.json");
    fs::write(
        &profiles,
        r#"{"profiles":{"fast":{"models":[{"id":"a","n_ctx":32000}]}},"default_profile":"fast"}"#,
    )
    .unwrap();

    let builtin_path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("templates/builtin/mission-configs/review.json");
    let mut doc: serde_json::Value = serde_json::from_str(&fs::read_to_string(&builtin_path).unwrap()).unwrap();
    doc["id"] = serde_json::json!("review-emit-fallback-test");
    let report_phase = doc["phases"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|p| p["id"] == "report")
        .expect("review.json declares a report phase");
    let report_task = report_phase["tasks"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|t| t["id"] == "review-report-task")
        .expect("review.json declares review-report-task");
    let report_step = report_task["steps"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|s| s["id"] == "review-report-step")
        .expect("review.json declares review-report-step");
    let stamped_emit_path = tmp.path().join("stamped-emit.json");
    report_step["config"] = serde_json::json!({ "emit": stamped_emit_path.display().to_string() });

    let config_dir = tmp.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("review-emit-fallback-test.json"), serde_json::to_string(&doc).unwrap()).unwrap();

    let output = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", tmp.path())
        .args([
            "mission",
            "launch",
            "review-emit-fallback-test",
            "--param",
            &format!("worktree={}", tmp.path().to_str().unwrap()),
            "--param",
            &format!("diff_file={}", diff_path.to_str().unwrap()),
            "--param",
            &format!("profiles={}", profiles.to_str().unwrap()),
            "--param",
            "bundler=/definitely/not/a/real/darkmux-bundler-xyz",
            // Deliberately NO --param emit= — the document's own stamped
            // path is the only thing naming a destination.
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
    assert!(
        stdout.trim().is_empty(),
        "with no --param emit=, the document's own stamped path must receive the payload, not stdout: {stdout}"
    );
    let written = fs::read_to_string(&stamped_emit_path).unwrap_or_else(|e| {
        panic!(
            "expected the document-stamped emit path to receive the fallback's rendered payload: {e}"
        )
    });
    let v: serde_json::Value = serde_json::from_str(&written).expect("stamped emit file holds valid JSON");
    assert_eq!(v["mode"], "degraded", "an errored graph run must still render as degraded, not silence");
}

/// (#2345 CONSIDER-3, round 2) `effective_attribution` (the fallback's own
/// read of the merged report-step config) is DEAD CODE on the CLEAN path —
/// `pr_review_run_document_stamped_attribution_survives_the_launcher_merge`
/// exercises the step's own direct read of `step.config`, never this
/// launcher-side variable, so a mutation to `effective_attribution`'s own
/// computation left that test green. This forces the FALLBACK (the same
/// hermetic bogus-`--bundler` error C1/CONSIDER-1 use) over a document
/// that stamps a fixed `attribution`, with no `--param attribution=` of
/// its own — the only path that actually reads `effective_attribution`.
#[test]
fn pr_review_run_errored_graph_honors_the_documents_stamped_attribution_in_the_fallback() {
    let tmp = TempDir::new().unwrap();
    let diff_path = tmp.path().join("pr.diff");
    fs::write(&diff_path, pr_review_run_diff()).unwrap();
    let profiles = tmp.path().join("profiles.json");
    fs::write(
        &profiles,
        r#"{"profiles":{"fast":{"models":[{"id":"a","n_ctx":32000}]}},"default_profile":"fast"}"#,
    )
    .unwrap();

    let builtin_path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("templates/builtin/mission-configs/review.json");
    let mut doc: serde_json::Value = serde_json::from_str(&fs::read_to_string(&builtin_path).unwrap()).unwrap();
    doc["id"] = serde_json::json!("review-attrib-fallback-test");
    let report_phase = doc["phases"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|p| p["id"] == "report")
        .expect("review.json declares a report phase");
    let report_task = report_phase["tasks"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|t| t["id"] == "review-report-task")
        .expect("review.json declares review-report-task");
    let report_step = report_task["steps"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|s| s["id"] == "review-report-step")
        .expect("review.json declares review-report-step");
    report_step["config"] = serde_json::json!({ "attribution": "custom-fallback-attribution-marker" });

    let config_dir = tmp.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("review-attrib-fallback-test.json"), serde_json::to_string(&doc).unwrap()).unwrap();

    let output = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", tmp.path())
        .args([
            "mission",
            "launch",
            "review-attrib-fallback-test",
            "--param",
            &format!("worktree={}", tmp.path().to_str().unwrap()),
            "--param",
            &format!("diff_file={}", diff_path.to_str().unwrap()),
            "--param",
            &format!("profiles={}", profiles.to_str().unwrap()),
            "--param",
            "bundler=/definitely/not/a/real/darkmux-bundler-xyz",
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
    assert!(
        stdout.contains("custom-fallback-attribution-marker"),
        "the document's own stamped attribution must survive the FALLBACK's config merge too: {stdout}"
    );
}

// ─── #2124: SIGTERM mid-probe leaves a terminal record + no orphaned curl ──

/// A tiny local server that ACCEPTS every connection and never responds —
/// makes a review probe's remote `curl` call (`darkmux-crew`'s
/// `remote_chat_attempt`) hang exactly the way a live LLM endpoint that
/// stopped answering would. Each accepted connection gets its own thread
/// doing a blocking read with no timeout; that read returns (`Ok(0)`/`Err`)
/// the instant the PEER's socket closes — i.e. the instant `curl` itself
/// dies — which is what [`ReapProbe::wait_for_close`] below waits on. This
/// is a more precise proof of reaping than polling `ps`/`pgrep` for a
/// process that might not exist yet: it observes the OS actually tearing
/// the connection down, not just a name disappearing from a process list.
struct HangingStubServer {
    port: u16,
    accepted_rx: std::sync::mpsc::Receiver<()>,
    closed_rx: std::sync::mpsc::Receiver<()>,
}

impl HangingStubServer {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binding the stub listener");
        let port = listener.local_addr().unwrap().port();
        let (accepted_tx, accepted_rx) = std::sync::mpsc::channel::<()>();
        let (closed_tx, closed_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let _ = accepted_tx.send(());
                let closed_tx = closed_tx.clone();
                std::thread::spawn(move || {
                    use std::io::Read;
                    let mut buf = [0u8; 1];
                    // Blocks until the peer closes (curl dies) or sends a
                    // byte (never happens — this server never writes a
                    // response, so curl never gets anything back that
                    // would make it close on its own before its `-m`
                    // bound, which this test's SIGTERM preempts).
                    let _ = stream.read(&mut buf);
                    let _ = closed_tx.send(());
                });
            }
        });
        Self { port, accepted_rx, closed_rx }
    }

    /// Block until curl has actually connected — the precise "now it's
    /// mid-probe" signal this test sends SIGTERM against, in place of a
    /// fixed sleep that would either race a slow CI runner or waste time
    /// on a fast one.
    fn wait_for_a_connection(&self, timeout: std::time::Duration) -> bool {
        self.accepted_rx.recv_timeout(timeout).is_ok()
    }

    /// Block until SOME accepted connection's read returned — i.e. some
    /// `curl` this test's review dispatch spawned has been torn down.
    fn wait_for_a_connection_to_close(&self, timeout: std::time::Duration) -> bool {
        self.closed_rx.recv_timeout(timeout).is_ok()
    }
}

/// (#2265 review) A stub endpoint that ANSWERS — the counterpart to
/// `HangingStubServer`, for tests that need a dispatch to run to completion
/// and leave its flow records behind rather than to hang mid-probe. Replies
/// to every request with one minimal chat completion. No model, no Docker: the
/// role it serves is tool-less, so `dispatch_internal` takes the light
/// single-shot hosted path (a plain host `curl`).
struct RespondingStubServer {
    port: u16,
}

impl RespondingStubServer {
    fn start() -> Self {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("binding the stub listener");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    use std::io::{Read, Write};
                    // Read the request head (and whatever body arrives with
                    // it); curl waits for the response, so a bounded read is
                    // enough — this is a stub, not an HTTP server.
                    let mut buf = [0u8; 8192];
                    let _ = stream.read(&mut buf);
                    let body = serde_json::json!({
                        "choices": [{ "message": { "content": "ack" } }],
                        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
                    })
                    .to_string();
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    );
                    let _ = stream.flush();
                });
            }
        });
        Self { port }
    }
}

fn responding_endpoint_profiles_json(port: u16) -> String {
    format!(
        r#"{{
            "profiles": {{
                "stub": {{
                    "models": [
                        {{"id": "stub-model", "n_ctx": 8000, "endpoint": {{"url": "http://127.0.0.1:{port}"}}}}
                    ]
                }}
            }},
            "default_profile": "stub"
        }}"#
    )
}

fn hanging_endpoint_profiles_json(port: u16) -> String {
    format!(
        r#"{{
            "profiles": {{
                "hang": {{
                    "models": [
                        {{"id": "stub-model", "n_ctx": 8000, "endpoint": {{"url": "http://127.0.0.1:{port}"}}}}
                    ]
                }}
            }},
            "default_profile": "hang"
        }}"#
    )
}

/// Assert that no `curl` spawned by the darkmux child `pid` outlives it.
/// Polls the process table for that child's own `darkmux-remote-<pid>-`
/// config-file marker (see `remote_chat_attempt`) for up to 2s: the OS
/// tears the table down asynchronously after SIGKILL, and instrumented
/// (coverage) builds are slower than a fixed settle delay allows for.
/// Scoped to `pid` so a sibling test's live curl is never mistaken for a
/// survivor of this one.
fn assert_no_surviving_remote_curl(pid: u32, label: &str) {
    let marker = format!("darkmux-remote-{pid}-");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let survivors = loop {
        let Ok(out) = std::process::Command::new("pgrep").args(["-f", &marker]).output() else {
            return; // no `pgrep` on this image — the socket-close proof already covers it
        };
        let survivors = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if survivors.is_empty() || std::time::Instant::now() >= deadline {
            break survivors;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(
        survivors.is_empty(),
        "a darkmux {label} curl process is still running after the parent exited: {survivors}"
    );
}

/// (#2124) `kill <pid>` (SIGTERM) on a `mission launch review` blocked
/// mid-probe (a real `curl` call to an endpoint that never answers) must:
/// exit within 5s, leave a `mission close` flow record naming the signal,
/// leave the mission `finalized` with every phase `abandoned` (never stuck
/// `active`), and leave no `curl` process still holding the stub
/// connection open. Reproduces the exact scenario from the issue: `kill
/// <pid>` on a real review launch, mid-probe, previously left the mission
/// `active` forever with the `curl` child running past the parent's death.
#[test]
fn mission_launch_review_sigterm_mid_probe_finalizes_and_reaps_curl() {
    let stub = HangingStubServer::start();

    let home = TempDir::new().unwrap();
    // (#661/Beat-33 flattened layout) `DARKMUX_HOME` alone puts missions at
    // `<home>/missions` directly (`crew::loader::missions_dir` — no `crew/`
    // nesting), but flow records do NOT follow `DARKMUX_HOME` at all
    // (`config_access::flows_dir` resolves independently via
    // `DARKMUX_FLOWS_DIR` > config > `~/.darkmux/flows`, and a `cargo test`
    // build's own default is a SHARED `/tmp/darkmux-test-isolated/flows` —
    // isolating it here avoids colliding with any other test in this same
    // binary run). Both set explicitly so this test never depends on which
    // default each one happens to fall back to.
    let flows = TempDir::new().unwrap();
    let worktree = TempDir::new().unwrap();
    fs::create_dir_all(worktree.path().join("src")).unwrap();
    // A one-line change INSIDE a function body — `build_bundles`'s TS
    // extraction unit is the enclosing function, not a bare top-level
    // `const` (a diff touching only top-level statements, like
    // `pr_review_run_diff()` elsewhere in this file, produces ZERO
    // bundles and short-circuits to a degenerate envelope before any
    // probe ever dispatches — proven nothing about SIGTERM handling; this
    // fixture is deliberately function-shaped so a real probe dispatch
    // actually happens). The worktree file holds the POST-change content;
    // `pr.diff` describes the same edit as a unified diff.
    fs::write(
        worktree.path().join("src/x.ts"),
        "function computeB(a) {\n  const b = 2;\n  return a + b;\n}\n",
    )
    .unwrap();
    let diff_path = worktree.path().join("pr.diff");
    fs::write(
        &diff_path,
        "diff --git a/src/x.ts b/src/x.ts\n--- a/src/x.ts\n+++ b/src/x.ts\n@@ -1,4 +1,4 @@\n function computeB(a) {\n-  const b = 1;\n+  const b = 2;\n   return a + b;\n }\n",
    )
    .unwrap();
    let profiles_path = worktree.path().join("profiles.json");
    fs::write(&profiles_path, hanging_endpoint_profiles_json(stub.port)).unwrap();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_darkmux"))
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .args([
            "mission",
            "launch",
            "review",
            "--param",
            &format!("worktree={}", worktree.path().to_str().unwrap()),
            "--param",
            &format!("diff_file={}", diff_path.to_str().unwrap()),
            "--param",
            &format!("profiles={}", profiles_path.to_str().unwrap()),
            // Every declared review role routed at the same hanging
            // endpoint — the FIRST one dispatched is enough to reproduce
            // "mid-probe", but naming all of them means this test doesn't
            // silently stop proving anything if the probe roster changes.
            "--param",
            "review-probe-high=hang",
            "--param",
            "review-probe-mid=hang",
            "--param",
            "review-probe-low=hang",
            "--param",
            "review-judge=hang",
            "--param",
            "review-verify=hang",
            // Comfortably longer than this test's own 5s reap bound — the
            // signal must be what ends the run, not curl's own `-m`.
            "--timeout",
            "60",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawning darkmux mission launch review");
    let pid = child.id();

    // Wait for the precise "now it's mid-probe" signal — curl actually
    // connecting to the stub — rather than a fixed sleep that would either
    // race a slow CI runner (mint + bundle + crew resolution before the
    // first probe dispatch) or waste time on a fast one. A generous bound:
    // this is proving SIGTERM handling, not probe dispatch latency.
    assert!(
        stub.wait_for_a_connection(std::time::Duration::from_secs(20)),
        "the review dispatch never reached a probe call to the stub server within 20s — \
         something upstream of SIGTERM handling broke (mint, bundling, or crew resolution)"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "the review launcher must still be running (blocked on the hanging probe) before SIGTERM"
    );

    let kill_status = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("running kill -TERM");
    assert!(kill_status.success(), "kill -TERM itself must succeed");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let exit_status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "mission launch review did not exit within 5s of SIGTERM (#2124 regression)"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(!exit_status.success(), "a signal-interrupted review must not exit 0");

    // The stub server's per-connection reader must observe curl's socket
    // closing — proof the process-group reap actually reached the child,
    // not just that the PARENT died (an orphaned `curl` would leave this
    // hanging until ITS OWN `-m` bound, far past this wait).
    assert!(
        stub.wait_for_a_connection_to_close(std::time::Duration::from_secs(3)),
        "no `curl` connection to the stub server was ever torn down — a child process survived \
         the parent (#2124 regression)"
    );

    // Secondary corroboration via the process table: `remote_chat_attempt`
    // (crates/darkmux-crew/src/dispatch_internal.rs) always spawns curl as
    // `curl -sS -m <t> -K <tmp-path>`, where `<tmp-path>` is named
    // `darkmux-remote-<pid>-<n>.curl` — a distinctive `-f`-matchable
    // fragment regardless of the port curl was told to hit (the URL lives
    // INSIDE that config file, never in argv). The `<pid>` is THIS test's
    // darkmux child — matching on the bare prefix would also see the sibling
    // SIGTERM test's still-live curl when cargo runs them concurrently
    // (that cross-match failed both tests together on main's coverage job).
    // `pgrep` missing entirely (non-macOS/Linux CI image) — the socket-close
    // proof above already covers this.
    assert_no_surviving_remote_curl(child.id(), "review-dispatch");

    // The mission itself: exactly one was minted under this isolated
    // DARKMUX_HOME, so no id needs to be captured from the child's own
    // output — read whichever one is there. `<home>/missions` directly
    // (the flattened post-Beat-33 layout — `crew::loader::missions_dir`),
    // not `<home>/crew/missions`.
    let missions_dir = home.path().join("missions");
    let mission_id = fs::read_dir(&missions_dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", missions_dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .next()
        .expect("exactly one mission must have been minted");

    let mission_json: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(missions_dir.join(&mission_id).join("mission.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        mission_json["status"], "finalized",
        "an interrupted review must reach a terminal mission status, never stay active: {mission_json}"
    );

    let phases_dir = missions_dir.join(&mission_id).join("phases");
    let mut saw_a_phase = false;
    for entry in fs::read_dir(&phases_dir).unwrap().filter_map(|e| e.ok()) {
        let phase_json: serde_json::Value = serde_json::from_str(&fs::read_to_string(entry.path()).unwrap()).unwrap();
        assert_eq!(
            phase_json["status"], "abandoned",
            "a signal-interrupted run must abandon its phases, never complete one: {phase_json}"
        );
        saw_a_phase = true;
    }
    assert!(saw_a_phase, "the mint must have produced at least one phase to check");

    // The flow record: `mission close`, carrying the signal in its reason
    // — the "mission close with reason: signal" vocabulary #2124 asks for
    // (see `review_finalize_guard.rs`'s own doc for why this reuses
    // `finalize_review_mission`'s existing `mission close`/Finalized path
    // rather than inventing a separate `mission abort`/Aborted one — the
    // SAME choice `crawl_launch.rs`'s `CrawlFinalizeGuard` already made for
    // an interrupted crawl).
    let mut found_mission_close = false;
    for entry in fs::read_dir(flows.path()).unwrap_or_else(|e| panic!("reading {}: {e}", flows.path().display())) {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        for line in fs::read_to_string(&path).unwrap().lines() {
            let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            if record["action"] == "mission close" && record["mission_id"] == mission_id {
                found_mission_close = true;
                let reasoning = record["reasoning"].as_str().unwrap_or_default();
                assert!(
                    reasoning.to_lowercase().contains("signal"),
                    "the mission close record's reason must name the signal: {reasoning}"
                );
            }
        }
    }
    assert!(found_mission_close, "expected a `mission close` flow record for {mission_id}");
}

// ─── #2131: the shared LaunchFinalizeGuard, ported to crawl + generic ─────
//
// (#2131, historical) When this proof was written, `src/crawl_launch.rs` was a
// separate literal-routed launcher sharing the `LaunchFinalizeGuard`; it and
// `crawl_launch_tests.rs` were DELETED in #2301 — crawl now runs through the
// generic launcher this file proves, and its finalize/interrupt coverage lives
// in `crates/darkmux-lab/src/crawl/unit_step_tests.rs` (the scheduler-level
// probe that runs plan → unit → summary through `run_step_graph` with an
// injected dispatch). The paragraph that follows describes the situation as
// it was, kept because it explains why the live test below exercises the
// tool-less hosted path rather than a container:
// (then) `crawl_launch.rs` gained the same `LaunchFinalizeGuard` + SIGTERM/SIGHUP
// this file adds a live binary-level proof for below (the generic-graph/
// coder-phase launcher), but does NOT get an equivalent live-dispatch
// integration test here: crawl's role_id is hardcoded to `"crawler"`
// (tool-granting), so its dispatch always goes through the agentic
// `darkmux-runtime` CONTAINER path (`dispatch_internal.rs`'s docker spawn,
// #2114's own concurrent surface) rather than the tool-less light
// single-shot HOSTED path (a plain host `curl`) the test below exercises —
// and that container path's child pid isn't registered into
// `darkmux_types::child_registry` yet, so a real "no lingering container"
// proof isn't reachable without either Docker + the runtime image
// (unavailable in this environment; the release-gate doctrine reserves
// that kind of real-container run for dogfood, not `cargo test`) or wiring
// `child_registry` into the docker spawn site — deliberately left
// untouched here per this task's own boundary. Crawl's coverage instead
// rests on: `crawl_launch_tests.rs`'s
// `a_panic_mid_loop_still_finalizes_via_the_raii_guard` (proves the
// Drop-abort-writer shape survived the guard extraction) and
// `interrupted_at_readback_reports_interrupted_not_error` (proves the
// interrupt-flag read path), both passing unchanged against the shared
// guard, plus `crate::launch_guard::arm()` replacing the old SIGINT-only
// `darkmux_types::interrupt::install()` call (verified by `cargo check` +
// the module's own doc). Follow-up: wire `child_registry` into the docker
// spawn path, then add crawl's own live SIGTERM+reap test the same shape
// as the one below.

/// (#2131) `kill <pid>` (SIGTERM) on `mission launch <generic-graph-config>`
/// blocked mid-dispatch (a real `curl` call to an endpoint that never
/// answers) must: exit within 5s, leave the mission `finalized` with the
/// phase `abandoned` (never stuck `active`), and leave no `curl` process
/// still holding the stub connection open. This is the launcher #2131's own
/// issue named as having NO guard at all before this PR — a minimal
/// user-tier config with a single `dispatch.internal` step exercises the
/// SAME generic-graph path `coder-phase` and every `mission propose`-built
/// config also run through.
#[test]
fn mission_launch_generic_sigterm_mid_dispatch_finalizes_and_reaps_curl() {
    let stub = HangingStubServer::start();

    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();

    let profiles_path = home.path().join("profiles.json");
    fs::write(&profiles_path, hanging_endpoint_profiles_json(stub.port)).unwrap();

    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    // (#2131) `review-judge` is deliberately TOOL-LESS
    // (`tool_palette.allow: []`) — `dispatch_internal.rs` routes a
    // tool-less role's remote dispatch through the light single-shot
    // HOSTED path (a plain host-side `curl`, already `child_registry`-
    // wired) rather than spinning up a `darkmux-runtime` container, which
    // this test environment has neither Docker nor the image for. A
    // tool-granting role (e.g. `crawler`) would instead need a real
    // container.
    let config_json = r#"{
        "id": "sigterm-generic-test",
        "name": "SIGTERM Generic Test",
        "schema_version": "2.3",
        "phases": [{
            "id": "p1",
            "tasks": [{
                "id": "t1",
                "steps": [{
                    "id": "s1",
                    "kind": "dispatch.internal",
                    "config": { "role_id": "review-judge", "message": "hang please" }
                }]
            }]
        }]
    }"#;
    fs::write(config_dir.join("sigterm-generic-test.json"), config_json).unwrap();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_darkmux"))
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_PROFILES", &profiles_path)
        .args(["mission", "launch", "sigterm-generic-test", "--timeout", "60"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawning darkmux mission launch sigterm-generic-test");
    let pid = child.id();

    assert!(
        stub.wait_for_a_connection(std::time::Duration::from_secs(20)),
        "the generic-graph dispatch never reached a dispatch call to the stub server within 20s"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "the generic launcher must still be running (blocked on the hanging dispatch) before SIGTERM"
    );

    let kill_status =
        std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).status().expect("running kill -TERM");
    assert!(kill_status.success(), "kill -TERM itself must succeed");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let exit_status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "mission launch (generic) did not exit within 5s of SIGTERM (#2131 regression)"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(!exit_status.success(), "a signal-interrupted generic-graph run must not exit 0");

    assert!(
        stub.wait_for_a_connection_to_close(std::time::Duration::from_secs(3)),
        "no `curl` connection to the stub server was ever torn down — a child process survived \
         the parent (#2131 regression)"
    );

    assert_no_surviving_remote_curl(child.id(), "generic-graph");

    let missions_dir = home.path().join("missions");
    let mission_id = fs::read_dir(&missions_dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", missions_dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .next()
        .expect("exactly one mission must have been minted");

    let mission_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(missions_dir.join(&mission_id).join("mission.json")).unwrap())
            .unwrap();
    assert_eq!(
        mission_json["status"], "finalized",
        "an interrupted generic-graph run must reach a terminal mission status, never stay active: {mission_json}"
    );

    let phases_dir = missions_dir.join(&mission_id).join("phases");
    let mut saw_a_phase = false;
    for entry in fs::read_dir(&phases_dir).unwrap().filter_map(|e| e.ok()) {
        let phase_json: serde_json::Value = serde_json::from_str(&fs::read_to_string(entry.path()).unwrap()).unwrap();
        assert_eq!(
            phase_json["status"], "abandoned",
            "a signal-interrupted generic-graph run must abandon its phase, never complete one: {phase_json}"
        );
        saw_a_phase = true;
    }
    assert!(saw_a_phase, "the mint must have produced at least one phase to check");
}

/// (#2345 C2) `outcome_from` names the task whose last step's output the
/// launcher promotes as the `mission close` record's payload. Before this
/// fix, a typo'd `outcome_from` was refused only AFTER the whole run — the
/// close-time check in `run_summary_payload` — so a config-authoring
/// mistake on a long-running mission surfaced only once every step had
/// already dispatched. `MissionConfig::validate` now catches it
/// statically, and `mission_launch::launch` runs `validate` (and `bail!`s
/// on any `Error` finding) BEFORE minting a mission at all — so a bad
/// `outcome_from` must fail loud with NO `missions/` entry ever created.
/// `procedural.noop` needs no model/network/Docker — purely hermetic.
#[test]
fn mission_launch_outcome_from_unknown_task_refused_before_minting() {
    let home = TempDir::new().unwrap();

    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    let config_json = r#"{
        "id": "outcome-from-typo-test",
        "name": "Outcome From Typo Test",
        "schema_version": "3.3",
        "phases": [{
            "id": "p1",
            "tasks": [{
                "id": "t1",
                "steps": [{ "id": "s1", "kind": "procedural.noop" }]
            }]
        }],
        "outcome_from": "no-such-task"
    }"#;
    fs::write(config_dir.join("outcome-from-typo-test.json"), config_json).unwrap();

    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", home.path())
        .args(["mission", "launch", "outcome-from-typo-test"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("outcome_from"))
        .stderr(predicate::str::contains("no-such-task"));

    let missions_dir = home.path().join("missions");
    assert!(
        !missions_dir.is_dir() || fs::read_dir(&missions_dir).unwrap().next().is_none(),
        "a refused outcome_from must never mint a mission — no missions/ entry may exist"
    );
}

#[test]
fn mission_launch_run_on_unknown_value_refused_before_minting() {
    // (#2310 P4a review M3) The CLI-refuse-before-mint twin of
    // `mission_launch_outcome_from_unknown_task_refused_before_minting`
    // above — a `run_on` validate Error (unknown value) must refuse the
    // SAME way, before anything is minted.
    let home = TempDir::new().unwrap();

    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    let config_json = r#"{
        "id": "run-on-typo-test",
        "name": "Run On Typo Test",
        "schema_version": "3.4",
        "phases": [{
            "id": "p1",
            "tasks": [{
                "id": "t1",
                "run_on": ["complete", "maybe"],
                "steps": [{ "id": "s1", "kind": "procedural.noop" }]
            }]
        }]
    }"#;
    fs::write(config_dir.join("run-on-typo-test.json"), config_json).unwrap();

    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", home.path())
        .args(["mission", "launch", "run-on-typo-test"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("run_on"))
        .stderr(predicate::str::contains("maybe"));

    let missions_dir = home.path().join("missions");
    assert!(
        !missions_dir.is_dir() || fs::read_dir(&missions_dir).unwrap().next().is_none(),
        "a refused run_on must never mint a mission — no missions/ entry may exist"
    );
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
    cmd.args(["lab", "eval", "--funnel", "--dialectic"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn review_bench_funnel_conflicts_with_agentic_and_freeform() {
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.args(["lab", "eval", "--funnel", "--agentic"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));

    let mut cmd2 = Command::cargo_bin("darkmux").unwrap();
    cmd2.args(["lab", "eval", "--funnel", "--freeform"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn review_bench_crew_requires_funnel() {
    // --crew named without --funnel: clap's `requires = "funnel"` fires
    // before the command handler ever runs (no dispatch, no cases loaded).
    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.args(["lab", "eval", "--roster-profile", "review-funnel"])
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
        cmd.args(["lab", "eval", flag, value])
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
    cmd.args(["lab", "eval", "--funnel", "--roster-profile", "review-funnel"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--funnel requires --workdirs"));
}

#[test]
fn review_bench_funnel_no_resolvable_roster_fails_preflight() {
    // (#1426 ship-2) --funnel + --workdirs but no --roster-profile/--profile AND
    // a registry with no default_profile: the funnel-context preflight
    // (resolve_funnel_ctx → the role→profile resolver) fails loud before any
    // dispatch spends a token, naming the missing roster. Uses a minimal
    // one-case fixture so the --workdirs tree-existence check (which runs
    // first) passes and the resourcing check is the one under test.
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
    let profiles = tmp.path().join("profiles.json");
    fs::write(
        &profiles,
        r#"{"profiles":{"fast":{"models":[{"id":"a","n_ctx":32000}]}}}"#,
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.args([
        "lab",
        "eval",
        "--cases-dir",
        cases_dir.to_str().unwrap(),
        "--funnel",
        "--profiles-file",
        profiles.to_str().unwrap(),
        "--workdirs",
        workdirs.to_str().unwrap(),
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains("roster profile"));
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
        "eval",
        "--funnel",
        "--roster-profile",
        "review-funnel",
        "--k",
        "0",
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains("not in 1.."));
}

#[test]
fn review_bench_funnel_roster_local_model_without_n_ctx_fails_loud() {
    // (#1426 ship-2) The registry LOADS fine, but the named ROSTER profile's
    // local model omits `n_ctx` (#1282) — a LOCAL review seat is loaded at its
    // declared context, so the resourcing resolver fails loud at that seat,
    // BEFORE the per-case table header prints, naming the seat and the field.
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
                "ctxless": {
                    "models": [{"id": "local-b"}]
                }
            },
            "default_profile": "ctxless"
        }"#,
    )
    .unwrap();

    Command::cargo_bin("darkmux")
        .unwrap()
        .args([
            "lab",
            "eval",
            "--cases-dir",
            cases_dir.to_str().unwrap(),
            "--funnel",
            "--workdirs",
            workdirs.to_str().unwrap(),
            "--roster-profile",
            "ctxless",
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

/// (#1475) A profiles registry whose `review-funnel` profile every review seat
/// is pinned to for the funnel bench (via the per-run role→profile override);
/// `--roster-profile review-funnel` names it. The resolver staffs
/// probe/judge/verify from its default model; no LMStudio involved.
fn funnel_registry_json() -> &'static str {
    r#"{
        "profiles": {
            "review-funnel": {
                "description": "review roster",
                "models": [
                    {"id": "model-a", "n_ctx": 32000}
                ]
            }
        },
        "default_profile": "review-funnel"
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
fn review_bench_funnel_nonexistent_roster_fails_preflight_listing_available() {
    // (#1475) --roster-profile names a profile the registry doesn't have: the
    // bench's roster pre-check fails loud BEFORE any dispatch, and the error
    // names both the missing profile and the profiles that DO exist (get_profile's
    // "Available:" listing) — the operator never has to open profiles.json.
    let tmp = TempDir::new().unwrap();
    let (cases_dir, workdirs, registry) = write_funnel_fixture(&tmp);

    let mut cmd = Command::cargo_bin("darkmux").unwrap();
    cmd.args([
        "lab",
        "eval",
        "--cases-dir",
        cases_dir.to_str().unwrap(),
        "--funnel",
        "--workdirs",
        workdirs.to_str().unwrap(),
        "--roster-profile",
        "ghost",
        "--profiles-file",
        registry.to_str().unwrap(),
    ])
    .assert()
    .failure()
    .stderr(
        predicate::str::contains("ghost")
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
        "eval",
        "--cases-dir",
        cases_dir.to_str().unwrap(),
        "--funnel",
        "--workdirs",
        workdirs.to_str().unwrap(),
        "--roster-profile",
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

    // scores.json: funnel provenance extras (crew / exec_mode).
    let scores: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&scores_out).unwrap()).unwrap();
    assert_eq!(scores["mode"], serde_json::json!("funnel"));
    assert_eq!(scores["crew"], serde_json::json!("review-funnel"));
    assert_eq!(scores["exec_mode"], serde_json::json!("sequential"));
    // (#1512, #1513 review M1) The `"k"` extras field is RETIRED — draw
    // multiplication no longer exists, so there is no value left to snapshot.
    assert!(scores.get("k").is_none(), "the retired \"k\" field must not reappear in the artifact");

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
        "eval",
        "--cases-dir",
        cases_dir.to_str().unwrap(),
        "--funnel",
        "--workdirs",
        workdirs.to_str().unwrap(),
        "--roster-profile",
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
            "default_profile": "fast"
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
            "profile=fast",
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

/// (#2345 MUST FIX, round 2) `--charges-file` mints NO graph — before this
/// fix, nothing ever rendered this path's envelope: `launch`'s own tail
/// used to render + emit unconditionally after every `run_dispatch`
/// (pre-#2310 P3); P3 moved the render into `review-report-step`, a step
/// this path never reaches (it mints no Mission and runs no graph at all).
/// Production symptom: `darkmux mission launch review --param
/// charges_file=<flags> --param emit=-` exited 0 with EMPTY stdout. An
/// EMPTY flags array makes `run_judge_only` short-circuit before any
/// dispatch (`env.degenerate = Some("--charges-file carried zero
/// flags")`), so this needs no LMStudio; an empty diff/worktree keeps the
/// eager pre-dispatch bundling pass hermetic too (the built-in bundler
/// yields zero bundles for an empty diff, no `--bundler` subprocess
/// needed).
#[test]
fn pr_review_run_charges_file_renders_a_payload_to_stdout() {
    let tmp = TempDir::new().unwrap();
    let diff_path = tmp.path().join("empty.diff");
    fs::write(&diff_path, "").unwrap();
    let profiles_path = tmp.path().join("profiles.json");
    fs::write(
        &profiles_path,
        r#"{"profiles":{"fast":{"models":[{"id":"a","n_ctx":32000}]}},"default_profile":"fast"}"#,
    )
    .unwrap();
    let charges_path = tmp.path().join("charges.json");
    fs::write(&charges_path, "[]").unwrap();

    let output = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", tmp.path())
        .args([
            "mission",
            "launch",
            "review",
            "--param",
            &format!("worktree={}", tmp.path().to_str().unwrap()),
            "--param",
            &format!("diff_file={}", diff_path.to_str().unwrap()),
            "--param",
            &format!("profiles={}", profiles_path.to_str().unwrap()),
            "--param",
            &format!("charges_file={}", charges_path.to_str().unwrap()),
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
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("stdout was not JSON — the charges_file path rendered nothing ({e}): {stdout}")
    });
    assert!(v["mode"].is_string(), "expected a rendered {{mode, review, comment}} payload: {stdout}");

    // Bonus: the (#2345 I2-style) synthesis/done liveness bracket applies
    // to this path too now, not just the graph path.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_in_order(&stderr, &["synthesis", "done"], "stderr");
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

// ── `darkmux radio` (#1698 Packet A) ─────────────────────────────────────
//
// No assert_cmd binary-level test exists for `radio`'s live routing path,
// for a reason worth recording: the built-in `review` mission config
// (`templates/builtin/mission-configs/review.json`) declares a `panel`
// block, so it is ALWAYS merged into `radio::compile_catalog`'s output
// regardless of `DARKMUX_CREW_DIR` — built-ins are embedded at compile
// time, independent of the user-tier crew dir an isolated TempDir can
// override. There is therefore no environment override that produces a
// genuinely EMPTY catalog (the fail-closed short-circuit
// `radio::route_with_empty_catalog_refuses_without_invoking_call` covers),
// and every non-empty-catalog path — a real route OR a real refusal —
// requires an actual dispatch to the `radio-router` role through a live
// LMStudio instance, which a deterministic test must never depend on
// (verified empirically while writing these tests: a real local run here
// dispatched successfully and routed `"review this for me"` to `/review`
// end-to-end — reassuring, but not something a test suite can rely on
// being true on every machine/CI run). This mirrors the codebase's
// existing precedent: `mission propose` (`src/mission_propose.rs`), the
// other CLI verb built on the same `crate::fleet::dispatch_routed`
// mechanism, likewise has no assert_cmd-level test of its own live
// dispatch — only its pure parsing/validation functions are unit tested.
// `radio`'s full contract (catalog compilation, the frozen prompt
// assembly, all five fail-closed validation paths, the dry-run decision
// shape) is covered at the function level instead, with an injected
// canned model call — see `src/radio.rs::tests` and
// `src/radio_cli.rs::tests`.

/// (#1775) The exit-status belt and the sentence that describes it must
/// agree. The pure `integrity_exit_code` is unit tested in `darkmux-flow`;
/// what is NOT reachable from there is the human output, which is where
/// the first version of this feature printed "exit status stays 0" on a
/// run that exited 2 — a legacy file and a broken file in the same
/// directory. Spawning the binary is the only way to catch that, and the
/// belt previously carried a comment conceding it was review-only.
///
/// Deliberately covers the MIXED case, not the happy one: with only a
/// legacy file present the buggy and fixed versions behave identically,
/// so a single-file test proves nothing.
#[test]
fn integrity_check_never_claims_exit_zero_on_a_run_that_exits_nonzero() {
    let tmp = tempfile::tempdir().unwrap();
    let audit = tmp.path().join("audit");
    std::fs::create_dir_all(&audit).unwrap();

    // Build one legacy file (header marker stripped -> unverifiable) and
    // one genuinely broken file (marker intact, record bytes mutated), by
    // emitting real records and then editing them the way an attacker
    // would rather than hand-rolling the chain format.
    for (day, text) in [("2026-01-01", "alpha"), ("2026-01-02", "bravo")] {
        let staging = tmp.path().join(format!("stage-{day}"));
        std::fs::create_dir_all(&staging).unwrap();
        Command::cargo_bin("darkmux")
            .unwrap()
            .env("DARKMUX_AUDIT_DIR", &staging)
            .args(["flow", "note", "--text", text, "--source", "orchestrator"])
            .assert()
            .success();
        let produced = std::fs::read_dir(&staging)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .expect("flow note must write an audit file");
        let body = std::fs::read_to_string(&produced).unwrap();
        let mut lines: Vec<String> = body.lines().map(str::to_string).collect();
        assert!(lines.len() >= 2, "need a header plus a record: {body}");

        if day == "2026-01-01" {
            // Strip the format marker -> the walk downgrades to legacy and
            // content-verifies nothing.
            lines[0] = lines[0].replace(",\"hash_format\":\"prefix-blake3-v1\"", "");
            assert!(!lines[0].contains("hash_format"), "marker must be gone: {}", lines[0]);
        } else {
            // Mutate the record bytes AFTER the hash prefix -> a real break.
            let sp = lines[1].find(' ').expect("record line is `<hash> <json>`");
            let (hash, rec) = lines[1].split_at(sp);
            lines[1] = format!("{hash}{}", rec.replace("bravo", "BRAVX"));
        }
        std::fs::write(audit.join(format!("{day}.jsonl")), lines.join("\n") + "\n").unwrap();
    }

    let out = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_AUDIT_DIR", &audit)
        .args(["flow", "integrity-check"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert_eq!(
        out.status.code(),
        Some(2),
        "a genuine break must exit 2 even beside an unverifiable file; stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("exit status stays 0"),
        "the run exited 2 — it must not print a claim that the status stays 0; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("BROKEN"),
        "the break must still be reported; stdout:\n{stdout}"
    );

    // The SAME run under --strict. The first fix for this defect missed
    // this branch: a per-file line claiming "(exit 3)" was gated on the
    // `strict` flag rather than on the computed code, so it printed the
    // wrong status beside the tamper signal while the process exited 2.
    // Non-strict coverage alone does not reach it.
    let out = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_AUDIT_DIR", &audit)
        .args(["flow", "integrity-check", "--strict"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert_eq!(
        out.status.code(),
        Some(2),
        "a break outranks an unverifiable file under --strict too; stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("exit 3"),
        "the run exited 2 — no line may name exit 3; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("takes precedence"),
        "the unverifiable file must still be called out, naming the real code; stdout:\n{stdout}"
    );
}

/// (#2093, folded into `flow status` by #1959's flow-hooks-family
/// retirement) `darkmux flow status` wires the hooks section end-to-end:
/// parses, dispatches, and prints valid JSON naming the resolved
/// (disabled-by-default) state — a fresh `DARKMUX_HOME` has no
/// config.json, so hooks are off and no rules are configured.
#[test]
fn flow_status_json_reports_hooks_disabled_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", tmp.path())
        .args(["flow", "status", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON on stdout");
    assert_eq!(v["hooks"]["enabled"], serde_json::Value::Bool(false));
    assert_eq!(v["hooks"]["rules"], serde_json::json!([]));
}

/// The human-formatted form names the disabled state too, without needing
/// `--json`.
#[test]
fn flow_status_human_reports_hooks_disabled_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", tmp.path())
        .args(["flow", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Hooks"))
        .stdout(predicate::str::contains("enabled:      false"));
}

/// Sets a process-global env var and restores its previous value on drop.
///
/// Same shape as `crates/darkmux-serve/src/lib_tests.rs`'s `CrewDirGuard`.
/// Process env is shared by every test in this binary AND inherited by
/// every `assert_cmd` subprocess it spawns, so a test that sets one
/// without restoring it is an order-dependent flake waiting for the next
/// test to be appended below it.
struct EnvVarGuard {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let prev = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, prev }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

/// (#1905) End-to-end conformance for `runs.rs`'s "Two callers, one union"
/// contract: the `darkmux run list` BINARY and a direct
/// `darkmux_serve::build_runs` call must report the same runs for the same
/// on-disk state.
///
/// This is the half `crates/darkmux-serve/src/lib_tests.rs`'s
/// `run_list_verb_and_runs_handler_agree_on_the_same_fixture` explicitly
/// cannot cover: that test proves the HTTP handler does not transform rows
/// on the way out, but it never invokes the verb, so it stays green if the
/// verb stops calling the shared union and starts computing its own. This
/// test spawns the real binary, so a verb that grew a private aggregation,
/// a private filter, or a different input triple fails HERE.
///
/// Every input is pinned by env so both sides read the SAME state: the
/// subprocess gets them as env vars, and the in-process side gets them via
/// `set_var` (hence `#[serial]` — this mutates process-global env).
/// `DARKMUX_HOME` isolates config.json, and `DARKMUX_REDIS_URL` is removed
/// on both sides so the fleet input is an empty slice for each.
#[test]
#[serial_test::serial]
fn run_list_binary_agrees_with_the_shared_union_it_calls() {
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let lab = TempDir::new().unwrap();
    let crew = TempDir::new().unwrap();

    // A lab run is any directory carrying one of the three marker files
    // `scan_lab_runs` matches on (`funnels.json` / `funnel-events.jsonl` /
    // `scores.json`). One is enough to make the union non-empty, which is
    // what stops this test from passing vacuously on two empty lists.
    let run_dir = lab.path().join("case-a/run1");
    fs::create_dir_all(&run_dir).unwrap();
    fs::write(run_dir.join("scores.json"), r#"{"cases":[]}"#).unwrap();

    // A MISSION too, so the fixture spans two kinds. With a single-kind
    // fixture this comparison discriminates on only one axis: a verb-side
    // filter that dropped every mission row would leave both sides at the
    // same lone lab row and pass. `spec.config_id` is deliberately not
    // `"dispatch"` (which is how a crew-of-one dispatch is distinguished,
    // #1509), so this lands as `kind: "mission"`.
    let mission_dir = crew.path().join("missions/conformance-m1");
    fs::create_dir_all(&mission_dir).unwrap();
    fs::write(
        mission_dir.join("mission.json"),
        r#"{
            "id": "conformance-m1",
            "description": "run-list conformance fixture",
            "status": "finalized",
            "phase_ids": [],
            "created_ts": 1700000000,
            "started_ts": 1700000000,
            "finalized_ts": 1700000060,
            "spec": {"config_id": "review", "inputs_fingerprint": "conformance"}
        }"#,
    )
    .unwrap();

    let out = Command::cargo_bin("darkmux")
        .unwrap()
        .args(["run", "list", "--json", "--all"])
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LAB_DIR", lab.path())
        .env("DARKMUX_CREW_DIR", crew.path())
        .env_remove("DARKMUX_REDIS_URL")
        .output()
        .unwrap();
    assert!(out.status.success(), "run list --json failed: {}", String::from_utf8_lossy(&out.stderr));

    let json: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("run list --json emitted non-JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)));
    let mut verb_ids: Vec<String> = json["runs"]
        .as_array()
        .expect("run list --json always emits a runs array")
        .iter()
        .map(|r| format!("{}/{}/{}", r["kind"].as_str().unwrap(), r["status"].as_str().unwrap(), r["id"].as_str().unwrap()))
        .collect();
    verb_ids.sort();

    // Only ONE env var is load-bearing on the in-process side, and it is
    // restored rather than leaked. `build_runs` takes the flows and lab
    // paths as ARGUMENTS, so `DARKMUX_FLOWS_DIR`/`DARKMUX_LAB_DIR` would
    // be inert here; `darkmux-types` is compiled with `test-support` in
    // this binary, so `config()` is empty and `DARKMUX_HOME` buys nothing;
    // and the fleet slice is literally `&[]`, so Redis is never consulted.
    // That leaves `DARKMUX_CREW_DIR`, which `crew_dir_override()` feeds to
    // `load_missions()` — without it the in-process half would read the
    // developer's REAL `~/.darkmux` missions and this comparison would be
    // an accident.
    //
    // The restore is not hygiene theater. `assert_cmd::Command` inherits
    // the parent env, only 15 of this file's ~100 `cargo_bin` call sites
    // set `DARKMUX_HOME` themselves, and `#[serial]` orders this test
    // against the file's other `#[serial]` tests only — the rest run
    // concurrently in this same process. A leaked var pointing at a
    // `TempDir` that has since been deleted turns green tests red
    // depending on declaration order.
    let _crew_guard = EnvVarGuard::set("DARKMUX_CREW_DIR", crew.path());
    let direct = darkmux_serve::build_runs(flows.path(), Some(lab.path()), &[]);
    let mut direct_ids: Vec<String> = direct
        .iter()
        .map(|r| {
            format!(
                "{}/{}/{}",
                serde_json::to_value(r.kind).unwrap().as_str().unwrap(),
                serde_json::to_value(r.status).unwrap().as_str().unwrap(),
                r.id
            )
        })
        .collect();
    direct_ids.sort();

    // Anti-vacuity, on the axis this test actually names: the fixture must
    // produce BOTH kinds, or a same-kind filter on either side would be
    // invisible and the comparison would pass by accident.
    assert!(
        direct_ids.iter().any(|s| s.starts_with("lab/")),
        "fixture produced no lab run — the marker file is no longer recognized, and this test \
         would now pass vacuously on the lab axis: {direct_ids:?}"
    );
    assert!(
        direct_ids.iter().any(|s| s.starts_with("mission/")),
        "fixture produced no mission run — the mission record shape or crew-dir layout changed, \
         and this test would now pass vacuously on the mission axis: {direct_ids:?}"
    );
    assert_eq!(
        verb_ids, direct_ids,
        "`darkmux run list` and darkmux_serve::build_runs disagree on the SAME fixture — the \
         \"one union\" contract (#1905) is broken: the verb is aggregating, filtering, or \
         reading different inputs instead of rendering what the shared union returned"
    );
}

// ── crawl --dry-run (#1959) ──
//
// Migrated from the retired `darkmux crawl plan` verb (deleted alongside
// the standalone CLI family — see `src/cli.rs`'s Crawl retirement commit).
// The equivalent surface today is `darkmux mission launch crawl
// --dry-run`: it resolves + plans exactly the same way, mints nothing,
// and either prints the human plan table (the default) or writes the
// full plan JSON to `--param plan_out=<path>` when the assertion needs
// structured data. `--param workspace=<spec.json>` replaces the old
// positional manifest arg; the manifest/spec JSON SHAPE is unchanged
// (`SourceSpec`/`EdgeSpec` are wire-compatible with the retired
// `CorpusManifest`'s own fields).

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("running git {args:?} in {}: {e}", dir.display()));
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_repo(dir: &std::path::Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "test"]);
}

fn commit_all(dir: &std::path::Path, message: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", message]);
}

/// Build the `app` consumer repo: a `.ts` file with two well-separated
/// `catch` blocks (so their windows don't merge into one site — the
/// prefilter is mechanical and must find both), a `package.json` pinning
/// `@org/lib` to `pin_range`, and a `src/uses-lib.ts` that imports it.
fn write_app_repo(dir: &std::path::Path, pin_range: &str) {
    init_repo(dir);
    fs::write(
        dir.join("package.json"),
        serde_json::json!({"name": "app", "dependencies": {"@org/lib": pin_range}}).to_string(),
    )
    .unwrap();
    fs::create_dir_all(dir.join("src")).unwrap();

    // Two catch sites separated by far more than 2x the built-in rule's
    // window (30 lines each side), so they land as two DISTINCT sites
    // rather than merging into one span.
    let mut lines = vec!["function a() {".to_string(), "  try { risky(); }".to_string(), "  catch (e) { console.error(e); }".to_string(), "}".to_string()];
    for _ in 0..80 {
        lines.push(String::new());
    }
    lines.push("function b() {".to_string());
    lines.push("  try { risky(); }".to_string());
    lines.push("  catch (e) { }".to_string()); // the bare swallow the model would flag
    lines.push("}".to_string());
    fs::write(dir.join("src/x.ts"), lines.join("\n")).unwrap();

    fs::write(
        dir.join("src/uses-lib.ts"),
        "import { thing } from '@org/lib';\nthing();\n",
    )
    .unwrap();

    commit_all(dir, "app: initial");
}

fn write_lib_repo(dir: &std::path::Path, version: &str) {
    init_repo(dir);
    fs::write(
        dir.join("package.json"),
        serde_json::json!({"name": "@org/lib", "version": version, "types": "index.d.ts"}).to_string(),
    )
    .unwrap();
    // A resolvable entry point — #1959 second-round CONSIDER 5 stops
    // emitting an edge unit when `library_surface` is empty, so a stale
    // edge test needs the library to actually have one. `.d.ts` (not
    // `.js`/`.ts`) deliberately: every built-in rule's `exclude` already
    // drops `**/*.d.ts`, so this stays invisible to the site/read rules
    // and doesn't perturb the unit counts those tests assert.
    fs::write(dir.join("index.d.ts"), "export {};\n").unwrap();
    commit_all(dir, "lib: initial");
}

/// (#1959) `SourceSpec`/`EdgeSpec`'s wire shape is unchanged from the
/// retired `CorpusManifest`'s own fields — only the file's ROLE changed
/// (a generic `WorkspaceSpec` any mission can take, not a crawl-only
/// manifest).
fn write_workspace_spec(path: &std::path::Path, root: &std::path::Path, app: &std::path::Path, lib: &std::path::Path) {
    let spec = serde_json::json!({
        "schema_version": "1.0",
        "name": "test-workspace",
        "root": root.to_string_lossy(),
        "sources": [
            {"id": "app", "path": app.to_string_lossy(), "ref": "main"},
            {"id": "lib", "path": lib.to_string_lossy(), "ref": "main"}
        ],
        "edges": [{"consumer": "app", "library": "lib", "package": "@org/lib"}],
        "rules": ["swallowed-error", "doc-contradicts-code", "stale-consumer"]
    });
    fs::write(path, spec.to_string()).unwrap();
}

/// (#2301) `mission launch crawl --dry-run` on the REAL built-in config.
///
/// The retired launcher's own dry run planned in-process and printed a plan
/// table; the six tests that asserted on that table went with it (their
/// subject — the plan shape — is covered directly by `darkmux-lab`'s
/// `crawl::plan` unit tests, which run the same planner without a
/// subprocess). What a dry run proves NOW is the generic thing: which
/// graph this launch would mint.
fn crawl_dry_run(home: &TempDir, spec_path: &std::path::Path, extra: &[&str]) -> std::process::Output {
    let mut args: Vec<String> = vec![
        "mission".into(),
        "launch".into(),
        "crawl".into(),
        "--dry-run".into(),
        "--param".into(),
        format!("workspace={}", spec_path.display()),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    Command::cargo_bin("darkmux")
        .unwrap()
        .args(&args)
        .env("DARKMUX_HOME", home.path())
        .output()
        .expect("mission launch crawl --dry-run runs")
}

#[test]
fn crawl_dry_run_prints_the_graph_the_launch_would_mint() {
    let workdir = TempDir::new().unwrap();
    let app = workdir.path().join("app");
    let lib = workdir.path().join("lib");
    write_app_repo(&app, "^1.0.0");
    write_lib_repo(&lib, "1.2.0");
    let spec_path = workdir.path().join("workspace.json");
    write_workspace_spec(&spec_path, workdir.path(), &app, &lib);

    let out = crawl_dry_run(&workdir, &spec_path, &[]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The document IS the crawl now: its three phases and its per-rule
    // tracks are what a dry run shows.
    for want in ["Plan", "Crawl", "Summarize", "crawl.plan", "crawl.unit", "crawl.summary"] {
        assert!(stdout.contains(want), "dry run never mentioned `{want}`:\n{stdout}");
    }
    // (#2302) The create-mods phase ships OFF, so it is PRUNED at mint and
    // the default graph ends at Summarize — never drawn gray.
    assert!(
        !stdout.contains("Create mods"),
        "the create-mod task is `enabled: false`, so its phase is pruned:\n{stdout}"
    );
    assert!(
        stdout.trim_end().ends_with("[crawl.summary]"),
        "and the default graph still ENDS at the summary, whose output is the close payload:\n{stdout}"
    );
    // Nothing was minted, and nothing was planned.
    assert!(!workdir.path().join("missions").exists(), "a dry run mints nothing");
}

/// (#2302) The same document with the create-mod task turned ON — a
/// user-tier copy of `crawl.json`, which is exactly how an operator enables
/// it — shows the template in the graph it would mint.
#[test]
fn crawl_dry_run_shows_the_create_mod_template_when_a_user_tier_copy_enables_it() {
    let workdir = TempDir::new().unwrap();
    let app = workdir.path().join("app");
    let lib = workdir.path().join("lib");
    write_app_repo(&app, "^1.0.0");
    write_lib_repo(&lib, "1.2.0");
    let spec_path = workdir.path().join("workspace.json");
    write_workspace_spec(&spec_path, workdir.path(), &app, &lib);

    // The built-in document, copied to the user tier with ONE field flipped.
    let mut doc: serde_json::Value = serde_json::from_str(include_str!(
        "../templates/builtin/mission-configs/crawl.json"
    ))
    .expect("the built-in crawl config parses");
    let phases = doc["phases"].as_array_mut().unwrap();
    let create_mods = phases.last_mut().expect("the create-mods phase");
    assert_eq!(create_mods["id"], serde_json::json!("create-mods"));
    create_mods["tasks"][0]["enabled"] = serde_json::json!(true);
    let config_dir = workdir.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("crawl.json"), doc.to_string()).unwrap();

    let out = crawl_dry_run(&workdir, &spec_path, &[]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Create mods"), "the enabled phase is in the graph:\n{stdout}");
    assert!(stdout.contains("dispatch.internal"), "and its step is a coder dispatch:\n{stdout}");
}

#[test]
fn crawl_dry_run_prunes_the_rules_the_launch_did_not_select() {
    let workdir = TempDir::new().unwrap();
    let app = workdir.path().join("app");
    let lib = workdir.path().join("lib");
    write_app_repo(&app, "^1.0.0");
    write_lib_repo(&lib, "1.2.0");
    let spec_path = workdir.path().join("workspace.json");
    write_workspace_spec(&spec_path, workdir.path(), &app, &lib);

    let all = crawl_dry_run(&workdir, &spec_path, &[]);
    let one = crawl_dry_run(&workdir, &spec_path, &["--param", "rules=swallowed-error"]);
    assert!(one.status.success(), "stderr: {}", String::from_utf8_lossy(&one.stderr));
    let all_out = String::from_utf8_lossy(&all.stdout).to_string();
    let one_out = String::from_utf8_lossy(&one.stdout).to_string();

    assert!(all_out.contains("unnamed-predicate"), "the full graph has every rule:\n{all_out}");
    assert!(one_out.contains("swallowed-error"), "the selected rule survives:\n{one_out}");
    assert!(
        !one_out.contains("unnamed-predicate"),
        "a deselected rule is PRUNED, never drawn gray:\n{one_out}"
    );
    // Mutation guard: if the selection stopped pruning, these would match.
    assert_ne!(all_out, one_out, "`--param rules=` must change the minted graph");
}

/// (#2310 P4c review round 2, item (f) — proven) `review-v2.json`'s
/// `bundler` input was documented as "accepted and ignored" with no actual
/// warning — a silent no-op that would leave an operator carrying the
/// funnel's `bundler=` param over from the frozen `review` config with no
/// signal that it does nothing here. `--param bundler=<anything>` must now
/// print a named warning naming what it's ignored for, on the SAME
/// `--dry-run` path (before any real dispatch), so the signal is visible
/// with zero cost.
#[test]
fn review_v2_dry_run_warns_when_bundler_is_passed() {
    let workdir = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let workspace_root = workdir.path().join("tree");
    fs::create_dir_all(&workspace_root).unwrap();
    let spec_path = workdir.path().join("workspace.json");
    fs::write(
        &spec_path,
        serde_json::json!({
            "name": "review-v2-fixture",
            "sources": [{"id": "app", "path": workspace_root.to_string_lossy(), "ref": "main"}]
        })
        .to_string(),
    )
    .unwrap();
    let diff_path = workdir.path().join("d.diff");
    fs::write(&diff_path, "").unwrap();

    let out = Command::cargo_bin("darkmux")
        .unwrap()
        .args([
            "mission",
            "launch",
            "review-v2",
            "--dry-run",
            "--param",
            &format!("workspace={}", spec_path.display()),
            "--param",
            &format!("diff_file={}", diff_path.display()),
            "--param",
            "bundler=/usr/bin/true",
        ])
        .env("DARKMUX_HOME", home.path())
        .output()
        .expect("mission launch review-v2 --dry-run runs");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("bundler") && stderr.contains("ignored"),
        "a `bundler` param on review-v2 must warn that it is ignored: {stderr}"
    );

    // Negative leg: no `bundler` param, no warning.
    let quiet = Command::cargo_bin("darkmux")
        .unwrap()
        .args([
            "mission",
            "launch",
            "review-v2",
            "--dry-run",
            "--param",
            &format!("workspace={}", spec_path.display()),
            "--param",
            &format!("diff_file={}", diff_path.display()),
        ])
        .env("DARKMUX_HOME", home.path())
        .output()
        .expect("mission launch review-v2 --dry-run runs");
    assert!(quiet.status.success(), "stderr: {}", String::from_utf8_lossy(&quiet.stderr));
    let quiet_stderr = String::from_utf8_lossy(&quiet.stderr);
    assert!(
        !quiet_stderr.contains("bundler"),
        "no `bundler` param means no warning: {quiet_stderr}"
    );
}

/// (#2310 P4c-2 item 4 — proven structurally) A SYNTHETIC config (not
/// `review-v2`, not any name the launcher's source recognizes) proves the
/// `"ignored": true` input-declaration flag is honored by ANY config, not
/// detected by matching `config.id`. Both legs: an ignored input supplied
/// warns naming the input and reason; a LIVE (non-ignored) input supplied
/// never warns, even on the same launch.
#[test]
fn an_ignored_input_flag_warns_on_any_config_never_by_id() {
    let home = TempDir::new().unwrap();
    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("synthetic-ignored-test.json"),
        serde_json::json!({
            "id": "synthetic-ignored-test",
            "name": "Synthetic ignored-input test",
            "schema_version": "3.4",
            "inputs": [
                {"name": "legacy_flag", "required": false, "ignored": true, "ignored_reason": "kept for CLI parity only, never read"},
                {"name": "live_flag", "required": false}
            ],
            "phases": []
        })
        .to_string(),
    )
    .unwrap();

    let with_ignored = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", home.path())
        .args([
            "mission",
            "launch",
            "synthetic-ignored-test",
            "--dry-run",
            "--param",
            "legacy_flag=anything",
        ])
        .output()
        .expect("mission launch synthetic-ignored-test --dry-run runs");
    assert!(with_ignored.status.success(), "stderr: {}", String::from_utf8_lossy(&with_ignored.stderr));
    let stderr = String::from_utf8_lossy(&with_ignored.stderr);
    assert!(
        stderr.contains("legacy_flag") && stderr.contains("ignored") && stderr.contains("never read"),
        "an ignored input supplied must warn, naming the input and its reason: {stderr}"
    );

    let with_live = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", home.path())
        .args([
            "mission",
            "launch",
            "synthetic-ignored-test",
            "--dry-run",
            "--param",
            "live_flag=anything",
        ])
        .output()
        .expect("mission launch synthetic-ignored-test --dry-run runs");
    assert!(with_live.status.success(), "stderr: {}", String::from_utf8_lossy(&with_live.stderr));
    let live_stderr = String::from_utf8_lossy(&with_live.stderr);
    assert!(
        !live_stderr.contains("ignored"),
        "a LIVE (non-ignored) input must never warn: {live_stderr}"
    );
}

/// (#2310 P4c-2 item 0 — the P4c-1 BLOCKER, proven) A REAL (non-dry-run)
/// `review-v2` launch — stubbed before dispatch via `DARKMUX_LMS_BIN=/usr/
/// bin/true`, so this proves MINTING, not model behavior — must leave no
/// literal `{{` in ANY minted step's config, in both the statically
/// declared `plan-<rule>-step`s (`{{workspace}}`/`{{diff_file}}`) and the
/// GROWN `unit-<rule>-step`s (`{{intent_file}}`, wired into `grow.config`
/// by this same packet). Before this packet, `crawl_plan_step_overrides`
/// only ever substituted `{{workspace}}`, and only for `kind ==
/// "crawl.plan"` — `review-v2.json`'s `plan.sites` steps got NO
/// substitution on a real launch, so this is the fix's own regression
/// test, not incidental coverage.
#[test]
fn review_v2_real_launch_leaves_no_literal_braces_in_any_minted_step_config() {
    let workdir = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let app = workdir.path().join("app");
    write_app_repo(&app, "^1.0.0");
    // A real `git diff` between the empty tree and the fully-populated
    // `write_app_repo` state — the tree materializes at HEAD (the "after"
    // state), and the diff's added lines are exactly that content, so
    // `DiffSource`'s tree-agreement check passes and the bare
    // `catch (e) { }` in `src/x.ts` is a genuine `swallowed-error` hit.
    let empty_tree = std::process::Command::new("git")
        .current_dir(&app)
        .args(["hash-object", "-t", "tree", "/dev/null"])
        .output()
        .unwrap();
    let empty_tree_sha = String::from_utf8_lossy(&empty_tree.stdout).trim().to_string();
    let diff_out = std::process::Command::new("git")
        .current_dir(&app)
        .args(["diff", &empty_tree_sha, "HEAD"])
        .output()
        .unwrap();
    assert!(diff_out.status.success(), "{}", String::from_utf8_lossy(&diff_out.stderr));
    let diff_text = String::from_utf8_lossy(&diff_out.stdout).to_string();
    assert!(
        diff_text.contains("catch (e) { }"),
        "the fixture's bare (swallowed) catch must appear in the diff: {diff_text}"
    );

    let spec_path = workdir.path().join("workspace.json");
    fs::write(
        &spec_path,
        serde_json::json!({
            "name": "review-v2-real-launch",
            "sources": [{"id": "app", "path": app.to_string_lossy(), "ref": "main"}]
        })
        .to_string(),
    )
    .unwrap();
    let diff_path = workdir.path().join("d.diff");
    fs::write(&diff_path, &diff_text).unwrap();
    let intent_path = workdir.path().join("intent.md");
    fs::write(&intent_path, "Fix the swallowed catch in src/x.ts.").unwrap();

    let out = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args([
            "mission",
            "launch",
            "review-v2",
            "--param",
            &format!("workspace={}", spec_path.display()),
            "--param",
            &format!("diff_file={}", diff_path.display()),
            "--param",
            "rules=swallowed-error",
            "--param",
            &format!("intent_file={}", intent_path.display()),
        ])
        .output()
        .expect("mission launch review-v2 runs");
    let combined =
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));

    let mission_dir = one_mission_dir(&home);
    let steps_dir = mission_dir.join("steps");
    assert!(steps_dir.exists(), "no steps/ dir was written:\n{combined}");

    let mut all_configs: Vec<(String, serde_json::Value)> = Vec::new();
    let mut saw_plan_step = false;
    let mut saw_grown_unit_step = false;
    for phase_entry in fs::read_dir(&steps_dir).unwrap() {
        let phase_dir = phase_entry.unwrap().path();
        if !phase_dir.is_dir() {
            continue;
        }
        for step_entry in fs::read_dir(&phase_dir).unwrap() {
            let path = step_entry.unwrap().path();
            let step: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            // Keyed on `kind`, not `id` — the id carries the composed real
            // PHASE id as its prefix (`substitute_id`), not the document's
            // bare `plan-`/`unit-` prefix.
            if step["kind"] == serde_json::json!("plan.sites") {
                saw_plan_step = true;
                assert_eq!(
                    step["config"]["workspace"],
                    serde_json::json!(spec_path.to_string_lossy()),
                    "the plan step's `{{{{workspace}}}}` must resolve to the real path: {step}"
                );
                assert_eq!(
                    step["config"]["diff_file"],
                    serde_json::json!(diff_path.to_string_lossy()),
                    "the plan step's `{{{{diff_file}}}}` must resolve to the real path: {step}"
                );
                assert!(
                    step["config"].get("head_sha").is_none(),
                    "an unset optional input's placeholder key must be OMITTED, not an empty string: {step}"
                );
            }
            if step["kind"] == serde_json::json!("crawl.unit") && step["config"].get("grown_from").is_some() {
                saw_grown_unit_step = true;
                assert_eq!(
                    step["config"]["intent_file"],
                    serde_json::json!(intent_path.to_string_lossy()),
                    "a GROWN unit step's `{{{{intent_file}}}}` must resolve too, not just static tasks: {step}"
                );
            }
            all_configs.push((path.to_string_lossy().to_string(), step["config"].clone()));
        }
    }
    assert!(saw_plan_step, "no `plan-*` step was minted:\n{combined}");
    assert!(
        saw_grown_unit_step,
        "the fixture's bare catch must have grown at least one `unit-swallowed-error-*` step:\n{combined}"
    );

    let mut braces: Vec<String> = Vec::new();
    for (path, config) in &all_configs {
        darkmux_crew::mission_config::find_unsubstituted_braces(config, path, &mut braces);
    }
    assert!(braces.is_empty(), "literal `{{{{` survived minting:\n{}", braces.join("\n"));
}


#[test]
fn a_real_crawl_plan_step_grows_one_task_per_planned_unit() {
    // (#2301) The whole plan→grow seam end to end through the CLI, with a
    // REAL `crawl.plan` step over a real git fixture and a
    // `procedural.noop` standing in for the unit dispatch (no model, no
    // container — the live crawl is the operator's own proof).
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let app = home.path().join("app");
    write_app_repo(&app, "^1.0.0");

    let spec_path = home.path().join("workspace.json");
    fs::write(
        &spec_path,
        serde_json::json!({
            "name": "grow-e2e",
            "root": home.path().join("ws").to_string_lossy(),
            "sources": [{"id": "app", "path": app.to_string_lossy(), "ref": "main"}]
        })
        .to_string(),
    )
    .unwrap();

    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("crawl-e2e.json"),
        serde_json::json!({
            "id": "crawl-e2e",
            "name": "Crawl E2E",
            "schema_version": "3.2",
            "inputs": [{"name": "workspace", "required": true}],
            "phases": [
                {"id": "plan", "tasks": [{
                    "id": "plan-swallowed-error",
                    "steps": [{"id": "plan-swallowed-error-step", "kind": "crawl.plan",
                               "config": {"rule": "swallowed-error", "workspace": "{{workspace}}"}}]
                }]},
                {"id": "units", "tasks": [{
                    "id": "unit",
                    "depends_on": ["plan-swallowed-error"],
                    "grow": {"from": "plan-swallowed-error", "items": "units", "id": "{{item.id}}",
                             "config": {"plan": "{{from.output}}", "unit": "{{item.id}}"}},
                    "steps": [{"id": "unit-step", "kind": "procedural.noop", "config": {}}]
                }]}
            ]
        })
        .to_string(),
    )
    .unwrap();

    let out = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args([
            "mission",
            "launch",
            "crawl-e2e",
            "--param",
            &format!("workspace={}", spec_path.display()),
            "--param",
            "no_fetch=true",
        ])
        .output()
        .expect("mission launch crawl-e2e runs");
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("grew"), "the plan's units grew into tasks:\n{stdout}");

    // The plan landed under the run, WRAPPED, and the graph report says
    // what grew from it.
    let mission_dir = one_mission_dir(&home);
    let plan_path = mission_dir.join("plan").join("swallowed-error.json");
    let plan: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&plan_path).expect("the plan was written")).unwrap();
    assert_eq!(plan["kind"], serde_json::json!("crawl.plan"), "the plan is a typed output envelope");
    assert!(!plan["hash"].as_str().unwrap_or("").is_empty(), "and carries its body's digest");
    let units = plan["body"]["units"].as_array().expect("the body holds the units");
    assert!(!units.is_empty(), "the fixture's swallowed catch was planned: {plan}");

    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(mission_dir.join("graph-report.json")).unwrap()).unwrap();
    let grown = report["grown"].as_array().expect("growth is recorded");
    assert_eq!(grown[0]["items"].as_u64().unwrap() as usize, units.len());
    assert_eq!(grown[0]["minted"].as_array().unwrap().len(), units.len(), "one task per unit");
    assert_eq!(grown[0]["from"], serde_json::json!("plan-swallowed-error"));
}

/// (#2302) `mission config show crawl --json` says which tasks the mint
/// would prune. The create-mod task ships OFF; every other task in the document
/// declares no gate at all and runs.
#[test]
fn mission_config_show_names_the_create_mod_task_as_disabled() {
    let home = TempDir::new().unwrap();
    let out = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "config", "show", "crawl", "--json"])
        .output()
        .expect("mission config show crawl --json runs");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("--json is JSON");
    let phases = v["phases"].as_array().expect("phases");
    let create_mods = phases.last().expect("the create-mods phase");
    assert_eq!(create_mods["id"], serde_json::json!("create-mods"));
    assert_eq!(
        create_mods["tasks"][0]["enabled"],
        serde_json::json!(false),
        "the gate is VISIBLE, not inferred from the description: {create_mods}"
    );
    for phase in &phases[..phases.len() - 1] {
        for task in phase["tasks"].as_array().unwrap() {
            assert_eq!(
                task["enabled"],
                serde_json::Value::Null,
                "a task that declares no gate reports none: {task}"
            );
        }
    }
}

/// (#2302) The FOLLOW-ON seam end to end through the CLI: a producer whose
/// wrapped output names findings, and a template that grows one
/// `dispatch.internal` step per finding carrying that finding's key in
/// `config.brief_refs`.
///
/// The finding store is left EMPTY on purpose. `brief_refs` resolution
/// refuses a key that addresses no stored record, and it refuses BEFORE any
/// model or container work, so the refusal is both the proof that the
/// substituted ref reached `dispatch.internal` and the proof of the
/// readiness guard the create-mods phase leans on — with no docker, no model
/// and no dispatch anywhere in this test.
#[test]
#[serial_test::serial]
fn a_template_grows_one_dispatch_per_finding_carrying_its_key_in_brief_refs() {
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let findings = TempDir::new().unwrap();

    // The producer's output: the SAME envelope `crawl.summary` writes — a
    // wrapped body whose top-level `finding_refs` is the array to map over.
    let summary_body = serde_json::json!({
        "findings": 2,
        "finding_refs": [
            {"key": "sess-a/1", "id": "sess-a-1", "file": "src/a.ts", "line": 7,
             "rule": "unnamed-predicate", "tree_root": home.path().to_string_lossy()},
            {"key": "sess-a/2", "id": "sess-a-2", "file": "src/b.ts", "line": 9,
             "rule": "unnamed-predicate", "tree_root": home.path().to_string_lossy()},
        ],
    });
    let summary_output = serde_json::json!({
        "schema_version": "1.0",
        "kind": "crawl.summary",
        "producer": {"mission": "m", "task": "summary", "step": "summary-step", "machine_id": "t"},
        "produced_at": "2026-09-04T00:00:00Z",
        "body": summary_body,
    })
    .to_string();

    // An ENDPOINT profile, so nothing here is a local placement and the
    // scheduler never tries to make a model resident. Nothing is ever
    // called at this URL: `brief_refs` resolution refuses first.
    let profiles = home.path().join("profiles.json");
    fs::write(
        &profiles,
        serde_json::json!({
            "schema_version": "1.5",
            "default_profile": "stub",
            "profiles": {"stub": {"models": [
                {"id": "stub-model", "n_ctx": 8000, "endpoint": {"url": "http://127.0.0.1:9"}}
            ]}}
        })
        .to_string(),
    )
    .unwrap();

    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("follow-on-e2e.json"),
        serde_json::json!({
            "id": "follow-on-e2e",
            "name": "Follow-on E2E",
            "schema_version": "3.2",
            "inputs": [],
            "phases": [
                {"id": "summarize", "tasks": [{
                    "id": "summary",
                    "steps": [{"id": "summary-step", "kind": "procedural.noop",
                               "config": {"output": summary_output}}]
                }]},
                {"id": "follow-on", "tasks": [{
                    "id": "follow-on",
                    "role_id": "coder",
                    "depends_on": ["summary"],
                    "grow": {"from": "summary", "items": "finding_refs", "id": "{{item.id}}",
                             "config": {
                                 "workdir": "{{item.tree_root}}",
                                 "brief_refs": [{"kind": "finding", "key": "{{item.key}}"}],
                                 "message": "make the change the finding describes",
                                 "profile_name": "stub",
                                 "skip_preflight": true
                             }},
                    "steps": [{"id": "follow-on-step", "kind": "dispatch.internal", "config": {}}]
                }]}
            ]
        })
        .to_string(),
    )
    .unwrap();

    let out = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_FINDINGS_DIR", findings.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .env("DARKMUX_PROFILES", &profiles)
        .args(["mission", "launch", "follow-on-e2e"])
        .output()
        .expect("mission launch follow-on-e2e runs");
    let combined =
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));

    // Two tasks grew, from the summary, one per finding.
    let mission_dir = one_mission_dir(&home);
    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(mission_dir.join("graph-report.json")).unwrap()).unwrap();
    let grown = report["grown"].as_array().expect("growth is recorded");
    assert_eq!(grown.len(), 1, "{report}");
    assert_eq!(grown[0]["from"], serde_json::json!("summary"), "grown from the SUMMARY: {report}");
    assert_eq!(grown[0]["items"].as_u64().unwrap(), 2);
    let minted: Vec<&str> = grown[0]["minted"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(minted.len(), 2, "{report}");
    assert!(minted[0].ends_with("follow-on-sess-a-1"), "the id is `/`-free: {report}");
    assert!(minted[1].ends_with("follow-on-sess-a-2"), "the id is `/`-free: {report}");
    assert!(minted.iter().all(|m| !m.contains('/')), "a task id is one segment: {report}");

    // Each grown step config carries the SUBSTITUTED ref and its provenance.
    let steps_dir = mission_dir.join("steps");
    let phase_dir = fs::read_dir(&steps_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.file_name().unwrap().to_string_lossy().ends_with("-follow-on"))
        .expect("a follow-on phase dir under steps/");
    let mut seen: Vec<String> = Vec::new();
    let mut refusals: Vec<String> = Vec::new();
    for entry in fs::read_dir(&phase_dir).unwrap() {
        let step: serde_json::Value = serde_json::from_str(&fs::read_to_string(entry.unwrap().path()).unwrap()).unwrap();
        assert_eq!(step["kind"], serde_json::json!("dispatch.internal"));
        assert_eq!(step["status"], serde_json::json!("error"), "an unresolvable ref refuses the step: {step}");
        refusals.push(step["output"].as_str().unwrap_or_default().to_string());
        assert_eq!(step["config"]["grown_from"]["task"], serde_json::json!("summary"));
        let refs = step["config"]["brief_refs"].as_array().expect("the grown ref");
        assert_eq!(refs.len(), 1, "{step}");
        assert_eq!(refs[0]["kind"], serde_json::json!("finding"));
        seen.push(refs[0]["key"].as_str().unwrap().to_string());
        assert_eq!(
            step["config"]["workdir"],
            serde_json::json!(home.path().to_string_lossy()),
            "the finding's own tree is the follow-on's workdir: {step}"
        );
    }
    seen.sort();
    assert_eq!(seen, vec!["sess-a/1".to_string(), "sess-a/2".to_string()], "one step per finding, keyed");

    // And with nothing in the store, each step REFUSES BY NAME — before any
    // container work, which is the readiness guard stated out loud.
    assert!(
        combined.contains("2 errored"),
        "both follow-on steps must refuse, and the run must say so:\n{combined}"
    );
    let all = refusals.join("\n");
    for key in ["sess-a/1", "sess-a/2"] {
        assert!(all.contains(key), "the refusal names the key it could not resolve:\n{all}");
    }
    assert!(all.contains("no finding"), "and names the STORE it looked in:\n{all}");
    let lower = all.to_lowercase();
    assert!(!lower.contains("docker"), "nothing downstream of resolution was reached:\n{all}");
}

// ─── `finding` family (#2265) ────────────────────────────────────────────
//
// The finding record is what was observed: an event, keyed `<dispatch>/<seq>`,
// written once and never rewritten. `finding sync` is the SECOND producer —
// it replays the flow stream for anything the live tailer missed (an older
// binary, a killed process) and must be idempotent, because the tailer and it
// race by design.

/// One flow day file holding the three shapes `sync` has to tell apart: an
/// accepted `create_finding` with an emission, the pre-2026-09-03
/// `report_finding` name (historical records carry it; the stream is
/// append-only), and an accepted call from a runtime that predates FLOW
/// 1.33.0 and therefore carried no `emitted` at all.
fn write_finding_day_file(flows: &std::path::Path) {
    fs::create_dir_all(flows).unwrap();
    // `mission_id` / `phase_id` are TOP-LEVEL on a flow record; the crawl's
    // `context` is the launcher's blob (workspace / source / sha / rule / unit)
    // and carries no mission. A fixture that put the mission inside `context`
    // would test a shape the producer never emits.
    let rec = |ts: &str, sess: &str, tool: &str, seq: u64, mission: &str, emitted: Option<serde_json::Value>| {
        let mut payload = serde_json::json!({
            "tool_name": tool, "ok": true, "args": "{}",
            "context": {"unit": "u1", "rule": "unnamed-predicate", "source": "acme"},
        });
        if let Some(e) = emitted {
            payload["emitted"] = e;
            payload["emit_seq"] = serde_json::json!(seq);
        }
        serde_json::json!({
            "ts": ts, "level": "info", "category": "work", "tier": "local",
            "stage": "dispatch", "action": "dispatch.tool", "handle": "crawler",
            "session_id": sess, "model": "darkmux:qwen3.6", "machine_id": "test-machine",
            "mission_id": mission, "phase_id": format!("{mission}-crawl"),
            "payload": payload,
        })
        .to_string()
    };
    let lines = [
        rec("2026-09-03T01:00:00Z", "sess-a", "create_finding", 1, "crawl-1",
            Some(serde_json::json!({"file": "a.ts", "line": 4, "why": "unnamed operands"}))),
        rec("2026-09-03T02:00:00Z", "sess-b", "report_finding", 2, "crawl-2",
            Some(serde_json::json!({"file": "b.ts", "line": 9}))),
        // Pre-FLOW-1.33.0: no `emitted` key at all — in the stream, not a record.
        rec("2026-09-03T03:00:00Z", "sess-c", "create_finding", 3, "crawl-1", None),
    ];
    fs::write(flows.join("2026-09-03.jsonl"), lines.join("\n") + "\n").unwrap();
}

#[test]
fn finding_sync_materializes_then_is_idempotent_and_list_show_read_the_store() {
    let home = TempDir::new().unwrap();
    let flows = home.path().join("flows");
    write_finding_day_file(&flows);

    let dm = |args: &[&str]| {
        Command::cargo_bin("darkmux")
            .unwrap()
            .env("DARKMUX_HOME", home.path())
            .env("DARKMUX_FLOWS_DIR", &flows)
            .env("DARKMUX_LMS_BIN", "/usr/bin/true")
            .args(args)
            .output()
            .expect("darkmux runs")
    };

    // First pass: two records made, one call that cannot become one.
    let out = dm(&["finding", "sync", "--json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout))
        .expect("finding sync --json emits JSON");
    assert_eq!(v["created"], 2, "got: {v}");
    assert_eq!(v["present"], 0, "got: {v}");
    assert_eq!(v["skipped_no_emission"], 1, "the pre-1.33.0 call cannot become a record: {v}");
    assert_eq!(v["scanned"], 3, "got: {v}");

    // On disk where the key says, one file per finding.
    assert!(home.path().join("findings/sess-a/1/finding.json").exists());
    assert!(home.path().join("findings/sess-b/2/finding.json").exists());
    assert!(!home.path().join("findings/sess-c").exists());

    // Second pass: idempotent. Nothing new, both already present.
    let out = dm(&["finding", "sync", "--json"]);
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert_eq!(v["created"], 0, "a second sync must create nothing: {v}");
    assert_eq!(v["present"], 2, "got: {v}");

    // The human output NAMES the calls that cannot become records, rather
    // than dropping them silently.
    let out = dm(&["finding", "sync"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no emission"), "human output must name the skip: {stdout}");

    // `finding list` reads the STORE, one line per finding, ts-ascending.
    let out = dm(&["finding", "list"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let rows: Vec<&str> = stdout.lines().filter(|l| l.contains("sess-")).collect();
    assert_eq!(rows.len(), 2, "one line per finding: {stdout}");
    assert!(rows[0].contains("sess-a/1"), "ts-ascending, keyed: {stdout}");
    assert!(rows[0].contains("crawler"), "the proposer is named: {stdout}");

    let out = dm(&["finding", "list", "--json"]);
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    let arr = v["findings"].as_array().expect("findings array");
    assert_eq!(arr.len(), 2, "got: {v}");
    assert_eq!(arr[0]["emitted"]["file"], "a.ts", "the emission rides whole: {v}");

    // The record carries the mission the dispatch ran under as its OWN field —
    // never read out of the launcher's context blob, which has no mission in it.
    assert_eq!(arr[0]["mission_id"], "crawl-1", "got: {v}");
    assert_eq!(arr[0]["phase_id"], "crawl-1-crawl", "got: {v}");
    assert!(
        arr[0]["context"].get("mission_id").is_none(),
        "the launcher's context stays verbatim — no mission injected into it: {v}"
    );

    // `--mission` selects on that field. THE #2288 live-proof gap: sync made
    // every record, `--rule` matched them all, and `--mission` matched none,
    // because the filter read a key `context` never holds.
    let mission_ids = |args: &[&str]| -> Vec<String> {
        let out = dm(args);
        let v: serde_json::Value =
            serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
        v["findings"]
            .as_array()
            .expect("findings array")
            .iter()
            .map(|f| f["key"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        mission_ids(&["finding", "list", "--mission", "crawl-1", "--json"]),
        vec!["sess-a/1".to_string()],
        "--mission must return exactly that mission's findings"
    );
    assert_eq!(
        mission_ids(&["finding", "list", "--mission", "crawl-2", "--json"]),
        vec!["sess-b/2".to_string()],
    );
    assert!(
        mission_ids(&["finding", "list", "--mission", "no-such-mission", "--json"]).is_empty(),
        "an unknown mission returns none"
    );

    // `--dispatch` narrows to one dispatch. Unpinned, the filter could be
    // `.filter(|_| true)` and nothing would notice.
    assert_eq!(
        mission_ids(&["finding", "list", "--dispatch", "sess-b", "--json"]),
        vec!["sess-b/2".to_string()],
        "--dispatch must return exactly that dispatch's findings"
    );
    assert!(
        mission_ids(&["finding", "list", "--dispatch", "sess-nope", "--json"]).is_empty(),
        "an unknown dispatch returns none"
    );
    // …and the three filters compose rather than replacing each other.
    assert!(
        mission_ids(&["finding", "list", "--mission", "crawl-1", "--dispatch", "sess-b", "--json"])
            .is_empty(),
        "filters compose: sess-b is not in crawl-1"
    );

    // A filter that matches nothing must not read like an EMPTY STORE — the
    // remedy for the two is different ("your filter matched nothing" vs "run
    // sync").
    let out = dm(&["finding", "list", "--mission", "no-such-mission"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no findings match"), "got:\n{stdout}");
    assert!(
        !stdout.contains("finding sync"),
        "the store is NOT empty — do not tell the operator to sync: {stdout}"
    );

    // A malformed --since would match no day file and exit clean, which reads
    // exactly like "there are no findings". It must refuse instead.
    let out = dm(&["finding", "sync", "--since", "last-tuesday"]);
    assert_ne!(out.status.code(), Some(0), "a non-date --since must not exit clean");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("YYYY-MM-DD"),
        "the error names the shape: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A key that would escape the store is refused, not resolved — proved
    // against a record that DOES exist at the escaped path. The store is
    // `<home>/findings`, so `../x/1` addresses `<home>/x/1`, the store's own
    // parent. Without the key check this read would succeed and print a file
    // from outside the store; an assertion that planted nothing there would
    // have passed on the absence instead of on the refusal.
    fs::create_dir_all(home.path().join("x/1")).unwrap();
    fs::write(
        home.path().join("x/1/finding.json"),
        serde_json::json!({
            "key": "x/1", "dispatch": "x", "seq": 1, "ts": "2026-09-03T00:00:00Z",
            "tool_name": "create_finding",
            "proposer": {"handle": "outside-the-store", "model": "m"},
            "context": serde_json::Value::Null,
            "emitted": {"file": "ESCAPED-THE-STORE.ts"},
            "schema_version": "1",
        })
        .to_string(),
    )
    .unwrap();
    let out = dm(&["finding", "show", "../x/1"]);
    assert_eq!(out.status.code(), Some(1), "a traversal key must not resolve");
    let escaped_stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        escaped_stdout.is_empty(),
        "nothing from outside the store is printed: {escaped_stdout}"
    );
    let escaped_stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        escaped_stderr.contains("not a finding key"),
        "the key is INVALID, not merely missing — a 'no finding <key>' message would mean \
the key was ACCEPTED and only the file happened to be absent: {escaped_stderr}"
    );
    assert!(!escaped_stderr.contains("ESCAPED-THE-STORE"), "got: {escaped_stderr}");

    // The human list names the mission when the finding has one.
    let out = dm(&["finding", "list", "--mission", "crawl-1"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("crawl-1"), "the mission is shown: {stdout}");

    // Filters otherwise read the record's own context — never the emission.
    let out = dm(&["finding", "list", "--rule", "nope", "--json"]);
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert!(v["findings"].as_array().unwrap().is_empty(), "got: {v}");

    // `finding show` prints ONE record, addressed by its key.
    let out = dm(&["finding", "show", "sess-a/1"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("sess-a/1"), "got:\n{stdout}");
    assert!(stdout.contains("a.ts"), "the emission is shown: {stdout}");
    assert!(stdout.contains("crawl-1"), "show names the mission: {stdout}");
    assert!(!stdout.contains("sess-b"), "show is ONE record: {stdout}");

    // A missing key is an error with a clear message, not an empty success.
    let out = dm(&["finding", "show", "sess-a/99"]);
    assert_eq!(out.status.code(), Some(1), "a missing finding exits 1");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("sess-a/99"), "the message names the key: {stderr}");
}

// ─── `mod` family (#2265) ────────────────────────────────────────────────
//
// A mod is how something COULD change: a KIT of instructions plus data, in
// whatever form the proposer chose. darkmux never types a kit and never opens
// it. The key is MINTED per mod, never derived from a finding, so two agents
// proposing for one observation produce two records rather than one
// overwriting the other. The view from a finding to its mods is DERIVED by
// scanning mods — nothing is written back onto the finding.

#[test]
fn mod_create_mints_per_call_copies_attachments_and_finding_show_lists_the_mods() {
    let home = TempDir::new().unwrap();
    let flows = home.path().join("flows");
    write_finding_day_file(&flows);

    let dm = |args: &[&str]| {
        Command::cargo_bin("darkmux")
            .unwrap()
            .env("DARKMUX_HOME", home.path())
            .env("DARKMUX_FLOWS_DIR", &flows)
            .env("DARKMUX_LMS_BIN", "/usr/bin/true")
            .args(args)
            .output()
            .expect("darkmux runs")
    };
    // Same invocation, with a kit piped on stdin.
    let dm_stdin = |args: &[&str], stdin: &str| {
        Command::cargo_bin("darkmux")
            .unwrap()
            .env("DARKMUX_HOME", home.path())
            .env("DARKMUX_FLOWS_DIR", &flows)
            .env("DARKMUX_LMS_BIN", "/usr/bin/true")
            .args(args)
            .write_stdin(stdin)
            .output()
            .expect("darkmux runs")
    };

    // The findings the mods will name have to exist first.
    assert!(dm(&["finding", "sync"]).status.success());

    // ── create, from stdin ────────────────────────────────────────────────
    let out = dm_stdin(
        &["mod", "create", "--by", "sonnet", "--for", "sess-a/1", "--kit", "-"],
        "rename the predicate, then add a test\n",
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    // stdout is the KEY, alone, on the last line — the orchestrator pipes it
    // straight into `mod show <key>` / `--for`. Anything else it has to say
    // (the path it wrote, a missing `for`) goes to stderr, so `$(...)` around
    // this command captures a key and never a path.
    let key_a = String::from_utf8_lossy(&out.stdout).lines().last().unwrap().trim().to_string();
    assert!(key_a.starts_with("mod-"), "create prints the minted key: {key_a}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        key_a,
        "stdout is the key ALONE — a path on it would be captured by `$(darkmux mod create …)`"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("mod.json"),
        "the path it wrote is still reported, on stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The captured line is directly usable as an address.
    let out_show = dm(&["mod", "show", &key_a]);
    assert!(
        out_show.status.success(),
        "`mod show $(darkmux mod create …)` must work: {}",
        String::from_utf8_lossy(&out_show.stderr)
    );
    let rec: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(home.path().join("mods").join(&key_a).join("mod.json")).unwrap(),
    )
    .expect("the record is on disk where the key says");
    assert_eq!(rec["by"], "sonnet");
    assert_eq!(rec["for"], serde_json::json!(["sess-a/1"]));
    // The kit is kept VERBATIM — prose stays the prose that was written.
    assert_eq!(rec["kit"], "rename the predicate, then add a test\n");
    // The named finding's own provenance is copied on, so a reader of the mod
    // never has to go find the finding.
    assert_eq!(rec["context"]["findings"][0]["mission_id"], "crawl-1");
    assert_eq!(rec["context"]["findings"][0]["emitted"]["file"], "a.ts");

    // ── create, from a file, with two attachments ─────────────────────────
    let src = home.path().join("src");
    fs::create_dir_all(&src).unwrap();
    // Duplicate keys and an integer past f64 — both survive a byte-exact
    // store and neither survives a parse/re-serialize round trip.
    let kit_text = "{\n  \"diff\": \"--- a\",\n  \"diff\": \"+++ b\",\n  \"n\": 12345678901234567890123\n}\n";
    fs::write(src.join("kit.json"), kit_text).unwrap();
    fs::write(src.join("patch.diff"), b"--- a\n+++ b\n").unwrap();
    fs::write(src.join("shot.png"), [0x89u8, 0x50, 0x4e, 0x47, 0x0d]).unwrap();
    let out = dm(&[
        "mod", "create", "--by", "kain", "--for", "sess-b/2",
        "--kit", src.join("kit.json").to_str().unwrap(),
        "--attach", src.join("patch.diff").to_str().unwrap(),
        "--attach", src.join("shot.png").to_str().unwrap(),
    ]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let key_b = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let attach_dir = home.path().join("mods").join(&key_b).join("attachments");
    assert_eq!(
        fs::read(attach_dir.join("patch.diff")).unwrap(),
        b"--- a\n+++ b\n",
        "an attachment is copied byte for byte"
    );
    assert_eq!(fs::read(attach_dir.join("shot.png")).unwrap(), [0x89u8, 0x50, 0x4e, 0x47, 0x0d]);
    // A JSON-looking kit is stored as the RAW TEXT, byte for byte — parsing
    // and re-serializing it would collapse duplicate keys and round large
    // integers, so a kit is never parsed on write.
    let rec_b: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(home.path().join("mods").join(&key_b).join("mod.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        rec_b["kit"].as_str().expect("the kit is ALWAYS a string"),
        kit_text,
        "byte-exact: the file's own bytes, not a re-serialization"
    );
    assert_eq!(rec_b["kit_looks_json"], true, "a reader hint, computed once at write time");
    assert_eq!(rec_b["attachments"], serde_json::json!(["patch.diff", "shot.png"]));

    // ── two creates for ONE finding are two mods ──────────────────────────
    let out = dm_stdin(
        &["mod", "create", "--by", "kain", "--for", "sess-a/1", "--kit", "-"],
        "just add a comment",
    );
    let key_a2 = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_ne!(key_a, key_a2, "the key is MINTED per mod — the second must not overwrite the first");
    assert!(home.path().join("mods").join(&key_a).join("mod.json").exists());
    assert!(home.path().join("mods").join(&key_a2).join("mod.json").exists());

    // A `for` key with no stored finding is allowed, recorded, and NAMED.
    let out = dm_stdin(
        &["mod", "create", "--by", "kain", "--for", "sess-z/9", "--kit", "-"],
        "for something not in the store",
    );
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let key_z = stdout.trim().to_string();
    assert!(key_z.starts_with("mod-"), "stdout is still the key alone: {stdout}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("sess-z/9"),
        "a missing finding is named, not silent — on stderr, so stdout stays pipeable: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // `--json` is where the path lives, beside the record itself.
    let out = dm_stdin(
        &["mod", "create", "--by", "kain", "--kit", "-", "--json"],
        "a standalone kit",
    );
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("--json emits JSON");
    let key_j = v["record"]["key"].as_str().expect("the key").to_string();
    let path_j = home.path().join("mods").join(&key_j).join("mod.json");
    assert_eq!(v["path"].as_str().expect("--json carries the path"), path_j.to_str().unwrap());
    assert_eq!(v["record"]["by"], "kain", "the whole record is there: {v}");
    // `path` sits BESIDE the record, never inside it: the printed record has
    // to carry exactly the stored record's fields and nothing darkmux added.
    // Compared as VALUES (both parsed) — this pins the field set, not the
    // formatting, which pretty-printing changes either way.
    let on_disk: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path_j).unwrap()).unwrap();
    assert_eq!(v["record"], on_disk, "the printed record has the stored record's fields");

    // A non-canonical finding key is CANONICALIZED on create, so one finding
    // has one address: `sess-a/01` must be findable as `sess-a/1` by both the
    // filter and the finding's own derived section.
    let out = dm_stdin(
        &["mod", "create", "--by", "zero-padded", "--for", "sess-a/01", "--kit", "-"],
        "same finding, padded key",
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let key_pad = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let rec_pad: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(home.path().join("mods").join(&key_pad).join("mod.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(rec_pad["for"], serde_json::json!(["sess-a/1"]), "stored canonical");

    // A key that can address no finding is refused loudly, not stored as a
    // link nothing can follow.
    let out = dm_stdin(&["mod", "create", "--by", "kain", "--for", "no-slash", "--kit", "-"], "x");
    assert_ne!(out.status.code(), Some(0), "an unaddressable --for must not be stored");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("finding key"),
        "the error names the shape: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A mod with neither instructions nor data is not a kit.
    let out = dm(&["mod", "create", "--by", "kain"]);
    assert_ne!(out.status.code(), Some(0), "a mod needs a kit or an attachment");

    // ── list ──────────────────────────────────────────────────────────────
    let keys = |args: &[&str]| -> Vec<String> {
        let out = dm(args);
        let v: serde_json::Value =
            serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("--json emits JSON");
        v["mods"].as_array().unwrap().iter().map(|m| m["key"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(keys(&["mod", "list", "--json"]).len(), 6, "every mod, ts-ascending");
    let for_a = keys(&["mod", "list", "--for", "sess-a/1", "--json"]);
    assert_eq!(for_a.len(), 3, "one observation can attract competing changes");
    assert!(
        for_a.contains(&key_pad),
        "the canonicalized `sess-a/01` mod is found under `sess-a/1`: {for_a:?}"
    );
    // The QUERY is canonicalized too. Canonicalizing only on write left one
    // finding with two addresses from the reader's side: the mod created with
    // `--for sess-a/01` was invisible to `--for sess-a/01`.
    assert_eq!(
        keys(&["mod", "list", "--for", "sess-a/01", "--json"]),
        for_a,
        "a non-canonical query returns exactly what the canonical one does"
    );
    // An unaddressable query is refused loudly — an empty result would read
    // as "no mods for that finding" when the key names no finding at all.
    let out = dm(&["mod", "list", "--for", "no-slash", "--json"]);
    assert_ne!(out.status.code(), Some(0), "an unaddressable --for must not exit clean");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("finding key"),
        "the error names the shape: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(keys(&["mod", "list", "--for", "sess-b/2", "--json"]), vec![key_b.clone()]);
    // `--mission` matches through the `for` finding's OWN recorded mission.
    assert_eq!(keys(&["mod", "list", "--mission", "crawl-2", "--json"]), vec![key_b.clone()]);
    assert_eq!(keys(&["mod", "list", "--mission", "crawl-1", "--json"]).len(), 3);
    assert!(
        !keys(&["mod", "list", "--mission", "crawl-1", "--json"]).contains(&key_z),
        "a mod whose finding is not in the store belongs to no mission"
    );
    assert!(keys(&["mod", "list", "--mission", "no-such-mission", "--json"]).is_empty());
    assert!(keys(&["mod", "list", "--for", "sess-nope/1", "--json"]).is_empty());

    // A filter that matches nothing must not read like an EMPTY STORE.
    let out = dm(&["mod", "list", "--for", "sess-nope/1"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no mods match — 6 in the store"), "got:\n{stdout}");
    assert!(!stdout.contains("mod create"), "the store is NOT empty: {stdout}");

    // The human list previews the RAW kit and names the mod's findings.
    let out = dm(&["mod", "list", "--for", "sess-a/1"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("rename the predicate"), "the raw kit is previewed: {stdout}");
    assert!(stdout.contains("sess-a/1"), "the `for` keys are shown: {stdout}");

    // ── show ──────────────────────────────────────────────────────────────
    let out = dm(&["mod", "show", &key_b]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(&key_b) && stdout.contains("kain"), "got:\n{stdout}");
    assert!(stdout.contains("sess-b/2"), "show names the findings: {stdout}");
    assert!(stdout.contains("patch.diff") && stdout.contains("bytes"), "attachments + sizes: {stdout}");
    assert!(
        stdout.contains("\"diff\": \"--- a\",\n  \"diff\": \"+++ b\""),
        "the kit is printed as its own bytes, duplicate keys and all: {stdout}"
    );
    assert!(
        stdout.contains("12345678901234567890123"),
        "an integer past f64 survives, because nothing parsed it: {stdout}"
    );
    // Nothing is appended to the kit. The fixture ends in a newline, so the
    // output ends in exactly one — a second would be a byte darkmux invented.
    assert!(stdout.ends_with("}\n"), "the kit's own bytes end the output: {stdout:?}");
    assert!(!stdout.contains(&key_a), "show is ONE record: {stdout}");

    let out = dm(&["mod", "show", "mod-nope"]);
    assert_eq!(out.status.code(), Some(1), "a missing mod exits 1");
    assert!(String::from_utf8_lossy(&out.stderr).contains("mod-nope"), "the message names the key");

    // A key that would escape the store is refused, proved against a record
    // that DOES exist at the escaped path — the store is `<home>/mods`, so
    // `../x-mod` addresses `<home>/x-mod`, its parent.
    fs::create_dir_all(home.path().join("x-mod")).unwrap();
    fs::write(
        home.path().join("x-mod/mod.json"),
        serde_json::json!({
            "key": "x-mod", "ts": "2026-09-03T00:00:00Z", "by": "outside-the-store",
            "for": [], "kit": "ESCAPED-THE-STORE", "attachments": [],
            "context": {"findings": []}, "schema_version": "1",
        })
        .to_string(),
    )
    .unwrap();
    let out = dm(&["mod", "show", "../x-mod"]);
    assert_eq!(out.status.code(), Some(1), "a traversal key must not resolve");
    assert!(String::from_utf8_lossy(&out.stdout).is_empty(), "nothing outside the store is printed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not a mod key"),
        "the key is INVALID, not merely missing — the two need different remedies: {stderr}"
    );
    assert!(!stderr.contains("ESCAPED-THE-STORE"), "got: {stderr}");

    // ── the derived view: `finding show` lists the mods that name it ──────
    let out = dm(&["finding", "show", "sess-a/1"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\nmods\n"), "the section is present: {stdout}");
    assert!(
        stdout.contains(&key_a) && stdout.contains(&key_a2) && stdout.contains(&key_pad),
        "every mod naming this finding is listed, canonicalized one included: {stdout}"
    );
    assert!(!stdout.contains(&key_b), "only the mods naming THIS finding: {stdout}");
    // Nothing is written back onto the finding — the view is derived.
    let on_disk = fs::read_to_string(home.path().join("findings/sess-a/1/finding.json")).unwrap();
    assert!(!on_disk.contains("mod-"), "the finding record is never rewritten: {on_disk}");

    // A finding with no mods prints no section at all.
    let out = dm(&["finding", "show", "sess-b/2"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(&key_b), "sess-b/2's own mod is listed: {stdout}");
    let out = dm(&["finding", "list", "--json"]);
    assert!(out.status.success(), "the finding verbs still read the store unchanged");
}

/// (#2265) A mod the host recorded PARTIALLY — a dropped attachment, an
/// unaddressable `for` key — carries `warnings`. `mod show` must print them:
/// the field exists so the record is honest about being partial, and a
/// rendering that hides it makes the record look whole again.
#[test]
fn mod_show_prints_the_warnings_of_a_partial_mod() {
    let home = TempDir::new().unwrap();
    let dir = home.path().join("mods").join("mod-1788430000-abc123");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mod.json"),
        r#"{"schema_version":"1","key":"mod-1788430000-abc123","ts":"2026-09-03T10:00:00Z","by":"coder (qwen)",
            "for":["sess-a/1"],"kit":"apply mod.diff\n","attachments":[],
            "warnings":["dropped attachment \"mod.diff\": no path/bytes pair"]}"#,
    )
    .unwrap();
    let out = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mod", "show", "mod-1788430000-abc123"])
        .output()
        .expect("darkmux runs");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("warnings"), "a partial mod says so: {stdout}");
    assert!(stdout.contains("dropped attachment \"mod.diff\""), "and names the part: {stdout}");
    // The kit still ends the output byte-exact — the warnings print ABOVE it.
    assert!(stdout.ends_with("apply mod.diff\n"), "got: {stdout:?}");
}

/// (#2299) `enabled: false` is honored at mint: the disabled step never exists
/// in the run, the config snapshot keeps the flag, `graph-report.json` names
/// what was pruned and why, the `mission start` record carries the same
/// report, and `mission status` counts it. No CLI override exists: the config
/// is the only place the run's shape comes from.
#[test]
fn mission_launch_prunes_disabled_steps_at_mint_and_reports_them() {
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    // 4 steps declared, 1 disabled; a task that depends only on the disabled
    // task goes with it; the freeform phase stays.
    let config_json = r#"{
        "id": "enabled-test",
        "name": "Enabled Test",
        "schema_version": "3.1",
        "phases": [{
            "id": "p1",
            "tasks": [
                {"id": "t-off", "enabled": false, "steps": [{"id": "s-off", "kind": "procedural.noop"}]},
                {"id": "t-on", "steps": [
                    {"id": "s-on-1", "kind": "procedural.noop"},
                    {"id": "s-on-2", "kind": "procedural.noop", "enabled": false}
                ]},
                {"id": "t-orphan", "depends_on": ["t-off"], "steps": [{"id": "s-orphan", "kind": "procedural.noop"}]}
            ]
        }]
    }"#;
    fs::write(config_dir.join("enabled-test.json"), config_json).unwrap();

    let out = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "launch", "enabled-test", "--timeout", "60"])
        .output()
        .expect("darkmux mission launch runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("graph: 1 of 4 steps minted (3 left out by config)"), "got:\n{stdout}");

    // One mission on disk; its report and snapshot say what happened.
    let missions_dir = home.path().join("missions");
    let mission_dir = fs::read_dir(&missions_dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.is_dir())
        .expect("one mission dir");
    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(mission_dir.join("graph-report.json")).unwrap()).unwrap();
    assert_eq!(report["steps_in_config"], 4);
    assert_eq!(report["steps_minted"], 1);
    assert_eq!(report["tasks_minted"], 1);
    let pruned: Vec<(String, String)> = report["pruned"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["id"].as_str().unwrap().to_string(), p["reason"].as_str().unwrap().to_string()))
        .collect();
    assert!(pruned.contains(&("t-off".into(), "disabled".into())), "{pruned:?}");
    assert!(pruned.contains(&("s-on-2".into(), "disabled".into())), "{pruned:?}");
    assert!(pruned.contains(&("t-orphan".into(), "all_dependencies_pruned".into())), "{pruned:?}");
    assert!(pruned.contains(&("s-orphan".into(), "parent_pruned".into())), "{pruned:?}");
    let snapshot: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(mission_dir.join("config-snapshot.json")).unwrap()).unwrap();
    assert_eq!(snapshot["phases"][0]["tasks"][0]["enabled"], false, "the snapshot keeps the DECLARED config");
    assert_eq!(snapshot["phases"][0]["tasks"].as_array().unwrap().len(), 3);
    // The pruned items left no task record behind — nothing gray on disk.
    // Task records live under `tasks/<phase-id>/`; walk one level down.
    let task_files: Vec<String> = fs::read_dir(mission_dir.join("tasks"))
        .map(|d| {
            d.filter_map(|e| e.ok())
                .flat_map(|phase_dir| {
                    fs::read_dir(phase_dir.path())
                        .map(|dd| dd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect::<Vec<_>>())
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default();
    assert!(task_files.iter().any(|f| f.contains("t-on")), "{task_files:?}");
    assert!(!task_files.iter().any(|f| f.contains("t-off") || f.contains("t-orphan")), "{task_files:?}");

    // The `mission start` record carries the same report.
    let mut day = String::new();
    for e in fs::read_dir(flows.path()).unwrap().filter_map(|e| e.ok()) {
        day.push_str(&fs::read_to_string(e.path()).unwrap());
    }
    let start = day
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|r| r["action"] == "mission start")
        .expect("a mission start record");
    assert_eq!(start["payload"]["graph"]["steps_minted"], 1, "{start}");
    assert_eq!(start["payload"]["graph"]["steps_in_config"], 4);

    // `mission status` counts it, human and JSON.
    let status = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "status", "--json"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&status.stdout)).unwrap();
    assert_eq!(v["missions"][0]["graph"]["steps_in_config"], 4, "{v}");
    assert_eq!(v["missions"][0]["graph"]["steps_minted"], 1);
    let human = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "status", "--all"])
        .output()
        .unwrap();
    let human_out = String::from_utf8_lossy(&human.stdout);
    assert!(human_out.contains("1 of 4 steps minted (3 left out by config)"), "got:\n{human_out}");
}

// ── (#2300) growth: a step's OUTPUT grows tasks into the graph ───────────
//
// The invariant under test is the SEAM, not any one mission: phase 1 writes
// a JSON artifact and returns its path as the step's `output`; phase 2
// declares a `grow` template naming phase 1's task; the launcher reads that
// artifact at the phase boundary and mints one copy of the template per
// item. Everything here is `procedural.shell`/`procedural.noop`, so no
// model, no container and no network is involved.

/// Writes a two-phase config where `plan-task` emits `units` and
/// `unit-task` grows over them. Returns (home, flows) tempdirs.
fn grow_fixture(units_json: &str, config_extra: &str) -> (TempDir, TempDir, std::path::PathBuf) {
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let plan_path = home.path().join("plan.json");
    fs::write(&plan_path, units_json).unwrap();

    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    // `procedural.shell`'s output is the command's stdout, so echoing the
    // path IS "the step's output is a path to a JSON file" — the contract
    // every producing step honors (`crawl.plan` writes `plan/<rule>.json`
    // and returns that path).
    let config_json = format!(
        r#"{{
        "id": "grow-test",
        "name": "Grow Test",
        "schema_version": "3.2",
        "phases": [
          {{
            "id": "plan",
            "tasks": [{{
              "id": "plan-task",
              "steps": [{{
                "id": "plan-step",
                "kind": "procedural.shell",
                "config": {{ "command": "echo {plan}" }}
              }}]
            }}]
          }},
          {{
            "id": "units",
            "tasks": [{{
              "id": "unit-task",
              "depends_on": ["plan-task"],
              "grow": {{
                "from": "plan-task",
                "items": "units",
                "id": "{{{{item.id}}}}",
                "config": {{ "unit": "{{{{item.id}}}}", "rule": "{{{{item.rule}}}}" }}
              }},
              "steps": [{{ "id": "unit-step", "kind": "procedural.noop", "config": {{}} }}]
            }}]
          }}
        ]{extra}
    }}"#,
        plan = plan_path.display(),
        extra = config_extra,
    );
    fs::write(config_dir.join("grow-test.json"), config_json).unwrap();
    (home, flows, plan_path)
}

fn launch_grow(home: &TempDir, flows: &TempDir) -> std::process::Output {
    Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "launch", "grow-test"])
        .output()
        .unwrap()
}

fn one_mission_dir(home: &TempDir) -> std::path::PathBuf {
    let missions = home.path().join("missions");
    let mut entries: Vec<_> = fs::read_dir(&missions)
        .unwrap_or_else(|e| panic!("reading {}: {e}", missions.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();
    assert_eq!(entries.len(), 1, "expected exactly one minted mission: {entries:?}");
    entries.pop().unwrap()
}

fn phase_record(mission_dir: &std::path::Path, phase_id: &str) -> serde_json::Value {
    let path = mission_dir.join("phases").join(format!("{phase_id}.json"));
    serde_json::from_str(
        &fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display())),
    )
    .unwrap()
}

fn flow_actions(flows: &TempDir) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for entry in fs::read_dir(flows.path()).unwrap().filter_map(|e| e.ok()) {
        let Ok(text) = fs::read_to_string(entry.path()) else { continue };
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                out.push(v);
            }
        }
    }
    out
}

#[test]
fn mission_launch_grows_one_task_per_plan_unit_with_provenance() {
    let (home, flows, plan_path) = grow_fixture(
        r#"{"units":[{"id":"u-1","rule":"r"},{"id":"u-2","rule":"r"}]}"#,
        "",
    );
    let out = launch_grow(&home, &flows);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "launch failed\nstdout:\n{stdout}\nstderr:\n{stderr}");

    let dir = one_mission_dir(&home);
    // Phase ids are composed from the real mission id at mint, so the
    // grown phase's directory name is `<mission id>-units`, not `units`.
    let mission_id = dir.file_name().unwrap().to_string_lossy().to_string();
    let grown_phase = format!("{mission_id}-units");
    // Two grown tasks on disk, in the GROWN phase, one per unit.
    for unit in ["u-1", "u-2"] {
        let task = dir.join("tasks").join(&grown_phase).join(format!("unit-task-{unit}.json"));
        assert!(task.is_file(), "missing grown task record {}\n{stdout}", task.display());
        let step_path = dir.join("steps").join(&grown_phase).join(format!("unit-step-{unit}.json"));
        let step: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&step_path).unwrap()).unwrap();
        assert_eq!(step["config"]["unit"], serde_json::json!(unit), "step: {step}");
        assert_eq!(step["config"]["rule"], serde_json::json!("r"), "step: {step}");
        assert_eq!(step["config"]["grown_from"]["task"], serde_json::json!("plan-task"));
        assert_eq!(step["config"]["grown_from"]["item"], serde_json::json!(unit));
        assert_eq!(step["status"], serde_json::json!("complete"), "the grown step must RUN: {step}");
    }
    // The template itself is never minted.
    assert!(
        !dir.join("tasks").join(&grown_phase).join("unit-task.json").is_file(),
        "the `grow` template must not be minted as a task of its own"
    );

    // (#2300) The phase record names the tasks it owns — the declared ids
    // at mint, the grown ids appended at the boundary. `crawl_launch.rs`
    // has always written this field; the generic launcher does now too.
    let plan_phase = phase_record(&dir, &format!("{mission_id}-plan"));
    assert_eq!(
        plan_phase["task_ids"],
        // The placeholder-prefix rule rewrites `plan-task` (prefixed by the
        // document phase id `plan`) into `<real phase id>-task`.
        serde_json::json!([format!("{mission_id}-plan-task")]),
        "the producing phase lists its declared task: {plan_phase}"
    );
    let units_phase = phase_record(&dir, &grown_phase);
    let listed = units_phase["task_ids"].as_array().expect("task_ids is a list");
    assert_eq!(
        listed.len(),
        2,
        "the grown phase lists exactly its grown tasks (the template is not one): {units_phase}"
    );
    for unit in ["u-1", "u-2"] {
        assert!(
            listed.contains(&serde_json::json!(format!("unit-task-{unit}"))),
            "missing grown id in task_ids: {units_phase}"
        );
    }
    assert!(
        !listed.contains(&serde_json::json!("unit-task")),
        "the `grow` template must never be listed as a task: {units_phase}"
    );

    // graph-report.json carries the growth event.
    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("graph-report.json")).unwrap()).unwrap();
    let grown = report["grown"].as_array().expect("graph-report has a `grown` section");
    assert_eq!(grown.len(), 1, "report: {report}");
    assert_eq!(grown[0]["from"], serde_json::json!("plan-task"));
    assert_eq!(grown[0]["task_template"], serde_json::json!("unit-task"));
    assert_eq!(grown[0]["items"], serde_json::json!(2));
    // (#2301) `source_path` -> `source`, holding the RESOLVED name: a
    // wrapped producer's output is a `{"ref": …}` pointer, so the raw
    // output string stopped being a path.
    assert_eq!(grown[0]["source"], serde_json::json!(plan_path.display().to_string()));
    assert!(grown[0]["source_path"].is_null(), "the old key is renamed, not aliased");
    assert_eq!(grown[0]["minted"].as_array().unwrap().len(), 2);

    // One `mission.grow` flow record naming the same facts.
    let records = flow_actions(&flows);
    let grow_records: Vec<_> = records.iter().filter(|r| r["action"] == "mission.grow").collect();
    assert_eq!(grow_records.len(), 1, "records: {records:?}");
    let payload = &grow_records[0]["payload"];
    assert_eq!(payload["from"], serde_json::json!("plan-task"));
    assert_eq!(payload["items"], serde_json::json!(2));
    assert_eq!(payload["minted"].as_array().unwrap().len(), 2);
    assert!(
        payload.get("reason").is_none(),
        "`reason` is omitted, never null, when the growth minted something: {payload}"
    );

    // `mission status` names the growth.
    let status = Command::cargo_bin("darkmux")
        .unwrap()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .args(["mission", "status"])
        .output()
        .unwrap();
    let status_out = String::from_utf8_lossy(&status.stdout);
    assert!(
        status_out.contains("grew 2 task(s) from `plan-task`"),
        "mission status must name the growth, got:\n{status_out}"
    );
}

#[test]
fn mission_launch_grows_nothing_from_an_empty_plan_and_still_completes() {
    let (home, flows, _) = grow_fixture(r#"{"units":[]}"#, "");
    let out = launch_grow(&home, &flows);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "an empty plan is a real outcome, not a failure:\n{stdout}");
    assert!(
        stdout.contains("grew nothing") && stdout.contains("grew_nothing"),
        "the zero-item outcome must be named, got:\n{stdout}"
    );
    let dir = one_mission_dir(&home);
    let mission_id = dir.file_name().unwrap().to_string_lossy().to_string();
    let grown_dir = dir.join("tasks").join(format!("{mission_id}-units"));
    assert!(
        !grown_dir.exists() || fs::read_dir(&grown_dir).unwrap().next().is_none(),
        "zero items must mint zero tasks"
    );
    // (#2300) The phase must read `complete`, not `abandoned`. A phase with
    // no steps is invisible to the step-driven lazy start/close, so without
    // an explicit completion it sits Planned all run and the #1504 finalize
    // backstop sweeps it to Abandoned — recording a failure where the plan
    // simply planned nothing.
    let units_phase = phase_record(&dir, &format!("{mission_id}-units"));
    assert_eq!(
        units_phase["status"],
        serde_json::json!("complete"),
        "a phase that grew nothing must COMPLETE: {units_phase}"
    );
    assert!(
        !stdout.contains("reconciled to Abandoned"),
        "no backstop reconcile warning may print for a phase that legitimately grew nothing:\n{stdout}"
    );
    let records = flow_actions(&flows);
    let grow = records.iter().find(|r| r["action"] == "mission.grow").expect("a grow record");
    assert_eq!(grow["payload"]["reason"], serde_json::json!("grew_nothing"), "{grow}");
    assert_eq!(grow["payload"]["items"], serde_json::json!(0));
}

#[test]
fn mission_launch_fails_loudly_when_the_producer_output_is_not_a_json_path() {
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    // The producing step completes fine — it just echoes a path that does
    // not exist. A silent zero-task growth here is exactly the failure the
    // retired `expand` primitive shipped; this must be an error naming the
    // task AND the path.
    let config_json = r#"{
        "id": "grow-test",
        "name": "Grow Test",
        "schema_version": "3.2",
        "phases": [
          { "id": "plan", "tasks": [{ "id": "plan-task", "steps": [
              { "id": "plan-step", "kind": "procedural.shell",
                "config": { "command": "echo /nope/not-a-plan.json" } }]}]},
          { "id": "units", "tasks": [{ "id": "unit-task", "depends_on": ["plan-task"],
              "grow": { "from": "plan-task", "items": "units", "id": "{{item.id}}", "config": {} },
              "steps": [{ "id": "unit-step", "kind": "procedural.noop", "config": {} }]}]}
        ]
    }"#;
    fs::write(config_dir.join("grow-test.json"), config_json).unwrap();

    let out = launch_grow(&home, &flows);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.status.success(), "a missing artifact must fail the run, got:\n{combined}");
    assert!(
        combined.contains("unit-task") && combined.contains("/nope/not-a-plan.json"),
        "the error must name the task and the path, got:\n{combined}"
    );
}
