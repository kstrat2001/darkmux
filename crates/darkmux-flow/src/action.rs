//! The flow-record action vocabulary: one closed enum, one wire spelling per
//! event.
//!
//! Every action darkmux writes is a [`FlowAction`] variant, and every wire
//! string follows one grammar: `<scope>.<event>[.<detail>]`, lowercase ASCII
//! segments (`[a-z0-9_]`), two or three of them, dot-separated. The first
//! segment is the variant's [`FlowScope`], declared beside the variant rather
//! than split out of the string, and the unit tests pin the two together.
//!
//! The one list lives in the `flow_actions!` invocation below. A later typed
//! payload per action attaches there: each row gains its payload type, and the
//! macro grows one `match` that maps a variant to it.
//!
//! [`FlowAction::Retired`] is an action darkmux once wrote and retired with
//! no current equivalent (`telemetry.process`, the pre-graph `funnel.*` and
//! `crawl.*` records, ...): known, readable, never written.
//!
//! [`FlowAction::Other`] exists only for READING: an archive may hold an
//! action this binary does not know (a newer writer's record, or an
//! old spelling [`crate::legacy`] has no mapping for). Its field is private,
//! so no code outside this crate can build one, and no writer can emit a
//! string that is not in the list.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

macro_rules! flow_scopes {
    ( $( $variant:ident => $wire:literal; )* ) => {
        /// The subject an action is about: the first segment of its wire
        /// string.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum FlowScope {
            $( $variant, )*
        }

        impl FlowScope {
            /// Every scope's wire segment, in declaration order.
            pub const KNOWN_WIRE: &'static [&'static str] = &[ $( $wire, )* ];

            /// The scope's wire segment.
            pub fn as_str(self) -> &'static str {
                match self {
                    $( FlowScope::$variant => $wire, )*
                }
            }
        }
    };
}

flow_scopes! {
    Audit => "audit";
    Battery => "battery";
    Budget => "budget";
    Dispatch => "dispatch";
    Gh => "gh";
    Hook => "hook";
    Machine => "machine";
    Mission => "mission";
    Operator => "operator";
    Phase => "phase";
    Radio => "radio";
    Session => "session";
    Step => "step";
    Stream => "stream";
    Telemetry => "telemetry";
    Thermal => "thermal";
    Tier => "tier";
    Utility => "utility";
}

/// An action this binary does not know, kept verbatim so the record still
/// reads. Built only by the deserializer; see the module doc.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UnknownAction(String);

impl UnknownAction {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

macro_rules! flow_actions {
    ( $( $(#[$meta:meta])* $variant:ident => $scope:ident, $wire:literal; )* ) => {
        /// A flow record's action. See the module doc for the grammar.
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub enum FlowAction {
            $( $(#[$meta])* $variant, )*
            /// An action darkmux once wrote and no longer does, read from an
            /// archive. Known, never written (see
            /// [`crate::legacy::RetiredAction`]).
            Retired(crate::legacy::RetiredAction),
            /// An action read from a record this binary has no variant for.
            Other(UnknownAction),
        }

        impl FlowAction {
            /// Every known wire string, in declaration order.
            pub const KNOWN_WIRE: &'static [&'static str] = &[ $( $wire, )* ];

            /// Every variant's Rust name, aligned with [`Self::KNOWN_WIRE`]:
            /// for a source scanner that meets `FlowAction::<Name>` and needs
            /// the wire string it writes.
            #[doc(hidden)]
            pub const VARIANT_NAMES: &'static [&'static str] = &[ $( stringify!($variant), )* ];

            /// The wire spelling.
            pub fn as_str(&self) -> &str {
                match self {
                    $( FlowAction::$variant => $wire, )*
                    FlowAction::Retired(r) => r.as_str(),
                    FlowAction::Other(u) => u.as_str(),
                }
            }

            /// The scope this action belongs to; `None` for an unknown one.
            pub fn scope(&self) -> Option<FlowScope> {
                match self {
                    $( FlowAction::$variant => Some(FlowScope::$scope), )*
                    FlowAction::Retired(_) | FlowAction::Other(_) => None,
                }
            }

            /// Parse a CURRENT wire string. An unknown string becomes
            /// [`FlowAction::Other`]; a retired spelling is
            /// [`crate::legacy::read_action`]'s job, not this one's.
            pub(crate) fn from_wire(s: &str) -> FlowAction {
                match s {
                    $( $wire => FlowAction::$variant, )*
                    other => FlowAction::Other(UnknownAction(other.to_string())),
                }
            }

            /// Parse a current wire string, refusing an unknown one. The
            /// boundary for operator input (`darkmux flow record --action`).
            pub fn parse_known(s: &str) -> Result<FlowAction, String> {
                match FlowAction::from_wire(s) {
                    FlowAction::Other(_) => Err(format!(
                        "unknown flow action `{s}`; known actions: {}",
                        FlowAction::KNOWN_WIRE.join(", ")
                    )),
                    known => Ok(known),
                }
            }
        }
    };
}

flow_actions! {
    AuditWriteFailed => Audit, "audit.write_failed";
    BatteryPauseUnsupported => Battery, "battery.pause_unsupported";
    /// A budget was reached (or its `warn_at` fraction was) under `warn`, or
    /// a per-step cap was crossed; the call went ahead.
    BudgetWarn => Budget, "budget.warn";
    /// A call is waiting on a budget (`wait`).
    BudgetWait => Budget, "budget.wait";
    /// A waiting call went ahead.
    BudgetResume => Budget, "budget.resume";
    /// A budget wait ended because its run was stopped; nothing was sent.
    BudgetStop => Budget, "budget.stop";
    DispatchStart => Dispatch, "dispatch.start";
    DispatchComplete => Dispatch, "dispatch.complete";
    DispatchError => Dispatch, "dispatch.error";
    DispatchTurn => Dispatch, "dispatch.turn";
    DispatchTurnHeartbeat => Dispatch, "dispatch.turn.heartbeat";
    DispatchTool => Dispatch, "dispatch.tool";
    DispatchCompaction => Dispatch, "dispatch.compaction";
    DispatchCheckpoint => Dispatch, "dispatch.checkpoint";
    DispatchReasoning => Dispatch, "dispatch.reasoning";
    DispatchFeedbackInjected => Dispatch, "dispatch.feedback.injected";
    DispatchRest => Dispatch, "dispatch.rest";
    DispatchDegeneracyWarning => Dispatch, "dispatch.degeneracy.warning";
    DispatchWorkdirGitUnavailable => Dispatch, "dispatch.workdir_git_unavailable";
    DispatchRoute => Dispatch, "dispatch.route";
    GhVerbExecuted => Gh, "gh.verb.executed";
    HookFired => Hook, "hook.fired";
    HookFailed => Hook, "hook.failed";
    HookDryRun => Hook, "hook.dry_run";
    MachineOnline => Machine, "machine.online";
    MachineOffline => Machine, "machine.offline";
    MachineTelemetry => Machine, "machine.telemetry";
    MachineThermal => Machine, "machine.thermal";
    MachineBattery => Machine, "machine.battery";
    MachineBatteryHealth => Machine, "machine.battery_health";
    MachineRollup => Machine, "machine.rollup";
    MissionStart => Mission, "mission.start";
    MissionClose => Mission, "mission.close";
    MissionAbort => Mission, "mission.abort";
    MissionGrow => Mission, "mission.grow";
    MissionDebriefPrompt => Mission, "mission.debrief.prompt";
    MissionRunFinalize => Mission, "mission.run.finalize";
    MissionRunAbort => Mission, "mission.run.abort";
    OperatorNote => Operator, "operator.note";
    OperatorCatch => Operator, "operator.catch";
    PhaseStart => Phase, "phase.start";
    PhaseComplete => Phase, "phase.complete";
    PhaseAbandon => Phase, "phase.abandon";
    PhaseIdAmbiguous => Phase, "phase.id_ambiguous";
    PhaseReviewBegin => Phase, "phase.review.begin";
    PhaseReviewAborted => Phase, "phase.review.aborted";
    PhaseReviewDispatch => Phase, "phase.review.dispatch";
    PhaseReviewFailed => Phase, "phase.review.failed";
    PhaseReviewVerdict => Phase, "phase.review.verdict";
    RadioRoute => Radio, "radio.route";
    SessionEnd => Session, "session.end";
    StepStart => Step, "step.start";
    StepComplete => Step, "step.complete";
    StepError => Step, "step.error";
    StepResult => Step, "step.result";
    StepTiming => Step, "step.timing";
    StepSeatUnresolved => Step, "step.seat_unresolved";
    StreamError => Stream, "stream.error";
    TelemetryTokens => Telemetry, "telemetry.tokens";
    TelemetryDetector => Telemetry, "telemetry.detector";
    TelemetryContext => Telemetry, "telemetry.context";
    TelemetryCompaction => Telemetry, "telemetry.compaction";
    TelemetryRuntime => Telemetry, "telemetry.runtime";
    TelemetryLms => Telemetry, "telemetry.lms";
    TierDecision => Tier, "tier.decision";
    ThermalStopUnresolved => Thermal, "thermal.stop_unresolved";
    ThermalTier5Eject => Thermal, "thermal.tier5_eject";
    ThermalTier5EjectFailed => Thermal, "thermal.tier5_eject_failed";
    UtilityStart => Utility, "utility.start";
    UtilityError => Utility, "utility.error";
}

impl fmt::Display for FlowAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for FlowAction {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for FlowAction {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Strict: a current spelling, or an old spelling of a current
        // action. An unknown or retired action is REFUSED here, so no code
        // outside this crate can mint one through serde; the lenient
        // archive read is `crate::reader`'s, and it is crate-private.
        let s = String::deserialize(d)?;
        match crate::legacy::read_action(&s) {
            FlowAction::Other(_) => Err(serde::de::Error::custom(format!("unknown flow action `{s}`"))),
            FlowAction::Retired(_) => Err(serde::de::Error::custom(format!("retired flow action `{s}`"))),
            known => Ok(known),
        }
    }
}

/// The TypeScript bindings: each enum exports as the union of its wire
/// strings, generated from the same list the Rust side matches on.
#[cfg(feature = "ts-export")]
mod ts {
    use super::{FlowAction, FlowScope};
    use crate::legacy::RetiredAction;
    use std::path::Path;

    fn union(wires: &[&str]) -> String {
        wires.iter().map(|w| format!("\"{w}\"")).collect::<Vec<_>>().join(" | ")
    }

    macro_rules! ts_union {
        ($ty:ident, $file:literal, $docs:literal) => {
            impl ts_rs::TS for $ty {
                type WithoutGenerics = Self;
                const DOCS: Option<&'static str> = Some($docs);
                fn name() -> String {
                    stringify!($ty).to_string()
                }
                fn decl() -> String {
                    format!("type {} = {};", Self::name(), Self::inline())
                }
                fn decl_concrete() -> String {
                    Self::decl()
                }
                fn inline() -> String {
                    union($ty::KNOWN_WIRE)
                }
                fn inline_flattened() -> String {
                    panic!("{} cannot be flattened", Self::name())
                }
                fn output_path() -> Option<&'static Path> {
                    Some(Path::new(concat!("../../../ui/src/types/generated/", $file)))
                }
            }
        };
    }

    ts_union!(
        FlowAction,
        "FlowAction.ts",
        "/**\n * A flow record's action: `<scope>.<event>[.<detail>]`. The daemon serves\n * every record with one of these spellings; an archive may still hold an\n * action this list does not name, which reads as a plain string.\n */\n"
    );
    ts_union!(
        FlowScope,
        "FlowScope.ts",
        "/**\n * The first segment of a flow action: the subject the action is about.\n */\n"
    );

    ts_union!(
        RetiredAction,
        "RetiredAction.ts",
        "/**\n * An action darkmux wrote before 4.0 and retired with no current\n * equivalent. An archive may still hold it; nothing writes it.\n */\n"
    );

    #[test]
    fn export_bindings_retiredaction() {
        <RetiredAction as ts_rs::TS>::export_all().expect("could not export RetiredAction");
    }

    #[test]
    fn export_bindings_flowaction() {
        <FlowAction as ts_rs::TS>::export_all().expect("could not export FlowAction");
    }

    #[test]
    fn export_bindings_flowscope() {
        <FlowScope as ts_rs::TS>::export_all().expect("could not export FlowScope");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The grammar: 2 or 3 dot-separated segments of `[a-z0-9_]`, each
    /// starting with a letter.
    fn fits_grammar(wire: &str) -> bool {
        let segs: Vec<&str> = wire.split('.').collect();
        (2..=3).contains(&segs.len())
            && segs.iter().all(|seg| {
                seg.starts_with(|c: char| c.is_ascii_lowercase())
                    && seg.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            })
    }

    #[test]
    fn every_wire_string_fits_the_grammar() {
        for wire in FlowAction::KNOWN_WIRE {
            assert!(fits_grammar(wire), "`{wire}` is not <scope>.<event>[.<detail>]");
        }
    }

    #[test]
    fn grammar_check_rejects_the_malformed_shapes() {
        for bad in ["dispatch start", "note", "a.b.c.d", "Dispatch.start", "phase-review.x", "dispatch.", ".x"] {
            assert!(!fits_grammar(bad), "`{bad}` should not fit the grammar");
        }
    }

    #[test]
    fn every_wire_string_is_unique() {
        let mut seen = HashSet::new();
        for wire in FlowAction::KNOWN_WIRE {
            assert!(seen.insert(*wire), "duplicate wire string `{wire}`");
        }
    }

    #[test]
    fn every_variant_round_trips_and_its_scope_is_its_first_segment() {
        for wire in FlowAction::KNOWN_WIRE {
            let action = FlowAction::from_wire(wire);
            assert_eq!(action.as_str(), *wire);
            let json = serde_json::to_string(&action).unwrap();
            assert_eq!(serde_json::from_str::<FlowAction>(&json).unwrap(), action);
            let scope = action.scope().expect("a known wire string parses to a known variant");
            assert_eq!(wire.split('.').next(), Some(scope.as_str()), "`{wire}`'s scope");
        }
    }

    #[test]
    fn every_scope_is_used_by_some_action() {
        let used: HashSet<&str> = FlowAction::KNOWN_WIRE.iter().filter_map(|w| w.split('.').next()).collect();
        for scope in FlowScope::KNOWN_WIRE {
            assert!(used.contains(scope), "scope `{scope}` has no action");
        }
    }

    #[test]
    fn an_unknown_string_reads_as_other_and_writes_back_verbatim() {
        let a = crate::legacy::read_action("future.thing");
        assert!(matches!(a, FlowAction::Other(_)));
        assert_eq!(a.scope(), None);
        assert_eq!(serde_json::to_string(&a).unwrap(), "\"future.thing\"");
    }

    /// The public deserializer is strict: an unknown or retired action is
    /// refused, a current or old spelling of a current action reads.
    #[test]
    fn serde_refuses_unknown_and_retired_actions() {
        assert!(serde_json::from_str::<FlowAction>("\"future.thing\"").is_err());
        assert!(serde_json::from_str::<FlowAction>("\"telemetry.process\"").is_err());
        assert_eq!(serde_json::from_str::<FlowAction>("\"dispatch start\"").unwrap(), FlowAction::DispatchStart);
    }

    #[test]
    fn parse_known_refuses_an_unknown_action() {
        assert_eq!(FlowAction::parse_known("dispatch.start"), Ok(FlowAction::DispatchStart));
        assert!(FlowAction::parse_known("dispatch start").is_err());
    }
}
