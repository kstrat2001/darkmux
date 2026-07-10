//! [`LmsHost`] — the [`ModelHost`] port over the `lms` CLI (#1274 packet 2b).
//!
//! Sibling to (not a replacement for) `crate::lms`: the existing wrappers
//! keep serving `swap.rs` untouched until the packet-3 cutover. This adapter
//! differs from them in exactly the ways the gestalt ports require:
//!
//! - **Enforced deadline on every mutating call (#1276).** The current
//!   `lms::load_with_identifier` blocks indefinitely via `Command::status()`
//!   — a wrong model id hangs the dispatch until the workflow's outer kill.
//!   Here every `lms load`/`lms unload` child is spawned, polled with
//!   `try_wait`, and hard-killed at deadline expiry, returning a typed
//!   [`HostError::Timeout`] naming the phase.
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
}

impl Default for LmsHost {
    fn default() -> Self {
        Self::new()
    }
}

impl LmsHost {
    pub fn new() -> Self {
        Self { bin: lms_bin() }
    }

    pub fn with_bin(bin: impl Into<String>) -> Self {
        Self { bin: bin.into() }
    }
}

impl ModelHost for LmsHost {
    /// `lms ps --json` → resident facts, HOST ORDER PRESERVED — the order is
    /// decision-bearing (first-match-wins residency + deterministic budget
    /// eviction walk; see the `ResidentFact` docs). Adapters MUST NOT sort,
    /// and this one doesn't.
    fn list_resident(&mut self) -> Result<Vec<ResidentFact>, HostError> {
        let out = Command::new(&self.bin)
            .args(["ps", "--json"])
            .output()
            .map_err(|e| HostError::CommandFailed { detail: format!("spawning `lms ps --json`: {e}") })?;
        if !out.status.success() {
            return Err(HostError::CommandFailed {
                detail: format!(
                    "`lms ps --json` exited with {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            });
        }
        let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| {
            HostError::CommandFailed { detail: format!("`lms ps --json` output is not JSON: {e}") }
        })?;
        let Some(arr) = parsed.as_array() else {
            return Err(HostError::CommandFailed {
                detail: "`lms ps --json` output is not a JSON array".to_string(),
            });
        };
        Ok(arr.iter().map(resident_fact_from_json).collect())
    }

    /// `lms ls --json` → catalog facts (the #1276 existence fast-fail input +
    /// the estimator's base term). A failure is a typed error here; the
    /// LENIENCY (catalog unavailable ⇒ `Facts.catalog = None`, fast-fail
    /// skipped not failed) belongs to the facts-assembling caller, per the
    /// `Facts::catalog` contract.
    fn list_catalog(&mut self) -> Result<Vec<CatalogFact>, HostError> {
        let out = Command::new(&self.bin)
            .args(["ls", "--json"])
            .output()
            .map_err(|e| HostError::CommandFailed { detail: format!("spawning `lms ls --json`: {e}") })?;
        if !out.status.success() {
            return Err(HostError::CommandFailed {
                detail: format!(
                    "`lms ls --json` exited with {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            });
        }
        let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| {
            HostError::CommandFailed { detail: format!("`lms ls --json` output is not JSON: {e}") }
        })?;
        let Some(arr) = parsed.as_array() else {
            return Err(HostError::CommandFailed {
                detail: "`lms ls --json` output is not a JSON array".to_string(),
            });
        };
        Ok(arr.iter().filter_map(catalog_fact_from_json).collect())
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
        let run = run_bounded(cmd, "load", deadline)?;
        if !run.status.success() {
            return Err(classify_load_failure(&run.stderr, model_key, run.exit_detail()));
        }
        // Best-effort post-load provenance (the #1257 interim): re-list
        // residents and report the ctx the host actually resolved. A re-list
        // failure degrades to an empty report, never fails the load.
        let resolved_ctx = self
            .list_resident()
            .ok()
            .and_then(|residents| {
                residents.iter().find(|r| r.identifier == identifier).map(|r| r.ctx)
            })
            .filter(|ctx| *ctx > 0);
        Ok(LoadReport { resolved_ctx, ..Default::default() })
    }

    /// `lms unload <identifier>` under the same deadline mechanics. Only a
    /// claim-checked [`OwnedTarget`] can reach this call — the namespace
    /// contract is structural at the port seam. A not-resident stderr maps to
    /// the typed [`HostError::NotResident`] (the #1279 double-release shape).
    fn unload(&mut self, target: &OwnedTarget, deadline: Deadline) -> Result<(), HostError> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(["unload", target.identifier()]);
        let run = run_bounded(cmd, "unload", deadline)?;
        if !run.status.success() {
            return Err(classify_unload_failure(&run.stderr, target.identifier(), run.exit_detail()));
        }
        Ok(())
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

// ── Deadline mechanics (#1276) ──

/// Outcome of a bounded child run that finished before the deadline.
struct BoundedRun {
    status: std::process::ExitStatus,
    stderr: String,
}

impl BoundedRun {
    /// The generic failure detail: exit status + trimmed stderr.
    fn exit_detail(&self) -> String {
        format!("exited with {}: {}", self.status, self.stderr.trim())
    }
}

/// How long, after the child has EXITED, to keep collecting stderr chunks
/// from the drain thread. The exited child's writes are already complete
/// (either read by the drain thread or sitting in the OS pipe buffer, which
/// the drain thread reads promptly), so one quiet interval means everything
/// arrived. Bounded so an inherited pipe held open by a grandchild can never
/// stall the success path (see [`run_bounded`]).
const STDERR_GRACE: Duration = Duration::from_millis(200);

/// Spawn `cmd` and wait for it under `deadline` — the #1276 fix as a
/// mechanism: `spawn` + `try_wait` polling (std-only; no wait-timeout crate),
/// and on expiry `kill()` + `wait()` (reaped, no zombie) + a typed
/// [`HostError::Timeout`] naming the phase.
///
/// stdin is nulled (a host call is never interactive), stdout is nulled
/// (#1135 envelope safety), stderr is piped and drained on a dedicated
/// thread — draining concurrently is load-bearing: a chatty child that fills
/// the OS pipe buffer would otherwise block forever and read as a timeout.
///
/// The drain thread is NEVER joined, and stderr chunks flow back over a
/// channel instead of the thread's return value. This is load-bearing, found
/// by the deadline test itself: killing the direct child does not close the
/// stderr pipe when a grandchild inherited its write end (the shell-wrapping
/// stub's `sleep`; any tool that leaves a background process behind), so a
/// join would block until the GRANDCHILD exits — the exact #1276 hang this
/// function exists to remove, re-entering through the drain. The detached
/// thread ends on its own when the pipe finally closes; collection after
/// child exit is bounded by [`STDERR_GRACE`].
fn run_bounded(mut cmd: Command, phase: &'static str, deadline: Deadline) -> Result<BoundedRun, HostError> {
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| HostError::CommandFailed { detail: format!("spawning `lms {phase}`: {e}") })?;
    let stderr_pipe = child.stderr.take();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    // Detached by design — see the function docs. Dropping the JoinHandle
    // detaches; the thread ends when the pipe closes.
    std::thread::spawn(move || {
        use std::io::Read;
        let Some(mut pipe) = stderr_pipe else { return };
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
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Collect stderr with a bounded quiet-interval wait: in the
                // normal case the drain thread hits EOF and disconnects the
                // channel immediately after the last chunk; in the held-open
                // pipe case we pay at most one STDERR_GRACE.
                let mut stderr = String::new();
                while let Ok(chunk) = rx.recv_timeout(STDERR_GRACE) {
                    stderr.push_str(&chunk);
                }
                return Ok(BoundedRun { status, stderr });
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
    const INSUFFICIENT: &[&str] =
        &["insufficient", "not enough memory", "out of memory", "requires more memory"];
    if UNKNOWN_MODEL.iter().any(|p| lower.contains(p)) {
        return HostError::UnknownModel { model_key: model_key.to_string() };
    }
    if INSUFFICIENT.iter().any(|p| lower.contains(p)) {
        return HostError::InsufficientResources { detail: stderr.trim().to_string() };
    }
    HostError::CommandFailed { detail: format!("`lms load` {detail}") }
}

/// Classify a failed `lms unload`'s stderr. "Not found"-shaped wordings map
/// to [`HostError::NotResident`] — for an unload the only referent is the
/// identifier, so the broader match is safe here (unlike load, where "not
/// found" could name other things).
fn classify_unload_failure(stderr: &str, identifier: &str, detail: String) -> HostError {
    let lower = stderr.to_ascii_lowercase();
    const NOT_RESIDENT: &[&str] = &["no model", "not loaded", "not found", "no such model"];
    if NOT_RESIDENT.iter().any(|p| lower.contains(p)) {
        return HostError::NotResident { identifier: identifier.to_string() };
    }
    HostError::CommandFailed { detail: format!("`lms unload` {detail}") }
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
    fn load_stderr_insufficient_resources_shapes() {
        for stderr in [
            "Error: Insufficient system resources to load this model",
            "error: not enough memory to load model at this context length",
        ] {
            let err = classify_load_failure(stderr, "m", "exited with 1".into());
            assert!(
                matches!(err, HostError::InsufficientResources { ref detail } if detail.contains(stderr.trim())),
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

    #[test]
    fn unload_stderr_not_resident_shapes() {
        for stderr in [
            "Error: No model found with identifier \"darkmux:m\"",
            "error: darkmux:m is not loaded",
            "Model \"darkmux:m\" not found",
        ] {
            assert_eq!(
                classify_unload_failure(stderr, "darkmux:m", "exited with 1".into()),
                HostError::NotResident { identifier: "darkmux:m".into() },
                "stderr: {stderr}"
            );
        }
    }

    #[test]
    fn unload_stderr_unmatched_falls_through_to_command_failed() {
        let err = classify_unload_failure("kaboom", "darkmux:m", "exited with exit status: 1: kaboom".into());
        assert!(matches!(err, HostError::CommandFailed { .. }), "{err:?}");
    }

    // ── deadline enforcement against a stub lms binary (#1276) ──
    //
    // The stub is pointed at via `LmsHost::with_bin` (no env mutation, no
    // `#[serial]` needed, no race with the crate's other DARKMUX_LMS_BIN
    // tests). The env resolution itself is covered by `new_honors_env_bin`.

    /// Write an executable shell stub standing in for `lms` (the
    /// `write_stub_lms` pattern from `darkmux-lab`'s funnel tests), with a
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
    fn unload_not_resident_stderr_maps_through_real_spawn_path() {
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
        // The drain-thread subtlety: the child exits 1 immediately, but a
        // backgrounded grandchild inherited its stderr write end and lives
        // on for 30 s — the pipe never hits EOF. The call must still return
        // within the bounded STDERR_GRACE with the stderr the child DID
        // write (classified), not block on pipe close.
        let dir = tempfile::TempDir::new().unwrap();
        let stub = write_stub(
            dir.path(),
            "sleep 30 &\necho 'Error: No model found with identifier \"darkmux:m\"' >&2\nexit 1",
        );
        let mut host = LmsHost::with_bin(stub.to_string_lossy());
        let started = Instant::now();
        let err = host.unload(&owned("darkmux:m"), Deadline(Duration::from_secs(10))).unwrap_err();
        assert_eq!(err, HostError::NotResident { identifier: "darkmux:m".into() });
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "returned within the stderr grace, not on grandchild exit ({:?})",
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
