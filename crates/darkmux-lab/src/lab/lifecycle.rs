//! The lab run's lifecycle record — the one artifact that must never lie.
//!
//! # Why this exists
//!
//! Before this, a lab run had no lifecycle record at all. Its status was
//! *inferred* from which artifacts happened to be on disk, and the inference
//! was wrong in two directions at once:
//!
//! * **A live run was invisible AS a lab run.** The scan only recognized a
//!   directory once `funnels.json` / `funnel-events.jsonl` / `scores.json`
//!   appeared, and those are written at the END. For its entire duration — the
//!   runs that take longest and most need watching — a lab run was
//!   flow-synthesized and displayed as a plain `DISPATCH`, tagged `untracked`
//!   (#1937).
//! * **A FAILED run read as live, or as merely abandoned.** `finished` meant
//!   only "`scores.json` exists", so a run that errored never set it, fell
//!   through to an idle-time heuristic, and reported `Running` while fresh and
//!   `Abandoned` once stale. Neither is what happened (#1930).
//!
//! Both are one defect: **the lab run had no start bookend and no terminal
//! record.** An observability tool whose own run records are wrong is not a
//! flawed tool, it is a tool arguing against its own thesis — the same
//! recursive standard as the "no blind runs" doctrine. So this is not a status
//! field bolted on; it is the missing half of contract 2 (dispatch liveness)
//! applied to the lab path, which never participated in it.
//!
//! # The guarantee, and its one honest limit
//!
//! [`RunLifecycle`] writes `running` at start and is **RAII-guarded**: every
//! ordinary exit path — `?`, an early `return`, `bail!`, a panic that unwinds —
//! runs [`Drop`] and stamps a terminal status. A run directory that exists
//! without a terminal record therefore means something specific, rather than
//! being the default state.
//!
//! **The limit, stated so a reader does not over-trust this:** `Drop` does not
//! run on `SIGKILL`, on a hard power loss, or under `panic = "abort"`. Those
//! leave `running` on disk forever. That residue is exactly what the existing
//! staleness heuristic is for, and it stays — but it is now the backstop for a
//! narrow, nameable case instead of the primary mechanism for every failure.
//! "100% of exit paths" means 100% of the paths a process can observe.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The record's filename inside a run directory. Also the marker the lab scan
/// keys on to recognize a live run before any other artifact exists.
pub const LIFECYCLE_FILE: &str = "lifecycle.json";

/// Bumped on any change to [`LifecycleRecord`]'s shape. Readers are
/// lenient — an unknown status reads as [`LifecycleStatus::Unknown`] rather
/// than failing the scan, matching the repo's lenient-on-read posture for
/// every other on-disk shape.
pub const LIFECYCLE_SCHEMA_VERSION: &str = "1.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleStatus {
    /// Start bookend written; no terminal record yet. Either genuinely live,
    /// or killed hard enough that `Drop` never ran (see the module doc).
    Running,
    /// Ran to completion. Says nothing about whether the work SUCCEEDED —
    /// `verify` and the provider's own result carry that.
    Complete,
    /// Ended with an error, and `error` says which.
    Error,
    /// The process exited without finishing the run — `Drop` fired on an
    /// early return, a `?`, or an unwinding panic.
    Interrupted,
    /// A status this binary does not recognize, from a newer writer.
    #[serde(other)]
    Unknown,
}

impl LifecycleStatus {
    /// Whether this is a terminal state. `Running` and `Unknown` are not:
    /// the first may still be live, and the second must not be interpreted.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Error | Self::Interrupted)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleRecord {
    pub schema_version: String,
    pub run_id: String,
    /// Always `"lab"` today. Present so the scan can tell a lab run from any
    /// other producer that later adopts this record, rather than inferring
    /// the kind from which directory it happened to be found in.
    pub kind: String,
    pub workload: String,
    pub profile: String,
    pub started_at_ms: u64,
    pub status: LifecycleStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Read a run directory's lifecycle record, or `None` when it has none.
///
/// Lenient by design: a malformed or truncated record reads as `None` rather
/// than failing the caller. A scan that refuses to list runs because one
/// directory has a bad file is a worse outcome than one stale row.
pub fn read(run_dir: &Path) -> Option<LifecycleRecord> {
    let raw = fs::read_to_string(run_dir.join(LIFECYCLE_FILE)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// The start bookend plus its RAII terminal guard.
///
/// Construct with [`RunLifecycle::start`] immediately after the run directory
/// exists and BEFORE anything that can fail — every fallible step after that
/// point is then covered.
#[derive(Debug)]
pub struct RunLifecycle {
    path: PathBuf,
    record: LifecycleRecord,
    finished: bool,
}

impl RunLifecycle {
    /// Write the `running` bookend.
    ///
    /// Fails loudly if the record cannot be written. That is deliberate: the
    /// whole point is that a run is visible from its first moment, so silently
    /// continuing without one would reintroduce the bug this closes.
    pub fn start(
        run_dir: &Path,
        run_id: &str,
        workload: &str,
        profile: &str,
    ) -> Result<Self> {
        let record = LifecycleRecord {
            schema_version: LIFECYCLE_SCHEMA_VERSION.to_string(),
            run_id: run_id.to_string(),
            kind: "lab".to_string(),
            workload: workload.to_string(),
            profile: profile.to_string(),
            started_at_ms: now_ms(),
            status: LifecycleStatus::Running,
            ended_at_ms: None,
            error: None,
        };
        let me = Self { path: run_dir.join(LIFECYCLE_FILE), record, finished: false };
        me.write().with_context(|| {
            format!("writing the lifecycle start record at {}", me.path.display())
        })?;
        Ok(me)
    }

    fn write(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.record)?;
        // Write-then-rename so a reader never observes a half-written record.
        // A torn lifecycle file is precisely the "record that lies" this type
        // exists to prevent.
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, json.as_bytes())?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    fn terminate(&mut self, status: LifecycleStatus, error: Option<String>) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.record.status = status;
        self.record.ended_at_ms = Some(now_ms());
        self.record.error = error;
        // Best-effort on the terminal write: the run is already over, and
        // returning an error from `Drop` is not possible. A failure here
        // degrades to the stale-`running` case the module doc names.
        if let Err(e) = self.write() {
            eprintln!(
                "[lab] warn: could not write the terminal lifecycle record at {}: {e}",
                self.path.display()
            );
        }
    }

    /// The run reached its end. Consumes the guard so a later `Drop` cannot
    /// overwrite the outcome.
    pub fn finish_complete(mut self) {
        self.terminate(LifecycleStatus::Complete, None);
    }

    /// The run ended with an error.
    pub fn finish_error(mut self, error: impl std::fmt::Display) {
        self.terminate(LifecycleStatus::Error, Some(error.to_string()));
    }
}

impl Drop for RunLifecycle {
    fn drop(&mut self) {
        // Reached on `?`, an early `return`, or an unwinding panic — every
        // exit path a process can observe that is not an explicit finish.
        self.terminate(LifecycleStatus::Interrupted, None);
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
