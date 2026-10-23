//! The grader, on a tiny fixture whose "suite" is a shell script that writes
//! JUnit: each `test/*.t` file names a line `src/a.txt` must hold. No test
//! runner beyond `sh` is needed, which is also the point: the grader knows
//! no runner, only the report.

use super::files::{glob_match, lines_kept, scope_violations, Changes};
use super::*;
use crate::lab::manifest::{GradeRecord, RunManifest};
use crate::workloads::grade_spec::*;
use std::fs;
use tempfile::TempDir;

const PROMPT: &str = "Write tests. Do not modify src/. Tests must pass. Say what you did.";

const SUITE_SH: &str = r#"report="$1"
pass=0; fail=0; cases=""
for t in test/*.t; do
  if grep -qx "$(cat "$t")" src/a.txt; then cases="$cases<testcase name=\"$t\"/>"
  else cases="$cases<testcase name=\"$t\"><failure message=\"x\"/></testcase>"; fail=1; fi
done
printf '<?xml version="1.0"?><testsuites><testsuite name="s">%s</testsuite></testsuites>' "$cases" > "$report"
exit $fail
"#;

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

/// A fixture with one source line and one (weak) test that any value passes.
fn fixture(root: &Path) -> PathBuf {
    let f = root.join("fixture");
    write(&f, "src/a.txt", "value=1\nother=2\n");
    write(&f, "test/a.t", "other=2");
    write(&f, "run.sh", SUITE_SH);
    write(&f, "notes.txt", "keep\n");
    f
}

fn result_from(root: &Path, fixture: &Path) -> PathBuf {
    let r = root.join("result");
    crate::lab::cow_clone::cow_clone_dir(fixture, &r).unwrap();
    r
}

fn spec() -> GradeSpec {
    GradeSpec {
        suite: Some(SuiteGrade {
            asks: vec!["Tests must pass.".into()],
            command: "sh run.sh ${REPORT}".into(),
            report: TestReport::Junit,
            more_than_baseline: true,
            timeout_seconds: Some(30),
        }),
        scope: Some(ScopeGrade {
            asks: vec!["Do not modify src/.".into()],
            may_add: vec!["test/**".into()],
            append_only: vec![],
            comment_prefixes: vec![],
        }),
        portable: Some(AsksOnly {
            asks: vec!["Write tests.".into()],
        }),
        style: None,
        behaviors: Some(BehaviorsGrade {
            asks: vec!["Write tests.".into()],
            mutants: vec![Mutant {
                behavior: "value".into(),
                file: "src/a.txt".into(),
                find: "value=1".into(),
                replace: "value=9".into(),
            }],
        }),
        reply: Some(ReplyGrade {
            asks: vec!["Say what you did.".into()],
            required: true,
        }),
    }
}

fn run(
    spec: &GradeSpec,
    fixture: &Path,
    result: &Path,
    dispatch: DispatchEnd,
    reply: Option<&str>,
    root: &Path,
) -> Result<GradeRecord> {
    let work = root.join("work");
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work).unwrap();
    grade(&GradeInput {
        spec,
        prompt: PROMPT,
        fixture,
        result,
        dispatch: &dispatch,
        reply,
        baseline_test_count: Some(1),
        work: &work,
    })
}

fn failed(record: &GradeRecord) -> Vec<&str> {
    record
        .checks
        .iter()
        .filter(|c| !c.passed)
        .map(|c| c.id.as_str())
        .collect()
}

#[test]
fn a_result_that_tests_the_behavior_passes_every_check() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let r = result_from(tmp.path(), &f);
    write(&r, "test/b.t", "value=1");
    let record = run(
        &spec(),
        &f,
        &r,
        DispatchEnd::Completed,
        Some("done"),
        tmp.path(),
    )
    .unwrap();
    assert!(record.passed, "{record:?}");
    assert_eq!(record.tests_passed, Some(2));
    let ids: Vec<&str> = record.checks.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "run_finished",
            "scope",
            "suite",
            "portable",
            "behaviors",
            "reply_required"
        ]
    );
    // The model's side is never touched.
    assert_eq!(
        fs::read_to_string(r.join("src/a.txt")).unwrap(),
        "value=1\nother=2\n"
    );
}

/// The old gate's blind spot: a test that adds to the count but would pass
/// with the behavior broken.
#[test]
fn a_test_that_misses_the_planted_bug_fails_behaviors_only() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let r = result_from(tmp.path(), &f);
    write(&r, "test/b.t", "other=2");
    let record = run(
        &spec(),
        &f,
        &r,
        DispatchEnd::Completed,
        Some("done"),
        tmp.path(),
    )
    .unwrap();
    assert_eq!(failed(&record), ["behaviors"]);
    assert!(
        record
            .checks
            .iter()
            .any(|c| c.evidence.contains("planted bug in value (bug #1)")),
        "{record:?}"
    );
}

#[test]
fn no_added_test_an_edited_source_and_a_deleted_file_each_fail() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let r = result_from(tmp.path(), &f);
    let record = run(
        &spec(),
        &f,
        &r,
        DispatchEnd::Completed,
        Some("done"),
        tmp.path(),
    )
    .unwrap();
    assert_eq!(failed(&record), ["suite", "behaviors"]);
    assert!(
        record.checks[2]
            .evidence
            .contains("no tests added: 1 passing, the fixture's baseline is 1"),
        "{record:?}"
    );

    write(&r, "test/b.t", "value=1");
    write(&r, "src/a.txt", "value=1\nother=2\nextra=3\n");
    fs::remove_file(r.join("notes.txt")).unwrap();
    write(&r, "src/new.txt", "x");
    let record = run(
        &spec(),
        &f,
        &r,
        DispatchEnd::Completed,
        Some("done"),
        tmp.path(),
    )
    .unwrap();
    assert_eq!(failed(&record), ["scope"]);
    let scope = &record.checks[1].evidence;
    for expected in [
        "deleted notes.txt",
        "added src/new.txt",
        "changed src/a.txt",
    ] {
        assert!(scope.contains(expected), "{scope}");
    }
}

#[test]
fn a_red_suite_leaves_behaviors_ungraded_and_failed() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let r = result_from(tmp.path(), &f);
    write(&r, "test/b.t", "value=1");
    write(&r, "test/c.t", "never=there");
    let record = run(
        &spec(),
        &f,
        &r,
        DispatchEnd::Completed,
        Some("done"),
        tmp.path(),
    )
    .unwrap();
    assert_eq!(failed(&record), ["suite", "portable", "behaviors"]);
    assert!(
        record.checks[4].evidence.starts_with("not graded"),
        "{record:?}"
    );
}

#[test]
fn an_unfinished_run_or_a_missing_reply_fails_however_good_the_tests() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let r = result_from(tmp.path(), &f);
    write(&r, "test/b.t", "value=1");
    let record = run(
        &spec(),
        &f,
        &r,
        DispatchEnd::Failed,
        Some("done"),
        tmp.path(),
    )
    .unwrap();
    assert_eq!(failed(&record), ["run_finished"]);
    let record = run(
        &spec(),
        &f,
        &r,
        DispatchEnd::Completed,
        Some("  \n"),
        tmp.path(),
    )
    .unwrap();
    assert_eq!(failed(&record), ["reply_required"]);
    // `required: false` grades nothing about the reply.
    let mut quiet = spec();
    quiet.reply = Some(ReplyGrade {
        asks: vec!["Say what you did.".into()],
        required: false,
    });
    assert!(
        run(&quiet, &f, &r, DispatchEnd::Completed, None, tmp.path())
            .unwrap()
            .passed
    );
}

/// Graded ⇒ asked: a check whose quoted sentence the prompt does not hold
/// refuses to grade, and so does a check that quotes nothing.
#[test]
fn a_check_quoting_what_the_prompt_does_not_say_refuses_to_grade() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let r = result_from(tmp.path(), &f);
    let mut s = spec();
    s.scope.as_mut().unwrap().asks = vec!["Do not modify src/ ever.".into()];
    let err = run(&s, &f, &r, DispatchEnd::Completed, Some("done"), tmp.path())
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("verify.grade.scope quotes \"Do not modify src/ ever.\""),
        "{err}"
    );
    s.scope.as_mut().unwrap().asks = vec![];
    let err = run(&s, &f, &r, DispatchEnd::Completed, Some("done"), tmp.path())
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("verify.grade.scope quotes no prompt sentence"),
        "{err}"
    );
    let mut s = spec();
    s.suite = None;
    let err = run(&s, &f, &r, DispatchEnd::Completed, Some("done"), tmp.path())
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("verify.grade.portable reruns the suite"),
        "{err}"
    );
}

#[test]
fn a_planted_bug_whose_text_the_fixture_no_longer_holds_refuses_to_grade() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let r = result_from(tmp.path(), &f);
    write(&r, "test/b.t", "value=1");
    let mut s = spec();
    s.behaviors.as_mut().unwrap().mutants[0].find = "value=7".into();
    let err = run(&s, &f, &r, DispatchEnd::Completed, Some("done"), tmp.path())
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("planted bug 1 (value) no longer applies"),
        "{err}"
    );
}

/// The planted bug goes into the FIXTURE's copy of the file, replacing the
/// result's: a result that edited the file is graded on scope for that, and
/// its tests are still measured against the fixture's code.
#[test]
fn a_planted_bug_is_applied_to_the_fixtures_file_not_the_results() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let r = result_from(tmp.path(), &f);
    write(&r, "test/b.t", "value=1");
    // The result's own file holds the line twice, so the bug planted in
    // ITS copy would leave one behind and survive; the fixture's does not.
    write(&r, "src/a.txt", "value=1\nother=2\nvalue=1\n");
    let record = run(
        &spec(),
        &f,
        &r,
        DispatchEnd::Completed,
        Some("done"),
        tmp.path(),
    )
    .unwrap();
    assert_eq!(failed(&record), ["scope"]);
}

#[test]
fn style_reads_new_files_only() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let r = result_from(tmp.path(), &f);
    write(&r, "test/b.t", "value=1");
    let mut s = spec();
    s.style = Some(StyleGrade {
        asks: vec!["Write tests.".into()],
        files: "test/**/*.t".into(),
        must_contain: vec!["value".into(), "=".into()],
    });
    assert!(
        run(&s, &f, &r, DispatchEnd::Completed, Some("done"), tmp.path())
            .unwrap()
            .passed
    );
    write(&r, "test/deep/c.t", "other=2");
    let record = run(&s, &f, &r, DispatchEnd::Completed, Some("done"), tmp.path()).unwrap();
    assert_eq!(failed(&record), ["style"]);
    assert!(
        record
            .checks
            .iter()
            .any(|c| c.evidence == "test/deep/c.t lacks `value`"),
        "{record:?}"
    );
}

#[test]
fn append_only_files_may_gain_lines_but_never_lose_or_change_one() {
    assert!(lines_kept("a\nb\nc\n", "x\na\nb\ny\nc\nz\n", &[]));
    assert!(!lines_kept("a\nb\nc\n", "a\nc\nb\n", &[]));
    assert!(!lines_kept("a\nb\n", "a\nB\n", &[]));
    // A comment or a blank line may go when the grader names the comment
    // marker (a TODO replaced by its test); nothing else may.
    let slashes = ["//".to_string()];
    assert!(lines_kept(
        "a\n  // TODO: test b\n\nc\n",
        "a\nit(b)\nc\n",
        &slashes
    ));
    assert!(!lines_kept(
        "a\n  // TODO: test b\n\nc\n",
        "a\nit(b)\nc\n",
        &[]
    ));
    assert!(!lines_kept("a\n// x\nc\n", "a\n// x\n", &slashes));
    assert!(!lines_kept("a\n\nb\n", "a\nb\n", &[]));
    let spec = ScopeGrade {
        asks: vec![],
        may_add: vec![],
        append_only: vec!["test/*.js".into()],
        comment_prefixes: vec![],
    };
    let changes = Changes {
        modified: vec![
            (
                "test/a.js".into(),
                Some(b"one\ntwo\n".to_vec()),
                Some(b"one\ntwo\nthree\n".to_vec()),
            ),
            (
                "test/b.js".into(),
                Some(b"one\ntwo\n".to_vec()),
                Some(b"one\n2\n".to_vec()),
            ),
            ("src/c.js".into(), Some(b"x".to_vec()), Some(b"y".to_vec())),
        ],
        ..Default::default()
    };
    assert_eq!(
        scope_violations(&changes, &spec),
        [
            "changed existing lines in test/b.js (it may only gain lines)",
            "changed src/c.js"
        ]
    );
}

#[test]
fn globs_match_whole_segments() {
    assert!(glob_match("test/**", "test/a.js"));
    assert!(glob_match("test/**", "test/x/y/a.js"));
    assert!(glob_match("test/**/*.test.js", "test/a.test.js"));
    assert!(glob_match("test/**/*.test.js", "test/x/a.test.js"));
    assert!(!glob_match("test/**/*.test.js", "test/a.js"));
    assert!(!glob_match("test/**", "tests/a.js"));
    assert!(!glob_match("test/*.js", "test/x/a.js"));
    assert!(glob_match("src/?.js", "src/a.js"));
}

#[test]
fn junit_counts_testcases_and_ignores_comments_and_cdata() {
    let xml = r#"<?xml version="1.0"?><testsuites><testsuite name="s">
        <testcase name="a &gt; b" classname="x"/>
        <testcase name="b"><failure message="boom"/></testcase>
        <testcase name="c"><skipped/></testcase>
        <testcase name="d"><error/></testcase>
        <testcase name="e"><system-out><![CDATA[<testcase name="forged"/>]]></system-out></testcase>
        <!-- tests 99 --><!-- <testcase name="also forged"/> -->
        </testsuite></testsuites>"#;
    let c = junit::parse(xml).unwrap();
    assert_eq!((c.tests, c.failed, c.skipped, c.passed()), (5, 2, 1, 2));
    assert_eq!(junit::parse("# pass 14\n# fail 0"), None);
    assert_eq!(
        junit::parse("<testsuites></testsuites>").map(|c| c.tests),
        Some(0)
    );
    // `<testcases>` is not a testcase.
    assert_eq!(
        junit::parse("<testsuite><testcases/></testsuite>").map(|c| c.tests),
        Some(0)
    );
}

/// The record ANDs the ungraded verdict with the grader's, and grading again
/// ANDs with the ungraded verdict the first grading saw, never with itself.
#[test]
fn recording_a_grade_keeps_the_ungraded_verdict() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    fs::write(
        dir.join("manifest.json"),
        r#"{"verify": {"passed": true, "details": "gate ok"}}"#,
    )
    .unwrap();
    let rec = |passed| GradeRecord {
        passed,
        checks: vec![GradeCheck {
            id: "suite".into(),
            passed,
            evidence: "e".into(),
            asks: vec![],
        }],
        tests_passed: None,
        ungraded_passed: None,
    };
    record(dir, rec(false)).unwrap();
    let m = RunManifest::read(dir).unwrap();
    assert_eq!(m.verify_passed(), Some(false));
    assert_eq!(
        m.verify_report().unwrap().details,
        "graded: fail (suite: e)"
    );
    record(dir, rec(true)).unwrap();
    let m = RunManifest::read(dir).unwrap();
    assert_eq!(m.verify_passed(), Some(true));
    assert_eq!(
        m.verify_report()
            .unwrap()
            .grade
            .as_ref()
            .unwrap()
            .ungraded_passed,
        Some(true)
    );
    assert_eq!(m.schema_version, Some(9));

    fs::write(
        dir.join("manifest.json"),
        r#"{"verify": {"passed": false, "details": "no tests added"}}"#,
    )
    .unwrap();
    record(dir, rec(true)).unwrap();
    let m = RunManifest::read(dir).unwrap();
    assert_eq!(m.verify_passed(), Some(false));
    assert_eq!(
        m.verify_report().unwrap().details,
        "no tests added; graded: pass (1 checks)"
    );
}

/// The shipped pepper-grinder grader quotes its prompt word for word, so it
/// grades (the fixture itself is not in this repo; its planted bugs are
/// checked against it by `lab grade` and the lab run).
#[test]
fn the_pepper_grinder_grader_quotes_its_prompt() {
    let (_, json) = crate::workloads::load::EMBEDDED_WORKLOADS
        .iter()
        .find(|(id, _)| *id == "pepper-grinder")
        .unwrap();
    let manifest: crate::workloads::types::WorkloadManifest = serde_json::from_str(json).unwrap();
    let spec = manifest
        .workload
        .verify
        .as_ref()
        .unwrap()
        .grade
        .as_ref()
        .unwrap();
    check_asks(spec, manifest.workload.prompt.as_deref().unwrap()).unwrap();
    assert_eq!(
        behavior_names(spec.behaviors.as_ref().unwrap()),
        ["pepper", "row lock", "boot guard", "expiry parsing"]
    );
}

#[test]
fn the_final_reply_is_read_from_the_runtime_envelope() {
    let tmp = TempDir::new().unwrap();
    assert_eq!(final_reply(tmp.path()), None);
    fs::write(
        tmp.path().join("qa-reply.json"),
        "pull noise\n{\"result\":\"stop\",\"final_assistant\":\"# pass 23\"}\n",
    )
    .unwrap();
    assert_eq!(final_reply(tmp.path()).as_deref(), Some("# pass 23"));
    fs::write(tmp.path().join("qa-reply.json"), "{\"result\":\"error\"}").unwrap();
    assert_eq!(final_reply(tmp.path()), None);
}

/// The lab-run step grades a workload that declares a grader, records it,
/// and hands back the recorded verdict; a workload without one is untouched.
#[test]
fn the_lab_run_step_grades_records_and_returns_the_verdict() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let r = result_from(tmp.path(), &f);
    write(&r, "test/b.t", "other=2");
    let run_dir = tmp.path().join("run");
    fs::create_dir_all(&run_dir).unwrap();
    fs::write(run_dir.join("manifest.json"), r#"{"verify": {"passed": true, "details": "gate ok"}}"#).unwrap();
    fs::write(run_dir.join("qa-reply.json"), r#"{"result":"stop","final_assistant":"done"}"#).unwrap();
    let loaded = |grade: Option<&GradeSpec>| crate::workloads::types::LoadedWorkload {
        manifest: serde_json::from_value(serde_json::json!({"workload": {"id": "w", "provider": "coding-task",
            "prompt": PROMPT, "verify": {"command": "true", "grade": grade}}}))
        .unwrap(),
        base_dir: tmp.path().to_path_buf(),
        source: crate::workloads::types::WorkloadSource::OnDisk,
    };
    let mut s = spec();
    s.suite.as_mut().unwrap().more_than_baseline = false;
    let untouched = apply(&run_dir, &f, &r, &DispatchEnd::Completed, &loaded(None), Ok(None)).unwrap();
    assert!(untouched.is_none());
    let graded = apply(&run_dir, &f, &r, &DispatchEnd::Completed, &loaded(Some(&s)), Ok(None)).unwrap().unwrap();
    assert!(!graded.passed);
    assert!(graded.details.starts_with("graded: fail (behaviors:"), "{}", graded.details);
    let m = RunManifest::read(&run_dir).unwrap();
    assert_eq!(m.verify_passed(), Some(false));
    assert_eq!(m.verify_report().unwrap().grade.as_ref().unwrap().checks.len(), 6);
    assert!(!run_dir.join(WORK_DIR).exists(), "the scratch copies are removed");
}
