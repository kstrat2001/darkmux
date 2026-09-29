//! Pre-4.0 spellings, and the one read-side upgrade that maps them.
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

use crate::{Bookend, FlowAction, FlowSource, Grain, Tier};
use serde_json::Value;

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

/// Every old spelling of a current `source`, and the source it now is. The
/// `kebab-case` and `sprint_*` spellings are the ones the closed
/// [`FlowSource`] retired; `process` was the per-dispatch host sampler's,
/// whose records are host telemetry. A source with no entry here (a retired
/// launcher's, or a newer build's) is left as written and reads as
/// [`FlowSource::Unknown`].
pub const OLD_SOURCES: &[(&str, FlowSource)] = &[
    ("host-sampler", FlowSource::HostSampler),
    ("presence-reconciler", FlowSource::PresenceReconciler),
    ("cmd-gate-audit", FlowSource::CmdGateAudit),
    ("sprint_lifecycle", FlowSource::PhaseLifecycle),
    ("sprint_review", FlowSource::PhaseReview),
    ("frontier-orchestrator", FlowSource::Frontier),
    ("process", FlowSource::Host),
];

/// Every old spelling of a current `tier`: it named where the model ran, and
/// every record darkmux itself wrote said `local`, hosted endpoints included.
pub const OLD_TIERS: &[(&str, Tier)] = &[("local", Tier::Darkmux)];

/// The current action for an old spelling, or `None` when `old` is not one.
/// Never consulted on write: producers build [`FlowAction`] directly.
pub fn upgrade_action(old: &str) -> Option<FlowAction> {
    if old.starts_with("verdict: ") {
        return Some(FlowAction::PhaseReviewVerdict);
    }
    OLD_SPELLINGS.iter().find(|(spelling, _)| *spelling == old).map(|(_, action)| action.clone())
}

/// How an old payload value becomes the current one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ValueChange {
    /// A number (or an array of numbers) times this factor: a duration in a
    /// coarser or finer unit, now milliseconds.
    Scale(f64),
    /// An ISO `ts`-shaped string, now epoch milliseconds.
    IsoToEpochMs,
}

/// One payload key an action's records spelled another way before 4.0. Time
/// is `*_ms` for a duration and `*_at_ms` for an instant, in epoch
/// milliseconds; these were the keys that used seconds, hours, microseconds
/// or an ISO string.
#[derive(Debug, Clone, Copy)]
pub struct PayloadRename {
    /// The actions whose payload carries the old key.
    pub actions: &'static [FlowAction],
    /// The object inside `payload` holding the key, when it is nested.
    pub within: Option<&'static str>,
    pub old: &'static str,
    pub new: &'static str,
    pub change: ValueChange,
}

const HOURS_TO_MS: f64 = 3_600_000.0;

/// Every renamed payload key. An old key is renamed on read, its value
/// converted; a record already carrying the new key is left as it is.
pub const OLD_PAYLOAD_KEYS: &[PayloadRename] = &[
    PayloadRename {
        actions: &[FlowAction::BudgetWait],
        within: None,
        old: "wait_seconds",
        new: "wait_ms",
        change: ValueChange::Scale(1_000.0),
    },
    PayloadRename {
        actions: &[FlowAction::BudgetWait],
        within: None,
        old: "resume_at",
        new: "resume_at_ms",
        change: ValueChange::IsoToEpochMs,
    },
    PayloadRename {
        actions: &[FlowAction::UtilityStart],
        within: None,
        old: "stall_after_seconds",
        new: "stall_after_ms",
        change: ValueChange::Scale(1_000.0),
    },
    PayloadRename {
        actions: &[FlowAction::MachineRollup],
        within: None,
        old: "period_seconds",
        new: "period_ms",
        change: ValueChange::Scale(1_000.0),
    },
    PayloadRename {
        actions: &[FlowAction::DispatchComplete],
        within: Some("live"),
        old: "sampler_us",
        new: "sampler_ms",
        change: ValueChange::Scale(0.001),
    },
    PayloadRename {
        actions: &[FlowAction::DispatchComplete],
        within: Some("live"),
        old: "forward_us",
        new: "forward_ms",
        change: ValueChange::Scale(0.001),
    },
    PayloadRename {
        actions: &[FlowAction::MachineBatteryHealth],
        within: None,
        old: "total_operating_time_hours",
        new: "total_operating_ms",
        change: ValueChange::Scale(HOURS_TO_MS),
    },
    PayloadRename {
        actions: &[FlowAction::MachineBatteryHealth],
        within: None,
        old: "time_at_soc_hours",
        new: "time_at_soc_ms",
        change: ValueChange::Scale(HOURS_TO_MS),
    },
];

/// Rename the old payload keys of a record of `action`, in place. Returns
/// whether it changed anything.
pub(crate) fn upgrade_payload(record: &mut Value, action: &FlowAction) -> bool {
    let Some(payload) = record.get_mut("payload") else { return false };
    let mut changed = false;
    for rename in OLD_PAYLOAD_KEYS.iter().filter(|r| r.actions.contains(action)) {
        let holder = match rename.within {
            Some(key) => payload.get_mut(key),
            None => Some(&mut *payload),
        };
        if let Some(Value::Object(map)) = holder {
            changed |= rename_key(map, rename);
        }
    }
    changed
}

/// Move `rename.old` to `rename.new` in `map` with its value converted.
/// Nothing moves when the new key is already there, or the value has no
/// reading (it stays under its old key: an archive is not ours to discard).
fn rename_key(map: &mut serde_json::Map<String, Value>, rename: &PayloadRename) -> bool {
    if map.contains_key(rename.new) {
        return false;
    }
    let Some(converted) = map.get(rename.old).and_then(|v| convert(v, rename.change)) else {
        return false;
    };
    map.remove(rename.old);
    map.insert(rename.new.to_string(), converted);
    true
}

fn convert(value: &Value, change: ValueChange) -> Option<Value> {
    match (change, value) {
        (ValueChange::Scale(factor), Value::Array(items)) => {
            items.iter().map(|v| scaled(v, factor)).collect::<Option<Vec<_>>>().map(Value::Array)
        }
        (ValueChange::Scale(factor), v) => scaled(v, factor),
        (ValueChange::IsoToEpochMs, Value::String(iso)) => {
            crate::parse_ts_utc(iso).map(|secs| Value::from(secs.saturating_mul(1_000)))
        }
        (ValueChange::IsoToEpochMs, _) => None,
    }
}

/// A number times `factor`: an integer when the product is whole, else a
/// float. `null` (an absent reading) stays `null`.
fn scaled(value: &Value, factor: f64) -> Option<Value> {
    if value.is_null() {
        return Some(Value::Null);
    }
    let product = value.as_f64()? * factor;
    if product.fract() == 0.0 && product.abs() < 9.0e15 {
        Some(Value::from(product as i64))
    } else {
        serde_json::Number::from_f64(product).map(Value::Number)
    }
}

/// The [`FlowSource`] a wire string is, current or old: a spelling that maps
/// nowhere is [`FlowSource::Unknown`].
pub fn read_source(wire: &str) -> FlowSource {
    let current = serde_json::from_value(Value::String(wire.to_string())).unwrap_or(FlowSource::Unknown);
    match (current, OLD_SOURCES.iter().find(|(spelling, _)| *spelling == wire)) {
        (FlowSource::Unknown, Some((_, now))) => *now,
        (current, _) => current,
    }
}

/// Rewrite the retired `source` and `tier` spellings of a record, in place.
/// Returns whether it changed anything. The action is upgraded separately
/// ([`upgrade_action`]); this is every other field's old spelling.
pub(crate) fn upgrade_fields(record: &mut Value) -> bool {
    let mut changed = rewrite_field(record, "source", |old| {
        OLD_SOURCES.iter().find(|(spelling, _)| *spelling == old).and_then(|(_, now)| serde_json::to_value(now).ok())
    });
    changed |= rewrite_field(record, "tier", |old| {
        OLD_TIERS.iter().find(|(spelling, _)| *spelling == old).and_then(|(_, now)| serde_json::to_value(now).ok())
    });
    changed
}

/// Replace `record[key]` with `upgrade(old)` when it is a string that has one.
fn rewrite_field(record: &mut Value, key: &str, upgrade: impl Fn(&str) -> Option<Value>) -> bool {
    let Some(now) = record.get(key).and_then(Value::as_str).and_then(upgrade) else {
        return false;
    };
    record[key] = now;
    true
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
