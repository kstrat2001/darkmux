//! (#3353) `verify.grade`: a workload's grader, as data. Not tied to code:
//! `run_finished`, `reply` and `scope` fit any task, and `suite` is any
//! command that judges the result and writes a report. Only `behaviors`
//! (planted bugs) is about code. Wired into the `coding-task` provider today.
//!
//! Each check names the prompt sentence(s) it grades (`asks`). The grader
//! refuses to run a check whose sentence is not in the prompt word for word,
//! so everything graded is something the prompt asks for. Nothing here names
//! a fixture, a language or a test runner: the fixture-specific parts (globs,
//! commands, planted bugs) are this data, and the runner's results are read
//! from a standard report format (`report`).

use serde::{Deserialize, Serialize};

/// The grader for one workload. `run_finished` always runs; every other check
/// runs only when declared. `portable` and `behaviors` rerun the `suite`, so
/// they need one.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct GradeSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suite: Option<SuiteGrade>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<ScopeGrade>,
    /// The suite must also pass from a second copy at another path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portable: Option<AsksOnly>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<StyleGrade>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behaviors: Option<BehaviorsGrade>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<ReplyGrade>,
}

/// The test results format a suite command writes to `${REPORT}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TestReport {
    /// JUnit XML: one `<testcase>` per test, `<failure>`/`<error>`/`<skipped>` inside.
    Junit,
}

/// Run the tests on a copy of the result. Green means exit 0, a readable
/// report, and no failing test.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct SuiteGrade {
    pub asks: Vec<String>,
    /// A shell command run in the copy. `${REPORT}` is replaced with the path
    /// the report must be written to, outside the copy.
    pub command: String,
    pub report: TestReport,
    /// Also require more passing tests than the fixture's
    /// `baseline.test_count` (the prompt asks for tests to be added).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub more_than_baseline: bool,
    /// The bound on one run of the command. Default 120.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
}

/// Which files the result may touch, against the pristine fixture. A new
/// file must match `may_add`; an existing file matching `append_only` may
/// only gain lines (every original line kept, in order); every other file
/// must be unchanged, and none may be deleted.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct ScopeGrade {
    pub asks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub may_add: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub append_only: Vec<String>,
}

/// A check that needs nothing but the prompt sentence it grades.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct AsksOnly {
    pub asks: Vec<String>,
}

/// Every NEW file matching `files` must contain each of `must_contain`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct StyleGrade {
    pub asks: Vec<String>,
    pub files: String,
    pub must_contain: Vec<String>,
}

/// Planted bugs: each mutant is applied to the fixture's own copy of `file`
/// inside a copy of the result, and the suite must go red. One pass/fail for
/// the whole set; the evidence names the behaviors whose bugs survived.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct BehaviorsGrade {
    pub asks: Vec<String>,
    pub mutants: Vec<Mutant>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct Mutant {
    /// The behavior this bug breaks, as the evidence names it.
    pub behavior: String,
    /// Path relative to the fixture root.
    pub file: String,
    /// Text that occurs exactly once in the fixture's file.
    pub find: String,
    pub replace: String,
}

/// `required: true`: the run must end with a final reply. Its content is
/// never graded here; a reply is graded on content only when the prompt asks
/// for structured output (`verify.expect`).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct ReplyGrade {
    pub asks: Vec<String>,
    pub required: bool,
}
