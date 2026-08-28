//! Hooks — a fourth `FlowSink` kind (#2093): match a flow record against
//! operator-configured rules, and POST a match verbatim to a loopback
//! receiver. **Enqueue, never block**: `write()` only appends to a local
//! outbox file (flock'd, mirroring `AuditFileSink`); a background drainer
//! thread does the actual HTTP delivery with bounded retries, so a down
//! receiver never stalls a dispatch.
//!
//! # Loop prevention
//!
//! A record whose `action` starts with `hook.` (the sink's own
//! `hook.fired`/`hook.failed` firing records) never matches any rule —
//! checked unconditionally, first, in both the match predicate and
//! `HookSink::write`. This is what makes it safe for the drainer to write
//! `hook.fired`/`hook.failed` back through an ordinary `FlowSink` (even one
//! that happens to include this very `HookSink`) without risking a loop.
//!
//! # Wiring
//!
//! `build_default_sink()` constructs `HookSink` against the sinks
//! accumulated so far (LocalFile, optionally Audit/Redis) wrapped as its
//! `report_sink` — the destination `hook.fired`/`hook.failed` records are
//! written to. This avoids a self-referential `Arc` at construction time;
//! the loop guard above is what makes even a literal self-reference safe,
//! but this ordering doesn't rely on that as the only defense.

use crate::schema::{self, Category, FlowRecord, Level, Stage, Tier};
use crate::{FlowSink, SinkInfo};
use anyhow::{anyhow, bail, Context, Result};
use darkmux_types::config::{HookMatch, HookRule};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// ─── Match predicate ──────────────────────────────────────────────────

/// A small glob matcher for `HookMatch::action`: `*` matches within a
/// segment, and a trailing `*` segment matches one-or-more further
/// dot-separated segments. A bare `*` matches every (non-`hook.`) action.
/// Deliberately minimal — not a general glob engine.
pub fn action_glob_matches(pattern: &str, action: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let pat_segs: Vec<&str> = pattern.split('.').collect();
    let act_segs: Vec<&str> = action.split('.').collect();
    if let Some((last, head)) = pat_segs.split_last() {
        if *last == "*" {
            if act_segs.len() <= head.len() {
                return false;
            }
            return head.iter().zip(act_segs.iter()).all(|(p, a)| segment_glob(p, a));
        }
    }
    pat_segs.len() == act_segs.len()
        && pat_segs.iter().zip(act_segs.iter()).all(|(p, a)| segment_glob(p, a))
}

fn segment_glob(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    match pattern.find('*') {
        None => pattern == value,
        Some(idx) => {
            let (prefix, suffix) = (&pattern[..idx], &pattern[idx + 1..]);
            value.len() >= prefix.len() + suffix.len()
                && value.starts_with(prefix)
                && value.ends_with(suffix)
        }
    }
}

fn category_wire(c: Category) -> String {
    serde_json::to_value(c)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn level_wire(l: Level) -> String {
    serde_json::to_value(l)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// True when `record` satisfies `m`. Records whose `action` starts with
/// `hook.` NEVER match, regardless of `m` — the loop guard, checked first
/// and unconditionally. An all-`None` match matches nothing (`m.is_empty()`
/// short-circuits false) — an empty match is a rule an operator forgot to
/// fill in, not an accidental catch-all.
pub fn hook_match(m: &HookMatch, record: &FlowRecord) -> bool {
    if record.action.starts_with("hook.") {
        return false;
    }
    if m.is_empty() {
        return false;
    }
    if let Some(pat) = m.action.as_deref() {
        if !action_glob_matches(pat, &record.action) {
            return false;
        }
    }
    if let Some(v) = m.session_id.as_deref() {
        if record.session_id.as_deref() != Some(v) {
            return false;
        }
    }
    if let Some(v) = m.mission_id.as_deref() {
        if record.mission_id.as_deref() != Some(v) {
            return false;
        }
    }
    if let Some(v) = m.machine_id.as_deref() {
        if record.machine_id.as_deref() != Some(v) {
            return false;
        }
    }
    if let Some(v) = m.category.as_deref() {
        if !category_wire(record.category).eq_ignore_ascii_case(v) {
            return false;
        }
    }
    if let Some(v) = m.level.as_deref() {
        if !level_wire(record.level).eq_ignore_ascii_case(v) {
            return false;
        }
    }
    true
}

// ─── URL policy (loopback-only) ────────────────────────────────────────

/// Refuse anything but a loopback `http://` target — `127.0.0.1`, `::1`
/// (bracketed), or `localhost`. A token-bearing remote hook is a later
/// packet (#2093's own "out of scope"); until then, refusing at config
/// load is the whole enforcement.
pub fn validate_loopback_http_url(raw: &str) -> Result<()> {
    let rest = raw.strip_prefix("http://").ok_or_else(|| {
        anyhow!(
            "hook URL `{raw}` must use http:// and target loopback — a token-bearing remote \
             hook is a later packet"
        )
    })?;
    let host_port = rest.split('/').next().unwrap_or("");
    let host = extract_host(host_port);
    let is_loopback = host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || (host.starts_with("127.") && host.split('.').count() == 4 && host.split('.').all(|o| o.parse::<u8>().is_ok()));
    if !is_loopback {
        bail!(
            "hook URL `{raw}` targets non-loopback host `{host}` — only 127.0.0.1/::1/localhost \
             are allowed; a token-bearing remote hook is a later packet"
        );
    }
    Ok(())
}

/// The `host[:port]` portion of a `scheme://host[:port]/path` URL, or
/// `None` if `raw` doesn't start with `http://`.
fn extract_host_port(raw: &str) -> Option<&str> {
    raw.strip_prefix("http://").map(|rest| rest.split('/').next().unwrap_or(""))
}

/// Extract the bare host from a `host[:port]` string, handling a bracketed
/// IPv6 literal (`[::1]:8790` → `::1`).
fn extract_host(host_port: &str) -> &str {
    if let Some(rest) = host_port.strip_prefix('[') {
        if let Some(idx) = rest.find(']') {
            return &rest[..idx];
        }
        return rest;
    }
    host_port.rsplit_once(':').map(|(h, _)| h).unwrap_or(host_port)
}

/// Filesystem-safe form of a `host[:port]` string for an outbox filename.
fn sanitize_host_port(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '-' })
        .collect()
}

fn outbox_paths(outbox_dir: &Path, index: usize, url: &str) -> (PathBuf, PathBuf) {
    let host_port = extract_host_port(url).unwrap_or("unknown");
    let sanitized = sanitize_host_port(host_port);
    let base = format!("{index}-{sanitized}");
    (outbox_dir.join(format!("{base}.outbox.jsonl")), outbox_dir.join(format!("{base}.cursor")))
}

// ─── Resolved rules ─────────────────────────────────────────────────────

/// A `HookRule` resolved against an outbox dir: validated URL + the outbox
/// / cursor file paths it owns. `HookSink::new` builds these (and refuses
/// the WHOLE sink on the first invalid rule); `summarize_configured_rules`
/// builds an unvalidated variant for read-only introspection (doctor,
/// `flow hooks status`) that never bails.
#[derive(Debug, Clone)]
pub struct ResolvedRule {
    pub index: usize,
    pub match_: HookMatch,
    pub url: String,
    pub outbox_path: PathBuf,
    pub cursor_path: PathBuf,
}

/// Resolve + validate every rule against `outbox_dir`. Bails on the FIRST
/// rule missing an `http` target or targeting a non-loopback host — the
/// whole hooks sink is refused rather than silently dropping one bad rule,
/// so a config mistake is loud at construction, not a quietly-smaller
/// rule set.
pub fn resolve_rules(rules: &[HookRule], outbox_dir: &Path) -> Result<Vec<ResolvedRule>> {
    let mut out = Vec::with_capacity(rules.len());
    for (index, r) in rules.iter().enumerate() {
        let url = r
            .http
            .clone()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("hook rule #{index} has no `http` target"))?;
        validate_loopback_http_url(&url).with_context(|| format!("hook rule #{index}"))?;
        let (outbox_path, cursor_path) = outbox_paths(outbox_dir, index, &url);
        out.push(ResolvedRule {
            index,
            match_: r.r#match.clone().unwrap_or_default(),
            url,
            outbox_path,
            cursor_path,
        });
    }
    Ok(out)
}

/// A read-only summary of one configured rule, for `darkmux doctor` and
/// `darkmux flow hooks status` — never bails (an invalid URL is reported
/// AS a field, not an error), and never touches the network.
#[derive(Debug, Clone)]
pub struct HookRuleSummary {
    pub index: usize,
    pub match_desc: String,
    pub url: String,
    pub is_loopback: bool,
    pub is_empty_match: bool,
    pub outbox_path: PathBuf,
    pub cursor_path: PathBuf,
    pub undelivered: usize,
}

fn describe_match(m: &HookMatch) -> String {
    if m.is_empty() {
        return "(empty — matches nothing)".to_string();
    }
    let mut parts = Vec::new();
    if let Some(v) = &m.action {
        parts.push(format!("action={v}"));
    }
    if let Some(v) = &m.session_id {
        parts.push(format!("session_id={v}"));
    }
    if let Some(v) = &m.mission_id {
        parts.push(format!("mission_id={v}"));
    }
    if let Some(v) = &m.machine_id {
        parts.push(format!("machine_id={v}"));
    }
    if let Some(v) = &m.category {
        parts.push(format!("category={v}"));
    }
    if let Some(v) = &m.level {
        parts.push(format!("level={v}"));
    }
    parts.join(", ")
}

/// Build a read-only summary of every configured rule — used by
/// `darkmux doctor` and `darkmux flow hooks status`. Unlike
/// `resolve_rules`, this never bails: an invalid URL shows up as
/// `is_loopback: false` rather than an error, so the caller can report ALL
/// rules' problems at once instead of stopping at the first.
pub fn summarize_configured_rules(rules: &[HookRule], outbox_dir: &Path) -> Vec<HookRuleSummary> {
    rules
        .iter()
        .enumerate()
        .map(|(index, r)| {
            let m = r.r#match.clone().unwrap_or_default();
            let url = r.http.clone().unwrap_or_default();
            let is_loopback = validate_loopback_http_url(&url).is_ok();
            let (outbox_path, cursor_path) = outbox_paths(outbox_dir, index, &url);
            let cursor = read_cursor(&cursor_path);
            let undelivered = undelivered_line_count(&outbox_path, cursor);
            HookRuleSummary {
                index,
                match_desc: describe_match(&m),
                url,
                is_loopback,
                is_empty_match: m.is_empty(),
                outbox_path,
                cursor_path,
                undelivered,
            }
        })
        .collect()
}

// ─── Outbox file I/O ────────────────────────────────────────────────────

/// Append `line` (a single record's serialized JSON, no trailing newline)
/// to the outbox at `path`, flock'd like `AuditFileSink` — cross-process
/// safe, and never torn against another writer (single `write_all` under
/// the lock, mirroring `audit_record_at_locked`).
fn append_outbox_line(path: &Path, line: &str) -> Result<()> {
    darkmux_types::flock::with_locked_file(path, |file| {
        file.seek(SeekFrom::End(0)).with_context(|| format!("seek to end of {}", path.display()))?;
        file.write_all(line.as_bytes())
            .and_then(|_| file.write_all(b"\n"))
            .with_context(|| format!("appending to hook outbox {}", path.display()))?;
        file.sync_all().with_context(|| format!("syncing hook outbox {}", path.display()))?;
        Ok(())
    })
}

fn read_cursor(cursor_path: &Path) -> u64 {
    fs::read_to_string(cursor_path).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

fn write_cursor(cursor_path: &Path, offset: u64) -> Result<()> {
    if let Some(parent) = cursor_path.parent() {
        fs::create_dir_all(parent).ok();
    }
    fs::write(cursor_path, offset.to_string()).with_context(|| format!("writing cursor {}", cursor_path.display()))
}

/// The next fully-committed line at or after `cursor`, plus the byte
/// offset just past it — or `None` when nothing is pending, INCLUDING a
/// partially-written line still missing its trailing newline (never
/// delivered half a record; the next poll picks it up once complete).
fn next_pending_line(outbox_path: &Path, cursor: u64) -> Option<(String, u64)> {
    let mut file = fs::File::open(outbox_path).ok()?;
    file.seek(SeekFrom::Start(cursor)).ok()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;
    let nl = buf.find('\n')?;
    Some((buf[..nl].to_string(), cursor + nl as u64 + 1))
}

/// Count of fully-committed (newline-terminated) lines at or after
/// `cursor` — the "undelivered" count `darkmux doctor` / `flow hooks
/// status` report.
pub fn undelivered_line_count(outbox_path: &Path, cursor: u64) -> usize {
    let Ok(mut file) = fs::File::open(outbox_path) else {
        return 0;
    };
    if file.seek(SeekFrom::Start(cursor)).is_err() {
        return 0;
    }
    let mut buf = String::new();
    if file.read_to_string(&mut buf).is_err() {
        return 0;
    }
    buf.matches('\n').count()
}

// ─── Delivery ───────────────────────────────────────────────────────────

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const MAX_CLIENT_ERROR_ATTEMPTS: u32 = 3;
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const POST_TIMEOUT: Duration = Duration::from_secs(5);

enum DeliveryOutcome {
    Success,
    ClientError,
    RetryableFailure,
}

fn try_post(url: &str, body: &str) -> DeliveryOutcome {
    let agent = ureq::AgentBuilder::new().timeout(POST_TIMEOUT).build();
    match agent.post(url).set("Content-Type", "application/json").send_string(body) {
        Ok(_resp) => DeliveryOutcome::Success,
        Err(ureq::Error::Status(code, _resp)) if (400..500).contains(&code) => DeliveryOutcome::ClientError,
        Err(_) => DeliveryOutcome::RetryableFailure,
    }
}

struct RuleRuntime {
    rule: ResolvedRule,
    backoff: Mutex<Duration>,
    next_attempt: Mutex<Instant>,
    /// Attempts made against the CURRENT undelivered line — reset on
    /// success or give-up, so it never leaks across lines.
    attempt_count: Mutex<u32>,
}

fn apply_backoff(rt: &RuleRuntime) {
    let mut backoff = rt.backoff.lock().unwrap();
    let wait = *backoff;
    *rt.next_attempt.lock().unwrap() = Instant::now() + wait;
    *backoff = (*backoff * 2).min(MAX_BACKOFF);
}

fn reset_backoff(rt: &RuleRuntime) {
    *rt.backoff.lock().unwrap() = INITIAL_BACKOFF;
    *rt.next_attempt.lock().unwrap() = Instant::now();
    *rt.attempt_count.lock().unwrap() = 0;
}

/// Emit `hook.fired` (success) or `hook.failed` (give-up) through
/// `report_sink`. Best-effort — a failure to emit is logged, never
/// propagated (this runs on the drainer thread; nothing is waiting on it).
fn emit_hook_record(
    report_sink: &dyn FlowSink,
    success: bool,
    rule: &ResolvedRule,
    delivered_line: &str,
    attempt: u32,
    error: Option<&str>,
) {
    let parsed: serde_json::Value = serde_json::from_str(delivered_line).unwrap_or_default();
    let action = parsed.get("action").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let hash = parsed.get("hash").and_then(|v| v.as_str()).map(str::to_string);
    let host = extract_host_port(&rule.url).unwrap_or("").to_string();

    let mut payload = serde_json::json!({
        "rule_index": rule.index,
        "target_host": host,
        "delivered_action": action,
        "attempt": attempt,
    });
    if let Some(h) = hash {
        payload["delivered_hash"] = serde_json::Value::String(h);
    }
    if let Some(e) = error {
        payload["error"] = serde_json::Value::String(e.to_string());
    }

    let rec = FlowRecord {
        ts: schema::ts_utc_now(),
        level: if success { Level::Info } else { Level::Error },
        category: Category::Machinery,
        tier: Tier::Local,
        stage: Stage::Ship,
        action: if success { "hook.fired".to_string() } else { "hook.failed".to_string() },
        handle: host,
        phase_id: None,
        session_id: None,
        source: Some("hook".to_string()),
        model: None,
        reasoning: None,
        mission_id: None,
        machine_id: None,
        machine_uid: None,
        prev_hash: None,
        hash: None,
        payload: Some(payload),
        work_id: None,
        attempt: None,
    };
    if let Err(e) = report_sink.write(&rec) {
        eprintln!("flow::HookSink: failed to emit {}: {e:#}", rec.action);
    }
}

fn drainer_loop(
    rules: Vec<Arc<RuleRuntime>>,
    stop: Arc<AtomicBool>,
    nudge: Arc<(Mutex<bool>, Condvar)>,
    report_sink: Arc<dyn FlowSink>,
) {
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let mut did_work = false;
        for rt in &rules {
            if stop.load(Ordering::Acquire) {
                return;
            }
            if Instant::now() < *rt.next_attempt.lock().unwrap() {
                continue;
            }
            let cursor = read_cursor(&rt.rule.cursor_path);
            let Some((line, new_cursor)) = next_pending_line(&rt.rule.outbox_path, cursor) else {
                continue;
            };
            did_work = true;
            match try_post(&rt.rule.url, &line) {
                DeliveryOutcome::Success => {
                    let attempt = {
                        let mut c = rt.attempt_count.lock().unwrap();
                        *c += 1;
                        *c
                    };
                    let _ = write_cursor(&rt.rule.cursor_path, new_cursor);
                    reset_backoff(rt);
                    emit_hook_record(report_sink.as_ref(), true, &rt.rule, &line, attempt, None);
                }
                DeliveryOutcome::ClientError => {
                    let attempt = {
                        let mut c = rt.attempt_count.lock().unwrap();
                        *c += 1;
                        *c
                    };
                    if attempt >= MAX_CLIENT_ERROR_ATTEMPTS {
                        let _ = write_cursor(&rt.rule.cursor_path, new_cursor);
                        reset_backoff(rt);
                        emit_hook_record(
                            report_sink.as_ref(),
                            false,
                            &rt.rule,
                            &line,
                            attempt,
                            Some("4xx response, skipped after 3 attempts"),
                        );
                    } else {
                        apply_backoff(rt);
                    }
                }
                DeliveryOutcome::RetryableFailure => {
                    {
                        let mut c = rt.attempt_count.lock().unwrap();
                        *c += 1;
                    }
                    apply_backoff(rt);
                }
            }
        }
        if !did_work && !stop.load(Ordering::Acquire) {
            let (lock, cvar) = &*nudge;
            let pending = lock.lock().unwrap();
            if !*pending {
                let (mut pending, _timeout) = cvar.wait_timeout(pending, POLL_INTERVAL).unwrap();
                *pending = false;
            } else {
                drop(pending);
                *lock.lock().unwrap() = false;
            }
        }
    }
}

// ─── HookSink ───────────────────────────────────────────────────────────

pub struct HookSink {
    outbox_dir: PathBuf,
    rules: Vec<Arc<RuleRuntime>>,
    stop: Arc<AtomicBool>,
    nudge: Arc<(Mutex<bool>, Condvar)>,
    drainer: Mutex<Option<JoinHandle<()>>>,
}

impl HookSink {
    /// Resolve + validate `rules` against `outbox_dir` (bails on the first
    /// invalid rule — see `resolve_rules`), then start ONE background
    /// drainer thread that services every rule. `report_sink` is where
    /// `hook.fired`/`hook.failed` records land — see the module doc for why
    /// it's a snapshot of the OTHER sinks, not this one.
    pub fn new(rules: &[HookRule], outbox_dir: PathBuf, report_sink: Arc<dyn FlowSink>) -> Result<Self> {
        let resolved = resolve_rules(rules, &outbox_dir)?;
        let now = Instant::now();
        let rule_runtimes: Vec<Arc<RuleRuntime>> = resolved
            .into_iter()
            .map(|r| {
                Arc::new(RuleRuntime {
                    rule: r,
                    backoff: Mutex::new(INITIAL_BACKOFF),
                    next_attempt: Mutex::new(now),
                    attempt_count: Mutex::new(0),
                })
            })
            .collect();

        let stop = Arc::new(AtomicBool::new(false));
        let nudge = Arc::new((Mutex::new(false), Condvar::new()));

        let thread_rules = rule_runtimes.clone();
        let thread_stop = stop.clone();
        let thread_nudge = nudge.clone();
        let handle = std::thread::Builder::new()
            .name("hook-drainer".to_string())
            .spawn(move || drainer_loop(thread_rules, thread_stop, thread_nudge, report_sink))
            .context("spawning hook drainer thread")?;

        Ok(Self { outbox_dir, rules: rule_runtimes, stop, nudge, drainer: Mutex::new(Some(handle)) })
    }

    pub fn outbox_dir(&self) -> &Path {
        &self.outbox_dir
    }
}

impl FlowSink for HookSink {
    fn write(&self, record: &FlowRecord) -> Result<()> {
        // Loop guard — never even considered against any rule.
        if record.action.starts_with("hook.") {
            return Ok(());
        }
        let line = serde_json::to_string(record).context("serializing record for hook outbox")?;
        let mut any = false;
        for rt in &self.rules {
            if hook_match(&rt.rule.match_, record) {
                if let Err(e) = append_outbox_line(&rt.rule.outbox_path, &line) {
                    eprintln!(
                        "flow::HookSink: rule #{} outbox append failed: {e:#} (this write is lost \
                         for that rule; other rules + other sinks are unaffected)",
                        rt.rule.index
                    );
                } else {
                    any = true;
                }
            }
        }
        if any {
            let (lock, cvar) = &*self.nudge;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }
        Ok(())
    }

    fn info(&self) -> SinkInfo {
        let mut config = BTreeMap::new();
        config.insert("outbox_dir".to_string(), self.outbox_dir.display().to_string());
        config.insert("rules".to_string(), self.rules.len().to_string());
        for rt in &self.rules {
            let idx = rt.rule.index;
            config.insert(format!("rule{idx}_url"), rt.rule.url.clone());
            let cursor = read_cursor(&rt.rule.cursor_path);
            let undelivered = undelivered_line_count(&rt.rule.outbox_path, cursor);
            config.insert(format!("rule{idx}_undelivered"), undelivered.to_string());
        }
        SinkInfo { kind: "Hooks".to_string(), config, children: vec![], raw_url: None }
    }
}

impl Drop for HookSink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        {
            let (lock, cvar) = &*self.nudge;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }
        let Some(handle) = self.drainer.lock().unwrap().take() else {
            return;
        };
        // Bounded join (≤2s) — mirrors `open_redis_connection_bounded`'s
        // shape: run the blocking join on a helper thread, wait on a
        // channel with a timeout. A drainer mid-HTTP-call can take up to
        // POST_TIMEOUT to notice `stop`; if it hasn't by the bound, we
        // detach rather than hang shutdown.
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("hook-drainer-joiner".to_string())
            .spawn(move || {
                let _ = tx.send(handle.join());
            });
        if spawned.is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
    }
}

// ─── Test-only loopback HTTP receiver ────────────────────────────────────
//
// A minimal HTTP/1.1 server for exercising real delivery — spun up per
// test, records each request's body, and returns a caller-supplied status
// sequence (repeating the last entry once exhausted). Gated behind this
// crate's `test-support` feature (the same convention as
// `isolate_test_env_once`), so a downstream crate's test build can reuse it
// via a dev-dependency without compiling it into release binaries.
#[cfg(any(test, feature = "test-support"))]
pub mod test_receiver {
    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    pub struct HookReceiver {
        pub addr: SocketAddr,
        received: Arc<Mutex<Vec<String>>>,
        statuses: Arc<Mutex<VecDeque<u16>>>,
        stop: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl HookReceiver {
        /// Bind an ephemeral loopback port and start accepting. Every
        /// request gets `200 OK` unless `with_status_sequence` overrides it.
        pub fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral loopback port");
            Self::from_listener(listener)
        }

        /// Bind a SPECIFIC address rather than an ephemeral one — for tests
        /// that need a receiver to come back up on the exact port a prior
        /// (now-dropped) receiver held, proving persisted-outbox redelivery
        /// without a process restart. Retries briefly since the OS may not
        /// release a just-closed listening socket instantaneously.
        pub fn start_on(addr: SocketAddr) -> Self {
            let mut last_err = None;
            for _ in 0..100 {
                match TcpListener::bind(addr) {
                    Ok(listener) => return Self::from_listener(listener),
                    Err(e) => {
                        last_err = Some(e);
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                }
            }
            panic!("could not rebind {addr}: {last_err:?}");
        }

        fn from_listener(listener: TcpListener) -> Self {
            listener.set_nonblocking(true).unwrap();
            let addr = listener.local_addr().unwrap();
            let received = Arc::new(Mutex::new(Vec::new()));
            let statuses = Arc::new(Mutex::new(VecDeque::new()));
            let stop = Arc::new(AtomicBool::new(false));

            let thread_received = received.clone();
            let thread_statuses = statuses.clone();
            let thread_stop = stop.clone();
            let handle = std::thread::spawn(move || {
                loop {
                    if thread_stop.load(Ordering::Acquire) {
                        return;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = handle_one(stream, &thread_received, &thread_statuses);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        Err(_) => return,
                    }
                }
            });

            Self { addr, received, statuses, stop, handle: Some(handle) }
        }

        /// Queue a sequence of HTTP status codes to return, one per
        /// request; the LAST entry repeats once the queue is exhausted.
        pub fn with_status_sequence(self, statuses: impl IntoIterator<Item = u16>) -> Self {
            *self.statuses.lock().unwrap() = statuses.into_iter().collect();
            self
        }

        pub fn url(&self, path: &str) -> String {
            format!("http://{}{}", self.addr, path)
        }

        /// Every request body received so far, in arrival order.
        pub fn bodies(&self) -> Vec<String> {
            self.received.lock().unwrap().clone()
        }

        pub fn request_count(&self) -> usize {
            self.received.lock().unwrap().len()
        }
    }

    impl Drop for HookReceiver {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            // Nudge the accept() loop past its `WouldBlock` poll by
            // connecting once; ignore any error (the loop may already have
            // seen `stop`).
            let _ = TcpStream::connect(self.addr);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    fn handle_one(
        mut stream: TcpStream,
        received: &Arc<Mutex<Vec<String>>>,
        statuses: &Arc<Mutex<VecDeque<u16>>>,
    ) -> std::io::Result<()> {
        stream.set_nonblocking(false)?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut request_line = String::new();
        reader.read_line(&mut request_line)?;
        if request_line.is_empty() {
            return Ok(());
        }
        let mut content_length: usize = 0;
        loop {
            let mut header_line = String::new();
            reader.read_line(&mut header_line)?;
            if header_line == "\r\n" || header_line.is_empty() {
                break;
            }
            if let Some(v) = header_line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body)?;
        }
        received.lock().unwrap().push(String::from_utf8_lossy(&body).into_owned());

        let status = {
            let mut q = statuses.lock().unwrap();
            if q.len() > 1 {
                q.pop_front().unwrap_or(200)
            } else {
                q.front().copied().unwrap_or(200)
            }
        };
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "Status",
        };
        let resp = format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        stream.write_all(resp.as_bytes())?;
        stream.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_types::config::HookMatch;
    use test_receiver::HookReceiver;

    /// A no-op `FlowSink` for tests that need a valid `report_sink` but
    /// aren't asserting on what lands in it. Deliberately NOT a real
    /// `LocalFileSink` — this crate's own `local_sink_dir()` doc explains
    /// why: it resolves a SHARED per-process test-mode temp dir, and a
    /// hook.fired/failed record written there during a delivery test can
    /// pollute an unrelated test reading "today's" flows file concurrently
    /// (the exact leak class #507/#811 exist to prevent). Tests that DO
    /// assert on emitted hook.fired/hook.failed records use a dedicated
    /// `CapturingSink` instead (see `delivers_matching_record_and_emits_hook_fired`).
    struct NullSink;
    impl FlowSink for NullSink {
        fn write(&self, _record: &FlowRecord) -> Result<()> {
            Ok(())
        }
        fn info(&self) -> SinkInfo {
            SinkInfo { kind: "Null".into(), config: Default::default(), children: vec![], raw_url: None }
        }
    }

    fn record(action: &str) -> FlowRecord {
        FlowRecord {
            ts: schema::ts_utc_now(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Local,
            stage: Stage::Dispatch,
            action: action.to_string(),
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
        }
    }

    // ─── 1. Match predicate ───────────────────────────────────────────

    #[test]
    fn action_glob_trailing_wildcard() {
        assert!(action_glob_matches("crawl.*", "crawl.finding"));
        assert!(!action_glob_matches("crawl.*", "crawler"), "crawler must NOT match crawl.*");
        assert!(!action_glob_matches("crawl.*", "crawl"), "no further segment to match the wildcard");
    }

    #[test]
    fn action_glob_bare_star_matches_everything_non_hook() {
        assert!(action_glob_matches("*", "crawl.finding"));
        assert!(action_glob_matches("*", "dispatch error"));
    }

    #[test]
    fn action_glob_exact_match_no_wildcard() {
        assert!(action_glob_matches("dispatch error", "dispatch error"));
        assert!(!action_glob_matches("dispatch error", "dispatch start"));
    }

    #[test]
    fn hook_match_exact_fields() {
        let mut r = record("crawl.finding");
        r.session_id = Some("s1".to_string());
        r.mission_id = Some("m1".to_string());
        r.machine_id = Some("studio".to_string());

        let m = HookMatch { session_id: Some("s1".to_string()), ..Default::default() };
        assert!(hook_match(&m, &r));

        let m = HookMatch { session_id: Some("other".to_string()), ..Default::default() };
        assert!(!hook_match(&m, &r));

        let m = HookMatch { mission_id: Some("m1".to_string()), machine_id: Some("studio".to_string()), ..Default::default() };
        assert!(hook_match(&m, &r));
    }

    #[test]
    fn hook_match_category_and_level() {
        let mut r = record("telemetry.tokens");
        r.category = Category::Telemetry;
        r.level = Level::Warn;
        assert!(hook_match(&HookMatch { category: Some("telemetry".to_string()), ..Default::default() }, &r));
        assert!(hook_match(&HookMatch { level: Some("warn".to_string()), ..Default::default() }, &r));
        assert!(!hook_match(&HookMatch { category: Some("work".to_string()), ..Default::default() }, &r));
    }

    #[test]
    fn empty_match_matches_nothing() {
        let r = record("crawl.finding");
        assert!(!hook_match(&HookMatch::default(), &r));
    }

    #[test]
    fn hook_actions_never_match_any_rule() {
        // Even the maximally-permissive `*` action pattern must not catch
        // the sink's own firing/failure records — loop prevention.
        let r = record("hook.fired");
        assert!(!hook_match(&HookMatch { action: Some("*".to_string()), ..Default::default() }, &r));
        let r = record("hook.failed");
        assert!(!hook_match(&HookMatch { action: Some("*".to_string()), ..Default::default() }, &r));
    }

    // ─── URL policy ─────────────────────────────────────────────────────

    #[test]
    fn loopback_urls_accepted() {
        assert!(validate_loopback_http_url("http://127.0.0.1:8790/events").is_ok());
        assert!(validate_loopback_http_url("http://localhost:9000/x").is_ok());
        assert!(validate_loopback_http_url("http://[::1]:8790/x").is_ok());
    }

    #[test]
    fn non_loopback_urls_refused() {
        assert!(validate_loopback_http_url("http://10.0.0.5:8790/x").is_err());
        assert!(validate_loopback_http_url("http://example.com/x").is_err());
        assert!(validate_loopback_http_url("https://127.0.0.1/x").is_err(), "https refused for loopback too — http only");
    }

    #[test]
    fn resolve_rules_refuses_on_first_non_loopback() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some("http://10.0.0.5:8790/x".to_string()),
            extras: Default::default(),
        }];
        assert!(resolve_rules(&rules, tmp.path()).is_err());
    }

    // ─── 2. Outbox append/read ──────────────────────────────────────────

    #[test]
    fn write_appends_one_line_per_matching_rule() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            extras: Default::default(),
        }];
        let report: Arc<dyn FlowSink> = Arc::new(NullSink);
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();

        sink.write(&record("crawl.finding")).unwrap();
        sink.write(&record("dispatch start")).unwrap(); // non-matching — appends nothing

        // Give the append a moment to land (write() itself is synchronous,
        // but read the file only after both writes to keep this simple).
        let outbox = &sink.rules[0].rule.outbox_path;
        let content = std::fs::read_to_string(outbox).unwrap_or_default();
        assert_eq!(content.lines().count(), 1, "only the matching record was appended: {content}");
    }

    #[test]
    fn concurrent_writes_produce_intact_lines() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            extras: Default::default(),
        }];
        let report: Arc<dyn FlowSink> = Arc::new(NullSink);
        let sink = Arc::new(HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap());

        let mut handles = Vec::new();
        for i in 0..8 {
            let sink = sink.clone();
            handles.push(std::thread::spawn(move || {
                for j in 0..10 {
                    sink.write(&record(&format!("work.item.{i}.{j}"))).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let outbox = &sink.rules[0].rule.outbox_path;
        let content = std::fs::read_to_string(outbox).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 80, "no lost or torn lines under concurrent writers");
        for l in &lines {
            assert!(serde_json::from_str::<serde_json::Value>(l).is_ok(), "intact JSON line: {l}");
        }
    }

    // ─── 3. Delivery ─────────────────────────────────────────────────────

    fn wait_until<F: Fn() -> bool>(cond: F, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        cond()
    }

    #[test]
    fn delivers_matching_record_and_emits_hook_fired() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            extras: Default::default(),
        }];

        #[derive(Default)]
        struct CapturingSink(Mutex<Vec<FlowRecord>>);
        impl FlowSink for CapturingSink {
            fn write(&self, record: &FlowRecord) -> Result<()> {
                self.0.lock().unwrap().push(record.clone());
                Ok(())
            }
            fn info(&self) -> SinkInfo {
                SinkInfo { kind: "Capturing".into(), config: Default::default(), children: vec![], raw_url: None }
            }
        }
        let capture = Arc::new(CapturingSink::default());
        let report: Arc<dyn FlowSink> = capture.clone();
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();

        sink.write(&record("crawl.finding")).unwrap();

        assert!(wait_until(|| receiver.request_count() == 1, Duration::from_secs(3)));
        let bodies = receiver.bodies();
        let delivered: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
        assert_eq!(delivered["action"], "crawl.finding");

        assert!(wait_until(|| capture.0.lock().unwrap().iter().any(|r| r.action == "hook.fired"), Duration::from_secs(3)));
        let fired = capture.0.lock().unwrap();
        let fired = fired.iter().find(|r| r.action == "hook.fired").unwrap();
        assert_eq!(fired.payload.as_ref().unwrap()["delivered_action"], "crawl.finding");
    }

    #[test]
    fn down_receiver_does_not_block_write() {
        let tmp = tempfile::TempDir::new().unwrap();
        // A port nothing is listening on.
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some("http://127.0.0.1:1/unreachable".to_string()),
            extras: Default::default(),
        }];
        let report: Arc<dyn FlowSink> = Arc::new(NullSink);
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();

        let start = Instant::now();
        sink.write(&record("dispatch start")).unwrap();
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_millis(100), "write() must not block on the network, took {elapsed:?}");

        // The line stays in the outbox, cursor unchanged (nothing delivered yet).
        let outbox = &sink.rules[0].rule.outbox_path;
        assert_eq!(std::fs::read_to_string(outbox).unwrap().lines().count(), 1);
        assert_eq!(read_cursor(&sink.rules[0].rule.cursor_path), 0);
    }

    #[test]
    fn client_error_skipped_after_three_attempts_with_hook_failed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start().with_status_sequence([400, 400, 400]);
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            extras: Default::default(),
        }];

        #[derive(Default)]
        struct CapturingSink(Mutex<Vec<FlowRecord>>);
        impl FlowSink for CapturingSink {
            fn write(&self, record: &FlowRecord) -> Result<()> {
                self.0.lock().unwrap().push(record.clone());
                Ok(())
            }
            fn info(&self) -> SinkInfo {
                SinkInfo { kind: "Capturing".into(), config: Default::default(), children: vec![], raw_url: None }
            }
        }
        let capture = Arc::new(CapturingSink::default());
        let report: Arc<dyn FlowSink> = capture.clone();
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();
        sink.write(&record("dispatch error")).unwrap();

        assert!(wait_until(|| receiver.request_count() >= 3, Duration::from_secs(5)));
        assert!(
            wait_until(|| capture.0.lock().unwrap().iter().any(|r| r.action == "hook.failed"), Duration::from_secs(2)),
            "hook.failed emitted after 3 client-error attempts"
        );
        let failed = capture.0.lock().unwrap();
        let failed = failed.iter().find(|r| r.action == "hook.failed").unwrap();
        assert_eq!(failed.payload.as_ref().unwrap()["attempt"], 3);

        // Cursor advanced past the skipped line — it's gone from the pending queue.
        assert!(wait_until(
            || undelivered_line_count(&sink.rules[0].rule.outbox_path, read_cursor(&sink.rules[0].rule.cursor_path)) == 0,
            Duration::from_secs(2)
        ));
    }

    #[test]
    fn server_error_retried_then_delivered() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start().with_status_sequence([500, 500, 200]);
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            extras: Default::default(),
        }];

        #[derive(Default)]
        struct CapturingSink(Mutex<Vec<FlowRecord>>);
        impl FlowSink for CapturingSink {
            fn write(&self, record: &FlowRecord) -> Result<()> {
                self.0.lock().unwrap().push(record.clone());
                Ok(())
            }
            fn info(&self) -> SinkInfo {
                SinkInfo { kind: "Capturing".into(), config: Default::default(), children: vec![], raw_url: None }
            }
        }
        let capture = Arc::new(CapturingSink::default());
        let report: Arc<dyn FlowSink> = capture.clone();
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();
        sink.write(&record("dispatch error")).unwrap();

        assert!(wait_until(|| receiver.request_count() >= 3, Duration::from_secs(8)));
        assert!(
            wait_until(|| capture.0.lock().unwrap().iter().any(|r| r.action == "hook.fired"), Duration::from_secs(2)),
            "eventually delivered"
        );
        let fired = capture.0.lock().unwrap();
        let fired = fired.iter().find(|r| r.action == "hook.fired").unwrap();
        assert_eq!(fired.payload.as_ref().unwrap()["attempt"], 3, "500, 500, 200 = 3 attempts total");
    }

    #[test]
    fn restart_redelivers_persisted_outbox() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Bind a receiver, learn its port, then STOP it (drop) so the first
        // sink's write fails to deliver — but keep the port number to
        // rebind a second receiver on the exact same address.
        let probe = HookReceiver::start();
        let addr = probe.addr;
        drop(probe);

        let url = format!("http://{addr}/events");
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(url),
            extras: Default::default(),
        }];
        let report: Arc<dyn FlowSink> = Arc::new(NullSink);
        {
            let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report.clone()).unwrap();
            sink.write(&record("dispatch start")).unwrap();
            std::thread::sleep(Duration::from_millis(150)); // let the drainer try + fail at least once
        }

        // The outbox line must still be there (never delivered, never lost).
        let (outbox_path, cursor_path) = outbox_paths(tmp.path(), 0, &rules[0].http.clone().unwrap());
        assert_eq!(undelivered_line_count(&outbox_path, read_cursor(&cursor_path)), 1);

        // Rebind a receiver on the SAME address and construct a NEW sink —
        // `HookSink::new` must drain the persisted outbox without any
        // restart-specific code path; it's the same `new()` any process
        // start takes.
        let receiver = HookReceiver::start_on(addr);
        let sink2 = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();
        assert!(wait_until(|| receiver.request_count() >= 1, Duration::from_secs(3)));
        drop(sink2);
    }

    // ─── Naming ─────────────────────────────────────────────────────────

    #[test]
    fn outbox_and_cursor_paths_named_by_index_and_host_port() {
        let dir = PathBuf::from("/tmp/x");
        let (outbox, cursor) = outbox_paths(&dir, 2, "http://127.0.0.1:8790/events");
        assert_eq!(outbox, dir.join("2-127.0.0.1-8790.outbox.jsonl"));
        assert_eq!(cursor, dir.join("2-127.0.0.1-8790.cursor"));
    }
}
