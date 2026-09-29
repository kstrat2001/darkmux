//! What the runtime prints on stdout under `--json`: one line, the
//! [`RuntimeEnvelope`].
//!
//! The runtime writes it and the host reads it, so both name the one type. It
//! carries no counts: turns, tokens and rests are in the trajectory it points
//! at, written as they happened, and the host folds that one log and adds the
//! `metrics` block its own callers read (`darkmux-crew`'s `DispatchEnvelope`).

use serde::{Deserialize, Serialize};

/// A bash invocation that **failed to run** (never executed), as opposed to
/// one that ran and exited non-zero. Stamped onto the envelope so a SIGNOFF
/// claiming a verifier passed can be mechanically contradicted: the gate
/// cross-checks the claim against this list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct FailedExec {
    /// The command the model asked to run (from the bash tool args).
    #[serde(default)]
    pub command: String,
    /// Why it is classified as failed-to-run (e.g. "command not found (exit 127)").
    #[serde(default)]
    pub reason: String,
}

/// Where a resumed execution picked up: the checkpoint it reloaded and that
/// checkpoint's OWN turn index (where the execution resumed FROM, not where it
/// ended).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ResumedFrom {
    pub path: String,
    pub turn_index: u64,
}

/// The runtime's `--json` envelope. The error envelope is the same shape with
/// `final_assistant: null` and no `failed_tool_invocations`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct RuntimeEnvelope {
    /// How the loop ended (`stop`, `error`, an escalation reason, ...). Absent
    /// when the runtime that wrote the envelope did not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// The last assistant message; `null` on the error envelope.
    #[serde(default)]
    pub final_assistant: Option<String>,
    /// The trajectory file, under the runtime's out-dir (SEPARATE from
    /// `/workspace`); the host rewrites it to a path the caller can open.
    /// Absent when the runtime that wrote the envelope did not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trajectory_path: Option<String>,
    /// Bash invocations that failed to run this execution. Empty on an honest
    /// run; absent on the error envelope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_tool_invocations: Option<Vec<FailedExec>>,
    /// Present only when this execution reloaded a checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<ResumedFrom>,
}

impl RuntimeEnvelope {
    /// The envelope of a loop that produced no reply.
    pub fn error(trajectory_path: String) -> Self {
        RuntimeEnvelope {
            result: Some("error".to_string()),
            final_assistant: None,
            trajectory_path: Some(trajectory_path),
            failed_tool_invocations: None,
            resumed_from: None,
        }
    }
}
