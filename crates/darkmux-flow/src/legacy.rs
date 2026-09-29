//! Pre-4.0 action spellings, and the one read-side upgrade that maps them.
//!
//! Flow archives are append-only and never rewritten, so a 3.x day file
//! still holds the spellings 4.0 retired: the spaced bookends
//! (`dispatch start`), and every other action that did not fit the
//! `<scope>.<event>[.<detail>]` grammar. This module is the ONLY place those
//! spellings live, and the actions retired with no current equivalent
//! ([`RetiredAction`]). [`upgrade_action`] maps one to its [`FlowAction`];
//! [`FlowAction`]'s own deserializer and [`crate::reader`] both go through
//! it (through [`read_action`]), so a record reads with one spelling per
//! event whichever path read it. One upgrade needs the whole record, not
//! just its action: a pre-4.0 whole-run bookend, spelled as an execution's
//! and told apart by its `source`, reads as `run.*` ([`run_grain_of`],
//! applied by [`crate::reader`]). Another does too: a record of an
//! execution written before 4.0 names none, and [`stamp_execution`] gives
//! it the one synthesized identity.

use crate::{Bookend, FlowAction, Grain};

/// Read one wire string as it appears in a record of ANY age: a current
/// spelling, an old spelling of a current action (upgraded), a retired
/// action ([`FlowAction::Retired`]), or an unknown one (kept verbatim as
/// [`FlowAction::Other`]). The single read path for an action.
pub(crate) fn read_action(wire: &str) -> FlowAction {
    match FlowAction::from_wire(wire) {
        FlowAction::Other(unknown) => upgrade_action(wire)
            .or_else(|| RetiredAction::from_wire(wire).map(FlowAction::Retired))
            .unwrap_or(FlowAction::Other(unknown)),
        current => current,
    }
}

macro_rules! retired_actions {
    ( $( $(#[$meta:meta])* $variant:ident => $wire:literal; )* ) => {
        /// An action darkmux wrote before and retired with NO current
        /// equivalent (a retired spelling of a current action is upgraded
        /// instead, by [`upgrade_action`]). Read from archives only: the
        /// write path refuses it, and `darkmux doctor` does not count it as
        /// unknown.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum RetiredAction {
            $( $(#[$meta])* $variant, )*
        }

        impl RetiredAction {
            /// Every retired wire string, in declaration order.
            pub const KNOWN_WIRE: &'static [&'static str] = &[ $( $wire, )* ];

            /// The wire spelling it was written with.
            pub fn as_str(self) -> &'static str {
                match self {
                    $( RetiredAction::$variant => $wire, )*
                }
            }

            fn from_wire(s: &str) -> Option<RetiredAction> {
                match s {
                    $( $wire => Some(RetiredAction::$variant), )*
                    _ => None,
                }
            }
        }
    };
}

retired_actions! {
    /// Per-dispatch host samples (`cpu`/`gpu`/`mem`, session-scoped), retired
    /// at FLOW 1.42.0 (#2413). `machine.telemetry` replaced it with a
    /// machine-scoped sample of a different shape, so it does not upgrade.
    TelemetryProcess => "telemetry.process";
    /// The retired review funnel's records (#1247), deleted with the funnel.
    FunnelStep => "funnel.step";
    FunnelRuling => "funnel.ruling";
    FunnelTask => "funnel.task";
    /// The retired `mission run` / `mission ship` verbs (#782, #1426).
    MissionRunStart => "mission.run.start";
    MissionRunGate => "mission.run.gate";
    MissionRunError => "mission.run.error";
    MissionRunVerification => "mission.run.verification";
    MissionRunBlocked => "mission.run.blocked";
    MissionRunShip => "mission.run.ship";
    MissionRunShipMerged => "mission.run.ship.merged";
    /// Added in 9ce9c1864 (#799), removed with `mission ship` in a39a133e0
    /// (#1463).
    MissionRunShipHeld => "mission.run.ship.held";
    /// Added in 073d9adce (#782b), removed in 90c67d2e5 (#1348).
    MissionRunQaUnavailable => "mission.run.qa-unavailable";
    /// The retired `mission propose` compiler (#204, #2912).
    MissionCompileStart => "mission.compile.start";
    MissionCompileComplete => "mission.compile.complete";
    /// Added in ab7d75611 (#204), removed with `mission propose` in
    /// 3531f17ff (#2912).
    MissionCompileError => "mission.compile.error";
    /// Terminal-mission reopen (#1284), retired in #1503.
    MissionReopen => "mission reopen";
    /// The literal crawl launcher's records, retired with it (#2301).
    CrawlFinding => "crawl.finding";
    CrawlMissionStarted => "crawl.mission.started";
    CrawlMissionCompleted => "crawl.mission.completed";
    CrawlUnitStarted => "crawl.unit.started";
    CrawlUnitCompleted => "crawl.unit.completed";
}


/// Every old spelling of a current action, and the action it now is. The
/// pre-rename `sprint *` spellings are the Sprint→Phase rename's. A
/// `verdict: <v>` record is the one prefix-shaped spelling; see
/// [`upgrade_action`].
pub const OLD_SPELLINGS: &[(&str, FlowAction)] = &[
    ("dispatch start", FlowAction::DispatchStart),
    ("dispatch complete", FlowAction::DispatchComplete),
    ("dispatch error", FlowAction::DispatchError),
    ("dispatch route", FlowAction::DispatchRoute),
    ("step start", FlowAction::StepStart),
    ("step complete", FlowAction::StepComplete),
    ("step error", FlowAction::StepError),
    ("step result", FlowAction::StepResult),
    ("step timing", FlowAction::StepTiming),
    ("step seat unresolved", FlowAction::StepSeatUnresolved),
    ("phase start", FlowAction::PhaseStart),
    ("sprint start", FlowAction::PhaseStart),
    ("phase complete", FlowAction::PhaseComplete),
    ("sprint complete", FlowAction::PhaseComplete),
    ("phase abandon", FlowAction::PhaseAbandon),
    ("sprint abandon", FlowAction::PhaseAbandon),
    ("phase added", FlowAction::PhaseAdded),
    ("sprint added", FlowAction::PhaseAdded),
    ("ambiguous-phase-id", FlowAction::PhaseIdAmbiguous),
    ("phase review begin", FlowAction::PhaseReviewBegin),
    ("sprint review begin", FlowAction::PhaseReviewBegin),
    ("phase review aborted", FlowAction::PhaseReviewAborted),
    ("dispatch code-reviewer", FlowAction::PhaseReviewDispatch),
    ("dispatch failed", FlowAction::PhaseReviewFailed),
    ("mission start", FlowAction::MissionStart),
    ("mission close", FlowAction::MissionClose),
    ("mission abort", FlowAction::MissionAbort),
    ("mission pause", FlowAction::MissionPause),
    ("mission resume", FlowAction::MissionResume),
    ("tier-decision", FlowAction::TierDecision),
    ("note", FlowAction::OperatorNote),
    ("catch", FlowAction::OperatorCatch),
];

/// The current action for an old spelling, or `None` when `old` is not one.
/// Never consulted on write: producers build [`FlowAction`] directly.
pub fn upgrade_action(old: &str) -> Option<FlowAction> {
    if old.starts_with("verdict: ") {
        return Some(FlowAction::PhaseReviewVerdict);
    }
    OLD_SPELLINGS.iter().find(|(spelling, _)| *spelling == old).map(|(_, action)| action.clone())
}

/// The run-grain action a pre-4.0 whole-run bookend now is; `None` for any
/// other record.
///
/// Before the run grain had its own vocabulary (CLAUDE.md contract 8), a
/// mission launch and an ACP panel run (`source: "mission"`, since #1877)
/// and the retired review launcher (`source: "review"`) bracketed a whole
/// run in the execution bookends, `dispatch.start` / `complete` / `error`,
/// told apart from an execution only by `source`. Such a record reads as
/// `run.*`; `action` is the record's action as [`read_action`] read it.
pub(crate) fn run_grain_of(action: &FlowAction, record: &serde_json::Value) -> Option<FlowAction> {
    let whole_run = matches!(
        record.get("source").and_then(serde_json::Value::as_str),
        Some(WHOLE_RUN_SOURCE_MISSION | WHOLE_RUN_SOURCE_REVIEW)
    );
    if !whole_run {
        return None;
    }
    let Bookend { grain: Grain::Execution, edge } = action.bookend()? else {
        return None;
    };
    Some(Bookend { grain: Grain::Run, edge }.action())
}

/// The execution a record of any age is of: the one it names, else the one
/// [`darkmux_types::execution_id::ExecutionId::legacy`] synthesizes from its
/// session, mission, `ts`, `handle` and machine. The one place that mapping is
/// applied, by [`stamp_execution`] on read and by a consumer holding a
/// record that never went through the reader.
pub fn execution_of(record: &serde_json::Value) -> darkmux_types::execution_id::ExecutionId {
    let text = |key: &str| record.get(key).and_then(serde_json::Value::as_str);
    text("execution_id")
        .and_then(|named| darkmux_types::execution_id::ExecutionId::parse(named).ok())
        .unwrap_or_else(|| {
            darkmux_types::execution_id::ExecutionId::legacy(
                text("session_id"),
                text("mission_id"),
                text("ts").unwrap_or_default(),
                text("handle").unwrap_or_default(),
                text("machine_uid").unwrap_or_default(),
            )
        })
}

/// Give a record OF an execution that names none its synthesized identity
/// ([`execution_of`]), in place. Whether a record is of an execution is its
/// action's declared grain ([`FlowAction::grain`]); `action` is the record's
/// action as [`crate::reader::action_of`] read it, so a pre-4.0 whole-run
/// bookend (now `run.*`) gets none. Returns whether it wrote one; a record
/// that already names its execution is left as it is.
pub(crate) fn stamp_execution(record: &mut serde_json::Value, action: &FlowAction) -> bool {
    if action.grain() != Some(Grain::Execution) || record.get("execution_id").is_some_and(|v| !v.is_null()) {
        return false;
    }
    record["execution_id"] = serde_json::Value::String(execution_of(record).to_string());
    true
}

/// The `source` a pre-4.0 mission launch or ACP panel run stamped on its
/// whole-run bookend.
const WHOLE_RUN_SOURCE_MISSION: &str = "mission";
/// The `source` the retired review launcher stamped on its whole-run bookend.
const WHOLE_RUN_SOURCE_REVIEW: &str = "review";

/// A retired spelling that carried a value INSIDE the action string
/// (`verdict: clean`), and the payload key that value belongs under now.
pub fn detail_in_action(old: &str) -> Option<(&'static str, &str)> {
    old.strip_prefix("verdict: ").map(|verdict| ("verdict", verdict))
}
