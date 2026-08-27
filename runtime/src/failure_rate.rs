//! Tool-failure-rate threshold — flag when one tool keeps failing.
//!
//! Sibling to [`crate::cycle_detector`]: that module catches *"the
//! model keeps reading the same file"* (repeated SUCCESS pattern);
//! this module catches *"the model keeps trying `gcc` but it isn't
//! installed"* (repeated FAILURE pattern). Both are observability-
//! only in the MVP, both are part of epic #417 (automate-the-human-
//! in-the-loop), and both follow the same edge-triggered shape.
//!
//! Empirically observed in Beat 45: a coder dispatch kept retrying
//! `gcc` inside the dispatch sandbox where the binary doesn't exist.
//! Each turn: tool call → error → next turn → tool call → error → ...
//! Burned ~20 turns before bailing on MAX_TURNS. A human watching
//! would have stopped at attempt 3 and reconsidered. (#419)
//!
//! MVP: warn-only via trajectory event. No behavioral change. Bail-
//! on-cascade is a follow-up if warn alone proves insufficient.
//!
//! ## Failure detection
//!
//! Per-tool result strings carry the failure signal:
//! - Generic dispatch error: starts with `"tool 'NAME' returned error:"`
//!   (see `runtime/src/tools/mod.rs::dispatch`)
//! - Unknown tool: starts with `"tool 'NAME' is not available"`
//! - Bash non-zero exit: starts with `"exit: N"` where N != 0
//!   (see `execute_bash` — exit 124 is timeout-failure too)
//! - Otherwise: success
//!
//! ## Per-signature isolation
//!
//! Counters are keyed per `(tool, args)` signature (#484) — not global,
//! not per-tool-name. A dispatch with `gcc x.c` failing 3× while `read`
//! succeeds still flags: `read`'s success doesn't reset the `gcc`
//! signature. Mirrors how a human would say *"the gcc command isn't
//! working"*, not *"the dispatch is broken."*

use std::collections::{HashMap, HashSet};

/// Default threshold — failures of one `(tool, args)` signature before warning.
pub const DEFAULT_WARN_THRESHOLD: u32 = 3;

/// (#419) Signal emitted when one `(tool, args)` signature's failure
/// counter crosses the threshold. Observability-only in the MVP — no bail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureCascadeSignal {
    /// Edge-triggered when a `(tool, args)` signature's failure counter
    /// first reaches the threshold. Further failures of the same signature
    /// stay quiet; the warn re-arms when that signature next succeeds.
    Suspected {
        tool_name: String,
        failure_count: u32,
        /// (#2008) WHY it could not run — "command not found", "timed out",
        /// a toolchain that would not load. The nudge quotes this so the
        /// model is told what actually broke instead of a bare count, and so
        /// exit-127 and timeout cases read differently without the template
        /// needing branches it cannot express.
        reason: String,
    },
}

/// (#419) Per-signature failure counter — cumulative failures of a
/// `(tool, args)` signature since it last succeeded, NOT consecutive
/// across tools. One instance per dispatch / agent loop.
#[derive(Debug)]
pub struct FailureRateDetector {
    counters: HashMap<String, u32>,
    threshold: u32,
    /// Edge-trigger guard: tool names we've already warned about.
    /// Cleared on a per-tool success so a future re-emerging cascade
    /// fires again.
    warned: HashSet<String>,
}

impl FailureRateDetector {
    pub fn new() -> Self {
        Self::with_threshold(DEFAULT_WARN_THRESHOLD)
    }

    pub fn with_threshold(threshold: u32) -> Self {
        Self {
            counters: HashMap::new(),
            threshold,
            warned: HashSet::new(),
        }
    }

    /// Record a tool result. Returns `Some(Suspected)` only on the
    /// edge where this tool+args signature's counter first crosses the
    /// threshold.
    ///
    /// Counters are keyed on `(tool_name, canonical_args)` — NOT tool
    /// name alone (#484). A model running `npm test fileA`, `npm test
    /// fileB`, `npm test fileC` and getting an assertion failure on each
    /// is doing legitimate iterative diagnostic work, not cascading on a
    /// broken tool; those are three distinct signatures, each count=1, so
    /// no cascade fires. Three failures of the *same* command (the #419
    /// `gcc`-isn't-installed case) share a signature and still cascade.
    /// Canonicalization mirrors [`crate::cycle_detector::canonical_args`].
    pub fn record(
        &mut self,
        tool_name: &str,
        raw_args: &str,
        result: &str,
    ) -> Option<FailureCascadeSignal> {
        let sig = signature(tool_name, raw_args);
        // (#2008) Only a genuinely broken instrument cascades. A command that
        // RAN and reported non-zero — a red test, a lint finding — is the tool
        // working, and counting it here produced a nudge telling the model its
        // test runner was broken on exactly the workload darkmux exists to
        // run. Repetition of a still-failing command is a real pathology, but
        // it belongs to the cycle detector, which already fires on identical
        // args regardless of outcome and says something true about it.
        if !classify_outcome(tool_name, result).tool_worked() {
            let count = self.counters.entry(sig.clone()).or_insert(0);
            *count += 1;
            let count_now = *count;
            if count_now >= self.threshold && !self.warned.contains(&sig) {
                self.warned.insert(sig);
                let reason = match classify_outcome(tool_name, result) {
                    ToolOutcome::Failed { reason } => reason,
                    // Unreachable: this arm only runs when the outcome was
                    // Failed. Kept total rather than panicking on a future
                    // refactor that changes the guard above.
                    _ => "the tool did not complete".to_string(),
                };
                return Some(FailureCascadeSignal::Suspected {
                    tool_name: tool_name.to_string(),
                    failure_count: count_now,
                    reason,
                });
            }
            None
        } else {
            // Success resets this signature's counter AND clears its warn
            // flag so a future cascade can re-fire.
            self.counters.remove(&sig);
            self.warned.remove(&sig);
            None
        }
    }
}

impl Default for FailureRateDetector {
    fn default() -> Self {
        Self::new()
    }
}

/// Counter key for one tool call: `tool_name` + the cycle-detector's
/// canonical args. Reusing [`crate::cycle_detector::canonical_args`]
/// keeps the two sibling detectors' notion of "the same call" identical
/// — e.g. `bash` keys on the full command, so different `npm test <file>`
/// invocations are distinct signatures (#484).
fn signature(tool_name: &str, raw_args: &str) -> String {
    format!(
        "{tool_name}|{}",
        crate::cycle_detector::canonical_args(tool_name, raw_args)
    )
}

/// (#2008) What actually happened to a tool call.
///
/// Replaces the single boolean the old `is_failure_result` answered, because
/// one boolean was serving consumers that need different questions answered:
/// the inactivity watchdogs ask "did work happen?", the cascade detector asks
/// "is the instrument broken?", and the record asks "what should an operator
/// see?". A non-zero exit answers none of those — it conflates a command that
/// RAN and reported a result with one that never ran at all.
///
/// The distinction is not new: [`classify_failed_to_run`] (#799) already drew
/// it for the trust-critical sign-off case. This promotes it from one call
/// site to the type, so every consumer inherits it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutcome {
    /// Ran, reported success. `bash` exit 0, or any other tool's `Ok`.
    Ok,
    /// Ran correctly and reported a non-zero result. A failing test in TDD's
    /// red phase, a lint finding, `grep` with no matches. This is WORK — the
    /// tool did its job — so it is proof-of-work for the watchdogs and must
    /// never count toward a tool-failure cascade.
    Reported { exit_code: i32 },
    /// Did not run, or could not complete. Exit 127/126, a spawn or
    /// argument-parse failure, a toolchain that could not load, or a timeout.
    /// This is the only class that means the instrument is broken.
    Failed { reason: String },
}

impl ToolOutcome {
    /// Did the TOOL succeed? True for [`Self::Ok`] and [`Self::Reported`] —
    /// both mean the tool did its job, which is the question the `ok` field
    /// has always documented itself as answering (see `trajectory.rs`'s
    /// `append_tool_completed` and dispatch_internal's watchdog comment).
    pub fn tool_worked(&self) -> bool {
        !matches!(self, Self::Failed { .. })
    }

    /// The wire discriminator written into records.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Reported { .. } => "reported",
            Self::Failed { .. } => "failed",
        }
    }
}

/// (#2008) Classify a tool result into [`ToolOutcome`]. Supersedes the old
/// `is_failure_result`, which was deleted rather than kept: a superseded
/// classifier left beside its replacement is how the wrong one gets called
/// again.
///
/// **Tool-surface invariant** (carried over from that function): all non-bash
/// tools route failures through `Tool::execute`'s `Err`, which the dispatcher
/// wraps as `"tool 'NAME' returned error: ..."`. That is why only the generic
/// marker plus the bash exit-code parse are needed. Verified against
/// `runtime/src/tools/mod.rs::execute_{read,write,edit,search}`. A future tool
/// that returns `Ok(error-shaped-text)` must add its pattern here.
///
/// The timeout branch is load-bearing and easy to lose: `classify_failed_to_run`
/// deliberately returns `None` for exit 124 (it keys on 127/126 and stderr
/// load-markers), so deriving "failed" from that function alone would silently
/// reclassify every timeout as [`ToolOutcome::Reported`] — and a command that
/// keeps timing out is exactly the cascade this detector exists to catch.
/// The `(TIMED OUT` marker is trustworthy because #905 gates it on the
/// `timeout` wrapper having actually run, so a user command that genuinely
/// exits 124 on its own still reads as `Reported`.
pub fn classify_outcome(tool_name: &str, result: &str) -> ToolOutcome {
    let generic_marker = format!("tool '{tool_name}' returned error:");
    if result.starts_with(&generic_marker) {
        return ToolOutcome::Failed { reason: "the tool returned an error".to_string() };
    }
    let unknown_marker = format!("tool '{tool_name}' is not available");
    if result.starts_with(&unknown_marker) {
        return ToolOutcome::Failed { reason: "no such tool".to_string() };
    }

    if tool_name == "bash" {
        if let Some(rest) = result.strip_prefix("exit: ") {
            let code_token = rest.split_whitespace().next().unwrap_or("");
            let code: i32 = code_token.parse().unwrap_or(-1);
            if code == 0 {
                return ToolOutcome::Ok;
            }
            if result.contains("(TIMED OUT") {
                return ToolOutcome::Failed {
                    reason: "the command timed out before completing".to_string(),
                };
            }
            if let Some(reason) = classify_failed_to_run(tool_name, result) {
                return ToolOutcome::Failed { reason: reason.to_string() };
            }
            return ToolOutcome::Reported { exit_code: code };
        }
    }
    ToolOutcome::Ok
}

/// (#799) Classify whether a bash result means the command **failed to run**
/// — it never actually executed — as distinct from running and returning a
/// non-zero exit (a real code/test failure). This is the trust-critical class:
/// a verifier that never ran means any SIGNOFF claiming its result is
/// fabricated. Returns a short reason when failed-to-run, else `None`.
///
/// Deliberately a SOFT, heuristic signal: the caller surfaces it for human
/// review and never auto-acts on it (#799), so a rare false positive only
/// costs a glance. Two high-confidence signal classes:
///   1. **Exit 127 / 126** — the shell could not find / could not execute the
///      command at all (`tsc` not installed, etc.). Unambiguous.
///   2. **Toolchain-load failures in stderr** — the command started but its
///      runtime could not load (e.g. swc's "Failed to load native binding",
///      the exact #799 dogfood case), which an exit code alone can't tell
///      apart from a real code failure. A small curated list of env/load
///      markers, NOT ambiguous app-level errors.
pub fn classify_failed_to_run(tool_name: &str, result: &str) -> Option<&'static str> {
    if tool_name != "bash" {
        return None;
    }
    let rest = result.strip_prefix("exit: ")?;
    let code = rest.split_whitespace().next().unwrap_or("");
    // (#974 QA) A command that exited 0 RAN, by definition — no fabrication is
    // possible, so the stderr scan below is moot. Short-circuit, else an honest
    // passing run whose stderr merely mentions a load-failure phrase (e.g. a
    // build script logging "optional tool: command not found, skipping") would
    // be falsely flagged — the exact trust-eroding false positive to avoid.
    if code == "0" {
        return None;
    }
    if code == "127" {
        return Some("command not found (exit 127) — the verifier never ran");
    }
    if code == "126" {
        return Some("command not executable (exit 126) — the verifier never ran");
    }
    // The command exited non-zero: distinguish "started but its toolchain
    // couldn't load" (failed-to-run) from "ran and found real errors" (honest).
    // Scan ONLY the real stderr section — always the LAST split segment, so a
    // stdout that happens to contain the separator literal can't mis-scope it.
    // Bare "command not found" is deliberately NOT a marker: exit 127 already
    // catches the shell-level case, and the bare phrase is app-reachable.
    let stderr = result.rsplit("--- stderr ---").next().unwrap_or("");
    const LOAD_FAILURE_MARKERS: &[&str] = &[
        "Failed to load native binding",
        "Cannot find native binding",
        "error while loading shared libraries",
    ];
    for m in LOAD_FAILURE_MARKERS {
        if stderr.contains(m) {
            return Some("toolchain failed to load — the verifier could not run");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── classify_outcome ────────────────────────────────────────────────

    #[test]
    fn failure_when_generic_dispatch_error() {
        let r = "tool 'read' returned error: path traversal rejected";
        assert!(!classify_outcome("read", r).tool_worked());
    }

    // (#799) classify_failed_to_run — the verifier-fabrication backstop.
    fn bash(exit: &str, stderr: &str) -> String {
        format!("exit: {exit}\n--- stdout ---\n\n--- stderr ---\n{stderr}")
    }

    #[test]
    fn failed_to_run_on_command_not_found() {
        assert!(classify_failed_to_run("bash", &bash("127", "tsc: not found")).is_some());
    }

    #[test]
    fn failed_to_run_on_not_executable() {
        assert!(classify_failed_to_run("bash", &bash("126", "")).is_some());
    }

    #[test]
    fn ran_and_failed_is_not_failed_to_run() {
        // The critical distinction: exit 1 means the verifier RAN and found a
        // real failure — that's honest, not fabrication-class. Must be None.
        assert!(classify_failed_to_run("bash", &bash("1", "3 type errors")).is_none());
        assert!(classify_failed_to_run("bash", &bash("2", "test failures")).is_none());
    }

    #[test]
    fn passing_run_is_not_failed_to_run() {
        assert!(classify_failed_to_run("bash", &bash("0", "")).is_none());
    }

    #[test]
    fn passing_run_with_load_phrase_in_stderr_is_none() {
        // (#974 QA) exit 0 = the command ran; a stderr mention of a load-failure
        // phrase must NOT flag it (honest build-script logging is common).
        let r = bash("0", "optional tool foo: command not found, skipping");
        assert!(classify_failed_to_run("bash", &r).is_none());
    }

    #[test]
    fn failed_to_run_on_native_binding_load_failure() {
        // The exact #799 dogfood case: tsc started but its toolchain (swc's
        // native binding) couldn't load — a non-zero exit indistinguishable
        // from a code failure by exit code alone, caught via the stderr marker.
        let r = bash("1", "Error: Failed to load native binding\n  at Object..");
        assert!(classify_failed_to_run("bash", &r).is_some());
    }

    #[test]
    fn failed_to_run_on_shared_library_load_failure() {
        let r = bash("127", "error while loading shared libraries: libfoo.so");
        assert!(classify_failed_to_run("bash", &r).is_some());
    }

    #[test]
    fn ambiguous_app_error_not_overflagged() {
        // A plain non-zero with an app-level message NOT in the curated load-
        // failure list must not be flagged — soft signal, keep precision high.
        let r = bash("1", "AssertionError: expected 5 got 4");
        assert!(classify_failed_to_run("bash", &r).is_none());
    }

    #[test]
    fn non_bash_tool_never_failed_to_run() {
        assert!(classify_failed_to_run("read", "exit: 127\n").is_none());
    }

    #[test]
    fn failure_when_unknown_tool_dispatch() {
        let r = "tool 'wibble' is not available in this runtime. known tools: echo, bash, ...";
        assert!(!classify_outcome("wibble", r).tool_worked());
    }

    #[test]
    fn a_bare_nonzero_exit_is_reported_and_the_tool_still_worked() {
        // (#2008) This test previously asserted the DEFECT — that any
        // non-zero exit meant the tool failed. It is inverted deliberately,
        // not deleted: the case it covers still matters, the expected answer
        // is what changed. A plain exit 1 with no never-ran marker is a
        // command that ran and reported a result.
        let r = "exit: 1\n--- stdout ---\nfoo\n--- stderr ---\nbar";
        assert_eq!(classify_outcome("bash", r), ToolOutcome::Reported { exit_code: 1 });
        assert!(
            classify_outcome("bash", r).tool_worked(),
            "the tool ran and produced output — that is work"
        );
    }

    #[test]
    fn failure_when_bash_exit_124_timeout() {
        let r = "exit: 124 (TIMED OUT after 30s)\n--- stdout ---\n\n--- stderr ---\n";
        assert!(!classify_outcome("bash", r).tool_worked());
    }

    #[test]
    fn success_when_bash_exit_zero() {
        let r = "exit: 0\n--- stdout ---\nhello\n--- stderr ---\n";
        assert!(classify_outcome("bash", r).tool_worked());
    }

    #[test]
    fn success_when_read_returns_file_content() {
        let r = "     1\tfn main() {\n     2\t    println!(\"hi\");\n     3\t}\n";
        assert!(classify_outcome("read", r).tool_worked());
    }

    #[test]
    fn success_when_search_returns_matches() {
        let r = "src/main.rs:1:fn main() {\nsrc/main.rs:2:    println!(\"hi\");\n";
        assert!(classify_outcome("search", r).tool_worked());
    }

    #[test]
    fn success_when_search_returns_empty_no_matches() {
        // Search with no matches isn't a failure — the tool ran fine
        // and reported no matches. Different from "search command
        // errored out."
        let r = "no matches\n";
        assert!(classify_outcome("search", r).tool_worked());
    }

    #[test]
    fn unrelated_text_starting_with_exit_for_non_bash_tool_is_not_failure() {
        // Only bash uses the `exit: N` convention. A `read` result
        // that happens to start with "exit: " text from a source file
        // is not a failure signal.
        let r = "exit: 42 — this is fine, just a string in the file";
        assert!(classify_outcome("read", r).tool_worked());
    }

    // ─── FailureRateDetector behavior ────────────────────────────────────

    fn err_for(tool: &str) -> String {
        format!("tool '{tool}' returned error: synthetic failure for test")
    }

    fn ok_for(tool: &str) -> String {
        match tool {
            "bash" => "exit: 0\n--- stdout ---\nok\n--- stderr ---\n".to_string(),
            _ => "fake successful result".to_string(),
        }
    }

    // Canonical arg builders so a test's cascade intent is explicit:
    // same string ⇒ same signature ⇒ a real cascade.
    fn bash_args(cmd: &str) -> String {
        format!(r#"{{"command":"{cmd}"}}"#)
    }
    fn read_args(path: &str) -> String {
        format!(r#"{{"path":"{path}"}}"#)
    }

    #[test]
    fn detector_does_not_fire_under_threshold() {
        let mut d = FailureRateDetector::with_threshold(3);
        assert!(d.record("bash", &bash_args("gcc x.c"), &err_for("bash")).is_none());
        assert!(d.record("bash", &bash_args("gcc x.c"), &err_for("bash")).is_none());
    }

    #[test]
    fn detector_fires_on_third_consecutive_failure() {
        // The #419 case: the SAME command failing repeatedly (e.g. `gcc`
        // isn't installed) — three identical signatures → cascade.
        let mut d = FailureRateDetector::with_threshold(3);
        d.record("bash", &bash_args("gcc x.c"), &err_for("bash"));
        d.record("bash", &bash_args("gcc x.c"), &err_for("bash"));
        let signal = d.record("bash", &bash_args("gcc x.c"), &err_for("bash"));
        let Some(FailureCascadeSignal::Suspected { tool_name, failure_count, .. }) = signal else {
            panic!("expected Suspected, got {signal:?}");
        };
        assert_eq!(tool_name, "bash");
        assert_eq!(failure_count, 3);
    }

    #[test]
    fn detector_does_not_fire_on_distinct_bash_commands() {
        // #484 regression: `npm test fileA`, `npm test fileB`, `npm test
        // fileC` each failing once is legitimate iterative diagnosis, not
        // a broken-tool cascade. Three distinct signatures, each count=1.
        let mut d = FailureRateDetector::with_threshold(3);
        assert!(d.record("bash", &bash_args("npm test fileA"), &err_for("bash")).is_none());
        assert!(d.record("bash", &bash_args("npm test fileB"), &err_for("bash")).is_none());
        assert!(d.record("bash", &bash_args("npm test fileC"), &err_for("bash")).is_none());
        // A fourth distinct command still doesn't cascade.
        assert!(d.record("bash", &bash_args("npm test fileD"), &err_for("bash")).is_none());
    }

    #[test]
    fn detector_edge_triggers_does_not_warn_repeatedly() {
        let mut d = FailureRateDetector::with_threshold(3);
        d.record("bash", &bash_args("gcc x.c"), &err_for("bash"));
        d.record("bash", &bash_args("gcc x.c"), &err_for("bash"));
        let first = d.record("bash", &bash_args("gcc x.c"), &err_for("bash"));
        assert!(first.is_some());
        // Same signature continuing to fail → silent (already warned)
        assert!(d.record("bash", &bash_args("gcc x.c"), &err_for("bash")).is_none());
        assert!(d.record("bash", &bash_args("gcc x.c"), &err_for("bash")).is_none());
    }

    #[test]
    fn detector_resets_counter_and_warn_on_success() {
        let mut d = FailureRateDetector::with_threshold(3);
        d.record("bash", &bash_args("gcc x.c"), &err_for("bash"));
        d.record("bash", &bash_args("gcc x.c"), &err_for("bash"));
        let first = d.record("bash", &bash_args("gcc x.c"), &err_for("bash"));
        assert!(first.is_some());
        // Success of the same signature resets — next cascade re-fires.
        d.record("bash", &bash_args("gcc x.c"), &ok_for("bash"));
        d.record("bash", &bash_args("gcc x.c"), &err_for("bash"));
        d.record("bash", &bash_args("gcc x.c"), &err_for("bash"));
        let re_fire = d.record("bash", &bash_args("gcc x.c"), &err_for("bash"));
        assert!(re_fire.is_some(), "detector must re-arm after a success");
    }

    #[test]
    fn detector_isolates_per_tool() {
        // bash fails 3×, read succeeds → bail attribution should
        // name bash specifically; read's counter never increments.
        let mut d = FailureRateDetector::with_threshold(3);
        d.record("bash", &bash_args("gcc x.c"), &err_for("bash"));
        d.record("read", &read_args("/x.ts"), &ok_for("read"));
        d.record("bash", &bash_args("gcc x.c"), &err_for("bash"));
        d.record("read", &read_args("/x.ts"), &ok_for("read"));
        let signal = d.record("bash", &bash_args("gcc x.c"), &err_for("bash"));
        let Some(FailureCascadeSignal::Suspected { tool_name, .. }) = signal else {
            panic!("expected bash cascade, got {signal:?}");
        };
        assert_eq!(tool_name, "bash");
    }

    #[test]
    fn detector_fires_for_each_tool_independently() {
        let mut d = FailureRateDetector::with_threshold(3);
        // Three bash failures of the same command → fire for bash
        d.record("bash", &bash_args("gcc x.c"), &err_for("bash"));
        d.record("bash", &bash_args("gcc x.c"), &err_for("bash"));
        let bash_fire = d.record("bash", &bash_args("gcc x.c"), &err_for("bash"));
        assert!(matches!(bash_fire, Some(FailureCascadeSignal::Suspected { ref tool_name, .. }) if tool_name == "bash"));

        // Three read failures of the same path → fire for read independently
        d.record("read", &read_args("/x.ts"), &err_for("read"));
        d.record("read", &read_args("/x.ts"), &err_for("read"));
        let read_fire = d.record("read", &read_args("/x.ts"), &err_for("read"));
        assert!(matches!(read_fire, Some(FailureCascadeSignal::Suspected { ref tool_name, .. }) if tool_name == "read"));
    }

    #[test]
    fn detector_intermittent_failures_dont_accumulate_across_successes() {
        // Failure - success - failure - failure pattern should NOT
        // trip the cascade (counter resets on success).
        let mut d = FailureRateDetector::with_threshold(3);
        d.record("bash", &bash_args("gcc x.c"), &err_for("bash"));
        d.record("bash", &bash_args("gcc x.c"), &ok_for("bash"));    // resets
        d.record("bash", &bash_args("gcc x.c"), &err_for("bash"));
        d.record("bash", &bash_args("gcc x.c"), &err_for("bash"));
        // Only 2 consecutive failures since the last success.
        assert!(d.record("bash", &bash_args("gcc x.c"), &ok_for("bash")).is_none(), "intermittent should not trip");
    }

    #[test]
    fn red_tests_never_cascade_however_many_times_they_run() {
        // (#2008) The regression this whole change exists to prevent. Before
        // it, three `npm test` runs reporting failing tests fired a cascade
        // whose nudge told the model `bash` had failed and to switch tools.
        let mut d = FailureRateDetector::with_threshold(3);
        let red = "exit: 1\n--- stdout ---\nTests: 2 failed, 86 passed\n--- stderr ---\n";
        for _ in 0..6 {
            assert!(
                d.record("bash", &bash_args("npm test"), red).is_none(),
                "a red test must never cascade — the tool is working"
            );
        }
    }

    #[test]
    fn genuine_failures_still_cascade() {
        // The other half: the detector's founding case must survive the fix.
        let mut d = FailureRateDetector::with_threshold(3);
        let missing = "exit: 127\n--- stdout ---\n--- stderr ---\nbash: gcc: command not found\n";
        d.record("bash", &bash_args("gcc x.c"), missing);
        d.record("bash", &bash_args("gcc x.c"), missing);
        assert!(
            d.record("bash", &bash_args("gcc x.c"), missing).is_some(),
            "a command that never ran must still cascade"
        );
    }

    #[test]
    fn a_red_test_is_reported_not_failed() {
        // (#2008) THE case. A failing test in TDD's red phase is the tool
        // doing its job. Classifying it as a failure told the model its test
        // runner was broken and denied the watchdogs their proof-of-work.
        let red = "exit: 1\n--- stdout ---\nTests: 2 failed, 86 passed\n--- stderr ---\n";
        assert_eq!(
            classify_outcome("bash", red),
            ToolOutcome::Reported { exit_code: 1 },
            "a command that ran and reported non-zero is Reported, never Failed"
        );
        assert!(
            classify_outcome("bash", red).tool_worked(),
            "the TOOL worked — this is what the `ok` field has always documented itself as meaning"
        );
    }

    #[test]
    fn a_command_that_never_ran_is_failed() {
        // Exit 127 — the founding case of this detector (a `gcc` that was not
        // installed). Must stay Failed, or the cascade loses its real job.
        let missing = "exit: 127\n--- stdout ---\n--- stderr ---\nbash: gcc: command not found\n";
        let out = classify_outcome("bash", missing);
        assert!(matches!(out, ToolOutcome::Failed { .. }), "got {out:?}");
        assert!(!out.tool_worked());
    }

    #[test]
    fn a_timeout_is_failed_not_reported() {
        // The branch that is easy to lose: `classify_failed_to_run` returns
        // None for 124, so deriving Failed from it ALONE would silently make
        // every timeout Reported and drop genuine timeout cascades.
        let timed_out = "exit: 124 (TIMED OUT after 30s)\n--- stdout ---\n\n--- stderr ---\n";
        let out = classify_outcome("bash", timed_out);
        assert!(
            matches!(out, ToolOutcome::Failed { .. }),
            "a timed-out command did not complete: {out:?}"
        );
        assert!(
            classify_failed_to_run("bash", timed_out).is_none(),
            "guard: if classify_failed_to_run ever starts catching 124, this test \
             stops proving the timeout branch is load-bearing"
        );
    }

    #[test]
    fn a_user_command_exiting_124_on_its_own_is_still_reported() {
        // #905 gates the marker on the wrapper actually having run, so a
        // command that genuinely chooses exit 124 is not mislabeled a timeout.
        let own = "exit: 124\n--- stdout ---\nmy tool uses 124 for 'nothing to do'\n--- stderr ---\n";
        assert_eq!(classify_outcome("bash", own), ToolOutcome::Reported { exit_code: 124 });
    }

    #[test]
    fn a_tool_level_error_is_failed_for_any_tool() {
        let err = "tool 'edit' returned error: old_string not found";
        let out = classify_outcome("edit", err);
        assert!(matches!(out, ToolOutcome::Failed { .. }), "got {out:?}");
        assert_eq!(classify_outcome("bash", "exit: 0\n--- stdout ---\nok\n"), ToolOutcome::Ok);
    }

    #[test]
    fn detector_handles_bash_timeout_as_failure() {
        let mut d = FailureRateDetector::with_threshold(3);
        let timeout = "exit: 124 (TIMED OUT after 30s)\n--- stdout ---\n\n--- stderr ---\n";
        d.record("bash", &bash_args("sleep 99"), timeout);
        d.record("bash", &bash_args("sleep 99"), timeout);
        let signal = d.record("bash", &bash_args("sleep 99"), timeout);
        assert!(signal.is_some(), "bash timeout (exit 124) must count as failure");
    }

    #[test]
    fn detector_default_construction() {
        let d = FailureRateDetector::default();
        assert_eq!(d.threshold, DEFAULT_WARN_THRESHOLD);
    }
}
