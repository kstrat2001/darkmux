//! [`LmsHost`] — the [`ModelHost`] port over the `lms` CLI (#1274 packet 2b).
//!
//! Sibling to (not a replacement for) `crate::lms`: the existing wrappers
//! keep serving `swap.rs` untouched until the packet-3 cutover. This adapter
//! differs from them in exactly the ways the gestalt ports require:
//!
//! - **Enforced deadline on EVERY call (#1276).** The current
//!   `lms::load_with_identifier` blocks indefinitely via `Command::status()`
//!   — a wrong model id hangs the dispatch until the workflow's outer kill.
//!   Here every `lms` child — load, unload, and the read-only ps/ls — is
//!   spawned, polled with `try_wait`, and hard-killed at expiry, returning a
//!   typed [`HostError::Timeout`] naming the phase. Mutating calls run under
//!   the caller's [`Deadline`]; the list calls (whose port signatures carry
//!   no deadline — a packet-3 contract question) run under an adapter-level
//!   bound ([`DEFAULT_LIST_BOUND`]), and the post-load provenance re-list
//!   under the load deadline's remaining budget.
//! - **Raw `sizeBytes` (#1243 budget accounting).** `LoadedModel.size` is a
//!   display string; [`ResidentFact::est_bytes`] wants bytes. The JSON is
//!   parsed directly — never reformatted-and-reparsed.
//! - **Typed error shapes.** stderr is captured (not inherited) so the
//!   failure can be classified into the `Eq` [`HostError`] vocabulary the
//!   packet-3 executor matches on, instead of an opaque anyhow bail.

use crate::lms::lms_bin;
use darkmux_gestalt::{CatalogFact, Deadline, HostError, LoadReport, ModelHost, ResidentFact};
use darkmux_gestalt::OwnedTarget;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How often the deadline loop polls `try_wait` on the child. Short enough
/// that a tight test deadline (200 ms) is honored with useful resolution;
/// long enough to be free at the real 600 s default.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Adapter-level fixed bound on the read-only list calls (`lms ps`/`lms ls`).
/// The gestalt port reads take no `Deadline` today — whether they should is a
/// packet-3 contract question (see the [`ModelHost`] impl note) — so the
/// adapter supplies a generous bound of its own: a wedged `lms ps` must not
/// hang plan assembly any more than a wedged `lms load` may hang execution
/// (#1276).
const DEFAULT_LIST_BOUND: Duration = Duration::from_secs(30);

/// The `lms`-CLI implementation of the gestalt [`ModelHost`] port.
///
/// Holds the resolved binary path at construction: [`LmsHost::new`] resolves
/// `env(DARKMUX_LMS_BIN) > config.lms_bin > "lms"` (the #661 precedence via
/// `crate::lms::lms_bin`); [`LmsHost::with_bin`] pins an explicit path —
/// used by the deadline tests to point at a stub binary without mutating
/// process env, and available to callers embedding a known path.
#[derive(Debug, Clone)]
pub struct LmsHost {
    bin: String,
    /// Bound applied to the read-only list calls (see [`DEFAULT_LIST_BOUND`]);
    /// also caps the post-load provenance re-list inside [`ModelHost::load`].
    list_bound: Duration,
}

impl Default for LmsHost {
    fn default() -> Self {
        Self::new()
    }
}

impl LmsHost {
    pub fn new() -> Self {
        Self { bin: lms_bin(), list_bound: DEFAULT_LIST_BOUND }
    }

    pub fn with_bin(bin: impl Into<String>) -> Self {
        Self { bin: bin.into(), list_bound: DEFAULT_LIST_BOUND }
    }

    /// Override the adapter-level list-call bound (defaults to
    /// [`DEFAULT_LIST_BOUND`]). The test seam for the wedged-`lms ps` cases;
    /// also available to callers with a tighter latency budget.
    pub fn with_list_bound(mut self, bound: Duration) -> Self {
        self.list_bound = bound;
        self
    }

    /// `lms ps --json` under an explicit deadline — shared by the port's
    /// [`ModelHost::list_resident`] (adapter bound) and the post-load
    /// provenance re-list (remaining load budget, #1276).
    fn ps_bounded(&self, deadline: Deadline) -> Result<Vec<ResidentFact>, HostError> {
        let rows = self.run_json_list(&["ps", "--json"], "ps", deadline)?;
        Ok(rows.iter().map(resident_fact_from_json).collect())
    }

    /// Run `lms <args>` under `deadline` and parse a JSON array off stdout —
    /// the shared body of both list calls. Same polling mechanics as the
    /// mutating calls ([`run_bounded`]); stdout is captured instead of nulled.
    fn run_json_list(
        &self,
        args: &[&str],
        phase: &'static str,
        deadline: Deadline,
    ) -> Result<Vec<serde_json::Value>, HostError> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args);
        let run = run_bounded(cmd, phase, deadline, StdoutMode::Capture)?;
        if !run.status.success() {
            return Err(HostError::CommandFailed {
                detail: format!("`lms {phase} --json` {}", run.exit_detail()),
            });
        }
        let parsed: serde_json::Value = serde_json::from_str(&run.stdout).map_err(|e| {
            HostError::CommandFailed {
                detail: format!("`lms {phase} --json` output is not JSON: {e}"),
            }
        })?;
        match parsed {
            serde_json::Value::Array(rows) => Ok(rows),
            _ => Err(HostError::CommandFailed {
                detail: format!("`lms {phase} --json` output is not a JSON array"),
            }),
        }
    }
}

impl ModelHost for LmsHost {
    /// `lms ps --json` → resident facts, HOST ORDER PRESERVED — the order is
    /// decision-bearing (first-match-wins residency + deterministic budget
    /// eviction walk; see the `ResidentFact` docs). Adapters MUST NOT sort,
    /// and this one doesn't.
    ///
    /// Bounded at the ADAPTER level by `list_bound` ([`DEFAULT_LIST_BOUND`],
    /// same polling mechanics as the mutating calls): the [`ModelHost`] read
    /// signatures take no `Deadline` today, and whether they should is a
    /// packet-3 contract question — until that's settled the adapter refuses
    /// to run any `lms` child unbounded (#1276).
    fn list_resident(&mut self) -> Result<Vec<ResidentFact>, HostError> {
        self.ps_bounded(Deadline(self.list_bound))
    }

    /// `lms ls --json` → catalog facts (the #1276 existence fast-fail input +
    /// the estimator's base term). A failure is a typed error here; the
    /// LENIENCY (catalog unavailable ⇒ `Facts.catalog = None`, fast-fail
    /// skipped not failed) belongs to the facts-assembling caller, per the
    /// `Facts::catalog` contract. Adapter-bounded like `list_resident`.
    fn list_catalog(&mut self) -> Result<Vec<CatalogFact>, HostError> {
        let rows = self.run_json_list(&["ls", "--json"], "ls", Deadline(self.list_bound))?;
        Ok(rows.iter().filter_map(catalog_fact_from_json).collect())
    }

    /// `lms load <key> --context-length <ctx> --identifier <id> -y` under the
    /// enforced deadline. Flags match `crate::lms::load_with_identifier` plus
    /// `-y`: inside a dispatch the load must never wait on an interactive
    /// confirmation (#1276 — "never let `lms load` decide to download or wait
    /// interactively"); the deadline backstops whatever `-y` doesn't cover.
    ///
    /// stdout is ALWAYS nulled (the #1135 envelope-safety lesson: the load
    /// spinner leaking to stdout corrupts a `--json` dispatch envelope).
    /// stderr is captured rather than inherited — it feeds the typed error
    /// classification, and failures re-surface it in the error detail.
    ///
    /// **Orphan-load contract (#1276):** deadline expiry kills the CLIENT
    /// `lms` process only — the LMStudio server may still complete the load
    /// after the kill and leave a `darkmux:*` resident behind. That orphan is
    /// reconciled, not leaked: the gestalt executor re-verifies preconditions
    /// from a fresh `list_resident` at the next plan, observes the resident,
    /// and — absolute ownership over the darkmux namespace (#1274 contract) —
    /// treats it as darkmux's to unload or reuse.
    fn load(
        &mut self,
        model_key: &str,
        identifier: &str,
        min_ctx: u32,
        deadline: Deadline,
    ) -> Result<LoadReport, HostError> {
        let mut cmd = Command::new(&self.bin);
        cmd.args([
            "load",
            model_key,
            "--context-length",
            &min_ctx.to_string(),
            "--identifier",
            identifier,
            "-y",
        ]);
        let started = Instant::now();
        let run = run_bounded(cmd, "load", deadline, StdoutMode::Null)?;
        if !run.status.success() {
            return Err(classify_load_failure(&run.stderr, model_key, run.exit_detail()));
        }
        // Best-effort post-load provenance (the #1257 interim): re-list
        // residents and report the ctx the host actually resolved. The
        // re-list is part of `load()`'s observable wall clock, so it runs
        // under what's LEFT of the deadline (capped at the adapter list
        // bound) — an unbounded re-list here let a wedged `lms ps` hang a
        // SUCCESSFUL load forever despite the #1276 mechanics on the load
        // child itself. Any re-list failure, timeout included, degrades to
        // `resolved_ctx: None`: the load succeeded and is never failed for
        // provenance.
        let resolved_ctx = self
            .ps_bounded(relist_bound(deadline, started.elapsed(), self.list_bound))
            .ok()
            .and_then(|residents| {
                residents.iter().find(|r| r.identifier == identifier).map(|r| r.ctx)
            })
            .filter(|ctx| *ctx > 0);
        Ok(LoadReport { resolved_ctx, ..Default::default() })
    }

    /// `lms unload <identifier>` under the same deadline mechanics. Only a
    /// claim-checked [`OwnedTarget`] can reach this call — the namespace
    /// contract is structural at the port seam.
    ///
    /// Outcome classification is EXIT-CODE-BLIND for the not-resident shape:
    /// the real `lms unload` of a non-resident identifier EXITS 0 with the
    /// error on stderr only ("Model Not Found / Cannot find a model with the
    /// identifier …", live-verified), so a bare `!status.success()` gate made
    /// [`HostError::NotResident`] unreachable and the #1279 double-release
    /// invisible — the executor would believe bytes were freed. Error-shaped
    /// stderr on a 0-exit unload is a failure; see
    /// [`classify_unload_outcome`].
    fn unload(&mut self, target: &OwnedTarget, deadline: Deadline) -> Result<(), HostError> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(["unload", target.identifier()]);
        let run = run_bounded(cmd, "unload", deadline, StdoutMode::Null)?;
        match classify_unload_outcome(
            run.status.success(),
            &run.stderr,
            target.identifier(),
            &run.exit_detail(),
        ) {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

// ── JSON → fact parsers (pure; canned-payload tests below) ──

/// One `lms ps --json` row → [`ResidentFact`]. Field fallback chains mirror
/// `crate::lms::model_from_json` (`identifier`|`id`; `modelKey`|`model`|`id`;
/// `contextLength`|`context`), but `est_bytes` reads the RAW `sizeBytes`
/// integer — the display-string `size` field is never parsed back to bytes.
fn resident_fact_from_json(v: &serde_json::Value) -> ResidentFact {
    let identifier = v
        .get("identifier")
        .or_else(|| v.get("id"))
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let model_key = v
        .get("modelKey")
        .or_else(|| v.get("model"))
        .or_else(|| v.get("id"))
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let ctx = v
        .get("contextLength")
        .or_else(|| v.get("context"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let est_bytes = v.get("sizeBytes").and_then(|x| x.as_u64());
    ResidentFact { identifier, model_key, ctx, est_bytes }
}

/// One `lms ls --json` row → [`CatalogFact`]. `modelKey` is required (a row
/// without one is skipped — nothing to plan against); `sizeBytes` stays
/// `Option` so a size-less row degrades to the estimator's unknowable path
/// rather than a fake 0-byte model.
fn catalog_fact_from_json(v: &serde_json::Value) -> Option<CatalogFact> {
    let model_key = v.get("modelKey").and_then(|s| s.as_str())?.to_string();
    let size_bytes = v.get("sizeBytes").and_then(|n| n.as_u64());
    Some(CatalogFact { model_key, size_bytes })
}

/// The bound for the post-load provenance re-list (#1276): what's LEFT of
/// the load deadline after the load child ran, capped at the adapter list
/// bound. Zero remaining ⇒ an immediately-expiring deadline — the re-list is
/// skipped-by-timeout, never run unbounded. Pure so both terms of the `min`
/// are table-testable without a clock.
fn relist_bound(deadline: Deadline, load_elapsed: Duration, list_bound: Duration) -> Deadline {
    Deadline(deadline.0.saturating_sub(load_elapsed).min(list_bound))
}

// ── Deadline mechanics (#1276) ──

/// Whether the child's stdout is discarded or captured. Mutating calls null
/// it (the #1135 envelope-safety lesson: the load spinner leaking to stdout
/// corrupts a `--json` dispatch envelope); the `--json` list calls capture
/// it — through the same drain mechanics as stderr, never an unbounded
/// `Command::output()`.
///
/// `pub(crate)`: the #1286 memory-ledger gather reuses the same bounded-run
/// mechanism for its kernel-counter + lms-metadata probes (the ledger's
/// observer-effect constraint forbids unbounded child runs too).
pub(crate) enum StdoutMode {
    Null,
    Capture,
}

/// Outcome of a bounded child run that finished before the deadline.
pub(crate) struct BoundedRun {
    pub(crate) status: std::process::ExitStatus,
    /// Empty under [`StdoutMode::Null`].
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl BoundedRun {
    /// The generic failure detail: exit status + trimmed stderr.
    pub(crate) fn exit_detail(&self) -> String {
        format!("exited with {}: {}", self.status, self.stderr.trim())
    }
}

/// TOTAL bound on collecting a pipe's chunks from its drain thread after the
/// child has EXITED. The exited child's writes are already complete (either
/// read by the drain thread or sitting in the OS pipe buffer, which the
/// drain thread reads promptly), so one short window collects everything.
///
/// The bound is TOTAL, deliberately not per-chunk quiet-interval: a surviving
/// grandchild that inherited the pipe and keeps writing at a sub-interval
/// cadence resets a quiet-interval timer for its whole LIFETIME
/// (demonstrated — a 100 ms-cadence writer held the old grace open for 30 s),
/// while against a hard cap it can only pad the buffer that gets drained
/// before moving on. See [`collect_within`].
const PIPE_GRACE: Duration = Duration::from_millis(200);

/// Drain `pipe` on a detached thread, streaming chunks over a channel.
///
/// The thread is NEVER joined, and chunks flow back over the channel instead
/// of the thread's return value. This is load-bearing, found by the deadline
/// test itself: killing the direct child does not close the pipe when a
/// grandchild inherited its write end (the shell-wrapping stub's `sleep`;
/// any tool that leaves a background process behind), so a join would block
/// until the GRANDCHILD exits — the exact #1276 hang this machinery exists
/// to remove, re-entering through the drain. The detached thread ends on its
/// own when the pipe finally closes or the receiver is dropped; collection
/// after child exit is bounded by [`PIPE_GRACE`]. A `None` pipe yields an
/// immediately-disconnected channel (no thread spawned).
fn spawn_drain<R>(pipe: Option<R>) -> std::sync::mpsc::Receiver<String>
where
    R: std::io::Read + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let Some(mut pipe) = pipe else { return rx }; // tx drops → disconnected
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(String::from_utf8_lossy(&buf[..n]).into_owned()).is_err() {
                        break; // receiver gone (timeout path) — stop reading
                    }
                }
            }
        }
    });
    rx
}

/// Collect whatever a drain thread delivers within a TOTAL `cap` window.
/// Returns as soon as the channel disconnects (pipe EOF — everything
/// arrived); a pipe held open by a grandchild costs at most `cap`, however
/// chattily the grandchild writes (#1276 — the grace is total-bounded, not
/// quiet-interval-bounded).
fn collect_within(rx: &std::sync::mpsc::Receiver<String>, cap: Duration) -> String {
    let mut out = String::new();
    let hard_stop = Instant::now() + cap;
    loop {
        let now = Instant::now();
        if now >= hard_stop {
            break;
        }
        match rx.recv_timeout(hard_stop - now) {
            Ok(chunk) => out.push_str(&chunk),
            // Disconnected (EOF) or the cap expired — either way, stop
            // waiting.
            Err(_) => break,
        }
    }
    // Drain what already arrived without blocking, then move on.
    while let Ok(chunk) = rx.try_recv() {
        out.push_str(&chunk);
    }
    out
}

/// Spawn `cmd` and wait for it under `deadline` — the #1276 fix as a
/// mechanism: `spawn` + `try_wait` polling (std-only; no wait-timeout crate),
/// and on expiry `kill()` + `wait()` (reaped, no zombie) + a typed
/// [`HostError::Timeout`] naming the phase.
///
/// stdin is nulled (a host call is never interactive); stdout follows
/// `stdout_mode`; stderr is always piped. Piped streams are drained on
/// dedicated threads ([`spawn_drain`]) — draining concurrently is
/// load-bearing: a chatty child that fills the OS pipe buffer would
/// otherwise block forever and read as a timeout. Post-exit collection is
/// total-bounded per stream by [`PIPE_GRACE`] ([`collect_within`]).
pub(crate) fn run_bounded(
    mut cmd: Command,
    phase: &'static str,
    deadline: Deadline,
    stdout_mode: StdoutMode,
) -> Result<BoundedRun, HostError> {
    cmd.stdin(Stdio::null());
    cmd.stdout(match stdout_mode {
        StdoutMode::Null => Stdio::null(),
        StdoutMode::Capture => Stdio::piped(),
    });
    cmd.stderr(Stdio::piped());
    // Name the actual program in the spawn error: the #1286 ledger reuses this
    // runner for non-`lms` probes (vm_stat / sysctl / ps), so a hardcoded
    // "lms" prefix would mislabel those failures.
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .spawn()
        .map_err(|e| HostError::CommandFailed { detail: format!("spawning `{program}` ({phase}): {e}") })?;
    let stdout_rx = spawn_drain(child.stdout.take());
    let stderr_rx = spawn_drain(child.stderr.take());
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = collect_within(&stdout_rx, PIPE_GRACE);
                let stderr = collect_within(&stderr_rx, PIPE_GRACE);
                return Ok(BoundedRun { status, stdout, stderr });
            }
            Ok(None) => {
                let waited = start.elapsed();
                if waited >= deadline.0 {
                    let _ = child.kill();
                    let _ = child.wait(); // reap — the kill must not leave a zombie
                    return Err(HostError::Timeout { phase, waited });
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(HostError::CommandFailed {
                    detail: format!("waiting on `lms {phase}`: {e}"),
                });
            }
        }
    }
}

// ── stderr → typed-error classification (pure; fixture tests below) ──

/// Classify a failed `lms load`'s stderr into the typed [`HostError`]
/// vocabulary. Best-effort substring matching over the failure wordings the
/// `lms` CLI is known to emit — an unmatched stderr falls through to
/// [`HostError::CommandFailed`] carrying the full detail, never a silent
/// misclassification.
fn classify_load_failure(stderr: &str, model_key: &str, detail: String) -> HostError {
    let lower = stderr.to_ascii_lowercase();
    const UNKNOWN_MODEL: &[&str] =
        &["cannot find a model", "no model found", "model not found", "no models found"];
    // Memory-context wordings ONLY: a bare "insufficient" substring swept
    // "insufficient permissions" / "insufficient disk space" into the memory
    // fast-fail shape (#1139), which the planner treats as a capacity fact.
    // Non-memory wordings fall through to CommandFailed with the full detail
    // preserved — an unmatched real OOM wording degrades loudly, never
    // misclassifies silently.
    const INSUFFICIENT_MEMORY: &[&str] =
        &["insufficient memory", "not enough memory", "out of memory", "requires more memory"];
    if UNKNOWN_MODEL.iter().any(|p| lower.contains(p)) {
        return HostError::UnknownModel { model_key: model_key.to_string() };
    }
    if INSUFFICIENT_MEMORY.iter().any(|p| lower.contains(p)) {
        return HostError::InsufficientResources { detail: stderr.trim().to_string() };
    }
    HostError::CommandFailed { detail: format!("`lms load` {detail}") }
}

/// Classify an `lms unload` outcome: `None` = success, `Some` = the typed
/// failure.
///
/// "Not found"-shaped stderr maps to [`HostError::NotResident`] REGARDLESS
/// of exit code: the real CLI exits 0 for a non-resident identifier with the
/// error on stderr only (live-verified — "Model Not Found / Cannot find a
/// model with the identifier …"), and a `!success` gate alone made the #1279
/// double-release invisible. For an unload the only referent is the
/// identifier, so the broad match is safe here (unlike load, where "not
/// found" could name other things). Beyond that: a nonzero exit is a failure
/// whatever stderr says, and error-shaped stderr on a 0 exit is a failure
/// too — never a silent success. Non-error stderr noise (progress remnants)
/// on a 0 exit passes.
fn classify_unload_outcome(
    success: bool,
    stderr: &str,
    identifier: &str,
    exit_detail: &str,
) -> Option<HostError> {
    let lower = stderr.to_ascii_lowercase();
    const NOT_RESIDENT: &[&str] =
        &["no model", "not loaded", "not found", "no such model", "cannot find a model"];
    if NOT_RESIDENT.iter().any(|p| lower.contains(p)) {
        return Some(HostError::NotResident { identifier: identifier.to_string() });
    }
    if !success {
        return Some(HostError::CommandFailed { detail: format!("`lms unload` {exit_detail}") });
    }
    if lower.contains("error") {
        return Some(HostError::CommandFailed {
            detail: format!("`lms unload` exited 0 with error output: {}", stderr.trim()),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── parser tests (canned lms ps/ls payloads) ──

    #[test]
    fn resident_fact_reads_raw_size_bytes_not_display_string() {
        // The real `lms ps --json` wire shape: `sizeBytes` integer beside a
        // possible display `size` string. est_bytes must come from the raw
        // integer — never a reformat-and-reparse of the display string.
        let v = json!({
            "identifier": "darkmux:gpt-oss-20b",
            "modelKey": "openai/gpt-oss-20b",
            "status": "idle",
            "size": "12.10 GB",
            "sizeBytes": 12_104_297_682u64,
            "contextLength": 32768
        });
        let f = resident_fact_from_json(&v);
        assert_eq!(f.identifier, "darkmux:gpt-oss-20b");
        assert_eq!(f.model_key, "openai/gpt-oss-20b");
        assert_eq!(f.ctx, 32768);
        assert_eq!(f.est_bytes, Some(12_104_297_682));
    }

    #[test]
    fn resident_fact_missing_size_bytes_is_none_not_zero() {
        // A display-string-only payload (older shim) has no raw bytes —
        // est_bytes is the documented unknown (`ResidentBytesUnknown`
        // degradation), never a parsed-back or fabricated number.
        let v = json!({
            "identifier": "x",
            "modelKey": "x",
            "size": "5.00 GB",
            "contextLength": 1000
        });
        assert_eq!(resident_fact_from_json(&v).est_bytes, None);
    }

    #[test]
    fn resident_fact_context_fallback_chain() {
        // contextLength > context > 0 — mirrors `lms::model_from_json`.
        let primary = json!({"identifier": "a", "modelKey": "a", "contextLength": 68000});
        assert_eq!(resident_fact_from_json(&primary).ctx, 68000);
        let fallback = json!({"identifier": "a", "modelKey": "a", "context": 4096});
        assert_eq!(resident_fact_from_json(&fallback).ctx, 4096);
        let absent = json!({"identifier": "a", "modelKey": "a"});
        assert_eq!(resident_fact_from_json(&absent).ctx, 0, "unknown ctx compares as tiny (#1135 caution)");
    }

    #[test]
    fn resident_fact_model_key_fallback_chain() {
        let with_key = json!({"identifier": "i", "modelKey": "mk", "model": "m", "id": "d"});
        assert_eq!(resident_fact_from_json(&with_key).model_key, "mk");
        let with_model = json!({"identifier": "i", "model": "m", "id": "d"});
        assert_eq!(resident_fact_from_json(&with_model).model_key, "m");
        let id_only = json!({"id": "d"});
        let f = resident_fact_from_json(&id_only);
        assert_eq!(f.model_key, "d");
        assert_eq!(f.identifier, "d");
    }

    #[test]
    fn catalog_fact_requires_model_key_and_keeps_size_optional() {
        assert_eq!(
            catalog_fact_from_json(&json!({"modelKey": "qwen/qwen3-4b-2507", "sizeBytes": 2_500_000_000u64})),
            Some(CatalogFact {
                model_key: "qwen/qwen3-4b-2507".into(),
                size_bytes: Some(2_500_000_000)
            })
        );
        // No sizeBytes → Some fact, unknown size (the estimator's unknowable
        // path) — not a fake 0.
        assert_eq!(
            catalog_fact_from_json(&json!({"modelKey": "k"})),
            Some(CatalogFact { model_key: "k".into(), size_bytes: None })
        );
        // No modelKey → skipped entirely.
        assert_eq!(catalog_fact_from_json(&json!({"sizeBytes": 5})), None);
    }

    // ── error-shape classification (stderr fixtures → typed variants) ──

    #[test]
    fn load_stderr_unknown_model_shapes() {
        for stderr in [
            "Error: Cannot find a model matching the provided path (qwen3-4b-instruct-2507)",
            "No model found matching \"qwen3-4b-instruct-2507\"",
            "error: model not found",
        ] {
            assert_eq!(
                classify_load_failure(stderr, "qwen3-4b-instruct-2507", "exited with 1".into()),
                HostError::UnknownModel { model_key: "qwen3-4b-instruct-2507".into() },
                "stderr: {stderr}"
            );
        }
    }

    #[test]
    fn load_stderr_insufficient_memory_shapes() {
        for stderr in [
            "Error: insufficient memory to load this model",
            "error: not enough memory to load model at this context length",
            "Error: out of memory while allocating KV cache",
            "Error: this model requires more memory than is available",
        ] {
            let err = classify_load_failure(stderr, "m", "exited with 1".into());
            assert!(
                matches!(err, HostError::InsufficientResources { ref detail } if detail.contains(stderr.trim())),
                "stderr: {stderr} → {err:?}"
            );
        }
    }

    #[test]
    fn load_stderr_insufficient_without_memory_context_is_command_failed() {
        // Classifier precision: a bare "insufficient" substring mapped
        // permission and disk failures to InsufficientResources — a capacity
        // fact to the planner (#1139). Only memory-context wordings classify;
        // everything else keeps its full detail in CommandFailed. That
        // includes generic "insufficient system resources" wordings — an
        // unmatched shape degrades loud, not misclassified.
        for stderr in [
            "Error: insufficient permissions to access the model directory",
            "Error: insufficient disk space to download this model",
            "Error: Insufficient system resources to load this model",
        ] {
            let err = classify_load_failure(
                stderr,
                "m",
                format!("exited with exit status: 1: {stderr}"),
            );
            assert!(
                matches!(err, HostError::CommandFailed { ref detail } if detail.contains(stderr)),
                "stderr: {stderr} → {err:?}"
            );
        }
    }

    #[test]
    fn load_stderr_unmatched_falls_through_to_command_failed_with_detail() {
        let err = classify_load_failure(
            "some novel failure wording",
            "m",
            "exited with exit status: 1: some novel failure wording".into(),
        );
        match err {
            HostError::CommandFailed { detail } => {
                assert!(detail.contains("novel failure wording"), "{detail}");
                assert!(detail.contains("`lms load`"), "{detail}");
            }
            other => panic!("expected CommandFailed, got {other:?}"),
        }
    }

    /// The exact stderr the live `lms unload` emits for a non-resident
    /// identifier — WITH exit 0 (verified live 2026-07-10, #1279).
    const LIVE_NOT_FOUND_STDERR: &str = "Model Not Found\n\nCannot find a model with the identifier \"darkmux:m\".\n\nTo see a list of loaded models, run:\n\n    lms ps\n";

    #[test]
    fn unload_exit_zero_with_live_not_found_stderr_is_not_resident() {
        // THE #1279 visibility fix: the real CLI exits 0 here, so an
        // exit-code-only gate never fired and the executor believed bytes
        // were freed on a double release.
        assert_eq!(
            classify_unload_outcome(true, LIVE_NOT_FOUND_STDERR, "darkmux:m", "exited with exit status: 0"),
            Some(HostError::NotResident { identifier: "darkmux:m".into() })
        );
    }

    #[test]
    fn unload_stderr_not_resident_shapes_regardless_of_exit_code() {
        for stderr in [
            "Error: No model found with identifier \"darkmux:m\"",
            "error: darkmux:m is not loaded",
            "Model \"darkmux:m\" not found",
            "Cannot find a model with the identifier \"darkmux:m\".",
        ] {
            for success in [true, false] {
                assert_eq!(
                    classify_unload_outcome(success, stderr, "darkmux:m", "exited"),
                    Some(HostError::NotResident { identifier: "darkmux:m".into() }),
                    "stderr: {stderr}, success: {success}"
                );
            }
        }
    }

    #[test]
    fn unload_exit_zero_clean_or_noise_stderr_is_success() {
        // A quiet 0-exit unload is the success shape; non-error stderr noise
        // (progress remnants) doesn't fail it.
        assert_eq!(classify_unload_outcome(true, "", "darkmux:m", "exited with exit status: 0"), None);
        assert_eq!(
            classify_unload_outcome(true, "Unloading darkmux:m...", "darkmux:m", "exited with exit status: 0"),
            None
        );
    }

    #[test]
    fn unload_exit_zero_with_error_shaped_stderr_is_command_failed() {
        // Error-shaped stderr on a 0 exit = failure, never a silent success
        // (the same trust-stderr-over-exit-code lesson as NotResident).
        let err = classify_unload_outcome(
            true,
            "Error: something novel exploded",
            "darkmux:m",
            "exited with exit status: 0",
        );
        assert!(
            matches!(err, Some(HostError::CommandFailed { ref detail }) if detail.contains("something novel exploded")),
            "{err:?}"
        );
    }

    #[test]
    fn unload_stderr_unmatched_nonzero_exit_falls_through_to_command_failed() {
        let err = classify_unload_outcome(
            false,
            "kaboom",
            "darkmux:m",
            "exited with exit status: 1: kaboom",
        );
        assert!(matches!(err, Some(HostError::CommandFailed { .. })), "{err:?}");
    }

    // ── deadline enforcement against a stub lms binary (#1276) ──
    //
    // The stub is pointed at via `LmsHost::with_bin` (no env mutation, no
    // `#[serial]` needed, no race with the crate's other DARKMUX_LMS_BIN
    // tests). The env resolution itself is covered by `new_honors_env_bin`.

    /// Write an executable shell stub standing in for `lms` (the
    /// `write_stub_lms` pattern from `darkmux-lab`'s review tests), with a
    /// caller-supplied body dispatching on `$1`.
    #[cfg(unix)]
    fn write_stub(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("lms-stub.sh");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "{body}").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[cfg(unix)]
    fn owned(identifier: &str) -> OwnedTarget {
        OwnedTarget::claim(identifier, None).expect("darkmux-namespaced")
    }

    #[cfg(unix)]
    #[test]
    fn load_deadline_kills_hung_child_and_returns_typed_timeout() {
        // THE #1276 regression test: a load that would block far past the
        // bound (the stub sleeps 30 s; today's `load_with_identifier` would
        // sit in `Command::status()` for all 30) is killed at the 200 ms
        // deadline and surfaces as a typed Timeout naming the phase.
        //
        // `sleep 30 & wait` (not a bare `sleep 30`) pins the WORST shape:
        // the shell forks the sleep instead of exec-ing it, so the kill hits
        // only the direct child while a grandchild survives holding the
        // stderr pipe open — the case that flaked the first version of this
        // test by blocking `run_bounded` in a drain-thread join.
        let dir = tempfile::TempDir::new().unwrap();
        let stub = write_stub(dir.path(), "sleep 30 &\nwait");
        let mut host = LmsHost::with_bin(stub.to_string_lossy());
        let started = Instant::now();
        let err = host
            .load("qwen/qwen3-4b-2507", "darkmux:qwen3-4b", 32768, Deadline(Duration::from_millis(200)))
            .unwrap_err();
        let elapsed = started.elapsed();
        match err {
            HostError::Timeout { phase, waited } => {
                assert_eq!(phase, "load");
                assert!(waited >= Duration::from_millis(200), "waited {waited:?}");
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
        assert!(
            elapsed < Duration::from_secs(5),
            "the child was killed at the deadline, not waited out ({elapsed:?})"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unload_deadline_kills_hung_child_and_returns_typed_timeout() {
        let dir = tempfile::TempDir::new().unwrap();
        let stub = write_stub(dir.path(), "sleep 30");
        let mut host = LmsHost::with_bin(stub.to_string_lossy());
        let err = host
            .unload(&owned("darkmux:m"), Deadline(Duration::from_millis(200)))
            .unwrap_err();
        assert!(
            matches!(err, HostError::Timeout { phase: "unload", .. }),
            "expected unload-phase Timeout, got {err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn load_unknown_model_stderr_maps_through_real_spawn_path() {
        // End-to-end through run_bounded: a stub that fails with the
        // known "cannot find a model" wording → typed UnknownModel.
        let dir = tempfile::TempDir::new().unwrap();
        let stub = write_stub(
            dir.path(),
            "echo 'Error: Cannot find a model matching the provided path (bogus)' >&2\nexit 1",
        );
        let mut host = LmsHost::with_bin(stub.to_string_lossy());
        let err = host
            .load("bogus", "darkmux:bogus", 4096, Deadline(Duration::from_secs(10)))
            .unwrap_err();
        assert_eq!(err, HostError::UnknownModel { model_key: "bogus".into() });
    }

    #[cfg(unix)]
    #[test]
    fn unload_not_resident_exit_zero_stderr_maps_through_real_spawn_path() {
        // The LIVE CLI shape (#1279): the not-found error arrives on stderr
        // with EXIT 0. The old `!status.success()` gate returned Ok here —
        // NotResident never fired and the double-release was invisible.
        let dir = tempfile::TempDir::new().unwrap();
        let stub = write_stub(
            dir.path(),
            "echo 'Model Not Found' >&2\necho >&2\necho 'Cannot find a model with the identifier \"darkmux:m\".' >&2\nexit 0",
        );
        let mut host = LmsHost::with_bin(stub.to_string_lossy());
        let err = host.unload(&owned("darkmux:m"), Deadline(Duration::from_secs(10))).unwrap_err();
        assert_eq!(err, HostError::NotResident { identifier: "darkmux:m".into() });
    }

    #[cfg(unix)]
    #[test]
    fn unload_not_resident_nonzero_exit_still_classified() {
        // Older/other CLI behavior: same wording family with a nonzero exit
        // stays classified — the stderr shape is the signal either way.
        let dir = tempfile::TempDir::new().unwrap();
        let stub = write_stub(
            dir.path(),
            "echo 'Error: No model found with identifier \"darkmux:m\"' >&2\nexit 1",
        );
        let mut host = LmsHost::with_bin(stub.to_string_lossy());
        let err = host.unload(&owned("darkmux:m"), Deadline(Duration::from_secs(10))).unwrap_err();
        assert_eq!(err, HostError::NotResident { identifier: "darkmux:m".into() });
    }

    #[cfg(unix)]
    #[test]
    fn exit_returns_promptly_even_when_grandchild_holds_stderr_open() {
        // The drain-thread subtlety: the child exits 0 immediately (the live
        // CLI shape), but a backgrounded grandchild inherited its stderr
        // write end and lives on for 30 s — the pipe never hits EOF. The
        // call must still return within the bounded PIPE_GRACE with the
        // stderr the child DID write (classified), not block on pipe close.
        let dir = tempfile::TempDir::new().unwrap();
        let stub = write_stub(
            dir.path(),
            "sleep 30 &\necho 'Cannot find a model with the identifier \"darkmux:m\".' >&2\nexit 0",
        );
        let mut host = LmsHost::with_bin(stub.to_string_lossy());
        let started = Instant::now();
        let err = host.unload(&owned("darkmux:m"), Deadline(Duration::from_secs(10))).unwrap_err();
        assert_eq!(err, HostError::NotResident { identifier: "darkmux:m".into() });
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "returned within the pipe grace, not on grandchild exit ({:?})",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn stderr_grace_is_total_bounded_under_chatty_grandchild() {
        // A grandchild writing at a sub-cap cadence must not extend the wait
        // for its lifetime: the old quiet-interval collection reset its
        // 200 ms timer on EVERY chunk, so a 100 ms-cadence writer held the
        // "grace" open for its whole 30 s life (demonstrated). The total
        // bound drains what arrived and moves on (#1276).
        let dir = tempfile::TempDir::new().unwrap();
        let stub = write_stub(
            dir.path(),
            "( i=0; while [ $i -lt 300 ]; do echo tick >&2; sleep 0.1; i=$((i+1)); done ) &\necho 'Cannot find a model with the identifier \"darkmux:m\".' >&2\nexit 0",
        );
        let mut host = LmsHost::with_bin(stub.to_string_lossy());
        let started = Instant::now();
        let err = host.unload(&owned("darkmux:m"), Deadline(Duration::from_secs(10))).unwrap_err();
        assert_eq!(err, HostError::NotResident { identifier: "darkmux:m".into() });
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "total-bounded grace, not per-chunk quiet interval ({:?})",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn successful_load_reports_resolved_ctx_from_relist() {
        // The #1257 interim: after a 0-exit load, the adapter re-lists
        // residents and reports the ctx the host actually resolved.
        let dir = tempfile::TempDir::new().unwrap();
        let stub = write_stub(
            dir.path(),
            r#"case "$1" in
  ps) echo '[{"identifier":"darkmux:qwen3-4b","modelKey":"qwen/qwen3-4b-2507","sizeBytes":2500000000,"contextLength":32768}]' ;;
  load) exit 0 ;;
esac"#,
        );
        let mut host = LmsHost::with_bin(stub.to_string_lossy());
        let report = host
            .load("qwen/qwen3-4b-2507", "darkmux:qwen3-4b", 32768, Deadline(Duration::from_secs(10)))
            .expect("stub load succeeds");
        assert_eq!(report.resolved_ctx, Some(32768));
    }

    #[test]
    fn relist_bound_takes_remaining_budget_capped_at_list_bound() {
        // The pure half of the wedged-re-list fix (#1276): both terms of the
        // min are pinned without a clock. The generous list bound binds
        // early in the load window; the REMAINING deadline budget binds late;
        // a fully-spent deadline yields an immediately-expiring re-list —
        // never an unbounded one.
        let deadline = Deadline(Duration::from_secs(600));
        let list_bound = Duration::from_secs(30);
        assert_eq!(
            relist_bound(deadline, Duration::from_secs(10), list_bound),
            Deadline(Duration::from_secs(30)),
            "early in the window the adapter list bound binds"
        );
        assert_eq!(
            relist_bound(deadline, Duration::from_secs(590), list_bound),
            Deadline(Duration::from_secs(10)),
            "late in the window the remaining deadline budget binds"
        );
        assert_eq!(
            relist_bound(deadline, Duration::from_secs(700), list_bound),
            Deadline(Duration::ZERO),
            "a spent deadline expires the re-list immediately, never unbounded"
        );
    }

    #[cfg(unix)]
    #[test]
    fn load_succeeds_with_unknown_ctx_when_relist_wedges() {
        // The runtime half of the wedged-re-list fix (#1276): the provenance
        // re-list previously ran via an UNBOUNDED `Command::output()` OUTSIDE
        // the deadline — a wedged `lms ps` hung a SUCCESSFUL load() forever
        // despite the #1276 mechanics on the load child itself
        // (demonstrated). Now it runs under `relist_bound` (the pure table
        // test above pins which term binds when), and its timeout degrades
        // to `resolved_ctx: None` — the load itself succeeded and is never
        // failed for provenance. Timing here is deliberately slack (a tight
        // load deadline flaked under parallel-suite CPU contention): the
        // list bound is the small term, the load deadline stays generous.
        let dir = tempfile::TempDir::new().unwrap();
        let stub = write_stub(
            dir.path(),
            "case \"$1\" in\n  ps) sleep 30 ;;\n  load) exit 0 ;;\nesac",
        );
        let mut host = LmsHost::with_bin(stub.to_string_lossy())
            .with_list_bound(Duration::from_millis(300));
        let started = Instant::now();
        let report = host
            .load("qwen/qwen3-4b-2507", "darkmux:qwen3-4b", 32768, Deadline(Duration::from_secs(30)))
            .expect("the load succeeded; only provenance degraded");
        assert_eq!(report.resolved_ctx, None, "re-list timeout → unknown ctx, not a failure");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the re-list was killed at its bound, not waited out ({:?})",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn list_resident_wedged_ps_returns_typed_timeout_at_adapter_bound() {
        // The read-only calls carry no port-level Deadline (packet-3 contract
        // question) but must still be bounded: a wedged `lms ps` returns a
        // typed Timeout at the adapter-level list bound instead of hanging
        // plan assembly (#1276).
        let dir = tempfile::TempDir::new().unwrap();
        let stub = write_stub(dir.path(), "sleep 30");
        let mut host =
            LmsHost::with_bin(stub.to_string_lossy()).with_list_bound(Duration::from_millis(200));
        let started = Instant::now();
        let err = host.list_resident().unwrap_err();
        assert!(matches!(err, HostError::Timeout { phase: "ps", .. }), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }

    #[cfg(unix)]
    #[test]
    fn list_catalog_wedged_ls_returns_typed_timeout_at_adapter_bound() {
        let dir = tempfile::TempDir::new().unwrap();
        let stub = write_stub(dir.path(), "sleep 30");
        let mut host =
            LmsHost::with_bin(stub.to_string_lossy()).with_list_bound(Duration::from_millis(200));
        let err = host.list_catalog().unwrap_err();
        assert!(matches!(err, HostError::Timeout { phase: "ls", .. }), "{err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn list_resident_preserves_host_order_and_raw_bytes() {
        // Host order is decision-bearing (first-match-wins residency +
        // deterministic eviction walk) — the adapter must not sort, even
        // when the host order is not lexicographic.
        let dir = tempfile::TempDir::new().unwrap();
        let stub = write_stub(
            dir.path(),
            r#"case "$1" in
  ps) echo '[
    {"identifier":"zeta","modelKey":"zeta-model","sizeBytes":300,"contextLength":3000},
    {"identifier":"darkmux:alpha","modelKey":"alpha-model","sizeBytes":100,"contextLength":1000},
    {"identifier":"mid","modelKey":"mid-model","contextLength":2000}
  ]' ;;
esac"#,
        );
        let mut host = LmsHost::with_bin(stub.to_string_lossy());
        let residents = host.list_resident().expect("stub ps succeeds");
        assert_eq!(
            residents,
            vec![
                ResidentFact {
                    identifier: "zeta".into(),
                    model_key: "zeta-model".into(),
                    ctx: 3000,
                    est_bytes: Some(300),
                },
                ResidentFact {
                    identifier: "darkmux:alpha".into(),
                    model_key: "alpha-model".into(),
                    ctx: 1000,
                    est_bytes: Some(100),
                },
                ResidentFact {
                    identifier: "mid".into(),
                    model_key: "mid-model".into(),
                    ctx: 2000,
                    est_bytes: None,
                },
            ],
            "host-reported order preserved; raw sizeBytes carried; missing bytes = None"
        );
    }

    #[cfg(unix)]
    #[test]
    fn list_catalog_parses_ls_json() {
        let dir = tempfile::TempDir::new().unwrap();
        let stub = write_stub(
            dir.path(),
            r#"case "$1" in
  ls) echo '[
    {"modelKey":"qwen/qwen3-4b-2507","sizeBytes":2500000000,"type":"llm"},
    {"modelKey":"mlx-community/qwen3.6-35b","type":"llm"},
    {"displayName":"keyless row is skipped"}
  ]' ;;
esac"#,
        );
        let mut host = LmsHost::with_bin(stub.to_string_lossy());
        let catalog = host.list_catalog().expect("stub ls succeeds");
        assert_eq!(
            catalog,
            vec![
                CatalogFact { model_key: "qwen/qwen3-4b-2507".into(), size_bytes: Some(2_500_000_000) },
                CatalogFact { model_key: "mlx-community/qwen3.6-35b".into(), size_bytes: None },
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn list_resident_command_failure_is_typed() {
        let dir = tempfile::TempDir::new().unwrap();
        let stub = write_stub(dir.path(), "echo 'lms exploded' >&2\nexit 1");
        let mut host = LmsHost::with_bin(stub.to_string_lossy());
        let err = host.list_resident().unwrap_err();
        match err {
            HostError::CommandFailed { detail } => assert!(detail.contains("lms exploded"), "{detail}"),
            other => panic!("expected CommandFailed, got {other:?}"),
        }
    }

    #[serial_test::serial]
    #[test]
    fn new_honors_env_bin() {
        // `LmsHost::new()` rides the existing `env(DARKMUX_LMS_BIN) >
        // config.lms_bin > "lms"` resolution (#661). Env mutation → serial,
        // mirroring the crate's other DARKMUX_LMS_BIN tests.
        let prev = std::env::var("DARKMUX_LMS_BIN").ok();
        unsafe { std::env::set_var("DARKMUX_LMS_BIN", "/custom/lms") };
        assert_eq!(LmsHost::new().bin, "/custom/lms");
        unsafe { std::env::remove_var("DARKMUX_LMS_BIN") };
        assert_eq!(LmsHost::new().bin, "lms");
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_LMS_BIN", v),
                None => std::env::remove_var("DARKMUX_LMS_BIN"),
            }
        }
    }
}
