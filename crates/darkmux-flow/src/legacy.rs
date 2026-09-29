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
//! event whichever path read it.

use crate::FlowAction;

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
    /// The retired `mission pause` / `mission resume` / `add-phase` verbs
    /// (#2954), in the dotted spelling and the pre-4.0 spaced ones. The
    /// verbs only flipped a status label, and nothing emits these now.
    // drift-guard:allow mission pause — the archived spelling, read only
    MissionPause => "mission.pause";
    // drift-guard:allow mission pause — the archived spelling, read only
    MissionPauseSpaced => "mission pause";
    MissionResume => "mission.resume";
    // drift-guard:allow mission resume — the archived spelling, read only
    MissionResumeSpaced => "mission resume";
    PhaseAdded => "phase.added";
    PhaseAddedSpaced => "phase added";
    PhaseAddedSprintSpaced => "sprint added";
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
    ("ambiguous-phase-id", FlowAction::PhaseIdAmbiguous),
    ("phase review begin", FlowAction::PhaseReviewBegin),
    ("sprint review begin", FlowAction::PhaseReviewBegin),
    ("phase review aborted", FlowAction::PhaseReviewAborted),
    ("dispatch code-reviewer", FlowAction::PhaseReviewDispatch),
    ("dispatch failed", FlowAction::PhaseReviewFailed),
    ("mission start", FlowAction::MissionStart),
    ("mission close", FlowAction::MissionClose),
    ("mission abort", FlowAction::MissionAbort),
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

/// A retired spelling that carried a value INSIDE the action string
/// (`verdict: clean`), and the payload key that value belongs under now.
pub fn detail_in_action(old: &str) -> Option<(&'static str, &str)> {
    old.strip_prefix("verdict: ").map(|verdict| ("verdict", verdict))
}
