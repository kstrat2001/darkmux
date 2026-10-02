//! Reading a mission state file (`mission.json`, a phase, a task, a step).
//!
//! These are operator state, not append-only archives, so each is read in ONE
//! shape. A file written by a newer darkmux (its `schema_version` marker is
//! newer than this binary's) is refused with one message rather than read
//! with fields it does not know (#3035).

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Which state file a document is, and so which retired spellings apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateKind {
    Mission,
    Phase,
    Task,
    Step,
}

impl StateKind {
    /// The data-shape version this binary reads and writes for the file
    /// ([`darkmux_types::data_version`]).
    pub fn schema_version(self) -> &'static str {
        use darkmux_types::data_version as v;
        match self {
            StateKind::Mission => v::MISSION_SCHEMA_VERSION,
            StateKind::Phase => v::PHASE_SCHEMA_VERSION,
            StateKind::Task => v::TASK_SCHEMA_VERSION,
            StateKind::Step => v::STEP_SCHEMA_VERSION,
        }
    }

    /// How a message names the file.
    fn label(self) -> &'static str {
        match self {
            StateKind::Mission => "mission",
            StateKind::Phase => "phase",
            StateKind::Task => "task",
            StateKind::Step => "step",
        }
    }
}

/// Parse a state file, refusing one written by a newer darkmux (#3035: its
/// marker is newer than [`StateKind::schema_version`]). A file with no marker
/// predates it and is read. `path` only names the file in the refusal.
pub fn parse_state<T: DeserializeOwned>(kind: StateKind, path: &Path, text: &str) -> Result<T> {
    let doc: Value = serde_json::from_str(text).with_context(|| format!("parsing {}", path.display()))?;
    if let Some(file_version) = darkmux_types::data_version::newer(&doc, kind.schema_version()) {
        let why = darkmux_types::data_version::newer_refusal(kind.label(), &file_version, kind.schema_version());
        bail!("{}: {why}", path.display());
    }
    serde_json::from_value(doc).with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Mission, MissionStatus};
    use serde_json::json;

    /// (#3035) A marker newer than the binary's is refused for every kind;
    /// the same or older loads.
    #[test]
    fn a_newer_marker_is_refused_for_every_state_kind() {
        let path = Path::new("/x/f.json");
        for kind in [StateKind::Mission, StateKind::Phase, StateKind::Task, StateKind::Step] {
            let doc = json!({"schema_version": "999.0"});
            let err = parse_state::<Value>(kind, path, &doc.to_string()).unwrap_err().to_string();
            assert!(err.contains(&format!("newer darkmux ({} `999.0`", kind.label())) && err.contains("Upgrade darkmux."), "{err}");
            let same = json!({"schema_version": kind.schema_version()});
            assert!(parse_state::<Value>(kind, path, &same.to_string()).is_ok(), "{kind:?}");
        }
    }

    #[test]
    fn a_current_mission_document_loads() {
        let doc = json!({"id": "m", "description": "d", "status": "active", "phase_ids": ["p1"], "created_ts": 1});
        let m: Mission = parse_state(StateKind::Mission, Path::new("/x/mission.json"), &doc.to_string()).unwrap();
        assert_eq!(m.phase_ids, ["p1"]);
        assert_eq!(m.status, MissionStatus::Active);
    }
}
