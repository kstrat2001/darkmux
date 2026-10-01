//! How a lab run's dispatch ended, typed (F2, 5.0 dogfood).
//!
//! A lab run used to carry only `ok: bool` (runtime exit 0). A deliberate
//! escalation exits non-zero, so it was labeled a runtime/transport error by
//! the lab summary, the verify line and `run list`, while `run stats` and the
//! trajectory said it escalated. This is the one reading of the dispatch's
//! own envelope that every lab surface shares.

use darkmux_trajectory::TerminalResult;

/// The run-manifest key carrying the runtime's `escalation_*` reason.
pub const MANIFEST_ESCALATION_KEY: &str = "escalation";

/// The three ways a dispatch the lab ran can end.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum DispatchEnd {
    /// Runtime exit 0.
    Completed,
    /// The runtime stopped on purpose and handed the work to a higher tier.
    /// `reason` is the runtime's own `escalation_*` result.
    Escalated { reason: String },
    /// A non-zero exit that was not an escalation: a runtime or transport
    /// error.
    #[default]
    Failed,
}

impl DispatchEnd {
    /// Classify a finished dispatch. An escalation is read from the
    /// envelope's own `result` whatever the exit code, because the runtime
    /// exits non-zero after one.
    pub fn from_dispatch(exit_code: i32, stdout: &str) -> Self {
        let result = crate::lab::scores::envelope_result(stdout);
        match result {
            Some(r) if TerminalResult::parse(&r) == TerminalResult::Escalated => Self::Escalated { reason: r },
            _ if exit_code == 0 => Self::Completed,
            _ => Self::Failed,
        }
    }

    /// Did the runtime exit cleanly. An escalation did not finish its work,
    /// so it is not `ok`; it is also not an error.
    pub fn ok(&self) -> bool {
        matches!(self, Self::Completed)
    }

    /// Stamp this ending onto a run manifest: an `escalation` key naming the
    /// runtime's reason, written ONLY when the dispatch escalated, so a
    /// manifest without the key never escalated. The one writer of the key;
    /// `serve`'s lab summary is its one reader.
    pub(crate) fn record_in(&self, manifest: &mut serde_json::Value) {
        if let (Some(reason), Some(obj)) = (self.escalation(), manifest.as_object_mut()) {
            obj.insert(MANIFEST_ESCALATION_KEY.to_string(), serde_json::Value::String(reason.to_string()));
        }
    }

    /// The `run inspect` note for a manifest that recorded an escalation, read
    /// back through the key [`Self::record_in`] writes. `None` for a run that
    /// never escalated.
    pub(crate) fn inspect_note(manifest: &serde_json::Value) -> Option<String> {
        let reason = manifest.get(MANIFEST_ESCALATION_KEY)?.as_str()?;
        Some(format!("outcome=escalated ({reason})"))
    }

    /// The escalation reason, when the dispatch escalated.
    pub fn escalation(&self) -> Option<&str> {
        match self {
            Self::Escalated { reason } => Some(reason),
            _ => None,
        }
    }
}

/// The raw facts a finished lab dispatch hands its provider. A provider
/// derives how the dispatch ended from these (never from a value passed
/// beside them), so what it records can only be what the dispatch reported.
pub(crate) struct Dispatched {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Dispatched {
    /// How this dispatch ended.
    pub(crate) fn end(&self) -> DispatchEnd {
        DispatchEnd::from_dispatch(self.exit_code, &self.stdout)
    }

    /// The `RunResult.error` for this dispatch: set only for a real failure.
    /// An escalation is not an error (F2).
    pub(crate) fn error(&self) -> Option<String> {
        (self.end() == DispatchEnd::Failed).then(|| format!("runtime exit: {}", self.stderr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESCALATED: &str = r#"{"result":"escalation_compaction_reread_loop","final_assistant":"x"}"#;

    #[test]
    fn an_escalation_is_typed_from_the_envelope_even_though_the_exit_is_non_zero() {
        let end = DispatchEnd::from_dispatch(1, ESCALATED);
        assert_eq!(end, DispatchEnd::Escalated { reason: "escalation_compaction_reread_loop".into() });
        assert!(!end.ok());
        assert_eq!(end.escalation(), Some("escalation_compaction_reread_loop"));
    }

    #[test]
    fn the_manifest_carries_the_escalation_only_when_there_was_one() {
        let mut m = serde_json::json!({"ok": false});
        DispatchEnd::from_dispatch(1, ESCALATED).record_in(&mut m);
        assert_eq!(m[MANIFEST_ESCALATION_KEY], "escalation_compaction_reread_loop");
        let mut plain = serde_json::json!({"ok": false});
        DispatchEnd::Failed.record_in(&mut plain);
        assert!(plain.get(MANIFEST_ESCALATION_KEY).is_none());
    }

    #[test]
    fn inspect_names_a_recorded_escalation_and_is_silent_otherwise() {
        let mut m = serde_json::json!({"ok": false});
        assert_eq!(DispatchEnd::inspect_note(&m), None);
        DispatchEnd::from_dispatch(1, ESCALATED).record_in(&mut m);
        assert_eq!(
            DispatchEnd::inspect_note(&m).as_deref(),
            Some("outcome=escalated (escalation_compaction_reread_loop)")
        );
    }

    #[test]
    fn a_non_escalation_keeps_its_exit_code_reading() {
        assert_eq!(DispatchEnd::from_dispatch(0, r#"{"result":"stop"}"#), DispatchEnd::Completed);
        assert_eq!(DispatchEnd::from_dispatch(1, r#"{"result":"error"}"#), DispatchEnd::Failed);
        assert_eq!(DispatchEnd::from_dispatch(137, "no envelope at all"), DispatchEnd::Failed);
        assert_eq!(DispatchEnd::from_dispatch(0, ""), DispatchEnd::Completed);
    }
}
