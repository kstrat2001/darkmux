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
mod hook;

pub use audit::AuditWriteFailedPayload;
pub use hook::{HookDeliveryPayload, HookDryRunPayload, HookFailedPayload, HookNoticePayload};

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

macro_rules! flow_payloads {
    ( $( $variant:ident => $ty:ty; )* ) => {
        /// A flow record's payload: one variant per payload-bearing action.
        /// See the module doc.
        #[derive(Debug, Clone, PartialEq, Serialize)]
        #[serde(untagged)]
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
    BatteryPauseUnsupported => OpenPayload;
    BudgetWarn => OpenPayload;
    BudgetWait => OpenPayload;
    BudgetResume => OpenPayload;
    BudgetStop => OpenPayload;
    DispatchStart => OpenPayload;
    DispatchComplete => OpenPayload;
    DispatchError => OpenPayload;
    DispatchTurn => OpenPayload;
    DispatchTurnHeartbeat => OpenPayload;
    DispatchTool => OpenPayload;
    DispatchCompaction => OpenPayload;
    DispatchCheckpoint => OpenPayload;
    DispatchReasoning => OpenPayload;
    DispatchFeedbackInjected => OpenPayload;
    DispatchRest => OpenPayload;
    DispatchDegeneracyWarning => OpenPayload;
    DispatchWorkdirGitUnavailable => OpenPayload;
    DispatchRoute => OpenPayload;
    GhVerbExecuted => OpenPayload;
    HookFired => HookDeliveryPayload;
    HookFailed => HookFailedPayload;
    HookDryRun => HookDryRunPayload;
    MachineTelemetry => OpenPayload;
    MachineThermal => OpenPayload;
    MachineBattery => OpenPayload;
    MachineBatteryHealth => OpenPayload;
    MachineRollup => OpenPayload;
    MissionStart => OpenPayload;
    MissionClose => OpenPayload;
    MissionAbort => OpenPayload;
    MissionGrow => OpenPayload;
    MissionDebriefPrompt => OpenPayload;
    MissionRunFinalize => OpenPayload;
    MissionRunAbort => OpenPayload;
    OperatorNote => OpenPayload;
    OperatorCatch => OpenPayload;
    PhaseReviewBegin => OpenPayload;
    PhaseReviewAborted => OpenPayload;
    PhaseReviewDispatch => OpenPayload;
    PhaseReviewFailed => OpenPayload;
    PhaseReviewVerdict => OpenPayload;
    RadioRoute => OpenPayload;
    RunStart => OpenPayload;
    RunComplete => OpenPayload;
    RunError => OpenPayload;
    StepStart => OpenPayload;
    StepComplete => OpenPayload;
    StepError => OpenPayload;
    StepResult => OpenPayload;
    StepTiming => OpenPayload;
    StepSeatUnresolved => OpenPayload;
    StreamError => OpenPayload;
    TelemetryTokens => OpenPayload;
    TelemetryDetector => OpenPayload;
    TelemetryContext => OpenPayload;
    TelemetryCompaction => OpenPayload;
    TelemetryRuntime => OpenPayload;
    TelemetryLms => OpenPayload;
    TierDecision => OpenPayload;
    ThermalStopUnresolved => OpenPayload;
    ThermalTier5Eject => OpenPayload;
    ThermalTier5EjectFailed => OpenPayload;
    UtilityStart => OpenPayload;
    UtilityError => OpenPayload;
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
