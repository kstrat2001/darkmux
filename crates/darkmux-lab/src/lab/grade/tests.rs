//! The grader, on a tiny fixture whose "suite" is a shell script that writes
//! JUnit: each `test/*.t` file names a line `src/a.txt` must hold. No test
//! runner beyond `sh` is needed, which is also the point: the grader knows
//! no runner, only the report.

use super::files::{glob_match, lines_kept, scope_violations, Changes};
use super::*;
use darkmux_crew::contained_command::Placement;
use crate::lab::manifest::{GradeRecord, RunManifest};
use crate::workloads::grade_spec::*;
use std::fs;
use tempfile::TempDir;

const PROMPT: &str = "Write tests. Do not modify src/. Tests must pass. Say what you did.";

/// A test reading `skip…` is switched off (it reports nothing); one reading
/// `exact` passes only while `src/a.txt` is byte-identical to `test/exact.copy`
/// (a test of the source text, not of what it says).
const SUITE_SH: &str = r#"report="$1"
pass=0; fail=0; cases=""
for t in test/*.t; do
  want="$(cat "$t")"
  case "$want" in
    skip*) continue;;
    exact) cmp -s src/a.txt test/exact.copy; ok=$?;;
    *) grep -qx "$want" src/a.txt; ok=$?;;
  esac
  if [ $ok = 0 ]; then cases="$cases<testcase name=\"$t\"/>"
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
        result_kind: Some(kind_of(&dispatch)),
        baseline_test_count: Some(1),
        work: &work,
        runner: &Runner::host(),
    })
}

/// The envelope `result` the runtime writes for each way a dispatch ends.
fn kind_of(dispatch: &DispatchEnd) -> &'static str {
    match dispatch {
        DispatchEnd::Completed => "stop",
        DispatchEnd::Escalated { .. } => "escalated",
        DispatchEnd::Failed => "error",
    }
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

/// The record ANDs the ungraded verdict with the grader's, and a regrade
/// takes the ungraded verdict the first grading recorded, never its own.
#[test]
fn recording_a_grade_ands_the_ungraded_verdict() {
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
            ..Default::default()
        }],
        ..Default::default()
    };
    let ok = Ungraded::from_manifest(&RunManifest::read(dir).unwrap());
    assert!(ok.passed);
    record(dir, rec(false), &ok).unwrap();
    let m = RunManifest::read(dir).unwrap();
    assert_eq!(m.verify_passed(), Some(false));
    assert_eq!(
        m.verify_report().unwrap().details,
        "graded: fail (suite: e)"
    );
    // Graded again: the ungraded verdict is the recorded one, not the
    // failed verdict the first grading wrote.
    let again = Ungraded::from_manifest(&m);
    assert!(again.passed);
    record(dir, rec(true), &again).unwrap();
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

    let failed_gate = Ungraded {
        passed: false,
        details: "no tests added".into(),
    };
    record(dir, rec(true), &failed_gate).unwrap();
    let m = RunManifest::read(dir).unwrap();
    assert_eq!(m.verify_passed(), Some(false));
    assert_eq!(
        m.verify_report().unwrap().details,
        "no tests added; graded: pass (1 checks)"
    );
    assert!(!Ungraded::from_manifest(&m).passed);
}

/// A test that checks the source text (here, a byte-for-byte copy) catches
/// every planted bug for that reason alone. The control edit, which changes
/// no behavior, shows it up.
#[test]
fn tests_of_the_source_text_do_not_count_as_catching_bugs() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let r = result_from(tmp.path(), &f);
    write(&r, "test/b.t", "exact");
    fs::copy(f.join("src/a.txt"), r.join("test/exact.copy")).unwrap();
    let record = run(
        &spec(),
        &f,
        &r,
        DispatchEnd::Completed,
        Some("done"),
        tmp.path(),
    )
    .unwrap();
    assert_eq!(failed(&record), ["behaviors"], "{record:?}");
    let ev = &record
        .checks
        .iter()
        .find(|c| c.id == "behaviors")
        .unwrap()
        .evidence;
    assert!(ev.contains("check the source text"), "{ev}");
}

/// Every copy the suite runs in sits under an opaque name, so a test cannot
/// tell a planted-bug run from the real one by its path.
#[test]
fn suite_copies_have_opaque_names() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let r = result_from(tmp.path(), &f);
    write(&r, "test/b.t", "value=1");
    let log = tmp.path().join("cwd.log");
    let mut s = spec();
    s.suite.as_mut().unwrap().command = format!(
        "sh run.sh ${{REPORT}}; s=$?; pwd >> '{}'; exit $s",
        log.display()
    );
    let record = run(&s, &f, &r, DispatchEnd::Completed, Some("done"), tmp.path()).unwrap();
    assert!(record.passed, "{record:?}");
    let dirs: Vec<String> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    // fixture, result, elsewhere, control, one planted bug
    assert_eq!(dirs.len(), 5, "{dirs:?}");
    for d in &dirs {
        // The last `/work/`: the scratch root itself may sit under one (CI runners do).
        let rel = d.rsplit("/work/").next().unwrap_or_else(|| panic!("{d}"));
        let opaque = rel.split('/').next().unwrap();
        assert!(
            opaque.len() == 16 && opaque.bytes().all(|b| b.is_ascii_hexdigit()),
            "{d}"
        );
        assert!(
            rel == format!("{opaque}/result") || rel == format!("{opaque}/elsewhere/result"),
            "{d}"
        );
    }
}

/// A planted bug is never written through a symlink: the link could lead
/// out of the copy, into the run's own sandbox.
#[cfg(unix)]
#[test]
fn a_planted_bug_is_never_written_through_a_symlink() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let r = result_from(tmp.path(), &f);
    write(&r, "test/b.t", "value=1");
    let outside = tmp.path().join("outside");
    write(&outside, "a.txt", "value=1\nother=2\n");
    fs::remove_dir_all(r.join("src")).unwrap();
    std::os::unix::fs::symlink(&outside, r.join("src")).unwrap();
    let err = run(
        &spec(),
        &f,
        &r,
        DispatchEnd::Completed,
        Some("done"),
        tmp.path(),
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("symlink"), "{err:#}");
    assert_eq!(
        fs::read_to_string(outside.join("a.txt")).unwrap(),
        "value=1\nother=2\n"
    );
}

/// A test switched off inside a file that may only gain lines keeps every
/// line, so scope passes it; the suite sees the test is gone.
#[test]
fn an_existing_test_switched_off_fails_the_suite() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let r = result_from(tmp.path(), &f);
    write(&r, "test/b.t", "value=1");
    write(&r, "test/c.t", "value=1");
    write(&r, "test/a.t", "skip\nother=2");
    let mut s = spec();
    s.scope.as_mut().unwrap().append_only = vec!["test/a.t".into()];
    let record = run(&s, &f, &r, DispatchEnd::Completed, Some("done"), tmp.path()).unwrap();
    assert_eq!(failed(&record), ["suite"], "{record:?}");
    let ev = &record
        .checks
        .iter()
        .find(|c| c.id == "suite")
        .unwrap()
        .evidence;
    assert!(ev.contains("\"test/a.t\""), "{ev}");
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

/// (#906) The lab-run step's suites run where `runtime.tests_on_host` says;
/// these tests run them on the host, and put the setting back.
struct TestsOnHost(Option<String>);

impl TestsOnHost {
    fn set() -> Self {
        let prev = std::env::var("DARKMUX_TESTS_ON_HOST").ok();
        unsafe { std::env::set_var("DARKMUX_TESTS_ON_HOST", "true") };
        Self(prev)
    }
}

impl Drop for TestsOnHost {
    fn drop(&mut self) {
        unsafe {
            match &self.0 {
                Some(v) => std::env::set_var("DARKMUX_TESTS_ON_HOST", v),
                None => std::env::remove_var("DARKMUX_TESTS_ON_HOST"),
            }
        }
    }
}

/// The lab-run step grades a workload that declares a grader, records it,
/// and hands back the recorded verdict; a workload without one is untouched.
#[test]
#[serial_test::serial]
fn the_lab_run_step_grades_records_and_returns_the_verdict() {
    let _host = TestsOnHost::set();
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let r = result_from(tmp.path(), &f);
    write(&r, "test/b.t", "other=2");
    let run_dir = tmp.path().join("run");
    fs::create_dir_all(&run_dir).unwrap();
    fs::write(
        run_dir.join("manifest.json"),
        r#"{"verify": {"passed": true, "details": "gate ok"}}"#,
    )
    .unwrap();
    fs::write(
        run_dir.join("qa-reply.json"),
        r#"{"result":"stop","final_assistant":"done"}"#,
    )
    .unwrap();
    let loaded = |grade: Option<&GradeSpec>| crate::workloads::types::LoadedWorkload {
        manifest: serde_json::from_value(
            serde_json::json!({"workload": {"id": "w", "provider": "coding-task",
            "prompt": PROMPT, "verify": {"command": "true", "grade": grade}}}),
        )
        .unwrap(),
        base_dir: tmp.path().to_path_buf(),
        source: crate::workloads::types::WorkloadSource::OnDisk,
    };
    let mut s = spec();
    s.suite.as_mut().unwrap().more_than_baseline = false;
    let untouched = apply(
        &run_dir,
        &f,
        &r,
        &DispatchEnd::Completed,
        None,
        &loaded(None),
        Ok(None),
    )
    .unwrap();
    assert!(untouched.is_none());
    let graded = apply(
        &run_dir,
        &f,
        &r,
        &DispatchEnd::Completed,
        None,
        &loaded(Some(&s)),
        Ok(None),
    )
    .unwrap()
    .unwrap();
    assert!(!graded.passed);
    assert!(
        graded.details.starts_with("graded: fail (behaviors:"),
        "{}",
        graded.details
    );
    let m = RunManifest::read(&run_dir).unwrap();
    assert_eq!(m.verify_passed(), Some(false));
    assert_eq!(
        m.verify_report()
            .unwrap()
            .grade
            .as_ref()
            .unwrap()
            .checks
            .len(),
        6
    );
    assert!(
        !run_dir.join(WORK_DIR).exists(),
        "the scratch copies are removed"
    );
}

/// A gate that could not be applied (here one that stands, beside a coverage
/// threshold), or a raw verify that failed, still leaves a recorded, graded
/// run that fails and says why.
#[test]
#[serial_test::serial]
fn the_lab_run_step_records_a_fail_when_the_gate_errs() {
    let _host = TestsOnHost::set();
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let r = result_from(tmp.path(), &f);
    write(&r, "test/b.t", "value=1");
    let run_dir = tmp.path().join("run");
    fs::create_dir_all(&run_dir).unwrap();
    fs::write(
        run_dir.join("manifest.json"),
        r#"{"verify": {"passed": true, "details": "x"}}"#,
    )
    .unwrap();
    fs::write(
        run_dir.join("qa-reply.json"),
        r#"{"result":"stop","final_assistant":"done"}"#,
    )
    .unwrap();
    let mut s = spec();
    s.suite.as_mut().unwrap().more_than_baseline = false;
    let loaded = crate::workloads::types::LoadedWorkload {
        manifest: serde_json::from_value(
            serde_json::json!({"workload": {"id": "w", "provider": "coding-task",
            "prompt": PROMPT, "verify": {"command": "true", "coverage_min_pct": 50.0, "grade": s}}}),
        )
        .unwrap(),
        base_dir: tmp.path().to_path_buf(),
        source: crate::workloads::types::WorkloadSource::OnDisk,
    };
    let got = apply(
        &run_dir,
        &f,
        &r,
        &DispatchEnd::Completed,
        None,
        &loaded,
        Err(anyhow!("boom")),
    )
    .unwrap()
    .unwrap();
    assert!(!got.passed);
    assert!(
        got.details
            .starts_with("verify gate could not be applied: boom; graded: pass"),
        "{}",
        got.details
    );
    let m = RunManifest::read(&run_dir).unwrap();
    assert_eq!(m.verify_passed(), Some(false));
    assert_eq!(
        m.verify_report()
            .unwrap()
            .grade
            .as_ref()
            .unwrap()
            .ungraded_passed,
        Some(false)
    );

    let raw = crate::workloads::types::VerifyOutcome {
        passed: false,
        details: "npm test failed".into(),
    };
    let got = apply(
        &run_dir,
        &f,
        &r,
        &DispatchEnd::Completed,
        Some(&raw),
        &loaded,
        Ok(None),
    )
    .unwrap()
    .unwrap();
    assert!(!got.passed);
    assert!(
        got.details.starts_with("npm test failed; graded: pass"),
        "{}",
        got.details
    );
}

/// A dispatch that exits cleanly at its turn limit, or leaves no envelope,
/// did not finish: only the runtime's `stop` is the model stopping itself.
#[test]
fn a_turn_limit_or_a_missing_envelope_is_not_a_finish() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let r = result_from(tmp.path(), &f);
    write(&r, "test/b.t", "value=1");
    let work = tmp.path().join("work");
    for (kind, says) in [
        (Some("max_turns"), "`max_turns`"),
        (None, "no runtime envelope"),
    ] {
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work).unwrap();
        let record = grade(&GradeInput {
            spec: &spec(),
            prompt: PROMPT,
            fixture: &f,
            result: &r,
            dispatch: &DispatchEnd::Completed,
            reply: Some("done"),
            result_kind: kind,
            baseline_test_count: Some(1),
            work: &work,
            runner: &Runner::host(),
        })
        .unwrap();
        assert_eq!(failed(&record), ["run_finished"], "{record:?}");
        assert!(
            record.checks[0].evidence.contains(says),
            "{}",
            record.checks[0].evidence
        );
    }
}

/// The runtime writes `<empty>` as the final answer when there was none.
#[test]
fn the_runtimes_empty_sentinel_is_no_reply() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("qa-reply.json"),
        r#"{"result":"stop","final_assistant":"<empty>"}"#,
    )
    .unwrap();
    assert_eq!(final_reply(tmp.path()), None);
    assert_eq!(result_kind(tmp.path()).as_deref(), Some("stop"));
}

/// A test that asserts a planted bug's own text, rather than what the code
/// does, survives a trailing blank line; the respaced span shows it up.
#[test]
fn tests_that_assert_the_planted_span_do_not_count() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    // The bug's span gains a space; `grep -qx` of the exact line then fails.
    write(&f, "src/a.txt", "value = 1\nother=2\n");
    write(&f, "test/a.t", "other=2");
    let r = result_from(tmp.path(), &f);
    write(&r, "test/b.t", "value = 1");
    let mut s = spec();
    s.behaviors.as_mut().unwrap().mutants[0].find = "value = 1".into();
    s.behaviors.as_mut().unwrap().mutants[0].replace = "value = 9".into();
    let record = run(&s, &f, &r, DispatchEnd::Completed, Some("done"), tmp.path()).unwrap();
    assert_eq!(failed(&record), ["behaviors"], "{record:?}");
    let ev = &record
        .checks
        .iter()
        .find(|c| c.id == "behaviors")
        .unwrap()
        .evidence;
    assert!(ev.contains("gains a space"), "{ev}");
    assert_eq!(respace("  if (a && b)").as_deref(), Some("  if  (a && b)"));
    assert_eq!(respace("nospace"), None);
}

/// Scope sees what the suite runs: a dependency tree is diffed, and a build
/// dropping at any depth is ignored by scope and removed from the suite's copy.
#[test]
fn scope_diffs_dependencies_and_ignores_droppings_at_any_depth() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let r = result_from(tmp.path(), &f);
    write(&r, "test/b.t", "value=1");
    write(&r, "src/__pycache__/a.pyc", "x");
    write(&r, ".coverage", "x");
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
    write(&r, "node_modules/helper/index.js", "x");
    let record = run(
        &spec(),
        &f,
        &r,
        DispatchEnd::Completed,
        Some("done"),
        tmp.path(),
    )
    .unwrap();
    assert_eq!(failed(&record), ["scope"], "{record:?}");

    let copy = tmp.path().join("copy");
    write(&copy, "src/__pycache__/a.pyc", "x");
    write(&copy, "src/a.txt", "x");
    write(&copy, "node_modules/p/coverage/c.js", "x");
    super::files::prune_artifacts(&copy, "").unwrap();
    assert!(!copy.join("src/__pycache__").exists());
    assert!(copy.join("src/a.txt").exists());
    assert!(
        copy.join("node_modules/p/coverage/c.js").exists(),
        "a package's own files stay"
    );
}

/// A planted bug's file is a plain relative path: one that climbs out of the
/// copy or is absolute is refused before anything is written.
#[test]
fn a_planted_bug_path_never_leaves_the_copy() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let r = result_from(tmp.path(), &f);
    write(&r, "test/b.t", "value=1");
    write(&f, "../escape.txt", "value=1\n");
    let mut s = spec();
    s.behaviors.as_mut().unwrap().mutants[0].file = "../escape.txt".into();
    let err = run(&s, &f, &r, DispatchEnd::Completed, Some("done"), tmp.path()).unwrap_err();
    assert!(
        format!("{err:#}").contains("not a plain relative path"),
        "{err:#}"
    );
}

/// The fixture's own run must leave a readable report, or the grader cannot
/// tell which tests must still pass.
#[test]
fn an_unreadable_fixture_report_refuses_to_grade() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let r = result_from(tmp.path(), &f);
    write(&r, "test/b.t", "value=1");
    // Only the result writes a report.
    write(&r, "run.sh", SUITE_SH);
    write(&f, "run.sh", "exit 0\n");
    let err = run(
        &spec(),
        &f,
        &r,
        DispatchEnd::Completed,
        Some("done"),
        tmp.path(),
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("no readable report"), "{err:#}");
}

#[test]
fn an_empty_quote_beside_a_real_one_is_refused() {
    let mut s = spec();
    s.scope.as_mut().unwrap().asks.push("  ".into());
    let err = check_asks(&s, PROMPT).unwrap_err();
    assert!(err.to_string().contains("empty sentence"), "{err}");
}

/// Invalid UTF-8 in an append-only file cannot be compared line by line, so
/// any change to it fails.
#[test]
fn a_non_utf8_append_only_change_fails() {
    let changes = Changes {
        modified: vec![(
            "t.bin".into(),
            Some(b"a\xff\n".to_vec()),
            Some(b"a\xfe\n".to_vec()),
        )],
        ..Default::default()
    };
    let spec = ScopeGrade {
        asks: vec![],
        may_add: vec![],
        append_only: vec!["t.bin".into()],
        comment_prefixes: vec![],
    };
    assert_eq!(scope_violations(&changes, &spec).len(), 1);
}

/// A comment opener inside CDATA is text, not a comment that eats the
/// testcases after it.
#[test]
fn junit_cdata_holding_a_comment_opener_hides_nothing() {
    let xml = r#"<testsuites><testsuite>
        <testcase name="a"><system-out><![CDATA[log <!-- unclosed]]></system-out></testcase>
        <testcase name="b"><failure message="x"/></testcase>
        <!-- <testcase name="fake"/> -->
    </testsuite></testsuites>"#;
    let c = junit::parse(xml).unwrap();
    assert_eq!(
        (c.tests, c.failed, c.passing.clone()),
        (2, 1, vec!["a".to_string()])
    );
}

/// With a grader suite and no coverage threshold, the work gate's node-only
/// count does not decide the run: the verify command's own verdict does.
#[test]
fn a_grader_suite_supersedes_the_node_only_work_gate() {
    let raw = crate::workloads::types::VerifyOutcome {
        passed: true,
        details: "ok".into(),
    };
    let unreadable = WorkGateResult {
        passed: false,
        details: "verify output's passing-test count is unreadable".into(),
        tests_added: None,
    };
    let (u, _) = ungraded_verdict(&spec(), None, Some(&raw), Ok(Some(unreadable.clone())));
    assert!(u.passed);
    // A coverage threshold has no grader check, so the gate stands.
    let (u, _) = ungraded_verdict(
        &spec(),
        Some(90.0),
        Some(&raw),
        Ok(Some(unreadable.clone())),
    );
    assert!(!u.passed);
    let mut no_suite = spec();
    no_suite.suite = None;
    let (u, _) = ungraded_verdict(&no_suite, None, Some(&raw), Ok(Some(unreadable)));
    assert!(!u.passed);
}

/// (#906) The grader in a container reaches the same verdicts as on the host:
/// the suite, the second path and the planted bugs all run there, each copy
/// the only thing mounted. Needs Docker and the image; run with `--ignored`.
#[test]
#[ignore = "needs a Docker daemon and node:22-bookworm-slim"]
fn the_grader_in_a_container_reaches_the_hosts_verdicts() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let good = result_from(tmp.path(), &f);
    write(&good, "test/b.t", "value=1");
    let weak = tmp.path().join("weak");
    crate::lab::cow_clone::cow_clone_dir(&f, &weak).unwrap();
    write(&weak, "test/b.t", "other=2");
    let contained = Runner {
        placement: Placement::Container("node:22-bookworm-slim".into()),
        user: None,
    };
    for (result, passed) in [(&good, true), (&weak, false)] {
        let mut verdicts = vec![];
        for runner in [Runner::host(), contained.clone()] {
            let work = tmp.path().join("work");
            let _ = fs::remove_dir_all(&work);
            fs::create_dir_all(&work).unwrap();
            let record = grade(&GradeInput {
                spec: &spec(),
                prompt: PROMPT,
                fixture: &f,
                result,
                dispatch: &DispatchEnd::Completed,
                reply: Some("done"),
                result_kind: Some("stop"),
                baseline_test_count: Some(1),
                work: &work,
                runner: &runner,
            })
            .unwrap();
            assert_eq!(record.passed, passed, "{record:?}");
            verdicts.push(record.checks.iter().map(|c| (c.id.clone(), c.passed)).collect::<Vec<_>>());
        }
        assert_eq!(verdicts[0], verdicts[1]);
    }
}

/// `DARKMUX_HOME` pointed at a scratch root for one test, put back after.
struct Home(Option<std::ffi::OsString>);

impl Home {
    fn set(root: &Path) -> Self {
        let prev = std::env::var_os("DARKMUX_HOME");
        unsafe { std::env::set_var("DARKMUX_HOME", root) };
        Self(prev)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        unsafe {
            match &self.0 {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
    }
}

/// A kept run of workload `w` (declared under `DARKMUX_HOME` with `spec`):
/// the result in the run's own `sandbox`, the fixture where the manifest
/// says, both hashes recorded, and a finished dispatch with a reply.
/// `edit` changes the manifest before it is written.
fn kept_run(
    root: &Path,
    spec: Option<&GradeSpec>,
    edit: impl FnOnce(&mut serde_json::Value),
) -> (PathBuf, PathBuf) {
    let f = fixture(root);
    let run_dir = root.join("runs").join("r1");
    let r = run_dir.join("sandbox");
    fs::create_dir_all(&run_dir).unwrap();
    crate::lab::cow_clone::cow_clone_dir(&f, &r).unwrap();
    write(&r, "test/b.t", "value=1");
    write(
        Path::new(&std::env::var_os("DARKMUX_HOME").unwrap()),
        "workloads/w.json",
        &serde_json::json!({"workload": {"id": "w", "provider": "coding-task",
            "prompt": PROMPT, "verify": {"command": "true", "grade": spec}}})
        .to_string(),
    );
    let hash = |d: &Path| crate::lab::sandbox_hash::hash_sandbox_dir(d).unwrap();
    let mut manifest = serde_json::json!({
        "workload": "w",
        "ok": true,
        "sandbox": r.to_string_lossy(),
        "final_hash": hash(&r),
        "fixture": {"source_path": f.to_string_lossy(), "baseline_hash": hash(&f)},
        "verify": {"passed": true, "details": "gate ok"},
    });
    edit(&mut manifest);
    fs::write(run_dir.join("manifest.json"), manifest.to_string()).unwrap();
    fs::write(
        run_dir.join("qa-reply.json"),
        r#"{"result":"stop","final_assistant":"done"}"#,
    )
    .unwrap();
    (run_dir, f)
}

/// The grader a kept run declares: [`spec`], with no baseline to beat (the
/// test fixture declares none).
fn kept_spec() -> GradeSpec {
    let mut s = spec();
    s.suite.as_mut().unwrap().more_than_baseline = false;
    s
}

fn regrade_at(run_dir: &Path, fixture: Option<&Path>, write: bool) -> Result<Regrade> {
    regrade(run_dir.to_str().unwrap(), fixture, write)
}

/// `lab grade` grades a kept run against the fixture it started from, prints
/// every check, and writes the manifest only when asked.
#[test]
#[serial_test::serial]
fn a_kept_run_is_graded_again_and_recorded_only_when_asked() {
    let _host = TestsOnHost::set();
    let tmp = TempDir::new().unwrap();
    let _home = Home::set(&tmp.path().join("home"));
    let (run_dir, f) = kept_run(tmp.path(), Some(&kept_spec()), |_| {});
    let before = fs::read_to_string(run_dir.join("manifest.json")).unwrap();

    let graded = regrade_at(&run_dir, None, false).unwrap();
    assert!(graded.passed(), "{:?}", graded.record);
    assert!(!graded.recorded);
    assert_eq!(fs::read_to_string(run_dir.join("manifest.json")).unwrap(), before);
    let text = render(&graded);
    assert!(text.contains("  PASS  behaviors"), "{text}");
    assert!(text.ends_with("  verdict: pass\n"), "{text}");

    // Another checkout of the same content is as good as the run's own.
    let copy = tmp.path().join("checkout");
    crate::lab::cow_clone::cow_clone_dir(&f, &copy).unwrap();
    let graded = regrade_at(&run_dir, Some(&copy), true).unwrap();
    assert!(graded.recorded);
    assert!(render(&graded).ends_with("  verdict: pass (recorded in the run's manifest)\n"));
    let m = RunManifest::read(&run_dir).unwrap();
    assert_eq!(m.verify_report().unwrap().grade.as_ref().unwrap().checks.len(), 6);
    assert!(!run_dir.join(WORK_DIR).exists());

    // The CLI's exit code: 0 only when every run is graded and passes.
    let run = run_dir.to_string_lossy().into_owned();
    assert_eq!(grade_runs(std::slice::from_ref(&run), None, false), 0);
    let missing = tmp.path().join("no/such/run").to_string_lossy().into_owned();
    assert_eq!(grade_runs(&[run, missing], None, false), 1);
}

/// A run kept outside the lab (no `sandbox` beside its manifest) is graded
/// from the manifest's sandbox path; a dispatch that escalated or failed
/// fails `run_finished`, and a failed ungraded verdict shows as its own row.
#[test]
#[serial_test::serial]
fn a_kept_runs_dispatch_and_ungraded_verdict_come_from_its_manifest() {
    let _host = TestsOnHost::set();
    let tmp = TempDir::new().unwrap();
    let _home = Home::set(&tmp.path().join("home"));
    let elsewhere = tmp.path().join("elsewhere");
    let (run_dir, _) = kept_run(tmp.path(), Some(&kept_spec()), |m| {
        m["escalation"] = "stuck".into();
        m["sandbox"] = elsewhere.to_string_lossy().into();
        m["verify"] = serde_json::json!({"passed": false, "details": "no tests added"});
    });
    fs::rename(run_dir.join("sandbox"), &elsewhere).unwrap();
    let graded = regrade_at(&run_dir, None, false).unwrap();
    assert!(!graded.passed());
    let text = render(&graded);
    assert!(text.contains("  FAIL  run_finished"), "{text}");
    assert!(text.contains("  FAIL  ungraded        no tests added"), "{text}");
    assert!(text.ends_with("  verdict: fail\n"), "{text}");
    assert_eq!(grade_runs(&[run_dir.to_string_lossy().into_owned()], None, false), 1);

    let (run_dir, _) = kept_run(&tmp.path().join("b"), Some(&kept_spec()), |m| {
        m["ok"] = false.into();
        m["verify"] = serde_json::json!({"passed": false, "details": ""});
    });
    let graded = regrade_at(&run_dir, None, false).unwrap();
    assert!(!graded.record.checks[0].passed, "{:?}", graded.record);
    assert!(
        render(&graded).contains("the verify command or work gate failed"),
        "{}",
        render(&graded)
    );
}

/// `lab grade` refuses, and writes nothing, when it cannot tell what the run
/// was: no workload, no grader, no fixture, no baseline, or a fixture or
/// sandbox whose content is no longer what the run had.
#[test]
#[serial_test::serial]
fn a_kept_run_that_cannot_be_graded_is_refused() {
    let _host = TestsOnHost::set();
    let tmp = TempDir::new().unwrap();
    let _home = Home::set(&tmp.path().join("home"));
    type Edit = fn(&mut serde_json::Value);
    let cases: [(&str, Edit, &str); 4] = [
        ("a", |m| m["workload"] = serde_json::Value::Null, "names no workload"),
        ("b", |m| m["fixture"]["source_path"] = serde_json::Value::Null, "names no fixture; pass --fixture"),
        ("c", |m| m["fixture"]["baseline_hash"] = serde_json::Value::Null, "records no baseline_hash"),
        ("d", |m| m["final_hash"] = "0".into(), "changed after the run"),
    ];
    for (dir, edit, want) in cases {
        let (run_dir, _) = kept_run(&tmp.path().join(dir), Some(&kept_spec()), edit);
        let err = regrade_at(&run_dir, None, true).err().unwrap();
        assert!(format!("{err:#}").contains(want), "{dir}: {err:#}");
    }

    let (run_dir, f) = kept_run(&tmp.path().join("e"), Some(&kept_spec()), |_| {});
    write(&f, "notes.txt", "moved on\n");
    let err = regrade_at(&run_dir, None, true).err().unwrap();
    assert!(format!("{err:#}").contains("no longer matches"), "{err:#}");

    let (run_dir, _) = kept_run(&tmp.path().join("f"), Some(&kept_spec()), |_| {});
    fs::remove_dir_all(run_dir.join("sandbox")).unwrap();
    let err = regrade_at(&run_dir, None, true).err().unwrap();
    assert!(format!("{err:#}").contains("sandbox is gone"), "{err:#}");

    let (run_dir, _) = kept_run(&tmp.path().join("g"), None, |_| {});
    let err = regrade_at(&run_dir, None, true).err().unwrap();
    assert!(format!("{err:#}").contains("declares no verify.grade"), "{err:#}");
}

/// A manifest with no verify report gets one to hold the grade; one whose
/// `verify` is not an object is refused rather than overwritten.
#[test]
fn recording_a_grade_makes_a_report_or_refuses_a_foreign_one() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let rec = GradeRecord {
        passed: true,
        ..Default::default()
    };
    let failed_quietly = Ungraded {
        passed: false,
        details: String::new(),
    };
    fs::write(dir.join("manifest.json"), "{}").unwrap();
    record(dir, rec.clone(), &failed_quietly).unwrap();
    let m = RunManifest::read(dir).unwrap();
    assert_eq!(m.verify_passed(), Some(false));
    assert!(
        m.verify_report()
            .unwrap()
            .details
            .starts_with("ungraded check failed; graded: pass"),
        "{}",
        m.verify_report().unwrap().details
    );

    fs::write(dir.join("manifest.json"), r#"{"verify": "pass"}"#).unwrap();
    let err = record(dir, rec, &failed_quietly).unwrap_err();
    assert!(err.to_string().contains("not a JSON object"), "{err}");
}

/// A grader without a suite grades what it declares and reports no test count.
#[test]
fn a_grader_without_a_suite_grades_scope_and_reply_only() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let r = result_from(tmp.path(), &f);
    write(&r, "test/b.t", "value=1");
    let s = GradeSpec {
        suite: None,
        portable: None,
        behaviors: None,
        ..spec()
    };
    let record = run(&s, &f, &r, DispatchEnd::Completed, Some("done"), tmp.path()).unwrap();
    assert!(record.passed, "{record:?}");
    assert_eq!(record.tests_passed, None);
    let ids: Vec<&str> = record.checks.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["run_finished", "scope", "reply_required"]);
}

/// Bugs planted in one behavior that all survive are named once, by number.
#[test]
fn surviving_bugs_of_one_behavior_are_named_together() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let r = result_from(tmp.path(), &f);
    write(&r, "test/b.t", "other=2");
    let mut s = spec();
    let mutants = &mut s.behaviors.as_mut().unwrap().mutants;
    mutants.push(Mutant {
        behavior: "value".into(),
        file: "src/a.txt".into(),
        find: "value=1".into(),
        replace: "value=8".into(),
    });
    let record = run(&s, &f, &r, DispatchEnd::Completed, Some("done"), tmp.path()).unwrap();
    let ev = &record
        .checks
        .iter()
        .find(|c| c.id == "behaviors")
        .unwrap()
        .evidence;
    assert!(ev.contains("planted bug in value (bug #1, #2)"), "{ev}");
}

/// A tree the grader cannot read is an error, never an empty diff.
#[test]
fn an_unreadable_tree_is_an_error_not_an_empty_diff() {
    let tmp = TempDir::new().unwrap();
    let f = fixture(tmp.path());
    let missing = tmp.path().join("missing");
    for (a, b) in [(&missing, &f), (&f, &missing)] {
        let err = super::files::compare(a, b).unwrap_err();
        assert!(format!("{err:#}").contains("reading"), "{err:#}");
    }
    assert!(super::files::prune_artifacts(&missing, "").is_err());
}

/// (#3361) The eval's own folder never reaches the sandbox, so the scope
/// check does not read its absence as a deletion; a file the model adds
/// under that name is still a change.
#[test]
fn the_eval_folder_is_not_a_deletion() {
    let tmp = TempDir::new().unwrap();
    let fixture = tmp.path().join("fixture");
    let result = tmp.path().join("result");
    for d in [&fixture, &result] {
        fs::create_dir_all(d.join("src")).unwrap();
        fs::write(d.join("src/a.js"), "a").unwrap();
        fs::write(d.join(".fixture.json"), r#"{"name": "x", "satisfies": "x@1", "eval_dir": "eval"}"#).unwrap();
    }
    fs::create_dir_all(fixture.join("eval/workloads")).unwrap();
    fs::write(fixture.join("eval/workloads/x.json"), "{}").unwrap();
    let changes = super::files::compare(&fixture, &result).unwrap();
    assert!(changes.deleted.is_empty(), "{:?}", changes.deleted);

    fs::create_dir_all(result.join("eval")).unwrap();
    fs::write(result.join("eval/planted.txt"), "x").unwrap();
    let changes = super::files::compare(&fixture, &result).unwrap();
    assert_eq!(changes.added, vec!["eval/planted.txt".to_string()]);
}
