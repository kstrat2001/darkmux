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
//!
//! # Joining a live row to its own flow session (#2511)
//!
//! `start` writes the `running` bookend before the workload's provider has
//! even been called, so it cannot carry a dispatch session id yet — nothing
//! has minted one. A single-dispatch provider (`coding-task`, `prompt`)
//! mints its session id partway through its own `run()`, immediately before
//! dispatching, and reports it back via [`RunLifecycle::set_session_id`] at
//! that exact moment — so the record on disk gains `session_id` while the
//! run is still genuinely `Running`, not only once `manifest.json` is
//! written at the end. That is what makes a live lab row joinable to its
//! own flow session for a dedup/collapse consumer (`darkmux-serve::runs`)
//! during the run's dispatch phase, not just after it finishes.
//!
//! A multi-dispatch provider (`tool-bench` fans out into many sessions, one
//! per task × trial) has no single id to report and never calls
//! `set_session_id` — `session_id` stays `None` for its whole run, which is
//! the honest answer, not a fabricated representative id.

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
///
/// `1.1` (#2511) — additive: [`LifecycleRecord::session_id`] appended.
/// Older records simply lack the key and deserialize with `None` (`Option<T>`
/// is absent-tolerant on read without a `#[serde(default)]`, same as
/// `ended_at_ms`/`error` above); a pre-1.1 binary reading a 1.1 record
/// silently ignores the new key. This is `LifecycleRecord`'s OWN schema,
/// per-run-local and never written to a `FlowRecord` or the fleet flow
/// stream — it has nothing to do with `FLOW_SCHEMA_VERSION`
/// (`crates/darkmux-flow/src/schema.rs`), which versions a disjoint wire
/// shape governed by the lab/fleet sink boundary.
pub const LIFECYCLE_SCHEMA_VERSION: &str = "1.1";

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
    /// early return, a `?`, or an unwinding panic — OR the run's own error
    /// path explicitly detected that a caught SIGINT/SIGTERM/SIGHUP was the
    /// cause (`darkmux_types::interrupt::is_set()`) and called
    /// [`RunLifecycle::finish_interrupted`] rather than
    /// [`RunLifecycle::finish_error`] (#2462). The two entry paths share
    /// this one status deliberately: both mean "did not finish on its
    /// own" — the explicit path just also knows why, and records it in
    /// `error` rather than leaving the field empty.
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
    /// (#2511) The dispatch session id the provider's own inner dispatch
    /// used, once it exists. `None` at [`RunLifecycle::start`] time — the
    /// provider hasn't minted one yet — and stays `None` for the run's
    /// whole duration for a provider with no single governing dispatch
    /// session (`tool-bench` fans out into many, one per task × trial; see
    /// its own `run()` for why it never calls
    /// [`RunLifecycle::set_session_id`]). Set via
    /// [`RunLifecycle::set_session_id`] the moment a single-dispatch
    /// provider (`coding-task`, `prompt`) mints its id — BEFORE the
    /// dispatch fires, so a still-`Running` record is joinable to its own
    /// flow session for the run's live window, not only after
    /// `manifest.json` is written at the end.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
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
            session_id: None,
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

    /// (#2511) Attach the dispatch session id once the provider mints it —
    /// the record is still `Running` at this point, and stays so; only
    /// `session_id` changes. This is what closes the join gap: before this
    /// call, a live lab row has no key a flow-session consumer can match it
    /// on; after it, the still-`Running` record on disk carries the exact
    /// string the provider is about to (or already did) dispatch under.
    ///
    /// Best-effort like the terminal write below: a write failure here
    /// degrades to the pre-#2511 gap (the id only becomes visible once
    /// `manifest.json` is written at the run's end) rather than failing the
    /// dispatch — this is observability, not correctness.
    ///
    /// (#2511 review CONSIDER 5) Enforces both halves of the trait's own
    /// doc (`WorkloadProvider::run`'s `on_session_id` param: "Called AT
    /// MOST ONCE"), rather than trusting every current and future caller to
    /// honor it unchecked:
    ///
    /// - **An empty string is never a session id.** Assigning one would
    ///   still satisfy every downstream `Option::is_some()` read (this
    ///   struct's own `read`/scan consumers included) while joining to
    ///   nothing — the same non-empty guard `runs.rs`'s session-id readers
    ///   already apply is applied here at the write, so the empty case
    ///   never reaches disk in the first place.
    /// - **The first call wins.** A second call — a provider bug, or a
    ///   future caller that doesn't honor "at most once" — is a debug-time
    ///   assertion (loud in tests/dev, where the mistake belongs) and a
    ///   silent no-op in release (keeping the first, already-claimed
    ///   session rather than letting a later value overwrite something a
    ///   flow session may already be joined to).
    pub fn set_session_id(&mut self, session_id: impl Into<String>) {
        let session_id = session_id.into();
        if session_id.is_empty() {
            return;
        }
        debug_assert!(
            self.record.session_id.is_none(),
            "set_session_id called more than once on {} (already {:?}, now attempting {session_id:?}) \
             — the trait's own doc says AT MOST ONCE",
            self.path.display(),
            self.record.session_id,
        );
        if self.record.session_id.is_some() {
            return;
        }
        self.record.session_id = Some(session_id);
        if let Err(e) = self.write() {
            eprintln!(
                "[lab] warn: could not attach the dispatch session id to {}: {e}",
                self.path.display()
            );
        }
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

    /// (#2462) The run ended because a caught SIGINT/SIGTERM/SIGHUP caused
    /// darkmux's own reap watchdog to kill this run's in-flight child (the
    /// container, or the hosted `curl`) — the `Err` this run's dispatch
    /// returned is real, but its CAUSE was the operator's own signal, not
    /// the endpoint or the model. Callers decide when this applies by
    /// checking `darkmux_types::interrupt::is_set()` themselves; this
    /// method just records the distinction once they have. Recording
    /// `Error` here would archive an operator's Ctrl-C as "the endpoint
    /// broke" — exactly the misattribution #2462 is about.
    ///
    /// Formats `error` with `{:#}` (alternate), not `{}` — unlike
    /// `finish_error`'s plain `.to_string()`, this deliberately unwraps the
    /// FULL `anyhow` cause chain. `run.rs`'s call site passes a
    /// `.context("internal-runtime dispatch via lab harness")`-wrapped
    /// error; a plain `{}` shows only that outer context and discards the
    /// actual cause (`remote_chat_attempt`'s "hosted dispatch interrupted
    /// by an operator signal..." message, which is the whole point of this
    /// method existing) one level down. `finish_error` is left as `.to_
    /// string()` deliberately — this is a targeted fix for the one field
    /// #2462 is about, not a blanket change to every existing error record.
    pub fn finish_interrupted(mut self, error: impl std::fmt::Display) {
        self.terminate(LifecycleStatus::Interrupted, Some(format!("{error:#}")));
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
