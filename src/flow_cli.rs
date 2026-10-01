//! CLI dispatcher for `darkmux flow` shortcut verbs.

use crate::cli_json;
use crate::flow;
use crate::flow::{Category, FlowAction, FlowRecord, FlowSource, Level, OperatorSource, Stage, Tier};
use anyhow::{bail, Context, Result};
use clap::Subcommand;
use serde::Serialize;
use darkmux_types::execution_id::ExecutionId;


/// Top-level `flow` subcommand enum.
#[derive(Subcommand)]
pub enum FlowCmd {
    /// Record an operator-narrative observation. With `--execution <id>
    /// --source adjudication` it records a reviewer correction against a
    /// dispatch, which later coder briefs in that mission carry (#849).
    Note {
        #[arg(long)]
        text: String,
        /// Optional phase identifier.
        #[arg(long = "phase-id")]
        phase_id: Option<String>,
        /// Optional role execution id (the `exec-...` id `darkmux dispatch`
        /// prints).
        #[arg(long, value_parser = parse_execution_arg)]
        execution: Option<ExecutionId>,
        /// Optional source label.
        #[arg(long, value_enum)]
        source: Option<OperatorSource>,
    },
    /// Record an operator-flagged catch / mid-stream observation.
    Catch {
        #[arg(long)]
        text: String,
        /// Optional phase identifier.
        #[arg(long = "phase-id")]
        phase_id: Option<String>,
        /// Optional role execution id (the `exec-...` id `darkmux dispatch`
        /// prints).
        #[arg(long, value_parser = parse_execution_arg)]
        execution: Option<ExecutionId>,
        /// Optional source label.
        #[arg(long, value_enum)]
        source: Option<OperatorSource>,
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
        /// One of the flow actions this darkmux knows (`dispatch.start`,
        /// `operator.note`, ...); anything else is refused.
        #[arg(long, value_parser = FlowAction::parse_known)]
        action: FlowAction,
        #[arg(long)]
        handle: String,
        /// Optional phase identifier.
        #[arg(long = "phase-id")]
        phase_id: Option<String>,
        /// Optional role execution id (the `exec-...` id `darkmux dispatch`
        /// prints).
        #[arg(long, value_parser = parse_execution_arg)]
        execution: Option<ExecutionId>,
        /// Optional source label.
        #[arg(long, value_enum)]
        source: Option<OperatorSource>,
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
    /// show *why this layer was chosen* — the routing rationale that
    /// dispatch records alone don't capture.
    ///
    /// Typical use: the frontier orchestrator runs this verb before
    /// dispatching (or before deciding to hold work in frontier) and
    /// captures the reasoning in operator-readable prose.
    // flow-action-guard:allow — the CLI subcommand name, not an action
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
        /// Optional role execution id (the `exec-...` id `darkmux dispatch`
        /// prints), when the decision links to an already-dispatched
        /// execution, e.g. recorded after the fact.
        #[arg(long, value_parser = parse_execution_arg)]
        execution: Option<ExecutionId>,
        /// Optional source label.
        #[arg(long, value_enum)]
        source: Option<OperatorSource>,
    },
    /// Print a diagnostic snapshot of the flow substrate (sinks, Redis
    /// health, disk health, schema state): useful to operators debugging
    /// substrate problems.
    Status {
        /// Emit machine-readable JSON instead of the human-formatted
        /// summary.
        #[arg(long)]
        json: bool,
    },
    /// Walk every audit file under `DARKMUX_AUDIT_DIR` (or the default
    /// `~/.darkmux/audit/`), recompute the hash chain, and report the
    /// first divergence per file (#163). A clean walk means no divergence
    /// was found at this check — not that the file is unaltered; see
    /// SECURITY.md for the chain's known gaps. Exits with status 2 when
    /// any chain is broken so CI/cron can flag tampering, and with 3 under
    /// `--strict` when a file could not be content-verified at all.
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
        /// (#1775) Exit 3 when a file PRESENT in the walk could not be
        /// content-verified — a legacy pre-2.6.0 struct-hash file, one
        /// whose `hash_format` header marker is missing, or one naming a
        /// format this binary does not recognize. Without this the walk
        /// reports those files honestly but still exits 0, so a tripwire
        /// keyed on the exit code cannot tell "verified" from "never
        /// checked".
        ///
        /// Scope: this is about files that ARE there. It says nothing
        /// about records or files that are ABSENT — a truncated tail or a
        /// deleted file still exits 0, because the chain records neither
        /// how many records a file should hold nor which files should
        /// exist (see SECURITY.md).
        ///
        /// Opt-in because a genuine read-only pre-2.6.0 archive is not a
        /// failure. On a fleet already writing byte-hashed files, no NEW
        /// legacy file should appear — use this there.
        #[arg(long)]
        strict: bool,
    },
    /// Tail flow records, optionally filtered to one role execution, following new
    /// appends live (like `tail -f`). Ctrl-C to stop.
    #[command(name = "tail")]
    Tail {
        /// Only show records for this role execution (the `exec-...` id
        /// `darkmux dispatch` prints).
        #[arg(long, value_parser = parse_execution_arg)]
        execution: Option<ExecutionId>,
        /// Emit raw JSON lines instead of a formatted one-line summary.
        #[arg(long)]
        json: bool,
    },
    /// (#1959 — the `hooks` sub-family under `flow` retired; this was
    /// `hooks drain`.)
    /// Deliver whatever a sink has queued on disk, synchronously, then
    /// report delivered/failed and exit. Today the only sink that queues
    /// is the hook sink (its outbox); `--rule <N>`, `--max-seconds`,
    /// `--file <path> --to <loopback url>` (stray outbox), `--json` keep
    /// their meaning. Sink-agnostic name and shape — same precedent as
    /// `flow integrity-check`, an on-demand action against a sink, not a
    /// sink-specific sub-family.
    Drain {
        /// Only drain this rule's outbox (by its index in `hooks.rules`).
        /// Unset drains every configured rule. Mutually exclusive with
        /// `--file`.
        #[arg(long)]
        rule: Option<usize>,
        /// Give up waiting for the queue to empty after this many
        /// seconds (the drainer's own retry/backoff still applies, so an
        /// unreachable receiver's pending lines may remain undelivered
        /// when this returns — that's reported, not treated as an
        /// error).
        #[arg(long, default_value_t = 30)]
        max_seconds: u64,
        /// (fix-round finding 6) Drain a STRAY outbox file by exact path
        /// instead — one `darkmux doctor`/`flow status` named as
        /// belonging to no currently-configured rule (its rule was
        /// removed or edited). Requires `--to`. One straight pass, no
        /// retry/backoff — a repeat call after fixing the receiver picks
        /// up where the last one stopped.
        #[arg(long, conflicts_with = "rule")]
        file: Option<std::path::PathBuf>,
        /// The loopback URL to deliver `--file`'s lines to — validated
        /// the same way every configured rule's `http` target is.
        #[arg(long, requires = "file")]
        to: Option<String>,
        /// Emit machine-readable JSON instead of the human-formatted summary.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(cmd: FlowCmd) -> Result<()> {
    // Read verbs are intercepted ahead of build_record so the latter
    // only sees write verbs.
    match cmd {
        FlowCmd::Status { json } => return print_status(json),
        FlowCmd::IntegrityCheck { path, json, strict } => {
            return print_integrity_check(path, json, strict)
        }
        FlowCmd::Tail { execution, json } => return run_tail(execution.as_ref(), json),
        FlowCmd::Drain { file: Some(file), to: Some(to), json, .. } => return run_drain_file(&file, &to, json),
        FlowCmd::Drain { file: Some(_), to: None, .. } => bail!("--file requires --to"),
        FlowCmd::Drain { rule, max_seconds, json, .. } => return run_drain(rule, max_seconds, json),
        _ => {}
    }
    let mut record = build_record(cmd);
    stamp_execution_session(&mut record, |id| {
        flow::reader::session_of_execution(&flow::flows_dir(), id)
    })?;
    flow::record(record).context("writing flow record")
}

/// The value of an `--execution` flag: the `exec-...` id `darkmux dispatch`
/// prints. Anything else, a session id included, is refused at parse time,
/// so no verb takes one for the other.
pub(crate) fn parse_execution_arg(wire: &str) -> std::result::Result<ExecutionId, String> {
    ExecutionId::parse_minted(wire).map_err(|e| {
        format!("`{wire}` is not a role execution id: {e}; pass the `exec-...` id that `darkmux dispatch` prints")
    })
}

/// A note about a role execution carries that execution's session too, so
/// the readers keyed on sessions and phases (the coder brief's corrections)
/// still find it. The session is the one the execution's `dispatch.start`
/// record names; an execution `resolve` cannot find is refused, never
/// recorded as a note that nothing can key on.
fn stamp_execution_session(
    record: &mut FlowRecord,
    resolve: impl FnOnce(&ExecutionId) -> Option<String>,
) -> Result<()> {
    let Some(execution) = record.execution_id.as_ref() else { return Ok(()) };
    let session = resolve(execution).with_context(|| {
        format!(
            "no role execution `{execution}` in the flow trail around the day it was minted \
             (`darkmux flow tail` lists recent records); refusing to record a note nothing can find"
        )
    })?;
    record.session_id = Some(session);
    Ok(())
}

/// (fix-round finding 6, #1959 renamed from `run_hooks_drain_file`)
/// `flow drain --file <path> --to <url>` — see
/// `flow::hooks::drain_stray_file`'s doc for the one-shot semantics.
fn run_drain_file(file: &std::path::Path, to: &str, json: bool) -> Result<()> {
    let result = flow::hooks::drain_stray_file(file, to)?;
    if json {
        darkmux_types::style::set_colorize_override(Some(false));
        cli_json::emit(&StrayDrainOutput {
            file: file.display().to_string(),
            to: to.to_string(),
            delivered: result.delivered,
            failed: result.failed,
            remaining_undelivered: result.remaining_undelivered,
        })?;
    } else {
        println!(
            "darkmux flow drain --file {} — delivered: {}, failed: {}, remaining undelivered: {}",
            file.display(),
            result.delivered,
            result.failed,
            result.remaining_undelivered
        );
    }
    Ok(())
}

/// A `FlowSink` that counts `hook.fired`/`hook.failed` records for ONE
/// targeted rule index (or every rule, when unfiltered) — the drain
/// verb's report is "how many did THIS run deliver/fail", which the
/// persisted `.last`/`.dropped` sidecar files don't distinguish (they
/// hold the latest outcome / a running total, not "since I started
/// draining"), so counting live through a dedicated sink for the
/// duration of this one call is the correct source of truth.
#[derive(Default)]
struct DrainCountingSink {
    rule_filter: Option<usize>,
    delivered: std::sync::atomic::AtomicU64,
    failed: std::sync::atomic::AtomicU64,
}

impl flow::FlowSink for DrainCountingSink {
    fn persist(&self, record: crate::flow::CheckedRecord<'_>) -> Result<()> {
        let record = record.get();
        if let Some(idx) = self.rule_filter {
            let rule_index = match record.payload.as_ref() {
                Some(flow::Payload::HookFired(p)) | Some(flow::Payload::HookFailed(flow::payload::HookFailedPayload::Delivery(p))) => {
                    Some(p.rule_index)
                }
                Some(flow::Payload::HookFailed(flow::payload::HookFailedPayload::Notice(p))) => Some(p.rule_index),
                _ => None,
            };
            let matches_idx = rule_index == Some(idx);
            if !matches_idx {
                return Ok(());
            }
        }
        if record.action == FlowAction::HookFired {
            self.delivered.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        } else if record.action == FlowAction::HookFailed {
            self.failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(())
    }
    fn info(&self) -> flow::SinkInfo {
        flow::SinkInfo { kind: "DrainCounting".into(), config: Default::default(), children: vec![], raw_url: None }
    }
}

/// Outcome of one `drain_hooks` call — split out from the CLI's own
/// render/print so it's testable directly against synthetic `rules`,
/// mirroring `build_hooks_check`'s split (#811 empties the global config
/// tier in test builds, so there's no way to inject `hooks.rules` through
/// the real accessor path in a unit test).
#[derive(Debug, PartialEq, Eq)]
struct DrainResult {
    rule_filter: Option<usize>,
    delivered: u64,
    failed: u64,
    remaining_undelivered: usize,
    /// (fix-round finding 8) Target rule indices whose drain lock a
    /// post-timeout probe found held by ANOTHER process — see
    /// `flow::hooks::rules_with_drain_lock_held_elsewhere`'s doc on why
    /// this is best-effort. Always empty when `remaining_undelivered`
    /// is 0 (nothing left to explain).
    lock_held_rules: Vec<usize>,
}

/// Run the drainer SYNCHRONOUSLY, against `rules`/`outbox_dir` directly,
/// until every targeted rule's outbox is empty or `max_seconds` elapses.
fn drain_hooks(
    rules: &[darkmux_types::config::HookRule],
    outbox_dir: &std::path::Path,
    rule_filter: Option<usize>,
    max_seconds: u64,
) -> Result<DrainResult> {
    if let Some(idx) = rule_filter {
        if idx >= rules.len() {
            bail!("no hook rule #{idx} configured ({} rule(s) total)", rules.len());
        }
    }
    if rules.is_empty() {
        return Ok(DrainResult {
            rule_filter,
            delivered: 0,
            failed: 0,
            remaining_undelivered: 0,
            lock_held_rules: Vec::new(),
        });
    }

    let counting = std::sync::Arc::new(DrainCountingSink { rule_filter, ..Default::default() });
    let report: std::sync::Arc<dyn flow::FlowSink> = counting.clone();
    // Constructed against the FULL rule set (not a filtered slice) so
    // every rule's outbox/cursor/lock file paths — which are derived from
    // a content hash of the rule's (match, host), NOT its array position
    // (see `darkmux_flow::hooks::rule_key`) — line up EXACTLY with what
    // the live dispatch process already wrote. `--rule N` narrows what
    // this call WAITS on and REPORTS, not which rules get a `HookSink`
    // at all.
    let sink = flow::hooks::HookSink::new(rules, outbox_dir.to_path_buf(), report)
        .context("constructing hook sink for drain")?;

    let targets: Vec<usize> = match rule_filter {
        Some(idx) => vec![idx],
        None => (0..rules.len()).collect(),
    };
    let start = std::time::Instant::now();
    let deadline = std::time::Duration::from_secs(max_seconds);
    loop {
        let summaries = flow::hooks::summarize_configured_rules(rules, outbox_dir);
        let all_drained = targets.iter().all(|&idx| summaries.get(idx).map(|s| s.undelivered == 0).unwrap_or(true));
        if all_drained || start.elapsed() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    drop(sink); // bounded join stops the background drainer thread

    let delivered = counting.delivered.load(std::sync::atomic::Ordering::Relaxed);
    let failed = counting.failed.load(std::sync::atomic::Ordering::Relaxed);
    let remaining_undelivered: usize = {
        let summaries = flow::hooks::summarize_configured_rules(rules, outbox_dir);
        targets.iter().filter_map(|&idx| summaries.get(idx).map(|s| s.undelivered)).sum()
    };
    // (fix-round finding 8) Only worth probing when something's actually
    // left undelivered — distinguishes "another drainer is already
    // working this rule" from an ordinary down/slow receiver.
    let lock_held_rules = if remaining_undelivered > 0 {
        let held = flow::hooks::rules_with_drain_lock_held_elsewhere(rules, outbox_dir);
        held.into_iter().filter(|idx| targets.contains(idx)).collect()
    } else {
        Vec::new()
    };

    Ok(DrainResult { rule_filter, delivered, failed, remaining_undelivered, lock_held_rules })
}

fn render_drain_result_human(r: &DrainResult, max_seconds: u64) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let scope = match r.rule_filter {
        Some(idx) => format!("rule #{idx}"),
        None => "all rules".to_string(),
    };
    let _ = writeln!(out, "darkmux flow drain ({scope}) — delivered: {}, failed: {}", r.delivered, r.failed);
    if r.remaining_undelivered > 0 {
        let _ = writeln!(
            out,
            "  {} line(s) still undelivered after {max_seconds}s — the drainer's own retry/backoff continues \
             in a real dispatch process; this was a bounded wait, not an error.",
            r.remaining_undelivered
        );
        // (fix-round finding 8) A specific, actionable reason ON TOP of
        // the generic line above: a live process's own drainer already
        // holds this rule's lock, so nothing was ever going to be
        // delivered by THIS invocation regardless of how long it waited.
        for idx in &r.lock_held_rules {
            let _ =
                writeln!(out, "  another drainer holds the lock for rule #{idx}; nothing delivered by this process.");
        }
    }
    out
}

/// `flow drain --json`.
#[derive(Serialize, schemars::JsonSchema)]
pub struct DrainOutput {
    /// The rule index the drain was limited to, when it was.
    pub rule: Option<usize>,
    pub delivered: u64,
    pub failed: u64,
    pub remaining_undelivered: usize,
    /// True when records remain after the wait.
    pub timed_out: bool,
    /// Rules whose drain lock another process held when the wait ended.
    pub lock_held_rules: Vec<usize>,
}

impl From<&DrainResult> for DrainOutput {
    fn from(r: &DrainResult) -> Self {
        DrainOutput {
            rule: r.rule_filter,
            delivered: r.delivered,
            failed: r.failed,
            remaining_undelivered: r.remaining_undelivered,
            timed_out: r.remaining_undelivered > 0,
            lock_held_rules: r.lock_held_rules.clone(),
        }
    }
}

/// `flow drain --file <path> --to <url> --json`.
#[derive(Serialize, schemars::JsonSchema)]
pub struct StrayDrainOutput {
    pub file: String,
    pub to: String,
    pub delivered: usize,
    pub failed: usize,
    pub remaining_undelivered: usize,
}

/// `flow integrity-check --json`: one report per audit file checked.
#[derive(Serialize, schemars::JsonSchema)]
pub struct IntegrityCheckOutput<'a> {
    pub reports: &'a [flow::IntegrityReport],
}

fn run_drain(rule_filter: Option<usize>, max_seconds: u64, json: bool) -> Result<()> {
    let rules = darkmux_types::config_access::hooks_rules();
    let outbox_dir = darkmux_types::config_access::hooks_outbox_dir();
    let result = drain_hooks(&rules, &outbox_dir, rule_filter, max_seconds)?;
    if json {
        darkmux_types::style::set_colorize_override(Some(false));
        cli_json::emit(&DrainOutput::from(&result))?;
    } else {
        print!("{}", render_drain_result_human(&result, max_seconds));
    }
    Ok(())
}

/// Render `darkmux flow status` to stdout. Calls `flow::collect_status()`
/// for the snapshot; format gated by `--json`.
fn print_status(json: bool) -> Result<()> {
    let status = flow::collect_status();
    if json {
        // (#776) machine-readable: force color off (defense-in-depth).
        darkmux_types::style::set_colorize_override(Some(false));
        cli_json::emit(&status)?;
    } else {
        print!("{}", flow::format_status_human(&status));
    }
    Ok(())
}

/// Render `darkmux flow integrity-check` to stdout. Walks the audit dir
/// (or a single `--path`), recomputes each file's hash chain, reports
/// pass/break per file. Exits with status 2 when any chain is genuinely
/// broken (`chain_valid == false`) so CI / cron / monitoring can flag
/// tampering. (#1769) A legacy pre-2.6.0 file — struct-hash format, no
/// `hash_format` marker on its header — is NOT a break: `chain_valid`
/// stays `true`, the exit status stays 0, and the caveat prints as a
/// warning (readable, never content-verified) rather than an error.
///
/// (#1775) `strict` promotes that caveat to exit 3, so a cron keyed on
/// the exit code can tell "verified" from "could not verify". The status
/// decision itself lives in `flow::integrity_exit_code`, which is unit
/// tested — this belt used to be reachable only by review.
fn print_integrity_check(
    path: Option<std::path::PathBuf>,
    json: bool,
    strict: bool,
) -> Result<()> {
    let reports = if let Some(p) = path {
        vec![flow::integrity_check_file(&p)?]
    } else {
        flow::integrity_check_all()?
    };

    // (#1775) Computed BEFORE rendering, because one of the lines below
    // makes a factual claim ABOUT this value. Gating that claim on an
    // input predicate instead ("are any files legacy?") printed "exit
    // status stays 0" on a run that exited 2 — a legacy file and a broken
    // file in the same directory — which is the same class of defect this
    // command exists to catch, in the output of the command itself.
    let exit_code = flow::integrity_exit_code(&reports, strict);

    use darkmux_types::style;
    if json {
        // (#776) machine-readable: force color off (defense-in-depth).
        style::set_colorize_override(Some(false));
        cli_json::emit(&IntegrityCheckOutput { reports: &reports })?;
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
            } else if r.legacy_format {
                // (#1769) Chain-valid in the sense that nothing was broken,
                // but this file predates byte-hash verification and its
                // content was NOT checked at all. Loud, not silent — exit
                // status stays 0 (this is not tampering), but an operator
                // watching the output must still see the caveat, or
                // "valid" quietly becomes a stronger claim than the walk
                // actually supports.
                println!(
                    "{}",
                    style::warn(&format!(
                        "       {} record(s) NOT content-verified — legacy pre-2.6.0 format",
                        r.records_checked
                    ))
                );
                if let Some(note) = r.note.as_ref() {
                    println!("{}", style::warn(&format!("       {note}")));
                }
                if strict {
                    // Same rule as the hint below: a line naming the exit
                    // code is gated on the COMPUTED code, never on `strict`
                    // alone. A chain break elsewhere in the walk outranks
                    // this file, and saying "(exit 3)" on a run that exits
                    // 2 is the defect this command exists to catch.
                    println!(
                        "{}",
                        style::error(&format!(
                            "       --strict: counted as a failure ({})",
                            if exit_code == 3 {
                                "exit 3".to_string()
                            } else {
                                format!("a chain break elsewhere takes precedence — exit {exit_code}")
                            }
                        ))
                    );
                }
            }
        }
        // (#1775) Without --strict the walk still exits 0 on an
        // unverifiable file. Say so, so an operator reading the output
        // knows the exit code they'd get from cron does NOT reflect the
        // warning they can see here. Gated on the COMPUTED code, never on
        // `!strict` alone: with a broken file also present the exit is 2,
        // and claiming otherwise would be a false statement printed right
        // beside a tamper signal.
        if exit_code == 0 && reports.iter().any(|r| r.legacy_format) {
            println!(
                "{}",
                style::dim(
                    "       (exit status stays 0 — re-run with --strict to fail on files that \
                     could not be content-verified)"
                )
            );
        }
    }

    match exit_code {
        0 => Ok(()),
        code => std::process::exit(code),
    }
}

/// Filter a single JSONL line for tail output.
///
/// Returns `Some(string)` when the line should be printed, `None` otherwise.
/// When `execution` is `Some(id)`, only records whose `execution_id` equals
/// `id` are returned. When `json` is true, the line is returned as the flow
/// reader forwards it (verbatim, unless its action is a retired spelling);
/// otherwise a concise one-line summary is built from available fields.
fn tail_match(line: &str, execution: Option<&ExecutionId>, json: bool) -> Option<String> {
    let forwarded = flow::reader::upgrade_line(line)?;
    let parsed: serde_json::Value = serde_json::from_str(&forwarded).ok()?;

    if let Some(id) = execution {
        if parsed.get("execution_id").and_then(|v| v.as_str()) != Some(id.as_str()) {
            return None;
        }
    }

    if json {
        Some(forwarded.into_owned())
    } else {
        use darkmux_types::style;
        let ts = parsed.get("ts").and_then(|v| v.as_str()).unwrap_or("");
        let action = parsed.get("action").and_then(|v| v.as_str()).unwrap_or("-");
        let handle = parsed.get("handle").and_then(|v| v.as_str()).unwrap_or("-");
        let origin = tail_origin(&parsed);
        // Space-separated (not column-padded) → coloring is alignment-safe.
        Some(format!(
            "{} {} {} {}",
            style::dim(ts),
            style::accent(action),
            handle,
            style::dim(&origin)
        ))
    }
}

/// What a tailed record is about, for its last column: the role execution
/// when it names one it was minted with, else the run its session belongs to
/// (a pre-4.0 execution record's synthesized id is built from its session,
/// so it reads as no execution), else `-`. The session itself never shows.
fn tail_origin(record: &serde_json::Value) -> String {
    let text = |key: &str| record.get(key).and_then(|v| v.as_str());
    if let Some(id) = text("execution_id").and_then(|wire| ExecutionId::parse_minted(wire).ok()) {
        return id.to_string();
    }
    text("session_id")
        .and_then(|sid| darkmux_types::session_id::SessionId::parse_legacy(sid, text("mission_id")))
        .map_or_else(|| "-".to_string(), |session| session.run_id().to_string())
}

/// Run `darkmux flow tail`: read today's JSONL file, then follow new appends
/// until interrupted (Ctrl-C / SIGINT — default signal handler).
pub fn run_tail(execution: Option<&ExecutionId>, json: bool) -> anyhow::Result<()> {
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
                            if let Some(s) = tail_match(line, execution, json) {
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
        FlowCmd::Note { text, phase_id, execution, source } => FlowRecord {
            ts,
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: FlowAction::OperatorNote,
            handle: text,
            phase_id,
            session_id: None,
            execution_id: execution,
            source: source.map(FlowSource::from),
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
        },
        FlowCmd::Catch { text, phase_id, execution, source } => FlowRecord {
            ts,
            level: Level::Warn,
            category: Category::Audit,
            tier: Tier::Operator,
            stage: Stage::Review,
            action: FlowAction::OperatorCatch,
            handle: text,
            phase_id,
            session_id: None,
            execution_id: execution,
            source: source.map(FlowSource::from),
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
        },
        FlowCmd::Record {
            level,
            category,
            tier,
            stage,
            action,
            handle,
            phase_id,
            execution,
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
            session_id: None,
            execution_id: execution,
            source: source.map(FlowSource::from),
            model: None,
            reasoning,
            mission_id,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
        },
        FlowCmd::TierDecision {
            decision,
            reasoning,
            role_chosen,
            phase_id,
            mission_id,
            execution,
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
            action: FlowAction::TierDecision,
            handle: role_chosen.clone().unwrap_or_else(|| decision.clone()),
            phase_id,
            session_id: None,
            execution_id: execution,
            source: source.map(FlowSource::from),
            model: None,
            reasoning: Some(format!("[{decision}] {reasoning}")),
            mission_id,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
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
        FlowCmd::Drain { .. } => unreachable!(
            "FlowCmd::Drain is a read verb and must be handled by run() before build_record"
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

    // (#1959 flow-hooks-family retirement) the retired `hooks status` sub-verb's render/JSON
    // tests moved to `darkmux-flow`'s `status::hooks_status_tests` — the
    // hooks section is now part of `FlowStatus`/`format_status_human`
    // rather than a CLI-local render path. `flow_json_paths_force_colorize_off`
    // below still covers this crate's own responsibility: `flow status --json`
    // forces color off regardless of what FlowStatus now carries.

    // ─── (#2093 merge-gate finding 10) drain verb ────────────────────────

    #[test]
    fn drain_hooks_delivers_pending_lines_and_reports_counts() {
        use darkmux_types::config::{HookMatch, HookRule};
        let tmp = TempDir::new().unwrap();
        let receiver = flow::hooks::test_receiver::HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];

        // Seed the outbox directly (bypassing a live HookSink write) —
        // simulates lines a PRIOR dispatch process already wrote.
        // `summarize_configured_rules` is the read-only path that derives
        // the same deterministic outbox path a real `HookSink` would.
        let outbox_path = flow::hooks::summarize_configured_rules(&rules, tmp.path())[0].outbox_path.clone();
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(&outbox_path, "{\"action\":\"work.a\"}\n{\"action\":\"work.b\"}\n{\"action\":\"work.c\"}\n").unwrap();

        let result = drain_hooks(&rules, tmp.path(), None, 10).unwrap();
        assert_eq!(result.delivered, 3, "{result:?}");
        assert_eq!(result.failed, 0, "{result:?}");
        assert_eq!(result.remaining_undelivered, 0, "{result:?}");
        assert_eq!(receiver.request_count(), 3);

        let human = render_drain_result_human(&result, 10);
        assert!(human.contains("delivered: 3"), "{human}");
        assert!(!human.contains("still undelivered"), "{human}");

        let json = serde_json::to_value(DrainOutput::from(&result)).unwrap();
        assert_eq!(json["delivered"], serde_json::json!(3));
        assert_eq!(json["timed_out"], serde_json::json!(false));
    }

    #[test]
    fn drain_hooks_refuses_an_out_of_range_rule_index() {
        let tmp = TempDir::new().unwrap();
        let err = drain_hooks(&[], tmp.path(), Some(0), 1).unwrap_err();
        assert!(err.to_string().contains("no hook rule #0"), "{err}");
    }

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
            execution: None,
            source: None,
        })
        .unwrap();

        let rec = single_record(&guard);
        assert_eq!(rec["tier"], "operator");
        assert_eq!(rec["level"], "info");
        assert_eq!(rec["category"], "work");
        assert_eq!(rec["action"], "operator.note");
        assert_eq!(rec["handle"], "hello");
    }

    #[serial_test::serial]
    #[test]
    fn catch_writes_record_with_warn_level_and_audit_category() {
        let guard = FlowsDirGuard::new();
        run(FlowCmd::Catch {
            text: "oops".to_string(),
            phase_id: None,
            execution: None,
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
            tier: Tier::Darkmux,
            stage: Stage::Dispatch,
            action: FlowAction::OperatorNote,
            handle: "y".to_string(),
            phase_id: None,
            execution: None,
            source: None,
            reasoning: None,
            mission_id: None,
        })
        .unwrap();

        let rec = single_record(&guard);
        assert_eq!(rec["level"], "error");
        assert_eq!(rec["category"], "machinery");
        assert_eq!(rec["tier"], "darkmux");
        assert_eq!(rec["stage"], "dispatch");
        assert_eq!(rec["action"], "operator.note");
        assert_eq!(rec["handle"], "y");
    }

    #[test]
    fn record_threads_optional_fields_when_provided() {
        let execution = ExecutionId::mint();
        let rec = build_record(FlowCmd::Record {
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: FlowAction::OperatorNote,
            handle: "opt-handle".to_string(),
            phase_id: Some("66".to_string()),
            execution: Some(execution.clone()),
            source: Some(OperatorSource::Manual),
            reasoning: None,
            mission_id: None,
        });

        assert_eq!(rec.phase_id.as_deref(), Some("66"));
        assert_eq!(rec.execution_id, Some(execution));
        assert_eq!(rec.source, Some(FlowSource::Manual));
    }

    fn note_about(execution: &ExecutionId) -> FlowRecord {
        build_record(FlowCmd::Note {
            text: "verdict".into(),
            phase_id: None,
            execution: Some(execution.clone()),
            source: Some(OperatorSource::Adjudication),
        })
    }

    /// A note about an execution keys on the execution AND carries the
    /// session that execution ran under, so the session-keyed corrections
    /// reader (#849) still finds it.
    #[test]
    fn a_note_about_an_execution_is_stamped_with_it_and_its_session() {
        let execution = ExecutionId::mint();
        let mut rec = note_about(&execution);
        assert_eq!(rec.execution_id.as_ref(), Some(&execution));
        assert_eq!(rec.session_id, None, "the session is resolved, never taken from the flag");
        stamp_execution_session(&mut rec, |id| {
            assert_eq!(id, &execution);
            Some("run-a.phase.p1".to_string())
        })
        .unwrap();
        assert_eq!(rec.session_id.as_deref(), Some("run-a.phase.p1"));
        assert_eq!(rec.execution_id, Some(execution));
    }

    /// The inverse: an execution the flow trail never saw is refused, naming
    /// the id, rather than recorded as a note nothing can key on.
    #[test]
    fn a_note_about_an_unknown_execution_is_refused_naming_it() {
        let execution = ExecutionId::mint();
        let mut rec = note_about(&execution);
        let err = stamp_execution_session(&mut rec, |_| None).unwrap_err().to_string();
        assert!(err.contains(execution.as_str()), "{err}");
        assert_eq!(rec.session_id, None);
    }

    #[test]
    fn a_note_about_nothing_needs_no_lookup() {
        let mut rec = build_record(FlowCmd::Note { text: "t".into(), phase_id: None, execution: None, source: None });
        stamp_execution_session(&mut rec, |_| panic!("no execution, no lookup")).unwrap();
        assert_eq!(rec.session_id, None);
    }

    /// End to end: a `dispatch.start` in the flow trail is what a note's
    /// `--execution` resolves against.
    #[serial_test::serial]
    #[test]
    fn run_resolves_the_session_from_the_flow_trail_and_refuses_an_unseen_execution() {
        let guard = FlowsDirGuard::new();
        let execution = ExecutionId::mint();
        let start = serde_json::json!({
            "ts": "2026-09-30T00:00:00Z", "action": "dispatch.start",
            "execution_id": execution.as_str(), "session_id": "run-a.adhoc.coder.x",
        });
        let day = format!("{}.jsonl", flow::ts_utc_now().chars().take(10).collect::<String>());
        std::fs::write(guard.tmp.path().join(day), format!("{start}\n")).unwrap();

        let unseen = ExecutionId::mint();
        let err = run(FlowCmd::Note { text: "t".into(), phase_id: None, execution: Some(unseen.clone()), source: None })
            .unwrap_err()
            .to_string();
        assert!(err.contains(unseen.as_str()), "{err}");

        run(FlowCmd::Note { text: "verdict".into(), phase_id: None, execution: Some(execution.clone()), source: None })
            .unwrap();
        let written = std::fs::read_dir(guard.tmp.path())
            .unwrap()
            .flatten()
            .map(|e| std::fs::read_to_string(e.path()).unwrap())
            .collect::<String>();
        let note = written
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|r| r["action"] == "operator.note")
            .expect("the note was recorded");
        assert_eq!(note["execution_id"], execution.as_str());
        assert_eq!(note["session_id"], "run-a.adhoc.coder.x");
    }

    /// A session id is not an execution id, and is refused where the flag
    /// is parsed, so `--execution` cannot be handed the wrong noun.
    #[test]
    fn a_session_shaped_value_is_refused_as_an_execution() {
        for wrong in ["run-a.adhoc.coder.x", "mission-run-auth-s1", "", "legacy:s:m", "exec-zz-1-2"] {
            let err = parse_execution_arg(wrong).unwrap_err();
            assert!(err.contains("not a role execution id"), "{wrong:?}: {err}");
        }
        let minted = ExecutionId::mint();
        assert_eq!(parse_execution_arg(minted.as_str()).unwrap(), minted);
    }

    /// `--source` is the operator-writable subset of `FlowSource`, and the
    /// channels other features read (`adjudication`, `orchestrator`) parse to
    /// the source they name; a source only darkmux writes is refused.
    #[test]
    fn source_flag_accepts_the_operator_writable_sources_only() {
        use clap::Parser;
        let parse = |source: &str| {
            crate::cli::Cli::try_parse_from(["darkmux", "flow", "note", "--text", "t", "--source", source])
        };
        for (flag, want) in [
            ("adjudication", FlowSource::Adjudication),
            ("orchestrator", FlowSource::Orchestrator),
            ("manual", FlowSource::Manual),
            ("frontier", FlowSource::Frontier),
        ] {
            let cli = parse(flag).ok().unwrap_or_else(|| panic!("`--source {flag}` must parse"));
            match cli.command {
                crate::cli::Cmd::Flow { sub: FlowCmd::Note { source: Some(got), .. } } => {
                    assert_eq!(FlowSource::from(got), want, "{flag}")
                }
                _ => panic!("`flow note --source {flag}` did not parse as a note"),
            }
        }
        for refused in ["scheduler", "host-sampler", "frontier-orchestrator", "estimator"] {
            assert!(parse(refused).is_err(), "`--source {refused}` is not operator-writable");
        }
    }

    #[serial_test::serial]
    #[test]
    fn multiple_calls_append_to_same_day_file() {
        let guard = FlowsDirGuard::new();

        run(FlowCmd::Note { text: "a".into(), phase_id: None, execution: None, source: None }).unwrap();
        run(FlowCmd::Note { text: "b".into(), phase_id: None, execution: None, source: None }).unwrap();
        run(FlowCmd::Note { text: "c".into(), phase_id: None, execution: None, source: None }).unwrap();

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
            execution: None,
            source: Some(OperatorSource::Frontier),
        })
        .unwrap();

        let rec = single_record(&guard);
        assert_eq!(rec["category"], "audit");
        assert_eq!(rec["tier"], "frontier");
        assert_eq!(rec["stage"], "tier-decision");
        assert_eq!(rec["action"], "tier.decision");
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
            execution: None,
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
    fn tail_match_execution_filter_matches_the_record_execution() {
        let id = ExecutionId::mint();
        let line = format!(r#"{{"ts":"2025-01-01T00:00:00Z","action":"note","handle":"hello","execution_id":"{id}"}}"#);
        assert!(tail_match(&line, Some(&id), false).is_some());
    }

    /// The inverse, and the noun's whole point: a record of the same session
    /// but another execution is not this execution's.
    #[test]
    fn tail_match_execution_filter_excludes_other_executions_of_the_same_session() {
        let (mine, other) = (ExecutionId::mint(), ExecutionId::mint());
        let line = format!(
            r#"{{"ts":"2025-01-01T00:00:00Z","action":"note","handle":"hello","session_id":"abc","execution_id":"{other}"}}"#
        );
        assert!(tail_match(&line, Some(&mine), false).is_none());
        let no_execution = r#"{"ts":"2025-01-01T00:00:00Z","action":"note","handle":"hello","session_id":"abc"}"#;
        assert!(tail_match(no_execution, Some(&mine), false).is_none());
    }

    #[test]
    fn tail_match_no_execution_filter_always_some() {
        let line = r#"{"ts":"2025-01-01T00:00:00Z","action":"note","handle":"hello"}"#;
        assert!(tail_match(line, None, false).is_some());
    }

    /// The last column names the role execution, or the run for a record
    /// outside any execution; the session never shows.
    #[test]
    fn tail_column_is_the_execution_or_else_the_run_never_the_session() {
        let id = ExecutionId::mint();
        let in_execution = format!(
            r#"{{"ts":"2025-01-01T00:00:00Z","action":"dispatch.start","handle":"coder","session_id":"run-a.adhoc.coder.n1","execution_id":"{id}"}}"#
        );
        let shown = tail_match(&in_execution, None, false).unwrap();
        assert!(shown.ends_with(id.as_str()), "{shown}");
        assert!(!shown.contains("run-a.adhoc"), "{shown}");

        let outside = r#"{"ts":"2025-01-01T00:00:00Z","action":"mission.start","handle":"m","session_id":"m-auth.phase.p1","mission_id":"m-auth"}"#;
        let shown = tail_match(outside, None, false).unwrap();
        assert!(shown.ends_with("m-auth"), "the run, not the phase session: {shown}");
        assert!(!shown.contains("phase"), "{shown}");

        // A pre-4.0 execution record reads with a synthesized id, which is
        // built from its session: it shows the run instead.
        // flow-action-guard:allow — a pre-4.0 archive spelling, read leniently
        let old = r#"{"ts":"2025-01-01T00:00:00Z","action":"dispatch start","handle":"coder","session_id":"mission-run-auth-s1","mission_id":"auth"}"#;
        let shown = tail_match(old, None, false).unwrap();
        assert!(shown.ends_with("auth"), "{shown}");
        assert!(!shown.contains("legacy:") && !shown.contains("mission-run-auth-s1"), "{shown}");

        let neither = r#"{"ts":"2025-01-01T00:00:00Z","action":"operator.note","handle":"hi"}"#;
        assert!(tail_match(neither, None, false).unwrap().ends_with(" -"));
    }

    #[test]
    fn tail_match_unparseable_line_returns_none() {
        assert!(tail_match("not json at all", None, false).is_none());
    }

    #[test]
    fn tail_match_json_mode_returns_a_current_line_verbatim() {
        let line = r#"{"ts":"2025-01-01T00:00:00Z","action":"operator.note"}"#;
        assert_eq!(tail_match(line, None, true), Some(line.to_string()));
    }

    /// A pre-4.0 line tails with its current spelling, in both modes.
    #[test]
    fn tail_match_upgrades_a_retired_spelling() {
        let line = r#"{"ts":"2025-01-01T00:00:00Z","action":"note","session_id":"abc"}"#;
        let json: serde_json::Value = serde_json::from_str(&tail_match(line, None, true).unwrap()).unwrap();
        assert_eq!(json["action"], "operator.note");
        assert!(tail_match(line, None, false).unwrap().contains("operator.note"));
    }
}
