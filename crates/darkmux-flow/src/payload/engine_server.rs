//! (#3578) An engine's own server that darkmux started because a dispatch
//! needed it and the engine reported it off (LM Studio's local server):
//! `machine.engine_server.start` once the engine reports it running, and
//! `machine.engine_server.error` when it did not start and the dispatch was
//! refused. A machine's state, not a role's work: no execution, no session.

use super::Attribution;
use serde::{Deserialize, Serialize};

/// One start of an engine's own server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../ui/src/types/generated/")
)]
pub struct EngineServerPayload {
    /// The engine whose server it is, as `default_engine` spells it
    /// (`lmstudio`).
    pub engine: String,
    /// The port it was started on, when the address names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub port: Option<u16>,
    /// The command darkmux ran, as an operator types it
    /// (`lms server start --port 1234`).
    pub command: String,
    /// On `start`: how long the command took, until the engine reported the
    /// server running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub elapsed_ms: Option<u64>,
    /// On `error`: why the server did not start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub error: Option<String>,
}

impl Attribution for EngineServerPayload {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FlowAction, FlowScope, Payload};

    fn a_start() -> EngineServerPayload {
        EngineServerPayload {
            engine: "lmstudio".into(),
            port: Some(1234),
            command: "lms server start --port 1234".into(),
            elapsed_ms: Some(900),
            error: None,
        }
    }

    /// Two actions, one shape, each its own variant, under the `machine`
    /// scope and no execution grain.
    #[test]
    fn an_engine_server_start_has_one_wire_spelling_per_event() {
        for (payload, action, wire) in [
            (Payload::MachineEngineServerStart(a_start()), FlowAction::MachineEngineServerStart, "machine.engine_server.start"),
            (Payload::MachineEngineServerError(a_start()), FlowAction::MachineEngineServerError, "machine.engine_server.error"),
        ] {
            assert_eq!(payload.action(), action);
            assert_eq!(action.as_str(), wire);
            assert_eq!(FlowAction::from_wire(wire), action);
            assert_eq!(action.scope(), Some(FlowScope::Machine));
            assert_eq!(action.grain(), None, "{wire}");
        }
    }

    /// The JSON: optional fields absent until set.
    #[test]
    fn an_engine_server_payload_reads_back_as_written() {
        let failed = EngineServerPayload { elapsed_ms: None, error: Some("no".into()), ..a_start() };
        let v = serde_json::to_value(&failed).unwrap();
        assert_eq!(v, serde_json::json!({"engine": "lmstudio", "port": 1234, "command": "lms server start --port 1234", "error": "no"}));
        assert_eq!(serde_json::from_value::<EngineServerPayload>(v).unwrap(), failed);
    }
}
