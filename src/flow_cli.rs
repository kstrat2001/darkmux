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
        /// Optional phase identifier.
        #[arg(long = "phase-id")]
        phase_id: Option<String>,
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
        /// Optional phase identifier.
        #[arg(long = "phase-id")]
        phase_id: Option<String>,
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
        /// Optional phase identifier.
        #[arg(long = "phase-id")]
        phase_id: Option<String>,
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
        /// Optional phase identifier this decision is scoped to.
        #[arg(long = "phase-id")]
        phase_id: Option<String>,
        /// Optional mission identifier this decision is scoped to.
        #[arg(long = "mission-id")]
        mission_id: Option<String>,
        /// Optional session identifier (when the decision links to an
        /// already-dispatched session — e.g., recorded after the fact).
        #[arg(long = "session-id")]
        session_id: Option<String>,
        /// Optional source label (e.g., `frontier`, `operator-manual`).
        #[arg(long)]
        source: Option<String>,
    },
    /// Print a diagnostic snapshot of the flow substrate (sinks, Redis
    /// health, disk health, schema state). The store-status pill in
    /// the shared shell polls this via the daemon's `/flow-status`
    /// endpoint; the verb is also useful standalone for operators
    /// debugging substrate problems.
    Status {
        /// Emit machine-readable JSON instead of the human-formatted
        /// summary. The daemon's `/flow-status` endpoint also returns
        /// this shape so the shell pill and the CLI share one format.
        #[arg(long)]
        json: bool,
    },
    /// Walk every audit file under `DARKMUX_AUDIT_DIR` (or the default
    /// `~/.darkmux/audit/`), recompute the hash chain, and report the
    /// first divergence per file. Compliance verb (#163): proves the
    /// AuditFileSink output hasn't been edited since write. Exits with
    /// status 2 when any chain is broken so CI/cron can flag tampering.
    #[command(name = "integrity-check")]
    IntegrityCheck {
        /// Restrict the walk to a single file path. Useful when the
        /// operator just wants to check one day's audit log rather
        /// than the entire directory.
        #[arg(long)]
        path: Option<std::path::PathBuf>,
        /// Emit machine-readable JSON instead of the human-formatted
        /// summary.
        #[arg(long)]
        json: bool,
    },
    /// Tail flow records, optionally filtered to one session, following new
    /// appends live (like `tail -f`). Ctrl-C to stop.
    #[command(name = "tail")]
    Tail {
        /// Only show records for this session id.
        #[arg(long = "session")]
        session: Option<String>,
        /// Emit raw JSON lines instead of a formatted one-line summary.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(cmd: FlowCmd) -> Result<()> {
    // Read verbs are intercepted ahead of build_record so the latter
    // only sees write verbs.
    match cmd {
        FlowCmd::Status { json } => return print_status(json),
        FlowCmd::IntegrityCheck { path, json } => return print_integrity_check(path, json),
        FlowCmd::Tail { session, json } => return run_tail(session.as_deref(), json),
        _ => {}
    }
    let record = build_record(cmd);
    flow::record(record).context("writing flow record")
}

/// Render `darkmux flow status` to stdout. Calls `flow::collect_status()`
/// for the snapshot; format gated by `--json`.
fn print_status(json: bool) -> Result<()> {
    let status = flow::collect_status();
    if json {
        // (#776) machine-readable: force color off (defense-in-depth).
        darkmux_types::style::set_colorize_override(Some(false));
        let s = serde_json::to_string_pretty(&status)
            .context("serializing FlowStatus to JSON")?;
        println!("{s}");
    } else {
        print!("{}", flow::format_status_human(&status));
    }
    Ok(())
}

/// Render `darkmux flow integrity-check` to stdout. Walks the audit dir
/// (or a single `--path`), recomputes each file's hash chain, reports
/// pass/break per file. Exits with status 2 when any chain is broken so
/// CI / cron / monitoring can flag tampering.
fn print_integrity_check(path: Option<std::path::PathBuf>, json: bool) -> Result<()> {
    let reports = if let Some(p) = path {
        vec![flow::integrity_check_file(&p)?]
    } else {
        flow::integrity_check_all()?
    };

    use darkmux_types::style;
    if json {
        // (#776) machine-readable: force color off (defense-in-depth).
        style::set_colorize_override(Some(false));
        let s = serde_json::to_string_pretty(&reports)
            .context("serializing integrity reports to JSON")?;
        println!("{s}");
    } else if reports.is_empty() {
        println!(
            "{}",
            style::dim(&format!(
                "darkmux flow integrity-check — no audit files under {}",
                flow::audit_dir().display()
            ))
        );
    } else {
        for r in &reports {
            // Status token isn't column-padded here, so coloring it directly
            // is alignment-safe.
            let status = if r.chain_valid {
                style::success("✓ valid")
            } else {
                style::error("✗ BROKEN")
            };
            println!(
                "{status}  {}  {}",
                r.path,
                style::dim(&format!("({} record(s))", r.records_checked))
            );
            if !r.chain_valid {
                if let Some(line) = r.break_at_line {
                    println!("{}", style::error(&format!("       chain break at line {line}")));
                }
                if let Some(reason) = r.break_reason.as_ref() {
                    println!("{}", style::error(&format!("       reason: {reason}")));
                }
            }
        }
    }

    if reports.iter().any(|r| !r.chain_valid) {
        std::process::exit(2);
    }
    Ok(())
}

/// Filter a single JSONL line for tail output.
///
/// Returns `Some(string)` when the line should be printed, `None` otherwise.
/// When `session` is `Some(s)`, only records whose `session_id` equals `s`
/// are returned. When `json` is true, the raw line is returned; otherwise
/// a concise one-line summary is built from available fields.
fn tail_match(line: &str, session: Option<&str>, json: bool) -> Option<String> {
    let parsed = match serde_json::from_str::<serde_json::Value>(line) {
        Ok(v) => v,
        Err(_) => return None, // unparseable line — skip
    };

    if let Some(s) = session {
        if parsed.get("session_id").and_then(|v| v.as_str()) != Some(s) {
            return None;
        }
    }

    if json {
        Some(line.to_string())
    } else {
        use darkmux_types::style;
        let ts = parsed.get("ts").and_then(|v| v.as_str()).unwrap_or("");
        let action = parsed.get("action").and_then(|v| v.as_str()).unwrap_or("-");
        let handle = parsed.get("handle").and_then(|v| v.as_str()).unwrap_or("-");
        let session_id = parsed
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("-");
        // Space-separated (not column-padded) → coloring is alignment-safe.
        Some(format!(
            "{} {} {} {}",
            style::dim(ts),
            style::accent(action),
            handle,
            style::dim(session_id)
        ))
    }
}

/// Run `darkmux flow tail`: read today's JSONL file, then follow new appends
/// until interrupted (Ctrl-C / SIGINT — default signal handler).
pub fn run_tail(session: Option<&str>, json: bool) -> anyhow::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::thread;
    use std::time::Duration;

    // (#776) When emitting raw JSON lines, force color off (defense-in-depth;
    // tail_match returns the verbatim line under --json, but a future styled
    // path must not leak ANSI into a piped consumer).
    if json {
        darkmux_types::style::set_colorize_override(Some(false));
    }

    let flows_dir = flow::flows_dir();
    // Track which day file we're tailing + our byte offset into it. The day is
    // recomputed each tick so a tail running across UTC midnight follows the new
    // day's `<date>.jsonl` instead of going silent (#695; same rollover the
    // viewer handles per #730). First iteration: day == "" → reads from offset 0.
    let mut day = String::new();
    let mut offset: u64 = 0;

    loop {
        let today: String = flow::ts_utc_now().chars().take(10).collect();
        if today != day {
            day = today;
            offset = 0; // new day file — start from the top
        }
        let today_file = flows_dir.join(format!("{day}.jsonl"));

        if let Ok(mut f) = std::fs::File::open(&today_file) {
            if let Ok(meta) = f.metadata() {
                let current_len = meta.len();
                if current_len > offset {
                    let mut buf = Vec::new();
                    if f.seek(SeekFrom::Start(offset)).is_ok() && f.read_to_end(&mut buf).is_ok() {
                        offset = current_len;
                        let content = String::from_utf8_lossy(&buf);
                        for line in content.lines() {
                            if let Some(s) = tail_match(line, session, json) {
                                println!("{s}");
                            }
                        }
                        let _ = std::io::stdout().flush();
                    }
                } else if current_len < offset {
                    // File shrank/rotated under us — restart from the top.
                    offset = 0;
                }
            }
        }
        // (If the file doesn't exist yet, just keep polling until it appears.)

        thread::sleep(Duration::from_millis(500));
    }
}

pub fn build_record(cmd: FlowCmd) -> FlowRecord {
    let ts = flow::ts_utc_now();
    match cmd {
        FlowCmd::Note { text, phase_id, session_id, source } => FlowRecord {
            ts,
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "note".to_string(),
            handle: text,
            phase_id,
            session_id,
            source,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        },
        FlowCmd::Catch { text, phase_id, session_id, source } => FlowRecord {
            ts,
            level: Level::Warn,
            category: Category::Audit,
            tier: Tier::Operator,
            stage: Stage::Review,
            action: "catch".to_string(),
            handle: text,
            phase_id,
            session_id,
            source,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        },
        FlowCmd::Record {
            level,
            category,
            tier,
            stage,
            action,
            handle,
            phase_id,
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
            phase_id,
            session_id,
            source,
            model: None,
            reasoning,
            mission_id,
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        },
        FlowCmd::TierDecision {
            decision,
            reasoning,
            role_chosen,
            phase_id,
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
            phase_id,
            session_id,
            source,
            model: None,
            reasoning: Some(format!("[{decision}] {reasoning}")),
            mission_id,
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        },
        // Read verbs are intercepted by `run` before build_record.
        // Reaching here would mean run() was bypassed; assert loudly.
        FlowCmd::Status { .. } => unreachable!(
            "FlowCmd::Status is a read verb and must be handled by run() before build_record"
        ),
        FlowCmd::IntegrityCheck { .. } => unreachable!(
            "FlowCmd::IntegrityCheck is a read verb and must be handled by run() before build_record"
        ),
        FlowCmd::Tail { .. } => unreachable!(
            "FlowCmd::Tail is a read verb and must be handled by run() before build_record"
        ),
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

    /// (#776) The `--json` handlers must force colorize OFF so machine-readable
    /// output can never carry ANSI — even when stdout is a TTY (e.g. `--json`
    /// piped to a pager, or run interactively for debugging). We simulate the
    /// TTY case by forcing colorize ON, then assert the `--json` path flips it
    /// back off. `#[serial]` because the override is process-global.
    #[serial_test::serial]
    #[test]
    fn flow_json_paths_force_colorize_off() {
        use darkmux_types::style;
        // Pretend stdout is a color-capable TTY.
        style::set_colorize_override(Some(true));
        assert!(style::colorize_enabled(), "precondition: forced on");
        // The --json status path must disable color (defense-in-depth belt).
        let _ = print_status(true);
        assert!(
            !style::colorize_enabled(),
            "`flow status --json` must force colorize OFF so the envelope stays ANSI-free"
        );
        // And the rendered helpers must now produce plain text.
        assert!(!style::accent("x").contains("\u{1b}["));

        // `flow tail --json` returns each line verbatim — assert the json arm
        // is ANSI-free even with color forced ON (covers the tail belt's
        // intent without entering run_tail's infinite follow-loop). The
        // fleet `emit_json` + integrity-check belts share this one-line
        // pattern but can't be unit-called safely (network probe /
        // `process::exit(2)` respectively) — they're covered by review.
        style::set_colorize_override(Some(true));
        let line = r#"{"ts":"t","action":"a","handle":"h","session_id":"s"}"#;
        let out = tail_match(line, None, true).expect("json line passes the filter");
        assert!(
            !out.contains('\u{1b}'),
            "`flow tail --json` lines must be ANSI-free, got: {out:?}"
        );

        style::set_colorize_override(None); // restore auto-detect
    }

    /// Isolates the flow-write env vars so a test runs against a clean
    /// flows-dir AND doesn't inherit the operator's daily-shell
    /// `DARKMUX_REDIS_URL` / `DARKMUX_AUDIT_DIR` (which would route
    /// flow records to a possibly-unreachable Redis or to the
    /// operator's real audit log). Pre-#278, an operator with their
    /// daily Redis URL exported saw flow tests run 75s/record while
    /// the connect-timeout wedged; even with the timeout fix landed,
    /// flow records were still being shipped at an unreachable peer
    /// and TeeSink::write returned errors that legitimately failed
    /// the asserts. Two layers of fix: (a) flow.rs bounds the wall-
    /// clock per write; (b) THIS guard removes the env vars at the
    /// start of any test that uses it.
    struct FlowsDirGuard {
        prev_flows_dir: Option<String>,
        prev_redis_url: Option<String>,
        prev_audit_dir: Option<String>,
        tmp: TempDir,
    }

    impl FlowsDirGuard {
        fn new() -> Self {
            // Scrub the binary-wide env once (#278). The OnceLock at
            // `flow::isolate_test_env_once` handles the common case
            // (operator's daily-shell env var pollution). The
            // per-instance removes below are belt-and-suspenders for
            // a future test that might set these env vars mid-run —
            // no current test in this module does that, but the
            // restore-in-Drop semantics make the guard safe to
            // extend later without re-thinking isolation.
            crate::flow::isolate_test_env_once();
            let tmp = TempDir::new().unwrap();
            let prev_flows_dir = env::var("DARKMUX_FLOWS_DIR").ok();
            let prev_redis_url = env::var("DARKMUX_REDIS_URL").ok();
            let prev_audit_dir = env::var("DARKMUX_AUDIT_DIR").ok();
            // SAFETY: serialized via `#[serial_test::serial]` on every test
            // that mutates this env var.
            unsafe {
                env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
                env::remove_var("DARKMUX_REDIS_URL");
                env::remove_var("DARKMUX_AUDIT_DIR");
            }
            Self {
                prev_flows_dir,
                prev_redis_url,
                prev_audit_dir,
                tmp,
            }
        }

        fn path(&self) -> &std::path::Path {
            self.tmp.path()
        }
    }

    impl Drop for FlowsDirGuard {
        fn drop(&mut self) {
            // SAFETY: serialized via the test attribute.
            unsafe {
                match &self.prev_flows_dir {
                    Some(v) => env::set_var("DARKMUX_FLOWS_DIR", v),
                    None => env::remove_var("DARKMUX_FLOWS_DIR"),
                }
                match &self.prev_redis_url {
                    Some(v) => env::set_var("DARKMUX_REDIS_URL", v),
                    None => env::remove_var("DARKMUX_REDIS_URL"),
                }
                match &self.prev_audit_dir {
                    Some(v) => env::set_var("DARKMUX_AUDIT_DIR", v),
                    None => env::remove_var("DARKMUX_AUDIT_DIR"),
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
            phase_id: None,
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
            phase_id: None,
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
            phase_id: None,
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
            phase_id: Some("66".to_string()),
            session_id: Some("abc".to_string()),
            source: Some("manual".to_string()),
            reasoning: None,
            mission_id: None,
        })
        .unwrap();

        let rec = single_record(&guard);
        assert_eq!(rec["phase_id"], "66");
        assert_eq!(rec["session_id"], "abc");
        assert_eq!(rec["source"], "manual");
    }

    #[serial_test::serial]
    #[test]
    fn multiple_calls_append_to_same_day_file() {
        let guard = FlowsDirGuard::new();

        run(FlowCmd::Note { text: "a".into(), phase_id: None, session_id: None, source: None }).unwrap();
        run(FlowCmd::Note { text: "b".into(), phase_id: None, session_id: None, source: None }).unwrap();
        run(FlowCmd::Note { text: "c".into(), phase_id: None, session_id: None, source: None }).unwrap();

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
            phase_id: Some("113-s1".into()),
            mission_id: Some("113-mission-propose-pipeline".into()),
            session_id: None,
            source: Some("frontier".into()),
        })
        .unwrap();

        let rec = single_record(&guard);
        assert_eq!(rec["category"], "audit");
        assert_eq!(rec["tier"], "frontier");
        assert_eq!(rec["stage"], "tier-decision");
        assert_eq!(rec["action"], "tier-decision");
        // handle carries role-chosen when dispatch + role known.
        assert_eq!(rec["handle"], "coder");
        assert_eq!(rec["phase_id"], "113-s1");
        assert_eq!(rec["mission_id"], "113-mission-propose-pipeline");
        assert_eq!(rec["source"], "frontier");
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
            phase_id: Some("japan-day-3".into()),
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

    #[test]
    fn tail_match_session_filter_matches() {
        let line = r#"{"ts":"2025-01-01T00:00:00Z","action":"note","handle":"hello","session_id":"abc"}"#;
        assert!(tail_match(line, Some("abc"), false).is_some());
    }

    #[test]
    fn tail_match_session_filter_no_match() {
        let line = r#"{"ts":"2025-01-01T00:00:00Z","action":"note","handle":"hello","session_id":"abc"}"#;
        assert!(tail_match(line, Some("xyz"), false).is_none());
    }

    #[test]
    fn tail_match_no_session_filter_always_some() {
        let line = r#"{"ts":"2025-01-01T00:00:00Z","action":"note","handle":"hello"}"#;
        assert!(tail_match(line, None, false).is_some());
    }

    #[test]
    fn tail_match_unparseable_line_returns_none() {
        assert!(tail_match("not json at all", None, false).is_none());
    }

    #[test]
    fn tail_match_json_mode_returns_raw_line() {
        let line = r#"{"ts":"2025-01-01T00:00:00Z","action":"note"}"#;
        assert_eq!(tail_match(line, None, true), Some(line.to_string()));
    }
}
