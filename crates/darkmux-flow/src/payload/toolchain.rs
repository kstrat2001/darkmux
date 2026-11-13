//! (#3561) The runtimes darkmux fetches and verifies itself for a sandbox
//! on the host (a pinned Node.js): `toolchain.fetch`, `toolchain.ready` and
//! `toolchain.error`. They are records of this machine's state, written once
//! per install (the first sandbox that needs it), never per use.

use super::Attribution;
use serde::{Deserialize, Serialize};

/// What needed the toolchain: the sandbox that was about to start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../ui/src/types/generated/")
)]
pub enum ToolchainUse {
    /// A client preview.
    Preview,
    /// A dispatch's sandbox.
    Dispatch,
    /// A contained command (a lab run's tests, a mods gate).
    Command,
    /// A value this build does not name, read from an archive written by another version.
    /// Never written.
    #[serde(other)]
    Unknown,
}

/// One fetch of a managed toolchain: when it began (`toolchain.fetch`), when
/// its archive matched the pinned SHA-256 and was installed
/// (`toolchain.ready`), or why nothing was installed (`toolchain.error`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../ui/src/types/generated/")
)]
pub struct ToolchainPayload {
    /// The runtime (`node`).
    pub tool: String,
    /// Its version (`26.13.1`).
    pub version: String,
    /// The build's platform (`darwin-arm64`).
    pub platform: String,
    /// Where the archive is fetched from.
    pub url: String,
    /// What needed it.
    pub needed_by: ToolchainUse,
    /// On `toolchain.ready` and `toolchain.error`: how long the fetch took.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub elapsed_ms: Option<u64>,
    /// On `toolchain.error`: why nothing was installed, and the fix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub error: Option<String>,
}

impl Attribution for ToolchainPayload {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FlowAction, FlowScope, Payload};

    fn a_fetch() -> ToolchainPayload {
        ToolchainPayload {
            tool: "node".into(),
            version: "26.13.1".into(),
            platform: "darwin-arm64".into(),
            url: "https://nodejs.org/dist/v26.13.1/node-v26.13.1-darwin-arm64.tar.gz".into(),
            needed_by: ToolchainUse::Dispatch,
            elapsed_ms: None,
            error: None,
        }
    }

    /// (#3561) Three actions, one shape, each its own variant: a fetch's
    /// start, its verified install and its failure, under the `toolchain`
    /// scope and no execution grain (a machine's state, not a role's work).
    #[test]
    fn a_toolchain_fetch_has_one_wire_spelling_per_event() {
        for (payload, action, wire) in [
            (
                Payload::ToolchainFetch(a_fetch()),
                FlowAction::ToolchainFetch,
                "toolchain.fetch",
            ),
            (
                Payload::ToolchainReady(a_fetch()),
                FlowAction::ToolchainReady,
                "toolchain.ready",
            ),
            (
                Payload::ToolchainError(a_fetch()),
                FlowAction::ToolchainError,
                "toolchain.error",
            ),
        ] {
            assert_eq!(payload.action(), action);
            assert_eq!(action.as_str(), wire);
            assert_eq!(FlowAction::from_wire(wire), action);
            assert_eq!(action.scope(), Some(FlowScope::Toolchain));
            assert_eq!(action.grain(), None, "{wire}");
        }
    }

    /// The payload's JSON: the optional fields absent until set, the use in
    /// snake case, an unknown use read back as `unknown`.
    #[test]
    fn a_toolchain_payload_reads_back_as_written() {
        let mut p = a_fetch();
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["needed_by"], "dispatch");
        assert!(
            v.get("elapsed_ms").is_none() && v.get("error").is_none(),
            "{v}"
        );
        p.elapsed_ms = Some(1200);
        p.error = Some("hash mismatch".into());
        let back: ToolchainPayload =
            serde_json::from_value(serde_json::to_value(&p).unwrap()).unwrap();
        assert_eq!(back, p);
        let mut odd = serde_json::to_value(&p).unwrap();
        odd["needed_by"] = "kiln".into();
        assert_eq!(
            serde_json::from_value::<ToolchainPayload>(odd)
                .unwrap()
                .needed_by,
            ToolchainUse::Unknown
        );
    }
}
