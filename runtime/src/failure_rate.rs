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
        if is_failure_result(tool_name, result) {
            let count = self.counters.entry(sig.clone()).or_insert(0);
            *count += 1;
            let count_now = *count;
            if count_now >= self.threshold && !self.warned.contains(&sig) {
                self.warned.insert(sig);
                return Some(FailureCascadeSignal::Suspected {
                    tool_name: tool_name.to_string(),
                    failure_count: count_now,
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

/// (#419) Classify a tool-result string as success or failure.
///
/// Centralized so the detector doesn't carry per-tool branching
/// internally. The error-shape contract is owned by
/// `runtime/src/tools/mod.rs::dispatch` (generic) and the per-tool
/// `execute_*` functions (specific). When that contract changes,
/// update this function to match.
///
/// **Tool-surface invariant**: all non-bash tools (read / write /
/// edit / search) route their failure cases through `Tool::execute`'s
/// `Err` return, which the dispatcher wraps as `"tool 'NAME' returned
/// error: ..."`. That's why this function only needs the generic
/// marker + the bash-specific exit-code parse. Verified against
/// `runtime/src/tools/mod.rs::execute_{read,write,edit,search}` as
/// of 2026-05-27. If a future tool returns `Ok(error-shaped-text)`
/// (i.e., reports failure without using the Err path), add its
/// pattern here.
pub fn is_failure_result(tool_name: &str, result: &str) -> bool {
    // Generic dispatch wrapper for any Err returned by Tool::execute.
    let generic_marker = format!("tool '{tool_name}' returned error:");
    if result.starts_with(&generic_marker) {
        return true;
    }
    // Unknown-tool path from the dispatcher.
    let unknown_marker = format!("tool '{tool_name}' is not available");
    if result.starts_with(&unknown_marker) {
        return true;
    }
    // Bash-specific: exit code in the first line. Anything other than
    // `exit: 0` is a failure (includes timeout exit 124, real
    // command errors, signals, etc.).
    if tool_name == "bash" {
        if let Some(rest) = result.strip_prefix("exit: ") {
            // `rest` looks like "N\n--- stdout ---..." or "N (TIMED OUT...)...".
            // `split_whitespace` already handles the trailing newline / parens.
            let code_token = rest.split_whitespace().next().unwrap_or("");
            return code_token != "0";
        }
    }
    false
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

    // ─── is_failure_result ───────────────────────────────────────────────

    #[test]
    fn failure_when_generic_dispatch_error() {
        let r = "tool 'read' returned error: path traversal rejected";
        assert!(is_failure_result("read", r));
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
        assert!(is_failure_result("wibble", r));
    }

    #[test]
    fn failure_when_bash_exit_nonzero() {
        let r = "exit: 1\n--- stdout ---\nfoo\n--- stderr ---\nbar";
        assert!(is_failure_result("bash", r));
    }

    #[test]
    fn failure_when_bash_exit_124_timeout() {
        let r = "exit: 124 (TIMED OUT after 30s)\n--- stdout ---\n\n--- stderr ---\n";
        assert!(is_failure_result("bash", r));
    }

    #[test]
    fn success_when_bash_exit_zero() {
        let r = "exit: 0\n--- stdout ---\nhello\n--- stderr ---\n";
        assert!(!is_failure_result("bash", r));
    }

    #[test]
    fn success_when_read_returns_file_content() {
        let r = "     1\tfn main() {\n     2\t    println!(\"hi\");\n     3\t}\n";
        assert!(!is_failure_result("read", r));
    }

    #[test]
    fn success_when_search_returns_matches() {
        let r = "src/main.rs:1:fn main() {\nsrc/main.rs:2:    println!(\"hi\");\n";
        assert!(!is_failure_result("search", r));
    }

    #[test]
    fn success_when_search_returns_empty_no_matches() {
        // Search with no matches isn't a failure — the tool ran fine
        // and reported no matches. Different from "search command
        // errored out."
        let r = "no matches\n";
        assert!(!is_failure_result("search", r));
    }

    #[test]
    fn unrelated_text_starting_with_exit_for_non_bash_tool_is_not_failure() {
        // Only bash uses the `exit: N` convention. A `read` result
        // that happens to start with "exit: " text from a source file
        // is not a failure signal.
        let r = "exit: 42 — this is fine, just a string in the file";
        assert!(!is_failure_result("read", r));
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
        let Some(FailureCascadeSignal::Suspected { tool_name, failure_count }) = signal else {
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
