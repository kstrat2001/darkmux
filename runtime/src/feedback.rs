//! Per-dispatch feedback injection — routes runtime telemetry signals
//! (cycle warnings, tool-failure cascades, future signals) INTO the
//! model's next-turn prompt as synthetic system messages.
//!
//! ## Why
//!
//! The runtime already computes meta-signals about the model's
//! behavior (E13 cycle-detector, #419 failure-rate detector, post-
//! compaction events, budget-approach warnings). Pre-this-primitive
//! those signals were emitted **only** to operator stderr + trajectory
//! events — the MODEL itself never saw them. Operators (or frontier-
//! tier supervisors) could observe a local-AI dispatch spinning in a
//! diagnostic loop, but the model had no awareness it was stuck.
//!
//! The architectural pattern is established by frontier-tier coding
//! harnesses (Claude Code's `<system-reminder>` injection, etc.):
//! wrap telemetry in a synthetic system message, inject into next
//! prompt, model gets meta-awareness it can't compute on its own.
//! This primitive brings the same pattern to the local-AI runtime.
//!
//! ## Phase boundary
//!
//! **Step 1 (this scaffold)**: hardcoded generic templates per signal
//! kind. Producer-side primitive only. Operator-disable via the
//! `DARKMUX_FEEDBACK_INJECTION` env var.
//!
//! **Step 2 (queued)**: per-role `feedback_templates` field on the
//! Role manifest, operator-overridable at `~/.darkmux/roles/<id>.json`.
//! Templates support placeholder substitution (`{tool_name}`,
//! `{count}`, `{window_size}`). The producer-side primitive doesn't
//! change shape; only the template-resolution path moves from
//! hardcoded constants into the role lookup.
//!
//! **Step 3 (queued)**: generalize signal producers — wire compaction
//! events, budget-approach signals, etc. into the same injector.
//!
//! ## Pattern parallel
//!
//! Mirrors PR #441's `STALL_NUDGE_MESSAGE` injection: pushes a
//! `Message::system()` prefixed with `[darkmux-runtime]` so the model
//! recognizes the message as runtime telemetry, not user input.
//! Difference: stall recovery is an INTRA-TURN intervention (drops
//! the useless turn + nudges immediately), while feedback injection
//! is an INTER-TURN intervention (queues at signal-fire, drains at
//! top of next loop iteration).
//!
//! **May co-occur with `STALL_NUDGE_MESSAGE`** — if a stall is
//! recovered AND cycle/cascade fired in the same window, the next
//! prompt may carry two back-to-back `[darkmux-runtime]`-prefixed
//! system messages. Both are model-facing telemetry, both are
//! prefixed identically, both inform the next decision — safe to
//! stack. The model reads each independently as runtime context.

use crate::lmstudio::Message;
use std::collections::BTreeMap;

/// (#457 Step 2) Default template for the cycle-suspected signal —
/// used when no per-role override is supplied. Placeholders
/// `{tool_name}`, `{count}`, `{window_size}` substituted at injection.
const DEFAULT_CYCLE_SUSPECTED_TEMPLATE: &str =
    "[darkmux-runtime] You've called `{tool_name}` {count} times in \
     {window_size} turns with the same arguments. Regroup and \
     change the strategy. If no productive next step is available, \
     stop and summarize what you have so the operator can review.";

/// (#457 Step 2) Default template for the tool-failure-cascade signal.
/// Placeholders `{tool_name}`, `{count}` substituted at injection.
const DEFAULT_TOOL_FAILURE_CASCADE_TEMPLATE: &str =
    "[darkmux-runtime] `{tool_name}` has failed {count} \
     times in a row with the same call pattern. Re-read the affected \
     files or state, then change the inputs. If the tool itself is \
     wrong for the next step, switch tools. If none of those apply, \
     stop and summarize what you have so the operator can review.";

/// (#457 Step 3) Default template for the post-compaction signal —
/// fires after a successful compaction event. Placeholder `{turn}`
/// is the current turn number when the compaction completed.
///
/// Compaction compresses the prior conversation into a synthetic
/// summary; the model's working state changes shape. Re-orientation
/// is the stall-prevention move: pick the smallest concrete next
/// step toward the goal and do that, rather than re-reading
/// everything to rebuild context (the Beat 45 retrace pattern).
const DEFAULT_POST_COMPACTION_TEMPLATE: &str =
    "[darkmux-runtime] Working memory was just compressed (compaction \
     fired after turn {turn}). Re-orient: identify the smallest \
     concrete next step toward the goal, and do that. Do not re-read \
     files you already have summarized state for; the summary is \
     load-bearing.";

/// (#465) Default template for the test-cadence-drift signal —
/// fires when N CONSECUTIVE edits have hit the SAME FILE without an
/// intervening `bash` verification call. Placeholders `{path}` and
/// `{count}` substituted at injection.
///
/// Beat 54 N=5 surfaced that the prior "edits-since-last-bash COUNT"
/// detector mis-fired on productive multi-file work and missed
/// genuine drift. The redesign discriminates by REPETITION on a
/// single file — that pattern correlates with thrash-on-single-bug
/// behavior that should pause for verification.
const DEFAULT_TEST_CADENCE_DRIFT_TEMPLATE: &str =
    "[darkmux-runtime] You've edited `{path}` {count} times in a row \
     without running a verification command (bash). Repeated edits to \
     the same file without verification usually mean an earlier change \
     didn't land the way you expected. If your instructions name test \
     or lint commands, run them now. Otherwise re-read the file before \
     stacking another edit.";

/// Per-dispatch state — queue of pending feedback messages to inject
/// at the top of the next loop iteration.
///
/// Construction reads `DARKMUX_FEEDBACK_INJECTION` once; when set to
/// `0`/`off`/`false`/`no`, `queue_*` is a no-op and `drain` returns
/// empty. Default (env var unset or any other value) is enabled.
///
/// (#457 Step 2) Accepts a per-role template override map at
/// construction. Keys are signal-kind names (`cycle_suspected`,
/// `tool_failure_cascade`); values are template strings with
/// `{placeholder}` substitution. Missing keys fall back to the
/// hardcoded defaults shipped in PR #455. Empty map = all defaults.
pub struct FeedbackInjector {
    pending: Vec<String>,
    /// (#457 Step 3) Parallel vec to `pending` — each queued message
    /// also pushes its signal-kind name here. Allows the trajectory
    /// event to discriminate per-kind instead of Step 1's combined
    /// `cycle_or_cascade` bucket. Drained alongside `pending`.
    pending_kinds: Vec<&'static str>,
    /// (#457 Step 3) Snapshot of the kinds from the most recent
    /// `drain()` call. Caller reads via `last_drained_kinds()`
    /// AFTER drain — pattern that keeps the existing
    /// `drain() -> Vec<Message>` signature stable across the API
    /// expansion.
    drained_kinds: Vec<&'static str>,
    enabled: bool,
    templates: BTreeMap<String, String>,
}

impl FeedbackInjector {
    pub fn new() -> Self {
        Self::with_templates(BTreeMap::new())
    }

    /// (#457 Step 2) Construct with per-role template overrides.
    /// The dispatcher serializes `Role.feedback_templates` to JSON
    /// and passes via `--feedback-templates-json` CLI flag; the
    /// runtime parses and supplies the map here. Keys with no
    /// matching producer (typos, future-only signal kinds) are
    /// stored but never looked up — harmless.
    pub fn with_templates(templates: BTreeMap<String, String>) -> Self {
        let enabled = std::env::var("DARKMUX_FEEDBACK_INJECTION")
            .map(|v| !matches!(v.as_str(), "0" | "off" | "false" | "no"))
            .unwrap_or(true);
        Self {
            pending: Vec::new(),
            pending_kinds: Vec::new(),
            drained_kinds: Vec::new(),
            enabled,
            templates,
        }
    }

    /// (#457 Step 3) Returns the signal-kind names that were drained
    /// by the most recent call to `drain()`. Caller reads after drain
    /// to populate the trajectory event's `signal_kinds` field with
    /// per-signal discrimination instead of Step 1's combined bucket.
    pub fn last_drained_kinds(&self) -> &[&'static str] {
        &self.drained_kinds
    }

    /// Resolve a template by signal-kind key — operator override
    /// wins; built-in default is the fallback. Substitution caller's
    /// responsibility (placeholder vocab differs per signal).
    fn template_for(&self, key: &str) -> &str {
        self.templates
            .get(key)
            .map(String::as_str)
            .unwrap_or_else(|| match key {
                "cycle_suspected" => DEFAULT_CYCLE_SUSPECTED_TEMPLATE,
                "tool_failure_cascade" => DEFAULT_TOOL_FAILURE_CASCADE_TEMPLATE,
                "post_compaction" => DEFAULT_POST_COMPACTION_TEMPLATE,
                "test_cadence_drift" => DEFAULT_TEST_CADENCE_DRIFT_TEMPLATE,
                _ => "",
            })
    }

    /// True if `DARKMUX_FEEDBACK_INJECTION` is enabled. Public so
    /// consumers (loop runner today, doctor / observability surfaces
    /// in future steps) can decide whether to surface
    /// feedback-injection state. Currently used in tests only; the
    /// loop runner relies on `drain()` returning empty when disabled.
    #[allow(dead_code)]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Queue a cycle-suspected feedback message. Called by the loop
    /// runner alongside the existing `eprintln!` + trajectory event
    /// when the cycle detector fires. The message will be injected
    /// into the model's next-turn prompt as a system-role message.
    ///
    /// Wording uses imperative verbs and concrete nouns ("regroup
    /// and change the strategy") rather than colloquial phrases
    /// ("try a different angle"). Words with ambiguous interpretation
    /// in technical contexts (angle, approach, lens) risk being read
    /// as domain references rather than figurative directives.
    ///
    /// Step-1 template is terse on purpose — one template serves
    /// every role until Step 2 ships per-role customization.
    pub fn queue_cycle_suspected(
        &mut self,
        tool_name: &str,
        count: usize,
        window_size: usize,
    ) {
        if !self.enabled {
            return;
        }
        let template = self.template_for("cycle_suspected");
        let message = template
            .replace("{tool_name}", tool_name)
            .replace("{count}", &count.to_string())
            .replace("{window_size}", &window_size.to_string());
        self.pending.push(message);
        self.pending_kinds.push("cycle_suspected");
    }

    /// Queue a tool-failure-cascade feedback message. Called when the
    /// failure-rate detector fires (e.g., 3 consecutive failures of
    /// the same tool).
    ///
    /// **Wording — directive and literal** (same doctrine as the
    /// cycle template). Imperative verbs ("re-read", "change",
    /// "stop") and concrete next actions. Beat 51 observed an
    /// `edit`-cascade where the operator-stale `old_string` was
    /// the root cause; calling out "re-read affected files" first
    /// targets that common case before listing the broader fallbacks.
    pub fn queue_tool_failure_cascade(
        &mut self,
        tool_name: &str,
        consecutive_failures: usize,
    ) {
        if !self.enabled {
            return;
        }
        let template = self.template_for("tool_failure_cascade");
        let message = template
            .replace("{tool_name}", tool_name)
            .replace("{count}", &consecutive_failures.to_string());
        self.pending.push(message);
        self.pending_kinds.push("tool_failure_cascade");
    }

    /// (#457 Step 3) Queue a post-compaction nudge. Called by the
    /// loop runner immediately after a compaction event lands —
    /// model's working state was just compressed; the nudge orients
    /// it toward the smallest concrete next step rather than the
    /// Beat 45 retrace pattern (re-read everything to rebuild context).
    pub fn queue_post_compaction(&mut self, turn: u32) {
        if !self.enabled {
            return;
        }
        let template = self.template_for("post_compaction");
        let message = template.replace("{turn}", &turn.to_string());
        self.pending.push(message);
        self.pending_kinds.push("post_compaction");
    }

    /// (#465) Queue a test-cadence-drift nudge. Called by the loop
    /// runner when N CONSECUTIVE edits hit the SAME `path` without an
    /// intervening `bash` tool call. Edge-triggered upstream: the
    /// loop runner fires once per threshold crossing, then resets its
    /// counter.
    ///
    /// Pre-#465 this took a count of "edits since last bash" across
    /// any files; that mis-fired on productive multi-file work (Beat
    /// 54 N=5). Same-file repetition is the discriminator that
    /// correlates with single-bug thrash patterns.
    pub fn queue_test_cadence_drift(&mut self, consecutive_count: u32, path: &str) {
        if !self.enabled {
            return;
        }
        let template = self.template_for("test_cadence_drift");
        let message = template
            .replace("{count}", &consecutive_count.to_string())
            .replace("{path}", path);
        self.pending.push(message);
        self.pending_kinds.push("test_cadence_drift");
    }

    /// Number of pending messages waiting to be drained. Tests use
    /// this to verify queue state; loop runner uses `drain()`'s
    /// returned `Vec::len()` instead (cheaper than two calls).
    #[allow(dead_code)]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Drain all pending messages, returning them as ready-to-append
    /// `Message::system()` instances. Resets the queue. Call at the
    /// top of each loop iteration; if the queue is empty, returns an
    /// empty Vec (no message injected, no work done).
    ///
    /// (#457 Step 3) Also moves `pending_kinds` into `drained_kinds`
    /// so the caller can read which signal kinds were drained via
    /// `last_drained_kinds()`. API for the existing `drain() -> Vec<Message>`
    /// caller surface unchanged.
    pub fn drain(&mut self) -> Vec<Message> {
        if !self.enabled {
            self.pending.clear();
            self.pending_kinds.clear();
            self.drained_kinds.clear();
            return Vec::new();
        }
        self.drained_kinds = std::mem::take(&mut self.pending_kinds);
        self.pending.drain(..).map(Message::system).collect()
    }
}

impl Default for FeedbackInjector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Env-var-mutating tests are marked `#[serial_test::serial]` per
    // the project convention (see `loop_runner.rs` tests). serial_test
    // serializes all `#[serial]` tests project-wide so the global env
    // var state isn't raced. Tests that don't mutate env vars don't
    // need the attribute.

    #[test]
    #[serial_test::serial]
    fn new_starts_with_empty_queue() {
        std::env::remove_var("DARKMUX_FEEDBACK_INJECTION");
        let f = FeedbackInjector::new();
        assert_eq!(f.pending_count(), 0);
        assert!(f.enabled());
    }

    #[test]
    #[serial_test::serial]
    fn queue_cycle_suspected_grows_queue() {
        std::env::remove_var("DARKMUX_FEEDBACK_INJECTION");
        let mut f = FeedbackInjector::new();
        f.queue_cycle_suspected("read", 3, 10);
        assert_eq!(f.pending_count(), 1);
    }

    #[test]
    #[serial_test::serial]
    fn queue_tool_failure_cascade_grows_queue() {
        std::env::remove_var("DARKMUX_FEEDBACK_INJECTION");
        let mut f = FeedbackInjector::new();
        f.queue_tool_failure_cascade("edit", 3);
        assert_eq!(f.pending_count(), 1);
    }

    #[test]
    #[serial_test::serial]
    fn drain_returns_messages_and_clears_queue() {
        std::env::remove_var("DARKMUX_FEEDBACK_INJECTION");
        let mut f = FeedbackInjector::new();
        f.queue_cycle_suspected("read", 3, 10);
        f.queue_tool_failure_cascade("bash", 3);
        let msgs = f.drain();
        assert_eq!(msgs.len(), 2);
        assert_eq!(f.pending_count(), 0);
        // Drained messages must be system-role for the model to read
        // them as runtime telemetry rather than user instruction.
        for m in &msgs {
            assert_eq!(m.role, "system", "feedback must inject as system role");
        }
    }

    #[test]
    #[serial_test::serial]
    fn drain_returns_empty_when_no_pending() {
        std::env::remove_var("DARKMUX_FEEDBACK_INJECTION");
        let mut f = FeedbackInjector::new();
        let msgs = f.drain();
        assert!(msgs.is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn cycle_message_includes_tool_name_count_and_window() {
        std::env::remove_var("DARKMUX_FEEDBACK_INJECTION");
        let mut f = FeedbackInjector::new();
        f.queue_cycle_suspected("read", 5, 12);
        let msgs = f.drain();
        let content = msgs[0].content.as_ref().expect("system message has content");
        assert!(content.contains("`read`"), "should name the tool: {content}");
        assert!(content.contains("5 times"), "should name the count: {content}");
        assert!(content.contains("12 turns"), "should name the window: {content}");
    }

    #[test]
    #[serial_test::serial]
    fn cycle_message_has_runtime_prefix() {
        std::env::remove_var("DARKMUX_FEEDBACK_INJECTION");
        let mut f = FeedbackInjector::new();
        f.queue_cycle_suspected("read", 3, 10);
        let msgs = f.drain();
        let content = msgs[0].content.as_ref().expect("system message has content");
        assert!(
            content.starts_with("[darkmux-runtime]"),
            "must prefix as runtime telemetry so the model recognizes the \
             message is not user input: {content}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn failure_message_includes_tool_name_and_count() {
        std::env::remove_var("DARKMUX_FEEDBACK_INJECTION");
        let mut f = FeedbackInjector::new();
        f.queue_tool_failure_cascade("edit", 3);
        let msgs = f.drain();
        let content = msgs[0].content.as_ref().expect("system message has content");
        assert!(content.contains("`edit`"), "should name the tool: {content}");
        assert!(content.contains("3 times"), "should name the count: {content}");
    }

    #[test]
    #[serial_test::serial]
    fn disabled_via_env_var_makes_queue_noop() {
        std::env::set_var("DARKMUX_FEEDBACK_INJECTION", "0");
        let mut f = FeedbackInjector::new();
        assert!(!f.enabled(), "env var `0` must disable");
        f.queue_cycle_suspected("read", 3, 10);
        f.queue_tool_failure_cascade("edit", 3);
        assert_eq!(f.pending_count(), 0, "queuing must be no-op when disabled");
        let msgs = f.drain();
        assert!(msgs.is_empty(), "drain must be empty when disabled");
        std::env::remove_var("DARKMUX_FEEDBACK_INJECTION");
    }

    #[test]
    #[serial_test::serial]
    fn disabled_via_env_var_off() {
        std::env::set_var("DARKMUX_FEEDBACK_INJECTION", "off");
        let f = FeedbackInjector::new();
        assert!(!f.enabled());
        std::env::remove_var("DARKMUX_FEEDBACK_INJECTION");
    }

    #[test]
    #[serial_test::serial]
    fn disabled_via_env_var_false() {
        std::env::set_var("DARKMUX_FEEDBACK_INJECTION", "false");
        let f = FeedbackInjector::new();
        assert!(!f.enabled());
        std::env::remove_var("DARKMUX_FEEDBACK_INJECTION");
    }

    #[test]
    #[serial_test::serial]
    fn unrecognized_env_var_value_treats_as_enabled() {
        std::env::set_var("DARKMUX_FEEDBACK_INJECTION", "yes-please");
        let f = FeedbackInjector::new();
        assert!(
            f.enabled(),
            "any non-disable-keyword value must keep the feature enabled \
             (defensive: an operator typo shouldn't silently disable the feature)"
        );
        std::env::remove_var("DARKMUX_FEEDBACK_INJECTION");
    }

    #[test]
    #[serial_test::serial]
    fn multiple_drains_independent() {
        std::env::remove_var("DARKMUX_FEEDBACK_INJECTION");
        let mut f = FeedbackInjector::new();
        f.queue_cycle_suspected("read", 3, 10);
        let first = f.drain();
        assert_eq!(first.len(), 1);
        // Second drain on an emptied injector is a no-op.
        let second = f.drain();
        assert!(second.is_empty());
        // After re-queue, drain works again — verifies the queue
        // isn't somehow permanently drained.
        f.queue_tool_failure_cascade("bash", 3);
        let third = f.drain();
        assert_eq!(third.len(), 1);
    }

    // ─── (#465) test-cadence-drift v2: same-file repetition semantics ────

    #[test]
    #[serial_test::serial]
    fn cadence_drift_message_includes_path_and_count() {
        std::env::remove_var("DARKMUX_FEEDBACK_INJECTION");
        let mut f = FeedbackInjector::new();
        f.queue_test_cadence_drift(3, "/workspace/src/main.rs");
        let msgs = f.drain();
        let content = msgs[0].content.as_ref().expect("system message has content");
        assert!(
            content.contains("`/workspace/src/main.rs`"),
            "must name the repeatedly-edited path so the model knows which file: {content}"
        );
        assert!(
            content.contains("3 times"),
            "must name the repeat count so the model has the right calibration: {content}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn cadence_drift_message_has_runtime_prefix() {
        std::env::remove_var("DARKMUX_FEEDBACK_INJECTION");
        let mut f = FeedbackInjector::new();
        f.queue_test_cadence_drift(3, "/workspace/x.rs");
        let msgs = f.drain();
        let content = msgs[0].content.as_ref().expect("system message has content");
        assert!(
            content.starts_with("[darkmux-runtime]"),
            "runtime telemetry prefix is the boundary marker that tells the model \
             this is system-internal, not user input: {content}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn cadence_drift_per_role_override_substitutes_both_placeholders() {
        std::env::remove_var("DARKMUX_FEEDBACK_INJECTION");
        let mut templates = BTreeMap::new();
        templates.insert(
            "test_cadence_drift".to_string(),
            "[darkmux-runtime] Override: {path} edited {count} times. \
             Stop and verify before editing again."
                .to_string(),
        );
        let mut f = FeedbackInjector::with_templates(templates);
        f.queue_test_cadence_drift(4, "/workspace/lib.rs");
        let msgs = f.drain();
        let content = msgs[0].content.as_ref().expect("system message has content");
        assert!(content.contains("/workspace/lib.rs"), "{content}");
        assert!(content.contains("4 times"), "{content}");
        assert!(content.starts_with("[darkmux-runtime] Override:"), "{content}");
    }
}
