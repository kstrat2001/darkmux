//! The payload of `audit.write_failed`.

use super::Attribution;
use serde::{Deserialize, Serialize};

/// The breadcrumb the tee sink writes to its casual sink when the audit sink refused a record:
/// which record was dropped and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct AuditWriteFailedPayload {
    /// The dropped record's action, as spelled on the wire.
    pub dropped_action: String,
    /// The dropped record's session, `null` when it had none.
    pub dropped_session_id: Option<String>,
    /// The audit sink's error.
    pub error: String,
}

impl Attribution for AuditWriteFailedPayload {}
