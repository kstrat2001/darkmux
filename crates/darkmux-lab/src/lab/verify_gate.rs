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
//! The operator's pass rule (decided, #2833; sharpened by the follow-up
//! frontier review that found three holes in the first cut): a pass
//! requires ALL of —
//!   1. **Work done** — `final_hash != baseline_hash` AND the suite now has
//!      MORE PASSING tests than the fixture's declared baseline. Measured
//!      from `pass`, never `tests` — `tests` counts skipped/todo entries
//!      too, so a model that neuters its own failing test with `.skip`/
//!      `.todo` instead of fixing it must not read as "added".
//!   2. **No errors** — the dispatch itself succeeded (not a runtime/
//!      transport error), the verify command's OWN test script wasn't
//!      edited by the agent, and the verify command exited 0.
//!   3. **Coverage, only when the workload directs it** — if (and only if)
//!      `VerifySpec::coverage_min_pct` is set, the run's line coverage must
//!      meet it.
//!
//! [`evaluate`] is the pure decision function (no I/O, exhaustively unit
//! tested); [`apply`] is the thin I/O wrapper `lab::run::lab_run` calls
//! after the provider has run and the manifest has been enriched with
//! fixture provenance (`final_hash` + `fixture.baseline_hash` both present).
//! `apply` is fail-CLOSED: a malformed baseline (a mistyped
//! `baseline.test_count`) forces an explicit FAIL via
//! `gate_with_forced_failure` rather than silently disabling the gate; a
//! valid baseline whose gate application hits a genuine internal error
//! bubbles `Err` rather than reporting an unexamined pass, and
//! `lab::run::lab_run`'s caller-side handling fails the in-memory verdict
//! closed for that case too.

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
    /// PASSING tests the verify output reports NOW — the metric "added" is
    /// measured from (#2833 review finding 1). `None` when the output
    /// couldn't be parsed by any reporter format this module understands.
    pub tests_passed: Option<u64>,
    /// Total tests (pass + fail + skip + todo + cancelled) the verify
    /// output reports NOW. Evidence only — NOT what "added" is measured
    /// from, since it counts tests a model neutered rather than fixed.
    pub tests_total: Option<u64>,
    /// Failing tests the verify output reports NOW, when readable.
    pub tests_failed: Option<u64>,
    /// Skipped tests, when readable. Evidence only.
    pub tests_skipped: Option<u64>,
    /// Todo (stubbed) tests, when readable. Evidence only.
    pub tests_todo: Option<u64>,
    /// The raw "did the verify command exit 0" signal
    /// (`VerifyOutcome::passed`, unchanged from `run_verify_command`).
    pub verify_command_passed: bool,
    /// `Some(reason)` when the sandbox's own verify-command definition
    /// (today: `package.json`'s `scripts.test`) was found to differ from
    /// the pristine fixture's — the agent edited what "pass" means instead
    /// of making the suite pass (#2833 review finding 5). `None` when no
    /// tampering was detected, or the check doesn't apply (no
    /// `package.json` on the fixture side to compare against).
    pub command_tampered: Option<String>,
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
    /// `tests_passed - baseline_test_count`, when `tests_passed` was
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
///   2. `command_tampered.is_some()` -> fail (the verify command itself was
///      edited; every downstream signal is now untrustworthy).
///   3. `sandbox_changed != Some(true)` -> fail ("no work" — covers both the
///      no-op case and "hash unavailable, can't confirm").
///   4. `tests_passed` unreadable -> fail ("count unreadable").
///   5. `tests_added <= 0` (measured from PASSING tests) -> fail ("no tests
///      added").
///   6. `!verify_command_passed` -> fail ("suite red").
///   7. a declared coverage threshold not met (or unmeasurable) -> fail.
///   8. otherwise -> pass.
pub fn evaluate(input: &WorkGateInput) -> WorkGateResult {
    if !input.dispatch_ok {
        return fail(
            "dispatch did not complete cleanly (runtime/transport error) — \
             cannot confirm any work was done",
        );
    }

    if let Some(reason) = &input.command_tampered {
        return fail(format!("verify command altered: {reason}"));
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

    let Some(passed_now) = input.tests_passed else {
        return fail(
            "verify output's passing-test count is unreadable (unrecognized \
             test-runner output format) — cannot confirm tests were added",
        );
    };
    let added = passed_now as i64 - input.baseline_test_count as i64;
    if added <= 0 {
        return WorkGateResult {
            passed: false,
            details: format!(
                "no tests added: fixture baseline declares {} passing tests, verify now \
                 reports {passed_now} passing{}",
                input.baseline_test_count,
                match (input.tests_skipped, input.tests_todo) {
                    (Some(s), Some(t)) if s + t > 0 => {
                        format!(" ({s} skipped, {t} todo — neither counts as added)")
                    }
                    _ => String::new(),
                }
            ),
            tests_added: Some(added),
        };
    }

    if !input.verify_command_passed {
        let detail = match input.tests_failed {
            Some(f) => format!("{f} tests failing"),
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
            "{added} passing test(s) added ({passed_now} passing now, baseline {}), suite green{}",
            input.baseline_test_count,
            match (input.coverage_min_pct, input.coverage_pct) {
                (Some(min), Some(pct)) => format!(", coverage {pct:.1}% >= {min:.1}%"),
                _ => String::new(),
            }
        ),
        tests_added: Some(added),
    }
}

/// One reporter's fixed-order summary block, exactly as `node --test`
/// prints it — measured directly (node v24.16.0, scratch copy of
/// pepper-grinder) against BOTH the default "spec" reporter (`ℹ`-prefixed)
/// and the `--test-reporter=tap` reporter (`#`-prefixed): both emit the
/// SAME eight keys in the SAME fixed order —
/// `tests, suites, pass, fail, cancelled, skipped, todo, duration_ms`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct NodeTestSummary {
    pub tests: u64,
    pub pass: u64,
    pub fail: u64,
    pub cancelled: u64,
    pub skipped: u64,
    pub todo: u64,
}

const SUMMARY_INT_KEYS: [&str; 7] = [
    "tests",
    "suites",
    "pass",
    "fail",
    "cancelled",
    "skipped",
    "todo",
];

/// Parse ONE `<prefix> <key> <value>` line, requiring the value to be the
/// ENTIRE remainder of the line (exactly one whitespace-delimited token).
/// Rejecting a trailing extra token is what stops a forged line like
/// `ℹ tests 99 (fake)` from being counted as a real 2-token summary line —
/// PROVEN as a real forgery vector, not dead code: without this check that
/// exact line parses (key="tests", value="99") and a smuggled reason string
/// after it is silently dropped by a `.next().next()` scan (see
/// `rejects_a_smuggled_trailing_token_on_an_otherwise_valid_line` below).
fn parse_kv_line(line: &str, prefix: char) -> Option<(&str, &str)> {
    let rest = line.trim_start().strip_prefix(prefix)?.trim_start();
    let mut it = rest.splitn(2, char::is_whitespace);
    let key = it.next()?;
    let value = it.next()?.trim();
    if value.is_empty() || value.split_whitespace().count() != 1 {
        return None;
    }
    Some((key, value))
}

/// Match `lines[0..7]` against the 7 fixed integer keys (`tests` ..
/// `todo`), then require `lines[7]` to be a `duration_ms` line parsing as a
/// plain number (node emits it as a float, e.g. `34.671` — NOT stored, just
/// validated, since its presence is what marks this as node's OWN real
/// summary block rather than 7 coincidentally-numbered lines).
fn try_match_summary_block(lines: &[&str], prefix: char) -> Option<NodeTestSummary> {
    if lines.len() < 8 {
        return None;
    }
    let mut vals = [0u64; 7];
    for (idx, key) in SUMMARY_INT_KEYS.iter().enumerate() {
        let (k, v) = parse_kv_line(lines[idx], prefix)?;
        if k != *key {
            return None;
        }
        vals[idx] = v.parse::<u64>().ok()?;
    }
    let (k, v) = parse_kv_line(lines[7], prefix)?;
    if k != "duration_ms" {
        return None;
    }
    v.parse::<f64>().ok()?;
    Some(NodeTestSummary {
        tests: vals[0],
        pass: vals[2],
        fail: vals[3],
        cancelled: vals[4],
        skipped: vals[5],
        todo: vals[6],
    })
}

/// Parse `node --test`'s own summary block out of a verify command's
/// captured output, ANCHORED to the exact fixed-order 8-line sequence node
/// itself always emits (#2833 review finding 2 — a free-form per-line scan
/// let a single forged `ℹ tests 99` line anywhere in the output win).
/// Scans for every complete, contiguous, same-prefix match and returns the
/// LAST one — a verify command that reruns the suite internally reports the
/// final state, matching this module's existing "last occurrence wins"
/// discipline, while still requiring each candidate to be a real,
/// structurally-complete node summary rather than any lone matching line.
///
/// Deliberately narrow: this is the ONLY reporter shape this module
/// understands (node's default "spec" reporter and its "tap" reporter — the
/// two verified live). An unrecognized format yields `None`, which the gate
/// turns into an explicit "count unreadable" failure — never a silent pass.
pub fn parse_node_test_summary(output: &str) -> Option<NodeTestSummary> {
    let lines: Vec<&str> = output.lines().collect();
    let mut found = None;
    for prefix in ['ℹ', '#'] {
        for start in 0..lines.len() {
            if let Some(summary) = try_match_summary_block(&lines[start..], prefix) {
                found = Some(summary);
            }
        }
    }
    found
}

/// Parse the LINE-coverage percentage from node's `--experimental-test-
/// coverage` summary table, ANCHORED to node's own `start of coverage
/// report` / `end of coverage report` markers (#2833 review finding 2 — an
/// unanchored scan for any `all files | ...` row let a forged row printed
/// BEFORE the real table win, since the free scan returned the FIRST
/// match). Only a row seen strictly between a matching start/end pair, with
/// the SAME reporter prefix as the markers, counts; the LAST complete
/// block's row wins if the output contains more than one (a verify command
/// that reruns the suite). A row outside any such block — including one
/// printed by a test's own `console.log` before real coverage output —
/// never reaches the `all files` check at all.
///
/// Returns `None` when no complete coverage block appears — including when
/// coverage wasn't measured, which the gate (when a threshold is declared)
/// turns into an explicit failure rather than skipping the check.
pub fn parse_node_line_coverage_pct(output: &str) -> Option<f64> {
    let mut result = None;
    for prefix in ['ℹ', '#'] {
        let mut in_block = false;
        let mut block_pct: Option<f64> = None;
        for line in output.lines() {
            let Some(rest) = line.trim_start().strip_prefix(prefix).map(str::trim_start) else {
                continue;
            };
            match rest.trim() {
                "start of coverage report" => {
                    in_block = true;
                    block_pct = None;
                }
                "end of coverage report" => {
                    if in_block {
                        if let Some(pct) = block_pct {
                            result = Some(pct);
                        }
                    }
                    in_block = false;
                }
                row if in_block && row.trim_start().starts_with("all files") => {
                    if let Some(pct) = row
                        .split('|')
                        .nth(1)
                        .and_then(|s| s.trim().parse::<f64>().ok())
                    {
                        block_pct = Some(pct);
                    }
                }
                _ => {}
            }
        }
    }
    result
}

/// How a fixture's `.fixture.json` relates to the baseline-test-count gate.
#[derive(Debug, Clone, PartialEq)]
enum BaselineDeclaration {
    /// No `.fixture.json`, or one with no `baseline.test_count` key at all
    /// — "this workload doesn't use the gate", not "something is wrong".
    NotDeclared,
    /// A `.fixture.json` exists and has a `baseline.test_count` key, but it
    /// isn't a non-negative integer (a string, a float, negative) — or the
    /// file itself failed to load/parse. #2833 review finding 3: this used
    /// to collapse into `NotDeclared` via `.ok()?`, silently letting a
    /// mistyped baseline disable the gate and leave the raw exit-0 verdict
    /// standing.
    Malformed(String),
    /// A valid non-negative integer baseline — the gate is fully active.
    Declared(u64),
}

fn fixture_baseline_declaration(source_sandbox_dir: &Path) -> BaselineDeclaration {
    let manifest = match FixtureManifest::load_from_dir(source_sandbox_dir) {
        Ok(m) => m,
        Err(e) => {
            // The overwhelmingly common reason this fails is "no
            // .fixture.json at all" (a self-contained workload with no
            // requires_fixture) — NOT malformed. Only escalate when the
            // file genuinely exists but is bad (unparseable JSON, fails
            // FixtureManifest's own validation).
            return if source_sandbox_dir.join(".fixture.json").exists() {
                BaselineDeclaration::Malformed(format!("{e:#}"))
            } else {
                BaselineDeclaration::NotDeclared
            };
        }
    };
    match manifest.baseline.get("test_count") {
        None => BaselineDeclaration::NotDeclared,
        Some(v) => match v.as_u64() {
            Some(n) => BaselineDeclaration::Declared(n),
            None => BaselineDeclaration::Malformed(format!(
                "baseline.test_count is not a non-negative integer: {v}"
            )),
        },
    }
}

/// Read a directory's `package.json` `scripts.test` string, if present.
fn read_package_test_script(dir: &Path) -> Option<String> {
    let raw = fs::read_to_string(dir.join("package.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("scripts")?.get("test")?.as_str().map(str::to_string)
}

/// Detect whether the sandbox's OWN definition of "the verify command" was
/// edited relative to the pristine fixture (#2833 review finding 5 — the
/// verify command runs the sandbox's `package.json` "test" script, which is
/// agent-editable; `"test": "echo tests-pass"` would otherwise sail
/// through). Deliberately narrow to this ONE field, npm-fixture-specific:
/// `None` (not tampered / check doesn't apply) whenever the FIXTURE itself
/// has no `package.json` `scripts.test` to compare against — a non-npm
/// fixture is simply out of scope for this specific check, not flagged.
fn detect_command_tampering(fixture_dir: &Path, sandbox_dir: &Path) -> Option<String> {
    let fixture_script = read_package_test_script(fixture_dir)?;
    match read_package_test_script(sandbox_dir) {
        Some(s) if s == fixture_script => None,
        Some(s) => Some(format!(
            "sandbox's package.json scripts.test changed from `{fixture_script}` to `{s}`"
        )),
        None => Some(format!(
            "sandbox's package.json no longer declares scripts.test \
             (fixture declares `{fixture_script}`)"
        )),
    }
}

fn read_manifest(run_dir: &Path) -> Result<serde_json::Value> {
    let manifest_path = run_dir.join("manifest.json");
    let raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("parsing {} as JSON", manifest_path.display()))
}

fn write_manifest(run_dir: &Path, manifest: &serde_json::Value) -> Result<()> {
    let manifest_path = run_dir.join("manifest.json");
    fs::write(&manifest_path, serde_json::to_string_pretty(manifest)?)
        .with_context(|| format!("writing {}", manifest_path.display()))
}

/// Force `verify.passed = false` with `reason` into the manifest's `verify`
/// object, when one exists (`Ok(None)` when `verify` is null/absent — there
/// is no raw verdict to override in the first place). This is the fail-
/// CLOSED path: `apply` routes here whenever a baseline is known to be
/// declared (valid or malformed) but something else about applying the gate
/// went wrong, so the raw exit-0 verdict can never stand unexamined.
fn gate_with_forced_failure(run_dir: &Path, reason: &str) -> Result<Option<WorkGateResult>> {
    let mut manifest = read_manifest(run_dir)?;
    let verify_is_null = manifest.get("verify").map(|v| v.is_null()).unwrap_or(true);
    if verify_is_null {
        return Ok(None);
    }
    let obj = manifest
        .as_object_mut()
        .ok_or_else(|| anyhow!("manifest is not a JSON object"))?;
    let verify_obj = obj
        .get_mut("verify")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| anyhow!("manifest `verify` is not a JSON object"))?;
    verify_obj.insert("passed".to_string(), json!(false));
    verify_obj.insert("details".to_string(), json!(reason));
    verify_obj.insert(
        "work_gate".to_string(),
        json!({ "forced_failure_reason": reason }),
    );
    let bumped = obj
        .get("schema_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .max(6);
    obj.insert("schema_version".to_string(), json!(bumped));
    write_manifest(run_dir, &manifest)?;
    Ok(Some(WorkGateResult {
        passed: false,
        details: reason.to_string(),
        tests_added: None,
    }))
}

/// A workload declares `coverage_min_pct` against a fixture with NO
/// baseline declared (#2833 review finding 6). The gate itself does not
/// apply — there's no baseline to measure "added" against — but silently
/// ignoring the threshold hides a real misconfiguration. Warns (console,
/// same `[lab] warn:` convention as every other best-effort step in this
/// area of the codebase) and records a note in the manifest's
/// `verify.details`, WITHOUT changing `verify.passed`.
fn warn_coverage_declared_without_baseline(
    run_dir: &Path,
    coverage_min_pct: Option<f32>,
) -> Result<Option<WorkGateResult>> {
    let Some(min) = coverage_min_pct else {
        return Ok(None); // no coverage threshold declared — fully untouched, today's behavior.
    };
    let mut manifest = read_manifest(run_dir)?;
    let note = format!(
        "note: workload declares coverage_min_pct={min:.1}% but this run's fixture declares no \
         baseline.test_count, so the write-the-tests work gate does not apply here — coverage \
         was NOT graded"
    );
    eprintln!("[lab] warn: {note}");
    let Some(obj) = manifest.as_object_mut() else {
        return Ok(None);
    };
    let Some(verify_obj) = obj.get_mut("verify").and_then(|v| v.as_object_mut()) else {
        return Ok(None); // verify null/absent — nothing to annotate.
    };
    let existing = verify_obj
        .get("details")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let updated = if existing.is_empty() {
        note
    } else {
        format!("{existing}; {note}")
    };
    let passed = verify_obj
        .get("passed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    verify_obj.insert("details".to_string(), json!(updated));
    write_manifest(run_dir, &manifest)?;
    Ok(Some(WorkGateResult {
        passed,
        details: updated,
        tests_added: None,
    }))
}

/// The full gate, run only once `baseline_test_count` is known valid.
/// Mirrors the pre-review `apply`'s body, plus: `tests_passed` (not
/// `tests_total`) drives the decision, the tamper check runs, and the
/// richer evidence block (skipped/todo/cancelled/tampering) is recorded.
fn run_full_gate(
    run_dir: &Path,
    source_sandbox_dir: &Path,
    per_run_sandbox_dir: &Path,
    dispatch_ok: bool,
    baseline_test_count: u64,
    coverage_min_pct: Option<f32>,
) -> Result<Option<WorkGateResult>> {
    let mut manifest = read_manifest(run_dir)?;

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

    // #2833 review finding 7a: missing/non-bool `passed` must read as
    // FAILED, never passed — `.unwrap_or(false)` is load-bearing, not
    // decorative (see `apply_treats_missing_command_passed_as_failed`).
    let verify_command_passed = manifest
        .get("verify")
        .and_then(|v| v.get("passed"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let verify_output = fs::read_to_string(run_dir.join("verify-output.txt")).unwrap_or_default();
    let summary = parse_node_test_summary(&verify_output);
    let coverage_pct = if coverage_min_pct.is_some() {
        parse_node_line_coverage_pct(&verify_output)
    } else {
        None
    };
    let command_tampered = detect_command_tampering(source_sandbox_dir, per_run_sandbox_dir);

    let input = WorkGateInput {
        dispatch_ok,
        sandbox_changed,
        baseline_test_count,
        tests_passed: summary.map(|s| s.pass),
        tests_total: summary.map(|s| s.tests),
        tests_failed: summary.map(|s| s.fail),
        tests_skipped: summary.map(|s| s.skipped),
        tests_todo: summary.map(|s| s.todo),
        verify_command_passed,
        command_tampered: command_tampered.clone(),
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
            "tests_total": summary.map(|s| s.tests),
            "tests_passed": summary.map(|s| s.pass),
            "tests_added": gate_result.tests_added,
            "tests_failed": summary.map(|s| s.fail),
            "tests_skipped": summary.map(|s| s.skipped),
            "tests_todo": summary.map(|s| s.todo),
            "sandbox_changed": sandbox_changed,
            "coverage_min_pct": coverage_min_pct,
            "coverage_pct": coverage_pct,
            "command_tampered": command_tampered,
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

    write_manifest(run_dir, &manifest)?;
    Ok(Some(gate_result))
}

/// The I/O wrapper `lab::run::lab_run` calls once per run, right after
/// `enrich_manifest_with_fixture_info` has written `final_hash` (top-level,
/// from the provider) and `fixture.baseline_hash` (from the enricher) into
/// `<run_dir>/manifest.json`.
///
/// Returns `Ok(None)` (manifest untouched) whenever the fixture declares no
/// `baseline.test_count` and the workload declares no coverage threshold
/// either — "Rules 1-2 apply when the run's fixture declares a baseline
/// test count; a workload/fixture that declares none keeps today's
/// behavior" (#2833). Otherwise returns `Ok(Some(result))` so the caller can
/// sync the SAME gated verdict into the in-memory `RunResult` it already
/// holds (the CLI's printed notes and `RunOutcome::verify_passed` must not
/// disagree with what just got written to `manifest.json`).
///
/// FAIL-CLOSED (#2833 review finding 4): a **malformed** baseline (a
/// `.fixture.json` with a `baseline.test_count` that isn't a non-negative
/// integer) NEVER leaves the raw, ungated verdict standing — it is
/// converted into an explicit FAIL via [`gate_with_forced_failure`], since
/// silently falling back to "not declared" is exactly what let a typo
/// disable the gate. A **valid, declared** baseline whose gate application
/// hits an internal error (in practice: `manifest.verify` present but not
/// the `{passed, details}` object shape — a shape nothing here can write a
/// verdict into either) bubbles `Err` to the caller rather than pretending
/// to recover; `lab::run::lab_run`'s own `Err` arm is what fails the
/// in-memory `RunResult::verify` closed in that residual case, regardless
/// of what could be persisted to `manifest.json`.
pub fn apply(
    run_dir: &Path,
    source_sandbox_dir: &Path,
    per_run_sandbox_dir: &Path,
    dispatch_ok: bool,
    coverage_min_pct: Option<f32>,
) -> Result<Option<WorkGateResult>> {
    match fixture_baseline_declaration(source_sandbox_dir) {
        BaselineDeclaration::NotDeclared => {
            warn_coverage_declared_without_baseline(run_dir, coverage_min_pct)
        }
        BaselineDeclaration::Malformed(reason) => gate_with_forced_failure(
            run_dir,
            &format!("fixture's baseline.test_count is malformed: {reason}"),
        ),
        // A baseline is known declared (valid). `run_full_gate`'s only
        // realistic internal failure mode is `manifest.verify` being
        // present-but-not-an-object — a shape neither it NOR
        // `gate_with_forced_failure` (which needs the same object to write
        // a forced verdict into) can do anything with, so retrying via the
        // forced-failure path here would be dead code that can never
        // recover differently. This `Err` therefore bubbles to the caller
        // as-is — `lab::run::lab_run` is what actually fails closed on it,
        // by forcing its own in-memory `RunResult::verify` regardless of
        // what could or couldn't be written to `manifest.json`.
        BaselineDeclaration::Declared(baseline_test_count) => run_full_gate(
            run_dir,
            source_sandbox_dir,
            per_run_sandbox_dir,
            dispatch_ok,
            baseline_test_count,
            coverage_min_pct,
        ),
    }
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
            tests_passed: Some(22),
            tests_total: Some(22),
            tests_failed: Some(0),
            tests_skipped: Some(0),
            tests_todo: Some(0),
            verify_command_passed: true,
            command_tampered: None,
            coverage_min_pct: None,
            coverage_pct: None,
        }
    }

    // ─── evaluate: the operator's pass rule ─────────────────────────

    #[test]
    fn no_op_run_fails_no_work() {
        let input = WorkGateInput {
            sandbox_changed: Some(false),
            tests_passed: Some(14),
            tests_total: Some(14),
            tests_failed: Some(0),
            ..base_input()
        };
        let r = evaluate(&input);
        assert!(!r.passed);
        assert!(r.details.contains("no work"), "got: {}", r.details);
    }

    #[test]
    fn changed_sandbox_but_same_passing_count_fails_no_tests_added() {
        let input = WorkGateInput {
            tests_passed: Some(14),
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

    /// #2833 review finding 1, PROVEN at the `evaluate` layer: `tests_total`
    /// grew by 2 (16 total vs baseline 14) but NEITHER new test actually
    /// passed — one is `.skip`, one is `.todo`. Measuring "added" from
    /// `tests_total` (the pre-fix behavior) would pass this. Measuring from
    /// `tests_passed` fails it, since `tests_passed` is still 14.
    #[test]
    fn skipped_and_todo_tests_do_not_count_as_added() {
        let input = WorkGateInput {
            tests_passed: Some(14),
            tests_total: Some(16),
            tests_failed: Some(0),
            tests_skipped: Some(1),
            tests_todo: Some(1),
            verify_command_passed: true, // node exits 0 — skip/todo are not failures
            ..base_input()
        };
        let r = evaluate(&input);
        assert!(
            !r.passed,
            "a model marking its failing test .skip must not pass"
        );
        assert!(r.details.contains("no tests added"), "got: {}", r.details);
        assert!(
            r.details.contains("skipped") && r.details.contains("todo"),
            "got: {}",
            r.details
        );
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
        assert!(r.details.contains("3 tests failing"), "got: {}", r.details);
    }

    #[test]
    fn dispatch_runtime_error_with_green_untouched_suite_fails() {
        // The #2833 issue's own headline example: `ok: false`, hashes
        // identical, verify's raw exit code still 0.
        let input = WorkGateInput {
            dispatch_ok: false,
            sandbox_changed: Some(false),
            tests_passed: Some(14),
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
            tests_passed: None,
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

    /// #2833 review finding 5, at the `evaluate` layer: even with the
    /// sandbox changed and tests apparently added, a tampered verify
    /// command must fail regardless — everything downstream of it is
    /// untrustworthy.
    #[test]
    fn command_tampering_fails_even_with_apparent_work() {
        let input = WorkGateInput {
            command_tampered: Some("scripts.test changed".into()),
            ..base_input()
        };
        let r = evaluate(&input);
        assert!(!r.passed);
        assert!(
            r.details.contains("verify command altered"),
            "got: {}",
            r.details
        );
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

    // ─── parse_node_test_summary: real node --test output, block-anchored ──

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
# suites 5
# pass 19
# fail 3
# cancelled 0
# skipped 0
# todo 0
# duration_ms 12.3
";

    #[test]
    fn parses_spec_reporter_summary() {
        // Measured directly: `npm test` in a scratch copy of pepper-grinder
        // under node v24.16.0 — the default ("spec") reporter uses the `ℹ`
        // prefix, NOT `#` (which only the `tap` reporter emits).
        let s = parse_node_test_summary(SPEC_REPORTER_OUTPUT).unwrap();
        assert_eq!((s.tests, s.pass, s.fail), (22, 22, 0));
    }

    #[test]
    fn parses_tap_reporter_summary() {
        // Measured directly: `node --test --test-reporter=tap` in the same
        // scratch copy — same 8-key fixed order as the spec reporter, `#`
        // prefix instead of `ℹ`.
        let s = parse_node_test_summary(TAP_REPORTER_OUTPUT).unwrap();
        assert_eq!((s.tests, s.pass, s.fail), (22, 19, 3));
    }

    #[test]
    fn takes_the_last_complete_block_when_the_suite_reruns() {
        let output = format!("{SPEC_REPORTER_OUTPUT}\n--- rerun ---\n{TAP_REPORTER_OUTPUT}");
        let s = parse_node_test_summary(&output).unwrap();
        assert_eq!((s.pass, s.fail), (19, 3)); // the TAP block, which comes second
    }

    #[test]
    fn unrecognized_output_format_is_unreadable() {
        assert!(parse_node_test_summary("PASS  22 tests, 0 failures\n").is_none());
        assert!(parse_node_test_summary("").is_none());
    }

    /// #2833 review finding 2, PROVEN: without block-anchoring, a single
    /// forged `ℹ tests 99` line placed ANYWHERE (here: before the real
    /// summary, as a test's own `console.log` would print it) would have
    /// been counted directly. Anchored to the fixed 8-line block, an
    /// isolated line — real key, real-looking value, no siblings in the
    /// right order right after it — never matches.
    #[test]
    fn a_lone_forged_summary_line_before_the_real_block_does_not_win() {
        let output =
            format!("ℹ tests 99\nℹ pass 99\nsome test's own console.log\n\n{SPEC_REPORTER_OUTPUT}");
        let s = parse_node_test_summary(&output).unwrap();
        assert_eq!(s.pass, 22, "the forged 99 must not have been picked up");
    }

    /// The vulnerability actually latent in the PRE-review parser: it took
    /// the LAST occurrence of each `key` line independently, so a lone
    /// forged `ℹ pass 99` printed AFTER the real summary (e.g. by a global
    /// `after()` hook, which runs once all tests finish) would have
    /// overwritten the real `pass` value even though `tests`/`fail` still
    /// came from the real block. Block-anchoring closes this: the forged
    /// line has no siblings in the right position right after it, so it
    /// never forms a complete match and the real (earlier, complete) block
    /// wins instead.
    #[test]
    fn a_lone_forged_summary_line_after_the_real_block_does_not_win() {
        let output = format!("{SPEC_REPORTER_OUTPUT}\nℹ pass 999\n");
        let s = parse_node_test_summary(&output).unwrap();
        assert_eq!(
            s.pass, 22,
            "the forged trailing pass=999 must not have been picked up"
        );
    }

    #[test]
    fn rejects_a_smuggled_trailing_token_on_an_otherwise_valid_line() {
        let output = "ℹ tests 99 (fake)\nℹ suites 1\nℹ pass 99\nℹ fail 0\nℹ cancelled 0\nℹ skipped 0\nℹ todo 0\nℹ duration_ms 1.0\n";
        assert!(
            parse_node_test_summary(output).is_none(),
            "a trailing extra token must break the block match, not be silently ignored"
        );
    }

    #[test]
    fn coverage_table_row_alone_is_never_mistaken_for_a_summary_block() {
        let output = "ℹ all files               |  91.82 |    77.42 |   88.89 | \n";
        assert!(parse_node_test_summary(output).is_none());
    }

    // ─── parse_node_line_coverage_pct: anchored to node's real block ──

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

    /// #2833 review finding 2, PROVEN: a test's own `console.log` prints a
    /// forged `all files | 100.00 | ...` row (unprefixed — a real
    /// `console.log` from test code isn't reporter-prefixed) BEFORE node's
    /// real coverage table. An unanchored first-match scan takes the fake
    /// 100%; anchored to inside a real `start`/`end of coverage report`
    /// block, the unprefixed forged row is skipped entirely.
    #[test]
    fn a_forged_coverage_row_before_the_real_table_does_not_win() {
        let forged = format!("all files | 100.00 | 100.00 | 100.00 |\n{COVERAGE_OUTPUT}");
        assert_eq!(
            parse_node_line_coverage_pct(&forged),
            Some(91.82),
            "the forged 100% row must not have been picked up"
        );
    }

    #[test]
    fn a_forged_coverage_row_after_a_real_table_but_outside_any_block_does_not_win() {
        // Same forgery, but placed AFTER the real (properly closed) block —
        // still outside start/end markers, so still rejected.
        let forged = format!("{COVERAGE_OUTPUT}\nall files | 100.00 | 100.00 | 100.00 |\n");
        assert_eq!(parse_node_line_coverage_pct(&forged), Some(91.82));
    }

    // ─── fixture_baseline_declaration ────────────────────────────────

    fn write_fixture_json(dir: &Path, json: &str) {
        fs::write(dir.join(".fixture.json"), json).unwrap();
    }

    #[test]
    fn no_fixture_json_is_not_declared() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            fixture_baseline_declaration(tmp.path()),
            BaselineDeclaration::NotDeclared
        );
    }

    #[test]
    fn fixture_with_no_baseline_key_is_not_declared() {
        let tmp = TempDir::new().unwrap();
        write_fixture_json(tmp.path(), r#"{"name": "demo"}"#);
        assert_eq!(
            fixture_baseline_declaration(tmp.path()),
            BaselineDeclaration::NotDeclared
        );
    }

    #[test]
    fn valid_integer_baseline_is_declared() {
        let tmp = TempDir::new().unwrap();
        write_fixture_json(
            tmp.path(),
            r#"{"name": "demo", "baseline": {"test_count": 14}}"#,
        );
        assert_eq!(
            fixture_baseline_declaration(tmp.path()),
            BaselineDeclaration::Declared(14)
        );
    }

    /// #2833 review finding 3, PROVEN: a string baseline must not silently
    /// collapse into "not declared" (which would leave the raw exit-0
    /// verdict standing).
    #[test]
    fn string_baseline_is_malformed_not_not_declared() {
        let tmp = TempDir::new().unwrap();
        write_fixture_json(
            tmp.path(),
            r#"{"name": "demo", "baseline": {"test_count": "14"}}"#,
        );
        assert!(matches!(
            fixture_baseline_declaration(tmp.path()),
            BaselineDeclaration::Malformed(_)
        ));
    }

    #[test]
    fn float_baseline_is_malformed() {
        let tmp = TempDir::new().unwrap();
        write_fixture_json(
            tmp.path(),
            r#"{"name": "demo", "baseline": {"test_count": 14.5}}"#,
        );
        assert!(matches!(
            fixture_baseline_declaration(tmp.path()),
            BaselineDeclaration::Malformed(_)
        ));
    }

    #[test]
    fn negative_baseline_is_malformed() {
        let tmp = TempDir::new().unwrap();
        write_fixture_json(
            tmp.path(),
            r#"{"name": "demo", "baseline": {"test_count": -1}}"#,
        );
        assert!(matches!(
            fixture_baseline_declaration(tmp.path()),
            BaselineDeclaration::Malformed(_)
        ));
    }

    #[test]
    fn unparseable_fixture_json_is_malformed_not_not_declared() {
        let tmp = TempDir::new().unwrap();
        write_fixture_json(tmp.path(), "not json at all");
        assert!(matches!(
            fixture_baseline_declaration(tmp.path()),
            BaselineDeclaration::Malformed(_)
        ));
    }

    // ─── detect_command_tampering ────────────────────────────────────

    fn write_package_json(dir: &Path, test_script: &str) {
        fs::write(
            dir.join("package.json"),
            format!(r#"{{"scripts": {{"test": "{test_script}"}}}}"#),
        )
        .unwrap();
    }

    #[test]
    fn identical_test_script_is_not_tampered() {
        let fixture = TempDir::new().unwrap();
        let sandbox = TempDir::new().unwrap();
        write_package_json(fixture.path(), "npm test");
        write_package_json(sandbox.path(), "npm test");
        assert_eq!(
            detect_command_tampering(fixture.path(), sandbox.path()),
            None
        );
    }

    #[test]
    fn changed_test_script_is_tampered() {
        let fixture = TempDir::new().unwrap();
        let sandbox = TempDir::new().unwrap();
        write_package_json(fixture.path(), "node --test test/*.test.js");
        write_package_json(sandbox.path(), "echo 'ℹ tests 99'; echo 'ℹ pass 99'");
        let reason = detect_command_tampering(fixture.path(), sandbox.path()).unwrap();
        assert!(reason.contains("changed from"), "got: {reason}");
    }

    #[test]
    fn no_fixture_package_json_skips_the_check() {
        // Non-npm fixture — out of scope for this specific field-check.
        let fixture = TempDir::new().unwrap();
        let sandbox = TempDir::new().unwrap();
        assert_eq!(
            detect_command_tampering(fixture.path(), sandbox.path()),
            None
        );
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
    fn apply_is_a_noop_when_fixture_declares_no_baseline_and_no_coverage() {
        let run = TempDir::new().unwrap();
        let fixture = TempDir::new().unwrap();
        let sandbox = TempDir::new().unwrap();
        // No .fixture.json at all.
        write_manifest(
            run.path(),
            r#"{"schema_version":5,"verify":{"passed":true,"details":"verify command exited 0"}}"#,
        );
        apply(run.path(), fixture.path(), sandbox.path(), true, None).unwrap();
        let raw = fs::read_to_string(run.path().join("manifest.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Untouched: still the raw provider-written shape, no work_gate.
        assert_eq!(v["verify"]["passed"], true);
        assert!(v["verify"].get("work_gate").is_none());
    }

    /// #2833 review finding 6.
    #[test]
    fn apply_warns_but_does_not_gate_coverage_without_a_baseline() {
        let run = TempDir::new().unwrap();
        let fixture = TempDir::new().unwrap();
        let sandbox = TempDir::new().unwrap();
        write_manifest(
            run.path(),
            r#"{"schema_version":5,"verify":{"passed":true,"details":"verify command exited 0"}}"#,
        );
        apply(run.path(), fixture.path(), sandbox.path(), true, Some(90.0)).unwrap();
        let raw = fs::read_to_string(run.path().join("manifest.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // `passed` unchanged (not gated)...
        assert_eq!(v["verify"]["passed"], true);
        // ...but the operator is told, in the manifest itself.
        assert!(
            v["verify"]["details"]
                .as_str()
                .unwrap()
                .contains("coverage_min_pct=90.0"),
            "got: {}",
            v["verify"]["details"]
        );
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
        apply(run.path(), fixture.path(), fixture.path(), true, None).unwrap();
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
        apply(run.path(), fixture.path(), fixture.path(), true, None).unwrap();
        let raw = fs::read_to_string(run.path().join("manifest.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["verify"]["passed"], true);
        assert_eq!(v["verify"]["work_gate"]["tests_added"], 8);
        assert_eq!(v["verify"]["work_gate"]["command_passed"], true);
    }

    /// #2833 review finding 1, end-to-end through `apply` (not just the pure
    /// `evaluate` layer): 16 total tests vs baseline 14 LOOKS like "2
    /// added", but 1 is `.skip` and 1 is `.todo` — only 14 actually pass.
    /// Proves the WIRING in `run_full_gate` reads `pass`, not `tests`.
    #[test]
    fn apply_end_to_end_ignores_skipped_and_todo_when_counting_added() {
        let run = TempDir::new().unwrap();
        let fixture = TempDir::new().unwrap();
        write_fixture(fixture.path(), 14);
        write_manifest(
            run.path(),
            r#"{"schema_version":5,"final_hash":"blake3:after",
               "fixture":{"baseline_hash":"blake3:before"},
               "verify":{"passed":true,"details":"verify command exited 0"}}"#,
        );
        let output = "\
ℹ tests 16
ℹ suites 5
ℹ pass 14
ℹ fail 0
ℹ cancelled 0
ℹ skipped 1
ℹ todo 1
ℹ duration_ms 40.0
";
        fs::write(run.path().join("verify-output.txt"), output).unwrap();
        apply(run.path(), fixture.path(), fixture.path(), true, None).unwrap();
        let raw = fs::read_to_string(run.path().join("manifest.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["verify"]["passed"], false, "16 total is NOT 16 passing");
        assert!(v["verify"]["details"]
            .as_str()
            .unwrap()
            .contains("no tests added"));
    }

    #[test]
    fn apply_leaves_null_verify_untouched() {
        let run = TempDir::new().unwrap();
        let fixture = TempDir::new().unwrap();
        write_fixture(fixture.path(), 14);
        write_manifest(run.path(), r#"{"schema_version":5,"verify":null}"#);
        apply(run.path(), fixture.path(), fixture.path(), true, None).unwrap();
        let raw = fs::read_to_string(run.path().join("manifest.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(v["verify"].is_null());
    }

    /// #2833 review finding 3, end-to-end: a mistyped baseline must FAIL
    /// the run, not silently leave the raw (passing) verdict standing.
    #[test]
    fn apply_fails_closed_on_a_malformed_baseline() {
        let run = TempDir::new().unwrap();
        let fixture = TempDir::new().unwrap();
        write_fixture_json(
            fixture.path(),
            r#"{"name": "demo", "baseline": {"test_count": "14"}}"#,
        );
        write_manifest(
            run.path(),
            r#"{"schema_version":5,"verify":{"passed":true,"details":"verify command exited 0"}}"#,
        );
        apply(run.path(), fixture.path(), fixture.path(), true, None).unwrap();
        let raw = fs::read_to_string(run.path().join("manifest.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["verify"]["passed"], false);
        assert!(
            v["verify"]["details"]
                .as_str()
                .unwrap()
                .contains("malformed"),
            "got: {}",
            v["verify"]["details"]
        );
    }

    /// #2833 review finding 5, end-to-end.
    #[test]
    fn apply_fails_a_run_whose_test_script_was_edited() {
        let run = TempDir::new().unwrap();
        let fixture = TempDir::new().unwrap();
        let sandbox = TempDir::new().unwrap();
        write_fixture(fixture.path(), 14);
        write_package_json(fixture.path(), "node --test test/*.test.js");
        write_package_json(sandbox.path(), "echo ok"); // the tamper
        write_manifest(
            run.path(),
            r#"{"schema_version":5,"final_hash":"blake3:after",
               "fixture":{"baseline_hash":"blake3:before"},
               "verify":{"passed":true,"details":"verify command exited 0"}}"#,
        );
        fs::write(run.path().join("verify-output.txt"), SPEC_REPORTER_OUTPUT).unwrap();
        apply(run.path(), fixture.path(), sandbox.path(), true, None).unwrap();
        let raw = fs::read_to_string(run.path().join("manifest.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["verify"]["passed"], false);
        assert!(v["verify"]["details"]
            .as_str()
            .unwrap()
            .contains("verify command altered"));
    }

    /// #2833 review finding 7a, end-to-end: a manifest whose `verify`
    /// object exists but has no `passed` KEY (malformed provider write, or
    /// a future format drift) must read as FAILED, never passed.
    #[test]
    fn apply_treats_missing_command_passed_as_failed() {
        let run = TempDir::new().unwrap();
        let fixture = TempDir::new().unwrap();
        write_fixture(fixture.path(), 14);
        write_manifest(
            run.path(),
            r#"{"schema_version":5,"final_hash":"blake3:after",
               "fixture":{"baseline_hash":"blake3:before"},
               "verify":{"details":"no passed key at all"}}"#,
        );
        fs::write(run.path().join("verify-output.txt"), SPEC_REPORTER_OUTPUT).unwrap();
        apply(run.path(), fixture.path(), fixture.path(), true, None).unwrap();
        let raw = fs::read_to_string(run.path().join("manifest.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["verify"]["passed"], false);
    }

    /// #2833 review finding 4, end-to-end (fail-closed on an internal
    /// error): `verify` is present (non-null — a bare string, not the
    /// `{passed, details}` shape) once a baseline IS known declared. That
    /// makes `run_full_gate`'s `.as_object_mut()` fail internally — the
    /// whole point of this test is that `apply` must still turn this into
    /// an explicit recorded FAILURE (via `gate_with_forced_failure`)
    /// instead of bubbling the raw internal `Err` and leaving whatever was
    /// on disk unexamined.
    #[test]
    fn apply_fails_closed_on_an_internal_gate_error_once_baseline_is_known() {
        // `verify` is present (not null, so the gate is genuinely active)
        // once a baseline is known, but isn't the `{passed, details}`
        // OBJECT shape at all — a bare string. There is nowhere in this
        // shape for EITHER the real gate or a forced-failure fallback to
        // write a verdict, so `apply` correctly bubbles the error rather
        // than silently discarding it or reporting a manufactured pass.
        // `lab::run::lab_run`'s own `Err` arm (see its comment) is what
        // fails the in-memory `RunResult::verify` closed in this residual
        // case — proven directly in this module by the fact that NOTHING
        // in `manifest.json` gets left claiming `passed: true` here either.
        let run = TempDir::new().unwrap();
        let fixture = TempDir::new().unwrap();
        write_fixture(fixture.path(), 14);
        write_manifest(
            run.path(),
            r#"{"schema_version":5,"verify":"not-an-object"}"#,
        );
        let err = apply(run.path(), fixture.path(), fixture.path(), true, None).unwrap_err();
        assert!(err.to_string().contains("not a JSON object"), "got: {err}");
        // The manifest itself is untouched — critically, NOT left saying
        // `"verify": "not-an-object"` was somehow a pass.
        let raw = fs::read_to_string(run.path().join("manifest.json")).unwrap();
        assert!(raw.contains(r#""verify":"not-an-object""#));
    }
}
