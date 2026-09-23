//! The write-the-tests work gate (#2833).
//!
//! `run_verify_command`'s raw signal ("did the verify command exit 0?") is
//! vacuous for a fixture whose suite is green on an untouched sandbox: a
//! dispatch that did nothing — or even one whose runtime errored out after
//! zero turns — passes it. Measured directly on the pepper-grinder fixture
//! (#2833's issue body): a no-op run's `final_hash == baseline_hash` and its
//! `verify: passed` were BOTH true at once.
//!
//! This module is the stricter gate, applied ONLY when a run's fixture
//! declares `baseline.test_count` (`FixtureManifest::baseline`, the single
//! source of truth for "how many tests did the untouched fixture have" — see
//! the removal note on `ExpectedSpec` in `crate::workloads::types`). A
//! workload/fixture that declares no baseline keeps today's behavior
//! (`run_verify_command`'s raw exit-0 result stands unmodified) — this is
//! deliberately NOT a global stricter-verify policy; a read-only review
//! workload's "the suite still passes" IS a meaningful signal, just not a
//! write-the-tests one.
//!
//! The operator's pass rule (decided, #2833): a pass requires ALL of —
//!   1. **Work done** — `final_hash != baseline_hash` AND the suite now
//!      reports MORE tests than the fixture's declared baseline.
//!   2. **No errors** — the dispatch itself succeeded (not a runtime/
//!      transport error) AND the verify command exited 0 (every test
//!      passes, so nothing previously green broke).
//!   3. **Coverage, only when the workload directs it** — if (and only if)
//!      `VerifySpec::coverage_min_pct` is set, the run's line coverage must
//!      meet it.
//!
//! [`evaluate`] is the pure decision function (no I/O, exhaustively unit
//! tested); [`apply`] is the thin I/O wrapper `lab::run::lab_run` calls
//! after the provider has run and the manifest has been enriched with
//! fixture provenance (`final_hash` + `fixture.baseline_hash` both present).

use crate::lab::fixture::FixtureManifest;
use anyhow::{anyhow, Context, Result};
use serde_json::json;
use std::fs;
use std::path::Path;

/// Inputs to the pure gate decision. Everything here is already resolved —
/// no path or file-format knowledge lives in [`evaluate`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkGateInput {
    /// Did the dispatch itself complete without a runtime/transport error?
    /// (`RunOutcome::ok` / the provider's `RunResult::ok` — the same flag
    /// the #2833 issue's headline example recorded as `false`.)
    pub dispatch_ok: bool,
    /// `final_hash != baseline_hash`. `None` when either hash is
    /// unavailable — treated as "cannot confirm", which fails the gate
    /// rather than assuming either direction.
    pub sandbox_changed: Option<bool>,
    /// The fixture's declared `baseline.test_count` — why this gate is
    /// active at all.
    pub baseline_test_count: u64,
    /// Total tests the verify output reports NOW. `None` when the output
    /// couldn't be parsed by any reporter format this module understands.
    pub tests_total: Option<u64>,
    /// Failing tests the verify output reports NOW, when readable.
    pub tests_failed: Option<u64>,
    /// The raw "did the verify command exit 0" signal
    /// (`VerifyOutcome::passed`, unchanged from `run_verify_command`).
    pub verify_command_passed: bool,
    /// `VerifySpec::coverage_min_pct`, when the workload declares one.
    pub coverage_min_pct: Option<f64>,
    /// Line coverage percentage the verify output reports, when readable.
    /// Only consulted when `coverage_min_pct` is `Some`.
    pub coverage_pct: Option<f64>,
}

/// The gate's verdict, plus the evidence that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkGateResult {
    pub passed: bool,
    /// Names which rule failed (or, on a pass, what was confirmed). Never
    /// empty — "no silent wrong key": a gated run always says why.
    pub details: String,
    /// `tests_total - baseline_test_count`, when `tests_total` was
    /// readable. Signed so a shrinking suite is visible rather than
    /// saturating at 0.
    pub tests_added: Option<i64>,
}

fn fail(details: impl Into<String>) -> WorkGateResult {
    WorkGateResult {
        passed: false,
        details: details.into(),
        tests_added: None,
    }
}

/// The pure decision. Order matters — earlier branches dominate, same
/// discipline as `crate::lab::loop_report::classify`:
///   1. `!dispatch_ok` -> fail (nothing else is trustworthy).
///   2. `sandbox_changed != Some(true)` -> fail ("no work" — covers both the
///      no-op case and "hash unavailable, can't confirm").
///   3. `tests_total` unreadable -> fail ("count unreadable").
///   4. `tests_added <= 0` -> fail ("no tests added").
///   5. `!verify_command_passed` -> fail ("suite red").
///   6. a declared coverage threshold not met (or unmeasurable) -> fail.
///   7. otherwise -> pass.
pub fn evaluate(input: &WorkGateInput) -> WorkGateResult {
    if !input.dispatch_ok {
        return fail(
            "dispatch did not complete cleanly (runtime/transport error) — \
             cannot confirm any work was done",
        );
    }
    match input.sandbox_changed {
        Some(true) => {}
        Some(false) => {
            return fail(
                "no work: the sandbox is unchanged from the fixture's baseline \
                 (a no-op run cannot pass)",
            )
        }
        None => {
            return fail(
                "no baseline/final sandbox hash recorded — cannot confirm whether \
                 the sandbox changed",
            )
        }
    }

    let Some(total) = input.tests_total else {
        return fail(
            "verify output's test count is unreadable (unrecognized test-runner \
             output format) — cannot confirm tests were added",
        );
    };
    let added = total as i64 - input.baseline_test_count as i64;
    if added <= 0 {
        return WorkGateResult {
            passed: false,
            details: format!(
                "no tests added: fixture baseline declares {} tests, verify now \
                 reports {total}",
                input.baseline_test_count
            ),
            tests_added: Some(added),
        };
    }

    if !input.verify_command_passed {
        let detail = match input.tests_failed {
            Some(f) => format!("{f} of {total} tests failing"),
            None => "the verify command exited non-zero".to_string(),
        };
        return WorkGateResult {
            passed: false,
            details: format!("verify command failed: {detail}"),
            tests_added: Some(added),
        };
    }

    if let Some(min) = input.coverage_min_pct {
        match input.coverage_pct {
            None => {
                return WorkGateResult {
                    passed: false,
                    details: format!(
                        "coverage threshold declared ({min:.1}%) but verify output \
                         has no coverage table"
                    ),
                    tests_added: Some(added),
                }
            }
            Some(pct) if pct < min => {
                return WorkGateResult {
                    passed: false,
                    details: format!(
                        "coverage {pct:.1}% is below the declared threshold of {min:.1}%"
                    ),
                    tests_added: Some(added),
                }
            }
            Some(_) => {}
        }
    }

    WorkGateResult {
        passed: true,
        details: format!(
            "{added} test(s) added ({total} total, baseline {}), suite green{}",
            input.baseline_test_count,
            match (input.coverage_min_pct, input.coverage_pct) {
                (Some(min), Some(pct)) => format!(", coverage {pct:.1}% >= {min:.1}%"),
                _ => String::new(),
            }
        ),
        tests_added: Some(added),
    }
}

/// Strip a node:test reporter's line-prefix (`ℹ ` — the default "spec"
/// reporter — or `# ` — the "tap" reporter), when present. `None` when the
/// line carries neither, i.e. it isn't a reporter summary line at all.
fn strip_reporter_prefix(line: &str) -> Option<&str> {
    let line = line.trim_start();
    line.strip_prefix('ℹ')
        .or_else(|| line.strip_prefix('#'))
        .map(str::trim_start)
}

/// Parse `node --test`'s own summary lines — `tests N` / `pass N` / `fail N`
/// (spec reporter: `ℹ tests 14`; tap reporter: `# tests 14`) — out of a
/// verify command's captured output. Returns `(total, failed)`, each `None`
/// if that specific line was never seen. Takes the LAST match of each key,
/// so a verify command that reruns the suite internally reports the final
/// state rather than an earlier attempt.
///
/// Deliberately narrow: this is the ONLY reporter shape this module
/// understands. An unrecognized format (a different runner, a reporter this
/// wasn't taught) yields `(None, None)`, which the gate turns into an
/// explicit "count unreadable" failure — never a silent pass.
pub fn parse_node_test_counts(output: &str) -> (Option<u64>, Option<u64>) {
    let mut total = None;
    let mut failed = None;
    for line in output.lines() {
        let Some(rest) = strip_reporter_prefix(line) else {
            continue;
        };
        let mut parts = rest.split_whitespace();
        let (Some(key), Some(val)) = (parts.next(), parts.next()) else {
            continue;
        };
        // Reject a trailing 3rd token — a coverage-table row's first cell
        // ("all files               |  91.82 ...") must never look like a
        // 2-token summary line.
        if parts.next().is_some() {
            continue;
        }
        let Ok(n) = val.parse::<u64>() else { continue };
        match key {
            "tests" => total = Some(n),
            "fail" => failed = Some(n),
            _ => {}
        }
    }
    (total, failed)
}

/// Parse the LINE-coverage percentage from node's `--experimental-test-
/// coverage` summary table (`ℹ all files | <line%> | <branch%> | <funcs%> |
/// <uncovered>`). Returns `None` when no `all files` row appears — including
/// when coverage wasn't measured at all, which the gate (when a threshold is
/// declared) turns into an explicit failure rather than skipping the check.
pub fn parse_node_line_coverage_pct(output: &str) -> Option<f64> {
    for line in output.lines() {
        let rest = strip_reporter_prefix(line).unwrap_or(line);
        let rest = rest.trim();
        if !rest.starts_with("all files") {
            continue;
        }
        let mut cols = rest.split('|');
        cols.next()?; // "all files" label column
        let line_pct = cols.next()?.trim();
        return line_pct.parse::<f64>().ok();
    }
    None
}

/// Read the fixture's declared `baseline.test_count`, when the fixture at
/// `source_sandbox_dir` declares one. `None` (not an error) when the
/// directory has no `.fixture.json`, or the manifest declares no baseline —
/// both mean "this workload doesn't use the gate", not "something is wrong".
fn fixture_baseline_test_count(source_sandbox_dir: &Path) -> Option<u64> {
    let manifest = FixtureManifest::load_from_dir(source_sandbox_dir).ok()?;
    manifest.baseline.get("test_count")?.as_u64()
}

/// The I/O wrapper `lab::run::lab_run` calls once per run, right after
/// `enrich_manifest_with_fixture_info` has written `final_hash` (top-level,
/// from the provider) and `fixture.baseline_hash` (from the enricher) into
/// `<run_dir>/manifest.json`.
///
/// Returns `Ok(None)` (manifest untouched) whenever the fixture declares no
/// `baseline.test_count` — "Rules 1-2 apply when the run's fixture declares
/// a baseline test count; a workload/fixture that declares none keeps
/// today's behavior" (#2833) — or when the provider recorded no verify
/// outcome at all. Otherwise returns `Ok(Some(result))` so the caller can
/// sync the SAME gated verdict into the in-memory `RunResult` it already
/// holds (the CLI's printed notes and `RunOutcome::verify_passed` must not
/// disagree with what just got written to `manifest.json`).
///
/// Best-effort in the SAME sense every other manifest enrichment in this
/// module is: a read/parse error here is returned to the caller, which logs
/// it but never fails the run — the gate is observability sharpening an
/// existing signal, not new correctness the dispatch depends on.
pub fn apply(
    run_dir: &Path,
    source_sandbox_dir: &Path,
    dispatch_ok: bool,
    coverage_min_pct: Option<f32>,
) -> Result<Option<WorkGateResult>> {
    let Some(baseline_test_count) = fixture_baseline_test_count(source_sandbox_dir) else {
        return Ok(None);
    };

    let manifest_path = run_dir.join("manifest.json");
    let raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let mut manifest: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {} as JSON", manifest_path.display()))?;

    // A workload whose fixture HAS a baseline but which itself declares no
    // verify command at all is an odd combination, but not this module's to
    // adjudicate — leave `verify: null` exactly as the provider wrote it.
    let verify_is_null = manifest.get("verify").map(|v| v.is_null()).unwrap_or(true);
    if verify_is_null {
        return Ok(None);
    }

    let final_hash = manifest
        .get("final_hash")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let baseline_hash = manifest
        .get("fixture")
        .and_then(|f| f.get("baseline_hash"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let sandbox_changed = match (&final_hash, &baseline_hash) {
        (Some(f), Some(b)) => Some(f != b),
        _ => None,
    };

    let verify_command_passed = manifest
        .get("verify")
        .and_then(|v| v.get("passed"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let verify_output = fs::read_to_string(run_dir.join("verify-output.txt")).unwrap_or_default();
    let (tests_total, tests_failed) = parse_node_test_counts(&verify_output);
    let coverage_pct = if coverage_min_pct.is_some() {
        parse_node_line_coverage_pct(&verify_output)
    } else {
        None
    };

    let input = WorkGateInput {
        dispatch_ok,
        sandbox_changed,
        baseline_test_count,
        tests_total,
        tests_failed,
        verify_command_passed,
        coverage_min_pct: coverage_min_pct.map(|v| v as f64),
        coverage_pct,
    };
    let gate_result = evaluate(&input);

    let obj = manifest
        .as_object_mut()
        .ok_or_else(|| anyhow!("manifest is not a JSON object"))?;
    let verify_obj = obj
        .get_mut("verify")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| anyhow!("manifest `verify` is not a JSON object"))?;
    verify_obj.insert("passed".to_string(), json!(gate_result.passed));
    verify_obj.insert("details".to_string(), json!(gate_result.details));
    verify_obj.insert(
        "work_gate".to_string(),
        json!({
            "baseline_test_count": baseline_test_count,
            "tests_total": tests_total,
            "tests_added": gate_result.tests_added,
            "tests_failed": tests_failed,
            "sandbox_changed": sandbox_changed,
            "coverage_min_pct": coverage_min_pct,
            "coverage_pct": coverage_pct,
            // The raw "did the verify command exit 0" signal, preserved
            // distinctly from the gated `passed` above it — #2833's finding
            // was precisely that this signal alone is vacuous for a
            // write-the-tests workload, not that it's wrong to record.
            "command_passed": verify_command_passed,
        }),
    );

    // RAISE-never-lower, same discipline as `enrich_manifest_with_fixture_info`
    // two callers up the same file.
    let bumped = obj
        .get("schema_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .max(6);
    obj.insert("schema_version".to_string(), json!(bumped));

    fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("writing {}", manifest_path.display()))?;
    Ok(Some(gate_result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn base_input() -> WorkGateInput {
        WorkGateInput {
            dispatch_ok: true,
            sandbox_changed: Some(true),
            baseline_test_count: 14,
            tests_total: Some(22),
            tests_failed: Some(0),
            verify_command_passed: true,
            coverage_min_pct: None,
            coverage_pct: None,
        }
    }

    // ─── evaluate: the operator's pass rule ─────────────────────────

    #[test]
    fn no_op_run_fails_no_work() {
        let input = WorkGateInput {
            sandbox_changed: Some(false),
            tests_total: Some(14),
            tests_failed: Some(0),
            ..base_input()
        };
        let r = evaluate(&input);
        assert!(!r.passed);
        assert!(r.details.contains("no work"), "got: {}", r.details);
    }

    #[test]
    fn changed_sandbox_but_same_test_count_fails_no_tests_added() {
        let input = WorkGateInput {
            tests_total: Some(14),
            ..base_input()
        };
        let r = evaluate(&input);
        assert!(!r.passed);
        assert!(r.details.contains("no tests added"), "got: {}", r.details);
        assert_eq!(r.tests_added, Some(0));
    }

    #[test]
    fn tests_added_and_green_passes() {
        let r = evaluate(&base_input());
        assert!(r.passed, "got: {}", r.details);
        assert_eq!(r.tests_added, Some(8));
    }

    #[test]
    fn tests_added_but_suite_red_fails() {
        let input = WorkGateInput {
            tests_failed: Some(3),
            verify_command_passed: false,
            ..base_input()
        };
        let r = evaluate(&input);
        assert!(!r.passed);
        assert!(
            r.details.contains("3 of 22 tests failing"),
            "got: {}",
            r.details
        );
    }

    #[test]
    fn dispatch_runtime_error_with_green_untouched_suite_fails() {
        // The #2833 issue's own headline example: `ok: false`, hashes
        // identical, verify's raw exit code still 0.
        let input = WorkGateInput {
            dispatch_ok: false,
            sandbox_changed: Some(false),
            tests_total: Some(14),
            tests_failed: Some(0),
            ..base_input()
        };
        let r = evaluate(&input);
        assert!(!r.passed);
        assert!(
            r.details.contains("runtime/transport error"),
            "got: {}",
            r.details
        );
    }

    #[test]
    fn baseline_declared_but_unreadable_count_fails_with_reason() {
        let input = WorkGateInput {
            tests_total: None,
            ..base_input()
        };
        let r = evaluate(&input);
        assert!(!r.passed);
        assert!(r.details.contains("unreadable"), "got: {}", r.details);
        assert_eq!(r.tests_added, None);
    }

    #[test]
    fn missing_hashes_fail_cannot_confirm_rather_than_silently_passing() {
        let input = WorkGateInput {
            sandbox_changed: None,
            ..base_input()
        };
        let r = evaluate(&input);
        assert!(!r.passed);
        assert!(r.details.contains("cannot confirm"), "got: {}", r.details);
    }

    #[test]
    fn coverage_threshold_met_passes() {
        let input = WorkGateInput {
            coverage_min_pct: Some(80.0),
            coverage_pct: Some(91.82),
            ..base_input()
        };
        let r = evaluate(&input);
        assert!(r.passed, "got: {}", r.details);
        assert!(r.details.contains("coverage 91.8"), "got: {}", r.details);
    }

    #[test]
    fn coverage_threshold_unmet_fails() {
        let input = WorkGateInput {
            coverage_min_pct: Some(95.0),
            coverage_pct: Some(91.82),
            ..base_input()
        };
        let r = evaluate(&input);
        assert!(!r.passed);
        assert!(
            r.details.contains("below the declared threshold"),
            "got: {}",
            r.details
        );
    }

    #[test]
    fn coverage_threshold_declared_but_absent_from_output_fails() {
        let input = WorkGateInput {
            coverage_min_pct: Some(80.0),
            coverage_pct: None,
            ..base_input()
        };
        let r = evaluate(&input);
        assert!(!r.passed);
        assert!(
            r.details.contains("no coverage table"),
            "got: {}",
            r.details
        );
    }

    #[test]
    fn no_coverage_threshold_declared_ignores_coverage_entirely() {
        // pepper-grinder's actual shipped workload: no threshold set, even
        // though coverage IS measurable in principle. Must not be graded.
        let input = WorkGateInput {
            coverage_min_pct: None,
            coverage_pct: Some(1.0),
            ..base_input()
        };
        let r = evaluate(&input);
        assert!(r.passed, "got: {}", r.details);
    }

    // ─── parse_node_test_counts: real node --test output ────────────

    const SPEC_REPORTER_OUTPUT: &str = "\
▶ suite portability
  ✔ ok (0.5ms)
✔ suite portability (0.9ms)
ℹ tests 22
ℹ suites 5
ℹ pass 22
ℹ fail 0
ℹ cancelled 0
ℹ skipped 0
ℹ todo 0
ℹ duration_ms 34.6
";

    const TAP_REPORTER_OUTPUT: &str = "\
# tests 22
# pass 19
# fail 3
# cancelled 0
# skipped 0
# todo 0
";

    #[test]
    fn parses_spec_reporter_counts() {
        // Measured directly: `npm test` in a scratch copy of pepper-grinder
        // under node v24.16.0 — the default ("spec") reporter uses the `ℹ`
        // prefix, NOT `#` (which only the `tap` reporter emits).
        assert_eq!(
            parse_node_test_counts(SPEC_REPORTER_OUTPUT),
            (Some(22), Some(0))
        );
    }

    #[test]
    fn parses_tap_reporter_counts() {
        assert_eq!(
            parse_node_test_counts(TAP_REPORTER_OUTPUT),
            (Some(22), Some(3))
        );
    }

    #[test]
    fn takes_the_last_occurrence_of_each_key() {
        let output = "ℹ tests 14\nℹ fail 2\nsome retry banner\nℹ tests 22\nℹ fail 0\n";
        assert_eq!(parse_node_test_counts(output), (Some(22), Some(0)));
    }

    #[test]
    fn unrecognized_output_format_is_unreadable() {
        assert_eq!(
            parse_node_test_counts("PASS  22 tests, 0 failures\n"),
            (None, None)
        );
        assert_eq!(parse_node_test_counts(""), (None, None));
    }

    #[test]
    fn coverage_table_row_is_never_mistaken_for_a_summary_line() {
        // A 4-token "line" (after the label) must not parse as "tests" +
        // some accidental number.
        let output = "ℹ all files               |  91.82 |    77.42 |   88.89 | \n";
        assert_eq!(parse_node_test_counts(output), (None, None));
    }

    // ─── parse_node_line_coverage_pct: real node --experimental-test-coverage output ──

    const COVERAGE_OUTPUT: &str = "\
ℹ tests 14
ℹ pass 14
ℹ fail 0
ℹ start of coverage report
ℹ ------------------------------------------------------------------------------------------------
ℹ file                    | line % | branch % | funcs % | uncovered lines
ℹ ------------------------------------------------------------------------------------------------
ℹ src                     |        |          |         |
ℹ  config.js              |  93.55 |    42.86 |  100.00 | 22-23
ℹ  refreshTokenService.js |  86.63 |    73.33 |   85.71 | 29-42 88-89 113 115-116 125-126 166-167
ℹ  store.js               |  99.13 |    92.00 |   88.89 | 20
ℹ ------------------------------------------------------------------------------------------------
ℹ all files               |  91.82 |    77.42 |   88.89 |
ℹ ------------------------------------------------------------------------------------------------
ℹ end of coverage report
";

    #[test]
    fn parses_line_coverage_from_real_node_coverage_table() {
        // Measured directly: `node --test --experimental-test-coverage
        // test/*.test.js` in a scratch copy of pepper-grinder, node v24.16.0.
        assert_eq!(parse_node_line_coverage_pct(COVERAGE_OUTPUT), Some(91.82));
    }

    #[test]
    fn no_coverage_table_in_output_is_none() {
        assert_eq!(parse_node_line_coverage_pct(SPEC_REPORTER_OUTPUT), None);
    }

    // ─── apply: end-to-end over synthetic run artifacts ─────────────

    fn write_fixture(dir: &Path, test_count: u64) {
        fs::write(
            dir.join(".fixture.json"),
            format!(r#"{{"name": "demo", "baseline": {{"test_count": {test_count}}}}}"#),
        )
        .unwrap();
    }

    fn write_manifest(dir: &Path, json: &str) {
        fs::write(dir.join("manifest.json"), json).unwrap();
    }

    #[test]
    fn apply_is_a_noop_when_fixture_declares_no_baseline() {
        let run = TempDir::new().unwrap();
        let fixture = TempDir::new().unwrap();
        // No .fixture.json at all.
        write_manifest(
            run.path(),
            r#"{"schema_version":5,"verify":{"passed":true,"details":"verify command exited 0"}}"#,
        );
        apply(run.path(), fixture.path(), true, None).unwrap();
        let raw = fs::read_to_string(run.path().join("manifest.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Untouched: still the raw provider-written shape, no work_gate.
        assert_eq!(v["verify"]["passed"], true);
        assert!(v["verify"].get("work_gate").is_none());
    }

    #[test]
    fn apply_gates_the_noop_run_to_fail() {
        // The exact #2833 shape: hashes identical, raw verify passed.
        let run = TempDir::new().unwrap();
        let fixture = TempDir::new().unwrap();
        write_fixture(fixture.path(), 14);
        write_manifest(
            run.path(),
            r#"{"schema_version":5,"final_hash":"blake3:same",
               "fixture":{"baseline_hash":"blake3:same"},
               "verify":{"passed":true,"details":"verify command exited 0"}}"#,
        );
        fs::write(
            run.path().join("verify-output.txt"),
            SPEC_REPORTER_OUTPUT.replace("22", "14"),
        )
        .unwrap();
        apply(run.path(), fixture.path(), true, None).unwrap();
        let raw = fs::read_to_string(run.path().join("manifest.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["verify"]["passed"], false);
        assert!(v["verify"]["details"].as_str().unwrap().contains("no work"));
        assert_eq!(v["verify"]["work_gate"]["sandbox_changed"], false);
        assert_eq!(v["schema_version"], 6);
    }

    #[test]
    fn apply_passes_a_real_write_the_tests_run() {
        let run = TempDir::new().unwrap();
        let fixture = TempDir::new().unwrap();
        write_fixture(fixture.path(), 14);
        write_manifest(
            run.path(),
            r#"{"schema_version":5,"final_hash":"blake3:after",
               "fixture":{"baseline_hash":"blake3:before"},
               "verify":{"passed":true,"details":"verify command exited 0"}}"#,
        );
        fs::write(run.path().join("verify-output.txt"), SPEC_REPORTER_OUTPUT).unwrap();
        apply(run.path(), fixture.path(), true, None).unwrap();
        let raw = fs::read_to_string(run.path().join("manifest.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["verify"]["passed"], true);
        assert_eq!(v["verify"]["work_gate"]["tests_added"], 8);
        assert_eq!(v["verify"]["work_gate"]["command_passed"], true);
    }

    #[test]
    fn apply_leaves_null_verify_untouched() {
        let run = TempDir::new().unwrap();
        let fixture = TempDir::new().unwrap();
        write_fixture(fixture.path(), 14);
        write_manifest(run.path(), r#"{"schema_version":5,"verify":null}"#);
        apply(run.path(), fixture.path(), true, None).unwrap();
        let raw = fs::read_to_string(run.path().join("manifest.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(v["verify"].is_null());
    }
}
