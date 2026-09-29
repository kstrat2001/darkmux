//! The typed payload of a flow record: one named type per action.
//!
//! [`Payload`] has one variant per payload-bearing [`FlowAction`], declared
//! in the one [`flow_payloads!`] list below. A variant fixes its action, so a
//! record built from a payload ([`crate::FlowRecord::for_session_with`],
//! [`crate::FlowRecord::for_execution_with`]) takes its action from it and a
//! producer cannot pair an action with another action's payload. An action
//! with no row here never carries a payload: the write check
//! ([`crate::CheckedRecord`]) refuses one.
//!
//! Two actions may share a payload TYPE where they share a shape; each still
//! has its own variant, so the type never decides the action.
//!
//! # Reading
//!
//! An archived record's payload is parsed into its action's type after the
//! legacy upgrade ([`crate::reader`]); a field the type does not name is
//! ignored. A payload that does not parse as its action's type (an old shape,
//! a hand edit) is kept as [`UnreadPayload`]: its JSON is preserved and
//! re-serializes byte-for-byte, and a typed reader treats it as absent. It is
//! never written.
//!
//! # Open payloads
//!
//! [`OpenPayload`] is the explicit type for a payload whose keys are chosen by
//! someone other than darkmux (a mission config's outcome document, a
//! model's own `create_finding` arguments). Every action that carries one is
//! named in the [`flow_payloads!`] list, so "which payloads are free-form" is
//! one grep.

mod audit;
mod dispatch;
mod hook;
mod lifecycle;
mod machine;
mod telemetry;
mod thermal;
mod usage;

pub use audit::AuditWriteFailedPayload;
pub use hook::{HookDeliveryPayload, HookDryRunPayload, HookFailedPayload, HookNoticePayload};
pub use dispatch::{
    BoundRef, BriefRef, BriefRefKind, CheckpointVerdict, DispatchCheckpointPayload, DispatchCompactionPayload,
    DispatchDegeneracyWarningPayload, DispatchEndPayload, DispatchFeedbackPayload, DispatchHeartbeatPayload,
    DispatchReasoningPayload, DispatchRestPayload, DispatchRoutePayload, DispatchStartPayload, DispatchToolPayload,
    DispatchTurnPayload, DispatchWorkdirGitUnavailablePayload, GitCheckout, GitdirKind, HostWindow, Knob, KnobSource,
    LiveSummary, ResultClass, RouteDecision, RuntimeBounds, StreamPhase, ToolOutcome, TurnUsage,
};
pub use lifecycle::{
    BreachLevel, BudgetMetric, BudgetPayload, BudgetPolicyKind, BudgetScope, FailedVerifier, GhVerbExecutedPayload, GrowReason,
    MissionGrowPayload, MissionRunTerminalPayload, PhaseReviewVerdictPayload, RadioDecision, RadioRoutePayload, RadioSurface,
    ReviewVerdict, RunPayload, SeatClass, StepResultPayload, StepSeatUnresolvedPayload, StepStartPayload, StepTimingPayload,
};
pub use machine::{
    BatteryCharge, BatteryHealthNow, BatteryTransition, CpuClusterNow, HostSampleNow, LoadWindow, MachineBatteryHealthPayload,
    MachineBatteryPayload, MachineLoad, MachineRollupPayload, MachineTelemetryPayload, MachineThermalPayload, MetricWindow,
    PowerNow, PowerWindowWire, ThermalNow, ThermalWindowWire,
};
pub use telemetry::{
    DetectorArea, DetectorKind, DetectorSeverity, LmsEvent, LmsRole, TelemetryCompactionPayload, TelemetryContextPayload,
    TelemetryDetectorPayload, TelemetryLmsPayload, TelemetryRuntimePayload,
};
pub use thermal::{
    BatteryPauseUnsupportedPayload, EjectFailure, EjectedModel, ThermalStopUnresolvedPayload, ThermalTier5EjectFailedPayload,
    ThermalTier5EjectPayload,
};
pub use usage::{CallKind, TokenSource, UsagePayload, UsagePurpose, UtilityErrorPayload, UtilityJobKind, UtilityStartPayload};

use crate::FlowAction;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

/// A payload whose keys are not darkmux's to define: an operator- or
/// model-authored JSON object, carried through verbatim.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(transparent)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/", type = "Record<string, unknown>"))]
pub struct OpenPayload(pub serde_json::Map<String, Value>);

impl From<serde_json::Map<String, Value>> for OpenPayload {
    fn from(map: serde_json::Map<String, Value>) -> Self {
        Self(map)
    }
}

/// A payload read from a record whose JSON is not its action's typed shape.
/// Built only by [`Payload::settle`]; it re-serializes as the JSON it was
/// read from.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/", type = "Record<string, unknown>"))]
pub struct UnreadPayload {
    #[cfg_attr(feature = "ts-export", ts(skip))]
    action: FlowAction,
    #[cfg_attr(feature = "ts-export", ts(skip))]
    raw: Value,
}

impl UnreadPayload {
    /// The JSON as it was read.
    pub fn raw(&self) -> &Value {
        &self.raw
    }

    pub(crate) fn into_raw(self) -> Value {
        self.raw
    }
}

/// A payload deserialized by serde alone knows no action, so it is unread
/// until the record around it settles it ([`crate::FlowRecord::settled`]).
impl<'de> Deserialize<'de> for Payload {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = Value::deserialize(d)?;
        Ok(Payload::Unread(UnreadPayload { action: FlowAction::unsettled(), raw }))
    }
}

impl Serialize for UnreadPayload {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.raw.serialize(s)
    }
}

/// The attribution a crew-built record lays over its payload, beyond the
/// fields the payload's own type names: the graph step a step session names
/// (`step_id`), the caller's provenance (`context`, a JSON object), and, on a
/// record whose existence or content is a host reading, the scripted source
/// behind it (`simulated_host_source`). A type carries the field or leaves the
/// slot `None`, so a payload without it is never attributed.
pub trait Attribution {
    /// The payload's `step_id`, when its type has one.
    fn step_slot(&mut self) -> Option<&mut Option<String>> {
        None
    }
    /// The payload's `step_id`, read, when its type has one and it is set.
    fn step(&self) -> Option<&str> {
        None
    }
    /// The payload's `context`, when its type has one.
    fn context_slot(&mut self) -> Option<&mut Option<serde_json::Map<String, Value>>> {
        None
    }
    /// The payload's `simulated_host_source`, when its type has one.
    fn host_source_slot(&mut self) -> Option<&mut Option<String>> {
        None
    }
}

impl Attribution for OpenPayload {}

macro_rules! flow_payloads {
    ( $( $variant:ident => $ty:ty; )* ) => {
        /// A flow record's payload: one variant per payload-bearing action.
        /// See the module doc.
        #[derive(Debug, Clone, PartialEq, Serialize)]
        #[serde(untagged)]
        // A record is built, written and dropped; boxing the big variants would
        // add an allocation per record and a `Box::new` at every producer to
        // shrink a value that is never stored in bulk.
        #[allow(clippy::large_enum_variant)]
        #[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
        #[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
        pub enum Payload {
            $( $variant($ty), )*
            /// A payload that did not parse as its action's type.
            Unread(UnreadPayload),
        }

        impl Payload {
            /// The action this payload belongs to.
            pub fn action(&self) -> FlowAction {
                match self {
                    $( Payload::$variant(_) => FlowAction::$variant, )*
                    Payload::Unread(u) => u.action.clone(),
                }
            }

            /// Name the graph step this payload belongs to, unless it already
            /// names one. A payload whose type has no `step_id` is unchanged.
            pub fn attribute_step(&mut self, step: &str) {
                match self {
                    $( Payload::$variant(p) => {
                        if let Some(slot) = Attribution::step_slot(p) {
                            slot.get_or_insert_with(|| step.to_string());
                        }
                    } )*
                    Payload::Unread(_) => {}
                }
            }

            /// Set the caller's provenance object, when the payload's type
            /// carries one.
            pub fn attribute_context(&mut self, context: &serde_json::Map<String, Value>) {
                match self {
                    $( Payload::$variant(p) => {
                        if let Some(slot) = Attribution::context_slot(p) {
                            *slot = Some(context.clone());
                        }
                    } )*
                    Payload::Unread(_) => {}
                }
            }

            /// The step this payload belongs to, when its type names one.
            pub fn step_id(&self) -> Option<&str> {
                match self {
                    $( Payload::$variant(p) => Attribution::step(p), )*
                    Payload::Unread(_) => None,
                }
            }

            /// Name the scripted host source behind this payload, when its
            /// type carries the field.
            pub fn attribute_host_source(&mut self, path: &str) {
                match self {
                    $( Payload::$variant(p) => {
                        if let Some(slot) = Attribution::host_source_slot(p) {
                            *slot = Some(path.to_string());
                        }
                    } )*
                    Payload::Unread(_) => {}
                }
            }

            /// Whether `action` carries a payload at all.
            pub fn carried_by(action: &FlowAction) -> bool {
                match action {
                    $( FlowAction::$variant => true, )*
                    _ => false,
                }
            }

            /// `raw` read as `action`'s payload type: typed when it parses,
            /// [`Payload::Unread`] when it does not.
            pub fn settle(action: &FlowAction, raw: Value) -> Payload {
                match action {
                    $(
                        FlowAction::$variant => match <$ty as Deserialize>::deserialize(&raw) {
                            Ok(typed) => Payload::$variant(typed),
                            Err(_) => Payload::Unread(UnreadPayload { action: action.clone(), raw }),
                        },
                    )*
                    _ => Payload::Unread(UnreadPayload { action: action.clone(), raw }),
                }
            }
        }

        /// Every payload-bearing action's wire spelling with the name of its
        /// payload type, in declaration order: the source of the generated
        /// TypeScript `FlowPayloads` map.
        #[cfg(all(test, feature = "ts-export"))]
        pub(crate) fn payload_types() -> Vec<(String, &'static str)> {
            vec![ $( (FlowAction::$variant.as_str().to_string(), stringify!($ty)), )* ]
        }
    };
}

flow_payloads! {
    AuditWriteFailed => AuditWriteFailedPayload;
    BatteryPauseUnsupported => BatteryPauseUnsupportedPayload;
    BudgetWarn => BudgetPayload;
    BudgetWait => BudgetPayload;
    BudgetResume => BudgetPayload;
    BudgetStop => BudgetPayload;
    DispatchStart => DispatchStartPayload;
    DispatchComplete => DispatchEndPayload;
    DispatchError => DispatchEndPayload;
    DispatchTurn => DispatchTurnPayload;
    DispatchTurnHeartbeat => DispatchHeartbeatPayload;
    DispatchTool => DispatchToolPayload;
    DispatchCompaction => DispatchCompactionPayload;
    DispatchCheckpoint => DispatchCheckpointPayload;
    DispatchReasoning => DispatchReasoningPayload;
    DispatchFeedbackInjected => DispatchFeedbackPayload;
    DispatchRest => DispatchRestPayload;
    DispatchDegeneracyWarning => DispatchDegeneracyWarningPayload;
    DispatchWorkdirGitUnavailable => DispatchWorkdirGitUnavailablePayload;
    DispatchRoute => DispatchRoutePayload;
    GhVerbExecuted => GhVerbExecutedPayload;
    HookFired => HookDeliveryPayload;
    HookFailed => HookFailedPayload;
    HookDryRun => HookDryRunPayload;
    MachineTelemetry => MachineTelemetryPayload;
    MachineThermal => MachineThermalPayload;
    MachineBattery => MachineBatteryPayload;
    MachineBatteryHealth => MachineBatteryHealthPayload;
    MachineRollup => MachineRollupPayload;
    MissionStart => OpenPayload;
    MissionClose => OpenPayload;
    MissionAbort => OpenPayload;
    MissionGrow => MissionGrowPayload;
    MissionRunFinalize => MissionRunTerminalPayload;
    MissionRunAbort => MissionRunTerminalPayload;
    PhaseReviewVerdict => PhaseReviewVerdictPayload;
    RadioRoute => RadioRoutePayload;
    RunStart => RunPayload;
    RunComplete => RunPayload;
    RunError => RunPayload;
    StepStart => StepStartPayload;
    StepResult => StepResultPayload;
    StepTiming => StepTimingPayload;
    StepSeatUnresolved => StepSeatUnresolvedPayload;
    TelemetryTokens => UsagePayload;
    TelemetryDetector => TelemetryDetectorPayload;
    TelemetryContext => TelemetryContextPayload;
    TelemetryCompaction => TelemetryCompactionPayload;
    TelemetryRuntime => TelemetryRuntimePayload;
    TelemetryLms => TelemetryLmsPayload;
    ThermalStopUnresolved => ThermalStopUnresolvedPayload;
    ThermalTier5Eject => ThermalTier5EjectPayload;
    ThermalTier5EjectFailed => ThermalTier5EjectFailedPayload;
    UtilityStart => UtilityStartPayload;
    UtilityError => UtilityErrorPayload;
}

/// The generated TypeScript `FlowPayloads`: each payload-bearing action's wire
/// spelling to its payload type, so a viewer reads a record's payload through
/// the type of its own action.
#[cfg(all(test, feature = "ts-export"))]
mod ts_export {
    use super::payload_types;

    fn render() -> String {
        let types = payload_types();
        let mut names: Vec<&str> = types.iter().map(|(_, ty)| *ty).collect();
        names.sort_unstable();
        names.dedup();
        let mut out = String::from("// This file was generated by [darkmux-flow](crates/darkmux-flow/src/payload/mod.rs). Do not edit this file manually.\n");
        for name in &names {
            out.push_str(&format!("import type {{ {name} }} from \"./{name}\";\n"));
        }
        out.push_str("\n/**\n * The payload type of each payload-bearing flow action, keyed by the action's\n * wire spelling. An action absent here never carries a payload.\n */\nexport type FlowPayloads = {\n");
        for (wire, ty) in &types {
            out.push_str(&format!("  \"{wire}\": {ty},\n"));
        }
        out.push_str("};\n");
        out
    }

    #[test]
    fn export_bindings_flowpayloads() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/src/types/generated/FlowPayloads.ts");
        std::fs::write(&path, render()).unwrap_or_else(|e| panic!("could not write {}: {e}", path.display()));
    }
}
