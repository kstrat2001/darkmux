//! CLI dispatcher for `darkmux flow` shortcut verbs.

use crate::flow;
use crate::flow::{Category, FlowRecord, Level, Stage, Tier};
use anyhow::{Context, Result};
use clap::Subcommand;


/// Top-level `flow` subcommand enum.
#[derive(Subcommand)]
pub enum FlowCmd {
    /// Record an operator-narrative observation.
    Note {
        #[arg(long)]
        text: String,
        /// Optional sprint identifier.
        #[arg(long = "sprint-id")]
        sprint_id: Option<String>,
        /// Optional session identifier.
        #[arg(long = "session-id")]
        session_id: Option<String>,
        /// Optional source label.
        #[arg(long)]
        source: Option<String>,
    },
    /// Record an operator-flagged catch / mid-stream observation.
    Catch {
        #[arg(long)]
        text: String,
        /// Optional sprint identifier.
        #[arg(long = "sprint-id")]
        sprint_id: Option<String>,
        /// Optional session identifier.
        #[arg(long = "session-id")]
        session_id: Option<String>,
        /// Optional source label.
        #[arg(long)]
        source: Option<String>,
    },
    /// Record a raw flow event — all six fields explicit from flags.
    Record {
        #[arg(long)]
        level: Level,
        #[arg(long)]
        category: Category,
        #[arg(long)]
        tier: Tier,
        #[arg(long)]
        stage: Stage,
        #[arg(long)]
        action: String,
        #[arg(long)]
        handle: String,
        /// Optional sprint identifier.
        #[arg(long = "sprint-id")]
        sprint_id: Option<String>,
        /// Optional session identifier.
        #[arg(long = "session-id")]
        session_id: Option<String>,
        /// Optional source label.
        #[arg(long)]
        source: Option<String>,
        /// Optional operator-supplied reasoning. The audit substrate's
        /// WHY layer for events emitted via this raw verb.
        #[arg(long)]
        reasoning: Option<String>,
        /// Optional mission identifier this event is scoped to.
        #[arg(long = "mission-id")]
        mission_id: Option<String>,
    },
    /// Record a tier-decision — the frontier orchestrator's reasoning for
    /// routing a piece of work to local vs. holding in frontier (#136).
    ///
    /// Tier-decision records form the audit substrate's *why* layer.
    /// Where dispatch records show *what* ran, tier-decision records
    /// show *why this layer was chosen* — the missing provenance step
    /// for compliance-bearing AI orchestration.
    ///
    /// Typical use: the frontier orchestrator runs this verb before
    /// dispatching (or before deciding to hold work in frontier) and
    /// captures the reasoning in operator-readable prose.
    #[command(name = "tier-decision")]
    TierDecision {
        /// `dispatch` (work routed to local) or `direct` (work held in
        /// frontier). Free-form, but those two are the conventional values.
        #[arg(long)]
        decision: String,
        /// Operator-readable rationale. The prose that future audit will
        /// read to understand *why* this routing was chosen. Required —
        /// a tier-decision record without reasoning is just a dispatch.
        #[arg(long)]
        reasoning: String,
        /// Optional role chosen (when `decision=dispatch`). E.g., `coder`,
        /// `trip-researcher`. Captured in the `handle` field.
        #[arg(long = "role-chosen")]
        role_chosen: Option<String>,
        /// Optional sprint identifier this decision is scoped to.
        #[arg(long = "sprint-id")]
        sprint_id: Option<String>,
        /// Optional mission identifier this decision is scoped to.
        #[arg(long = "mission-id")]
        mission_id: Option<String>,
        /// Optional session identifier (when the decision links to an
        /// already-dispatched session — e.g., recorded after the fact).
        #[arg(long = "session-id")]
        session_id: Option<String>,
        /// Optional source label (e.g., `frontier-claude`, `operator-manual`).
        #[arg(long)]
        source: Option<String>,
    },
}

pub fn run(cmd: FlowCmd) -> Result<()> {
    let record = build_record(cmd);
    flow::record(record).context("writing flow record")
}

pub fn build_record(cmd: FlowCmd) -> FlowRecord {
    let ts = flow::ts_utc_now();
    match cmd {
        FlowCmd::Note { text, sprint_id, session_id, source } => FlowRecord {
            ts,
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "note".to_string(),
            handle: text,
            sprint_id,
            session_id,
            source,
            model: None,
            reasoning: None,
            mission_id: None,
        },
        FlowCmd::Catch { text, sprint_id, session_id, source } => FlowRecord {
            ts,
            level: Level::Warn,
            category: Category::Audit,
            tier: Tier::Operator,
            stage: Stage::Review,
            action: "catch".to_string(),
            handle: text,
            sprint_id,
            session_id,
            source,
            model: None,
            reasoning: None,
            mission_id: None,
        },
        FlowCmd::Record {
            level,
            category,
            tier,
            stage,
            action,
            handle,
            sprint_id,
            session_id,
            source,
            reasoning,
            mission_id,
        } => FlowRecord {
            ts,
            level,
            category,
            tier,
            stage,
            action,
            handle,
            sprint_id,
            session_id,
            source,
            model: None,
            reasoning,
            mission_id,
        },
        FlowCmd::TierDecision {
            decision,
            reasoning,
            role_chosen,
            sprint_id,
            mission_id,
            session_id,
            source,
        } => FlowRecord {
            ts,
            level: Level::Info,
            category: Category::Audit,
            // The frontier orchestrator is the tier doing the routing, so
            // its decisions are frontier-tier records. Even when the
            // decision routes work TO local, the act of deciding lives at
            // frontier.
            tier: Tier::Frontier,
            stage: Stage::TierDecision,
            // `action` is the operator-facing event name; `handle` carries
            // role-chosen for searchability. When no role is chosen
            // (decision=direct), handle is the decision itself.
            action: "tier-decision".to_string(),
            handle: role_chosen.clone().unwrap_or_else(|| decision.clone()),
            sprint_id,
            session_id,
            source,
            model: None,
            reasoning: Some(format!("[{decision}] {reasoning}")),
            mission_id,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow::{Category, Level, Stage, Tier};
    use serde_json::Value;
    use std::env;
    use std::path::PathBuf;
    use tempfile::TempDir;

    struct FlowsDirGuard {
        prev: Option<String>,
        tmp: TempDir,
    }

    impl FlowsDirGuard {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let prev = env::var("DARKMUX_FLOWS_DIR").ok();
            // SAFETY: serialized via `#[serial_test::serial]` on every test
            // that mutates this env var.
            unsafe { env::set_var("DARKMUX_FLOWS_DIR", tmp.path()); }
            Self { prev, tmp }
        }

        fn path(&self) -> &std::path::Path {
            self.tmp.path()
        }
    }

    impl Drop for FlowsDirGuard {
        fn drop(&mut self) {
            // SAFETY: serialized via the test attribute.
            unsafe {
                match &self.prev {
                    Some(v) => env::set_var("DARKMUX_FLOWS_DIR", v),
                    None => env::remove_var("DARKMUX_FLOWS_DIR"),
                }
            }
        }
    }

    /// Read every `.jsonl` file under the guard's temp dir and return them
    /// sorted by filename. Midnight-UTC-safe: if records straddle UTC midnight
    /// they end up in two files; callers either expect exactly one (single-call
    /// tests) or sum across files (multi-call tests).
    fn jsonl_files(guard: &FlowsDirGuard) -> Vec<PathBuf> {
        let mut paths: Vec<_> = std::fs::read_dir(guard.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
            .collect();
        paths.sort();
        paths
    }

    /// Return all non-header record lines across every day file. Used by
    /// single-call tests; asserts there's exactly one record total (regardless
    /// of how many day files).
    fn single_record(guard: &FlowsDirGuard) -> Value {
        let files = jsonl_files(guard);
        let lines: Vec<String> = files
            .iter()
            .flat_map(|p| std::fs::read_to_string(p).unwrap().lines().map(String::from).collect::<Vec<_>>())
            .collect();
        // header(s) + 1 record. With 1 day file: 2 lines. With 2 (midnight): 3.
        assert!(lines.len() == 2 || lines.len() == 3, "unexpected line count: {}", lines.len());
        let records: Vec<&String> = lines
            .iter()
            .filter(|l| !l.contains("\"_type\":\"schema\""))
            .collect();
        assert_eq!(records.len(), 1, "expected exactly one record line");
        serde_json::from_str(records[0]).unwrap()
    }

    #[serial_test::serial]
    #[test]
    fn note_writes_record_with_operator_tier_and_info_level() {
        let guard = FlowsDirGuard::new();
        run(FlowCmd::Note {
            text: "hello".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
        })
        .unwrap();

        let rec = single_record(&guard);
        assert_eq!(rec["tier"], "operator");
        assert_eq!(rec["level"], "info");
        assert_eq!(rec["category"], "work");
        assert_eq!(rec["action"], "note");
        assert_eq!(rec["handle"], "hello");
    }

    #[serial_test::serial]
    #[test]
    fn catch_writes_record_with_warn_level_and_audit_category() {
        let guard = FlowsDirGuard::new();
        run(FlowCmd::Catch {
            text: "oops".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
        })
        .unwrap();

        let rec = single_record(&guard);
        assert_eq!(rec["level"], "warn");
        assert_eq!(rec["category"], "audit");
    }

    #[serial_test::serial]
    #[test]
    fn record_passes_through_all_flags() {
        let guard = FlowsDirGuard::new();
        run(FlowCmd::Record {
            level: Level::Error,
            category: Category::Machinery,
            tier: Tier::Local,
            stage: Stage::Dispatch,
            action: "x".to_string(),
            handle: "y".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
            reasoning: None,
            mission_id: None,
        })
        .unwrap();

        let rec = single_record(&guard);
        assert_eq!(rec["level"], "error");
        assert_eq!(rec["category"], "machinery");
        assert_eq!(rec["tier"], "local");
        assert_eq!(rec["stage"], "dispatch");
        assert_eq!(rec["action"], "x");
        assert_eq!(rec["handle"], "y");
    }

    #[serial_test::serial]
    #[test]
    fn record_threads_optional_fields_when_provided() {
        let guard = FlowsDirGuard::new();
        run(FlowCmd::Record {
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "test-optional".to_string(),
            handle: "opt-handle".to_string(),
            sprint_id: Some("66".to_string()),
            session_id: Some("abc".to_string()),
            source: Some("manual".to_string()),
            reasoning: None,
            mission_id: None,
        })
        .unwrap();

        let rec = single_record(&guard);
        assert_eq!(rec["sprint_id"], "66");
        assert_eq!(rec["session_id"], "abc");
        assert_eq!(rec["source"], "manual");
    }

    #[serial_test::serial]
    #[test]
    fn multiple_calls_append_to_same_day_file() {
        let guard = FlowsDirGuard::new();

        run(FlowCmd::Note { text: "a".into(), sprint_id: None, session_id: None, source: None }).unwrap();
        run(FlowCmd::Note { text: "b".into(), sprint_id: None, session_id: None, source: None }).unwrap();
        run(FlowCmd::Note { text: "c".into(), sprint_id: None, session_id: None, source: None }).unwrap();

        // Sum non-schema lines across however many day files the calls
        // produced (one in steady state; two if straddling UTC midnight).
        let files = jsonl_files(&guard);
        let total_records: usize = files
            .iter()
            .map(|p| {
                std::fs::read_to_string(p)
                    .unwrap()
                    .lines()
                    .filter(|l| !l.contains("\"_type\":\"schema\""))
                    .count()
            })
            .sum();
        assert_eq!(total_records, 3);
    }

    #[serial_test::serial]
    #[test]
    fn tier_decision_dispatch_records_role_and_reasoning() {
        let guard = FlowsDirGuard::new();

        run(FlowCmd::TierDecision {
            decision: "dispatch".into(),
            reasoning: "Bounded mechanical translation; testable via cargo test".into(),
            role_chosen: Some("coder".into()),
            sprint_id: Some("113-s1".into()),
            mission_id: Some("113-mission-propose-pipeline".into()),
            session_id: None,
            source: Some("frontier-claude".into()),
        })
        .unwrap();

        let rec = single_record(&guard);
        assert_eq!(rec["category"], "audit");
        assert_eq!(rec["tier"], "frontier");
        assert_eq!(rec["stage"], "tier-decision");
        assert_eq!(rec["action"], "tier-decision");
        // handle carries role-chosen when dispatch + role known.
        assert_eq!(rec["handle"], "coder");
        assert_eq!(rec["sprint_id"], "113-s1");
        assert_eq!(rec["mission_id"], "113-mission-propose-pipeline");
        assert_eq!(rec["source"], "frontier-claude");
        // reasoning carries the decision prefix + the operator's prose.
        let reasoning = rec["reasoning"].as_str().unwrap();
        assert!(reasoning.starts_with("[dispatch] "), "got: {reasoning}");
        assert!(reasoning.contains("Bounded mechanical"), "got: {reasoning}");
    }

    #[serial_test::serial]
    #[test]
    fn tier_decision_direct_records_decision_as_handle_when_no_role() {
        let guard = FlowsDirGuard::new();

        run(FlowCmd::TierDecision {
            decision: "direct".into(),
            reasoning: "Multi-variable holding, tone-critical; no testable threshold".into(),
            role_chosen: None,
            sprint_id: Some("japan-day-3".into()),
            mission_id: Some("japan-trip-2026-may".into()),
            session_id: None,
            source: None,
        })
        .unwrap();

        let rec = single_record(&guard);
        assert_eq!(rec["stage"], "tier-decision");
        // No role_chosen → handle falls back to the decision value.
        assert_eq!(rec["handle"], "direct");
        let reasoning = rec["reasoning"].as_str().unwrap();
        assert!(reasoning.starts_with("[direct] "), "got: {reasoning}");
        assert!(reasoning.contains("Multi-variable holding"), "got: {reasoning}");
    }
}
