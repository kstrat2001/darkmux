//! Host-driven pause file (#2114).
//!
//! Between turns the loop checks `<workspace>/.darkmux/pace.json`. While it
//! holds `pause: true` the loop rests in bounded increments rather than
//! exiting, re-reading the file each increment, so a host-side pause (the
//! thermal governor, #2110) never looks like a stall to the runtime's own
//! inactivity clock — each increment counts as proof-of-work the same way
//! #2094's `turn_delay_ms` rest does.
//!
//! Absent file = the overwhelmingly common case (no pause active) and is
//! NOT an error. A malformed file is ignored — logged once so a broken
//! writer is visible without spamming stderr once per 2s poll.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Shape of `.darkmux/pace.json`. All fields optional/defaulted so a
/// partial or forward-compat file still parses.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PaceFile {
    #[serde(default)]
    pub pause: bool,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
}

impl PaceFile {
    /// The reason string to stamp on a `runtime.rest` event — the
    /// operator-supplied reason, or a generic fallback when the pace file
    /// set `pause: true` without one.
    pub fn reason_or_default(&self) -> String {
        self.reason.clone().unwrap_or_else(|| "paused".to_string())
    }
}

/// `<workspace>/.darkmux/pace.json`.
pub fn pace_file_path(workspace: &Path) -> PathBuf {
    workspace.join(".darkmux").join("pace.json")
}

/// Tracks whether we've already warned about a malformed pace file, so the
/// warning fires once per malformed-window rather than once per 2s poll.
#[derive(Default)]
pub struct PaceReader {
    warned_malformed: bool,
}

impl PaceReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read + parse the pace file. `None` covers both "file absent" (the
    /// common no-pause case) and "malformed" (ignored, logged once) —
    /// callers can't and don't need to distinguish the two: either way
    /// there's no pause instruction to act on.
    pub fn read(&mut self, workspace: &Path) -> Option<PaceFile> {
        let path = pace_file_path(workspace);
        let contents = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str::<PaceFile>(&contents) {
            Ok(pace) => {
                // A subsequent valid write clears the warn state, so a
                // fixed file after a bad edit gets a fresh chance to warn
                // if it breaks again.
                self.warned_malformed = false;
                Some(pace)
            }
            Err(e) => {
                if !self.warned_malformed {
                    eprintln!(
                        "darkmux-runtime: ⚠ malformed pace file at {}: {e} (ignoring, treating as no pause)",
                        path.display()
                    );
                    self.warned_malformed = true;
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_file_reads_as_none() {
        let ws = tempfile::tempdir().unwrap();
        let mut reader = PaceReader::new();
        assert_eq!(reader.read(ws.path()), None);
    }

    #[test]
    fn valid_pause_file_parses() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join(".darkmux")).unwrap();
        std::fs::write(
            pace_file_path(ws.path()),
            r#"{"pause": true, "reason": "thermal", "state": "hot"}"#,
        )
        .unwrap();
        let mut reader = PaceReader::new();
        let pace = reader.read(ws.path()).unwrap();
        assert!(pace.pause);
        assert_eq!(pace.reason.as_deref(), Some("thermal"));
        assert_eq!(pace.state.as_deref(), Some("hot"));
    }

    #[test]
    fn malformed_file_is_ignored_not_fatal() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join(".darkmux")).unwrap();
        std::fs::write(pace_file_path(ws.path()), "{not json").unwrap();
        let mut reader = PaceReader::new();
        assert_eq!(reader.read(ws.path()), None);
    }

    #[test]
    fn reason_or_default_falls_back() {
        let pace = PaceFile { pause: true, reason: None, state: None };
        assert_eq!(pace.reason_or_default(), "paused");
    }
}
