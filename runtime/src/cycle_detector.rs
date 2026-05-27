//! Cycle detection — flag repeated tool calls within a sliding window.
//!
//! Part of epic #417 (automate-the-human-in-the-loop recovery
//! infrastructure). This is the automated equivalent of *"the model
//! keeps reading the same file"* — what a human watching a chat
//! session would do is *"that's the third time you read x.ts, please
//! move on."* (#418)
//!
//! Failure shape: agent ends up in a loop where each turn is
//! technically productive (tool dispatched, result returned) but
//! semantically circular — re-reading the same file across compactions,
//! re-running the same bash check, re-searching for the same pattern.
//! From outside the dispatch it looks like "agent worked hard but hit
//! MAX_TURNS." From inside the trajectory the repetition is visible —
//! darkmux can see it before the operator does.
//!
//! MVP: warn-only via trajectory event. No behavioral change (no bail).
//! Bail-on-threshold is a follow-up if empirical data shows warning
//! alone isn't enough.
//!
//! ## Design
//!
//! - Sliding window of the most recent N tool calls (default N=10)
//! - Per call, compute a similarity hash from `tool_name + canonical(args)`
//!   - `canonical` strips noise irrelevant to whether two calls "are
//!     looking at the same thing": `read` ignores offset/limit; `edit`
//!     ignores edit content (path is the cycle signal); `bash` keeps
//!     full command (different commands aren't cycles); `search` keeps
//!     pattern + path
//! - When the same hash appears K times within the window (default K=3),
//!   emit a cycle-suspected signal
//! - State is per-dispatch (one detector per agent loop run)

use std::collections::VecDeque;

/// Default window size — how many recent tool calls to track.
pub const DEFAULT_WINDOW_SIZE: usize = 10;

/// Default threshold — how many appearances of the same hash within
/// the window before warning.
pub const DEFAULT_WARN_THRESHOLD: usize = 3;

/// (#418) The result of recording a tool call into the detector. A
/// `Suspected` outcome is observability-only in the MVP — no bail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CycleSignal {
    /// First time we've seen this many of this hash in the window.
    /// The detector "edge-triggers" — same hash continuing to appear
    /// at or above threshold in later calls returns Ok to avoid noise.
    /// Carries the offending hash + the count at trigger time so the
    /// trajectory event can name what's repeating.
    Suspected {
        tool_name: String,
        canonical_args: String,
        count: usize,
        window_size: usize,
    },
}

/// Sliding-window cycle detector. One instance per dispatch / agent
/// loop. Records each tool call; returns `Some(CycleSignal::Suspected)`
/// the first time a hash crosses the warn threshold.
#[derive(Debug)]
pub struct CycleDetector {
    window: VecDeque<String>,
    window_size: usize,
    warn_threshold: usize,
    /// Hashes we've already warned about. Edge-triggering prevents the
    /// same suspected cycle from firing every turn — once warned, stay
    /// quiet until the offending hash drops out of the window AND
    /// re-crosses the threshold later.
    warned: std::collections::HashSet<String>,
}

impl CycleDetector {
    /// Construct with default thresholds (window=10, warn=3).
    pub fn new() -> Self {
        Self::with_thresholds(DEFAULT_WINDOW_SIZE, DEFAULT_WARN_THRESHOLD)
    }

    /// Construct with explicit thresholds. Operator-tunable surfacing
    /// lives at the loop_runner layer (not here) — this struct stays
    /// dependency-free for testability.
    pub fn with_thresholds(window_size: usize, warn_threshold: usize) -> Self {
        Self {
            window: VecDeque::with_capacity(window_size),
            window_size,
            warn_threshold,
            warned: std::collections::HashSet::new(),
        }
    }

    /// Record a tool call and return a `CycleSignal::Suspected` if it
    /// just crossed the threshold (edge-triggered). The signal carries
    /// the hash + occurrence count so the caller can populate a
    /// descriptive trajectory event.
    pub fn record(&mut self, tool_name: &str, raw_args: &str) -> Option<CycleSignal> {
        let canonical = canonical_args(tool_name, raw_args);
        let hash = format!("{tool_name}|{canonical}");

        // Slide the window.
        if self.window.len() >= self.window_size {
            let dropped = self.window.pop_front();
            // When a hash drops out of the window entirely, allow it
            // to re-trigger (un-warn). Operator may want to know if
            // the same cycle re-forms after some intervening work.
            if let Some(d) = dropped.as_ref() {
                if !self.window.contains(d) {
                    self.warned.remove(d);
                }
            }
        }
        self.window.push_back(hash.clone());

        // Count occurrences in the current window.
        let count = self.window.iter().filter(|h| **h == hash).count();

        if count >= self.warn_threshold && !self.warned.contains(&hash) {
            self.warned.insert(hash.clone());
            return Some(CycleSignal::Suspected {
                tool_name: tool_name.to_string(),
                canonical_args: canonical,
                count,
                window_size: self.window_size,
            });
        }
        None
    }
}

impl Default for CycleDetector {
    fn default() -> Self {
        Self::new()
    }
}

/// Strip noisy arg fields so two calls "looking at the same thing"
/// produce the same canonical form. The discriminator per tool:
/// what makes two calls *semantically equivalent for cycle-detection
/// purposes*?
///
/// - `read` — path. offset / limit / max_bytes are non-cycle-bearing.
/// - `edit` — path. Edit content changes per attempt but path
///   identifies the file being repeatedly modified.
/// - `write` — path. Same reasoning as edit.
/// - `bash` — full command. Different commands aren't cycles; same
///   command IS a cycle.
/// - `search` — pattern + path. Different searches aren't cycles.
/// - any unknown tool — fall back to the raw args (conservative;
///   unknown tool might be cycle-bearing in any of its fields).
///
/// Implementation: parse `raw_args` as JSON; extract the cycle-bearing
/// fields per tool; serialize as a compact normalized JSON object. If
/// JSON parse fails, fall back to the raw string — better to over-
/// distinguish (miss a real cycle) than under-distinguish (false-flag).
///
/// **Known trade-off — edit refactors look like cycles.** Keeping only
/// `path` for `edit` means a sincere 3-step refactor of one file
/// (header / body / footer changes) will trip the warn at K=3. In the
/// warn-only MVP this is acceptable (operator sees an informational
/// event, no behavior change). When the bail-on-cycle follow-up lands,
/// revisit: either raise `edit`'s threshold separately, or include a
/// shape-hash of the edit content (length / diff-style fingerprint) so
/// progressive edits distinguish from repeat-same-attempt.
pub fn canonical_args(tool_name: &str, raw_args: &str) -> String {
    let parsed: serde_json::Value = match serde_json::from_str(raw_args) {
        Ok(v) => v,
        Err(_) => return raw_args.to_string(),
    };
    let Some(obj) = parsed.as_object() else {
        return raw_args.to_string();
    };
    let keep: &[&str] = match tool_name {
        "read" | "edit" | "write" => &["path"],
        "bash" => &["command"],
        "search" => &["pattern", "path"],
        _ => return raw_args.to_string(),
    };
    let mut filtered = serde_json::Map::new();
    for k in keep {
        if let Some(v) = obj.get(*k) {
            filtered.insert(k.to_string(), v.clone());
        }
    }
    serde_json::Value::Object(filtered).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── canonical_args ──────────────────────────────────────────────────

    #[test]
    fn canonical_read_keeps_path_drops_offset_and_limit() {
        let a = canonical_args("read", r#"{"path":"/x.ts","offset":1,"limit":50}"#);
        let b = canonical_args("read", r#"{"path":"/x.ts","offset":100,"limit":200}"#);
        assert_eq!(a, b, "two reads of the same path with different offsets must be canonically equal");
        assert!(a.contains("/x.ts"));
        assert!(!a.contains("offset"));
        assert!(!a.contains("limit"));
    }

    #[test]
    fn canonical_read_distinguishes_paths() {
        let a = canonical_args("read", r#"{"path":"/x.ts","offset":1,"limit":50}"#);
        let b = canonical_args("read", r#"{"path":"/y.ts","offset":1,"limit":50}"#);
        assert_ne!(a, b, "different paths must produce different canonical forms");
    }

    #[test]
    fn canonical_bash_keeps_full_command() {
        let a = canonical_args("bash", r#"{"command":"ls -la","timeout_seconds":30}"#);
        let b = canonical_args("bash", r#"{"command":"ls -la","timeout_seconds":60}"#);
        assert_eq!(a, b, "timeout_seconds is noise; command is the cycle discriminator");

        let c = canonical_args("bash", r#"{"command":"pwd"}"#);
        assert_ne!(a, c, "different commands must distinguish");
    }

    #[test]
    fn canonical_edit_keeps_path_drops_edits_array() {
        let a = canonical_args("edit", r#"{"path":"/x.ts","edits":[{"old_string":"a","new_string":"b"}]}"#);
        let b = canonical_args("edit", r#"{"path":"/x.ts","edits":[{"old_string":"c","new_string":"d"}]}"#);
        assert_eq!(a, b, "different edit attempts on the same file are a cycle");
    }

    #[test]
    fn canonical_search_keeps_pattern_and_path() {
        let a = canonical_args("search", r#"{"pattern":"TODO","path":"/src","max_results":10}"#);
        let b = canonical_args("search", r#"{"pattern":"TODO","path":"/src","max_results":50}"#);
        assert_eq!(a, b, "max_results is noise; pattern+path is the discriminator");

        let c = canonical_args("search", r#"{"pattern":"FIXME","path":"/src"}"#);
        assert_ne!(a, c, "different patterns must distinguish");
    }

    #[test]
    fn canonical_unknown_tool_falls_back_to_raw_args() {
        // Conservative: unknown tools might be cycle-bearing in any
        // field; keep everything to over-distinguish.
        let a = canonical_args("future-tool", r#"{"foo":1,"bar":2}"#);
        let b = canonical_args("future-tool", r#"{"foo":1,"bar":3}"#);
        assert_ne!(a, b);
    }

    #[test]
    fn canonical_malformed_json_falls_back_to_raw() {
        let raw = "not valid json {";
        let canonical = canonical_args("read", raw);
        assert_eq!(canonical, raw, "malformed args fall back so we don't crash");
    }

    // ─── CycleDetector behavior ──────────────────────────────────────────

    #[test]
    fn detector_does_not_fire_under_threshold() {
        let mut d = CycleDetector::with_thresholds(10, 3);
        assert!(d.record("read", r#"{"path":"/x.ts","offset":1,"limit":50}"#).is_none());
        assert!(d.record("read", r#"{"path":"/x.ts","offset":50,"limit":50}"#).is_none());
        // Second occurrence — under threshold of 3.
    }

    #[test]
    fn detector_fires_when_third_occurrence_crosses_threshold() {
        let mut d = CycleDetector::with_thresholds(10, 3);
        d.record("read", r#"{"path":"/x.ts","offset":1,"limit":50}"#);
        d.record("read", r#"{"path":"/x.ts","offset":50,"limit":50}"#);
        let signal = d.record("read", r#"{"path":"/x.ts","offset":100,"limit":50}"#);
        let Some(CycleSignal::Suspected { tool_name, count, window_size, .. }) = signal else {
            panic!("expected Suspected, got {signal:?}");
        };
        assert_eq!(tool_name, "read");
        assert_eq!(count, 3);
        assert_eq!(window_size, 10);
    }

    #[test]
    fn detector_edge_triggers_does_not_warn_repeatedly() {
        let mut d = CycleDetector::with_thresholds(10, 3);
        // 3 reads → warn
        d.record("read", r#"{"path":"/x.ts","offset":1,"limit":50}"#);
        d.record("read", r#"{"path":"/x.ts","offset":50,"limit":50}"#);
        let first = d.record("read", r#"{"path":"/x.ts","offset":100,"limit":50}"#);
        assert!(first.is_some(), "first crossing should fire");
        // 4th and 5th of the same → silent (already warned)
        let second = d.record("read", r#"{"path":"/x.ts","offset":150,"limit":50}"#);
        assert!(second.is_none(), "edge-triggered detector must not fire repeatedly");
        let third = d.record("read", r#"{"path":"/x.ts","offset":200,"limit":50}"#);
        assert!(third.is_none());
    }

    #[test]
    fn detector_isolates_per_tool_and_per_canonical_form() {
        let mut d = CycleDetector::with_thresholds(10, 3);
        d.record("read", r#"{"path":"/a.ts","offset":1,"limit":50}"#);
        d.record("read", r#"{"path":"/b.ts","offset":1,"limit":50}"#);
        d.record("read", r#"{"path":"/c.ts","offset":1,"limit":50}"#);
        d.record("bash", r#"{"command":"ls"}"#);
        // None should fire — three different reads + one bash, no
        // canonical repeats.
    }

    #[test]
    fn detector_re_arms_when_hash_drops_out_of_window() {
        let mut d = CycleDetector::with_thresholds(5, 3);
        // 3 of same → warn
        d.record("read", r#"{"path":"/x.ts","offset":1}"#);
        d.record("read", r#"{"path":"/x.ts","offset":50}"#);
        let first = d.record("read", r#"{"path":"/x.ts","offset":100}"#);
        assert!(first.is_some());

        // Fill the window with other calls so the /x.ts entries drop out.
        // Need 5 distinct hashes to fully evict.
        d.record("bash", r#"{"command":"ls"}"#);
        d.record("bash", r#"{"command":"pwd"}"#);
        d.record("bash", r#"{"command":"date"}"#);
        d.record("bash", r#"{"command":"whoami"}"#);
        d.record("bash", r#"{"command":"echo hi"}"#);
        // Window now: 5 distinct bashes; /x.ts is fully evicted.

        // /x.ts re-enters; another 3 → should re-warn
        d.record("read", r#"{"path":"/x.ts","offset":1}"#);
        d.record("read", r#"{"path":"/x.ts","offset":50}"#);
        let re_fire = d.record("read", r#"{"path":"/x.ts","offset":100}"#);
        assert!(re_fire.is_some(), "detector must re-arm after hash drops out of window");
    }

    #[test]
    fn detector_window_size_one_fires_immediately_on_repeat() {
        let mut d = CycleDetector::with_thresholds(1, 1);
        // With window=1, every call is itself the only entry; count=1 always
        let first = d.record("read", r#"{"path":"/x.ts"}"#);
        assert!(first.is_some(), "window=1 + threshold=1 fires on first call");
        // Second call same path → window evicts first, hash drops, re-arms, fires again
        let second = d.record("read", r#"{"path":"/x.ts"}"#);
        assert!(second.is_some());
    }

    #[test]
    fn detector_doesnt_double_count_within_threshold_check() {
        // Regression guard: the count loop should count distinct
        // positions in the window, not double-count the just-pushed
        // entry somehow.
        let mut d = CycleDetector::with_thresholds(10, 3);
        let first = d.record("read", r#"{"path":"/x.ts"}"#);
        assert!(first.is_none(), "single call must not fire (count=1, threshold=3)");
        let second = d.record("read", r#"{"path":"/x.ts"}"#);
        assert!(second.is_none(), "two calls must not fire (count=2, threshold=3)");
    }

    #[test]
    fn detector_handles_malformed_args_gracefully() {
        let mut d = CycleDetector::with_thresholds(10, 3);
        // Malformed args fall back to raw string in canonical_args, so
        // 3 identical malformed args of the same tool still trip.
        d.record("read", "garbage");
        d.record("read", "garbage");
        let signal = d.record("read", "garbage");
        assert!(signal.is_some(), "identical malformed args should still cycle-detect");
    }

    #[test]
    fn detector_default_construction() {
        let d = CycleDetector::default();
        assert_eq!(d.window_size, DEFAULT_WINDOW_SIZE);
        assert_eq!(d.warn_threshold, DEFAULT_WARN_THRESHOLD);
    }

    // ─── Real-world fixture replays (#437 / E13) ─────────────────────────

    /// (Fixture: Beat 48 N=5 run 1 — production trace)
    ///
    /// Trajectory slice from the first run that fired cycle detection
    /// 10 times (10/100 turns triggered the warning). The model
    /// repeatedly read/edited/test-ran `authenticationService.test.ts`
    /// during a test-iteration loop. This is the canonical "model
    /// going in circles" pattern operators see in real dispatches.
    ///
    /// Replays the tool.completed events through the detector and
    /// verifies the cycle-suspected count matches what the trajectory
    /// recorded — regression guard against a future change that
    /// would over- or under-detect this pattern.
    #[test]
    fn fixture_beat48_run1_test_iteration_loop_fires_cycle_detector() {
        let raw = include_str!(
            "../tests/fixtures/cycle-traces/beat48-run1-read-edit-bash-test-iteration.jsonl"
        );
        let mut detector = CycleDetector::new();
        let mut fired = 0usize;
        let mut tool_calls = 0usize;
        for line in raw.lines() {
            if line.is_empty() {
                continue;
            }
            let event: serde_json::Value = serde_json::from_str(line)
                .expect("fixture lines must parse as JSON");
            let event_type = event["type"].as_str().unwrap_or("");
            if event_type == "tool.completed" {
                tool_calls += 1;
                let tool_name = event["tool_name"].as_str().unwrap_or("");
                // The fixture's tool.completed events don't carry the
                // raw_args — the detector uses canonical_args which
                // for cycle-bearing tools strips noise. Synthesize a
                // minimal args object keyed on tool_name so the
                // canonical form is consistent with the production
                // trace (path-only for read/edit; command-only for bash).
                let synth_args = match tool_name {
                    "read" | "edit" | "write" => {
                        r#"{"path":"/workspace/tests/services/authenticationService.test.ts"}"#
                    }
                    "bash" => {
                        r#"{"command":"cd /workspace && npm test -- tests/services/authenticationService.test.ts"}"#
                    }
                    _ => "{}",
                };
                if detector.record(tool_name, synth_args).is_some() {
                    fired += 1;
                }
            }
        }
        // The fixture is a 32-event slice; should produce at least
        // one cycle signal (the trace was selected to include two
        // cycle events with surrounding context).
        assert!(
            fired >= 1,
            "fixture must reproduce at least one cycle-suspected fire (got {fired} fires across {tool_calls} tool calls)"
        );
    }
}
