//! A session's run lifecycle, folded one record at a time: the daemon's
//! executor of the rules `ui/src/lib/lifecycle.ts` states for the viewer.
//! One spec (`tests/lifecycle/cases.json`) is asserted against both.
//!
//! 1. Attempts. A session's records segment into attempts, every mission's
//!    together in arrival order. An attempt opens on its first opening record
//!    (a `dispatch.start`, `budget.wait`, `mission.start` or `step.start`,
//!    or, when nothing of its mission opened yet, a turn, heartbeat, tool
//!    call or rest). A record naming a mission joins that mission's latest
//!    attempt (or adopts the current one when it names none yet); one naming
//!    none joins the attempt open at its time. A `dispatch.start` in an
//!    attempt that already has one, or a reopening record after the attempt
//!    closed, starts the next attempt.
//! 2. Close. An attempt closes on its first closing record. A closing record
//!    seen before anything opened closes the first attempt left with none.
//! 3. Outcome. A dispatch terminal is the outcome when the attempt has one,
//!    even when a `session.end` closed it first.
//! 4. Waiting. An open `budget.wait` holds the attempt live until its
//!    announced resume time plus the grace; the staleness clock then runs
//!    from there.
//! 5. Stale. An open attempt silent past the window, or one a later attempt
//!    of its session superseded, has stopped with no ending recorded.
//!
//! `/runs` judges a session by its current attempt (`SessionAgg::settle`).
//! Session presence is the one input the daemon lacks (the viewer holds a
//! session's current run open on it).

use crate::runs::{AbandonReason, RunStatus};
use darkmux_flow::FlowAction;

/// How a closed attempt ended: its status, and why when abandoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ending {
    pub status: RunStatus,
    pub reason: Option<AbandonReason>,
}

/// The ending a closing record implies; `None` for any other record.
fn ending_of(action: &FlowAction, v: &serde_json::Value) -> Option<Ending> {
    let ended = |status, reason| Some(Ending { status, reason });
    if matches!(action, FlowAction::DispatchComplete | FlowAction::StepComplete | FlowAction::MissionClose) {
        return ended(RunStatus::Complete, None);
    }
    if matches!(action, FlowAction::DispatchError | FlowAction::StepError) {
        return ended(RunStatus::Error, None);
    }
    if *action == FlowAction::SessionEnd {
        return ended(RunStatus::Abandoned, Some(AbandonReason::NoTerminal));
    }
    if *action == FlowAction::MissionAbort || (*action == FlowAction::BudgetStop && names_a_reason(v)) {
        return ended(RunStatus::Abandoned, Some(AbandonReason::Aborted));
    }
    (*action == FlowAction::BudgetStop).then_some(Ending { status: RunStatus::Abandoned, reason: Some(AbandonReason::NoTerminal) })
}

/// Whether a record's payload names a non-empty `reason`: every reason a
/// `budget.stop` producer writes is an operator's stop (an interrupt,
/// `mission abort`/`finalize`, an abandoned phase).
fn names_a_reason(v: &serde_json::Value) -> bool {
    darkmux_crew::usage::payload_of(v).get("reason").and_then(|r| r.as_str()).is_some_and(|r| !r.is_empty())
}

fn is_reopener(a: &FlowAction) -> bool {
    matches!(a, FlowAction::DispatchStart | FlowAction::BudgetWait | FlowAction::MissionStart | FlowAction::StepStart)
}

fn is_first_opener(a: &FlowAction) -> bool {
    matches!(a, FlowAction::DispatchTurn | FlowAction::DispatchTurnHeartbeat | FlowAction::DispatchTool | FlowAction::DispatchRest)
}

/// One attempt of a session (rule 1).
#[derive(Debug, Clone, Default)]
pub(crate) struct Attempt {
    pub mission: Option<String>,
    pub has_start: bool,
    pub start_ts: Option<String>,
    pub waited: bool,
    /// Its newest timed record's `ts` (an unparsable one never counts).
    pub last_activity_ts: Option<String>,
    /// Its first closing record: when, and what it implies.
    pub close: Option<(String, Ending)>,
    /// Its first dispatch terminal: the outcome over any other close (rule 3).
    terminal: Option<Ending>,
    /// While a `budget.wait` is open: when it lapses, epoch ms.
    pub wait_until_ms: Option<u64>,
}

impl Attempt {
    /// How it ended (rule 3), when it has closed.
    pub fn ending(&self) -> Option<Ending> {
        self.close.as_ref().map(|(_, e)| self.terminal.unwrap_or(*e))
    }

    fn add(&mut self, action: Option<&FlowAction>, mission: Option<&str>, ts: &str, v: &serde_json::Value) {
        if self.mission.is_none() {
            self.mission = mission.map(str::to_string);
        }
        // ISO-8601 `YYYY-MM-DDTHH:MM:SSZ` sorts as a plain string, so a
        // lexical compare keeps the newest even out of arrival order.
        if crate::runs::parse_flow_ts(ts).is_some() && self.last_activity_ts.as_deref().is_none_or(|cur| ts > cur) {
            self.last_activity_ts = Some(ts.to_string());
        }
        let Some(action) = action else { return };
        self.add_action(action, ts, v);
    }

    fn add_action(&mut self, action: &FlowAction, ts: &str, v: &serde_json::Value) {
        if *action == FlowAction::DispatchStart && !self.has_start {
            self.has_start = true;
            self.start_ts = (!ts.is_empty()).then(|| ts.to_string());
        }
        if let Some(ending) = ending_of(action, v) {
            self.close_with(ts, ending, matches!(action, FlowAction::DispatchComplete | FlowAction::DispatchError));
        }
        self.fold_wait(action, ts, v);
    }

    fn close_with(&mut self, ts: &str, ending: Ending, dispatch_terminal: bool) {
        if self.close.is_none() {
            self.close = Some((ts.to_string(), ending));
        }
        if dispatch_terminal && self.terminal.is_none() {
            self.terminal = Some(ending);
        }
        self.wait_until_ms = None;
    }

    /// Rule 4: a `budget.wait` opens a wait to its resume time plus the
    /// grace; `budget.resume` ends it (a closing record, in `close_with`).
    fn fold_wait(&mut self, action: &FlowAction, ts: &str, v: &serde_json::Value) {
        if *action == FlowAction::BudgetWait {
            self.waited = true;
            self.wait_until_ms = crate::runs::parse_flow_ts(ts).map(|secs| {
                let wait_secs = darkmux_crew::usage::payload_of(v).get("wait_seconds").and_then(|w| w.as_f64()).unwrap_or(0.0).max(0.0);
                secs.saturating_mul(1_000).saturating_add((wait_secs * 1_000.0) as u64).saturating_add(crate::runs::BUDGET_WAIT_GRACE_MS)
            });
        } else if *action == FlowAction::BudgetResume {
            self.wait_until_ms = None;
        }
    }
}

/// A closing record seen before anything of its mission opened.
#[derive(Debug, Clone)]
struct Orphan {
    mission: Option<String>,
    ts: String,
    ending: Ending,
}

/// A session's attempts, folded in arrival order.
#[derive(Debug, Clone, Default)]
pub(crate) struct RunFold {
    attempts: Vec<Attempt>,
    orphans: Vec<Orphan>,
}

impl RunFold {
    /// The attempt a record joins (rule 1), by index.
    fn target_for(&self, mission: Option<&str>) -> Option<usize> {
        let cur = self.attempts.len().checked_sub(1);
        let Some(m) = mission else { return cur };
        let own = self.attempts.iter().rposition(|a| a.mission.as_deref() == Some(m));
        own.or_else(|| cur.filter(|&i| self.attempts[i].mission.is_none()))
    }

    fn opens(&self, mine: Option<usize>, action: &FlowAction) -> bool {
        let Some(i) = mine else { return is_reopener(action) || is_first_opener(action) };
        let a = &self.attempts[i];
        is_reopener(action) && (a.close.is_some() || (*action == FlowAction::DispatchStart && a.has_start))
    }

    /// Fold one record (`ts` in the flow schema's spelling).
    pub fn fold(&mut self, action: Option<&FlowAction>, mission: Option<&str>, ts: &str, v: &serde_json::Value) {
        let mut target = self.target_for(mission);
        if action.is_some_and(|a| self.opens(target, a)) {
            self.attempts.push(Attempt { mission: mission.map(str::to_string), ..Attempt::default() });
            target = Some(self.attempts.len() - 1);
        }
        match target {
            Some(i) => self.attempts[i].add(action, mission, ts, v),
            None => self.fold_orphan(action, mission, ts, v),
        }
    }

    /// Rule 2's skew case: a closing record seen before anything of its
    /// mission opened is held, and closes the first attempt left with no
    /// close of its own (`attempt`).
    fn fold_orphan(&mut self, action: Option<&FlowAction>, mission: Option<&str>, ts: &str, v: &serde_json::Value) {
        if let Some(ending) = action.and_then(|a| ending_of(a, v)) {
            self.orphans.push(Orphan { mission: mission.map(str::to_string), ts: ts.to_string(), ending });
        }
    }

    /// Attempt `i` with any orphan close it takes (rule 2): orphans go, in
    /// order, to the first attempts of their mission (any, when they name
    /// none) that have no close of their own.
    fn attempt(&self, i: usize) -> Attempt {
        let mut placed: Vec<Attempt> = self.attempts.clone();
        for o in &self.orphans {
            let home = placed.iter_mut().find(|a| a.close.is_none() && (o.mission.is_none() || a.mission == o.mission));
            if let Some(a) = home {
                a.close = Some((o.ts.clone(), o.ending));
            }
        }
        placed.swap_remove(i)
    }

    /// The session's current attempt, whatever its mission.
    pub fn latest(&self) -> Option<Attempt> {
        self.attempts.len().checked_sub(1).map(|i| self.attempt(i))
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
