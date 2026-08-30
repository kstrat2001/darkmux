//! Turn-boundary checkpoint (#2114).
//!
//! After each completed turn boundary (a full turn, or a #1221 reasoning-
//! checkpoint hand-back — see `loop_runner`'s `resuming_after_checkpoint`)
//! the loop writes `<workspace>/.darkmux/checkpoint.json` atomically (temp
//! file + rename onto the same filesystem) so a killed container always
//! leaves a checkpoint from its last completed boundary, never a torn
//! write. `--resume <path>` (or `DARKMUX_RESUME_CHECKPOINT`) reloads one of
//! these instead of starting from the system prompt.

use crate::lmstudio::Message;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Schema version for `RunCheckpoint`. Bump on a breaking field change so
/// an older runtime resuming a newer checkpoint (or vice versa) fails
/// loud at load time instead of silently misreading fields.
pub const CHECKPOINT_SCHEMA_VERSION: u32 = 1;

/// (#1221) The reasoning-checkpoint hand-back pending on the turn in
/// flight, if the checkpoint was written mid-continuation rather than at
/// a clean turn boundary — captured so a resumed dispatch reconstructs
/// the SAME prefill continuation instead of restarting the turn (and
/// re-running any tool calls it already made) from scratch. Mirrors
/// `loop_runner::TurnAccum`'s fields; kept as a separate public type
/// since `TurnAccum` itself is private to `loop_runner` and carries
/// loop-internal invariants that don't belong in a serialized artifact.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PendingHandBack {
    pub thought: String,
    pub answer: String,
    pub think_closed: bool,
    pub is_reasoning: bool,
    /// The accumulated thought already begins with the model's own
    /// `<think>` opener — see `TurnAccum::carries_own_opener`'s own doc.
    /// Must round-trip or a resumed prefill can double-wrap the opener.
    #[serde(default)]
    pub carries_own_opener: bool,
}

/// Everything a resumed dispatch needs to continue from this checkpoint's
/// turn boundary instead of starting over.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunCheckpoint {
    pub schema_version: u32,
    /// Full conversation so far, in order — the tool-call cursor IS this
    /// vector's length/contents; there's no separate index to drift from
    /// it.
    pub messages: Vec<Message>,
    /// Completed turns (model calls that advanced the loop's `turns`
    /// counter; a #1221 hand-back continuation does NOT advance it).
    pub turns: u32,
    pub total_prompt_tokens: u32,
    pub total_completion_tokens: u32,
    /// darkmux compaction rewrites `messages` itself (the middle is
    /// replaced with a summary message), so the compacted history IS
    /// `messages` above — this counter is the only side state
    /// compaction carries across a resume.
    pub compactions: u32,
    pub rest_ms: u64,
    pub rests: u32,
    /// Pending #1221 hand-back, if the loop was mid-checkpoint-
    /// continuation when this was written. `None` on a clean turn
    /// boundary (the common case).
    pub pending_hand_back: Option<PendingHandBack>,
    pub written_at_unix_ms: u64,
}

/// `<workspace>/.darkmux/checkpoint.json`.
pub fn checkpoint_file_path(workspace: &Path) -> PathBuf {
    workspace.join(".darkmux").join("checkpoint.json")
}

/// Write atomically: serialize to a sibling temp file (named with this
/// process's pid so two runtimes racing on the same mount can't collide),
/// then `rename` over the real path. Rename is atomic on POSIX when both
/// paths share a filesystem, which they do here (both under the same
/// mounted `.darkmux` dir) — a reader never observes a partially-written
/// checkpoint, and a container killed mid-write leaves the PREVIOUS
/// complete checkpoint in place, never a torn one.
pub fn write_checkpoint(workspace: &Path, checkpoint: &RunCheckpoint) -> std::io::Result<()> {
    let dir = workspace.join(".darkmux");
    std::fs::create_dir_all(&dir)?;
    let final_path = checkpoint_file_path(workspace);
    let tmp_path = dir.join(format!("checkpoint.json.tmp.{}", std::process::id()));
    let body = serde_json::to_vec_pretty(checkpoint)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp_path, body)?;
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

/// Read + parse a checkpoint written by `write_checkpoint`. Errors
/// propagate (not swallowed): resuming is an explicit operator/host
/// action (`--resume <path>`), so a missing or corrupt checkpoint should
/// fail the dispatch loudly rather than silently starting fresh under a
/// name that looked like a resume.
pub fn read_checkpoint(path: &Path) -> anyhow::Result<RunCheckpoint> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading checkpoint {}: {e}", path.display()))?;
    let checkpoint: RunCheckpoint = serde_json::from_str(&contents)
        .map_err(|e| anyhow::anyhow!("parsing checkpoint {}: {e}", path.display()))?;
    if checkpoint.schema_version != CHECKPOINT_SCHEMA_VERSION {
        anyhow::bail!(
            "checkpoint {} has schema_version={}, this runtime supports {}",
            path.display(),
            checkpoint.schema_version,
            CHECKPOINT_SCHEMA_VERSION
        );
    }
    Ok(checkpoint)
}

pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RunCheckpoint {
        RunCheckpoint {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            messages: vec![Message::system("sys"), Message::user("hi")],
            turns: 1,
            total_prompt_tokens: 10,
            total_completion_tokens: 5,
            compactions: 0,
            rest_ms: 0,
            rests: 0,
            pending_hand_back: None,
            written_at_unix_ms: unix_ms(),
        }
    }

    #[test]
    fn round_trips_through_write_and_read() {
        let ws = tempfile::tempdir().unwrap();
        let checkpoint = sample();
        write_checkpoint(ws.path(), &checkpoint).unwrap();
        let loaded = read_checkpoint(&checkpoint_file_path(ws.path())).unwrap();
        assert_eq!(loaded.turns, 1);
        assert_eq!(loaded.messages.len(), 2);
    }

    #[test]
    fn write_is_atomic_no_tmp_file_left_behind() {
        let ws = tempfile::tempdir().unwrap();
        write_checkpoint(ws.path(), &sample()).unwrap();
        let dir = ws.path().join(".darkmux");
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries, vec!["checkpoint.json".to_string()]);
    }

    #[test]
    fn read_rejects_mismatched_schema_version() {
        let ws = tempfile::tempdir().unwrap();
        let mut checkpoint = sample();
        checkpoint.schema_version = 999;
        write_checkpoint(ws.path(), &checkpoint).unwrap();
        let err = read_checkpoint(&checkpoint_file_path(ws.path())).unwrap_err();
        assert!(err.to_string().contains("schema_version"));
    }

    #[test]
    fn pending_hand_back_round_trips() {
        let ws = tempfile::tempdir().unwrap();
        let mut checkpoint = sample();
        checkpoint.pending_hand_back = Some(PendingHandBack {
            thought: "still thinking".into(),
            answer: String::new(),
            think_closed: false,
            is_reasoning: true,
            carries_own_opener: false,
        });
        write_checkpoint(ws.path(), &checkpoint).unwrap();
        let loaded = read_checkpoint(&checkpoint_file_path(ws.path())).unwrap();
        assert_eq!(
            loaded.pending_hand_back,
            Some(PendingHandBack {
                thought: "still thinking".into(),
                answer: String::new(),
                think_closed: false,
                is_reasoning: true,
                carries_own_opener: false,
            })
        );
    }
}
