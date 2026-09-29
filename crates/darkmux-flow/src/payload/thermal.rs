//! Payloads of a run's thermal and battery governors' own records
//! (`thermal.*`, `battery.pause_unsupported`).

use super::Attribution;
use serde::{Deserialize, Serialize};

/// A resident model the tier-5 eject unloaded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct EjectedModel {
    pub identifier: String,
    /// Its context length.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub context: u64,
}

/// A resident model the tier-5 eject could not unload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct EjectFailure {
    pub identifier: String,
    pub error: String,
}

/// The thermal breaker tripped and could not write or resolve the crawl STOP file: the payload of
/// `thermal.stop_unresolved`. `state` is the host reading that tripped it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct ThermalStopUnresolvedPayload {
    pub stop_written: bool,
    /// Why the file could not be written or resolved.
    pub cause: String,
    pub reason: String,
    pub state: String,
    /// The scenario file behind a record made on scripted readings; absent on a real run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub simulated_host_source: Option<String>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied, carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, unknown>", optional))]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Attribution for ThermalStopUnresolvedPayload {
    fn step_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.step_id)
    }
    fn context_slot(&mut self) -> Option<&mut Option<serde_json::Map<String, serde_json::Value>>> {
        Some(&mut self.context)
    }
    fn host_source_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.simulated_host_source)
    }
}

/// The tier-5 eject on a literal `critical` thermal state unloaded the `darkmux:` residents: the
/// payload of `thermal.tier5_eject`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct ThermalTier5EjectPayload {
    /// Whether the runtime's next turn-boundary checkpoint landed before the eject, or the turn was
    /// cut off.
    pub reached_checkpoint_boundary: bool,
    pub ejected: Vec<EjectedModel>,
    /// Models the operator loaded, which darkmux never touches.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub user_loaded_count: u64,
    /// The scenario file behind a record made on scripted readings; absent on a real run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub simulated_host_source: Option<String>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied, carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, unknown>", optional))]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Attribution for ThermalTier5EjectPayload {
    fn step_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.step_id)
    }
    fn context_slot(&mut self) -> Option<&mut Option<serde_json::Map<String, serde_json::Value>>> {
        Some(&mut self.context)
    }
    fn host_source_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.simulated_host_source)
    }
}

/// The tier-5 eject could not unload everything: the payload of `thermal.tier5_eject_failed`.
/// Either models that refused to unload, or the failure to list the residents at all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct ThermalTier5EjectFailedPayload {
    pub reached_checkpoint_boundary: bool,
    /// Every model still resident.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub failed: Option<Vec<EjectFailure>>,
    /// How many did unload, when some did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub ejected_count: Option<u64>,
    /// The enumeration failure, when the residents could not be listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub error: Option<String>,
    /// The scenario file behind a record made on scripted readings; absent on a real run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub simulated_host_source: Option<String>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied, carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, unknown>", optional))]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Attribution for ThermalTier5EjectFailedPayload {
    fn step_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.step_id)
    }
    fn context_slot(&mut self) -> Option<&mut Option<serde_json::Map<String, serde_json::Value>>> {
        Some(&mut self.context)
    }
    fn host_source_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.simulated_host_source)
    }
}

/// The operator asked for a battery-floor pause and this run type cannot take one, so it continues
/// and says so: the payload of `battery.pause_unsupported`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct BatteryPauseUnsupportedPayload {
    pub reason: String,
    pub charge_pct: u8,
    pub floor_pct: u8,
    /// The config field that asked for the pause.
    pub policy_field: String,
    pub paused: bool,
    pub detail: String,
    /// The scenario file behind a record made on scripted readings; absent on a real run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub simulated_host_source: Option<String>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied, carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, unknown>", optional))]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Attribution for BatteryPauseUnsupportedPayload {
    fn step_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.step_id)
    }
    fn context_slot(&mut self) -> Option<&mut Option<serde_json::Map<String, serde_json::Value>>> {
        Some(&mut self.context)
    }
    fn host_source_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.simulated_host_source)
    }
}
