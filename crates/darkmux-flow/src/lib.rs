//! Flow observability — structured JSONL records for darkmux run tracking.
//!
//! # Storage model
//!
//! Records are appended to a per-day JSONL file (`YYYY-MM-DD.jsonl`) under
//! `~/.darkmux/flows/` (overridable via `DARKMUX_FLOWS_DIR`). The first write
//! atomically prepends a schema header so partial-file recovery is possible.

pub mod daemon_probe;
pub(crate) mod hmac_sha256;
pub mod hook_transform;
pub mod hooks;
pub mod presence;
pub mod presence_reconciler;
pub mod session_presence;

mod bookend;
mod integrity;
mod schema;
mod status;

pub use bookend::*;
pub use integrity::*;
pub use schema::*;
pub use status::*;

use crate::integrity::{audit_record_at, schema_header_line};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

// ─── FlowSink abstraction (#162 Phase 1) ─────────────────────────────────
//
// `FlowSink` is the trait every flow record is written through. The current
// (and default) implementation is `LocalFileSink` — preserves the existing
// per-day JSONL behavior. Future implementations (Phase 3+) include
// `RedisSink` (XADD to a Redis Stream for fleet coordination) and `TeeSink`
// (write to multiple sinks during migration). See [#162] for the full arc.
//
// Per-process default sink: `default_sink()` returns the singleton sink the
// public `record()` dispatches through. Tests can override via
// `set_default_sink_for_tests`.

/// Structured snapshot of a sink's identity + config for diagnostics
/// (`darkmux flow status`, `darkmux doctor` flow-sink-health). The
/// tree mirrors the sink composition: a TeeSink reports its `children`,
/// leaf sinks report empty `children`.
///
/// `config` is intentionally a flat key→string map (not a typed enum
/// per sink) so a new sink kind can be added without touching every
/// downstream consumer — the human formatter prints whatever's in
/// `config`; the JSON serializer is a pass-through.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SinkInfo {
    pub kind: String,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub config: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<SinkInfo>,
    /// Credential-bearing identifier that must round-trip through the
    /// in-process probe path (e.g., `RedisSink` URL) without ever
    /// leaving the process. Never serialized — `config` carries the
    /// redacted display form for any external surface (CLI JSON, daemon
    /// HTTP endpoint). See `find_redis_cfg` for the consumer side. (#216)
    #[serde(skip)]
    pub raw_url: Option<String>,
}

/// Abstraction over the destination of a flow record. Implementations
/// own the persistence semantics for their backend (file append, network
/// publish, etc.). All implementations must be `Send + Sync` because the
/// default sink is a process-wide singleton accessed from multiple
/// dispatch paths.
pub trait FlowSink: Send + Sync {
    /// Write a single record. Returns `Err` on persistence failure; the
    /// caller decides whether to bail or proceed (most current callers
    /// use `let _ = flow::record(...)` because audit-log writes are
    /// best-effort, but the trait signature is fallible for callers
    /// that DO want to react to write failures — e.g., a fleet
    /// coordinator might want to fall back to a local-file sink on
    /// network failure).
    fn write(&self, record: &FlowRecord) -> Result<()>;

    /// Introspection for diagnostics. Required so `darkmux flow status`
    /// and the doctor's `flow-sink-health` check can describe the active
    /// sink graph without per-sink-type knowledge.
    fn info(&self) -> SinkInfo;
}

/// File-based flow sink: appends to per-day JSONL files under
/// `~/.darkmux/flows/YYYY-MM-DD.jsonl`. The implementation darkmux has
/// shipped since v1.0 of the flow schema; preserved verbatim under
/// the trait abstraction.
///
/// Resolves the flows directory via `flows_dir()` at write time, NOT at
/// construction — so tests + operators that override `DARKMUX_FLOWS_DIR`
/// don't need to rebuild the sink to pick up the change. Symmetric with
/// how `record_at()` behaves today; refactor preserves the contract.
pub struct LocalFileSink;

impl LocalFileSink {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LocalFileSink {
    fn default() -> Self {
        Self::new()
    }
}

/// The directory `LocalFileSink` resolves per write.
///
/// Production builds: exactly `flows_dir()` (env > config > default) —
/// byte-identical behavior, this helper compiles down to that one call.
///
/// Test / `test-support` builds (#1355 review round): a test binary that
/// never sets `DARKMUX_FLOWS_DIR` must NOT write into the operator's real
/// `~/.darkmux/flows` day files through the process-global default sink.
/// Measured before this gate existed: one `cargo test -p darkmux-lab` run
/// leaked ~302 real flow records (205 `step result` work records with
/// session_id "case-1" + 97 `telemetry.tokens` records) into the live
/// fleet dashboard's day file — polluting the token odometer with mock
/// dispatches whose `reply()` fixture bills 10 tokens each. This is the
/// 4th+ recurrence of the leak class; per the structural-over-procedural
/// doctrine the fix is by-construction here, NOT another per-test env
/// guard (the OnceLock-pinned default sink makes env-guard ordering
/// load-bearing and therefore fragile).
///
/// Resolution in test builds: a LIVE `DARKMUX_FLOWS_DIR` still wins,
/// per write — the documented LocalFileSink contract ~9 downstream tests
/// (binary `phase_cli`/`flow_cli`, crew) rely on via their FlowsDirGuard
/// pattern. Only the FALLBACK tier changes: instead of the operator's
/// real flows dir, a per-process temp dir
/// (`$TMPDIR/darkmux-flow-test-<pid>`), created once per test binary.
fn local_sink_dir() -> PathBuf {
    #[cfg(any(test, feature = "test-support"))]
    {
        if std::env::var_os("DARKMUX_FLOWS_DIR").is_none() {
            static DIR: OnceLock<PathBuf> = OnceLock::new();
            return DIR
                .get_or_init(|| {
                    let dir = std::env::temp_dir()
                        .join(format!("darkmux-flow-test-{}", std::process::id()));
                    let _ = std::fs::create_dir_all(&dir);
                    dir
                })
                .clone();
        }
    }
    flows_dir()
}

impl FlowSink for LocalFileSink {
    // NOTE (#507): LocalFileSink still resolves its directory per write
    // (via `local_sink_dir()` — `flows_dir()` in production builds),
    // unlike AuditFileSink (which captures its dir at construction below).
    // Capturing here too is the right end-state, but it changes the
    // default sink's "honor a live DARKMUX_FLOWS_DIR" behavior that ~9
    // tests across the binary + crew rely on — converting those to
    // explicit sinks is its own scoped task, tracked as a #507 follow-up.
    // The per-record append is a single `write_all` under `O_APPEND` (see
    // `record_at`): O_APPEND's atomic EOF-positioning plus per-inode write
    // serialization on Linux/macOS keep concurrent writers to a shared day-file
    // from tearing each other's lines — the best-effort, lock-free counterpart
    // to AuditFileSink's tear-proof `flock`.
    fn write(&self, record: &FlowRecord) -> Result<()> {
        let dir = local_sink_dir();
        let day = day_utc_now();
        let path = dir.join(format!("{day}.jsonl"));
        record_at(record, &path)
    }

    fn info(&self) -> SinkInfo {
        let mut config = std::collections::BTreeMap::new();
        config.insert("flows_dir".to_string(), local_sink_dir().display().to_string());
        SinkInfo { kind: "LocalFile".to_string(), config, children: vec![], raw_url: None }
    }
}

// ─── AuditFileSink (#163) ────────────────────────────────────────────
//
// Detection-substrate sibling of LocalFileSink. Same per-day JSONL append
// format, plus:
//   - BLAKE3 hash chain — each record carries the prior record's hash,
//     making any after-the-fact edit detectable via a linear walk.
//   - Cross-process flock — concurrent CLI sessions writing the same
//     day file serialize through `flock(2)` so the hash chain can't
//     interleave (which would surface as a chain break the operator
//     might mistake for tampering).
//   - Separate directory (default `~/.darkmux/audit/`, overridable via
//     `DARKMUX_AUDIT_DIR`) — keeps casual flow records visually
//     distinct from audited records and lets the operator
//     mount the audit dir on different storage (encrypted volume,
//     read-only mirror, etc.).
//
// **POSIX-only** (`#[cfg(unix)]`) — `flock(2)` is the locking primitive.
// On Windows builds, AuditFileSink doesn't exist and `build_default_sink`
// silently skips it; the integrity-check verb + doctor check report
// "audit sink is unix-only on this platform". Cross-platform support
// would need `LockFileEx` and a separate code path — out of scope here.
//
// Edit-detecting, NOT tamper-proof. OS-level append-only flags
// (`chflags uappend` / `chattr +a`) are a follow-up; this PR ships the
// chain layer. Operators who need stronger guarantees compose this with
// disk encryption + filesystem-level immutability for layered defense.

/// Resolve the audit directory from env override (`DARKMUX_AUDIT_DIR`)
/// or default (`<darkmux root>/audit/`). Symmetric with `flows_dir()` but
/// deliberately separate so audit and casual records never share a path.
pub fn audit_dir() -> PathBuf {
    // (#875) `env(DARKMUX_AUDIT_DIR) > config.audit.dir` via config_access, then
    // the built-in default — so a config-only operator's `audit.dir` is honored.
    darkmux_types::config_access::audit_dir_override().unwrap_or_else(audit_dir_default)
}

/// (#2359) Same bug class as `flows_dir_default` before its fix, one
/// directory over: this went straight to `dirs::home_dir()`, so a
/// `DARKMUX_HOME`-scoped launch with no `DARKMUX_AUDIT_DIR`/`audit.dir`
/// override would still (were `audit.enabled` on) write the hash-chained
/// audit trail into the operator's REAL `~/.darkmux/audit`. Derived from the
/// SAME root resolution every sibling darkmux directory resolves through —
/// `darkmux_types::paths::resolve(Auto)`, which honors `DARKMUX_HOME` and a
/// project-local `./.darkmux` before `~/.darkmux`.
#[cfg(not(any(test, feature = "test-support")))]
fn audit_dir_default() -> PathBuf {
    darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::Auto)
        .root
        .join("audit")
}

/// Test / `test-support` builds must never default onto the operator's real
/// `~/.darkmux/audit` — same isolation discipline as
/// `darkmux_types::config_access`'s sibling `*_dir_default` test-build
/// variants (findings/mods/lab/flows). A test that DID isolate itself (a
/// `DARKMUX_HOME` tempdir, or a project-local `./.darkmux`) is honored
/// verbatim, because a test that isolated itself means it.
#[cfg(any(test, feature = "test-support"))]
fn audit_dir_default() -> PathBuf {
    let resolved = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::Auto);
    let real_user_root = dirs::home_dir().map(|h| h.join(".darkmux"));
    if real_user_root.as_ref() == Some(&resolved.root) {
        return PathBuf::from("/tmp/darkmux-test-isolated/audit");
    }
    resolved.root.join("audit")
}

/// (#877) Count `audit.write_failed` breadcrumbs in TODAY's local flow file.
/// Each one is a record the AuditFileSink dropped: the hash chain is INCOMPLETE
/// for it, even though `flow integrity-check` still validates the surviving
/// chain as clean (the next record re-derives `prev_hash` from the file tail).
/// `darkmux doctor` surfaces this count so a dropped audit write is DETECTABLE
/// instead of vanishing into stderr. Best-effort: a missing/unreadable flow
/// file → 0 (nothing to report).
pub fn count_audit_write_failures_today() -> usize {
    let path = flows_dir().join(format!("{}.jsonl", day_utc_now()));
    let Ok(content) = std::fs::read_to_string(&path) else {
        return 0;
    };
    content
        .lines()
        .filter(|l| {
            serde_json::from_str::<serde_json::Value>(l)
                .ok()
                .and_then(|v| v.get("action").and_then(|a| a.as_str()).map(|s| s == "audit.write_failed"))
                .unwrap_or(false)
        })
        .count()
}

/// BLAKE3 hash-chained audit sink — edits are detected by `darkmux flow
/// integrity-check` walking the chain (which is un-anchored, so it detects
/// edits *absent a full re-chain*, #899 — not an OS append-only guarantee).
/// See module-level comment for the design rationale. POSIX-only.
#[cfg(unix)]
pub struct AuditFileSink {
    // #507 — captured once at construction (see LocalFileSink). Capturing
    // the audit dir up front is what makes the cross-process hash chain
    // robust against a mid-sequence `DARKMUX_AUDIT_DIR` change (the
    // `records_checked == 1` flake the #463 cycle-break worked around at
    // the isolate layer; this removes the underlying per-write re-read).
    dir: PathBuf,
}

#[cfg(unix)]
impl AuditFileSink {
    /// Capture the audit dir from the environment (`DARKMUX_AUDIT_DIR` →
    /// default) at construction time.
    pub fn new() -> Self {
        Self { dir: audit_dir() }
    }

    /// Construct against an explicit dir (tests / config-driven dispatch).
    pub fn with_dir(dir: PathBuf) -> Self {
        Self { dir }
    }
}

#[cfg(unix)]
impl Default for AuditFileSink {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(unix)]
impl FlowSink for AuditFileSink {
    fn write(&self, record: &FlowRecord) -> Result<()> {
        let day = day_utc_now();
        let path = self.dir.join(format!("{day}.jsonl"));
        audit_record_at(record, &path)
    }

    fn info(&self) -> SinkInfo {
        let mut config = std::collections::BTreeMap::new();
        config.insert("audit_dir".to_string(), self.dir.display().to_string());
        config.insert("hash".to_string(), "blake3".to_string());
        SinkInfo { kind: "AuditFile".to_string(), config, children: vec![], raw_url: None }
    }
}

// ─── RedisSink (#162 Phase 3) ────────────────────────────────────────
//
// Live-coordination sink: XADD to a Redis Stream. Coexists with
// LocalFileSink via TeeSink — Redis is the coordination substrate,
// files are the audit substrate (see #163 for the detection-substrate
// AuditFileSink and #162's refinement comment on the split).
//
// Opt-in via `DARKMUX_REDIS_URL` env var. When set, the default sink
// becomes `TeeSink([LocalFileSink, RedisSink])`. When unset, the
// default sink stays `LocalFileSink` alone — no Redis dep code runs.
// Stream name defaults to `darkmux:flow`; override via
// `DARKMUX_REDIS_STREAM`.

/// Opaque wrapper for a Redis URL that contains credentials (#229).
/// `Display` produces the redacted form (`user:***@host:port`); raw
/// bytes are only accessible via `expose_for_probe()`, making accidental
/// password leakage into logs or serialized JSON a compile-time error
/// rather than a convention.
///
/// The only call site for `redact_url_creds` in production code is the
/// `Display` implementation below — all other paths reach the redacted
/// form through `format!("{raw_url}")` or `.to_string()`.
#[derive(Clone)]
pub struct RawRedisUrl(String);

// Debug is hand-written (NOT derived) so `{:?}` can't leak the password — the
// derived tuple-struct Debug would print the raw inner string verbatim, making
// the type safe for `Display` but a footgun for `{:?}` (a future log line, an
// anyhow context, an `.expect`). Delegating to the redacting form makes the
// "compile-time, not convention" promise actually hold for both. (#661, audit)
impl std::fmt::Debug for RawRedisUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RawRedisUrl({})", redact_url_creds(&self.0))
    }
}

impl RawRedisUrl {
    pub fn new(url: String) -> Self {
        Self(url)
    }

    /// Return the raw (unredacted) URL for `redis::Client::open` calls.
    /// The verbose name makes accidental use visible in review.
    pub fn expose_for_probe(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RawRedisUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&redact_url_creds(&self.0))
    }
}

/// (#1311/#1276) Hard bound on every `security find-generic-password` read.
/// A healthy Keychain read is <100ms; the leading hypothesis from a private
/// production incident is a locked/hung login keychain that froze a dispatch
/// ~19 min BEFORE any flow record — and the Redis-password read below runs
/// during flow-sink init, exactly the phase that incident never got past. 15s
/// is generous for a good read yet fails fast on a wedge.
pub const KEYCHAIN_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Poll cadence for [`run_security_bounded`]'s `try_wait` loop.
const KEYCHAIN_READ_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// Outcome of a bounded Keychain read. Callers decide how to treat each: an
/// OPTIONAL integration (Redis, serve token) degrades a timeout to a loud
/// warning + off; a REQUIRED credential (endpoint auth) bails.
#[derive(Debug)]
pub enum KeychainRead {
    /// Success — the RAW stdout (utf8-lossy); the caller trims as it needs.
    Found(String),
    /// The item is absent (non-zero exit).
    Absent,
    /// The read exceeded the bound and the child was killed.
    TimedOut,
    /// `security` couldn't be spawned (non-macOS, or a missing binary).
    Unavailable,
}

/// Bounded `security find-generic-password -s <item> [-a <account>] -w` read
/// (#1311/#1276). Shared by flow's own OPTIONAL secrets (Redis password, serve
/// token) and by the crew endpoint-auth read — every `security` call in a
/// dispatch goes through this one bound.
pub fn read_keychain_bounded(
    item: &str,
    account: Option<&str>,
    timeout: std::time::Duration,
) -> KeychainRead {
    let mut cmd = std::process::Command::new("security");
    cmd.arg("find-generic-password");
    if let Some(user) = account {
        cmd.args(["-a", user]);
    }
    cmd.args(["-s", item, "-w"]);
    run_security_bounded(cmd, timeout)
}

/// Core bounded runner: `spawn` + `try_wait` polling (std only, no wait-timeout
/// crate), and on expiry `kill()` + `wait()` (reaped — no zombie). stdout
/// carries the SECRET — drained on a detached thread (never an unbounded
/// `Command::output()` that could deadlock on a full pipe buffer) and NEVER
/// logged. Takes an already-built `Command` so tests can stand in a `sh -c`
/// stub for the `security` binary.
/// (#1965) Decide what a bounded `security` read actually produced.
///
/// Split out of `run_security_bounded` so the decision is testable without
/// spawning a process, because the bug it fixes was invisible from the outside:
/// success was gated ONLY on the child's exit status, and the stdout drain's
/// error was discarded via `unwrap_or_default()`. A pipe read that failed
/// partway — EINTR, EIO, a read racing the child's exit — left a TRUNCATED
/// secret (or an empty one) that was returned as `Found` and trusted verbatim
/// by every caller: the Redis password and the serve bearer token both resolve
/// through here.
///
/// A truncated credential presents to its consumer as a WRONG credential, not a
/// short one, so the resulting auth failure points nowhere near the read. This
/// operator has already lost time to that exact shape once, through a different
/// mechanism (a Keychain token truncated by an interactive prompt, producing
/// silent 401s).
///
/// `drained` is `None` when the read failed OR the drain thread panicked. Those
/// are the same fact — no trustworthy secret — and both must yield
/// `Unavailable`, which callers already handle, rather than a `Found` nobody
/// can distinguish from a good read.
fn keychain_outcome(exit_success: bool, drained: Option<Vec<u8>>) -> KeychainRead {
    // A non-zero exit means the item is not there; the read outcome is moot.
    if !exit_success {
        return KeychainRead::Absent;
    }
    match drained {
        Some(bytes) => KeychainRead::Found(String::from_utf8_lossy(&bytes).into_owned()),
        None => KeychainRead::Unavailable,
    }
}

fn run_security_bounded(
    mut cmd: std::process::Command,
    timeout: std::time::Duration,
) -> KeychainRead {
    use std::io::Read;
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return KeychainRead::Unavailable,
    };
    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    // (#1965) The read's error is PROPAGATED, not discarded. This buffer is the
    // secret itself, and a partial read that reaches the caller as a complete
    // one is the worst outcome available here — see `keychain_outcome`.
    let out_handle = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        if let Some(mut p) = out_pipe {
            p.read_to_end(&mut buf)?;
        }
        Ok(buf)
    });
    // stderr captured (not inherited) so an error line can't leak to the
    // terminal; joined + dropped, never logged.
    let err_handle = std::thread::spawn(move || {
        if let Some(mut p) = err_pipe {
            let _ = p.read_to_end(&mut Vec::new());
        }
    });
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // A read error and a panicked drain thread collapse to the same
                // thing: we do not hold a trustworthy secret. Neither may
                // become `Found`.
                let drained = out_handle.join().ok().and_then(|r| r.ok());
                let _ = err_handle.join();
                return keychain_outcome(status.success(), drained);
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait(); // reap — the kill must not leave a zombie
                    return KeychainRead::TimedOut;
                }
                std::thread::sleep(KEYCHAIN_READ_POLL);
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return KeychainRead::Unavailable;
            }
        }
    }
}

/// (#1311) Shared read for flow's own OPTIONAL secrets (Redis password, serve
/// token). Emits the dependency-free `credential-read:<item>` liveness marker
/// BEFORE the read (so a hang shows the item as the last-alive phase — the
/// leading #563 freeze point is the Redis read right here in flow-sink init),
/// bounds the read, and records a `done` marker with the resolution tier +
/// outcome + elapsed. On timeout it emits a LOUD actionable warning and
/// degrades to `None`: the integration is optional, so a locked keychain must
/// not abort the dispatch — but it must never be SILENT. `-a $USER` matches the
/// Homebrew wrapper's item shape. NEVER logs the value.
#[cfg(target_os = "macos")]
fn read_optional_keychain_secret(item: &str) -> Option<String> {
    let user = std::env::var("USER").ok()?;
    darkmux_types::dispatch_liveness::liveness_case(
        &format!("credential-read:{item}"),
        "flow-sink-init",
    );
    let start = std::time::Instant::now();
    let outcome = read_keychain_bounded(item, Some(&user), KEYCHAIN_READ_TIMEOUT);
    let ms = start.elapsed().as_millis();
    let tag = match &outcome {
        KeychainRead::Found(_) => "found",
        KeychainRead::Absent => "absent",
        KeychainRead::TimedOut => "timeout",
        KeychainRead::Unavailable => "unavailable",
    };
    darkmux_types::dispatch_liveness::liveness_detail(
        &format!("credential-read:{item}"),
        "flow-sink-init",
        &format!("done tier=keychain outcome={tag} elapsed_ms={ms}"),
    );
    match outcome {
        KeychainRead::Found(v) => {
            let v = v.trim().to_string();
            if v.is_empty() {
                None
            } else {
                Some(v)
            }
        }
        KeychainRead::Absent | KeychainRead::Unavailable => None,
        KeychainRead::TimedOut => {
            eprintln!(
                "[darkmux] WARNING: Keychain read for `{item}` timed out after {}s \
                 ({ms}ms elapsed) — is the login keychain locked on this machine? Continuing \
                 WITHOUT it (this optional integration is disabled for this run). Unlock it \
                 (`security unlock-keychain`) or use the env override. (#1311)",
                KEYCHAIN_READ_TIMEOUT.as_secs()
            );
            None
        }
    }
}

/// Read the Redis password from the macOS Keychain — the SAME `darkmux-redis`
/// item the Homebrew wrapper already populates (`security add-generic-password
/// -a $USER -s darkmux-redis -w`). Bounded + liveness-bracketed via
/// [`read_optional_keychain_secret`] (#1311), `OnceLock`-cached (the password
/// doesn't change mid-process). `-w` writes ONLY the password to stdout, which
/// flows into `assemble_redis_url` and from there only into a `RawRedisUrl`
/// (redacted `Display`). Non-macOS → `None` — portable operators use the
/// full-URL env path (tier-1). (#661 Slice 5)
#[cfg(target_os = "macos")]
fn keychain_redis_password() -> Option<String> {
    static PW: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    PW.get_or_init(|| read_optional_keychain_secret("darkmux-redis")).clone()
}
#[cfg(not(target_os = "macos"))]
fn keychain_redis_password() -> Option<String> {
    None
}

/// Percent-encode the URL-structural characters (`@ : / # ? &` — the exact set
/// the Homebrew wrapper documents as forbidden) in a Redis password. Two
/// reasons, one security + one functional:
/// - **Security:** `redact_url_creds` masks only the userinfo before the FIRST
///   `@`, so an `@` *inside* the password would push its tail into the
///   (unredacted) host portion and leak on `Display`. Encoding `@`→`%40` keeps
///   the whole secret in the masked userinfo.
/// - **Functional:** the other structural chars would otherwise split the URL.
///
/// A contract-compliant password contains none of these → **no-op**. The `redis`
/// crate percent-decodes userinfo, so an encoded edge-case password still
/// authenticates. `%` is intentionally NOT encoded (it's not in the forbidden
/// set and isn't a leak vector — leaving it avoids changing today's handling of
/// a literal `%`). Inline, no dep — per the project's dep discipline.
fn encode_redis_password(pw: &str) -> String {
    let mut out = String::with_capacity(pw.len());
    for c in pw.chars() {
        match c {
            '@' => out.push_str("%40"),
            ':' => out.push_str("%3A"),
            '/' => out.push_str("%2F"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            '&' => out.push_str("%26"),
            _ => out.push(c),
        }
    }
    out
}

/// Assemble a Redis URL from the non-secret bits + an optional password. Pure +
/// testable. The password is `encode_redis_password`'d, then placed as `:<pw>@`
/// (empty user), which `redact_url_creds` masks to `:***@`.
fn assemble_redis_url(host: &str, port: u16, db: Option<u8>, password: Option<&str>) -> String {
    let auth = match password {
        Some(pw) => format!(":{}@", encode_redis_password(pw)),
        None => String::new(),
    };
    let db_suffix = db.map(|d| format!("/{d}")).unwrap_or_default();
    format!("redis://{auth}{host}:{port}{db_suffix}")
}

/// Resolve the Redis connection URL, 3-tier (#661 Slice 5):
///   1. `DARKMUX_REDIS_URL` env set → **verbatim** (full URL, password inline)
///      — the backward-compat path; existing setups + the brew wrapper stay
///      byte-for-byte unchanged.
///   2. else `config.redis.enabled` + a host → **assemble** from the
///      non-secret config bits + the Keychain password (password-less if the
///      Keychain item is absent — local/Tailnet-trusted Redis is common; don't
///      hard-fail).
///   3. else `None` (Redis off — today's default).
///
/// Always returns `RawRedisUrl`, so the password can only reach a log through
/// the redacting `Display`; `expose_for_probe()` (deliberately verbose, visible
/// in review) is the sole raw-bytes path, for `redis::Client::open`.
pub fn redis_url() -> Option<RawRedisUrl> {
    // Tier 1 — env URL verbatim (password inline; redacted on Display).
    if let Some(url) = std::env::var("DARKMUX_REDIS_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        return Some(RawRedisUrl::new(url));
    }
    // Tier 2 — config-assembled (enabled + host), password from the Keychain.
    if darkmux_types::config_access::redis_enabled() {
        if let Some(host) = darkmux_types::config_access::redis_host() {
            let url = assemble_redis_url(
                &host,
                darkmux_types::config_access::redis_port(),
                darkmux_types::config_access::redis_db(),
                keychain_redis_password().as_deref(),
            );
            return Some(RawRedisUrl::new(url));
        }
    }
    // Tier 3 — off.
    None
}

/// Whether a (non-empty) Redis password is present in the macOS Keychain (item
/// `darkmux-redis`). Boolean ONLY — never the password itself. Lets `darkmux
/// doctor` surface a config Redis that would otherwise connect password-less.
/// `false` on non-macOS. (#661 Slice 5)
pub fn redis_keychain_password_present() -> bool {
    keychain_redis_password().is_some()
}

// ── (#881) serve-daemon auth token ───────────────────────────────────────────
// The serve daemon's bearer token is a SECRET, so it follows the SAME doctrine
// as the Redis password above: the `DARKMUX_SERVE_TOKEN` env override or the
// macOS Keychain item `darkmux-serve-token`, NEVER plaintext config.json. It
// lives in THIS crate (not darkmux-serve, which consumes it) to reuse the
// redacting-wrapper + Keychain-shell-out + OnceLock scaffold rather than
// duplicate ~60 lines of secret handling — a mild layering smell (a flow crate
// owning a serve concern) accepted for KISS pre-1.0. (#881)

/// Opaque wrapper for the serve-daemon bearer token. Like `RawRedisUrl`, the raw
/// bytes are reachable only via the verbosely-named `expose_for_compare`;
/// `Debug`/`Display` redact, so the token can't leak into a log line, an anyhow
/// context, or serialized JSON — a compile-time property, not a convention.
#[derive(Clone)]
pub struct RawServeToken(String);

impl std::fmt::Debug for RawServeToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RawServeToken(***)")
    }
}
impl std::fmt::Display for RawServeToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}
impl RawServeToken {
    pub fn new(token: String) -> Self {
        Self(token)
    }
    /// The raw token bytes, for the constant-time-ish header comparison in the
    /// serve auth middleware. Verbose name so accidental use is visible in
    /// review (mirrors `RawRedisUrl::expose_for_probe`).
    pub fn expose_for_compare(&self) -> &str {
        &self.0
    }
}

/// Read the serve-daemon token from the macOS Keychain — item
/// `darkmux-serve-token` (`security add-generic-password -a $USER -s
/// darkmux-serve-token -w`). Bounded + liveness-bracketed via
/// [`read_optional_keychain_secret`] (#1311), `OnceLock`-cached, same
/// `-w`-writes-only-to-stdout discipline as `keychain_redis_password`.
/// Non-macOS → `None` (portable operators use the `DARKMUX_SERVE_TOKEN` env
/// path). (#881)
#[cfg(target_os = "macos")]
fn keychain_serve_token() -> Option<String> {
    static TOK: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    TOK.get_or_init(|| read_optional_keychain_secret("darkmux-serve-token")).clone()
}
#[cfg(not(target_os = "macos"))]
fn keychain_serve_token() -> Option<String> {
    None
}

/// Resolve the serve-daemon bearer token, mirroring `redis_url`'s tiering:
///   1. `env(DARKMUX_SERVE_TOKEN)` verbatim (trimmed, empty-filtered) — the
///      portable/non-macOS path, no config gate (its presence is the opt-in);
///   2. else config gate on (`runtime.daemon_auth_enabled`) + Keychain item
///      `darkmux-serve-token`;
///   3. else `None` (auth off — today's default).
///
/// Always a `RawServeToken`, so the secret reaches a comparison only via
/// `expose_for_compare`. **Auth is "active" iff this returns `Some`** — the
/// config flag alone never activates auth (a gate-on-but-no-token state would
/// otherwise 401 every request with no way to pass). (#881)
pub fn serve_token() -> Option<RawServeToken> {
    // Tier 1 — env token verbatim.
    if let Some(tok) = std::env::var("DARKMUX_SERVE_TOKEN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return Some(RawServeToken::new(tok));
    }
    // Tier 2 — config gate on + Keychain item present.
    if darkmux_types::config_access::serve_auth_config_enabled() {
        if let Some(tok) = keychain_serve_token() {
            return Some(RawServeToken::new(tok));
        }
    }
    // Tier 3 — off.
    None
}

/// Whether a serve-daemon bearer token is configured (env or gated-Keychain).
/// Boolean ONLY — never the token. Drives the refuse-to-bind gate, the auth
/// middleware toggle, the startup banner, and `darkmux doctor`. (#881)
pub fn serve_token_present() -> bool {
    serve_token().is_some()
}

// ── (#2135 option 2) hook delivery signing secret ───────────────────────
// Same doctrine as the Redis password / serve token above, with one twist:
// the Keychain ITEM NAME is per-rule config (`HookRule::signing_secret_
// keychain_item`), not a fixed constant, so there's no single `OnceLock`-
// cached item to read — each rule's secret is read once, at `resolve_rules`
// time (which itself runs rarely — once per `HookSink` construction).

/// Opaque wrapper for a hook rule's HMAC signing secret. Same redaction
/// discipline as `RawServeToken`: `Debug`/`Display` never print the raw
/// bytes, only `expose_for_hmac` does (a verbose name so accidental use is
/// visible in review).
#[derive(Clone)]
pub struct RawHookSecret(String);

impl std::fmt::Debug for RawHookSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RawHookSecret(***)")
    }
}
impl std::fmt::Display for RawHookSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}
impl RawHookSecret {
    pub fn new(secret: String) -> Self {
        Self(secret)
    }
    /// The raw secret bytes, for HMAC signing. Verbose name so accidental
    /// use is visible in review (mirrors `RawRedisUrl::expose_for_probe`).
    pub fn expose_for_hmac(&self) -> &str {
        &self.0
    }
}

#[cfg(target_os = "macos")]
fn keychain_hook_secret(item: &str) -> Option<String> {
    read_optional_keychain_secret(item)
}
#[cfg(not(target_os = "macos"))]
fn keychain_hook_secret(_item: &str) -> Option<String> {
    None
}

/// Resolve rule `rule_index`'s HMAC signing secret:
///   1. `env(DARKMUX_HOOK_SECRET_<rule_index>)` verbatim (trimmed,
///      empty-filtered) — the portable/non-macOS path, wins on every
///      platform when set (mirrors `serve_token`'s env-first tiering);
///   2. else, when `keychain_item` names one, the macOS Keychain item of
///      that name (non-macOS → `None`, same as the Redis/serve-token
///      Keychain reads);
///   3. else `None` — this rule's deliveries go out unsigned.
pub fn hook_signing_secret(rule_index: usize, keychain_item: Option<&str>) -> Option<RawHookSecret> {
    let env_key = format!("DARKMUX_HOOK_SECRET_{rule_index}");
    if let Some(tok) = std::env::var(&env_key).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        return Some(RawHookSecret::new(tok));
    }
    let item = keychain_item.filter(|s| !s.trim().is_empty())?;
    keychain_hook_secret(item).map(RawHookSecret::new)
}

// ── (#2183) hook rule extra header values ────────────────────────────────
// Same wrapper (`RawHookSecret`) as the signing secret above — the SHAPE
// (an opaque string that must stay redacted in every log/record/doctor
// row) is identical for a literal header value and a Keychain-resolved
// one, so a new newtype would only duplicate `RawHookSecret`'s Debug/
// Display redaction. `is_secret` travels ALONGSIDE the wrapper (not
// inside it) because a LITERAL header (`Content-Type: application/json`)
// is not actually a secret — callers use it to decide whether a
// diagnostic surface may show the value or must print `"<redacted>"`.

/// Resolve one `headers` map entry to `(is_secret, value)`. A `Literal`
/// value resolves unconditionally (`is_secret: false`). A `Keychain`
/// reference resolves via the SAME bounded Keychain read
/// `hook_signing_secret` uses (`is_secret: true`); an absent/unreadable
/// item resolves to `(true, None)` — the header is silently DROPPED at
/// delivery (never sent empty), same fail-closed shape as an unresolved
/// signing secret. Non-macOS Keychain references always resolve to
/// `(true, None)` (no Keychain integration off-platform).
pub fn resolve_hook_header_value(v: &darkmux_types::config::HeaderValue) -> (bool, Option<RawHookSecret>) {
    match v {
        darkmux_types::config::HeaderValue::Literal(s) => (false, Some(RawHookSecret::new(s.clone()))),
        darkmux_types::config::HeaderValue::Keychain { keychain_item } => {
            (true, keychain_hook_secret(keychain_item).map(RawHookSecret::new))
        }
    }
}

/// Redis Streams-backed flow sink. Each `write` XADDs the record's
/// JSON-serialized fields to a single stream. Multiple consumers can
/// `XREAD BLOCK` for live updates; consumer groups handle multi-reader
/// fan-out; `MAXLEN ~ N` caps the stream size at the operator's chosen
/// retention.
///
/// **By design ephemeral** — Redis Streams with MAXLEN drop old records.
/// NOT the audit substrate. Pair with a durable sink (LocalFileSink or
/// AuditFileSink) via TeeSink for any operator who needs both
/// coordination AND audit. See #163 + the #162 refinement comment.
pub struct RedisSink {
    client: redis::Client,
    /// URL the sink was constructed with — retained for diagnostics
    /// (`SinkInfo`, `darkmux flow status`). Stored as `RawRedisUrl` so
    /// `Display` automatically redacts the password; raw bytes only
    /// accessible via `expose_for_probe()`. (#229)
    url: RawRedisUrl,
    stream: String,
    /// Optional MAXLEN ~ N retention cap. None = unbounded (don't use
    /// in production; the stream grows without bound).
    max_len: Option<usize>,
    /// (#388) Consecutive write-failure counter. Reset to 0 on any
    /// successful write. When it reaches `REDIS_DISABLE_THRESHOLD` the
    /// sink disables itself for the rest of the process.
    consecutive_failures: AtomicU32,
    /// (#388) Once the failure counter trips the threshold, the sink is
    /// disabled: subsequent writes skip silently (no connection attempt,
    /// no per-write log spam). Spares single-machine operators who set
    /// `DARKMUX_REDIS_URL` "just in case" from a 500ms-timeout-plus-log
    /// on every `darkmux` invocation when the peer is offline.
    disabled: AtomicBool,
}

/// (#388) Consecutive write failures before a `RedisSink` disables
/// itself for the process. 3 strikes balances "tolerate a one-off blip"
/// against "stop spamming a 500ms timeout + log line per write when the
/// peer is genuinely offline."
const REDIS_DISABLE_THRESHOLD: u32 = 3;

/// Hard cap on the wall-clock spent connecting + handshaking to Redis
/// from any `RedisSink` or sink-diagnostic probe (#278). The OS default
/// TCP-connect + handshake budget is platform-dependent and on macOS
/// can wait ~75 seconds when the host is reachable at the IP layer but
/// silent at the TCP/Redis layer (the canonical "Tailscale peer just
/// dropped" failure mode). Without this cap, every flow-record write
/// blocks the caller for the full OS budget — multiplied across the
/// ~30 tests that touch the flow pipeline, it turned a 5-second
/// `cargo test` into the 51-minute debacle from 2026-05-22.
///
/// 500ms is generous for a healthy LAN/tailnet round-trip (typical
/// connect+handshake ≤ 50ms) and bounds the worst-case per-write
/// cost at a known ceiling. The cost of the bound is that operators
/// running Redis behind a slow VPN where 500ms isn't enough will see
/// flow-record writes fail; if that surfaces in practice we'll need
/// to make the cap operator-configurable.
pub const REDIS_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Wall-clock bound on a single Redis COMMAND's response (#1570).
///
/// [`REDIS_CONNECT_TIMEOUT`] bounds only the CONNECT phase. That leaves the
/// failure mode an operator actually hit on 2026-07-29: a peer whose TCP port
/// accepts (so `nc -z` succeeds and any reachability probe passes) but which
/// never answers the command. The read then blocks indefinitely, the HTTP
/// route's own 30s timeout fires first and returns 408, and
/// `aggregate_flow_records_for_date`'s local-file fallback — which is correct
/// and already written — is NEVER REACHED, because a hang is not an `Err`.
/// Measured: `GET /flow/<a date with no records>` took 30.00s and 408'd with
/// Redis enabled, 0.5ms with it disabled.
///
/// Applied with `set_read_timeout`/`set_write_timeout` at every SERVE-SIDE
/// call site that takes a connection, and — as of #2227, via
/// `bound_redis_response` — at EVERY call site in this crate that issues a
/// command after `open_redis_connection_bounded`:
///
/// - `RedisSink::try_write`'s `XADD` and `RedisSink::connect`'s handed-out
///   connection (`lib.rs`),
/// - `session_presence`'s `write_session_beat` / `read_live_sessions` and
///   `SessionEmitter::stop`'s `DEL` (`session_presence.rs`),
/// - `presence`'s `write_beat` / `read_live` (`presence.rs`),
/// - `presence_reconciler`'s `claim_edge` / `release_edge_claim`
///   (`presence_reconciler.rs`),
/// - `status`'s `probe_redis` — `XLEN` / `XRANGE` / `XREVRANGE` (`status.rs`).
///
/// Before #2227 every one of those rode an unbounded connection. The `XADD` is
/// the worst on its own terms (a hang is not an `Err`, so
/// `REDIS_DISABLE_THRESHOLD` never tripped and the sink never self-disabled),
/// but the LIFECYCLE bug is the teardown path: `darkmux-crew`'s
/// `dispatch_internal` calls `SessionEmitter::stop()` immediately before
/// emitting `dispatch.complete`, and `stop()` joins a beat thread blocked in
/// `write_session_beat`'s `SET` and then issues two more commands. Against an
/// accepts-but-never-answers peer that stranded the dispatch with a
/// `dispatch.start` and no terminal record at all (measured 89.42s against a
/// peer that eventually closed; unbounded against a genuinely silent one).
///
/// STILL UNBOUNDED: `darkmux-fleet`'s queue (`queue.rs` — `publish_job`,
/// `init_consumer_group`, `claim_job`, `ack_job`) opens plain
/// `get_connection()` and is not bounded at either phase. Deliberately left
/// out of #2227's scope: `claim_job` issues `XREADGROUP ... BLOCK <block_ms>`,
/// an INTENTIONALLY long-blocking read that a 1s socket deadline would break,
/// so bounding that queue needs a per-call-site decision rather than this
/// constant. `darkmux-fleet`'s `routing.rs` (`wait_for_completion`) was
/// unbounded too and is BOUNDED AGAINST A SILENT PEER as of #2243 — its `--wait`
/// timeout was checked only at the top of the loop, which an unbounded read
/// never returned to, so the operator's declared timeout could never fire.
///
/// That call site does NOT use this constant for its READS, and the reason is
/// worth knowing before applying this constant to another POLLING loop. A fixed
/// per-command deadline shorter than a healthy peer's latency makes every poll
/// time out, and redis-0.27.6's `Connection::read` answers a timed-out RESPONSE
/// read with `messages_to_skip += 1`, so the next read DISCARDS a
/// successfully-parsed reply. A loop that re-issues its command on timeout
/// creates and consumes that deficit at the same rate and never closes it —
/// measured, against a peer answering every `XREVRANGE` correctly and in order:
/// 109ms to `Ok` at 100ms latency, and NEVER at 1200ms latency against a 1000ms
/// deadline. So `wait_for_completion` derives each poll's read deadline from its
/// REMAINING WAIT BUDGET instead, and a read that hits it ends the wait. It
/// still consumes `bound_redis_response` (widened to `pub` for it) for the WRITE
/// bound.
///
/// "BOUNDED" HERE MEANS BOUNDED AGAINST SILENCE, NOT BOUNDED OUTRIGHT — and this
/// qualifier applies to every entry in the list above, not just that one.
/// `set_read_timeout` is `SO_RCVTIMEO`: a per-`recv` deadline that fires only on
/// ZERO BYTES for the duration, restarted by any byte that arrives. It does not
/// bound a peer that DRIBBLES. Measured: one byte every 400ms into a reply that
/// never terminates blocked 12s against a declared 2s wait.
///
/// Do not read this list as exhaustive, and do not read this constant as a
/// standing guarantee that the codebase is bounded: a new call site is
/// unbounded until someone applies the deadline to it. Two rounds of review
/// on #2227 each found sites a previous version of this paragraph asserted
/// were not there — grep for `get_connection` before trusting it.
/// This is load-bearing beyond latency: wrapping the call
/// in `tokio::time::timeout` is NOT sufficient on its own, because a timeout
/// does not cancel an in-flight `spawn_blocking` task — without a socket-level
/// deadline the blocking thread stays wedged forever and repeated requests
/// leak the blocking pool.
///
/// 1s rather than 500ms: a command's round-trip legitimately costs more than a
/// connect handshake (an `XREVRANGE COUNT 10000` over a tailnet moves real
/// bytes), and unlike the connect bound this one is not paid per-write on the
/// hot path — it is a ceiling on pathology, not a budget for healthy work.
pub const REDIS_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1000);

/// Test-only env scrubber (#278). Tests in `flow::tests` write flow
/// records via the default sink path which respects `DARKMUX_REDIS_URL`.
/// An operator running tests from their daily shell with that var pointing
/// at an unreachable peer (the Studio-offline scenario from 2026-05-21)
/// saw the test bin wall-clock balloon by 75s/record. This helper scrubs
/// it in any flow test that writes records via the default sink path;
/// idempotent and safe to call multiple times. Uses `OnceLock` so the
/// scrub fires exactly once per test-binary invocation. (Deliberately does
/// NOT touch `DARKMUX_AUDIT_DIR` — see the note in the body.)
// Gated on `any(test, feature = "test-support")` rather than `test` alone:
// since #463 split flow into its own crate, a plain `#[cfg(test)]` would only
// compile this for flow's *own* test build, leaving it invisible to downstream
// crates' tests (e.g. `flow_cli` tests in the binary). The `test-support`
// feature lets the binary opt in via a dev-dependency without compiling the
// helper into release builds.
#[cfg(any(test, feature = "test-support"))]
pub fn isolate_test_env_once() {
    static INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INIT.get_or_init(|| {
        // (#811) The CONFIG tier is already neutralized by construction in test /
        // test-support builds — `darkmux_types::config_access::config()` returns
        // an empty config there, so `redis.enabled: true` in the operator's real
        // config.json can't re-enable the Redis sink (which leaked test records
        // to the real `darkmux:flow` stream and flaked `redis_url()`'s "off"
        // assertions). This helper only has to scrub the ENV tier:
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            // NOTE: we intentionally do NOT scrub DARKMUX_AUDIT_DIR here.
            // This OnceLock fires lazily on the first default-sink dispatch,
            // which can land mid-flight while a `#[serial]` audit test has
            // legitimately set DARKMUX_AUDIT_DIR to its own tmp dir — wiping
            // it and routing that test's later records elsewhere (the
            // intermittent `records_checked == 1` failure on
            // audit_file_sink_recovers_chain_across_process_boundaries).
            // REDIS_URL is the load-bearing scrub: an unreachable peer costs
            // 75s/record (the 2026-05-21 Studio-offline scenario). AUDIT_DIR
            // is a local file path — it never causes that timeout, so leaving
            // it untouched costs nothing and removes the race. (#463)
        }
    });
}

/// Wall-clock-bounded wrapper around `redis::Client::get_connection_with_timeout`
/// (#278). The redis crate's own timeout-bearing API bounds the TCP
/// connect phase only — the post-connect handshake (HELLO / AUTH /
/// HELLO etc.) is unbounded. A peer that ACCEPTS the TCP connection
/// but never completes the handshake (e.g. a half-functional Redis,
/// a TCP listener that does nothing, certain VPN-flap states) can
/// wedge the caller indefinitely. This wrapper runs the full
/// connect-and-handshake in a background thread and bails at
/// `timeout * 2` wall-clock regardless of which phase is stuck —
/// same shape as the DNS-resolution wrapper in `fleet::parse_address`
/// (#265 Wave-E.10).
///
/// `timeout * 2` is the wall ceiling because the redis crate uses
/// the same `Duration` for the TCP connect; doubling gives the
/// handshake the same budget so a healthy peer with a 400ms RTT
/// completes inside the bound.
///
/// `pub` rather than `pub(crate)` since #2243: `darkmux-fleet`'s
/// `wait_for_completion` needs the same bounded connect, and reusing this beats
/// a second copy of the connect budget drifting out of sync with it.
pub fn open_redis_connection_bounded(
    client: &redis::Client,
    timeout: std::time::Duration,
) -> Result<redis::Connection> {
    let client_clone = client.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("redis-connect-bounded".to_string())
        .spawn(move || {
            let result = client_clone.get_connection_with_timeout(timeout);
            // Ignore send errors — receiver may have given up on
            // timeout. The background thread keeps running until the
            // underlying socket gives up (post-connect handshake
            // hangs are bounded by the OS TCP keepalive + the redis-
            // crate's handshake, which can be minutes on a peer that
            // accepts but never responds). The leak is per-wedge, not
            // unbounded growth — but operators with a long-running
            // daemon hitting a half-functional peer may accumulate
            // background threads over time. Acceptable for the personal-
            // scope target; revisit if it bites.
            let _ = tx.send(result);
        })
        .map_err(|e| anyhow::anyhow!("spawning redis-connect thread: {e}"))?;
    match rx.recv_timeout(timeout * 2) {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(e)) => Err(anyhow::anyhow!("redis connect failed: {e}")),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(anyhow::anyhow!(
            "redis connect exceeded {}ms wall-clock budget — peer may be \
             reachable at TCP but silent at Redis handshake",
            (timeout * 2).as_millis()
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(anyhow::anyhow!(
            "redis-connect background thread panicked or exited without sending result"
        )),
    }
}

/// (#2227) Apply the per-command response deadline to a freshly obtained
/// connection. Same shape (and same reason) as `bound_redis_response` on the
/// serve side: `open_redis_connection_bounded` /
/// `get_connection_with_timeout` bound only the CONNECT phase, so without
/// this a peer that accepts TCP and completes the handshake but never answers
/// the command leaves the caller blocked on the socket read forever.
///
/// On the WRITE path that is worse than on the read path. A hang is not an
/// `Err`, so `note_failure` never runs, `REDIS_DISABLE_THRESHOLD` never trips
/// and the sink can never self-disable — every subsequent flow-record write
/// wedges too. And `darkmux_flow::record` is called SYNCHRONOUSLY from the
/// dispatch thread, so a wedged `XADD` stops the trajectory-tailer loop that
/// applies the inactivity-deadline resets and the host watchdog SIGKILLs a
/// fully productive dispatch (`darkmux-crew`'s `dispatch_internal`). With the
/// deadline applied, the stall surfaces as a `RedisError` out of `query()`,
/// `try_write` returns `Err`, and the #388 disable accounting does its job.
///
/// Best-effort by design, matching the serve-side site: a connection kind
/// that does not support socket deadlines must not turn a working write into
/// a failure. The bound is a safety net, not a correctness input.
///
/// `pub` rather than `pub(crate)` since #2243: `darkmux-fleet`'s
/// `wait_for_completion` needs the same WRITE bound on its polling connection,
/// and reusing this beats re-deriving `set_read_timeout(REDIS_RESPONSE_TIMEOUT)`
/// in another crate. This is not the codebase's only copy — `darkmux-serve` has
/// a private twin over the same constant (`darkmux-serve/src/lib.rs`), left
/// alone here because folding it in is a refactor of that crate rather than part
/// of #2243's fix.
///
/// HAZARD, now that any crate can call this: it OVERWRITES the connection's read
/// deadline with a fixed 1s. Do NOT apply it to a connection that legitimately
/// blocks longer — `XREADGROUP ... BLOCK <block_ms>` in `darkmux-fleet`'s
/// `claim_job` is the live example, and a connection whose read deadline is
/// managed per-call (as `wait_for_completion` does, deriving it from the
/// remaining wait budget) must set its own AFTER this, not before.
pub fn bound_redis_response(conn: &redis::Connection) {
    let _ = conn.set_read_timeout(Some(REDIS_RESPONSE_TIMEOUT));
    let _ = conn.set_write_timeout(Some(REDIS_RESPONSE_TIMEOUT));
}

/// (#2227) How long the test-only silent peer below holds an accepted socket
/// open before dropping it.
///
/// Deliberately LONGER than the per-call wall-clock ceilings the #2227 tests
/// assert (~3s), and that is the whole point: when the peer CLOSES the socket
/// the pending read returns EOF, which bounds the call *for free*. A hold
/// shorter than the assertion ceiling would make every one of these tests
/// pass with the socket deadline removed — the same vacuity the round-1
/// silent-peer test shipped with. 5× `REDIS_RESPONSE_TIMEOUT` keeps the
/// mutation red-proof meaningful while still being 6× shorter than the 30s
/// sleeper round 1 used.
#[cfg(test)]
pub(crate) const SILENT_PEER_HOLD: std::time::Duration =
    std::time::Duration::from_millis(REDIS_RESPONSE_TIMEOUT.as_millis() as u64 * 5);

/// (#2227) Spawn a fake Redis peer that COMPLETES redis-rs's connection-setup
/// handshake and then answers nothing.
///
/// This is the only shape that reaches the COMMAND phase at all: a peer that
/// merely accepts TCP (the #278 test's shape) wedges at the handshake, so
/// every command-phase assertion written against it passes vacuously. The two
/// `+OK` replies satisfy the two ignored `CLIENT SETINFO` commands redis-rs
/// 0.27 pipelines in `connection_setup_pipeline`.
///
/// The acceptor is BOUNDED at `max_connections` and each socket is held only
/// [`SILENT_PEER_HOLD`], so no sleeper thread outlives its test by more than
/// that hold. Every darkmux-flow call site opens its own connection, so
/// `max_connections` is "how many Redis calls does this test make", with
/// slack.
///
/// HONEST LIMIT, since a previous version of this comment claimed otherwise:
/// the acceptor is bounded at `max_connections` but every shipped test
/// deliberately budgets MORE than it uses, so the budget is never spent, and
/// `incoming().take(n)` parks in `accept()` rather than returning early. The
/// listener therefore stays bound for the test binary's life — one parked
/// thread and one ephemeral port per peer, no CPU, released at process exit.
/// That is acceptable for a test helper; it is not the "never accumulates
/// listeners" this comment used to promise.
#[cfg(test)]
pub(crate) fn spawn_silent_redis_peer(max_connections: usize) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().take(max_connections) {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                use std::io::Write;
                // Two `+OK` replies satisfy the two ignored `CLIENT SETINFO`
                // commands redis-rs 0.27's `connection_setup_pipeline` sends.
                let _ = stream.write_all(b"+OK\r\n+OK\r\n");
                let _ = stream.flush();
                std::thread::sleep(SILENT_PEER_HOLD);
                drop(stream);
            });
        }
        // Budget spent: `listener` drops here and the port closes.
    });
    // NOT required for correctness: `TcpListener::bind` already puts the
    // socket in LISTEN with a backlog, so a connect succeeds before
    // `accept()` is ever called. Kept as a small settling margin only.
    std::thread::sleep(std::time::Duration::from_millis(50));
    port
}

/// (#2227) Anti-vacuity guard for silent-peer tests whose subject returns no
/// error value to inspect (`claim_edge` → `bool`, `SessionEmitter::stop` →
/// `()`), so they can only assert wall-clock.
///
/// A wall-clock-only assertion passes just as happily when the CONNECT fails —
/// which is exactly how the round-1 test degenerated into a duplicate of the
/// #278 connect test. This proves the peer at `port` reaches the COMMAND
/// phase: the connect must SUCCEED, and a command against it must then time
/// out. Costs one connection from the peer's budget.
#[cfg(test)]
pub(crate) fn assert_silent_peer_reaches_command_phase(port: u16) {
    let client = redis::Client::open(format!("redis://127.0.0.1:{port}").as_str())
        .expect("open client against the fake peer");
    let mut conn = open_redis_connection_bounded(&client, REDIS_CONNECT_TIMEOUT).expect(
        "the fake peer must COMPLETE redis-rs's connection-setup pipeline — if \
         the connect fails, every wall-clock assertion in the caller is passing \
         vacuously against the CONNECT phase (#278's territory) rather than the \
         command phase #2227 is about",
    );
    bound_redis_response(&conn);
    let res: redis::RedisResult<String> = redis::cmd("PING").query(&mut conn);
    assert!(
        res.is_err(),
        "the fake peer answered a command; it must go silent after the \
         handshake or the caller is not exercising a command-phase stall"
    );
}

impl RedisSink {
    /// Build a sink connecting to `url` and writing to `stream`. Connection
    /// is not established until the first `write` call (the redis client
    /// is lazy by design).
    pub fn new(url: &str, stream: &str, max_len: Option<usize>) -> Result<Self> {
        let url = RawRedisUrl::new(url.to_string());
        let client = redis::Client::open(url.expose_for_probe()).with_context(|| {
            format!("opening Redis connection to {url}")
        })?;
        Ok(Self {
            client,
            url,
            stream: stream.to_string(),
            max_len,
            consecutive_failures: AtomicU32::new(0),
            disabled: AtomicBool::new(false),
        })
    }

    /// (#388) Whether the sink has disabled itself after repeated
    /// failures. Disabled writes skip silently.
    pub fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::Acquire)
    }

    /// (#388) Account one write failure. Disables the sink (and logs a
    /// single one-time warning) when the consecutive-failure counter
    /// first reaches `REDIS_DISABLE_THRESHOLD`. Returns true iff this
    /// call is the one that flipped the sink to disabled.
    fn note_failure(&self, err: &anyhow::Error) -> bool {
        let n = self.consecutive_failures.fetch_add(1, Ordering::AcqRel) + 1;
        if n >= REDIS_DISABLE_THRESHOLD && !self.disabled.swap(true, Ordering::AcqRel) {
            eprintln!(
                "flow::RedisSink: {} unreachable after {n} consecutive write failures \
                 ({err:#}); disabling Redis flow sink for this process. \
                 LocalFileSink is unaffected; re-run after the peer is reachable to re-enable.",
                self.url
            );
            true
        } else {
            false
        }
    }

    /// (#388) Account one successful write — clears the failure streak so
    /// a transient blip never counts toward the disable threshold.
    fn note_success(&self) {
        // A single success clears the streak. The load-then-store isn't
        // one atomic op, but that's benign: a racing failure between the
        // load and the store at worst delays a disable by one write — it
        // can never cause a spurious disable, and a disabled sink never
        // reaches here (write() returns early when disabled). The
        // Acquire/Release pair orders the reset against note_failure's
        // fetch_add so the cleared counter is visible to the next writer.
        if self.consecutive_failures.load(Ordering::Acquire) != 0 {
            self.consecutive_failures.store(0, Ordering::Release);
        }
    }

    /// Connect + return a usable connection. Exposed for diagnostics
    /// (status probe, doctor health check) that need to talk to the
    /// same Redis the sink writes to. Bounded by `REDIS_CONNECT_TIMEOUT`
    /// (#278) so a peer that's silent at the TCP/Redis layer bails
    /// fast instead of wedging the caller for the OS default.
    pub fn connect(&self) -> Result<redis::Connection> {
        let conn = open_redis_connection_bounded(&self.client, REDIS_CONNECT_TIMEOUT)
            .with_context(|| format!("connecting to Redis at {}", self.url))?;
        // (#2227) Hand the caller a connection whose COMMANDS are bounded too.
        // This accessor exists to give diagnostics a connection to the same
        // Redis the sink writes to — and diagnostics are exactly what an
        // operator runs against a peer that has stopped answering. Bounding it
        // here means an out-of-crate caller can't reintroduce the unbounded
        // shape by forgetting; a caller that legitimately needs a longer
        // deadline (fleet's `XREADGROUP ... BLOCK`) sets its own afterward.
        bound_redis_response(&conn);
        Ok(conn)
    }

    pub fn url(&self) -> &str { self.url.expose_for_probe() }
    pub fn stream(&self) -> &str { &self.stream }
    pub fn max_len(&self) -> Option<usize> { self.max_len }
}

impl FlowSink for RedisSink {
    fn write(&self, record: &FlowRecord) -> Result<()> {
        // (#388) Once disabled, skip silently — no connection attempt
        // (so no 500ms timeout) and no log. Returning Ok keeps this
        // best-effort coordination sink from masking the durable
        // LocalFileSink's own result in the TeeSink.
        if self.is_disabled() {
            return Ok(());
        }
        match self.try_write(record) {
            Ok(()) => {
                self.note_success();
                Ok(())
            }
            Err(e) => {
                // Swallow: log a single one-time warning at the disable
                // threshold (note_failure), but never propagate to the
                // TeeSink — that's what produced the per-write spam this
                // fixes. Redis is the coordination substrate, not the
                // durable record.
                self.note_failure(&e);
                Ok(())
            }
        }
    }

    fn info(&self) -> SinkInfo {
        self.sink_info()
    }
}

impl RedisSink {
    /// The actual XADD write — fallible. `write` (the trait method) wraps
    /// this with the #388 disable accounting.
    fn try_write(&self, record: &FlowRecord) -> Result<()> {
        let mut conn = open_redis_connection_bounded(&self.client, REDIS_CONNECT_TIMEOUT)
            .context("getting Redis connection")?;
        // (#2227) The connect above is bounded; the XADD below was not. An
        // accepts-but-never-answers peer (measured 2026-07-29 on a Tailscale
        // peer) wedged this write indefinitely — and because a hang is not an
        // `Err`, the #388 disable accounting never advanced.
        bound_redis_response(&conn);
        let payload = serde_json::to_string(record)
            .context("serializing FlowRecord for Redis")?;
        // Two-field encoding: `schema` carries the version (so downstream
        // consumers across darkmux versions can handle skew explicitly),
        // `record` carries the JSON-serialized FlowRecord. Single XADD
        // call per write; small payload (~1 KB typical) so MAXLEN trim
        // can run synchronously without affecting latency.
        let fields: &[(&str, &str)] = &[
            ("schema", FLOW_SCHEMA_VERSION),
            ("record", &payload),
        ];
        // XADD <stream> [MAXLEN ~ N] * field value [field value ...]
        let mut cmd = redis::cmd("XADD");
        cmd.arg(&self.stream);
        if let Some(n) = self.max_len {
            cmd.arg("MAXLEN").arg("~").arg(n);
        }
        cmd.arg("*"); // auto-generated ID
        for (k, v) in fields {
            cmd.arg(*k).arg(*v);
        }
        let _: String = cmd
            .query(&mut conn)
            .with_context(|| format!("XADD to Redis stream `{}`", self.stream))?;
        Ok(())
    }

    /// `SinkInfo` for diagnostics — called by the `FlowSink::info` impl.
    fn sink_info(&self) -> SinkInfo {
        let mut config = std::collections::BTreeMap::new();
        // The displayed URL is redacted — `config` rides through to JSON
        // output (`darkmux flow status --json` + the daemon's HTTP
        // endpoint), and the password must not appear there. The raw URL
        // is preserved on `SinkInfo.raw_url` (skip-serialized) for the
        // in-process probe path in `find_redis_cfg`. (#216)
        config.insert("url".to_string(), self.url.to_string());
        config.insert("stream".to_string(), self.stream.clone());
        config.insert(
            "max_len".to_string(),
            self.max_len.map(|n| n.to_string()).unwrap_or_else(|| "unbounded".to_string()),
        );
        // (#2227) Surface the #388 self-disable. `sink_info` feeds
        // `darkmux flow status --json` and the daemon's endpoint, and until now
        // it reported url/stream/max_len only — so a sink that had permanently
        // disabled itself after REDIS_DISABLE_THRESHOLD failures read as
        // healthy everywhere an operator would look, with the one-time stderr
        // warning long since scrolled away. That state was hard to reach before
        // #2227 (a hang is not an `Err`, so the counter never advanced); this
        // fix is precisely what makes it reachable from the silent-peer
        // scenario, which makes it an operator-sovereignty gap now:
        // "the operator never has to wonder where a decision came from."
        config.insert("disabled".to_string(), self.is_disabled().to_string());
        SinkInfo {
            kind: "Redis".to_string(),
            config,
            children: vec![],
            raw_url: Some(self.url.expose_for_probe().to_string()),
        }
    }
}

// ─── TeeSink (#162 Phase 3) ───────────────────────────────────────────
//
// Compositional sink: writes each record to N child sinks. Errors from
// any single child are logged but don't fail the overall write — the
// audit substrate has to remain durable even when coordination layer
// is degraded. Per the operator-sovereignty contract: surface failures
// loudly via stderr; don't silently lose the audit record.

/// `SinkInfo.kind` of the AuditFileSink — the detection-substrate child whose
/// write failures must be made DETECTABLE rather than vanish into stderr (#877).
pub(crate) const AUDIT_SINK_KIND: &str = "AuditFile";
/// `SinkInfo.kind` of the LocalFileSink — the durable casual sink the
/// audit-failure breadcrumb is written into (a DIFFERENT path than the failing
/// audit dir, so it's likely still writable when the audit write failed).
pub(crate) const LOCAL_SINK_KIND: &str = "LocalFile";

pub(crate) struct TeeSink {
    sinks: Vec<Arc<dyn FlowSink>>,
}

impl TeeSink {
    pub fn new(sinks: Vec<Arc<dyn FlowSink>>) -> Self {
        Self { sinks }
    }

    /// (#877) When the AUDIT child's write fails, the record never reaches the
    /// hash-chained log — and because the next record re-derives `prev_hash`
    /// from the file tail, the chain still validates CLEAN, so the gap is
    /// invisible to `flow integrity-check`. Leave a durable breadcrumb in the
    /// LocalFile sink (a clone of the dropped record, retagged
    /// `audit.write_failed`) so `darkmux doctor` can surface that the audit log
    /// is INCOMPLETE for that record. Detection, not block-at-the-moment —
    /// matches the "detect, don't claim tamper-proof" framing. Best-effort:
    /// written straight to the local child (never re-tee'd → no recursion).
    fn emit_audit_failure_breadcrumb(&self, dropped: &FlowRecord, err_msg: &str) {
        let Some(local) = self.sinks.iter().find(|s| s.info().kind == LOCAL_SINK_KIND) else {
            // No local sink to record into — stderr already logged the failure.
            return;
        };
        let mut bc = dropped.clone();
        bc.level = crate::schema::Level::Error;
        bc.category = crate::schema::Category::Audit;
        bc.action = "audit.write_failed".to_string();
        // Never carry chain fields on the casual-sink breadcrumb.
        bc.prev_hash = None;
        bc.hash = None;
        bc.payload = Some(serde_json::json!({
            "dropped_action": dropped.action,
            "dropped_session_id": dropped.session_id,
            "error": err_msg,
        }));
        if let Err(e) = local.write(&bc) {
            eprintln!(
                "flow::TeeSink: audit-failure breadcrumb ALSO failed to write to the \
                 local sink: {e:#} (original audit-write failure is unrecorded durably)"
            );
        }
    }
}

impl FlowSink for TeeSink {
    fn write(&self, record: &FlowRecord) -> Result<()> {
        // Best-effort: record per-sink failures but always attempt every
        // sink. Return the first error (so callers can react if they
        // want); log the rest to stderr so the operator sees them.
        let mut first_err: Option<anyhow::Error> = None;
        let mut audit_err: Option<String> = None;
        for (i, sink) in self.sinks.iter().enumerate() {
            if let Err(e) = sink.write(record) {
                eprintln!(
                    "flow::TeeSink: sink #{i} write failed: {e:#}"
                );
                // (#877) An AUDIT-sink failure is a compliance gap — capture it
                // for the durable breadcrumb below.
                if sink.info().kind == AUDIT_SINK_KIND {
                    audit_err = Some(format!("{e:#}"));
                }
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        // (#877) Drop the durable breadcrumb AFTER attempting every sink, so the
        // record still reached the other (non-audit) sinks first.
        if let Some(err_msg) = audit_err {
            self.emit_audit_failure_breadcrumb(record, &err_msg);
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn info(&self) -> SinkInfo {
        SinkInfo {
            kind: "Tee".to_string(),
            config: std::collections::BTreeMap::new(),
            children: self.sinks.iter().map(|s| s.info()).collect(),
            raw_url: None,
        }
    }
}

// ─── Default-sink selection (#162 Phase 3) ────────────────────────────

/// Build the process-wide default sink from env-var configuration.
///
/// Composition rules (#162, #163):
/// - `DARKMUX_AUDIT_DIR` set (and non-empty) → AuditFileSink is included.
/// - `DARKMUX_REDIS_URL` set (and non-empty) → RedisSink is included.
/// - LocalFileSink is always present (casual write target).
///
/// The TeeSink wraps every enabled sink in order: `[Audit, LocalFile, Redis]`
/// — **audit first** reflects the compliance hierarchy. The casual file
/// sink is the operator-familiar one, but the audit sink is the
/// load-bearing substrate for regulated deployments. A future short-
/// circuit mode (e.g., fail-fast on audit failure) naturally fits this
/// ordering.
///
/// Each record is broadcast to every active sink; failures are logged
/// but don't block the others — every substrate remains durable even
/// when one layer is degraded.
///
/// `DARKMUX_REDIS_STREAM` overrides the stream name (default `darkmux:flow`).
/// `DARKMUX_REDIS_MAXLEN` overrides the retention cap (default 10000;
/// set to `0` for unbounded — not recommended).
///
/// Connection errors at construction degrade gracefully: if Redis is
/// unreachable when the sink builds, the warning logs to stderr and the
/// default sink continues without it. Operators see the connection
/// failure loudly; the audit + casual substrates stay intact.
fn build_default_sink() -> Arc<dyn FlowSink> {
    let mut sinks: Vec<Arc<dyn FlowSink>> = Vec::new();

    // (#875) `env(DARKMUX_AUDIT_DIR) presence > config.audit.enabled` via
    // config_access — so a config-only operator with `audit.enabled: true` gets
    // the AuditFileSink instead of it silently staying off.
    let audit_enabled = darkmux_types::config_access::audit_enabled();
    if audit_enabled {
        #[cfg(unix)]
        {
            let path = audit_dir().display().to_string();
            eprintln!("flow: AuditFileSink enabled — audit_dir={path} (hash-chained, flock-serialized)");
            sinks.push(Arc::new(AuditFileSink::new()));
        }
        #[cfg(not(unix))]
        {
            eprintln!(
                "flow: DARKMUX_AUDIT_DIR set, but AuditFileSink is POSIX-only — skipping on this platform. \
                 Casual + Redis sinks remain active."
            );
        }
    }

    // LocalFile is always present.
    sinks.push(Arc::new(LocalFileSink::new()));

    // (#661 Slice 5) Resolve via the 3-tier resolver: env DARKMUX_REDIS_URL
    // verbatim, else config-assembled (enabled + host + Keychain password),
    // else None.
    if let Some(raw_url) = redis_url() {
        // (#875) Resolve stream + maxlen through config_access (env > config >
        // default) so a config-only operator's `redis.stream`/`redis.maxlen`
        // aren't silently dropped. The `0 → None` (unbounded) translation stays
        // at this call site per the accessor's contract.
        let stream = darkmux_types::config_access::redis_stream();
        let max_len = match darkmux_types::config_access::redis_maxlen() {
            0 => None,
            n => Some(n),
        };

        match RedisSink::new(raw_url.expose_for_probe(), &stream, max_len) {
            Ok(redis_sink) => {
                // (#1955) The URL is deliberately NOT printed.
                //
                // `RawRedisUrl`'s redaction covers the PASSWORD (#213/#229)
                // and is correct for that job — but it passes host:port
                // through by design, so this banner printed the operator's
                // tailnet address on every dispatch, to stdout+stderr, on a
                // machine whose repo history was scrubbed of exactly those
                // addresses (#1940). Not a repo leak; a leak vector for the
                // sharing that actually happens — pasting dispatch output
                // into an issue, an article, or a screenshot.
                //
                // Where Redis lives is CONFIG, and `darkmux doctor` is the
                // documented place to show a resolved value with its
                // provenance. A banner confirming the sink came up needs the
                // stream name and nothing else.
                eprintln!(
                    "flow: Redis sink enabled — stream={stream} \
                     max_len={max_len:?} (composed via TeeSink)"
                );
                sinks.push(Arc::new(redis_sink));
            }
            Err(e) => {
                eprintln!(
                    "flow: Redis sink construction failed ({e:#}); continuing without it. \
                     Other sinks intact."
                );
            }
        }
    }

    // (#2093) Hooks — composed AFTER Redis, against the sinks accumulated
    // so far (never against itself). `hook.fired`/`hook.failed` records
    // land in that snapshot tee, so they reach the durable + coordination
    // substrate without any self-referential `Arc` at construction time —
    // the module's own `hook.*` loop guard is what makes even a literal
    // self-reference safe, but this ordering doesn't lean on that as the
    // only defense.
    if darkmux_types::config_access::hooks_enabled() {
        let rules = darkmux_types::config_access::hooks_rules();
        let outbox_dir = darkmux_types::config_access::hooks_outbox_dir();
        let report_sink: Arc<dyn FlowSink> = if sinks.len() == 1 {
            sinks[0].clone()
        } else {
            Arc::new(TeeSink::new(sinks.clone()))
        };
        match hooks::HookSink::new(&rules, outbox_dir, report_sink) {
            Ok(hook_sink) => {
                eprintln!(
                    "flow: Hooks sink enabled — {} rule(s), outbox={}",
                    rules.len(),
                    hook_sink.outbox_dir().display()
                );
                sinks.push(Arc::new(hook_sink));
            }
            Err(e) => {
                eprintln!(
                    "flow: Hooks sink construction failed ({e:#}); continuing without it. \
                     Other sinks intact."
                );
            }
        }
    }

    if sinks.len() == 1 {
        // Single sink — skip the Tee wrapper for clarity in diagnostics.
        sinks.into_iter().next().unwrap()
    } else {
        Arc::new(TeeSink::new(sinks))
    }
}

/// Process-wide default sink. Initialized lazily on first call to
/// `record()`; default selection reads env config at init time.
///
/// Test / `test-support` builds only: scrubs `DARKMUX_REDIS_URL`
/// once before the sink is built so the cached sink doesn't capture
/// the operator's daily-shell env. Critical because the OnceLock
/// freezes the sink shape — any test that runs `record()` BEFORE
/// other isolation runs would otherwise lock in a RedisSink pointing
/// at the operator's real (possibly-unreachable) Redis. (#278)
/// (#1355 review round) Widened from `cfg(test)` to
/// `any(test, feature = "test-support")` — a plain `cfg(test)` only fires
/// when darkmux-flow ITSELF is the test harness, so a downstream crate's
/// test build (e.g. darkmux-lab's, whose review tests dispatch through the
/// global `record()`) got NO scrub at all and could freeze a RedisSink
/// against the operator's live shell env into the singleton. Downstream
/// crates opt in via a dev-dependency on this crate's `test-support`
/// feature — same wiring the binary already uses.
fn default_sink() -> Arc<dyn FlowSink> {
    #[cfg(any(test, feature = "test-support"))]
    isolate_test_env_once();

    static SINK: OnceLock<Arc<dyn FlowSink>> = OnceLock::new();
    SINK.get_or_init(build_default_sink).clone()
}

/// Introspect the process-wide default sink for diagnostics. Stable
/// pointer to the same singleton `record()` writes through, so the
/// reported sink graph cannot drift from the actually-active one.
pub(crate) fn default_sink_info() -> SinkInfo {
    default_sink().info()
}

/// Write a record through an explicit sink. Used by tests + future
/// config-driven dispatch paths where the caller picks the sink. The
/// production code path uses `record()` which dispatches through the
/// process-wide default sink.
pub fn record_via(sink: &dyn FlowSink, record: &FlowRecord) -> Result<()> {
    sink.write(record)
}

/// Append `record` to today's per-day JSONL file. Creates the file with a
/// schema header as line 1 if it doesn't exist (written atomically with the
/// first record so a partial file never ends up header-only).
///
/// Concurrent writes: append-on-Unix is atomic up to PIPE_BUF (~4 KB on
/// macOS). Single-line JSONL records are well under this limit, so no
/// explicit locking is needed.
///
/// **Phase 1 refactor (#162):** this function now dispatches through
/// `FlowSink`. The default sink is `LocalFileSink`, which preserves
/// the original behavior. No callers should see a behavior change.
///
/// **Schema 1.4 refactor (#167):** `machine_id` is auto-populated here if
/// the caller left it `None`. Callers that pre-set the field (e.g., a
/// remote ingest path forwarding records from another machine) win —
/// auto-populate fills it only when absent.
pub fn record(record: FlowRecord) -> Result<()> {
    record_to(default_sink().as_ref(), record)
}

/// Stamp provenance (machine_id when the caller left it `None`) and write
/// to an explicit sink. `record()` is `record_to(default_sink(), …)`.
/// Split out (#507) so callers — and tests — can target a sink built
/// against an explicit dir instead of depending on the process-global
/// default sink + live env. The provenance auto-populate is identical to
/// the pre-split `record()`.
pub(crate) fn record_to(sink: &dyn FlowSink, record: FlowRecord) -> Result<()> {
    let mut rec = record;
    if rec.machine_id.is_none() {
        rec.machine_id = resolve_machine_id();
    }
    if rec.machine_uid.is_none() {
        // (#640) Stamp the stable hardware identity at write time, like the
        // machine_id label above. Cached, so the ioreg shell-out runs once.
        rec.machine_uid = darkmux_hardware::machine_uid().map(str::to_string);
    }
    sink.write(&rec)
}

/// Internal entry point writing to an explicit path. Used by tests and the
/// public `record()` wrapper delegates here after resolving the path.
///
/// Atomic-first-write semantics: when the file doesn't exist yet, the
/// schema header AND the first record are written in a single `write_all`
/// call against an exclusively-created handle (`create_new(true)`). This
/// closes two race classes the prior naive open+metadata-check pattern had:
///
///   1. **TOCTOU on header-needed check** — two concurrent processes both
///      seeing `len()==0` and both writing headers. Fixed: `create_new` is
///      atomic at the syscall level; only one process wins the create.
///   2. **Crash between header and record** — header-only files when the
///      process dies after writing line 1 but before line 2. Fixed: both
///      lines join into one buffer, one `write_all` syscall.
///
/// Concurrent appenders after the file exists: append-on-Unix is atomic
/// up to PIPE_BUF (~4 KB on macOS); a single-line JSONL record is well
/// under that, so no explicit locking is needed for the append case.
///
/// `sync_all()` is called after both write paths so audit-log durability
/// survives power loss / crash between record emission and consumer read.
pub(crate) fn record_at(record: &FlowRecord, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating flows dir {}", parent.display()))?;
        }
    }

    // Header is centralized so LocalFileSink + AuditFileSink emit
    // byte-identical schema headers; audit's seed hash stays stable.
    let header_line = schema_header_line()?;
    let record_line = serde_json::to_string(record)?;

    // Try the atomic-create path: we win the create race → write header +
    // record together. If file already exists (other process or earlier
    // call), fall through to append-only.
    match fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
    {
        Ok(mut file) => {
            file.write_all(format!("{header_line}\n{record_line}\n").as_bytes())
                .with_context(|| format!("writing initial flow log {}", path.display()))?;
            file.sync_all()
                .with_context(|| format!("syncing flow log {}", path.display()))?;
            Ok(())
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(path)
                .with_context(|| format!("opening flow log for append {}", path.display()))?;
            // ONE `write_all` of the whole `line\n` — not `writeln!`, which
            // forwards the record text and the newline as SEPARATE `write()`s
            // that two concurrent appenders interleave mid-line, fusing two JSON
            // objects onto one line. Two real guarantees make the single-write
            // form tear-free for these small records on the targeted POSIX
            // platforms (Linux/macOS): `O_APPEND` atomically re-seeks each
            // `write()` to EOF — the actual POSIX promise, NOT PIPE_BUF (which
            // only governs pipes/FIFOs) — so no appender clobbers another; and
            // per-inode write serialization keeps one `write()` contiguous
            // against a concurrent one. `write_all` is a single `write()` in
            // practice for a sub-KB regular file (it loops only on a partial
            // return — effectively never here outside ENOSPC). Tear-proof BY
            // CONSTRUCTION is the audit sink's `flock` path; the casual sink
            // stays lock-free + best-effort.
            file.write_all(format!("{record_line}\n").as_bytes())
                .with_context(|| format!("appending to flow log {}", path.display()))?;
            file.sync_all()
                .with_context(|| format!("syncing flow log {}", path.display()))?;
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("creating flow log {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::env;
    use tempfile::TempDir;

    // Module-private helpers under test, reached explicitly across the
    // post-#508 submodule split (they are pub(crate), not part of the
    // crate's public re-export surface).
    use crate::schema::{epoch_to_hhmmss, epoch_to_yyyymmdd};

    // ─── #1311/#1276: bounded `security` read (shared keychain helper) ──
    //
    // `run_security_bounded` is exercised with `sh -c` stubs standing in for
    // the `security` binary — a sleeping stub proves the hard timeout kills the
    // read within the bound (instead of the 19-minute pre-flow freeze #563 saw
    // during flow-sink init), and echo/exit stubs prove the outcome mapping.
    use std::process::Command;
    use std::time::{Duration, Instant};

    /// (#1965) The bug: success was gated ONLY on the child's exit status,
    /// while the stdout drain's error was discarded with `unwrap_or_default()`.
    /// A pipe read that failed partway therefore returned a TRUNCATED (or
    /// empty) secret as `Found`, indistinguishable from a good read, and every
    /// caller trusted it verbatim — the Redis password and the serve bearer
    /// token both resolve through this path.
    ///
    /// These assert the DECISION rather than spawning a process, because a
    /// genuine mid-read I/O failure is not reproducible on demand — which is
    /// precisely why the defect survived: no test could reach it from outside.
    #[test]
    fn a_failed_stdout_read_is_never_reported_as_a_found_secret() {
        let out = super::keychain_outcome(true, None);
        assert!(
            !matches!(out, KeychainRead::Found(_)),
            "a partial read must not present as a complete secret, got {out:?}"
        );
        assert!(matches!(out, KeychainRead::Unavailable), "got {out:?}");
    }

    #[test]
    fn a_panicked_drain_thread_is_also_not_a_found_secret() {
        // `join()` returning Err collapses to the same `None`: both mean we do
        // not hold a trustworthy secret.
        assert!(matches!(
            super::keychain_outcome(true, None),
            KeychainRead::Unavailable
        ));
    }

    #[test]
    fn a_clean_read_still_yields_the_secret() {
        match super::keychain_outcome(true, Some(b"super-secret-value".to_vec())) {
            KeychainRead::Found(v) => assert_eq!(v, "super-secret-value"),
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn a_nonzero_exit_is_absent_regardless_of_what_was_read() {
        // The item is not there; the read outcome is moot and must not turn an
        // Absent into an Unavailable, which callers treat differently.
        assert!(matches!(
            super::keychain_outcome(false, None),
            KeychainRead::Absent
        ));
        assert!(matches!(
            super::keychain_outcome(false, Some(b"noise".to_vec())),
            KeychainRead::Absent
        ));
    }

    #[test]
    fn run_security_bounded_times_out_on_a_hung_read() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30"]);
        let start = Instant::now();
        let outcome = run_security_bounded(cmd, Duration::from_millis(300));
        assert!(matches!(outcome, KeychainRead::TimedOut), "got {outcome:?}");
        // Failed FAST — nowhere near the 30s sleep.
        assert!(start.elapsed() < Duration::from_secs(5), "did not fail fast: {:?}", start.elapsed());
    }

    #[test]
    fn run_security_bounded_returns_stdout_on_success() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf 'super-secret-value'"]);
        match run_security_bounded(cmd, Duration::from_secs(5)) {
            KeychainRead::Found(v) => assert_eq!(v, "super-secret-value"),
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn run_security_bounded_reports_absent_on_nonzero_exit() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exit 1"]);
        assert!(matches!(run_security_bounded(cmd, Duration::from_secs(5)), KeychainRead::Absent));
    }

    #[test]
    fn run_security_bounded_reports_unavailable_when_binary_missing() {
        let cmd = Command::new("darkmux-no-such-binary-xyz");
        assert!(matches!(
            run_security_bounded(cmd, Duration::from_secs(5)),
            KeychainRead::Unavailable
        ));
    }

    // ─── (#388) RedisSink graceful-disable-on-unreachable ────────────

    fn minimal_record() -> FlowRecord {
        FlowRecord {
            ts: "2025-01-15T12:34:56Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Dispatch,
            action: "test".to_string(),
            handle: "t".to_string(),
            phase_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        }
    }

    // Lazy client — never connects until a write, so we can exercise the
    // failure-accounting directly without a live Redis.
    fn unreachable_sink() -> RedisSink {
        RedisSink::new("redis://127.0.0.1:6390", "darkmux:test", None).unwrap()
    }

    #[test]
    fn redis_sink_disables_after_threshold_consecutive_failures() {
        let sink = unreachable_sink();
        let e = anyhow::anyhow!("synthetic connect failure");
        // Below threshold: accumulates, stays enabled.
        assert!(!sink.note_failure(&e));
        assert!(!sink.is_disabled());
        assert!(!sink.note_failure(&e));
        assert!(!sink.is_disabled());
        // Threshold (3rd) flips it — note_failure returns true exactly once.
        assert!(sink.note_failure(&e), "3rd failure should flip to disabled");
        assert!(sink.is_disabled());
        // Already disabled: further failures don't re-flip (no repeat log).
        assert!(!sink.note_failure(&e));
    }

    #[test]
    fn redis_sink_success_resets_failure_streak() {
        let sink = unreachable_sink();
        let e = anyhow::anyhow!("x");
        sink.note_failure(&e);
        sink.note_failure(&e);
        sink.note_success(); // a single success clears the streak
        sink.note_failure(&e);
        sink.note_failure(&e);
        assert!(!sink.is_disabled(), "2 failures after a reset must not disable");
        assert!(sink.note_failure(&e), "3 consecutive post-reset failures disable");
        assert!(sink.is_disabled());
    }

    #[test]
    fn redis_sink_disable_is_permanent_for_process() {
        // Disable is a one-way latch for the process: once tripped, a
        // later success does NOT re-enable the sink (a disabled sink
        // never even reaches note_success via write(), but assert the
        // contract directly), and further failures neither re-flip nor
        // re-log (note_failure returns false).
        let sink = unreachable_sink();
        let e = anyhow::anyhow!("x");
        sink.note_failure(&e);
        sink.note_failure(&e);
        assert!(sink.note_failure(&e));
        assert!(sink.is_disabled());
        sink.note_success();
        assert!(sink.is_disabled(), "success must not re-enable a disabled sink");
        assert!(!sink.note_failure(&e), "no re-flip / re-log after disable");
        assert!(sink.is_disabled());
    }

    #[test]
    fn redis_sink_disabled_write_is_a_fast_noop() {
        let sink = unreachable_sink();
        let e = anyhow::anyhow!("x");
        // Trip the threshold.
        sink.note_failure(&e);
        sink.note_failure(&e);
        sink.note_failure(&e);
        assert!(sink.is_disabled());
        // A write while disabled returns Ok WITHOUT attempting a
        // connection — proven by the absence of the ~500ms connect
        // timeout the unreachable URL would otherwise incur.
        let start = std::time::Instant::now();
        assert!(sink.write(&minimal_record()).is_ok());
        assert!(
            start.elapsed() < std::time::Duration::from_millis(200),
            "disabled write must skip the connection attempt, not pay the timeout"
        );
    }

    #[serial_test::serial]
    #[test]
    fn creates_file_with_schema_header_on_first_record() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("2025-01-15.jsonl");

        let record = FlowRecord {
            ts: "2025-01-15T12:34:56Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Dispatch,
            action: "ran".to_string(),
            handle: "test-1".to_string(),
            phase_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };

        record_at(&record, &path).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "expected 2 lines: header + record");

        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["_type"], "schema");

        let second: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["level"], "info");
    }

    #[serial_test::serial]
    #[test]
    fn appends_to_existing_file_without_re_emitting_header() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("2025-01-15.jsonl");

        let r = |action: &str| FlowRecord {
            ts: "2025-01-15T12:34:56Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Dispatch,
            action: action.to_string(),
            handle: "test".to_string(),
            phase_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };

        record_at(&r("first"), &path).unwrap();
        record_at(&r("second"), &path).unwrap();
        record_at(&r("third"), &path).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 4, "expected 4 lines: header + 3 records");

        // Header should be exactly once.
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["_type"], "schema");

        // Records 2 and 3 are plain flow records, no `_type: schema`.
        for line in &lines[1..] {
            let line: Value = serde_json::from_str(line).unwrap();
            assert!(
                line.get("_type").is_none(),
                "record line should not contain _type"
            );
        }
    }

    /// Concurrent appenders to a shared day-file must never tear each other's
    /// lines. Regression for the flow-test flake: the append path used
    /// `writeln!` (record text + `\n` as SEPARATE writes), so two threads could
    /// interleave mid-line into invalid JSON. The fix is one atomic `write_all`
    /// of `line\n` (atomic under `O_APPEND` for sub-`PIPE_BUF` records). Not
    /// `#[serial]` — it deliberately drives concurrency against ONE file it owns.
    #[test]
    fn concurrent_appends_never_tear_lines() {
        use std::collections::HashSet;
        use std::thread;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("2025-01-15.jsonl");
        let threads = 8;
        let per_thread = 40;

        // Pre-create the file so every thread takes the append path — isolates
        // this to the append-tearing concern, not the create-race.
        record_at(&minimal_record(), &path).unwrap();

        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let path = path.clone();
                thread::spawn(move || {
                    for i in 0..per_thread {
                        let mut rec = minimal_record();
                        rec.action = format!("thr{t}-rec{i}");
                        record_at(&rec, &path).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        // Every line must parse (no torn/interleaved JSON) ...
        let contents = fs::read_to_string(&path).unwrap();
        let mut markers = HashSet::new();
        for (n, line) in contents.lines().enumerate() {
            let v: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("torn line {n}: {e}: {line:?}"));
            if let Some(a) = v["action"].as_str() {
                markers.insert(a.to_string());
            }
        }
        // ... and every record must be present (none lost to a clobbering write).
        for t in 0..threads {
            for i in 0..per_thread {
                assert!(markers.contains(&format!("thr{t}-rec{i}")), "missing thr{t}-rec{i}");
            }
        }
    }

    #[test]
    fn record_serializes_with_expected_shape() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("2025-01-15.jsonl");

        let record = FlowRecord {
            ts: "2025-06-01T08:00:00Z".to_string(),
            level: Level::Warn,
            category: Category::Audit,
            tier: Tier::Local,
            stage: Stage::Estimate,
            action: "budget_check".to_string(),
            handle: "handle-42".to_string(),
            phase_id: Some("sp-100".to_string()),
            session_id: Some("sess-abc".to_string()),
            source: Some("estimator".to_string()),
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };

        record_at(&record, &path).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        // Second line is the record (first is schema header).
        let lines: Vec<&str> = contents.lines().collect();
        let rec_line = lines[1];

        // Parse as JSON and check fields.
        let parsed: Value = serde_json::from_str(rec_line).unwrap();

        assert_eq!(parsed["ts"], "2025-06-01T08:00:00Z");
        assert_eq!(parsed["level"], "warn");
        assert_eq!(parsed["category"], "audit");
        assert_eq!(parsed["tier"], "local");
        assert_eq!(parsed["stage"], "estimate");
        assert_eq!(parsed["action"], "budget_check");
        assert_eq!(parsed["handle"], "handle-42");

        // Optional fields should be present (not omitted) when set.
        let phase_id = parsed.get("phase_id").expect("expected phase_id");
        assert_eq!(phase_id, "sp-100");

        let session_id = parsed.get("session_id").expect("expected session_id");
        assert_eq!(session_id, "sess-abc");

        let source = parsed.get("source").expect("expected source");
        assert_eq!(source, "estimator");

        // Round-trip: parse back into FlowRecord.
        let roundtrip: FlowRecord = serde_json::from_str(rec_line).unwrap();
        assert_eq!(roundtrip.action, "budget_check");
        assert_eq!(roundtrip.handle, "handle-42");
    }

    #[serial_test::serial]
    #[test]
    fn record_at_uses_explicit_path() {
        let tmp = TempDir::new().unwrap();

        // Ensure DARKMUX_FLOWS_DIR is NOT set (or cleared) so we don't
        // accidentally write to an unexpected location.
        let prev = env::var("DARKMUX_FLOWS_DIR").ok();

        record_at(
            &FlowRecord {
                ts: "2025-03-21T14:00:00Z".to_string(),
                level: Level::Trace,
                category: Category::Review,
                tier: Tier::Frontier,
                stage: Stage::Scope,
                action: "scope_review".to_string(),
                handle: "ex-path-1".to_string(),
                phase_id: None,
                session_id: None,
                source: Some("reviewer".to_string()),
                model: None,
                reasoning: None,
                mission_id: None,
                machine_id: None,
                machine_uid: None,
                prev_hash: None,
                hash: None,
                payload: None,
                work_id: None,
                attempt: None,
            },
            &tmp.path().join("custom.jsonl"),
        )
        .unwrap();

        // Restore env var.
        match prev {
            Some(v) => env::set_var("DARKMUX_FLOWS_DIR", v),
            None => env::remove_var("DARKMUX_FLOWS_DIR"),
        }

        let contents = fs::read_to_string(tmp.path().join("custom.jsonl")).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2); // header + record

        let parsed: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(parsed["action"], "scope_review");
    }

    #[serial_test::serial]
    #[test]
    fn optional_fields_omit_when_none() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("2025-01-01.jsonl");

        let record = FlowRecord {
            ts: "2025-01-01T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Ship,
            action: "deploy".to_string(),
            handle: "ship-1".to_string(),
            phase_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };

        record_at(&record, &path).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        let rec_line = lines[1];

        // Optional fields should NOT appear when None.
        let parsed: Value = serde_json::from_str(rec_line).unwrap();

        // Verify keys don't exist (not null, absent entirely).
        assert!(parsed.get("phase_id").is_none());
        assert!(parsed.get("session_id").is_none());
        assert!(parsed.get("source").is_none());

        // Required fields must be present.
        assert!(parsed.get("ts").is_some());
        assert!(parsed.get("level").is_some());
        assert!(parsed.get("action").is_some());
    }

    #[serial_test::serial]
    #[test]
    fn flows_dir_respects_env_override() {
        isolate_test_env_once();
        let tmp = TempDir::new().unwrap();

        // SAFETY: serialized via `#[serial_test::serial]` on every test that
        // mutates this env var. Outside that lock, `set_var` is unsafe in
        // 2024 edition (race with other readers); serial-tests serializes it.
        let prev = env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { env::set_var("DARKMUX_FLOWS_DIR", tmp.path()); }

        let rec = FlowRecord {
            ts: "2025-04-10T10:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Review,
            action: "env_test".to_string(),
            handle: "ev-1".to_string(),
            phase_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };

        // Capture the day-key BEFORE calling record() so a midnight-UTC
        // crossing between record() and the assertion doesn't make the
        // file appear at a different name than we check.
        let day_before = day_utc_now();
        super::record(rec).unwrap();
        let day_after = day_utc_now();

        // SAFETY: same — serialized via the test attribute.
        unsafe {
            match prev {
                Some(v) => env::set_var("DARKMUX_FLOWS_DIR", v),
                None => env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }

        // Accept either day's file (handles the rare midnight crossing).
        let candidates = if day_before == day_after {
            vec![tmp.path().join(format!("{day_before}.jsonl"))]
        } else {
            vec![
                tmp.path().join(format!("{day_before}.jsonl")),
                tmp.path().join(format!("{day_after}.jsonl")),
            ]
        };
        let found = candidates.iter().find(|p| p.exists()).cloned();
        assert!(
            found.is_some(),
            "file should exist in env-override dir under {} or {}",
            day_before,
            day_after
        );

        let contents = fs::read_to_string(found.unwrap()).unwrap();
        assert!(contents.contains("env_test"));
    }

    #[test]
    fn epoch_to_yyyymmdd_known_dates() {
        // Unix epoch start
        let (y, m, d) = epoch_to_yyyymmdd(0);
        assert_eq!((y, m, d), (1970, 1, 1));

        // Leap year: 2024-02-29
        let (y, m, d) = epoch_to_yyyymmdd(1_709_164_800);
        assert_eq!((y, m, d), (2024, 2, 29));

        // Year boundary: 2025-01-01 = epoch 1735689600
        let (y, m, d) = epoch_to_yyyymmdd(1_735_689_600);
        assert_eq!((y, m, d), (2025, 1, 1));

        // Mid-year: 2024-07-04 = epoch 1_720_051_200
        let (y, m, d) = epoch_to_yyyymmdd(1_720_051_200);
        assert_eq!((y, m, d), (2024, 7, 4));
    }

    #[test]
    fn epoch_to_hhmmss_known_times() {
        // Midnight
        assert_eq!(epoch_to_hhmmss(0), (0, 0, 0));
        // 2024-01-01 00:00:00 UTC
        assert_eq!(epoch_to_hhmmss(1_704_067_200), (0, 0, 0));
        // 2024-01-01 12:34:56 UTC = epoch start + 12*3600 + 34*60 + 56 = 1_704_067_200 + 45_296
        assert_eq!(epoch_to_hhmmss(1_704_067_200 + 45_296), (12, 34, 56));
        // 23:59:59 boundary: midnight - 1 second
        assert_eq!(epoch_to_hhmmss(86_400 - 1), (23, 59, 59));
        // Mid-day check: epoch 1_720_094_400 = 2024-07-04 12:00:00 UTC
        // (epoch 1_720_051_200 is 2024-07-04 00:00:00 UTC; +43_200s = noon)
        assert_eq!(epoch_to_hhmmss(1_720_051_200 + 43_200), (12, 0, 0));
    }

    #[test]
    fn ts_utc_now_returns_iso8601_datetime() {
        // Schema 1.1: ts must be full datetime with time-of-day, not just a date.
        let ts = ts_utc_now();
        let bytes = ts.as_bytes();
        assert_eq!(ts.len(), 20, "expected YYYY-MM-DDTHH:MM:SSZ (20 chars), got {ts:?}");
        assert_eq!(bytes[4], b'-');
        assert_eq!(bytes[7], b'-');
        assert_eq!(bytes[10], b'T');
        assert_eq!(bytes[13], b':');
        assert_eq!(bytes[16], b':');
        assert_eq!(bytes[19], b'Z');
        // Digits in the expected positions
        for &i in &[0usize, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18] {
            assert!(
                bytes[i].is_ascii_digit(),
                "expected digit at index {i} in {ts:?}",
            );
        }
    }

    #[test]
    fn day_utc_now_returns_date_only() {
        // day_utc_now() is for file naming — must stay YYYY-MM-DD regardless
        // of the schema bump on ts_utc_now().
        let day = day_utc_now();
        let bytes = day.as_bytes();
        assert_eq!(day.len(), 10, "expected YYYY-MM-DD (10 chars), got {day:?}");
        assert_eq!(bytes[4], b'-');
        assert_eq!(bytes[7], b'-');
        for &i in &[0usize, 1, 2, 3, 5, 6, 8, 9] {
            assert!(
                bytes[i].is_ascii_digit(),
                "expected digit at index {i} in {day:?}",
            );
        }
    }

    // ─── #162 Phase 1: FlowSink trait ────────────────────────────────

    #[test]
    #[serial_test::serial] // (#902) mutates the process-global DARKMUX_FLOWS_DIR — must
                           // serialize with the other env-mutating tests (e.g. the audit
                           // breadcrumb scanner), or it races them and flakes under -j.
    fn local_file_sink_writes_through_to_per_day_jsonl() {
        // LocalFileSink should produce the same on-disk result as the
        // historical `record_at` path — preserving behavior under the
        // trait abstraction is the whole point of Phase 1.
        use std::env;
        let tmp = TempDir::new().unwrap();
        let prev = env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { env::set_var("DARKMUX_FLOWS_DIR", tmp.path()); }

        let sink = LocalFileSink::new();
        let rec = FlowRecord {
            ts: "2025-01-01T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "test".to_string(),
            handle: "h".to_string(),
            phase_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };
        sink.write(&rec).unwrap();

        // Result must be a per-day JSONL file at flows_dir() with the
        // record's content as line 2 (line 1 is the schema header).
        let day = day_utc_now();
        let path = tmp.path().join(format!("{day}.jsonl"));
        assert!(path.exists(), "sink should have created per-day file");
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert!(lines[0].contains("\"_type\":\"schema\""), "line 1 = header");
        assert!(lines[1].contains("\"action\":\"test\""), "line 2 = record");

        unsafe {
            match prev {
                Some(v) => env::set_var("DARKMUX_FLOWS_DIR", v),
                None => env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
    }

    /// Test-only sink that captures records in memory. Used to verify the
    /// trait contract without filesystem interaction.
    struct InMemorySink {
        captured: std::sync::Mutex<Vec<FlowRecord>>,
    }
    impl InMemorySink {
        fn new() -> Self {
            Self { captured: std::sync::Mutex::new(Vec::new()) }
        }
        fn count(&self) -> usize {
            self.captured.lock().unwrap().len()
        }
    }
    impl FlowSink for InMemorySink {
        fn write(&self, record: &FlowRecord) -> Result<()> {
            self.captured.lock().unwrap().push(record.clone());
            Ok(())
        }
        fn info(&self) -> SinkInfo {
            SinkInfo { kind: "InMemory".to_string(), config: Default::default(), children: vec![], raw_url: None }
        }
    }

    #[test]
    fn teesink_audit_failure_drops_breadcrumb_into_local_sink() {
        // (#877) When the AuditFile child write fails, TeeSink must leave a
        // durable `audit.write_failed` breadcrumb in the LocalFile child so the
        // dropped audit record is DETECTABLE (the hash chain itself can't show
        // the gap). Two test sinks reporting the real `kind` strings the
        // breadcrumb logic keys on.
        struct KindedRecorder {
            kind: &'static str,
            captured: std::sync::Mutex<Vec<FlowRecord>>,
        }
        impl FlowSink for KindedRecorder {
            fn write(&self, r: &FlowRecord) -> Result<()> {
                self.captured.lock().unwrap().push(r.clone());
                Ok(())
            }
            fn info(&self) -> SinkInfo {
                SinkInfo { kind: self.kind.to_string(), config: Default::default(), children: vec![], raw_url: None }
            }
        }
        struct FailingAudit;
        impl FlowSink for FailingAudit {
            fn write(&self, _r: &FlowRecord) -> Result<()> {
                Err(anyhow::anyhow!("audit dir unwritable (test)"))
            }
            fn info(&self) -> SinkInfo {
                SinkInfo { kind: AUDIT_SINK_KIND.to_string(), config: Default::default(), children: vec![], raw_url: None }
            }
        }

        let local = Arc::new(KindedRecorder {
            kind: LOCAL_SINK_KIND,
            captured: std::sync::Mutex::new(Vec::new()),
        });
        let tee = TeeSink::new(vec![
            local.clone() as Arc<dyn FlowSink>,
            Arc::new(FailingAudit),
        ]);

        let mut rec = minimal_record();
        rec.action = "dispatch.complete".to_string();
        rec.session_id = Some("sess-1".to_string());

        // TeeSink returns Err (the audit child failed), but the breadcrumb is
        // the point.
        assert!(tee.write(&rec).is_err(), "audit-child failure surfaces an Err");

        let captured = local.captured.lock().unwrap();
        assert_eq!(
            captured.len(),
            2,
            "local sink should hold the original record + the audit-failure breadcrumb"
        );
        assert_eq!(captured[0].action, "dispatch.complete", "original first");
        assert_eq!(captured[1].action, "audit.write_failed", "breadcrumb second");
        assert!(matches!(captured[1].level, Level::Error));
        assert!(matches!(captured[1].category, Category::Audit));
        assert!(captured[1].prev_hash.is_none() && captured[1].hash.is_none());
        let payload = captured[1].payload.as_ref().expect("breadcrumb carries payload");
        assert_eq!(payload["dropped_action"], "dispatch.complete");
        assert_eq!(payload["dropped_session_id"], "sess-1");
    }

    #[serial_test::serial]
    #[test]
    fn count_audit_write_failures_today_counts_only_breadcrumbs() {
        // (#877) Locks the action-string contract END TO END: the breadcrumb
        // emit literal (`emit_audit_failure_breadcrumb`) and the scanner literal
        // (`count_audit_write_failures_today`) are independent strings — this
        // catches them drifting apart. `DARKMUX_FLOWS_DIR` env wins `flows_dir()`.
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path()) };
        let path = tmp.path().join(format!("{}.jsonl", day_utc_now()));
        let lines = [
            r#"{"action":"audit.write_failed","category":"audit","level":"error"}"#,
            r#"{"action":"dispatch.complete","category":"work","level":"info"}"#, // ignored
            "not valid json",                                                     // ignored — no panic
            r#"{"action":"audit.write_failed","category":"audit","level":"error"}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        assert_eq!(count_audit_write_failures_today(), 2);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
    }

    #[test]
    fn sink_kind_consts_match_real_sinks() {
        // (#877) The breadcrumb path keys on these EXACT `info().kind` strings;
        // a silent rename of either sink's kind would disable detection. Lock
        // the consts to the real sinks so a rename fails at test time, not in
        // production (where the breadcrumb would just quietly no-op).
        assert_eq!(LocalFileSink::new().info().kind, LOCAL_SINK_KIND);
        #[cfg(unix)]
        assert_eq!(
            AuditFileSink::with_dir(std::path::PathBuf::from("/tmp/dm-kind-check")).info().kind,
            AUDIT_SINK_KIND
        );
    }

    #[test]
    fn record_via_dispatches_through_explicit_sink() {
        // The trait's contract: any FlowSink impl receives the record on
        // write. record_via is the public extension point for callers
        // that want to override the default LocalFileSink (tests today;
        // RedisSink + TeeSink in Phase 3 of #162).
        let sink = InMemorySink::new();
        let rec = FlowRecord {
            ts: "2025-01-01T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "explicit-sink".to_string(),
            handle: "h".to_string(),
            phase_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };

        record_via(&sink, &rec).unwrap();
        record_via(&sink, &rec).unwrap();
        assert_eq!(sink.count(), 2);
    }

    #[test]
    fn tee_sink_writes_to_all_children() {
        // #162 Phase 3: TeeSink composes N sinks. Each child receives
        // the record. This is the canonical full-composition deployment shape
        // ([LocalFileSink, RedisSink] in production); the test uses
        // two InMemorySink test doubles to verify the trait contract.
        let a = Arc::new(InMemorySink::new());
        let b = Arc::new(InMemorySink::new());
        let tee = TeeSink::new(vec![
            a.clone() as Arc<dyn FlowSink>,
            b.clone() as Arc<dyn FlowSink>,
        ]);

        let rec = FlowRecord {
            ts: "2025-01-01T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "tee-test".to_string(),
            handle: "h".to_string(),
            phase_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };
        tee.write(&rec).unwrap();
        tee.write(&rec).unwrap();

        assert_eq!(a.count(), 2);
        assert_eq!(b.count(), 2);
    }

    /// Test-only sink that always returns an error on write. Used to
    /// verify TeeSink's best-effort semantics — one failing child
    /// shouldn't prevent the others from receiving the record.
    struct FailingSink;
    impl FlowSink for FailingSink {
        fn write(&self, _record: &FlowRecord) -> Result<()> {
            anyhow::bail!("simulated sink failure for test")
        }
        fn info(&self) -> SinkInfo {
            SinkInfo { kind: "Failing".to_string(), config: Default::default(), children: vec![], raw_url: None }
        }
    }

    #[test]
    fn tee_sink_continues_writing_when_one_child_fails() {
        // The audit substrate must remain durable even when the
        // coordination layer (Redis) is unreachable. TeeSink logs the
        // failure and continues writing to other sinks. First error
        // bubbles up to the caller; subsequent sinks still receive.
        let good = Arc::new(InMemorySink::new());
        let bad = Arc::new(FailingSink);
        let tee = TeeSink::new(vec![
            bad as Arc<dyn FlowSink>,
            good.clone() as Arc<dyn FlowSink>,
        ]);

        let rec = FlowRecord {
            ts: "2025-01-01T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "tee-fail".to_string(),
            handle: "h".to_string(),
            phase_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };
        let err = tee.write(&rec).unwrap_err();
        // Caller sees the error (so they can react if they want)
        assert!(err.to_string().contains("simulated sink failure"));
        // But the audit substrate still received the record
        assert_eq!(good.count(), 1);
    }

    #[test]
    #[serial_test::serial]
    fn record_default_path_uses_local_file_sink() {
        // The public `record()` should dispatch through the default sink
        // and produce on-disk output (behavior-equivalent to pre-#162).
        // We can't easily intercept the default sink from a test, but we
        // can verify the round trip: write via record(), read from
        // flows_dir(), see the record.
        use std::env;
        isolate_test_env_once();
        let tmp = TempDir::new().unwrap();
        let prev = env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { env::set_var("DARKMUX_FLOWS_DIR", tmp.path()); }

        let rec = FlowRecord {
            ts: "2025-01-01T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "default-path".to_string(),
            handle: "h".to_string(),
            phase_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };
        super::record(rec).unwrap();

        let day = day_utc_now();
        let path = tmp.path().join(format!("{day}.jsonl"));
        assert!(path.exists(), "default sink should have written to {}", path.display());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"action\":\"default-path\""));

        unsafe {
            match prev {
                Some(v) => env::set_var("DARKMUX_FLOWS_DIR", v),
                None => env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
    }

    #[test]
    fn flow_schema_version_is_pinned_so_a_bump_is_deliberate() {
        // Pin the schema version so an accidental rename can't ship silently;
        // any bump beyond this should be a deliberate code change paired with
        // an update to this assertion (and corresponding viewer EXPECTED_*
        // bump if the change is breaking).
        //
        // Version history:
        //   1.2.0 — added optional `model` field (#106, Phase 4 of #104)
        //   1.3.0 — added optional `reasoning` and `mission_id` fields and a
        //           new `Stage::TierDecision` variant (#136). Minor bump.
        //   1.4.0 — added optional `machine_id` and `orchestrator` fields
        //           (#167; substrate for fleet UI). Minor bump.
        //   1.5.0 — added optional `prev_hash` and `hash` fields for
        //           AuditFileSink's chain-of-custody (#163). Minor bump:
        //           absent in records from LocalFileSink (casual write path).
        //   1.6.0 — added optional `payload` JSON field for event-specific
        //           data; new action types: dispatch.turn / .tool /
        //           .compaction / .reasoning + mission.compile.start /
        //           .complete (#204). Minor bump.
        //   1.7.0 — added action type `dispatch.turn.heartbeat` emitted by
        //           the live trajectory tailer to keep topology edges
        //           animated during long streaming turns; pairs with
        //           runtime-side `model.partial` SSE chunks (#231). Minor
        //           bump — older readers safely ignore the new action type.
        //   1.8.0 — added optional `machine_tier`, `work_id`, and `attempt`
        //           fields on FlowRecord for the parallel-dispatch substrate
        //           (#246 PR-A tier substrate). `machine_tier` auto-populated
        //           from `DARKMUX_MACHINE_TIER` env at record-write time;
        //           `work_id` + `attempt` populated by the dispatch path
        //           when work flowed through the queue. Minor bump — older
        //           readers safely ignore the new fields.
        //   1.9.0 — REMOVED `machine_tier` (the {inference/hub/client} machine-
        //           capacity label that no routing consumed; it conflated the
        //           `tier` enum with a hardware label. Capacity moves to
        //           capability-based model selection — #321/#322). Minor bump:
        //           old readers tolerate the now-unknown key. Pre-1.9.0
        //           AuditFileSink chains need rotation (canonical-form change).
        //   1.10.0 — added `Category::Telemetry` (#557): telemetry folds into the
        //           one flow stream as a first-class family, retiring
        //           instruments.jsonl. Minor + additive — new records only, so
        //           prior AuditFileSink chains survive without rotation.
        //   1.11.0 — added optional `machine_uid` (#640): the stable hardware
        //           identity. Minor + additive — new records only, chains survive.
        //   1.12.0 — added the `telemetry.tokens` action + `source=tokens` (#782):
        //           per-dispatch token totals (prompt/completion/total) in payload,
        //           the spine the live "tokens off-meter" view aggregates. Minor +
        //           additive — new action value, no struct change; older readers
        //           ignore it. New records only, chains survive.
        //   1.13.0 — `telemetry.tokens` now emitted PER TURN by the dispatch
        //           tailer (#795), new optional `turn_seq` payload field; the
        //           at-complete aggregate retired so records sum to the
        //           dispatch total without double-counting. Minor + additive —
        //           consumers that SUM the family are unaffected.
        //   1.14.0 — renamed Stage::Retrospect → Stage::Debrief (serde value
        //           "retrospect" → "debrief"), the NASA-vocabulary rename (#999).
        //           The variant was an unemitted placeholder — no record carried
        //           the old value — so only the enum's value-set changes; chains
        //           survive without rotation. Unemitted until #1000.
        //   1.15.0 — telemetry.process now samples the HOST system, not the
        //           container: payload gains `mem` + `gpu` (host RAM/GPU util%)
        //           and `cpu` shifts container→host (#814/#1064). Minor +
        //           additive — older readers ignore mem/gpu; chains survive.
        //   1.16.0 — dispatch.tool payload gains `args` (the actual tool
        //           arguments, capped) so the operator can recall what each
        //           call did. Minor + additive — older readers ignore it.
        //   1.17.0 — new action values for the review-pipeline driver's run
        //           observability (#1247 Part 1): funnel.task/funnel.step/
        //           funnel.ruling. Minor + additive — older readers ignore
        //           the unknown actions; no struct/field change.
        //   (code-internal, no FLOW_SCHEMA_VERSION bump) — #1349: the above
        //           three actions renamed to the review.{task,step,ruling}
        //           family (module renamed funnel -> review; see schema.rs's
        //           fuller changelog entry). Action STRING only, same payloads.
        //   (code-internal, no FLOW_SCHEMA_VERSION bump) — #1434: that review
        //           task/step/ruling vocabulary is RETIRED — both review paths
        //           now emit only the generic `step result` companion. Records
        //           are per-run-local/ephemeral, so no bump, no migration.
        //   1.18.0 — live seat-card metrics for agentic seats (#1483 emit half):
        //           the trajectory tailer's per-event records gain optional
        //           `payload.step_id` (viewer seat-card attribution), plus
        //           `turns_so_far` on dispatch.turn and `tool_calls_so_far` on
        //           dispatch.tool (authoritative running counts). Minor +
        //           additive — older readers ignore the new payload fields; see
        //           schema.rs's fuller changelog entry.
        //   1.19.0 — REMOVED `orchestrator` (#1758) — write-only, machine-scoped
        //           provenance describing an invocation-scoped fact; nothing ever
        //           read it. Same shape of removal as 1.9.0's `machine_tier`; see
        //           schema.rs's fuller changelog entry.
        //   1.20.0: new action `"step timing"` (#1877, final wiring step): a
        //           scheduler-produced `StepRecord` companion record, streamed
        //           live per step by every mission that runs through
        //           `run_step_graph`. Minor + additive: older readers ignore
        //           the unknown action; see schema.rs's fuller changelog entry.
        //   1.21.0: added the `result` payload key on `dispatch.tool` (#2007).
        //           The record carried `result_chars` and discarded the result,
        //           so a failed tool call could be counted but not diagnosed.
        //           Bounded at 64 KiB here (the container-side trajectory keeps
        //           it in full); truncation is in-band and `result_chars` stays
        //           the true length. Minor + additive; see schema.rs.
        //   1.22.0: added `outcome`/`exit_code`/`failure_reason` on
        //           `dispatch.tool` AND corrected what `ok` means for bash
        //           (#2008): a command that ran and reported non-zero is now
        //           `ok: true`. A defect correction — the field always
        //           documented itself as tool-success — but a boundary for
        //           any series aggregating `ok` across it. See schema.rs.
        //   1.23.0: new action values `hook.fired`/`hook.failed` (#2093) —
        //           `HookSink`'s own firing/failure records. No struct/enum
        //           change; older readers ignore the two new action values.
        //           See schema.rs's fuller changelog entry.
        //   1.24.0: new `crawl.*` action family for `darkmux mission
        //           launch crawl` (#1959 packet 2). See schema.rs.
        //   1.25.0: added `turn_delay_ms` on `dispatch.start` and
        //           `rest_ms`/`rests`/`turn_delay_effective_ms` on
        //           `dispatch.complete` — the global inter-turn rest
        //           (#2094). Also added the `dispatch.rest` action itself
        //           (one per `runtime.rest` trajectory event, live on the
        //           flow stream). Additive payload fields + one new
        //           action value, no struct change. See schema.rs.
        //   1.26.0 (#1959, revised): RETIRED the 1.24.0 `crawl.*` action
        //           family — the crawl launcher now uses the generic
        //           `mission start`/`mission close`/`step start`/`step
        //           complete`/`step error` actions with additive payload
        //           keys (`workspace`, `unit`, `source`, `sha`, `rule`,
        //           `est_tokens`, `findings`, …). `crawl.finding` has no
        //           replacement action: a rejected `create_finding` reply
        //           now classifies as a FAILED tool call (`payload.ok:
        //           false` on the ordinary `dispatch.tool` record), and
        //           `DispatchOpts::record_context` merges caller-supplied
        //           provenance under `payload.context` on every record a
        //           dispatch's flow-record surface emits. See schema.rs's
        //           fuller changelog entry.
        //   1.27.0 (#2107): `dispatch.complete`'s `host` envelope block
        //           gains a real per-metric reduction (`peak_pct`,
        //           `mean_pct`, `p95_pct`, `above_80_ms` for each of
        //           `cpu`/`mem`/`gpu`) instead of two bare peaks, plus
        //           `sample_interval_ms`. The pre-1.27.0 top-level
        //           `peak_cpu_pct`/`peak_mem_pct` are kept as aliases for
        //           one release. Additive; no struct/field REMOVAL. See
        //           schema.rs's fuller changelog entry.
        //   1.28.0 (#2108): `dispatch.complete`'s `host` envelope block
        //           gains `power` (`{cpu,gpu,total}` × `{mean_mw, peak_mw}`),
        //           `thermal` (`{worst_state, above_nominal_ms,
        //           min_cpu_speed_limit_pct}`) and `energy_mwh`, from the
        //           in-process host probe. Additive; every 1.27.0 field is
        //           byte-identical. See schema.rs.
        //   1.29.0 (#2165): `dispatch start`'s payload (and the finished
        //           envelope) gain `bounds` — the resolved runtime knobs
        //           WITH provenance (`{value, source}` per knob). Additive;
        //           same free-form `payload` blob. See schema.rs.
        //   1.30.0 (2026-08-30 fleet-observability finding): `dispatch.rest`
        //           gains `reason`/`state`; `dispatch.complete` (and the
        //           envelope) gain `paced_rest_ms`. Additive; same
        //           free-form `payload` blob. See schema.rs.
        //   1.31.0 (#2111): new actions `machine.thermal` (daemon-sampler
        //           TRANSITION events) and `machine.telemetry` (dispatch-
        //           sampler periodic SAMPLE curve); `dispatch complete`/
        //           `dispatch error` (and the envelope) gain `host_window`.
        //           Additive; same free-form `payload` blob. See schema.rs.
        //   1.32.0: `dispatch start` gains `tools_requested` (#2268).
        //   1.33.0: `dispatch.tool` gains `emitted` + `emit_seq` (#2272).
        //   1.34.0: `payload.tool_name` `create_finding` → `create_finding` (rename).
        //   1.35.0: the mod channel — `dispatch.tool` also carries
        //           `tool_name: "create_mod"` (riding 1.33.0's `emitted` /
        //           `emit_seq`), and `dispatch start` gains
        //           `findings_in_brief` (#2265).
        //   1.36.0: `dispatch start`'s `findings_in_brief` becomes
        //           `brief_refs: [{kind, key}]` — findings AND mods, one
        //           provenance list. The old key is dropped, not aliased: it
        //           had zero consumers and was one version old (#2295).
        //   1.37.0: `mission start` gains `graph` on config-launched runs —
        //           what the config declared vs what was minted, and every
        //           `enabled: false` item pruned with its reason (#2299).
        //   1.38.0: a new `mission.grow` action — one record per growth
        //           event, naming the template, the producing task, the
        //           artifact path, the item count and the real task ids
        //           minted from it (#2300).
        //   1.39.0: `mission close` gains a payload on EVERY generic config
        //           (the last phase's last step output, when it is a JSON
        //           object — a generic widening, not a crawl feature); the
        //           crawl's own payloads move onto it, `mission start`
        //           drops the retired launcher's crawl keys, the bespoke
        //           per-unit step payloads are gone, and `mission.grow`'s
        //           `source_path` is renamed `source` (#2301).
        //   1.40.0: `mission.grow` gains `producer_step` +
        //           `producer_status` and a third `reason` value
        //           (`producer_errored`); `producer_status` is the stable
        //           lowercase `NodeStatus` vocabulary, never a `Debug`
        //           rendering; `source` narrows to ONE meaning — the
        //           producing step's id, never an absolute host path
        //           (#2310 swarm F / S2-2).
        assert_eq!(FLOW_SCHEMA_VERSION, "1.40.0");
    }

    #[test]
    fn telemetry_category_serializes_to_lowercase_word() {
        // The served viewer keys on `category: "telemetry"` (docs/demo/index.html
        // flowToRenderModel). The `#[serde(rename_all = "lowercase")]` on Category
        // must produce exactly that string — pin it so a variant rename can't
        // silently desync the wire from the viewer.
        let v = serde_json::to_value(crate::schema::Category::Telemetry).unwrap();
        assert_eq!(v, serde_json::json!("telemetry"));
    }

    #[test]
    fn stage_tier_decision_round_trips_as_kebab_case() {
        // Schema 1.3 introduced Stage::TierDecision and changed the
        // serde rename from `lowercase` to `kebab-case`. Both directions
        // (serialize + deserialize) must agree for the new variant AND
        // for the existing single-word variants (which should be no-ops).
        for (variant, expected) in [
            (Stage::Scope, "scope"),
            (Stage::Estimate, "estimate"),
            (Stage::Dispatch, "dispatch"),
            (Stage::Review, "review"),
            (Stage::Ship, "ship"),
            (Stage::Debrief, "debrief"),
            (Stage::TierDecision, "tier-decision"),
        ] {
            let serialized = serde_json::to_string(&variant).unwrap();
            assert_eq!(serialized.trim_matches('"'), expected,
                "{variant:?} should serialize as {expected}");
            let parsed: Stage = serde_json::from_str(&serialized).unwrap();
            // Round-trip equality via Debug (Stage doesn't derive PartialEq).
            assert_eq!(format!("{parsed:?}"), format!("{variant:?}"));
        }
    }

    #[test]
    fn reasoning_and_mission_id_omit_when_none() {
        // schema_serialize_omit_when_none-style guarantee for the new
        // schema-1.3 fields. When both are None, the serialized JSON
        // must NOT contain "reasoning":null or "mission_id":null.
        let rec = FlowRecord {
            ts: "2025-01-01T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "test".to_string(),
            handle: "h".to_string(),
            phase_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };
        let serialized = serde_json::to_string(&rec).unwrap();
        assert!(!serialized.contains("reasoning"),
            "absent reasoning leaked into JSON: {serialized}");
        assert!(!serialized.contains("mission_id"),
            "absent mission_id leaked into JSON: {serialized}");
    }

    #[test]
    fn schema_header_contains_version_and_darkmux_version() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("2025-01-01.jsonl");

        record_at(
            &FlowRecord {
                ts: "2025-01-01T00:00:00Z".to_string(),
                level: Level::Info,
                category: Category::Work,
                tier: Tier::Operator,
                stage: Stage::Dispatch,
                action: "init".to_string(),
                handle: "schema-check".to_string(),
                phase_id: None,
                session_id: None,
                source: None,
                model: None,
                reasoning: None,
                mission_id: None,
                machine_id: None,
                machine_uid: None,
                prev_hash: None,
                hash: None,
                payload: None,
                work_id: None,
                attempt: None,
            },
            &path,
        )
        .unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        let header: Value = serde_json::from_str(lines[0]).unwrap();

        assert_eq!(header["version"], FLOW_SCHEMA_VERSION);
        // CARGO_PKG_VERSION is set by cargo; check it's a non-empty string.
        let ver: &str = header["darkmux_version"].as_str().unwrap();
        assert!(!ver.is_empty());
    }

    // ─── Status surface tests (#170) ────────────────────────────────

    #[test]
    fn summarize_sink_flat_local() {
        let info = LocalFileSink::new().info();
        let (kinds, composition) = summarize_sink(&info);
        assert_eq!(kinds, vec!["LocalFile"]);
        assert_eq!(composition, "LocalFile");
    }

    #[test]
    fn summarize_sink_nested_tee() {
        let info = SinkInfo {
            kind: "Tee".to_string(),
            config: Default::default(),
            children: vec![
                LocalFileSink::new().info(),
                SinkInfo {
                    kind: "Redis".to_string(),
                    config: Default::default(),
                    children: vec![],
                    raw_url: None,
                },
            ],
            raw_url: None,
        };
        let (kinds, composition) = summarize_sink(&info);
        assert_eq!(kinds, vec!["LocalFile", "Redis"]);
        assert_eq!(composition, "Tee([LocalFile, Redis])");
    }

    /// (#2093) `build_default_sink()` composes a `HookSink` after Redis when
    /// `DARKMUX_HOOKS_ENABLED` is truthy. Zero configured rules (the
    /// test-build config tier is empty by construction — see #811) means
    /// `HookSink::new` never touches disk beyond spawning its idle drainer
    /// thread, so this is safe to run without a real outbox dir.
    #[serial_test::serial]
    #[test]
    fn build_default_sink_composes_hooks_when_enabled() {
        isolate_test_env_once();
        let prev = std::env::var("DARKMUX_HOOKS_ENABLED").ok();
        unsafe { std::env::set_var("DARKMUX_HOOKS_ENABLED", "true"); }

        let sink = build_default_sink();
        let (kinds, _composition) = summarize_sink(&sink.info());
        assert!(kinds.contains(&"Hooks".to_string()), "kinds: {kinds:?}");
        assert!(kinds.contains(&"LocalFile".to_string()), "LocalFile stays present alongside Hooks");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOOKS_ENABLED", v),
                None => std::env::remove_var("DARKMUX_HOOKS_ENABLED"),
            }
        }
    }

    #[test]
    fn find_redis_cfg_walks_into_tee() {
        // Post-#216: `find_redis_cfg` reads the raw URL from
        // `SinkInfo.raw_url`, not `config["url"]`. `config["url"]` is
        // the redacted display form — a Redis sink without
        // `raw_url` populated is treated as unprobable.
        let info = SinkInfo {
            kind: "Tee".to_string(),
            config: Default::default(),
            children: vec![
                LocalFileSink::new().info(),
                {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert("url".to_string(), "redis://x:***".to_string());
                    m.insert("stream".to_string(), "test:stream".to_string());
                    m.insert("max_len".to_string(), "5000".to_string());
                    SinkInfo {
                        kind: "Redis".to_string(),
                        config: m,
                        children: vec![],
                        raw_url: Some("redis://x:1234".to_string()),
                    }
                },
            ],
            raw_url: None,
        };
        let cfg = find_redis_cfg(&info).expect("redis cfg should be found");
        assert_eq!(cfg.url.expose_for_probe(), "redis://x:1234");
        assert_eq!(cfg.stream, "test:stream");
        assert_eq!(cfg.max_len, Some(5000));
    }

    #[test]
    fn find_redis_cfg_returns_none_when_absent() {
        let info = LocalFileSink::new().info();
        assert!(find_redis_cfg(&info).is_none());
    }

    #[test]
    fn collect_status_produces_serializable_snapshot() {
        // collect_status() reads real env + disk + Redis; we just verify
        // the snapshot serializes round-trip without error. The expensive
        // probes degrade gracefully when their backends are absent.
        let status = collect_status();
        let json = serde_json::to_string(&status).expect("FlowStatus must be serializable");
        let parsed: FlowStatus =
            serde_json::from_str(&json).expect("FlowStatus must round-trip");
        assert_eq!(parsed.schema_version, FLOW_SCHEMA_VERSION);
        assert!(!parsed.sinks.active_kinds.is_empty());
    }

    // ─── Schema 1.4 fields (#167) ─────────────────────────────────────

    #[serial_test::serial]
    #[test]
    fn machine_id_resolves_from_env_var() {
        let prev = env::var("DARKMUX_MACHINE_ID").ok();
        unsafe { env::set_var("DARKMUX_MACHINE_ID", "studio"); }
        assert_eq!(resolve_machine_id().as_deref(), Some("studio"));
        unsafe {
            match prev {
                Some(v) => env::set_var("DARKMUX_MACHINE_ID", v),
                None => env::remove_var("DARKMUX_MACHINE_ID"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn machine_id_env_var_trims_whitespace() {
        let prev = env::var("DARKMUX_MACHINE_ID").ok();
        unsafe { env::set_var("DARKMUX_MACHINE_ID", "  named  "); }
        // Trim leading/trailing whitespace; preserve internal spaces (none here).
        assert_eq!(resolve_machine_id().as_deref(), Some("named"));
        unsafe {
            match prev {
                Some(v) => env::set_var("DARKMUX_MACHINE_ID", v),
                None => env::remove_var("DARKMUX_MACHINE_ID"),
            }
        }
        // The whitespace-only-env fall-through is NOT exercised here:
        // the OnceLock-cached hostname makes the per-test outcome
        // depend on suite ordering. The trim assertion above is the
        // load-bearing behavior; the fall-through is covered indirectly
        // by the doctor check's source labeling.
    }

    #[test]
    fn schema_1_4_fields_omit_when_none() {
        // machine_id must be skip-serialized when None so older viewers
        // can keep parsing without seeing an unexpected `null`.
        let rec = FlowRecord {
            ts: "2026-05-17T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "x".to_string(),
            handle: "y".to_string(),
            phase_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };
        let s = serde_json::to_string(&rec).unwrap();
        assert!(!s.contains("machine_id"), "machine_id should omit when None: {s}");
    }

    #[test]
    fn schema_1_4_fields_round_trip_when_set() {
        let rec = FlowRecord {
            ts: "2026-05-17T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "x".to_string(),
            handle: "y".to_string(),
            phase_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: Some("studio".to_string()),
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };
        let s = serde_json::to_string(&rec).unwrap();
        let parsed: FlowRecord = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed.machine_id.as_deref(), Some("studio"));
    }

    #[serial_test::serial]
    #[test]
    fn record_auto_populates_machine_id() {
        // record() should fill machine_id at write time when the caller
        // leaves it None. The operator-set env value wins over
        // auto-detection so the test can assert a deterministic value
        // regardless of hostname.
        isolate_test_env_once();
        let tmp = TempDir::new().unwrap();
        let prev_flows = env::var("DARKMUX_FLOWS_DIR").ok();
        let prev_machine = env::var("DARKMUX_MACHINE_ID").ok();
        unsafe {
            env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
            env::set_var("DARKMUX_MACHINE_ID", "test-machine");
        }

        let rec = FlowRecord {
            ts: "2026-05-17T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "auto-pop".to_string(),
            handle: "h".to_string(),
            phase_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };
        super::record(rec).unwrap();

        let day = day_utc_now();
        let path = tmp.path().join(format!("{day}.jsonl"));
        let content = std::fs::read_to_string(&path).unwrap();
        // Skip the schema header (line 1); the record is line 2.
        let record_line = content.lines().nth(1).expect("record line");
        let parsed: serde_json::Value = serde_json::from_str(record_line).unwrap();
        assert_eq!(parsed["machine_id"], "test-machine");

        unsafe {
            match prev_flows {
                Some(v) => env::set_var("DARKMUX_FLOWS_DIR", v),
                None => env::remove_var("DARKMUX_FLOWS_DIR"),
            }
            match prev_machine {
                Some(v) => env::set_var("DARKMUX_MACHINE_ID", v),
                None => env::remove_var("DARKMUX_MACHINE_ID"),
            }
        }
    }

    // ─── AuditFileSink (#163) ────────────────────────────────────────

    #[test]
    fn audit_hash_excludes_hash_field() {
        // hash() must NOT include the `hash` field in the input (would
        // be circular). Two records identical except for `hash` should
        // produce the same audit_hash_of() output.
        let base = FlowRecord {
            ts: "2026-05-17T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "x".to_string(),
            handle: "y".to_string(),
            phase_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: Some("seed".to_string()),
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };
        let mut other = base.clone();
        other.hash = Some("anything".to_string());

        let h1 = audit_hash_of(&base).unwrap();
        let h2 = audit_hash_of(&other).unwrap();
        assert_eq!(h1, h2, "hash should not depend on the hash field itself");
    }

    #[test]
    fn audit_hash_changes_when_content_changes() {
        // Sanity: changing ANY chain-bearing field changes the hash.
        let base = FlowRecord {
            ts: "2026-05-17T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "x".to_string(),
            handle: "y".to_string(),
            phase_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: Some("seed".to_string()),
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };
        let h1 = audit_hash_of(&base).unwrap();

        let mut diff_handle = base.clone();
        diff_handle.handle = "z".to_string();
        assert_ne!(audit_hash_of(&diff_handle).unwrap(), h1);

        let mut diff_prev = base.clone();
        diff_prev.prev_hash = Some("different-seed".to_string());
        assert_ne!(audit_hash_of(&diff_prev).unwrap(), h1);

        // PR-A schema 1.8 fields — must each contribute to the hash so a
        // future refactor that accidentally swapped `skip_serializing_if`
        // for `skip` (which omits the field from serialization entirely)
        // can't silently weaken the tamper-evidence invariant. (#246
        // PR-A review M1)
        let mut diff_work_id = base.clone();
        diff_work_id.work_id = Some("1716192000000-0".to_string());
        assert_ne!(
            audit_hash_of(&diff_work_id).unwrap(),
            h1,
            "work_id must contribute to audit hash"
        );

        let mut diff_attempt = base.clone();
        diff_attempt.attempt = Some(2);
        assert_ne!(
            audit_hash_of(&diff_attempt).unwrap(),
            h1,
            "attempt must contribute to audit hash"
        );
    }

    /// Cross-version audit-chain walk: records that lack the schema-1.8
    /// fields (work_id / attempt) must still validate
    /// under 1.8 reader code. The invariant rides on
    /// `skip_serializing_if = "Option::is_none"` — re-serialization of
    /// a None-valued field produces the same bytes a pre-1.8 writer
    /// would have produced, so the hash chain walks cleanly across the
    /// version boundary. (#246 PR-A review M2)
    #[serial_test::serial]
    #[test]
    fn integrity_walks_pre_1_8_records() {
        let tmp = TempDir::new().unwrap();
        let prev_audit = env::var("DARKMUX_AUDIT_DIR").ok();
        unsafe { env::set_var("DARKMUX_AUDIT_DIR", tmp.path()); }

        // Write records with all new schema-1.8 fields explicitly None.
        // The on-disk JSON lines omit those keys (skip_serializing_if),
        // which is byte-identical to what a pre-1.8 writer produced.
        let sink = AuditFileSink::new();
        for i in 0..3u32 {
            let rec = FlowRecord {
                ts: format!("2026-05-15T00:00:0{i}Z"),
                level: Level::Info,
                category: Category::Work,
                tier: Tier::Operator,
                stage: Stage::Scope,
                action: "pre-1.8-record".to_string(),
                handle: format!("h-{i}"),
                phase_id: None,
                session_id: None,
                source: None,
                model: None,
                reasoning: None,
                mission_id: None,
                machine_id: None,
                machine_uid: None,
                prev_hash: None,
                hash: None,
                payload: None,
                work_id: None,
                attempt: None,
            };
            sink.write(&rec).unwrap();
        }

        // Confirm the on-disk JSON does NOT carry the new keys — that's
        // the "pre-1.8 shape" assertion.
        let day = day_utc_now();
        let path = tmp.path().join(format!("{day}.jsonl"));
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("\"work_id\""),
            "None-valued work_id must be omitted"
        );
        assert!(
            !raw.contains("\"attempt\""),
            "None-valued attempt must be omitted"
        );

        // The chain walks cleanly — same invariant as a real pre-1.8 file
        // produced by an older darkmux build.
        let report = integrity_check_file(&path).unwrap();
        assert!(
            report.chain_valid,
            "cross-version chain must validate; reason: {report:?}"
        );
        assert_eq!(report.records_checked, 3);

        unsafe {
            match prev_audit {
                Some(v) => env::set_var("DARKMUX_AUDIT_DIR", v),
                None => env::remove_var("DARKMUX_AUDIT_DIR"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn audit_file_sink_writes_chained_records() {
        let tmp = TempDir::new().unwrap();
        let prev_audit = env::var("DARKMUX_AUDIT_DIR").ok();
        unsafe { env::set_var("DARKMUX_AUDIT_DIR", tmp.path()); }

        let sink = AuditFileSink::new();
        for i in 0..3u32 {
            let rec = FlowRecord {
                ts: format!("2026-05-17T00:00:0{i}Z"),
                level: Level::Info,
                category: Category::Work,
                tier: Tier::Operator,
                stage: Stage::Scope,
                action: format!("audit-{i}"),
                handle: format!("rec-{i}"),
                phase_id: None,
                session_id: None,
                source: None,
                model: None,
                reasoning: None,
                mission_id: None,
                machine_id: None,
                machine_uid: None,
                prev_hash: None, // sink stamps this
                hash: None,      // sink stamps this
                payload: None,
                work_id: None,
                attempt: None,
            };
            sink.write(&rec).unwrap();
        }

        // Walk the file we just produced.
        let day = day_utc_now();
        let path = tmp.path().join(format!("{day}.jsonl"));
        let report = integrity_check_file(&path).unwrap();
        assert!(report.chain_valid, "chain should validate; reason: {report:?}");
        assert_eq!(report.records_checked, 3);

        unsafe {
            match prev_audit {
                Some(v) => env::set_var("DARKMUX_AUDIT_DIR", v),
                None => env::remove_var("DARKMUX_AUDIT_DIR"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn integrity_check_detects_edited_record() {
        let tmp = TempDir::new().unwrap();
        let prev_audit = env::var("DARKMUX_AUDIT_DIR").ok();
        unsafe { env::set_var("DARKMUX_AUDIT_DIR", tmp.path()); }

        let sink = AuditFileSink::new();
        for i in 0..3u32 {
            let rec = FlowRecord {
                ts: format!("2026-05-17T00:00:0{i}Z"),
                level: Level::Info,
                category: Category::Work,
                tier: Tier::Operator,
                stage: Stage::Scope,
                action: format!("audit-{i}"),
                handle: format!("rec-{i}"),
                phase_id: None,
                session_id: None,
                source: None,
                model: None,
                reasoning: None,
                mission_id: None,
                machine_id: None,
                machine_uid: None,
                prev_hash: None,
                hash: None,
                payload: None,
                work_id: None,
                attempt: None,
            };
            sink.write(&rec).unwrap();
        }

        let day = day_utc_now();
        let path = tmp.path().join(format!("{day}.jsonl"));

        // Tamper: replace one record's handle inline. The hash should
        // no longer match the content.
        let contents = std::fs::read_to_string(&path).unwrap();
        let tampered = contents.replace("rec-1", "rec-1-EDITED");
        std::fs::write(&path, tampered).unwrap();

        let report = integrity_check_file(&path).unwrap();
        assert!(!report.chain_valid, "tampered record should break the chain");
        assert!(report.break_at_line.is_some());

        unsafe {
            match prev_audit {
                Some(v) => env::set_var("DARKMUX_AUDIT_DIR", v),
                None => env::remove_var("DARKMUX_AUDIT_DIR"),
            }
        }
    }

    /// (#1769) THE INVERTED CASE — mandatory alongside the FN-1 fix below. A
    /// record from a genuinely NEWER peer, carrying an enum spelling this
    /// binary does not recognize, must VALIDATE CLEANLY. Without this test, a
    /// future "fix" for FN-1 could simply reject every record with an
    /// unrecognized enum value — which would look like a fix (FN-1 passes)
    /// while actually regressing #1611/#1617's forward-compat guarantee. The
    /// correct behavior is neither "trust it blindly" (FN-1) nor "reject it
    /// outright" (this test) — it's "hash it like everything else," because
    /// under byte-hashing an unrecognized spelling is just more bytes.
    ///
    /// Superseded here: `a_newer_peers_record_reads_as_unverifiable_not_as_
    /// tampering` (#1611/#1617), which pinned the OLD struct-hash format's
    /// `unverifiable_newer` bypass — the mechanism #1769 deletes because it
    /// is the same mechanism FN-1 exploits. Under byte-hashing there is no
    /// bypass to pin; there is only "does it hash correctly," which is what
    /// this test asserts.
    #[serial_test::serial]
    #[test]
    fn a_newer_peers_record_with_an_unknown_enum_validates_cleanly() {
        let tmp = TempDir::new().unwrap();
        let prev_audit = env::var("DARKMUX_AUDIT_DIR").ok();
        unsafe { env::set_var("DARKMUX_AUDIT_DIR", tmp.path()); }

        let sink = AuditFileSink::new();
        for i in 0..3u32 {
            let rec = FlowRecord {
                ts: format!("2026-08-03T00:00:0{i}Z"),
                level: Level::Info,
                category: Category::Work,
                tier: Tier::Operator,
                stage: Stage::Scope,
                action: format!("audit-{i}"),
                handle: format!("rec-{i}"),
                phase_id: None,
                session_id: None,
                source: None,
                model: None,
                reasoning: None,
                mission_id: None,
                machine_id: None,
                machine_uid: None,
                prev_hash: None,
                hash: None,
                payload: None,
                work_id: None,
                attempt: None,
            };
            sink.write(&rec).unwrap();
        }

        let day = day_utc_now();
        let path = tmp.path().join(format!("{day}.jsonl"));
        let contents = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<String> = contents.lines().map(String::from).collect();

        // Rewrite the middle record the way a NEWER binary genuinely would:
        // an enum spelling this binary does not know, correctly hashed over
        // THOSE literal bytes (a real writer always hashes what it actually
        // wrote). Line 0 is the schema header, so record 1 is at index 2.
        let (_old_hash, record_json) = lines[2].split_once(' ').expect(
            "a byte-hash-format record line must have a `<hash> <json>` prefix",
        );
        let newer_json = record_json.replace("\"level\":\"info\"", "\"level\":\"catastrophe\"");
        assert_ne!(newer_json, record_json, "the newer spelling must actually land");
        let newer_hash = crate::integrity::audit_hash_of_bytes(newer_json.as_bytes());
        lines[2] = format!("{newer_hash} {newer_json}");

        // Re-link the tail, because a real newer writer would have. Rewriting
        // one record's bytes changes its hash, so the NEXT record's
        // `prev_hash` (baked into ITS bytes) would otherwise point at a hash
        // that no longer exists — a genuine chain break, which would make
        // this test assert the wrong thing. The records after the rewritten
        // one carry no unknown values, so parsing them through `FlowRecord`
        // to relink is lossless.
        let mut prev = newer_hash.clone();
        for line in lines.iter_mut().skip(3) {
            let (_, json) = line.split_once(' ').expect("record line must have a hash prefix");
            let mut rec: FlowRecord = serde_json::from_str(json).unwrap();
            rec.prev_hash = Some(prev.clone());
            rec.hash = None;
            let rebuilt = serde_json::to_string(&rec).unwrap();
            let h = crate::integrity::audit_hash_of_bytes(rebuilt.as_bytes());
            *line = format!("{h} {rebuilt}");
            prev = h;
        }
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();

        let report = integrity_check_file(&path).unwrap();

        assert!(
            report.chain_valid,
            "an untouched newer-peer record — unrecognized enum spelling and all — must \
             validate cleanly under byte-hashing; got {report:?}"
        );
        assert!(
            report.break_reason.is_none(),
            "nothing was tampered with — reporting a reason at all trains the operator to \
             dismiss the real thing; got {:?}",
            report.break_reason
        );
        assert!(!report.legacy_format);
        assert_eq!(report.records_checked, 3);

        unsafe {
            match prev_audit {
                Some(v) => env::set_var("DARKMUX_AUDIT_DIR", v),
                None => env::remove_var("DARKMUX_AUDIT_DIR"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn audit_file_sink_recovers_from_header_only_file() {
        // OS-crash-between-header-and-first-record recovery: a file
        // containing ONLY the schema header should not break the next
        // write. The sink should seed the chain from the existing header
        // (NOT re-emit it) and append the first record successfully.
        let tmp = TempDir::new().unwrap();
        let prev_audit = env::var("DARKMUX_AUDIT_DIR").ok();
        unsafe { env::set_var("DARKMUX_AUDIT_DIR", tmp.path()); }

        let day = day_utc_now();
        let path = tmp.path().join(format!("{day}.jsonl"));
        // Simulate the crash state: header line only, no records. Must be
        // the AUDIT header (carries the byte-hash format marker, #1769) —
        // the plain `schema_header_line()` LocalFileSink uses would look
        // like a legacy audit file and refuse to extend.
        let header = crate::integrity::audit_schema_header_line().unwrap();
        std::fs::write(&path, format!("{header}\n")).unwrap();

        let sink = AuditFileSink::new();
        let rec = FlowRecord {
            ts: "2026-05-17T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "post-recovery".to_string(),
            handle: "h".to_string(),
            phase_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };
        sink.write(&rec).expect("recovery should not bail");

        // File should now have: header (line 1) + one record (line 2).
        let report = integrity_check_file(&path).unwrap();
        assert!(report.chain_valid, "post-recovery chain should validate: {report:?}");
        assert_eq!(report.records_checked, 1);

        let contents = std::fs::read_to_string(&path).unwrap();
        let line_count = contents.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(line_count, 2, "should have exactly header + one record");

        unsafe {
            match prev_audit {
                Some(v) => env::set_var("DARKMUX_AUDIT_DIR", v),
                None => env::remove_var("DARKMUX_AUDIT_DIR"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn integrity_check_empty_file_passes() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("empty.jsonl");
        std::fs::write(&path, "").unwrap();
        let report = integrity_check_file(&path).unwrap();
        assert!(report.chain_valid);
        assert_eq!(report.records_checked, 0);
    }

    #[serial_test::serial]
    #[test]
    fn audit_file_sink_recovers_chain_across_process_boundaries() {
        // Two sink instances writing to the same file must produce a
        // chain that validates. Simulates two CLI sessions (without
        // actually forking — the flock + filesystem state covers it).
        let tmp = TempDir::new().unwrap();
        let prev_audit = env::var("DARKMUX_AUDIT_DIR").ok();
        unsafe { env::set_var("DARKMUX_AUDIT_DIR", tmp.path()); }

        let sink_a = AuditFileSink::new();
        let sink_b = AuditFileSink::new();

        let mk = |handle: &str| FlowRecord {
            ts: "2026-05-17T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Scope,
            action: "x".to_string(),
            handle: handle.to_string(),
            phase_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };

        sink_a.write(&mk("a1")).unwrap();
        sink_b.write(&mk("b1")).unwrap();
        sink_a.write(&mk("a2")).unwrap();

        let day = day_utc_now();
        let path = tmp.path().join(format!("{day}.jsonl"));
        let report = integrity_check_file(&path).unwrap();
        assert!(report.chain_valid, "alternating sinks should still form a valid chain: {report:?}");
        assert_eq!(report.records_checked, 3);

        unsafe {
            match prev_audit {
                Some(v) => env::set_var("DARKMUX_AUDIT_DIR", v),
                None => env::remove_var("DARKMUX_AUDIT_DIR"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn audit_dir_respects_env_override() {
        let prev = std::env::var("DARKMUX_AUDIT_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_AUDIT_DIR", "/tmp/dm-audit-test"); }
        assert_eq!(audit_dir(), std::path::PathBuf::from("/tmp/dm-audit-test"));
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_AUDIT_DIR", v),
                None => std::env::remove_var("DARKMUX_AUDIT_DIR"),
            }
        }
    }

    /// (#2359) `audit_dir`'s default must scope under `DARKMUX_HOME`, same
    /// as `flows_dir` — mirrors
    /// `darkmux_types::config_access::tests::flows_dir_honors_darkmux_home`.
    /// Before this fix `audit_dir_default` went straight to
    /// `dirs::home_dir()`, so a `DARKMUX_HOME`-scoped install with
    /// `audit.enabled` but no `DARKMUX_AUDIT_DIR`/`audit.dir` override would
    /// still write the hash-chained audit trail to the operator's real
    /// `~/.darkmux/audit`.
    #[test]
    #[serial_test::serial]
    fn audit_dir_honors_darkmux_home() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_home = std::env::var("DARKMUX_HOME").ok();
        let prev_audit = std::env::var("DARKMUX_AUDIT_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_AUDIT_DIR");
            std::env::set_var("DARKMUX_HOME", tmp.path());
        }
        let dir = audit_dir();
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
            match prev_audit {
                Some(v) => std::env::set_var("DARKMUX_AUDIT_DIR", v),
                None => std::env::remove_var("DARKMUX_AUDIT_DIR"),
            }
        }
        assert_eq!(
            dir,
            tmp.path().join("audit"),
            "must scope under DARKMUX_HOME, not the real user home"
        );
    }

    #[test]
    fn assemble_redis_url_places_password_and_db() {
        // password-less, no db
        assert_eq!(assemble_redis_url("h", 6379, None, None), "redis://h:6379");
        // password as `:<pw>@` (empty user) — and redact masks it to `:***@`
        let pw = assemble_redis_url("h", 6379, None, Some("secret"));
        assert_eq!(pw, "redis://:secret@h:6379");
        assert_eq!(redact_url_creds(&pw), "redis://:***@h:6379");
        // db suffix
        assert_eq!(assemble_redis_url("h", 6380, Some(2), None), "redis://h:6380/2");
        assert_eq!(assemble_redis_url("h", 6380, Some(2), Some("p")), "redis://:p@h:6380/2");
        // A non-URL-safe password (`@`/`/`/`&` in it) is percent-encoded so it
        // can't split the URL or escape redaction — the whole secret stays in
        // the masked userinfo (audit LOW).
        let at = assemble_redis_url("h", 6379, None, Some("p@ss/w&rd"));
        assert_eq!(at, "redis://:p%40ss%2Fw%26rd@h:6379");
        assert!(!at.contains("p@ss"), "raw `@` must not appear in the assembled URL");
        assert_eq!(redact_url_creds(&at), "redis://:***@h:6379", "no tail leaks past redaction");
    }

    #[test]
    fn raw_redis_url_debug_is_redacted() {
        // The `Debug` impl must NOT expose the raw password (audit MEDIUM) —
        // `{:?}` is as safe as `{}`.
        let u = RawRedisUrl::new("redis://:supersecret@h:6379".to_string());
        let dbg = format!("{u:?}");
        assert!(!dbg.contains("supersecret"), "Debug leaked the password: {dbg}");
        assert!(dbg.contains("***"), "Debug should show the redacted form: {dbg}");
    }

    #[serial_test::serial]
    #[test]
    fn redis_url_env_tier_verbatim_then_off() {
        // (#811) Neutralize the config tier so the "off" assertion holds on a
        // machine whose real config.json has `redis.enabled: true` (it flaked
        // there before — the config tier kept Redis on after the env scrub).
        isolate_test_env_once();
        let prev = std::env::var("DARKMUX_REDIS_URL").ok();
        // Tier 1: env verbatim — raw via expose_for_probe, redacted on Display.
        unsafe { std::env::set_var("DARKMUX_REDIS_URL", "redis://:hunter2@h:6379/0"); }
        let u = redis_url().expect("env tier → Some");
        assert_eq!(u.expose_for_probe(), "redis://:hunter2@h:6379/0");
        assert_eq!(u.to_string(), "redis://:***@h:6379/0", "Display redacts");
        // Tier 3: unset env + (in CI) no config.redis → None (off).
        unsafe { std::env::remove_var("DARKMUX_REDIS_URL"); }
        assert!(redis_url().is_none(), "no env + no config → Redis off");
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_REDIS_URL", v),
                None => std::env::remove_var("DARKMUX_REDIS_URL"),
            }
        }
    }

    #[test]
    fn raw_serve_token_redacts() {
        // (#881) The token must NEVER appear in `{:?}` or `{}` — only via the
        // explicit, verbosely-named `expose_for_compare`.
        let t = RawServeToken::new("sk-super-secret".to_string());
        assert!(!format!("{t:?}").contains("sk-super-secret"), "Debug leaked the token");
        assert!(format!("{t:?}").contains("***"), "Debug should redact");
        assert!(!format!("{t}").contains("sk-super-secret"), "Display leaked the token");
        assert_eq!(t.expose_for_compare(), "sk-super-secret");
    }

    #[serial_test::serial]
    #[test]
    fn serve_token_env_tier_then_off() {
        // (#811) Empty config tier under test, so the token resolves ONLY from
        // the env var (tier 1); tier 2 (config gate + Keychain) is off.
        isolate_test_env_once();
        let prev = std::env::var("DARKMUX_SERVE_TOKEN").ok();
        // Tier 1: env token, trimmed.
        unsafe { std::env::set_var("DARKMUX_SERVE_TOKEN", "  tok-xyz  "); }
        let t = serve_token().expect("env tier → Some");
        assert_eq!(t.expose_for_compare(), "tok-xyz", "surrounding whitespace trimmed");
        assert!(serve_token_present());
        // Tier 3: unset env + empty config gate → None (auth off).
        unsafe { std::env::remove_var("DARKMUX_SERVE_TOKEN"); }
        assert!(serve_token().is_none(), "no env + gate off → token absent");
        assert!(!serve_token_present());
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_SERVE_TOKEN", v),
                None => std::env::remove_var("DARKMUX_SERVE_TOKEN"),
            }
        }
    }

    #[test]
    fn redact_url_creds_masks_password() {
        assert_eq!(
            redact_url_creds("redis://kain:hunter2@redis.example.com:6379/0"),
            "redis://kain:***@redis.example.com:6379/0"
        );
        // Password-only userinfo (empty user) — still mask the password.
        assert_eq!(
            redact_url_creds("redis://:onlypass@host:6379"),
            "redis://:***@host:6379"
        );
        // Username-only (no colon) — leave as-is (no secret to hide).
        assert_eq!(
            redact_url_creds("redis://user@host:6379"),
            "redis://user@host:6379"
        );
        // No creds at all — unchanged.
        assert_eq!(
            redact_url_creds("redis://127.0.0.1:6379"),
            "redis://127.0.0.1:6379"
        );
        // Non-URL string — returned verbatim, no panic.
        assert_eq!(redact_url_creds("garbage"), "garbage");
    }

    /// A raw (unencoded) `@` inside the password. Unreachable from the
    /// Tier-2 assembled path (`assemble_redis_url` percent-encodes), but the
    /// Tier-1 `DARKMUX_REDIS_URL` path is documented VERBATIM — byte-for-byte
    /// unchanged — and cloud Redis providers generate `@`-bearing passwords.
    /// The redis crate parses the LAST `@` as the userinfo/host boundary and
    /// connects fine; the old first-`@` redaction disagreed with that parse
    /// and echoed everything after the password's first `@` in clear:
    /// `redis://:***@secretpw@real.host:6379/0`. A redactor that disagrees
    /// with the connection parser leaks exactly the disagreement.
    #[test]
    fn redact_url_creds_masks_a_password_containing_at() {
        assert_eq!(
            redact_url_creds("redis://:my@secretpw@real.host:6379/0"),
            "redis://:***@real.host:6379/0",
            "everything left of the LAST authority `@` is password"
        );
        assert_eq!(
            redact_url_creds("redis://user:p@ss@w@rd@h:6379"),
            "redis://user:***@h:6379",
            "multiple `@`s — only the final one is the boundary"
        );
        // An `@` in the PATH is data, not a boundary — the authority is
        // bounded at the first `/` so redaction neither mis-splits on it
        // nor swallows the path.
        assert_eq!(
            redact_url_creds("redis://user:pw@h:6379/queue@2"),
            "redis://user:***@h:6379/queue@2"
        );
        // No `@` in the authority but one in the path: no userinfo exists,
        // and the path `@` must not conjure one (the old code split on it
        // and mangled the URL).
        assert_eq!(
            redact_url_creds("redis://h:6379/queue@2"),
            "redis://h:6379/queue@2"
        );
    }

    #[test]
    fn sink_init_banner_carries_no_url_at_all() {
        // Regression for #213 (password) and #1955 (host).
        //
        // #213 redacted the password out of this banner and kept the rest of
        // the URL. That was the right fix for a credential and the wrong
        // stopping point for an ADDRESS: `redact_url_creds` passes host:port
        // through by design, so the banner printed the operator's tailnet
        // address on every single dispatch — on a machine whose repo history
        // had been scrubbed of precisely those addresses (#1940).
        //
        // The banner now carries no URL at all, so there is nothing left to
        // redact. Pinned as an ABSENCE rather than a redaction marker: a
        // future refactor that reintroduces `url={...}` fails here even if it
        // routes through the redacting form.
        let banner = format!(
            "flow: Redis sink enabled — stream={} max_len={:?} (composed via TeeSink)",
            "darkmux:flow",
            Some(10000_usize),
        );
        assert!(
            !banner.contains("redis://") && !banner.contains('@'),
            "banner must carry no URL: {banner}",
        );
        assert!(
            !banner.contains("url="),
            "banner must not reintroduce a url field, redacted or otherwise: {banner}",
        );
        assert!(
            banner.contains("stream=darkmux:flow"),
            "banner must still say which stream came up: {banner}",
        );
    }

    #[test]
    fn redis_sink_error_context_redacts_password() {
        // Regression for #213: `RedisSink::new` and `RedisSink::connect`
        // wrap their inner errors with `with_context` strings that
        // formerly embedded the raw URL. Both now route through
        // `redact_url_creds`. We exercise the format strings directly to
        // pin the contract — a future refactor that drops the redactor
        // call would resurrect the leak.
        let url = "redis://:supersecret@127.0.0.1:1/0";
        let open_ctx = format!("opening Redis connection to {}", redact_url_creds(url));
        let connect_ctx = format!("connecting to Redis at {}", redact_url_creds(url));
        for ctx in [&open_ctx, &connect_ctx] {
            assert!(
                !ctx.contains("supersecret"),
                "error context leaked password: {ctx}",
            );
            assert!(
                ctx.contains(":***@"),
                "error context missed redaction marker: {ctx}",
            );
        }
    }

    #[test]
    fn redis_sink_info_redacts_url_in_serialized_json() {
        // Regression for #216: `SinkInfo.config["url"]` previously carried
        // the raw `DARKMUX_REDIS_URL` value through to JSON consumers
        // — `darkmux flow status --json` and the daemon's CORS-permissive
        // HTTP endpoint. The raw URL now lives on `SinkInfo.raw_url`
        // (skip-serialized); `config["url"]` is the redacted display form.
        let sink = RedisSink::new(
            "redis://:supersecret@100.64.0.2:6379",
            "darkmux:flow",
            Some(10000),
        )
        .expect("RedisSink::new on a syntactically valid URL");
        let info = sink.info();

        // In-process path keeps the raw URL.
        assert_eq!(
            info.raw_url.as_deref(),
            Some("redis://:supersecret@100.64.0.2:6379"),
            "raw_url must round-trip the unredacted URL for the probe path",
        );

        // Display path strips it.
        assert_eq!(
            info.config.get("url").map(String::as_str),
            Some("redis://:***@100.64.0.2:6379"),
            "config[\"url\"] must be redacted",
        );

        // Serializing the SinkInfo (the exact path used by FlowStatus →
        // JSON output → daemon HTTP) must not contain the password.
        let json = serde_json::to_string(&info).expect("serialize SinkInfo");
        assert!(
            !json.contains("supersecret"),
            "serialized SinkInfo leaked password: {json}",
        );
        assert!(
            json.contains(":***@"),
            "serialized SinkInfo missed redaction marker: {json}",
        );
        assert!(
            !json.contains("raw_url"),
            "raw_url field must be skip-serialized (no key in JSON): {json}",
        );
    }

    #[test]
    fn find_redis_cfg_recovers_raw_url_from_redis_sink_info() {
        // Regression for #216: the probe path (`find_redis_cfg` →
        // `probe_redis` → `redis::Client::open`) must still see the raw
        // URL after #216 moved it off `config["url"]`. Round-trip:
        //   RedisSink::new(raw) → info() → find_redis_cfg → cfg.url == raw
        let raw = "redis://:hunter2@127.0.0.1:6379/0";
        let sink = RedisSink::new(raw, "darkmux:flow", Some(10000))
            .expect("RedisSink::new on a syntactically valid URL");
        let info = sink.info();
        let cfg = find_redis_cfg(&info).expect("Redis sink should resolve to a cfg");
        assert_eq!(cfg.url.expose_for_probe(), raw, "probe path must receive the raw URL");
        assert_eq!(cfg.stream, "darkmux:flow");
        assert_eq!(cfg.max_len, Some(10000));
    }

    /// #278 — `RedisSink::write` against an unresponsive Redis URL
    /// MUST bail within a bounded time, NOT wait the OS default 75s
    /// TCP-connect-or-handshake timeout. With the operator's normal-
    /// shell env var `DARKMUX_REDIS_URL` pointing at an offline peer
    /// (e.g. Studio during the 2026-05-21 incident), every flow-record
    /// write was wedging tests for 75s/record — the canonical 51-minute
    /// `cargo test` debacle from 2026-05-22.
    ///
    /// Repro: spawn a TCP listener that accepts connections but NEVER
    /// reads/writes (no SYN-ACK refusal; no handshake response). The
    /// pre-fix `get_connection()` hangs at the post-connect handshake
    /// step waiting for Redis's AUTH/INFO response. The post-fix
    /// `get_connection_with_timeout(REDIS_CONNECT_TIMEOUT)` bails at
    /// the timeout regardless of which phase is stuck.
    ///
    /// Contract: a single `.write()` against an unresponsive listener
    /// completes within 3 seconds (gives ~6× headroom on the new
    /// 500ms connect+handshake timeout for slow-loopback test runners).
    #[test]
    fn redis_sink_write_against_unresponsive_listener_completes_within_bounded_time() {
        // Spawn a TCP listener that accepts but never responds.
        // Mimics the Studio-offline failure mode: the network path
        // exists, the connect succeeds at the TCP layer, but no
        // Redis handshake response ever comes back.
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        // Background thread: accept the connection then hang on it
        // (drop the stream when test ends).
        std::thread::spawn(move || {
            // accept() blocks; one accept is enough for this test.
            // Leak the accepted stream — test will drop the listener.
            if let Ok((stream, _)) = listener.accept() {
                std::thread::sleep(std::time::Duration::from_secs(60));
                drop(stream);
            }
        });
        // Give the listener a beat to be ready.
        std::thread::sleep(std::time::Duration::from_millis(50));

        let url = format!("redis://127.0.0.1:{port}");
        let sink = RedisSink::new(&url, "darkmux:flow", Some(10000))
            .expect("RedisSink::new on a syntactically valid URL");

        let rec = FlowRecord {
            ts: ts_utc_now(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Local,
            stage: Stage::Dispatch,
            action: "test-unresponsive-redis".to_string(),
            handle: "test".to_string(),
            phase_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        };

        let start = std::time::Instant::now();
        let result = sink.write(&rec);
        let elapsed = start.elapsed();

        // (#388) write() is now best-effort: it swallows the underlying
        // failure (returns Ok) and accounts it toward the disable
        // threshold rather than propagating to the TeeSink (which is what
        // produced the per-write log spam). The value this test guards is
        // the bounded WALL-CLOCK of the underlying connect attempt — a
        // single failing write must still return within the connect-
        // timeout budget, not hang. The sink hasn't hit the disable
        // threshold after one failure, so the connection WAS attempted.
        assert!(
            result.is_ok(),
            "write is best-effort and swallows the failure (#388); got {result:?}"
        );
        assert!(
            !sink.is_disabled(),
            "one failure must not yet disable the sink (threshold is 3)"
        );
        // Bound is `REDIS_CONNECT_TIMEOUT * 2` (1000ms from the
        // wall-clock wrapper) + small slack for thread spawn +
        // mpsc plumbing. 1500ms is 50% headroom on the named contract;
        // a regression that bumps REDIS_CONNECT_TIMEOUT beyond ~700ms
        // will fail this test — the right behavior for "we changed
        // the connect budget without thinking about per-write wall."
        assert!(
            elapsed < std::time::Duration::from_millis(1500),
            "RedisSink::write against unresponsive listener took {elapsed:?}; \
             expected < 1500ms (REDIS_CONNECT_TIMEOUT * 2 + slack). \
             Was effectively unbounded before #278's connect-timeout fix. \
             This is the substrate test for the Studio-offline scenario."
        );
    }

    /// (#2227) A peer that COMPLETES the Redis handshake and then never
    /// answers the `XADD` must make the write fail as an `Err`, bounded by
    /// `REDIS_RESPONSE_TIMEOUT` — not hang forever.
    ///
    /// Distinct from the #278 test above, which covers the CONNECT phase.
    /// `REDIS_CONNECT_TIMEOUT` bounds connect+handshake only; before this fix
    /// the post-handshake command rode an unbounded socket, so the
    /// accepts-but-never-answers peer measured live on 2026-07-29 (a Tailscale
    /// peer accepting TCP but silent at the Redis layer) wedged the CALLER.
    ///
    /// Why that is worse than degraded observability: a hang is not an `Err`,
    /// so `note_failure` never runs and `REDIS_DISABLE_THRESHOLD` never trips
    /// — the sink can never self-disable. And every flow-record emit is
    /// synchronous on the dispatch thread, so a wedged `XADD` stops the
    /// trajectory-tailer loop that applies the inactivity-deadline resets and
    /// the host watchdog SIGKILLs a fully productive dispatch.
    ///
    /// The fake peer ([`spawn_silent_redis_peer`]) replies to the two `CLIENT
    /// SETINFO` commands redis-rs 0.27 pipelines at connection setup
    /// (`connection_setup_pipeline`), so the handshake SUCCEEDS, then goes
    /// silent — which is the only way to reach the command phase at all.
    ///
    /// That handshake reply count is a LOAD-BEARING and FRAGILE assumption, so
    /// the assertions below check the error came from the `XADD` by name. Round
    /// 1 shipped this test without that check and it passed vacuously: change
    /// the peer's reply to a single `+OK` (which is also what a redis-rs bump
    /// altering the setup pipeline, or an operator URL carrying a password or
    /// `db != 0`, does) and the handshake never completes, `try_write` fails at
    /// CONNECT, and `is_err` + the elapsed bounds + `is_disabled` ALL still
    /// hold — the test silently degenerates into a duplicate of the #278
    /// connect-phase test above while reading as #2227 coverage.
    #[test]
    fn redis_sink_xadd_against_silent_peer_errs_and_trips_the_disable_threshold() {
        // Every `try_write` opens its own connection, so the four writes below
        // need four accepted sockets.
        let port = spawn_silent_redis_peer(6);

        let url = format!("redis://127.0.0.1:{port}");
        let sink = RedisSink::new(&url, "darkmux:flow", Some(10000))
            .expect("RedisSink::new on a syntactically valid URL");
        let rec = silent_peer_test_record();

        // (1) The fallible inner write must SURFACE the stall as an `Err`.
        // A silent skip here is what let the disable machinery starve.
        let start = std::time::Instant::now();
        let inner = sink.try_write(&rec);
        let inner_elapsed = start.elapsed();
        let inner_err = inner.expect_err(
            "try_write against a handshake-completing, command-silent peer must \
             return Err (that is what feeds REDIS_DISABLE_THRESHOLD); got Ok",
        );
        // THE anti-vacuity assertion. `XADD` appears only in `try_write`'s
        // post-handshake context (`XADD to Redis stream \`<stream>\``), so this
        // is what distinguishes "the command timed out" — the #2227 bug — from
        // "the connect failed", which the #278 test above already covers and
        // which every other assertion in this test accepts silently.
        let inner_msg = format!("{inner_err:#}");
        assert!(
            inner_msg.contains("XADD"),
            "expected the COMMAND phase to fail (an `XADD ...` context), which \
             is the only thing that exercises #2227's socket deadline. Got \
             {inner_msg} — a connect-phase failure here means the fake peer no \
             longer completes redis-rs's setup pipeline and this test is \
             passing vacuously."
        );
        assert!(
            inner_elapsed < std::time::Duration::from_secs(3),
            "try_write took {inner_elapsed:?}; expected bounded by \
             REDIS_RESPONSE_TIMEOUT (1s) + slack. Unbounded before #2227."
        );

        // (2) The disable path: REDIS_DISABLE_THRESHOLD consecutive `write`s
        // must flip the sink off, exactly as a connect-phase failure does.
        // And every one of them returns Ok — a flow-record write is
        // best-effort and must never propagate a failure into the dispatch.
        let start = std::time::Instant::now();
        for i in 0..REDIS_DISABLE_THRESHOLD {
            assert!(
                sink.write(&rec).is_ok(),
                "write #{i} must stay best-effort (Ok) — a flow-record write \
                 must never fail a dispatch"
            );
        }
        let elapsed = start.elapsed();
        assert!(
            sink.is_disabled(),
            "after {REDIS_DISABLE_THRESHOLD} consecutive command-timeout writes \
             the sink must self-disable; before #2227 the hang was not an Err so \
             the counter never advanced"
        );
        // (3) (#2227) The self-disabled state must be VISIBLE. `sink_info`
        // feeds `darkmux flow status --json` and the daemon endpoint; without
        // this field a permanently-disabled sink reported as healthy in the
        // only two places an operator would look, and the one-time stderr
        // warning is long gone by then.
        assert_eq!(
            sink.info().config.get("disabled").map(String::as_str),
            Some("true"),
            "a self-disabled sink must say so in `sink_info` — the operator \
             never has to wonder where a decision came from (#44)"
        );
        let healthy = RedisSink::new("redis://127.0.0.1:1", "darkmux:flow", None).unwrap();
        assert_eq!(
            healthy.info().config.get("disabled").map(String::as_str),
            Some("false"),
            "the field must be present-and-false on a healthy sink, not merely \
             absent — an absent key reads as 'this build predates the field'"
        );
        // Hard upper bound so a regression FAILS FAST instead of hanging CI
        // forever. Budget: 3 writes x (connect + REDIS_RESPONSE_TIMEOUT).
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "{REDIS_DISABLE_THRESHOLD} bounded writes took {elapsed:?}; expected \
             well under 10s. An unbounded command socket hangs here forever."
        );
    }

    /// (#2227) `RedisSink::connect` exists to hand DIAGNOSTICS a connection to
    /// the same Redis the sink writes to — and diagnostics are what an operator
    /// runs against a peer that has stopped answering. The connection it hands
    /// out must therefore arrive with its command deadline already set, so an
    /// out-of-crate caller cannot reintroduce the unbounded shape by
    /// forgetting.
    #[test]
    fn redis_sink_connect_hands_out_a_command_bounded_connection() {
        let port = spawn_silent_redis_peer(2);
        let sink = RedisSink::new(&format!("redis://127.0.0.1:{port}"), "darkmux:flow", None)
            .expect("RedisSink::new on a syntactically valid URL");

        // The connect itself must SUCCEED — the peer completes the handshake.
        // If this fails, everything below is a vacuous connect-phase test.
        let mut conn = sink.connect().expect(
            "the fake peer completes redis-rs's setup pipeline, so connect must \
             succeed; a failure here means this test degenerated into a \
             connect-phase (#278) duplicate",
        );

        let start = std::time::Instant::now();
        let res: redis::RedisResult<String> = redis::cmd("PING").query(&mut conn);
        let elapsed = start.elapsed();

        assert!(res.is_err(), "a command-silent peer must surface as Err, not block");
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "a command on a connection from `connect()` took {elapsed:?}; \
             expected bounded by REDIS_RESPONSE_TIMEOUT (1s). Unbounded before \
             #2227."
        );
    }

    /// (#2227) Minimal FlowRecord for the silent-peer test — the field set is
    /// irrelevant to the socket deadline being asserted.
    fn silent_peer_test_record() -> FlowRecord {
        FlowRecord {
            ts: ts_utc_now(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Local,
            stage: Stage::Dispatch,
            action: "test-silent-redis-peer".to_string(),
            handle: "test".to_string(),
            phase_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        }
    }

    #[test]
    fn flow_status_serializes_without_leaking_redis_password() {
        // End-to-end shape: build a Redis sink, embed it in a FlowStatus
        // (via the SinkSummary path used by `collect_status`), serialize
        // the whole thing as the daemon's HTTP endpoint would, and assert
        // the password substring never appears anywhere in the JSON.
        let redis_sink = RedisSink::new(
            "redis://:supersecret@127.0.0.1:6379",
            "darkmux:flow",
            Some(10000),
        )
        .expect("RedisSink::new on a syntactically valid URL");
        let info = redis_sink.info();
        let (kinds, composition) = summarize_sink(&info);
        let summary = SinkSummary {
            info,
            active_kinds: kinds,
            composition,
        };
        let json = serde_json::to_string(&summary).expect("serialize SinkSummary");
        assert!(
            !json.contains("supersecret"),
            "SinkSummary JSON leaked password: {json}",
        );
    }

    #[test]
    fn human_format_includes_all_sections() {
        let status = collect_status();
        let text = format_status_human(&status);
        assert!(text.contains("darkmux flow status"));
        assert!(text.contains("schema:"));
        assert!(text.contains("composition:"));
        assert!(text.contains("Disk"));
        assert!(text.contains("Schema"));
    }

    #[test]
    fn audit_reseed_refuses_non_schema_single_line() {
        // (#899, folded into #1769's format gate) Truncating a multi-record
        // audit log down to ONE fabricated non-header line must NOT re-seed a
        // fresh clean-validating chain on the next write — the recovery
        // requires the surviving line to be a genuine byte-hash-format
        // schema header. Pre-#899-fix this re-seeded silently, laundering
        // tampering; #1769 folds the same fail-closed posture into the
        // format check (a fabricated line also lacks the `hash_format`
        // marker, so it hits the same refusal either way).
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("audit.jsonl");
        // Legit first write → header + record.
        audit_record_at(&minimal_record(), &path).unwrap();
        // Attacker truncates to a single fabricated non-header line.
        std::fs::write(&path, "{\"ts\":\"2026-01-01T00:00:00Z\",\"action\":\"forged\"}\n").unwrap();
        // Next write must bail rather than re-seed a clean chain.
        let err = audit_record_at(&minimal_record(), &path).unwrap_err();
        assert!(
            err.to_string().contains("refusing to re-seed"),
            "expected re-seed refusal, got: {err}"
        );
    }

    #[test]
    fn audit_reseed_recovers_from_header_only_file() {
        // (#899) The legit crash-recovery case must STILL work: a file with
        // only the schema header (crash between header write and the first
        // record) re-seeds cleanly on the next write — the guard must not turn
        // this into a bail.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("audit.jsonl");
        let header = crate::integrity::audit_schema_header_line().unwrap();
        std::fs::write(&path, format!("{header}\n")).unwrap();
        audit_record_at(&minimal_record(), &path).unwrap();
        let report = crate::integrity::integrity_check_file(&path).unwrap();
        assert!(report.chain_valid, "header-only recovery must produce a valid chain");
    }

    /// (#1768 threat model, FN-1) THE EXPLOIT, executed rather than argued.
    ///
    /// Before #1769, `integrity_check_file` SKIPPED content verification for
    /// any record whose enum fields carried a spelling this binary did not
    /// know (`has_unknown_enum`), and then advanced the chain using that
    /// record's STORED hash verbatim — trusted without ever being tied to
    /// the record's bytes. An attacker with write access needed only flip
    /// ONE enum field to a garbage value, then rewrite every other field
    /// freely, and the chain still reported valid.
    ///
    /// #1769's fix is the byte-hash format (see `integrity.rs`'s module
    /// doc): the stored hash covers the line's LITERAL bytes, so this test
    /// edits `reasoning` — the field carrying what the operator was told —
    /// and the unknown `tier` spelling alongside it, and asserts the tool
    /// notices regardless. There is no bypass left to exploit; this
    /// exercises the ordinary content check.
    ///
    /// If this test FAILS, the audit chain does not detect content tampering,
    /// which is the one thing it exists to do.
    #[test]
    #[serial_test::serial]
    fn fn1_an_unknown_enum_must_not_buy_a_free_content_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("chain.jsonl");

        // A legitimate two-record chain, written by the real sink path.
        for i in 0..2 {
            let mut rec = minimal_record();
            rec.reasoning = Some(format!("original reasoning {i}"));
            crate::integrity::audit_record_at(&rec, &path).unwrap();
        }
        let clean = integrity_check_file(&path).unwrap();
        assert!(clean.chain_valid, "the untampered chain must validate first, else this test proves nothing: {clean:?}");

        // The attack: rewrite the SECOND record's content in place, leaving
        // the STORED HASH PREFIX untouched — exactly what an attacker with
        // write access to the file would do. `tier` is set to an enum
        // spelling this binary doesn't recognize; under byte-hashing that
        // must not matter one way or the other (see the inverted-case test
        // above) — the content check still fires because the BYTES changed.
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<String> = raw.lines().map(str::to_string).collect();
        let last = lines.len() - 1;
        let (stored_hash, record_json) = lines[last]
            .split_once(' ')
            .expect("a byte-hash-format record line must have a `<hash> <json>` prefix");
        let mut v: serde_json::Value = serde_json::from_str(record_json).unwrap();
        v["reasoning"] = serde_json::json!("TAMPERED — this is not what the operator was told");
        v["tier"] = serde_json::json!("x");
        let tampered_json = serde_json::to_string(&v).unwrap();
        lines[last] = format!("{stored_hash} {tampered_json}");
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();

        let after = integrity_check_file(&path).unwrap();
        assert!(
            !after.chain_valid,
            "FN-1 CONFIRMED: a record's content was rewritten and the chain still reports VALID. \
             One unknown enum value bought a free edit of every other field. Report: {after:?}"
        );
    }

    /// Byte-identical round trip: write a chain of ordinary records, read it
    /// back, verify — clean, first pass, no surprises. The baseline every
    /// tampering test below is a deviation from.
    #[test]
    #[serial_test::serial]
    fn byte_hash_round_trip_is_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("chain.jsonl");

        for i in 0..5 {
            let mut rec = minimal_record();
            rec.action = format!("action-{i}");
            rec.reasoning = Some(format!("reasoning {i}"));
            crate::integrity::audit_record_at(&rec, &path).unwrap();
        }

        let report = integrity_check_file(&path).unwrap();
        assert!(report.chain_valid, "an untouched chain must validate cleanly: {report:?}");
        assert!(!report.legacy_format);
        assert_eq!(report.records_checked, 5);
        assert!(report.break_reason.is_none());
    }

    /// Tampering at every position — first, middle, last record — via edit,
    /// insert, delete, and reorder. Every one of these must break the chain.
    /// (#1769 acceptance criteria.)
    #[test]
    #[serial_test::serial]
    fn byte_hash_catches_tampering_at_every_position() {
        fn fresh_chain(path: &std::path::Path, n: usize) {
            for i in 0..n {
                let mut rec = minimal_record();
                rec.action = format!("action-{i}");
                rec.handle = format!("rec-{i}");
                crate::integrity::audit_record_at(&rec, path).unwrap();
            }
        }

        // Edit: mutate one byte of content in the FIRST, MIDDLE, and LAST
        // record, leaving the stored hash prefix untouched each time.
        for target_idx in [0usize, 1, 2] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("chain.jsonl");
            fresh_chain(&path, 3);

            let raw = std::fs::read_to_string(&path).unwrap();
            let mut lines: Vec<String> = raw.lines().map(str::to_string).collect();
            let line_idx = target_idx + 1; // skip the header
            let (stored_hash, record_json) = lines[line_idx].split_once(' ').unwrap();
            let mut v: serde_json::Value = serde_json::from_str(record_json).unwrap();
            v["handle"] = serde_json::json!("EDITED");
            let edited_json = serde_json::to_string(&v).unwrap();
            lines[line_idx] = format!("{stored_hash} {edited_json}");
            std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();

            let report = integrity_check_file(&path).unwrap();
            assert!(
                !report.chain_valid,
                "an edit at position {target_idx} must break the chain; got {report:?}"
            );
        }

        // Insert: splice a fabricated record between two real ones, with a
        // self-consistent hash (the attacker CAN compute a correct hash for
        // content they made up — the chain must catch the insertion via
        // linkage, not via a mismatched hash on the inserted line itself).
        {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("chain.jsonl");
            fresh_chain(&path, 3);

            let raw = std::fs::read_to_string(&path).unwrap();
            let mut lines: Vec<String> = raw.lines().map(str::to_string).collect();
            // Forge a record whose prev_hash points at record 0's stored
            // hash, self-consistently hashed — but NOT re-linked into the
            // record that follows it.
            let (rec0_hash, _) = lines[1].split_once(' ').unwrap();
            let mut forged = minimal_record();
            forged.action = "forged".to_string();
            forged.prev_hash = Some(rec0_hash.to_string());
            forged.hash = None;
            let forged_json = serde_json::to_string(&forged).unwrap();
            let forged_hash = crate::integrity::audit_hash_of_bytes(forged_json.as_bytes());
            lines.insert(2, format!("{forged_hash} {forged_json}"));
            std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();

            let report = integrity_check_file(&path).unwrap();
            assert!(!report.chain_valid, "an inserted record must break the chain: {report:?}");
        }

        // Delete: drop the middle record entirely.
        {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("chain.jsonl");
            fresh_chain(&path, 3);

            let raw = std::fs::read_to_string(&path).unwrap();
            let mut lines: Vec<String> = raw.lines().map(str::to_string).collect();
            lines.remove(2); // the middle record (index 0 = header, 1/2/3 = records)
            std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();

            let report = integrity_check_file(&path).unwrap();
            assert!(!report.chain_valid, "a deleted record must break the chain: {report:?}");
        }

        // Reorder: swap two record lines.
        {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("chain.jsonl");
            fresh_chain(&path, 3);

            let raw = std::fs::read_to_string(&path).unwrap();
            let mut lines: Vec<String> = raw.lines().map(str::to_string).collect();
            lines.swap(2, 3);
            std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();

            let report = integrity_check_file(&path).unwrap();
            assert!(!report.chain_valid, "reordered records must break the chain: {report:?}");
        }
    }

    // (#1775) The exit-status belt. "Verified" and "could not verify" are
    // DIFFERENT claims, and the automated consumer the docs name — a cron
    // keyed on the exit code — could previously only see the first one.
    // Stripping a file's `hash_format` marker downgraded it to legacy,
    // skipped content verification entirely, and still exited 0.
    //
    // These exercise the decision directly rather than through a spawned
    // binary, because the belt used to be reachable only by review.

    fn report(legacy: bool, valid: bool) -> crate::integrity::IntegrityReport {
        crate::integrity::IntegrityReport {
            path: "x.jsonl".into(),
            records_checked: 1,
            chain_valid: valid,
            break_at_line: None,
            break_reason: None,
            legacy_format: legacy,
            note: None,
            writer_schema_version: None,
        }
    }

    #[test]
    fn integrity_exit_code_is_zero_when_every_chain_verified() {
        let r = vec![report(false, true), report(false, true)];
        assert_eq!(crate::integrity::integrity_exit_code(&r, false), 0);
        // Strict changes nothing when there is nothing unverifiable.
        assert_eq!(crate::integrity::integrity_exit_code(&r, true), 0);
    }

    #[test]
    fn integrity_exit_code_is_two_for_a_genuine_break_regardless_of_strict() {
        let r = vec![report(false, false)];
        assert_eq!(crate::integrity::integrity_exit_code(&r, false), 2);
        assert_eq!(crate::integrity::integrity_exit_code(&r, true), 2);
    }

    /// The #1775 gap itself: a file whose content was never verified must
    /// not report the same status as one that passed. Non-strict keeps 0
    /// (a genuine read-only pre-2.6.0 archive is not a failure); strict
    /// makes it loud for the unattended consumer.
    #[test]
    fn integrity_exit_code_flags_unverifiable_only_under_strict() {
        let r = vec![report(true, true)];
        assert_eq!(
            crate::integrity::integrity_exit_code(&r, false),
            0,
            "default must stay 0 — a genuine legacy archive is not a failure"
        );
        assert_eq!(
            crate::integrity::integrity_exit_code(&r, true),
            3,
            "strict must distinguish could-not-verify from verified"
        );
    }

    /// A real break outranks an unverifiable file: 2 means "evidence of a
    /// break", 3 means "no evidence either way". Collapsing them would tell
    /// a cron the wrong thing about which file to look at.
    #[test]
    fn integrity_exit_code_prefers_a_break_over_unverifiable() {
        let r = vec![report(true, true), report(false, false)];
        assert_eq!(crate::integrity::integrity_exit_code(&r, true), 2);
    }

    #[test]
    fn integrity_exit_code_is_zero_for_no_files() {
        assert_eq!(crate::integrity::integrity_exit_code(&[], true), 0);
    }

    /// A legacy-format file (no `hash_format` marker on its header) must
    /// report Warn-shaped honesty — readable, not re-verifiable — never
    /// tampering, and never a silent pass. (#1769)
    #[test]
    #[serial_test::serial]
    fn legacy_format_file_reports_honestly_not_as_tampering() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("chain.jsonl");

        // A pre-2.6.0 struct-hash-format file: bare JSON per line, `hash`
        // embedded, header lacking the `hash_format` marker.
        let legacy_header = crate::integrity::schema_header_line().unwrap();
        let mut rec = minimal_record();
        rec.hash = Some("deadbeef".repeat(8));
        let legacy_line = serde_json::to_string(&rec).unwrap();
        std::fs::write(&path, format!("{legacy_header}\n{legacy_line}\n")).unwrap();

        let report = integrity_check_file(&path).unwrap();
        assert!(
            report.chain_valid,
            "a legacy-format file is a format boundary, not tampering — chain_valid must stay \
             true: {report:?}"
        );
        assert!(report.legacy_format, "must be flagged legacy: {report:?}");
        let note = report.note.expect("a legacy file must carry an honest note");
        assert!(
            !note.to_lowercase().contains("tamper") && !note.to_lowercase().contains("edited"),
            "wording must not assert editing or tampering; got {note:?}"
        );
        assert!(
            note.contains("legacy") || note.contains("not re-verifiable"),
            "wording must say why, honestly; got {note:?}"
        );
    }


    /// (#2310 swarm F / S2-2) The leniency half of the 1.40.0 bump: a
    /// reader built against an OLDER schema must still parse a
    /// `mission.grow` record carrying 1.40.0's new payload keys, and a
    /// reader built against 1.40.0 must still parse a record from a
    /// FUTURE writer. `payload` is a free-form `Value` and every
    /// consumer-facing field is either required-and-unchanged or
    /// `Option`, so this holds structurally — pinned here because
    /// "consumers are lenient-on-read, loud in doctor" (contract 5) is
    /// exactly the promise a schema bump is allowed to lean on, and an
    /// unpinned promise is one refactor from being false.
    #[test]
    fn a_mission_grow_record_from_a_newer_writer_still_parses() {
        let raw = serde_json::json!({
            "ts": "2026-09-05T00:00:00Z",
            "level": "info",
            "category": "mission",
            "tier": "local",
            "stage": "dispatch",
            "action": "mission.grow",
            "handle": "review-v2",
            "schema_version": "1.41.0",
            "an_unknown_top_level_key": 7,
            "payload": {
                "phase": "m-review",
                "task_template": "unit-swallowed-error",
                "from": "plan-swallowed-error",
                "source": "m-plan-swallowed-error-step",
                "items": 0,
                "minted": [],
                "reason": "producer_errored",
                "producer_step": "m-plan-swallowed-error-step",
                "producer_status": "error",
                "a_key_no_reader_knows": true
            }
        });
        let rec: FlowRecord = serde_json::from_value(raw).expect(
            "an older/newer reader must still parse a mission.grow record — the payload is \
             free-form and every added field is optional",
        );
        assert_eq!(rec.action, "mission.grow");
        let payload = rec.payload.expect("the record carries its payload");
        assert_eq!(payload["producer_status"], serde_json::json!("error"));
        assert_eq!(payload["reason"], serde_json::json!("producer_errored"));
    }

}
