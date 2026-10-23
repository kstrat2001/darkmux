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
//! SECURITY (#906): the suite runs the code the model wrote, so, like the
//! verify command (`coding_task::run_verify_command`), it runs where
//! [`Runner`] says: by default in a container with only the copy mounted, no
//! network and no capabilities (`darkmux_crew::contained_command`); on the
//! host as the operator only with `runtime.tests_on_host`. Either way it is
//! bounded by a timeout and killed as a process group.

mod files;
mod junit;
#[cfg(test)]
mod tests;

use crate::lab::dispatch_end::DispatchEnd;
use darkmux_crew::bounded_command::Bounded;
use darkmux_crew::contained_command;
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

/// Where the grader runs the suite: where the run's verify command runs.
pub(crate) use crate::providers::coding_task::TestRunner as Runner;

/// Everything one grading reads. Nothing here is written to.
pub(crate) struct GradeInput<'a> {
    pub spec: &'a GradeSpec,
    /// The prompt as the model was given it (placeholders expanded).
    pub prompt: &'a str,
    /// The pristine fixture the run started from.
    pub fixture: &'a Path,
    /// The run's result: its sandbox after the dispatch.
    pub result: &'a Path,
    pub dispatch: &'a DispatchEnd,
    /// The model's final reply, when there was one.
    pub reply: Option<&'a str>,
    /// How the runtime says the dispatch ended (its envelope's `result`).
    pub result_kind: Option<&'a str>,
    /// The fixture's `baseline.test_count`, when it declares one.
    pub baseline_test_count: Option<u64>,
    /// An empty scratch directory the grader may fill.
    pub work: &'a Path,
    /// Where the suite runs.
    pub runner: &'a Runner,
}

/// Where each suite run's copy lives: `<work>/<opaque>/result`, the opaque
/// name drawn per grading, so a test cannot tell from its path whether a
/// planted bug is in (it could otherwise fail on purpose there).
struct Copies<'a> {
    work: &'a Path,
    seed: std::collections::hash_map::RandomState,
    next: std::cell::Cell<u64>,
}

impl<'a> Copies<'a> {
    fn new(work: &'a Path) -> Self {
        Self {
            work,
            seed: std::collections::hash_map::RandomState::new(),
            next: std::cell::Cell::new(0),
        }
    }

    /// A fresh, empty directory to put one copy in.
    fn fresh(&self) -> Result<PathBuf> {
        use std::hash::BuildHasher;
        let n = self.next.get();
        self.next.set(n + 1);
        let at = self.work.join(format!("{:016x}", self.seed.hash_one(n)));
        fs::create_dir_all(&at).with_context(|| format!("creating {}", at.display()))?;
        Ok(at)
    }
}

/// Grade a result. `Err` means the grader could not grade at all (a quoted
/// sentence the prompt does not hold, a planted bug that no longer applies,
/// a suite that could not be started): the caller fails the run closed.
pub(crate) fn grade(input: &GradeInput) -> Result<GradeRecord> {
    check_asks(input.spec, input.prompt)?;
    let changes = files::compare(input.fixture, input.result)?;
    let copies = Copies::new(input.work);
    let mut checks = vec![run_finished(input.dispatch, input.result_kind)];
    checks.extend(scope_row(input, &changes));
    let (suite_checks, run) = suite_rows(input, &copies)?;
    checks.extend(suite_checks);
    checks.extend(style_row(input, &changes)?);
    let green = run.as_ref().is_some_and(|run| run.green());
    checks.extend(behaviors_row(input, green, &copies)?);
    checks.extend(reply_row(input));
    Ok(GradeRecord {
        passed: checks.iter().all(|c| c.passed),
        checks,
        tests_passed: run.and_then(|run| run.counts.as_ref().map(|c| c.passed())),
        ..Default::default()
    })
}

/// The suite's row and, when declared, the portable row, with the suite's
/// run on the result; nothing when the workload declares no suite.
fn suite_rows(input: &GradeInput, copies: &Copies) -> Result<(Vec<GradeCheck>, Option<SuiteRun>)> {
    let Some(spec) = &input.spec.suite else {
        return Ok((vec![], None));
    };
    let (row, run, spec) = suite_row(input, spec, copies)?;
    let mut rows = vec![row];
    rows.extend(portable_row(input, spec, &run, copies)?);
    Ok((rows, Some(run)))
}

fn scope_row(input: &GradeInput, changes: &files::Changes) -> Option<GradeCheck> {
    let scope = input.spec.scope.as_ref()?;
    Some(row(
        "scope",
        &scope.asks,
        files::scope_violations(changes, scope),
        "only allowed files changed",
    ))
}

/// The suite on the result, judged against the fixture's own run of it,
/// which names the tests that must still pass.
fn suite_row<'s>(
    input: &GradeInput,
    spec: &'s SuiteGrade,
    copies: &Copies,
) -> Result<(GradeCheck, SuiteRun, &'s SuiteGrade)> {
    let before = run_suite(input.runner, spec, input.fixture, &copies.fresh()?, "")?;
    if before.counts.is_none() {
        bail!("the fixture's own suite run left no readable report: cannot tell which tests must still pass");
    }
    let run = run_suite(input.runner, spec, input.result, &copies.fresh()?, "")?;
    let check = suite_check(spec, &run, &before, input.baseline_test_count)?;
    Ok((check, run, spec))
}

fn portable_row(
    input: &GradeInput,
    spec: &SuiteGrade,
    suite: &SuiteRun,
    copies: &Copies,
) -> Result<Option<GradeCheck>> {
    let Some(portable) = &input.spec.portable else {
        return Ok(None);
    };
    let elsewhere = run_suite(input.runner, spec, input.result, &copies.fresh()?, "elsewhere")?;
    let same =
        elsewhere.counts.as_ref().map(|c| c.passed()) == suite.counts.as_ref().map(|c| c.passed());
    let problems = match (elsewhere.green(), same) {
        (true, true) => vec![],
        (true, false) => vec![format!(
            "green, but {} passing there against {} here",
            elsewhere.passed_text(),
            suite.passed_text()
        )],
        (false, _) => vec![format!("red from another path: {}", elsewhere.describe())],
    };
    Ok(Some(row(
        "portable",
        &portable.asks,
        problems,
        "green from a second copy at another path",
    )))
}

fn style_row(input: &GradeInput, changes: &files::Changes) -> Result<Option<GradeCheck>> {
    let Some(style) = &input.spec.style else {
        return Ok(None);
    };
    Ok(Some(row(
        "style",
        &style.asks,
        files::style_violations(changes, style)?,
        "every new file matches",
    )))
}

fn behaviors_row(
    input: &GradeInput,
    suite_green: bool,
    copies: &Copies,
) -> Result<Option<GradeCheck>> {
    let Some(behaviors) = &input.spec.behaviors else {
        return Ok(None);
    };
    let problems = if suite_green {
        missed_behaviors(input, behaviors, copies)?
    } else {
        vec!["not graded: the suite is red without any planted bug".to_string()]
    };
    let caught = format!(
        "every planted bug turns the suite red ({})",
        behavior_names(behaviors).join(", ")
    );
    Ok(Some(row("behaviors", &behaviors.asks, problems, &caught)))
}

fn reply_row(input: &GradeInput) -> Option<GradeCheck> {
    let reply = input.spec.reply.as_ref().filter(|r| r.required)?;
    let present = input.reply.is_some_and(|r| !r.trim().is_empty());
    let problems = if present {
        vec![]
    } else {
        vec!["no final reply".to_string()]
    };
    Some(row(
        "reply_required",
        &reply.asks,
        problems,
        "a final reply is present (its content is not graded)",
    ))
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
        if asks.is_empty() {
            bail!("verify.grade.{check} quotes no prompt sentence: every graded check names what the prompt asks");
        }
        if asks.iter().any(|a| a.trim().is_empty()) {
            bail!("verify.grade.{check} quotes an empty sentence: every quote is a sentence the prompt holds");
        }
        if let Some(missing) = asks.iter().find(|a| !prompt.contains(a.as_str())) {
            bail!("verify.grade.{check} quotes \"{missing}\", which is not in the prompt word for word");
        }
    }
    Ok(())
}

/// Finished means the model stopped on its own: the dispatch completed AND
/// the runtime's envelope says `stop`. A turn limit or a missing envelope is
/// not a finish, though the dispatch may exit cleanly on either.
fn run_finished(dispatch: &DispatchEnd, result_kind: Option<&str>) -> GradeCheck {
    let problems = match (dispatch, result_kind) {
        (DispatchEnd::Escalated { reason }, _) => vec![format!(
            "the dispatch escalated ({reason}) before finishing"
        )],
        (DispatchEnd::Failed, _) => vec![
            "the dispatch did not complete (an error, a crash or a limit cut it off)".to_string(),
        ],
        (DispatchEnd::Completed, Some("stop")) => vec![],
        (DispatchEnd::Completed, Some(other)) => vec![format!(
            "the dispatch ended with `{other}`, not a stop of the model's own"
        )],
        (DispatchEnd::Completed, None) => {
            vec!["no runtime envelope says how the dispatch ended".to_string()]
        }
    };
    row(
        "run_finished",
        &[],
        problems,
        "the model stopped on its own",
    )
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
        ..Default::default()
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

/// Copy `source` to `<root>/<rel>/result` and run the suite there. `rel`
/// moves the copy to another path, inside a container too.
fn run_suite(runner: &Runner, spec: &SuiteGrade, source: &Path, root: &Path, rel: &str) -> Result<SuiteRun> {
    copy_into(source, &root.join(rel))?;
    run_suite_in(runner, spec, root, &Path::new(rel).join("result"))
}

/// Copy `source` to `<at>/result`, creating `at`.
fn copy_into(source: &Path, at: &Path) -> Result<PathBuf> {
    fs::create_dir_all(at).with_context(|| format!("creating {}", at.display()))?;
    let dir = at.join("result");
    crate::lab::cow_clone::cow_clone_dir_excluding(
        source,
        &dir,
        crate::lab::artifact_dirs::RUN_ARTIFACT_DIRS,
    )?;
    files::prune_artifacts(&dir, "")?;
    Ok(dir)
}

/// Run the suite command in `<root>/<dir>`, its report written to
/// `<root>/report/report.xml`. Only `root` is mounted when it runs in a
/// container.
fn run_suite_in(runner: &Runner, spec: &SuiteGrade, root: &Path, dir: &Path) -> Result<SuiteRun> {
    let reports = root.join("report");
    fs::create_dir_all(&reports).with_context(|| format!("creating {}", reports.display()))?;
    let report = format!("{}/report/report.xml", runner.view(root));
    let command = spec.command.replace("${REPORT}", &shell_quote(&report));
    let seconds = spec.timeout_seconds.unwrap_or(DEFAULT_SUITE_TIMEOUT_SECONDS);
    let run = contained_command::Run {
        command: &command,
        tree: root,
        workdir: &dir.to_string_lossy(),
        mounts: &[],
        user: runner.user.clone(),
        name_prefix: "darkmux-grade",
    };
    let ran = contained_command::run(&runner.placement, &run, Duration::from_secs(seconds))
        .map_err(|why| anyhow!("the suite did not run in its container: {why}"))?;
    let outcome = match ran {
        Bounded::Finished { code, .. } => SuiteOutcome::Exited { code },
        Bounded::TimedOut { seconds } => SuiteOutcome::TimedOut { seconds },
        Bounded::Interrupted => return Err(Interrupted.into()),
        Bounded::SpawnFailed(e) => bail!("could not start the suite command `{command}`: {e}"),
    };
    let counts = match spec.report {
        TestReport::Junit => fs::read_to_string(reports.join("report.xml"))
            .ok()
            .and_then(|xml| junit::parse(&xml)),
    };
    Ok(SuiteRun { outcome, counts })
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn suite_check(
    spec: &SuiteGrade,
    run: &SuiteRun,
    before: &SuiteRun,
    baseline: Option<u64>,
) -> Result<GradeCheck> {
    let mut problems = vec![];
    if !run.green() {
        problems.push(format!("not green: {}", run.describe()));
    }
    let gone = no_longer_passing(before, run);
    if !gone.is_empty() {
        let shown: Vec<String> = gone.iter().take(5).map(|n| format!("\"{n}\"")).collect();
        let more = if gone.len() > 5 {
            format!(" and {} more", gone.len() - 5)
        } else {
            String::new()
        };
        problems.push(format!(
            "{} of the fixture's own passing tests no longer pass or no longer run: {}{more}",
            gone.len(),
            shown.join(", ")
        ));
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

/// The fixture's passing tests (by name, counted) that the result's run does
/// not pass as many times. A test switched off inside its file (an early
/// `return`, a comment) passes scope's line check and lands here.
fn no_longer_passing(before: &SuiteRun, after: &SuiteRun) -> Vec<String> {
    fn count(run: &SuiteRun) -> std::collections::BTreeMap<&str, usize> {
        let mut m = std::collections::BTreeMap::new();
        for n in run.counts.iter().flat_map(|c| c.passing.iter()) {
            *m.entry(n.as_str()).or_default() += 1;
        }
        m
    }
    let (was, now) = (count(before), count(after));
    was.iter()
        .filter(|(name, n)| now.get(*name).copied().unwrap_or(0) < **n)
        .map(|(name, _)| name.to_string())
        .collect()
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
///
/// First, a control: each file a bug is planted in gets an edit that changes
/// no behavior (a trailing blank line), written the same way. The suite must
/// stay green through it. Tests that react to it check the source text (a
/// hash, a size, a modified time), not what the code does, and would "catch"
/// every planted bug for that reason alone.
fn missed_behaviors(
    input: &GradeInput,
    spec: &crate::workloads::grade_spec::BehaviorsGrade,
    copies: &Copies,
) -> Result<Vec<String>> {
    let suite = input
        .spec
        .suite
        .as_ref()
        .ok_or_else(|| anyhow!("verify.grade.behaviors needs a suite"))?;
    let originals = planted_files(input.fixture, spec)?;
    if let Some(problem) = control_problem(input, spec, suite, copies, &originals)? {
        return Ok(vec![problem]);
    }
    Ok(surviving_bugs(input, spec, suite, copies, &originals)?
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

/// Each behavior with a planted bug the suite stayed green through, and the
/// numbers of those bugs.
fn surviving_bugs(
    input: &GradeInput,
    spec: &crate::workloads::grade_spec::BehaviorsGrade,
    suite: &SuiteGrade,
    copies: &Copies,
    originals: &std::collections::BTreeMap<String, String>,
) -> Result<Vec<(String, Vec<usize>)>> {
    let mut survived: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, mutant) in spec.mutants.iter().enumerate() {
        let original = &originals[&mutant.file];
        let planted = original.replacen(&mutant.find, &mutant.replace, 1);
        // A timeout counts as caught: the suite did not pass with the bug in
        // (and the control above shows the tests do not tell an edit apart).
        if run_planted(input, suite, copies, &mutant.file, &planted)?.green() {
            match survived.iter_mut().find(|(b, _)| *b == mutant.behavior) {
                Some((_, ids)) => ids.push(i + 1),
                None => survived.push((mutant.behavior.clone(), vec![i + 1])),
            }
        }
    }
    Ok(survived)
}

/// The control: each planted file with a blank line added, which changes no
/// behavior. A suite that fails on it tests the source text.
fn control_problem(
    input: &GradeInput,
    spec: &crate::workloads::grade_spec::BehaviorsGrade,
    suite: &SuiteGrade,
    copies: &Copies,
    originals: &std::collections::BTreeMap<String, String>,
) -> Result<Option<String>> {
    for (file, original) in originals {
        let run = run_planted(input, suite, copies, file, &format!("{original}\n"))?;
        if !run.green() {
            return Ok(Some(format!(
                "the tests fail on an edit that changes no behavior (a blank line added to the end of {file}): \
                 they check the source text, not its behavior ({})",
                run.describe()
            )));
        }
    }
    for (i, mutant) in spec.mutants.iter().enumerate() {
        let Some(respaced) = respace(&mutant.find) else {
            continue;
        };
        let edited = originals[&mutant.file].replacen(&mutant.find, &respaced, 1);
        let run = run_planted(input, suite, copies, &mutant.file, &edited)?;
        if !run.green() {
            return Ok(Some(format!(
                "the tests fail when bug {}'s code in {} gains a space, which changes no behavior: \
                 they check the source text, not its behavior ({})",
                i + 1,
                mutant.file,
                run.describe()
            )));
        }
    }
    Ok(None)
}

/// The span a planted bug replaces, with one space doubled: the first space
/// after a non-space character, so leading indentation is never touched. A
/// test that asserts the span's text goes red on it; code does not.
fn respace(find: &str) -> Option<String> {
    let at = find
        .char_indices()
        .zip(find.chars().skip(1))
        .find(|((_, c), next)| !c.is_whitespace() && *next == ' ')
        .map(|((i, c), _)| i + c.len_utf8())?;
    Some(format!("{} {}", &find[..at], &find[at..]))
}

/// The fixture's text of every file a bug is planted in, each bug's text
/// checked to be there exactly once.
fn planted_files(
    fixture: &Path,
    spec: &crate::workloads::grade_spec::BehaviorsGrade,
) -> Result<std::collections::BTreeMap<String, String>> {
    if spec.mutants.is_empty() {
        bail!("verify.grade.behaviors declares no mutants");
    }
    let mut originals = std::collections::BTreeMap::new();
    for (i, mutant) in spec.mutants.iter().enumerate() {
        let original = fs::read_to_string(fixture.join(&mutant.file))
            .with_context(|| format!("reading the fixture's {} for a planted bug", mutant.file))?;
        if original.matches(&mutant.find).count() != 1 {
            bail!(
                "planted bug {} ({}) no longer applies: its text is not in the fixture's {} exactly once",
                i + 1,
                mutant.behavior,
                mutant.file
            );
        }
        originals.insert(mutant.file.clone(), original);
    }
    Ok(originals)
}

/// Copy the result, put `content` at `file` (the fixture's file, edited), and
/// run the suite. Never writes through a symlink: a link in the copy could
/// lead out of it, into the run's own sandbox or anywhere else.
fn run_planted(
    input: &GradeInput,
    suite: &SuiteGrade,
    copies: &Copies,
    file: &str,
    content: &str,
) -> Result<SuiteRun> {
    let at = copies.fresh()?;
    let dir = copy_into(input.result, &at)?;
    let target = dir.join(file);
    let mut walked = dir.clone();
    for part in Path::new(file).components() {
        let std::path::Component::Normal(name) = part else {
            bail!("planted bug file {file} is not a plain relative path");
        };
        walked.push(name);
        if fs::symlink_metadata(&walked).is_ok_and(|m| m.file_type().is_symlink()) {
            let rel = walked
                .strip_prefix(&dir)
                .unwrap_or(&walked)
                .display()
                .to_string();
            bail!("the result reaches {file} through a symlink ({rel}); the grader writes no planted bug through a link");
        }
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&target, content)?;
    let run = run_suite_in(input.runner, suite, &at, Path::new("result"))?;
    let _ = fs::remove_dir_all(&at);
    Ok(run)
}

/// The model's final reply, from the runtime envelope the run kept.
pub(crate) fn final_reply(run_dir: &Path) -> Option<String> {
    // The runtime writes `<empty>` when the model gave no final answer.
    envelope(run_dir)?
        .get("final_assistant")?
        .as_str()
        .filter(|r| *r != "<empty>")
        .map(str::to_string)
}

/// How the runtime says the dispatch ended: its envelope's `result`.
pub(crate) fn result_kind(run_dir: &Path) -> Option<String> {
    envelope(run_dir)?
        .get("result")?
        .as_str()
        .map(str::to_string)
}

fn envelope(run_dir: &Path) -> Option<serde_json::Value> {
    let stdout = fs::read_to_string(run_dir.join("qa-reply.json")).ok()?;
    serde_json::from_str(crate::lab::scores::envelope_candidate(&stdout)).ok()
}

fn baseline_test_count(fixture: &Path) -> Option<u64> {
    let manifest = crate::lab::fixture::FixtureManifest::load_from_dir(fixture).ok()?;
    manifest.baseline.get("test_count")?.as_u64()
}

/// A signal stopped the grader. Never recorded as the run's verdict: the run
/// was not graded, it was stopped.
#[derive(Debug)]
pub(crate) struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("interrupted while running the suite")
    }
}

impl std::error::Error for Interrupted {}

/// Grade a result into a record, in a scratch dir under the run that is
/// removed afterwards. `Err` when the grader could not grade at all.
pub(crate) fn try_grade_run(
    run_dir: &Path,
    spec: &GradeSpec,
    prompt: &str,
    fixture: &Path,
    result: &Path,
    dispatch: &DispatchEnd,
    runner: &Runner,
) -> Result<GradeRecord> {
    let work = run_dir.join(WORK_DIR);
    let _ = fs::remove_dir_all(&work);
    let reply = final_reply(run_dir);
    let result_kind = result_kind(run_dir);
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
                result_kind: result_kind.as_deref(),
                baseline_test_count: baseline_test_count(fixture),
                work: &work,
                runner,
            })
        });
    let _ = fs::remove_dir_all(&work);
    graded
}

/// The lab run's grading: a grader that cannot grade is recorded as a failed
/// `grader` row rather than an error, so the run fails closed and says why.
/// A signal is the one error passed on: a stopped run is not a failed one.
pub(crate) fn grade_run(
    run_dir: &Path,
    spec: &GradeSpec,
    prompt: &str,
    fixture: &Path,
    result: &Path,
    dispatch: &DispatchEnd,
    runner: &Runner,
) -> Result<GradeRecord> {
    match try_grade_run(run_dir, spec, prompt, fixture, result, dispatch, runner) {
        Err(e) if e.downcast_ref::<Interrupted>().is_some() => Err(e),
        graded => Ok(graded.unwrap_or_else(|e| cannot_grade(&e))),
    }
}

/// A record for a run the grader could not grade: one failed `grader` row.
fn cannot_grade(e: &anyhow::Error) -> GradeRecord {
    GradeRecord {
        passed: false,
        checks: vec![GradeCheck {
            id: "grader".to_string(),
            passed: false,
            evidence: format!("could not grade: {e:#}"),
            ..Default::default()
        }],
        ..Default::default()
    }
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

/// The verdict before grading: the raw verify command and the work gate.
/// Taken before the grader runs any of the result's code, so nothing that
/// code writes to disk can change it.
#[derive(Debug, Clone)]
pub struct Ungraded {
    pub passed: bool,
    pub details: String,
}

impl Ungraded {
    /// What a kept run's manifest says, read before regrading: the verdict a
    /// first grading recorded, else the report as the run left it.
    fn from_manifest(manifest: &RunManifest) -> Self {
        match manifest.verify_report() {
            Some(report) => match report.grade.as_ref() {
                Some(GradeRecord {
                    ungraded_passed: Some(passed),
                    ungraded_details,
                    ..
                }) => Self {
                    passed: *passed,
                    details: ungraded_details.clone().unwrap_or_default(),
                },
                _ => Self {
                    passed: report.passed,
                    details: report.details.clone(),
                },
            },
            None => Self {
                passed: true,
                details: String::new(),
            },
        }
    }
}

/// Write a record into the run's manifest. The verdict is the ungraded one
/// AND the grader's.
pub(crate) fn record(
    run_dir: &Path,
    mut record: GradeRecord,
    ungraded: &Ungraded,
) -> Result<GradeRecord> {
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
    record.ungraded_passed = Some(ungraded.passed);
    record.ungraded_details = (!ungraded.passed).then(|| ungraded.details.clone());
    report.passed = ungraded.passed && record.passed;
    report.details = if ungraded.passed {
        summary(&record)
    } else if ungraded.details.is_empty() {
        format!("ungraded check failed; {}", summary(&record))
    } else {
        format!("{}; {}", ungraded.details, summary(&record))
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
/// `raw` is the verify command's own verdict, for a run the gate skipped.
/// Every graded run is recorded, a gate or prompt that failed included: the
/// verdict is then a fail that says why.
pub(crate) fn apply(
    run_dir: &Path,
    fixture: &Path,
    result: &Path,
    dispatch: &DispatchEnd,
    raw: Option<&crate::workloads::types::VerifyOutcome>,
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
    let coverage = loaded
        .manifest
        .workload
        .verify
        .as_ref()
        .and_then(|v| v.coverage_min_pct);
    let (ungraded, tests_added) = ungraded_verdict(spec, coverage, raw, gate);
    let graded = match (crate::providers::coding_task::dispatched_prompt(loaded), Runner::for_workload(loaded)) {
        (Ok(prompt), Ok(runner)) => grade_run(run_dir, spec, &prompt, fixture, result, dispatch, &runner)?,
        (Err(e), _) => cannot_grade(&anyhow!("resolving the prompt: {e:#}")),
        (_, Err(e)) => cannot_grade(&e),
    };
    let tests_added = tests_added.or_else(|| {
        let baseline = baseline_test_count(fixture)?;
        Some(graded.tests_passed? as i64 - baseline as i64)
    });
    let record = record(run_dir, graded, &ungraded)?;
    let passed = ungraded.passed && record.passed;
    let details = RunManifest::read(run_dir)
        .ok()
        .and_then(|m| m.verify_report().map(|r| r.details.clone()))
        .unwrap_or_else(|| summary(&record));
    Ok(Some(WorkGateResult {
        passed,
        details,
        tests_added,
    }))
}

/// The verdict before grading, and the tests the gate counted as added.
///
/// A grader `suite` reads its runner's report, so it supersedes the work
/// gate, whose test count reads `node --test` output only: the ungraded
/// verdict is then the verify command's own (every other gate rule has a
/// grader check: run_finished, scope, suite). The gate still runs and
/// records its evidence. A declared `coverage_min_pct` has no grader check,
/// so with one the gate's verdict stands.
fn ungraded_verdict(
    spec: &GradeSpec,
    coverage_min_pct: Option<f32>,
    raw: Option<&crate::workloads::types::VerifyOutcome>,
    gate: Result<Option<WorkGateResult>>,
) -> (Ungraded, Option<i64>) {
    let from_raw = || {
        raw.map_or(
            Ungraded {
                passed: true,
                details: String::new(),
            },
            |v| Ungraded {
                passed: v.passed,
                details: v.details.clone(),
            },
        )
    };
    if spec.suite.is_some() && coverage_min_pct.is_none() {
        let added = gate.ok().flatten().and_then(|g| g.tests_added);
        return (from_raw(), added);
    }
    match gate {
        Ok(Some(g)) => (
            Ungraded {
                passed: g.passed,
                details: g.details,
            },
            g.tests_added,
        ),
        Ok(None) => (from_raw(), None),
        Err(e) => (
            Ungraded {
                passed: false,
                details: format!("verify gate could not be applied: {e:#}"),
            },
            None,
        ),
    }
}

/// `darkmux lab grade <run>`: grade a kept run again, without re-running it.
pub struct Regrade {
    pub run_dir: PathBuf,
    pub record: GradeRecord,
    /// The verdict before grading, which the run's verdict ANDs with.
    pub ungraded: Ungraded,
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
    let ungraded = Ungraded::from_manifest(&manifest);
    let graded = grade_kept(&run_dir, &manifest, fixture)?;
    let record = if write {
        self::record(&run_dir, graded, &ungraded)?
    } else {
        graded
    };
    Ok(Regrade {
        run_dir,
        record,
        ungraded,
        recorded: write,
    })
}

/// A kept run's result graded by its workload's current grader, against the
/// fixture it started from. Any error, a signal included, ends here: a kept
/// run's manifest is written only with a verdict the grader reached.
fn grade_kept(run_dir: &Path, manifest: &RunManifest, fixture: Option<&Path>) -> Result<GradeRecord> {
    let loaded = graded_workload(run_dir, manifest)?;
    let spec = declared_grade(&loaded)?;
    let fixture_dir = run_fixture(run_dir, manifest, fixture)?;
    let result = kept_result(run_dir, manifest)?;
    let (prompt, runner) = prompt_and_runner(&loaded)?;
    try_grade_run(run_dir, spec, &prompt, &fixture_dir, &result, &kept_dispatch(manifest), &runner)
}

/// `darkmux lab grade <run>...`: print each run's checks; 1 when any run
/// fails or cannot be graded, else 0.
pub fn grade_runs(runs: &[String], fixture: Option<&Path>, write: bool) -> i32 {
    let mut code = 0;
    for run in runs {
        match regrade(run, fixture, write) {
            Ok(graded) => {
                print!("{}", render(&graded));
                if !graded.passed() {
                    code = 1;
                }
            }
            Err(e) => {
                eprintln!("{run}: cannot grade: {e:#}");
                code = 1;
            }
        }
    }
    code
}

/// The prompt the model was given, and where the suite runs.
fn prompt_and_runner(loaded: &crate::workloads::types::LoadedWorkload) -> Result<(String, Runner)> {
    Ok((crate::providers::coding_task::dispatched_prompt(loaded)?, Runner::for_workload(loaded)?))
}

/// The workload's `verify.grade`, which a regrade needs.
fn declared_grade(loaded: &crate::workloads::types::LoadedWorkload) -> Result<&GradeSpec> {
    loaded
        .manifest
        .workload
        .verify
        .as_ref()
        .and_then(|v| v.grade.as_ref())
        .ok_or_else(|| anyhow!("workload \"{}\" declares no verify.grade", loaded.manifest.workload.id))
}

/// The run's own sandbox, where the lab keeps it; the manifest's path only
/// for a run kept elsewhere (a run folder copied between machines still
/// carries the first machine's path).
/// It must still hash to the run's recorded `final_hash`, when there is one:
/// a sandbox changed after the run is not what the run produced.
fn kept_result(run_dir: &Path, manifest: &RunManifest) -> Result<PathBuf> {
    let kept = run_dir.join("sandbox");
    let result = if kept.is_dir() {
        kept
    } else {
        manifest.sandbox.clone().map(PathBuf::from).unwrap_or(kept)
    };
    if !result.is_dir() {
        bail!("the run's sandbox is gone ({})", result.display());
    }
    if let Some(recorded) = manifest.final_hash_value() {
        let now = crate::lab::sandbox_hash::hash_sandbox_dir(&result)?;
        if now != recorded {
            bail!(
                "{} changed after the run (hash {now}, the run recorded {recorded}): it is not what the run produced",
                result.display()
            );
        }
    }
    Ok(result)
}

/// How a kept run's dispatch ended, from its manifest.
fn kept_dispatch(manifest: &RunManifest) -> DispatchEnd {
    match (&manifest.escalation, manifest.ok) {
        (Some(reason), _) => DispatchEnd::Escalated {
            reason: reason.clone(),
        },
        (None, Some(true)) => DispatchEnd::Completed,
        _ => DispatchEnd::Failed,
    }
}

/// The workload a kept run names, loaded after the same unknown-key refusal a
/// lab run gives (contract 7): a typo in `verify.grade` must not grade with a
/// check left out.
fn graded_workload(
    run_dir: &Path,
    manifest: &RunManifest,
) -> Result<crate::workloads::types::LoadedWorkload> {
    let workload_id = manifest
        .workload
        .clone()
        .ok_or_else(|| anyhow!("{}: the manifest names no workload", run_dir.display()))?;
    let user_root = crate::lab::paths::resolve(crate::lab::paths::ResolveScope::ForceUser).root;
    let loaded = crate::workloads::load::load(&workload_id, Some(&user_root));
    let binds = loaded
        .as_ref()
        .ok()
        .and_then(|w| w.manifest.workload.requires_fixture.clone());
    crate::user_files::preflight_with(
        darkmux_types::config_enum::Scope::LabRun,
        None,
        binds.as_deref(),
    )?;
    loaded
}

/// The fixture a kept run is graded against: its own (`fixture.source_path`)
/// unless `fixture` names another checkout, and either way hashing to the
/// run's recorded `baseline_hash`.
fn run_fixture(run_dir: &Path, manifest: &RunManifest, fixture: Option<&Path>) -> Result<PathBuf> {
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
    Ok(fixture_dir)
}

/// The text `lab grade` prints for one run.
impl Regrade {
    /// The run's verdict: the ungraded one AND the grader's.
    pub fn passed(&self) -> bool {
        self.ungraded.passed && self.record.passed
    }
}

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
    if !regrade.ungraded.passed {
        let why = match regrade.ungraded.details.as_str() {
            "" => "the verify command or work gate failed",
            d => d,
        };
        p!(out, "  FAIL  {:<15} {why}", "ungraded");
    }
    p!(
        out,
        "  verdict: {}{}",
        if regrade.passed() { "pass" } else { "fail" },
        if regrade.recorded {
            " (recorded in the run's manifest)"
        } else {
            ""
        }
    );
    out
}
