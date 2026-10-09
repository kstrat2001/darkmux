//! A session's run lifecycle: the daemon's executor of the rules
//! `ui/src/lib/lifecycle.ts` states for the viewer. One spec
//! (`tests/lifecycle/cases.json`) is asserted against both.
//!
//! Records are kept as they are read and segmented on demand, in time
//! order (an unparsable `ts` after every parsable one, arrival order among
//! equals): the order the viewer segments in, whatever order the day files
//! hand them over.
//!
//! 1. Attempts. A session's records segment into attempts, every mission's
//!    together. An attempt opens on its first opening record (a bookend
//!    start, `run.start` or `dispatch.start`; a `budget.wait`,
//!    `mission.start` or `step.start`; or, when nothing of its mission
//!    opened yet, a turn, heartbeat, tool call or rest). A record of an
//!    execution (every execution-grain record carries its `execution_id`) joins the latest attempt
//!    of that execution, so a session holding several (a map's items) keeps
//!    each one's records and its own close apart. Any other record, and one
//!    of an execution with no attempt yet, joins by mission: a record naming
//!    a mission joins that mission's latest attempt (or adopts the current
//!    one when it names none yet). A record naming none joins the latest
//!    attempt still open at its time, or the latest opened when none is. A
//!    bookend start in an attempt that already has one, or a reopening
//!    record after the attempt closed, starts the next attempt.
//! 2. Close. An attempt closes on its first closing record. A closing record
//!    seen before anything opened closes the first attempt left with none.
//! 3. Outcome. A bookend terminal (`run.complete` / `run.error`,
//!    `dispatch.complete` / `dispatch.error`) is the outcome when the
//!    attempt has one, even when a `session.end` closed it first.
//!    A terminal that names the operator's stop (a `budget.stop`'s
//!    `reason`, a `dispatch.error`'s `stop_reason`) is abandoned as
//!    aborted, never an error.
//! 4. Waiting. An open `budget.wait` holds the attempt live until its
//!    announced resume time plus the grace; the staleness clock then runs
//!    from there.
//! 5. Stale. An open attempt silent past the window, or one a later attempt
//!    of its own mission superseded, has stopped with no ending recorded.
//!    Another mission's later attempt on the same session does not: missions
//!    launched from one config share a task session and run at once (#2125).
//!
//! `/runs` judges a session by its current attempt (`SessionAgg::settle`),
//! and a mission by its own latest attempt on each session it shares
//! (`RunFold::latest_of`). Session presence is the one input the daemon
//! lacks (the viewer holds a session's current run open on it).

use crate::runs::{AbandonReason, RunStatus};
use darkmux_flow::{Edge, FlowAction, Grain};
use darkmux_types::execution_id::ExecutionId;
use std::sync::Arc;

/// How a closed attempt ended: its status, and why when abandoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ending {
    pub status: RunStatus,
    pub reason: Option<AbandonReason>,
}

/// What the lifecycle keeps of one record: its action, mission and `ts`,
/// and the two payload facts a rule reads.
#[derive(Debug, Clone)]
struct Folded {
    action: Option<FlowAction>,
    mission: Option<String>,
    /// The execution the record is of; `None` for a record of another grain.
    execution: Option<ExecutionId>,
    ts: String,
    /// Its `ts` as epoch seconds; `None` when unparsable.
    at: Option<u64>,
    /// Whether the payload names the operator's stop: a `budget.stop`'s
    /// non-empty `reason` (every reason its producers write is an operator's
    /// stop: an interrupt, `mission abort`/`finalize`, an abandoned phase), or
    /// a `dispatch.error`'s non-empty `stop_reason` (a caught signal).
    names_a_reason: bool,
    /// A `budget.wait`'s announced wait, milliseconds.
    wait_ms: u64,
    /// A `step.complete` that names a later step of its task still planned: the task's session
    /// goes on (#3074).
    later_step_planned: bool,
}

impl Folded {
    fn of(action: Option<&FlowAction>, mission: Option<&str>, ts: &str, v: &serde_json::Value) -> Self {
        let (names_a_reason, wait_ms, later_step_planned) = match darkmux_flow::reader::payload_of(v) {
            Some(darkmux_flow::Payload::BudgetStop(p)) => (p.reason.as_deref().is_some_and(|r| !r.is_empty()), 0, false),
            Some(darkmux_flow::Payload::DispatchError(p)) => (p.stop_reason.as_deref().is_some_and(|r| !r.is_empty()), 0, false),
            Some(darkmux_flow::Payload::BudgetWait(p)) => (false, p.wait_ms.unwrap_or(0), false),
            Some(darkmux_flow::Payload::StepComplete(p)) => (false, 0, p.later_step_planned == Some(true)),
            _ => (false, 0, false),
        };
        Folded {
            action: action.cloned(),
            mission: mission.map(str::to_string),
            execution: action.filter(|a| a.grain() == Some(Grain::Execution)).and_then(|_| darkmux_flow::reader::execution_id_of(v)),
            ts: ts.to_string(),
            at: crate::runs::parse_flow_ts(ts),
            names_a_reason,
            wait_ms,
            later_step_planned,
        }
    }

    /// The ending it implies when it is a closing record (rule 2).
    fn ending(&self) -> Option<Ending> {
        let action = self.action.as_ref()?;
        if *action == FlowAction::StepComplete && self.later_step_planned {
            return None;
        }
        ending_of(action, self.names_a_reason)
    }

    /// A bookend terminal: the outcome over any other close (rule 3).
    fn is_bookend_terminal(&self) -> bool {
        self.action.as_ref().and_then(FlowAction::bookend).is_some_and(|b| b.edge.is_terminal())
    }
}

/// The ending `action` implies when it is a closing record (rule 2).
/// `names_a_reason` is the payload's naming of an operator's stop (a
/// `budget.stop`'s `reason`, a `dispatch.error`'s `stop_reason`): a terminal
/// that names one is the operator's stop, abandoned as aborted, not a failure.
pub(crate) fn ending_of(action: &FlowAction, names_a_reason: bool) -> Option<Ending> {
    let ended = |status, reason| Some(Ending { status, reason });
    match action.bookend().map(|b| b.edge) {
        Some(Edge::Complete) => return ended(RunStatus::Complete, None),
        Some(Edge::Error) if names_a_reason => return ended(RunStatus::Abandoned, Some(AbandonReason::Aborted)),
        Some(Edge::Error) => return ended(RunStatus::Error, None),
        Some(Edge::Start) | None => {}
    }
    if matches!(action, FlowAction::StepComplete | FlowAction::MissionClose) {
        return ended(RunStatus::Complete, None);
    }
    if *action == FlowAction::StepError {
        return ended(RunStatus::Error, None);
    }
    if *action == FlowAction::SessionEnd {
        return ended(RunStatus::Abandoned, Some(AbandonReason::NoTerminal));
    }
    if *action == FlowAction::MissionAbort || (*action == FlowAction::BudgetStop && names_a_reason) {
        return ended(RunStatus::Abandoned, Some(AbandonReason::Aborted));
    }
    (*action == FlowAction::BudgetStop).then_some(Ending { status: RunStatus::Abandoned, reason: Some(AbandonReason::NoTerminal) })
}

/// Rules 2 and 3 as one decision: the first closing record closes, and the
/// first bookend terminal is the outcome over any other close. A session's
/// attempt and a peer's mission row both fold their closing records through
/// it, so the two cannot read one record sequence differently.
#[derive(Debug, Clone, Default)]
pub(crate) struct CloseFold {
    /// The first closing record: when, and what it implies.
    pub close: Option<(String, Ending)>,
    /// The first bookend terminal's ending.
    terminal: Option<Ending>,
}

impl CloseFold {
    /// Fold one closing record.
    pub fn fold(&mut self, ts: &str, ending: Ending, bookend_terminal: bool) {
        if self.close.is_none() {
            self.close = Some((ts.to_string(), ending));
        }
        if bookend_terminal && self.terminal.is_none() {
            self.terminal = Some(ending);
        }
    }

    /// How it ended: the bookend terminal over the first close.
    pub fn ending(&self) -> Option<Ending> {
        self.close.as_ref().map(|(_, e)| self.terminal.unwrap_or(*e))
    }
}

/// A bookend start, at either grain.
fn is_bookend_start(a: &FlowAction) -> bool {
    a.bookend().is_some_and(|b| b.edge == Edge::Start)
}

fn is_reopener(a: &FlowAction) -> bool {
    is_bookend_start(a) || matches!(a, FlowAction::BudgetWait | FlowAction::MissionStart | FlowAction::StepStart)
}

fn is_first_opener(a: &FlowAction) -> bool {
    matches!(a, FlowAction::DispatchTurn | FlowAction::DispatchTurnHeartbeat | FlowAction::DispatchTool | FlowAction::DispatchRest)
}

/// One attempt of a session (rule 1).
#[derive(Debug, Clone, Default)]
pub(crate) struct Attempt {
    pub mission: Option<String>,
    /// The execution of its first record that is of one; `None` for an
    /// attempt no execution has touched (a run's, a step's).
    execution: Option<ExecutionId>,
    pub has_start: bool,
    pub start_ts: Option<String>,
    pub waited: bool,
    /// Its newest timed record's `ts` (an unparsable one never counts).
    pub last_activity_ts: Option<String>,
    /// Its closing records, decided by rules 2 and 3.
    pub fold: CloseFold,
    /// While a `budget.wait` is open: when it lapses, epoch ms.
    pub wait_until_ms: Option<u64>,
}

impl Attempt {
    /// How it ended (rule 3), when it has closed.
    pub fn ending(&self) -> Option<Ending> {
        self.fold.ending()
    }

    fn add(&mut self, r: &Folded) {
        if self.mission.is_none() {
            self.mission.clone_from(&r.mission);
        }
        if self.execution.is_none() {
            self.execution.clone_from(&r.execution);
        }
        // Segmented in time order, so the newest timed record is the last.
        if r.at.is_some() {
            self.last_activity_ts = Some(r.ts.clone());
        }
        let Some(action) = r.action.as_ref() else { return };
        if is_bookend_start(action) && !self.has_start {
            self.has_start = true;
            self.start_ts = (!r.ts.is_empty()).then(|| r.ts.clone());
        }
        if let Some(ending) = r.ending() {
            self.close_with(&r.ts, ending, r.is_bookend_terminal());
        }
        self.fold_wait(action, r);
    }

    fn close_with(&mut self, ts: &str, ending: Ending, bookend_terminal: bool) {
        self.fold.fold(ts, ending, bookend_terminal);
        self.wait_until_ms = None;
    }

    /// Rule 4: a `budget.wait` opens a wait to its resume time plus the
    /// grace; `budget.resume` ends it (a closing record, in `close_with`).
    fn fold_wait(&mut self, action: &FlowAction, r: &Folded) {
        if *action == FlowAction::BudgetWait {
            self.waited = true;
            self.wait_until_ms = r.at.map(|secs| {
                secs.saturating_mul(1_000).saturating_add(r.wait_ms).saturating_add(crate::runs::BUDGET_WAIT_GRACE_MS)
            });
        } else if *action == FlowAction::BudgetResume {
            self.wait_until_ms = None;
        }
    }
}

/// The attempt a record joins (rule 1), by index.
fn target_for(attempts: &[Attempt], mission: Option<&str>, execution: Option<&ExecutionId>) -> Option<usize> {
    if let Some(own) = execution.and_then(|x| attempts.iter().rposition(|a| a.execution.as_ref() == Some(x))) {
        return Some(own);
    }
    let cur = attempts.len().checked_sub(1);
    let Some(m) = mission else {
        return attempts.iter().rposition(|a| a.fold.close.is_none()).or(cur);
    };
    let own = attempts.iter().rposition(|a| a.mission.as_deref() == Some(m));
    own.or_else(|| cur.filter(|&i| attempts[i].mission.is_none()))
}

/// Whether `action` opens a new attempt rather than joining `mine` (rule 1).
fn opens(attempts: &[Attempt], mine: Option<usize>, action: &FlowAction) -> bool {
    let Some(i) = mine else { return is_reopener(action) || is_first_opener(action) };
    let a = &attempts[i];
    is_reopener(action) && (a.fold.close.is_some() || (is_bookend_start(action) && a.has_start))
}

/// Rule 2's skew case: each closing record seen before anything of its
/// mission opened closes, in order, the first attempt of its mission (any,
/// when it names none) left with no close of its own.
fn place_orphans(attempts: &mut [Attempt], orphans: &[&Folded]) {
    for o in orphans {
        let home = attempts.iter_mut().find(|a| a.fold.close.is_none() && (o.mission.is_none() || a.mission == o.mission));
        if let (Some(a), Some(ending)) = (home, o.ending()) {
            a.fold.close = Some((o.ts.clone(), ending));
        }
    }
}

/// A session's records, kept as they are read, and segmented when sealed
/// (`seal`, rules 1 and 2) rather than on every read. Both are shared, so a
/// copy of a session (`SessionAgg::for_mission`) costs two reference
/// counts, not a copy of its records or a re-fold.
#[derive(Debug, Clone, Default)]
pub(crate) struct RunFold {
    records: Arc<Vec<Folded>>,
    attempts: Arc<Vec<Attempt>>,
}

/// A session's records as its attempts, time order.
fn segment(records: &[Folded]) -> Vec<Attempt> {
    let mut order: Vec<&Folded> = records.iter().collect();
    order.sort_by_key(|r| (r.at.is_none(), r.at));
    let mut attempts: Vec<Attempt> = Vec::new();
    let mut orphans: Vec<&Folded> = Vec::new();
    for r in order {
        let mut target = target_for(&attempts, r.mission.as_deref(), r.execution.as_ref());
        if r.action.as_ref().is_some_and(|a| opens(&attempts, target, a)) {
            attempts.push(Attempt { mission: r.mission.clone(), ..Attempt::default() });
            target = Some(attempts.len() - 1);
        }
        match target {
            Some(i) => attempts[i].add(r),
            None if r.ending().is_some() => orphans.push(r),
            None => {}
        }
    }
    place_orphans(&mut attempts, &orphans);
    attempts
}

impl RunFold {
    /// Keep one record (`ts` in the flow schema's spelling).
    pub fn fold(&mut self, action: Option<&FlowAction>, mission: Option<&str>, ts: &str, v: &serde_json::Value) {
        Arc::make_mut(&mut self.records).push(Folded::of(action, mission, ts, v));
    }

    /// Segment the records kept so far into attempts, once the fold has
    /// seen every record; reads until the next `seal` use these.
    pub fn seal(&mut self) {
        self.attempts = Arc::new(segment(&self.records));
    }

    /// The session's current attempt, whatever its mission (as of `seal`).
    pub fn latest(&self) -> Option<&Attempt> {
        self.attempts.last()
    }

    /// `mission`'s latest attempt on this session: how the session reads
    /// for that mission when several share it (as of `seal`).
    pub fn latest_of(&self, mission: &str) -> Option<&Attempt> {
        self.attempts.iter().rev().find(|a| a.mission.as_deref() == Some(mission))
    }

    /// How the session (`mission`'s records on it, when named) recorded its
    /// end when nothing of it opened (rule 2): not a run, but a session that
    /// recorded how it ended (the crash shape: the presence reconciler's
    /// `session.end`, the opening records in an older day or never written).
    /// An attempt holding only that close: its bookend terminal when it has
    /// one (rule 3), else its earliest closing record; `None` when nothing
    /// closed. Read only where no attempt opened (`latest`/`latest_of` are
    /// `None`): an opened session's closes are its attempts'.
    pub fn recorded_end(&self, mission: Option<&str>) -> Option<Attempt> {
        let mut closes: Vec<&Folded> =
            self.records.iter().filter(|r| r.ending().is_some() && (mission.is_none() || r.mission.as_deref() == mission)).collect();
        closes.sort_by_key(|r| (r.at.is_none(), r.at));
        let record = closes.iter().find(|r| r.is_bookend_terminal()).or(closes.first())?;
        let mut end = Attempt { mission: record.mission.clone(), ..Attempt::default() };
        end.add(record);
        Some(end)
    }
}

/// The one staleness rule: live while `now` is within the window of the
/// newest activity, or of an open wait's lapse when that is later (a wait
/// still ahead holds it live outright). No activity at all is not live.
pub(crate) fn quiet_clock_live(last_activity_ts: Option<&str>, wait_until_ms: Option<u64>, now_ms: u64, stale_after_ms: u64) -> bool {
    let last_ms = last_activity_ts.and_then(crate::runs::parse_flow_ts).map(|s| s.saturating_mul(1_000));
    let Some(quiet_from) = last_ms.max(wait_until_ms) else { return false };
    now_ms.saturating_sub(quiet_from) <= stale_after_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fold(f: &mut RunFold, action: FlowAction, mission: &str, ts: &str) {
        f.fold(Some(&action), Some(mission), ts, &serde_json::json!({}));
    }

    /// Reads use the attempts `seal` built: a record folded since is not
    /// segmented again until the next `seal`.
    #[test]
    fn reads_use_the_sealed_attempts_without_refolding() {
        let mut f = RunFold::default();
        fold(&mut f, FlowAction::DispatchStart, "a", "2026-09-27T10:00:00Z");
        f.seal();
        fold(&mut f, FlowAction::DispatchComplete, "a", "2026-09-27T10:01:00Z");
        assert!(f.latest().is_some_and(|a| a.fold.close.is_none()), "unsealed records are not read");
        f.seal();
        assert!(f.latest().is_some_and(|a| a.fold.close.is_some()), "a seal reads every record");
    }

    /// (#2101) A peer's run stays live through tool and turn records alone:
    /// the fleet stream no longer carries heartbeats, and the run's newest
    /// activity is its last timed record of any kind.
    #[test]
    fn a_run_stays_live_through_tool_and_turn_records_without_heartbeats() {
        let mut f = RunFold::default();
        fold(&mut f, FlowAction::DispatchStart, "a", "2026-09-27T10:00:00Z");
        fold(&mut f, FlowAction::DispatchTurn, "a", "2026-09-27T10:01:00Z");
        fold(&mut f, FlowAction::DispatchTool, "a", "2026-09-27T10:02:00Z");
        f.seal();
        let latest = f.latest().expect("an attempt");
        assert!(latest.fold.close.is_none(), "still open");
        assert_eq!(latest.last_activity_ts.as_deref(), Some("2026-09-27T10:02:00Z"));
        let at = |ts: &str| crate::runs::parse_flow_ts(ts).unwrap() * 1_000;
        let stale = 1_200_000;
        assert!(quiet_clock_live(latest.last_activity_ts.as_deref(), None, at("2026-09-27T10:12:00Z"), stale));
        assert!(!quiet_clock_live(latest.last_activity_ts.as_deref(), None, at("2026-09-27T10:23:00Z"), stale));
    }

    /// A copy of a sealed fold shares its records and attempts.
    #[test]
    fn a_copy_shares_records_and_attempts() {
        let mut f = RunFold::default();
        fold(&mut f, FlowAction::DispatchStart, "a", "2026-09-27T10:00:00Z");
        f.seal();
        let copy = f.clone();
        assert!(Arc::ptr_eq(&f.records, &copy.records));
        assert!(Arc::ptr_eq(&f.attempts, &copy.attempts));
    }
}
