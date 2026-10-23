//! (#3353) The coding grader: checks on a coding-task run's result, each tied
//! to the prompt sentence it grades.
//!
//! The write-the-tests work gate (`crate::lab::verify_gate`) asks whether the
//! suite grew and stayed green. That passes a run that adds one trivial test,
//! edits `src/`, or deletes a test the prompt said to leave alone. A workload
//! that declares `verify.grade` (`crate::workloads::grade_spec`) is graded on
//! top of the gate, by these checks, in this order:
//!
//! - `run_finished` (always): the dispatch completed. A run that errored,
//!   crashed or was cut off is not usable downstream, whatever its tests say.
//! - `scope`: which files the result touched, against the pristine fixture.
//! - `suite`: the grader's own run of the tests, on a copy of the result,
//!   read from a standard report (`junit`), green and (optionally) larger
//!   than the fixture's baseline.
//! - `portable`: the same suite, green from a second copy at another path.
//! - `style`: strings every new file of a kind must contain.
//! - `behaviors`: planted bugs, each applied to the fixture's own copy of a
//!   file inside a copy of the result; the suite must go red for every one.
//!   One pass/fail; the evidence names the behaviors whose bugs survived.
//! - `reply_required`: the run ended with a final reply. Its content is never
//!   graded: a free-text reply is not a structured-output request.
//!
//! Every declared check must quote at least one prompt sentence, and each
//! quoted sentence must be in the prompt word for word; a grader that quotes
//! something the prompt does not say refuses to grade. The model's side is
//! never touched: every check runs on copies, under a scratch directory in
//! the run directory, removed afterwards.
//!
//! SECURITY: like the verify command (`coding_task::run_verify_command`), the
//! suite runs on the HOST with the operator's privileges, bounded by a timeout
//! and killed as a process group (`darkmux_crew::bounded_command`).

mod files;
mod junit;
#[cfg(test)]
mod tests;

use crate::lab::dispatch_end::DispatchEnd;
use crate::lab::manifest::{
    GradeCheck, GradeRecord, ManifestVerify, ManifestVerifyReport, RunManifest,
};
use crate::lab::verify_gate::WorkGateResult;
use crate::workloads::grade_spec::{GradeSpec, SuiteGrade, TestReport};
use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The scratch directory, inside the run directory, that every copy lives in.
const WORK_DIR: &str = "grade-work";
/// The default bound on one run of the suite command.
const DEFAULT_SUITE_TIMEOUT_SECONDS: u64 = 120;

/// Everything one grading reads. Nothing here is written to.
pub(crate) struct GradeInput<'a> {
    pub spec: &'a GradeSpec,
    /// The workload's prompt, as the model was given it (placeholders unexpanded).
    pub prompt: &'a str,
    /// The pristine fixture the run started from.
    pub fixture: &'a Path,
    /// The run's result: its sandbox after the dispatch.
    pub result: &'a Path,
    pub dispatch: &'a DispatchEnd,
    /// The model's final reply, when there was one.
    pub reply: Option<&'a str>,
    /// The fixture's `baseline.test_count`, when it declares one.
    pub baseline_test_count: Option<u64>,
    /// An empty scratch directory the grader may fill.
    pub work: &'a Path,
}

/// Grade a result. `Err` means the grader could not grade at all (a quoted
/// sentence the prompt does not hold, a planted bug that no longer applies,
/// a suite that could not be started): the caller fails the run closed.
pub(crate) fn grade(input: &GradeInput) -> Result<GradeRecord> {
    check_asks(input.spec, input.prompt)?;
    let spec = input.spec;
    let mut checks = vec![run_finished(input.dispatch)];
    let changes = files::compare(input.fixture, input.result)?;
    if let Some(scope) = &spec.scope {
        checks.push(row(
            "scope",
            &scope.asks,
            files::scope_violations(&changes, scope),
            "only allowed files changed",
        ));
    }

    let suite = match &spec.suite {
        Some(suite_spec) => {
            let run = run_suite(
                suite_spec,
                input.result,
                &input.work.join("suite"),
                "result",
            )?;
            checks.push(suite_check(suite_spec, &run, input.baseline_test_count)?);
            Some((suite_spec, run))
        }
        None => None,
    };
    let suite_green = suite.as_ref().is_some_and(|(_, run)| run.green());

    if let (Some(portable), Some((suite_spec, suite))) = (&spec.portable, &suite) {
        let elsewhere = run_suite(
            suite_spec,
            input.result,
            &input.work.join("portable").join("elsewhere"),
            "result",
        )?;
        let same = elsewhere.counts.as_ref().map(|c| c.passed())
            == suite.counts.as_ref().map(|c| c.passed());
        let problems = match (elsewhere.green(), same) {
            (true, true) => vec![],
            (true, false) => vec![format!(
                "green, but {} passing there against {} here",
                elsewhere.passed_text(),
                suite.passed_text()
            )],
            (false, _) => vec![format!("red from another path: {}", elsewhere.describe())],
        };
        checks.push(row(
            "portable",
            &portable.asks,
            problems,
            "green from a second copy at another path",
        ));
    }

    if let Some(style) = &spec.style {
        checks.push(row(
            "style",
            &style.asks,
            files::style_violations(&changes, input.result, style)?,
            "every new file matches",
        ));
    }

    if let Some(behaviors) = &spec.behaviors {
        let problems = if suite_green {
            missed_behaviors(input, behaviors)?
        } else {
            vec!["not graded: the suite is red without any planted bug".to_string()]
        };
        let caught = format!(
            "every planted bug turns the suite red ({})",
            behavior_names(behaviors).join(", ")
        );
        checks.push(row("behaviors", &behaviors.asks, problems, &caught));
    }

    if let Some(reply) = spec.reply.as_ref().filter(|r| r.required) {
        let present = input.reply.is_some_and(|r| !r.trim().is_empty());
        let problems = if present {
            vec![]
        } else {
            vec!["no final reply".to_string()]
        };
        checks.push(row(
            "reply_required",
            &reply.asks,
            problems,
            "a final reply is present (its content is not graded)",
        ));
    }

    Ok(GradeRecord {
        passed: checks.iter().all(|c| c.passed),
        checks,
        tests_passed: suite
            .as_ref()
            .and_then(|(_, run)| run.counts.as_ref().map(|c| c.passed())),
        ungraded_passed: None,
    })
}

/// Every declared check quotes the prompt, word for word.
fn check_asks(spec: &GradeSpec, prompt: &str) -> Result<()> {
    let declared: Vec<(&str, &Vec<String>)> = [
        spec.suite.as_ref().map(|s| ("suite", &s.asks)),
        spec.scope.as_ref().map(|s| ("scope", &s.asks)),
        spec.portable.as_ref().map(|s| ("portable", &s.asks)),
        spec.style.as_ref().map(|s| ("style", &s.asks)),
        spec.behaviors.as_ref().map(|s| ("behaviors", &s.asks)),
        spec.reply.as_ref().map(|s| ("reply", &s.asks)),
    ]
    .into_iter()
    .flatten()
    .collect();
    if spec.suite.is_none() {
        if spec.portable.is_some() {
            bail!("verify.grade.portable reruns the suite: declare verify.grade.suite");
        }
        if spec.behaviors.is_some() {
            bail!("verify.grade.behaviors reruns the suite: declare verify.grade.suite");
        }
    }
    for (check, asks) in declared {
        if asks.iter().all(|a| a.trim().is_empty()) {
            bail!("verify.grade.{check} quotes no prompt sentence: every graded check names what the prompt asks");
        }
        if let Some(missing) = asks.iter().find(|a| !prompt.contains(a.as_str())) {
            bail!("verify.grade.{check} quotes \"{missing}\", which is not in the prompt word for word");
        }
    }
    Ok(())
}

fn run_finished(dispatch: &DispatchEnd) -> GradeCheck {
    let problems = match dispatch {
        DispatchEnd::Completed => vec![],
        DispatchEnd::Escalated { reason } => vec![format!(
            "the dispatch escalated ({reason}) before finishing"
        )],
        DispatchEnd::Failed => vec![
            "the dispatch did not complete (an error, a crash or a limit cut it off)".to_string(),
        ],
    };
    row("run_finished", &[], problems, "the dispatch completed")
}

/// One check row: passed when there are no problems.
fn row(id: &str, asks: &[String], problems: Vec<String>, ok: &str) -> GradeCheck {
    GradeCheck {
        id: id.to_string(),
        passed: problems.is_empty(),
        evidence: if problems.is_empty() {
            ok.to_string()
        } else {
            problems.join("; ")
        },
        asks: asks.to_vec(),
    }
}

/// One run of the suite command on a copy.
struct SuiteRun {
    outcome: SuiteOutcome,
    counts: Option<junit::Counts>,
}

enum SuiteOutcome {
    Exited { code: Option<i32> },
    TimedOut { seconds: u64 },
}

impl SuiteRun {
    fn green(&self) -> bool {
        matches!(self.outcome, SuiteOutcome::Exited { code: Some(0) })
            && self.counts.as_ref().is_some_and(|c| c.failed == 0)
    }

    fn passed_text(&self) -> String {
        self.counts
            .as_ref()
            .map_or("an unreadable number".to_string(), |c| {
                c.passed().to_string()
            })
    }

    fn describe(&self) -> String {
        let ended = match &self.outcome {
            SuiteOutcome::Exited { code: Some(c) } => format!("exit {c}"),
            SuiteOutcome::Exited { code: None } => "killed by a signal".to_string(),
            SuiteOutcome::TimedOut { seconds } => format!("timed out after {seconds}s"),
        };
        match &self.counts {
            Some(c) => format!(
                "{ended}, {} passing, {} failing, {} skipped",
                c.passed(),
                c.failed,
                c.skipped
            ),
            None => format!("{ended}, no readable test report"),
        }
    }
}

/// Copy `source` to `<at>/<name>` and run the suite there. `at` must not exist.
fn run_suite(spec: &SuiteGrade, source: &Path, at: &Path, name: &str) -> Result<SuiteRun> {
    let dir = at.join(name);
    fs::create_dir_all(at).with_context(|| format!("creating {}", at.display()))?;
    crate::lab::cow_clone::cow_clone_dir_excluding(
        source,
        &dir,
        crate::lab::artifact_dirs::RUN_ARTIFACT_DIRS,
    )?;
    run_suite_in(spec, &dir, &at.join("report"))
}

/// Run the suite command in `dir`, its report written to `report`.
fn run_suite_in(spec: &SuiteGrade, dir: &Path, report: &Path) -> Result<SuiteRun> {
    let command = spec
        .command
        .replace("${REPORT}", &shell_quote(&report.to_string_lossy()));
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(&command).current_dir(dir);
    let seconds = spec
        .timeout_seconds
        .unwrap_or(DEFAULT_SUITE_TIMEOUT_SECONDS);
    use darkmux_crew::bounded_command::{run_bounded, Bounded};
    let outcome = match run_bounded(cmd, Duration::from_secs(seconds)) {
        Bounded::Finished { code, .. } => SuiteOutcome::Exited { code },
        Bounded::TimedOut { seconds } => SuiteOutcome::TimedOut { seconds },
        Bounded::Interrupted => bail!("interrupted while running the suite"),
        Bounded::SpawnFailed(e) => bail!("could not start the suite command `{command}`: {e}"),
    };
    let counts = match spec.report {
        TestReport::Junit => fs::read_to_string(report)
            .ok()
            .and_then(|xml| junit::parse(&xml)),
    };
    Ok(SuiteRun { outcome, counts })
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn suite_check(spec: &SuiteGrade, run: &SuiteRun, baseline: Option<u64>) -> Result<GradeCheck> {
    let mut problems = vec![];
    if !run.green() {
        problems.push(format!("not green: {}", run.describe()));
    }
    if spec.more_than_baseline {
        let Some(baseline) = baseline else {
            bail!("verify.grade.suite.more_than_baseline needs the fixture to declare baseline.test_count");
        };
        match run.counts.as_ref().map(|c| c.passed()) {
            Some(n) if n > baseline => {}
            Some(n) => problems.push(format!(
                "no tests added: {n} passing, the fixture's baseline is {baseline}"
            )),
            None => problems.push("the passing count is unreadable".to_string()),
        }
    }
    Ok(row(
        "suite",
        &spec.asks,
        problems,
        &format!("green, {}", run.describe()),
    ))
}

fn behavior_names(spec: &crate::workloads::grade_spec::BehaviorsGrade) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for m in &spec.mutants {
        if !names.contains(&m.behavior) {
            names.push(m.behavior.clone());
        }
    }
    names
}

/// Run every planted bug; return one problem per behavior a bug survived in.
fn missed_behaviors(
    input: &GradeInput,
    spec: &crate::workloads::grade_spec::BehaviorsGrade,
) -> Result<Vec<String>> {
    if spec.mutants.is_empty() {
        bail!("verify.grade.behaviors declares no mutants");
    }
    let mut survived: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, mutant) in spec.mutants.iter().enumerate() {
        let original_path = input.fixture.join(&mutant.file);
        let original = fs::read_to_string(&original_path)
            .with_context(|| format!("reading the fixture's {} for a planted bug", mutant.file))?;
        if original.matches(&mutant.find).count() != 1 {
            bail!("planted bug {} ({}) no longer applies: its text is not in the fixture's {} exactly once", i + 1, mutant.behavior, mutant.file);
        }
        let at = input.work.join(format!("mutant-{}", i + 1));
        fs::create_dir_all(&at)?;
        let dir = at.join("result");
        crate::lab::cow_clone::cow_clone_dir_excluding(
            input.result,
            &dir,
            crate::lab::artifact_dirs::RUN_ARTIFACT_DIRS,
        )?;
        let target = dir.join(&mutant.file);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, original.replacen(&mutant.find, &mutant.replace, 1))?;
        let suite = input
            .spec
            .suite
            .as_ref()
            .ok_or_else(|| anyhow!("verify.grade.behaviors needs a suite"))?;
        let run = run_suite_in(suite, &dir, &at.join("report"))?;
        // A timeout counts as caught: the suite did not pass with the bug in.
        if run.green() {
            match survived.iter_mut().find(|(b, _)| *b == mutant.behavior) {
                Some((_, ids)) => ids.push(i + 1),
                None => survived.push((mutant.behavior.clone(), vec![i + 1])),
            }
        }
        let _ = fs::remove_dir_all(&at);
    }
    Ok(survived
        .into_iter()
        .map(|(b, ids)| {
            let ids: Vec<String> = ids.iter().map(|i| format!("#{i}")).collect();
            format!(
                "tests stay green with a planted bug in {b} (bug {})",
                ids.join(", ")
            )
        })
        .collect())
}

/// The model's final reply, from the runtime envelope the run kept.
pub(crate) fn final_reply(run_dir: &Path) -> Option<String> {
    let stdout = fs::read_to_string(run_dir.join("qa-reply.json")).ok()?;
    let envelope: serde_json::Value =
        serde_json::from_str(crate::lab::scores::envelope_candidate(&stdout)).ok()?;
    envelope
        .get("final_assistant")?
        .as_str()
        .map(str::to_string)
}

fn baseline_test_count(fixture: &Path) -> Option<u64> {
    let manifest = crate::lab::fixture::FixtureManifest::load_from_dir(fixture).ok()?;
    manifest.baseline.get("test_count")?.as_u64()
}

/// Grade a result into a record, with a grader that cannot grade recorded as
/// a failed `grader` row rather than an error: the run fails closed and says why.
pub(crate) fn grade_run(
    run_dir: &Path,
    spec: &GradeSpec,
    prompt: &str,
    fixture: &Path,
    result: &Path,
    dispatch: &DispatchEnd,
) -> GradeRecord {
    let work = run_dir.join(WORK_DIR);
    let _ = fs::remove_dir_all(&work);
    let reply = final_reply(run_dir);
    let graded = fs::create_dir_all(&work)
        .map_err(anyhow::Error::from)
        .and_then(|()| {
            grade(&GradeInput {
                spec,
                prompt,
                fixture,
                result,
                dispatch,
                reply: reply.as_deref(),
                baseline_test_count: baseline_test_count(fixture),
                work: &work,
            })
        });
    let _ = fs::remove_dir_all(&work);
    graded.unwrap_or_else(|e| GradeRecord {
        passed: false,
        checks: vec![GradeCheck {
            id: "grader".to_string(),
            passed: false,
            evidence: format!("could not grade: {e:#}"),
            asks: vec![],
        }],
        tests_passed: None,
        ungraded_passed: None,
    })
}

/// The one-line verdict a graded run's `verify.details` carries.
pub(crate) fn summary(record: &GradeRecord) -> String {
    if record.passed {
        return format!("graded: pass ({} checks)", record.checks.len());
    }
    let failed: Vec<String> = record
        .checks
        .iter()
        .filter(|c| !c.passed)
        .map(|c| format!("{}: {}", c.id, c.evidence))
        .collect();
    format!("graded: fail ({})", failed.join("; "))
}

/// Write a record into the run's manifest. The verdict is the ungraded one
/// (the raw verify command and the work gate) AND the grader's. A record that
/// is written again keeps the ungraded verdict the first one saw.
pub(crate) fn record(run_dir: &Path, mut record: GradeRecord) -> Result<GradeRecord> {
    let mut manifest = RunManifest::read(run_dir)?;
    let report = match manifest.verify.as_mut() {
        Some(Some(ManifestVerify::Report(report))) => report.as_mut(),
        Some(Some(_)) => bail!("manifest `verify` is not a JSON object: cannot record a grade"),
        _ => {
            manifest.verify = Some(Some(ManifestVerify::Report(Box::new(
                ManifestVerifyReport {
                    passed: true,
                    details: String::new(),
                    work_gate: None,
                    grade: None,
                    extras: Default::default(),
                },
            ))));
            let Some(Some(ManifestVerify::Report(report))) = manifest.verify.as_mut() else {
                return Err(anyhow!("manifest `verify` vanished"));
            };
            report.as_mut()
        }
    };
    let ungraded = report
        .grade
        .as_ref()
        .and_then(|g| g.ungraded_passed)
        .unwrap_or(report.passed);
    record.ungraded_passed = Some(ungraded);
    report.passed = ungraded && record.passed;
    report.details = match (ungraded, record.passed) {
        (false, _) if report.grade.is_none() => format!("{}; {}", report.details, summary(&record)),
        (false, _) => format!("ungraded check failed; {}", summary(&record)),
        (true, _) => summary(&record),
    };
    report.grade = Some(record.clone());
    // v9: `verify.grade`.
    manifest.raise_schema_version(9);
    manifest.write(run_dir)?;
    Ok(record)
}

/// The lab-run step: grade the run's result after the work gate, record it,
/// and hand back the verdict the in-memory outcome must agree with. A run
/// with no `verify.grade` is untouched (`gate` is returned as it was).
pub(crate) fn apply(
    run_dir: &Path,
    fixture: &Path,
    result: &Path,
    dispatch: &DispatchEnd,
    loaded: &crate::workloads::types::LoadedWorkload,
    gate: Result<Option<WorkGateResult>>,
) -> Result<Option<WorkGateResult>> {
    let Some(spec) = loaded
        .manifest
        .workload
        .verify
        .as_ref()
        .and_then(|v| v.grade.as_ref())
    else {
        return gate;
    };
    // A gate that could not be applied stands: the run fails closed on it.
    let gate = gate?;
    let prompt = crate::providers::coding_task::resolve_prompt(loaded)?;
    let graded = grade_run(run_dir, spec, &prompt, fixture, result, dispatch);
    record(run_dir, graded)?;
    let manifest = RunManifest::read(run_dir)?;
    let report = manifest
        .verify_report()
        .ok_or_else(|| anyhow!("the graded verify report is missing"))?;
    Ok(Some(WorkGateResult {
        passed: report.passed,
        details: report.details.clone(),
        tests_added: gate.and_then(|g| g.tests_added),
    }))
}

/// `darkmux lab grade <run>`: grade a kept run again, without re-running it.
pub struct Regrade {
    pub run_dir: PathBuf,
    pub record: GradeRecord,
    /// Whether the record was written into the run's manifest.
    pub recorded: bool,
}

/// Grade a kept run with its workload's current `verify.grade`. The fixture is
/// the run's own (`fixture.source_path`) unless `fixture` names another
/// checkout; either way its content must hash to the run's recorded
/// `baseline_hash`, so the run is graded against what it started from.
/// Writes into the run's manifest only when `write` is set.
pub fn regrade(run: &str, fixture: Option<&Path>, write: bool) -> Result<Regrade> {
    let run_dir = crate::lab::inspect::resolve_run_dir(run);
    let manifest = RunManifest::read(&run_dir)?;
    let workload_id = manifest
        .workload
        .clone()
        .ok_or_else(|| anyhow!("{}: the manifest names no workload", run_dir.display()))?;
    let user_root = crate::lab::paths::resolve(crate::lab::paths::ResolveScope::ForceUser).root;
    let loaded = crate::workloads::load::load(&workload_id, Some(&user_root))?;
    let spec = loaded
        .manifest
        .workload
        .verify
        .as_ref()
        .and_then(|v| v.grade.as_ref())
        .ok_or_else(|| anyhow!("workload \"{workload_id}\" declares no verify.grade"))?;
    let fixture_dir = match fixture {
        Some(f) => f.to_path_buf(),
        None => manifest
            .fixture
            .as_ref()
            .and_then(|f| f.source_path.clone().flatten())
            .map(PathBuf::from)
            .ok_or_else(|| {
                anyhow!(
                    "{}: the manifest names no fixture; pass --fixture",
                    run_dir.display()
                )
            })?,
    };
    let baseline = manifest.baseline_hash().ok_or_else(|| {
        anyhow!(
            "{}: the manifest records no baseline_hash",
            run_dir.display()
        )
    })?;
    let now = crate::lab::sandbox_hash::hash_sandbox_dir(&fixture_dir)?;
    if now != baseline {
        bail!(
            "{} no longer matches what this run started from (hash {now}, the run recorded {baseline}): \
             check out the fixture at the run's commit and pass it with --fixture",
            fixture_dir.display()
        );
    }
    // The run's own sandbox, where the lab keeps it; the manifest's path
    // only for a run kept elsewhere (a run folder copied between machines
    // still carries the first machine's path).
    let kept = run_dir.join("sandbox");
    let result = if kept.is_dir() {
        kept
    } else {
        manifest.sandbox.clone().map(PathBuf::from).unwrap_or(kept)
    };
    let dispatch = match (&manifest.escalation, manifest.ok) {
        (Some(reason), _) => DispatchEnd::Escalated {
            reason: reason.clone(),
        },
        (None, Some(true)) => DispatchEnd::Completed,
        _ => DispatchEnd::Failed,
    };
    let prompt = crate::providers::coding_task::resolve_prompt(&loaded)?;
    let graded = grade_run(&run_dir, spec, &prompt, &fixture_dir, &result, &dispatch);
    let record = if write {
        self::record(&run_dir, graded)?
    } else {
        graded
    };
    Ok(Regrade {
        run_dir,
        record,
        recorded: write,
    })
}

/// The text `lab grade` prints for one run.
pub fn render(regrade: &Regrade) -> String {
    let mut out = String::new();
    p!(out, "{}", regrade.run_dir.display());
    for c in &regrade.record.checks {
        p!(
            out,
            "  {}  {:<15} {}",
            if c.passed { "PASS" } else { "FAIL" },
            c.id,
            c.evidence
        );
    }
    p!(
        out,
        "  verdict: {}{}",
        if regrade.record.passed {
            "pass"
        } else {
            "fail"
        },
        if regrade.recorded {
            " (recorded in the run's manifest)"
        } else {
            ""
        }
    );
    out
}
