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
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// (#2093 merge-gate finding 11) True when `action` is the sink's OWN
/// vocabulary (or close enough to it that letting it through would risk
/// a loop) — checked case-insensitively, and covering more than the
/// literal `hook.fired`/`hook.failed` strings: the bare word `hook` (no
/// dot), and the PLURAL `hooks.` prefix (a record naming the FEATURE,
/// which an operator's own rule could plausibly emit under, e.g.
/// `hooks.debug`) are refused too. A case-sensitive, singular-only
/// `starts_with("hook.")` check would let `HOOK.FIRED` or a bare `hook`
/// action straight through the guard it exists to be.
fn is_hook_own_action(action: &str) -> bool {
    let lower = action.to_ascii_lowercase();
    lower == "hook" || lower.starts_with("hook.") || lower.starts_with("hooks.")
}

/// True when `record` satisfies `m`. Records whose `action` is the
/// sink's own vocabulary (see `is_hook_own_action`) NEVER match,
/// regardless of `m` — the loop guard, checked first and
/// unconditionally. An all-`None` match matches nothing (`m.is_empty()`
/// short-circuits false) — an empty match is a rule an operator forgot to
/// fill in, not an accidental catch-all.
pub fn hook_match(m: &HookMatch, record: &FlowRecord) -> bool {
    if is_hook_own_action(&record.action) {
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
///
/// (#2093 merge-gate finding 1) Parses with `url::Url` rather than
/// `strip_prefix`/`split('/')` string-slicing — the slicing approach was
/// fooled by userinfo confusion (`http://localhost@evil.com/`, where the
/// slicer read the pre-`@` text as the host but the browser/`ureq`/every
/// real HTTP client reads it as the AUTHORITY'S username and `evil.com` as
/// the actual target) and by fragment confusion
/// (`http://evil.com#127.0.0.1`). Three checks, all against the PARSED
/// structure, never against `raw` byte-slices of the authority:
///
/// 1. `raw` must literally start with `http://` (lowercase, no leading
///    whitespace) — `url::Url` normalizes the scheme to lowercase and
///    trims surrounding whitespace, so checking `parsed.scheme()` alone
///    would silently accept `HTTP://` or a leading-space URL.
/// 2. The authority may carry NO userinfo — any non-empty username or
///    non-empty password refuses the whole URL, unconditionally, before
///    the host is even inspected.
/// 3. The host, exactly as `url::Url` resolves it, must be one of the
///    three canonical spellings (`127.0.0.1` / `[::1]` / `localhost`,
///    case-insensitive) — AND the literal text `raw` uses for the host
///    must match that canonical spelling byte-for-byte (case-insensitive).
///    The second half of check 3 is deliberate belt-and-braces: standard
///    URL host parsing canonicalizes shorthand/alternate IPv4 notations
///    (`127.1`, octal, hex, decimal) onto the same `Ipv4Addr` as
///    `127.0.0.1` — genuinely the same bits, not a distinct target — but
///    this allowlist accepts exactly the one blessed spelling per host,
///    not every notation that happens to canonicalize onto it.
pub fn validate_loopback_http_url(raw: &str) -> Result<()> {
    if !raw.starts_with("http://") {
        bail!(
            "hook URL `{raw}` must literally start with `http://` (lowercase, no leading \
             whitespace) and target loopback — a token-bearing remote hook is a later packet"
        );
    }
    let parsed = url::Url::parse(raw).with_context(|| format!("parsing hook URL `{raw}`"))?;
    if parsed.scheme() != "http" {
        bail!("hook URL `{raw}` must use http:// and target loopback");
    }
    if !parsed.username().is_empty() || parsed.password().is_some_and(|p| !p.is_empty()) {
        bail!(
            "hook URL `{raw}` may not carry userinfo (username/password) in its authority — \
             `user@host` is refused unconditionally, regardless of what host follows"
        );
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("hook URL `{raw}` has no host"))?
        .to_ascii_lowercase();
    let is_loopback = host == "127.0.0.1" || host == "[::1]" || host == "localhost";
    if !is_loopback {
        bail!(
            "hook URL `{raw}` targets non-loopback host `{host}` — only 127.0.0.1/[::1]/localhost \
             are allowed; a token-bearing remote hook is a later packet"
        );
    }
    let raw_host = raw_authority_host(raw)
        .with_context(|| format!("hook URL `{raw}` has a malformed authority"))?;
    if !raw_host.eq_ignore_ascii_case(&host) {
        bail!(
            "hook URL `{raw}` spells its host as `{raw_host}`, which is not the canonical form \
             `{host}` — only the exact canonical spelling of a loopback host is accepted, not an \
             alternate notation that merely resolves to it"
        );
    }
    Ok(())
}

/// The literal `host[:port]` → `host` text `raw` uses in its authority,
/// with NO normalization — the counterpart `validate_loopback_http_url`
/// compares against `url::Url`'s canonicalized `host_str()` to catch
/// alternate IPv4 notations that canonicalize onto the same address.
/// Safe to slice `raw` directly here ONLY because the caller has already
/// confirmed (via `url::Url`) that the authority carries no userinfo — an
/// `@`-bearing authority is refused before this function is ever called,
/// so there is no username/password segment left to be confused with the
/// host.
fn raw_authority_host(raw: &str) -> Result<String> {
    let rest = raw.strip_prefix("http://").ok_or_else(|| anyhow!("missing http:// prefix"))?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if let Some(bracketed) = authority.strip_prefix('[') {
        let close = bracketed.find(']').ok_or_else(|| anyhow!("unterminated IPv6 literal"))?;
        return Ok(format!("[{}]", &bracketed[..close]));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => Ok(host.to_string()),
        _ => Ok(authority.to_string()),
    }
}

/// The `host[:port]` portion of a `scheme://host[:port]/path` URL, or
/// `None` if `raw` doesn't start with `http://`.
fn extract_host_port(raw: &str) -> Option<&str> {
    raw.strip_prefix("http://").map(|rest| rest.split('/').next().unwrap_or(""))
}

/// Filesystem-safe form of a `host[:port]` string — folded into
/// `rule_key`'s output for readability (an operator `ls`-ing the outbox
/// dir can eyeball which host a file targets without decoding the hash).
fn sanitize_host_port(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '-' })
        .collect()
}

/// (#2093 merge-gate finding 15) A rule's stable filename KEY, derived
/// from its own content (`match` + `http`) rather than its ARRAY INDEX
/// in `hooks.rules`. Index-based naming has a real correctness bug, not
/// just a hygiene one: reordering (not even removing) a rule in config —
/// an operator inserting a new rule at the front, say — silently
/// reassigns rule A's outbox/cursor/counters to whatever rule now
/// occupies index A's OLD slot. A content hash ties the files to the
/// RULE'S IDENTITY, immune to reordering; two rules with genuinely
/// identical `match`+`http` collide on purpose (they'd be redundant
/// duplicates sharing one outbox, not two independent ones). BLAKE3
/// (already a `darkmux-flow` dependency for `AuditFileSink`'s hash
/// chain) truncated to 16 hex chars (64 bits) — filename-length, not a
/// security boundary, so this collision space is more than sufficient
/// for the number of rules an operator hand-writes.
pub fn rule_key(m: &HookMatch, url: &str) -> String {
    let canonical = serde_json::to_string(m).unwrap_or_default();
    let hash = blake3::hash(format!("{canonical}\u{0}{url}").as_bytes());
    let host_port = extract_host_port(url).unwrap_or("unknown");
    format!("{}-{}", sanitize_host_port(host_port), &hash.to_hex()[..16])
}

fn outbox_paths(outbox_dir: &Path, key: &str) -> (PathBuf, PathBuf) {
    (outbox_dir.join(format!("{key}.outbox.jsonl")), outbox_dir.join(format!("{key}.cursor")))
}

/// Sibling of `outbox_paths`' pair — where the rule's last-terminal-outcome
/// (success or give-up; never an ordinary retry) is recorded, for `darkmux
/// doctor` and `darkmux flow hooks status`'s "last delivery ts / last
/// error" columns. Same naming scheme, different suffix.
fn last_status_path(outbox_dir: &Path, key: &str) -> PathBuf {
    outbox_dir.join(format!("{key}.last"))
}

/// (#2093 merge-gate finding 3) Sibling of `outbox_paths`' pair — a
/// dedicated lock file the DRAINER (never the appender) takes
/// non-blockingly for the whole read-cursor → POST → write-cursor
/// sequence. Kept SEPARATE from the outbox file's own append lock
/// (`append_outbox_line`'s `flock`) on purpose: a POST can take up to
/// `POST_TIMEOUT` (5s), and an appender taking the SAME lock the drainer
/// holds during a POST would block `write()` for that long — the one
/// thing this sink promises never to do.
fn drain_lock_path(outbox_dir: &Path, key: &str) -> PathBuf {
    outbox_dir.join(format!("{key}.drain.lock"))
}

/// (#2093 merge-gate finding 9) Sibling of `outbox_paths`' pair — where
/// the LIVE `dropped_appends` counter is persisted, so a SEPARATE process
/// invocation (`darkmux doctor`, `darkmux flow hooks status`) can see
/// drops a currently- or previously-running dispatch process counted
/// in-memory. Plain text, same shape as the `.cursor` file.
fn dropped_appends_path(outbox_dir: &Path, key: &str) -> PathBuf {
    outbox_dir.join(format!("{key}.dropped"))
}

fn read_dropped_appends(path: &Path) -> u64 {
    fs::read_to_string(path).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

fn write_dropped_appends(path: &Path, count: u64) {
    if let Err(e) = fs::write(path, count.to_string()) {
        eprintln!("flow::HookSink: failed to persist dropped-appends count to {}: {e:#}", path.display());
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LastStatus {
    ts: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn write_last_status(path: &Path, ok: bool, error: Option<&str>) {
    let status = LastStatus { ts: schema::ts_utc_now(), ok, error: error.map(str::to_string) };
    if let Ok(json) = serde_json::to_string(&status) {
        if let Err(e) = fs::write(path, json) {
            eprintln!("flow::HookSink: failed to write last-status {}: {e:#}", path.display());
        }
    }
}

fn read_last_status(path: &Path) -> Option<LastStatus> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

// ─── Resolved rules ─────────────────────────────────────────────────────

/// A `HookRule` resolved against an outbox dir: validated URL + the outbox
/// / cursor / last-status file paths it owns. `HookSink::new` builds these
/// (and refuses the WHOLE sink on the first invalid rule);
/// `summarize_configured_rules` builds an unvalidated variant for read-only
/// introspection (doctor, `flow hooks status`) that never bails.
#[derive(Debug, Clone)]
pub struct ResolvedRule {
    pub index: usize,
    pub match_: HookMatch,
    pub url: String,
    pub outbox_path: PathBuf,
    pub cursor_path: PathBuf,
    pub last_status_path: PathBuf,
    /// (#2093 merge-gate finding 3) The drainer's own non-blocking
    /// mutual-exclusion file — see `drain_lock_path`'s doc.
    pub drain_lock_path: PathBuf,
    /// (#2093 merge-gate finding 9) Where the live `dropped_appends`
    /// counter is persisted — see `dropped_appends_path`'s doc.
    pub dropped_appends_path: PathBuf,
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
        let match_ = r.r#match.clone().unwrap_or_default();
        let key = rule_key(&match_, &url);
        let (outbox_path, cursor_path) = outbox_paths(outbox_dir, &key);
        let last_status_path = last_status_path(outbox_dir, &key);
        let drain_lock_path = drain_lock_path(outbox_dir, &key);
        let dropped_appends_path = dropped_appends_path(outbox_dir, &key);
        out.push(ResolvedRule {
            index,
            match_,
            url,
            outbox_path,
            cursor_path,
            last_status_path,
            drain_lock_path,
            dropped_appends_path,
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
    /// When the last delivery attempt reached a TERMINAL outcome (success or
    /// give-up — never an ordinary in-progress retry), the ISO-8601
    /// timestamp of that outcome. `None` until the first terminal outcome.
    pub last_delivery_ts: Option<String>,
    /// The error named by the last terminal outcome, when it was a
    /// give-up. `None` after a successful delivery, or before any terminal
    /// outcome has happened yet.
    pub last_error: Option<String>,
    /// (#2093 merge-gate finding 9) Writes refused for this rule so far —
    /// either the hard cap (finding 5) or an outbox append failure.
    /// Read from the PERSISTED counter (`dropped_appends_path`), so this
    /// is visible from a separate `darkmux doctor` / `flow hooks status`
    /// process invocation, not just a live in-process `HookSink`.
    pub dropped_appends: u64,
    /// (#2093 merge-gate finding 15) This rule's stable filename key —
    /// see `rule_key`'s doc. Exposed so a caller (`darkmux doctor`) can
    /// diff the set of CURRENT rules' keys against what's actually on
    /// disk in `outbox_dir` and name any `*.outbox.jsonl` file that
    /// belongs to no current rule (a rule since removed from config, or
    /// — before this fix — the artifact of an index-based reassignment).
    pub key: String,
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
            let key = rule_key(&m, &url);
            let (outbox_path, cursor_path) = outbox_paths(outbox_dir, &key);
            let cursor = read_cursor(&cursor_path);
            let undelivered = undelivered_line_count(&outbox_path, cursor);
            let last = read_last_status(&last_status_path(outbox_dir, &key));
            let dropped_appends = read_dropped_appends(&dropped_appends_path(outbox_dir, &key));
            HookRuleSummary {
                index,
                match_desc: describe_match(&m),
                url,
                is_loopback,
                is_empty_match: m.is_empty(),
                outbox_path,
                cursor_path,
                undelivered,
                last_delivery_ts: last.as_ref().map(|s| s.ts.clone()),
                last_error: last.and_then(|s| s.error),
                dropped_appends,
                key,
            }
        })
        .collect()
}

// ─── Outbox file I/O ────────────────────────────────────────────────────

/// Append `line` (a single record's serialized JSON, no trailing newline)
/// to the outbox at `path`, flock'd like `AuditFileSink` — cross-process
/// safe, and never torn against another writer (single `write_all` under
/// the lock, mirroring `audit_record_at_locked`).
///
/// **Deliberately no `fsync`** (unlike `AuditFileSink`, which calls
/// `sync_all()` for compliance-grade crash durability). Measured (#2093
/// Self-QA gate, `cost_check_write_latency_hooks_enabled_vs_disabled`):
/// with `sync_all()`, 10k matching writes took ~41s (≈4.1ms/write, ~11,000x
/// the disabled baseline's 0.36us/write) on this machine's disk — almost
/// entirely the fsync itself (macOS's `fsync` crosses the journal layer on
/// every call). Without it, the same 10k writes took ~450ms (≈45us/write,
/// ~120x baseline) — flock + seek + write, no disk-flush wait. The outbox
/// still needs to survive an ordinary PROCESS restart (a `HookSink` drop +
/// reconstruction) — page-cache-buffered writes survive that fine, no
/// fsync required; only a hard power-loss between the write and the next
/// cache flush could lose an unflushed line, a narrower risk than the
/// audit trail's regulatory-compliance requirement. This sink sits on the
/// same write path as every other flow record when hooks are enabled, so
/// the throughput cost of fsync-per-line is not proportionate to what it
/// buys here.
fn append_outbox_line(path: &Path, line: &str) -> Result<()> {
    darkmux_types::flock::with_locked_file(path, |file| {
        file.seek(SeekFrom::End(0)).with_context(|| format!("seek to end of {}", path.display()))?;
        // (#2093 merge-gate finding 4) ONE `write_all` of the combined
        // body + trailing newline, not two separate calls — shrinks the
        // crash/kill window from "between two syscalls" (where a torn
        // write leaves the body with no newline, silently gluing onto
        // the NEXT append) down to "mid one syscall".
        let mut buf = Vec::with_capacity(line.len() + 1);
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
        file.write_all(&buf).with_context(|| format!("appending to hook outbox {}", path.display()))?;
        Ok(())
    })
}

/// (#2093 merge-gate finding 4) If `path` exists and its last byte isn't
/// `\n`, append one — turning a torn prefix (left by a kill mid-write,
/// before this fix's single-`write_all` change existed, or a filesystem
/// that doesn't guarantee syscall atomicity) into its OWN complete-but-
/// invalid line, so the JSON-validation quarantine check below catches it
/// as a distinct malformed record instead of it silently gluing onto the
/// next real append (corrupting BOTH). Locked the same way an append is,
/// so this can't race a concurrent appender.
fn ensure_trailing_newline(path: &Path) -> Result<()> {
    darkmux_types::flock::with_locked_file(path, |file| {
        let len = file.seek(SeekFrom::End(0)).with_context(|| format!("seek to end of {}", path.display()))?;
        if len == 0 {
            return Ok(());
        }
        file.seek(SeekFrom::Start(len - 1)).with_context(|| format!("seeking {}", path.display()))?;
        let mut last = [0u8; 1];
        file.read_exact(&mut last).with_context(|| format!("reading last byte of {}", path.display()))?;
        if last[0] != b'\n' {
            file.seek(SeekFrom::End(0)).with_context(|| format!("seek to end of {}", path.display()))?;
            file.write_all(b"\n").with_context(|| format!("appending newline to {}", path.display()))?;
        }
        Ok(())
    })
}

/// (#2093 merge-gate finding 4) Sibling of an outbox path — where a line
/// that failed JSON validation is preserved (never silently dropped)
/// before its cursor position is skipped past.
fn quarantine_path(outbox_path: &Path) -> PathBuf {
    let mut s = outbox_path.as_os_str().to_os_string();
    s.push(".quarantine");
    PathBuf::from(s)
}

fn quarantine_line(outbox_path: &Path, line: &str) {
    let path = quarantine_path(outbox_path);
    match fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(line.as_bytes()).and_then(|_| f.write_all(b"\n")) {
                eprintln!("flow::HookSink: failed to quarantine invalid outbox line into {}: {e:#}", path.display());
            }
        }
        Err(e) => {
            eprintln!("flow::HookSink: failed to open quarantine file {}: {e:#}", path.display());
        }
    }
}

fn read_cursor(cursor_path: &Path) -> u64 {
    fs::read_to_string(cursor_path).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

/// (#2093 merge-gate finding 3, cursor-monotonicity corollary) Writes via a
/// sibling temp file + atomic `rename(2)` rather than `fs::write`'s
/// truncate-then-write — a concurrent `read_cursor` racing a plain
/// truncate-then-write can observe the file MID-TRUNCATE (empty, parsed as
/// `0`), which is exactly a cursor regression from the outside even though
/// nothing durable ever moved backward. Discovered by the drain-lock
/// test's cursor monitor: the drain lock alone prevents two drainers from
/// racing the SAME cursor write, but says nothing about a READER racing a
/// single writer's own two-syscall write — `rename` closes that gap by
/// making the visible update a single atomic filesystem operation.
fn write_cursor(cursor_path: &Path, offset: u64) -> Result<()> {
    if let Some(parent) = cursor_path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let tmp_path = cursor_path.with_extension("cursor.tmp");
    fs::write(&tmp_path, offset.to_string()).with_context(|| format!("writing {}", tmp_path.display()))?;
    fs::rename(&tmp_path, cursor_path).with_context(|| format!("renaming {} to {}", tmp_path.display(), cursor_path.display()))
}

/// The next fully-committed line at or after `cursor`, plus the byte
/// offset just past it — or `None` when nothing is pending, INCLUDING a
/// partially-written line still missing its trailing newline (never
/// delivered half a record; the next poll picks it up once complete).
/// (#2093 merge-gate finding 5) Reads via a `BufReader::read_until` from
/// `cursor`, stopping at the FIRST newline, rather than `read_to_string`-
/// ing the entire remaining tail into memory on every call. The old shape
/// was O(remaining outbox size) PER delivered line — with N pending lines
/// each call re-read everything after `cursor`, so draining N lines did
/// O(N × average-remaining-size) total work. `read_until` still only
/// returns once it finds `\n` (or hits EOF), so this call is O(this one
/// line's length), not O(everything left in the file).
fn next_pending_line(outbox_path: &Path, cursor: u64) -> Option<(String, u64)> {
    let mut file = fs::File::open(outbox_path).ok()?;
    file.seek(SeekFrom::Start(cursor)).ok()?;
    let mut reader = BufReader::new(file);
    let mut buf = Vec::new();
    let n = reader.read_until(b'\n', &mut buf).ok()?;
    if n == 0 || buf.last() != Some(&b'\n') {
        // EOF with nothing pending, OR a partial/torn tail with no
        // trailing newline yet — never delivered half a record.
        return None;
    }
    buf.pop(); // drop the trailing '\n' itself
    let line = String::from_utf8(buf).ok()?;
    Some((line, cursor + n as u64))
}

/// Count of fully-committed (newline-terminated) lines at or after
/// `cursor` — the "undelivered" count `darkmux doctor` / `flow hooks
/// status` report.
///
/// (#2093 merge-gate finding 5) Streams through a `BufReader` in fixed-
/// size chunks rather than `read_to_string`-ing the whole tail into one
/// `String` — bounded memory regardless of how large the undelivered tail
/// has grown, which matters most on exactly the unhealthy-receiver path
/// this count is meant to report on.
pub fn undelivered_line_count(outbox_path: &Path, cursor: u64) -> usize {
    let Ok(mut file) = fs::File::open(outbox_path) else {
        return 0;
    };
    if file.seek(SeekFrom::Start(cursor)).is_err() {
        return 0;
    }
    let mut reader = BufReader::new(file);
    let mut chunk = [0u8; 8192];
    let mut count = 0usize;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => count += chunk[..n].iter().filter(|&&b| b == b'\n').count(),
            Err(_) => break,
        }
    }
    count
}

/// (#2093 merge-gate finding 5) Bytes at or after `cursor` — the
/// UNDELIVERED size, what the hard cap and compaction threshold both
/// compare against. `0` when the file is missing or unreadable (matches
/// `undelivered_line_count`'s fail-open-to-zero shape).
fn undelivered_byte_len(outbox_path: &Path, cursor: u64) -> u64 {
    fs::metadata(outbox_path).map(|m| m.len()).unwrap_or(0).saturating_sub(cursor)
}

/// (#2093 merge-gate finding 5) True when a rule's CURRENT undelivered
/// bytes already exceed `max_outbox_mb`. The write path checks this
/// BEFORE appending — so a single write whose own body is larger than the
/// cap still lands once (current undelivered was under the cap before
/// it), and only a write landing AFTER the outbox is already over cap is
/// dropped. `max_outbox_mb` of `0` means "no cap" (never over).
fn rule_over_cap(outbox_path: &Path, cursor: u64, max_outbox_mb: u64) -> bool {
    if max_outbox_mb == 0 {
        return false;
    }
    undelivered_byte_len(outbox_path, cursor) > max_outbox_mb.saturating_mul(1024 * 1024)
}

/// (#2093 merge-gate finding 5) The compaction threshold: once a rule's
/// CURSOR (not undelivered size — a mostly-delivered outbox with a huge
/// already-consumed prefix wastes disk exactly the same way) crosses
/// this many bytes, rewrite the file down to just its undelivered tail
/// and reset the cursor to 0. 8 MiB by default; a real caller passes
/// `DEFAULT_COMPACTION_THRESHOLD_BYTES`, tests pass a small value so the
/// behavior is exercised without writing megabytes.
const DEFAULT_COMPACTION_THRESHOLD_BYTES: u64 = 8 * 1024 * 1024;

/// (#2093 merge-gate finding 5) Rewrite `outbox_path` down to just its
/// undelivered tail (from `cursor` onward) and reset the cursor to 0,
/// when the cursor has crossed `threshold_bytes` — an already-delivered
/// prefix a healthy receiver has long since consumed otherwise grows
/// forever. Takes the outbox file's OWN append lock (in addition to
/// whatever drain lock the caller already holds) so an appender can never
/// interleave with the rewrite: write undelivered bytes to a sibling temp
/// file, `rename` it atomically over the real outbox path (so a reader
/// mid-open sees either the whole old file or the whole new one, never a
/// partial rewrite), THEN reset the cursor — in that order, so a crash
/// between the rename and the cursor reset is recovered by
/// `undelivered_line_count`/`next_pending_line` simply reading from the
/// (still-correct, non-zero) old cursor against the ALREADY-repacked
/// file, which is a safe, if not immediately obvious, no-op: the
/// undelivered tail is now at offset 0, so the stale cursor briefly
/// overshoots and reports 0 pending until the next successful cycle
/// re-derives it — never data loss, worst case a temporary stall.
fn maybe_compact_outbox(outbox_path: &Path, cursor_path: &Path, threshold_bytes: u64) {
    let cursor = read_cursor(cursor_path);
    if cursor < threshold_bytes {
        return;
    }
    let result: Result<()> = (|| {
        let mut guard = darkmux_types::flock::lock_exclusive(outbox_path)?;
        // Re-check under the lock — another compaction (or a delivery
        // that hadn't landed yet when we read `cursor` above) may have
        // already moved the cursor since the caller's unlocked read.
        let cursor = read_cursor(cursor_path);
        if cursor < threshold_bytes {
            return Ok(());
        }
        let file = guard.file();
        file.seek(SeekFrom::Start(cursor)).with_context(|| format!("seeking {}", outbox_path.display()))?;
        let mut remaining = Vec::new();
        file.read_to_end(&mut remaining).with_context(|| format!("reading tail of {}", outbox_path.display()))?;
        let tmp_path = PathBuf::from(format!("{}.compact.tmp", outbox_path.display()));
        fs::write(&tmp_path, &remaining).with_context(|| format!("writing {}", tmp_path.display()))?;
        fs::rename(&tmp_path, outbox_path)
            .with_context(|| format!("renaming {} to {}", tmp_path.display(), outbox_path.display()))?;
        drop(guard);
        write_cursor(cursor_path, 0)
    })();
    if let Err(e) = result {
        eprintln!("flow::HookSink: outbox compaction failed for {}: {e:#}", outbox_path.display());
    }
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
    /// (#2093 merge-gate finding 2) A 3xx response — the receiver telling
    /// us to go elsewhere, which we refuse rather than follow. Treated as
    /// a PERMANENT failure (never retried), same as `ClientError`'s
    /// give-up path, but distinct so the emitted reason can name the
    /// status + redirect target host rather than "4xx". Carries the
    /// status code and the `Location` target's host (best-effort — "" if
    /// the header is absent or unparseable) for the `hook.failed` reason.
    RedirectRefused(u16, String),
    RetryableFailure,
}

/// (#2093 merge-gate finding 429/408) A 429 (Too Many Requests) or 408
/// (Request Timeout) is the RECEIVER asking us to back off and retry —
/// not a permanent rejection of this payload the way a 400/404/422 is.
/// Counting it toward `MAX_CLIENT_ERROR_ATTEMPTS`'s give-up threshold
/// would abandon a delivery the receiver explicitly asked us to retry.
fn is_retryable_client_status(code: u16) -> bool {
    code == 429 || code == 408
}

fn try_post(url: &str, body: &str) -> DeliveryOutcome {
    // (#2093 merge-gate finding 1, belt-and-braces) Re-validate at POST
    // time — the URL was already validated at `resolve_rules` /
    // `HookSink::new`, but a future refactor that plumbs a URL through a
    // new path (or a construction bug) must not get a free pass to the
    // network just because construction-time validation happened to run.
    // No listener is contacted when this fails: refused locally, treated
    // as a permanent (never-retried) failure like any other 4xx.
    if let Err(e) = validate_loopback_http_url(url) {
        eprintln!("flow::HookSink: try_post refusing to send — URL failed re-validation: {e:#}");
        return DeliveryOutcome::ClientError;
    }
    // (#2093 merge-gate finding 2) Redirects are never followed — a
    // redirect target is the RECEIVER telling us to go elsewhere, and
    // "elsewhere" is exactly the case `validate_loopback_http_url` exists
    // to gate. `ureq` with `redirects(0)` does NOT error on a 3xx; it
    // returns it as an `Ok` response with the 3xx status, so the status
    // must be checked on the `Ok` arm too, not just the `Err` arm.
    let agent = ureq::AgentBuilder::new().timeout(POST_TIMEOUT).redirects(0).build();
    match agent.post(url).set("Content-Type", "application/json").send_string(body) {
        Ok(resp) => {
            let status = resp.status();
            if (300..400).contains(&status) {
                let location = resp.header("Location").unwrap_or("");
                let target_host = url::Url::parse(location)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_string))
                    .unwrap_or_else(|| location.to_string());
                DeliveryOutcome::RedirectRefused(status, target_host)
            } else {
                // ureq only returns `Ok` for 2xx/3xx by default; any other
                // status here would already have been `Err(Status(..))`
                // below. Treat conservatively as success only for 2xx.
                DeliveryOutcome::Success
            }
        }
        Err(ureq::Error::Status(code, _resp)) if is_retryable_client_status(code) => DeliveryOutcome::RetryableFailure,
        Err(ureq::Error::Status(code, _resp)) if (400..500).contains(&code) => DeliveryOutcome::ClientError,
        Err(_) => DeliveryOutcome::RetryableFailure,
    }
}

struct RuleRuntime {
    rule: ResolvedRule,
    backoff: Mutex<Duration>,
    next_attempt: Mutex<Instant>,
    /// Attempts made against the CURRENT undelivered line — reset on
    /// success or give-up, so it never leaks across lines. Reported
    /// verbatim in the emitted `hook.fired`/`hook.failed` payload's
    /// `attempt` field — the TRUE count across every outcome kind
    /// (success, client error, retryable failure), for observability.
    attempt_count: Mutex<u32>,
    /// (#2093 merge-gate finding 6) CLIENT-ERROR attempts only, against the
    /// CURRENT undelivered line — the counter the give-up threshold
    /// (`MAX_CLIENT_ERROR_ATTEMPTS`) actually checks. Kept separate from
    /// `attempt_count` on purpose: a line that saw two retryable 500s and
    /// then one 400 has seen 3 total attempts but only ONE client error,
    /// and must not be abandoned after that single 4xx.
    client_error_count: Mutex<u32>,
    /// (#2093 merge-gate finding 5, doubles as finding 9's write-failure
    /// counter) Appends refused for this rule — either because its
    /// undelivered bytes were already over `hooks.max_outbox_mb` (finding
    /// 5), or because the outbox append itself failed (finding 9, e.g. an
    /// unwritable/full disk). Surfaced by `flow hooks status` and
    /// `doctor`; never reset — a monotonically growing count across the
    /// process lifetime is the honest shape for "how much did we lose."
    dropped_appends: AtomicU64,
    /// (#2093 merge-gate finding 5) Rate-limits the `hook.failed` emitted
    /// when the hard cap is active, to at most once per minute — a
    /// receiver that's been down for hours must not turn into one
    /// `hook.failed` record per dropped write.
    last_drop_warning: Mutex<Option<Instant>>,
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
    *rt.client_error_count.lock().unwrap() = 0;
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

    let action = if success { "hook.fired" } else { "hook.failed" };
    let rec = FlowRecord {
        ts: schema::ts_utc_now(),
        level: if success { Level::Info } else { Level::Error },
        category: Category::Machinery,
        tier: Tier::Local,
        stage: Stage::Ship,
        action: action.to_string(),
        handle: host,
        phase_id: None,
        session_id: None,
        source: Some("hook".to_string()),
        model: None,
        reasoning: None,
        mission_id: None,
        // (#2093 merge-gate finding 12) Left `None` here on purpose —
        // `crate::record_to` below is the SAME stamping path every other
        // producer's `flow::record()` call goes through, and it only
        // fills a field the caller left absent.
        machine_id: None,
        machine_uid: None,
        prev_hash: None,
        hash: None,
        payload: Some(payload),
        work_id: None,
        attempt: None,
    };
    if let Err(e) = crate::record_to(report_sink, rec) {
        eprintln!("flow::HookSink: failed to emit {action}: {e:#}");
    }
}

/// (#2093 merge-gate finding 5) Emit a rate-limited `hook.failed` naming
/// the total drop count for this rule — at most once per minute, so a
/// receiver that's been down for hours doesn't turn every dropped write
/// into its own flow record.
fn maybe_warn_dropped(rt: &RuleRuntime, report_sink: &dyn FlowSink, max_outbox_mb: u64, dropped_count: u64) {
    const WARNING_INTERVAL: Duration = Duration::from_secs(60);
    let now = Instant::now();
    {
        let mut last = rt.last_drop_warning.lock().unwrap();
        if let Some(prev) = *last {
            if now.duration_since(prev) < WARNING_INTERVAL {
                return;
            }
        }
        *last = Some(now);
    }
    let reason =
        format!("outbox over the {max_outbox_mb} MiB cap — {dropped_count} write(s) dropped for this rule so far");
    let host = extract_host_port(&rt.rule.url).unwrap_or("").to_string();
    let rec = FlowRecord {
        ts: schema::ts_utc_now(),
        level: Level::Error,
        category: Category::Machinery,
        tier: Tier::Local,
        stage: Stage::Ship,
        action: "hook.failed".to_string(),
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
        payload: Some(serde_json::json!({
            "rule_index": rt.rule.index,
            "target_host": extract_host_port(&rt.rule.url).unwrap_or(""),
            "error": reason,
            "dropped_count": dropped_count,
        })),
        work_id: None,
        attempt: None,
    };
    if let Err(e) = crate::record_to(report_sink, rec) {
        eprintln!("flow::HookSink: failed to emit hook.failed (dropped-append warning): {e:#}");
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
            // (#2093 merge-gate finding 3) Non-blocking — another
            // `HookSink` instance (or drainer thread) draining this SAME
            // rule's outbox right now just means this cycle is skipped;
            // the next poll tries again. Held across the ENTIRE
            // read-cursor → POST → write-cursor sequence below, so two
            // concurrent drainers can never both read the same pending
            // line, both POST it, and both advance the cursor.
            let Ok(Some(_drain_lock)) = darkmux_types::flock::try_lock_exclusive(&rt.rule.drain_lock_path) else {
                continue;
            };
            // (#2093 merge-gate finding 5) Compaction runs under the SAME
            // drain lock this iteration already holds — checked (and, at
            // most, performed) once per poll cycle per rule.
            maybe_compact_outbox(&rt.rule.outbox_path, &rt.rule.cursor_path, DEFAULT_COMPACTION_THRESHOLD_BYTES);
            let cursor = read_cursor(&rt.rule.cursor_path);
            let Some((line, new_cursor)) = next_pending_line(&rt.rule.outbox_path, cursor) else {
                continue;
            };
            did_work = true;
            // (#2093 merge-gate finding 4) A line that isn't valid JSON —
            // most likely a torn fragment that `ensure_trailing_newline`
            // turned into its own complete-but-malformed line at
            // construction — is never POSTed. Quarantine it (preserve the
            // raw bytes, never silently drop them), advance the cursor
            // past it so it doesn't block every line after it forever,
            // and emit `hook.failed` naming the reason.
            if serde_json::from_str::<serde_json::Value>(&line).is_err() {
                quarantine_line(&rt.rule.outbox_path, &line);
                let _ = write_cursor(&rt.rule.cursor_path, new_cursor);
                reset_backoff(rt);
                let reason = "invalid outbox line";
                write_last_status(&rt.rule.last_status_path, false, Some(reason));
                emit_hook_record(report_sink.as_ref(), false, &rt.rule, &line, 1, Some(reason));
                continue;
            }
            match try_post(&rt.rule.url, &line) {
                DeliveryOutcome::Success => {
                    let attempt = {
                        let mut c = rt.attempt_count.lock().unwrap();
                        *c += 1;
                        *c
                    };
                    let _ = write_cursor(&rt.rule.cursor_path, new_cursor);
                    reset_backoff(rt);
                    write_last_status(&rt.rule.last_status_path, true, None);
                    emit_hook_record(report_sink.as_ref(), true, &rt.rule, &line, attempt, None);
                }
                DeliveryOutcome::ClientError => {
                    let attempt = {
                        let mut c = rt.attempt_count.lock().unwrap();
                        *c += 1;
                        *c
                    };
                    // (#2093 merge-gate finding 6) The give-up threshold
                    // counts CLIENT-ERROR attempts only — a line that also
                    // saw retryable 5xx/network failures first must not be
                    // abandoned early because `attempt` (the mixed total)
                    // happened to cross 3.
                    let client_errors = {
                        let mut c = rt.client_error_count.lock().unwrap();
                        *c += 1;
                        *c
                    };
                    if client_errors >= MAX_CLIENT_ERROR_ATTEMPTS {
                        let _ = write_cursor(&rt.rule.cursor_path, new_cursor);
                        reset_backoff(rt);
                        let reason = format!("4xx response, skipped after {client_errors} client-error attempts");
                        write_last_status(&rt.rule.last_status_path, false, Some(&reason));
                        emit_hook_record(report_sink.as_ref(), false, &rt.rule, &line, attempt, Some(&reason));
                    } else {
                        apply_backoff(rt);
                    }
                }
                // (#2093 merge-gate finding 2) A redirect is a PERMANENT
                // failure — never retried, cursor advances immediately —
                // because following it would mean sending this record's
                // body to a receiver-chosen destination `resolve_rules`
                // never validated.
                DeliveryOutcome::RedirectRefused(status, target_host) => {
                    let attempt = {
                        let mut c = rt.attempt_count.lock().unwrap();
                        *c += 1;
                        *c
                    };
                    let _ = write_cursor(&rt.rule.cursor_path, new_cursor);
                    reset_backoff(rt);
                    let reason = format!("redirect refused: {status} to {target_host}");
                    write_last_status(&rt.rule.last_status_path, false, Some(&reason));
                    emit_hook_record(report_sink.as_ref(), false, &rt.rule, &line, attempt, Some(&reason));
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
            // (#2093 merge-gate finding 16) `unwrap_or_else(|e| e.into_inner())`
            // recovers from a poisoned lock rather than propagating a
            // second panic — a nudge signal is best-effort coordination,
            // never a correctness invariant, so a stale/lost signal from
            // recovering a poisoned lock is a harmless missed wakeup (the
            // next poll cycle catches up), while panicking here would
            // take the WHOLE drainer thread down silently.
            let (lock, cvar) = &*nudge;
            let pending = lock.lock().unwrap_or_else(|e| e.into_inner());
            if !*pending {
                let (mut pending, _timeout) =
                    cvar.wait_timeout(pending, POLL_INTERVAL).unwrap_or_else(|e| e.into_inner());
                *pending = false;
            } else {
                drop(pending);
                *lock.lock().unwrap_or_else(|e| e.into_inner()) = false;
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
    /// (#2093 merge-gate finding 5) The hard cap, read ONCE (live) at
    /// construction — `write()` checks every matching rule's current
    /// undelivered bytes against it before appending. `0` = no cap.
    max_outbox_mb: u64,
    /// (#2093 merge-gate finding 5) A second handle to the same sink the
    /// drainer thread's copy points at — `write()` needs its OWN copy to
    /// emit a rate-limited `hook.failed` when the hard cap drops an
    /// append, since that decision happens on the CALLER's thread, not
    /// the drainer's.
    report_sink: Arc<dyn FlowSink>,
}

impl HookSink {
    /// Resolve + validate `rules` against `outbox_dir` (bails on the first
    /// invalid rule — see `resolve_rules`), then start ONE background
    /// drainer thread that services every rule. `report_sink` is where
    /// `hook.fired`/`hook.failed` records land — see the module doc for why
    /// it's a snapshot of the OTHER sinks, not this one.
    pub fn new(rules: &[HookRule], outbox_dir: PathBuf, report_sink: Arc<dyn FlowSink>) -> Result<Self> {
        let resolved = resolve_rules(rules, &outbox_dir)?;
        // (#2093 merge-gate finding 4) Fix up a torn trailing line BEFORE
        // this process's drainer (or any appender) touches the file — see
        // `ensure_trailing_newline`'s doc. Best-effort: a failure here
        // (e.g. an unreadable outbox on a fresh install where the file
        // doesn't exist yet) is logged, not fatal — construction must not
        // brick over pre-existing on-disk damage.
        for r in &resolved {
            if let Err(e) = ensure_trailing_newline(&r.outbox_path) {
                eprintln!("flow::HookSink: failed to check/fix trailing newline on {}: {e:#}", r.outbox_path.display());
            }
        }
        let now = Instant::now();
        let rule_runtimes: Vec<Arc<RuleRuntime>> = resolved
            .into_iter()
            .map(|r| {
                Arc::new(RuleRuntime {
                    rule: r,
                    backoff: Mutex::new(INITIAL_BACKOFF),
                    next_attempt: Mutex::new(now),
                    attempt_count: Mutex::new(0),
                    client_error_count: Mutex::new(0),
                    dropped_appends: AtomicU64::new(0),
                    last_drop_warning: Mutex::new(None),
                })
            })
            .collect();

        let stop = Arc::new(AtomicBool::new(false));
        let nudge = Arc::new((Mutex::new(false), Condvar::new()));
        // (#2093 merge-gate finding 5) Read live, once, at construction —
        // matches every other config accessor's "env wins live" contract
        // at the one point this sink consults it; a running sink doesn't
        // re-poll config on every write.
        let max_outbox_mb = darkmux_types::config_access::hooks_max_outbox_mb();

        let thread_rules = rule_runtimes.clone();
        let thread_stop = stop.clone();
        let thread_nudge = nudge.clone();
        let thread_report_sink = report_sink.clone();
        let handle = std::thread::Builder::new()
            .name("hook-drainer".to_string())
            .spawn(move || drainer_loop(thread_rules, thread_stop, thread_nudge, thread_report_sink))
            .context("spawning hook drainer thread")?;

        Ok(Self {
            outbox_dir,
            rules: rule_runtimes,
            stop,
            nudge,
            drainer: Mutex::new(Some(handle)),
            max_outbox_mb,
            report_sink,
        })
    }

    pub fn outbox_dir(&self) -> &Path {
        &self.outbox_dir
    }

    /// (#2093 merge-gate finding 16) True while the background drainer
    /// thread is still running. A drainer that panicked (a bug, not the
    /// expected shutdown path — `Drop` takes the handle via `.take()`,
    /// which this correctly reports as "not alive" too, since there is
    /// no drainer left to be alive) would otherwise silently stop
    /// delivering with no signal anywhere an operator would see it;
    /// `flow hooks status` and `doctor` surface this.
    pub fn drainer_alive(&self) -> bool {
        match self.drainer.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            Some(handle) => !handle.is_finished(),
            None => false,
        }
    }
}

impl FlowSink for HookSink {
    fn write(&self, record: &FlowRecord) -> Result<()> {
        // Loop guard — never even considered against any rule. See
        // `is_hook_own_action`'s doc (#2093 merge-gate finding 11).
        if is_hook_own_action(&record.action) {
            return Ok(());
        }
        let line = serde_json::to_string(record).context("serializing record for hook outbox")?;
        let mut any = false;
        for rt in &self.rules {
            if hook_match(&rt.rule.match_, record) {
                // (#2093 merge-gate finding 5) Checked BEFORE appending —
                // a write landing while undelivered bytes are already
                // over the cap is dropped outright, never touching the
                // outbox file. `rule_over_cap` reads current on-disk size
                // fresh each time (cheap — one `stat`), so this stays
                // correct across compaction shrinking the file back down.
                let cursor = read_cursor(&rt.rule.cursor_path);
                if rule_over_cap(&rt.rule.outbox_path, cursor, self.max_outbox_mb) {
                    let dropped = rt.dropped_appends.fetch_add(1, Ordering::Relaxed) + 1;
                    write_dropped_appends(&rt.rule.dropped_appends_path, dropped);
                    maybe_warn_dropped(rt, self.report_sink.as_ref(), self.max_outbox_mb, dropped);
                    continue;
                }
                if let Err(e) = append_outbox_line(&rt.rule.outbox_path, &line) {
                    // (#2093 merge-gate finding 9) An append failure is
                    // counted the SAME way a cap-drop is — both are "this
                    // record never reached the outbox for this rule."
                    let dropped = rt.dropped_appends.fetch_add(1, Ordering::Relaxed) + 1;
                    write_dropped_appends(&rt.rule.dropped_appends_path, dropped);
                    eprintln!(
                        "flow::HookSink: rule #{} outbox append failed: {e:#} (this write is lost \
                         for that rule; other rules + other sinks are unaffected; {dropped} dropped so far)",
                        rt.rule.index
                    );
                } else {
                    any = true;
                }
            }
        }
        if any {
            let (lock, cvar) = &*self.nudge;
            *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
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
            config.insert(format!("rule{idx}_dropped_appends"), rt.dropped_appends.load(Ordering::Relaxed).to_string());
        }
        config.insert("drainer_alive".to_string(), self.drainer_alive().to_string());
        SinkInfo { kind: "Hooks".to_string(), config, children: vec![], raw_url: None }
    }
}

impl Drop for HookSink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        {
            let (lock, cvar) = &*self.nudge;
            *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
            cvar.notify_all();
        }
        let Some(handle) = self.drainer.lock().unwrap_or_else(|e| e.into_inner()).take() else {
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
        redirect_location: Arc<Mutex<Option<String>>>,
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
            let redirect_location = Arc::new(Mutex::new(None));
            let stop = Arc::new(AtomicBool::new(false));

            let thread_received = received.clone();
            let thread_statuses = statuses.clone();
            let thread_redirect_location = redirect_location.clone();
            let thread_stop = stop.clone();
            let handle = std::thread::spawn(move || {
                loop {
                    if thread_stop.load(Ordering::Acquire) {
                        return;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = handle_one(stream, &thread_received, &thread_statuses, &thread_redirect_location);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        Err(_) => return,
                    }
                }
            });

            Self { addr, received, statuses, redirect_location, stop, handle: Some(handle) }
        }

        /// Queue a sequence of HTTP status codes to return, one per
        /// request; the LAST entry repeats once the queue is exhausted.
        pub fn with_status_sequence(self, statuses: impl IntoIterator<Item = u16>) -> Self {
            *self.statuses.lock().unwrap() = statuses.into_iter().collect();
            self
        }

        /// When a queued status is 3xx, answer with this `Location` header
        /// — for tests proving a redirect is refused rather than followed.
        pub fn with_redirect_location(self, location: &str) -> Self {
            *self.redirect_location.lock().unwrap() = Some(location.to_string());
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
        redirect_location: &Arc<Mutex<Option<String>>>,
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
            302 => "Found",
            307 => "Temporary Redirect",
            400 => "Bad Request",
            404 => "Not Found",
            408 => "Request Timeout",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            _ => "Status",
        };
        let location_header = if (300..400).contains(&status) {
            redirect_location.lock().unwrap().as_ref().map(|loc| format!("Location: {loc}\r\n")).unwrap_or_default()
        } else {
            String::new()
        };
        let resp = format!("HTTP/1.1 {status} {reason}\r\n{location_header}Content-Length: 0\r\nConnection: close\r\n\r\n");
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

    /// (#2093 merge-gate finding 11) The loop guard must catch case
    /// variants and near-miss spellings a naive `starts_with("hook.")`
    /// lets through: an upper/mixed-case `HOOK.FIRED`, the bare word
    /// `hook` with no dot at all, and the PLURAL `hooks.` prefix (a
    /// record naming the feature, not the sink's own vocabulary).
    #[test]
    fn hook_actions_never_match_case_insensitively_or_bare_or_plural_prefix() {
        let vectors = ["HOOK.FIRED", "Hook.Failed", "hook", "hooks.status"];
        for action in vectors {
            let r = record(action);
            assert!(
                !hook_match(&HookMatch { action: Some("*".to_string()), ..Default::default() }, &r),
                "must be excluded by the loop guard: {action}"
            );
        }
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

    /// (#2093 merge-gate finding 1) The reviewer's exact vectors: a real
    /// `url::Url` parse replaces the old `strip_prefix`/`split('/')`
    /// string-slicing, which was fooled by userinfo confusion
    /// (`user@evil.com`), fragment confusion (`evil.com#127.0.0.1`), and
    /// suffix confusion (`localhost.evil.com`). Every one of these must be
    /// REFUSED.
    #[test]
    fn validate_loopback_http_url_refuses_every_reviewer_bypass_vector() {
        let refused = [
            "http://[::1]@192.168.1.5:18901/x",
            "http://[::1]@evil.com/x",
            "http://[::1]:80@evil.com/x",
            "http://127.0.0.1:80@evil.com/x",
            "http://127.0.0.1:8790@evil.com/x",
            "http://localhost:80@evil.com/x",
            "http://127.0.0.1:1@169.254.169.254/latest/meta-data",
            "http://localhost.evil.com/",
            "http://127.0.0.1.evil.com/",
            "http://0.0.0.0/",
            "http://127.1/",
            "http://localhost@evil.com/",
            "http://evil.com#127.0.0.1",
            "http://user:pass@127.0.0.1/",
            "https://127.0.0.1/",
            "HTTP://127.0.0.1/",
            " http://127.0.0.1/",
            "http://[::ffff:127.0.0.1]/",
        ];
        for raw in refused {
            assert!(validate_loopback_http_url(raw).is_err(), "must be REFUSED: {raw}");
        }
    }

    #[test]
    fn validate_loopback_http_url_accepts_the_reviewer_allowlist() {
        let accepted = ["http://127.0.0.1:8790/events", "http://localhost:8790/x", "http://[::1]:8790/x"];
        for raw in accepted {
            assert!(validate_loopback_http_url(raw).is_ok(), "must be ACCEPTED: {raw}");
        }
    }

    #[test]
    fn last_status_summary_reflects_delivery_outcome() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            extras: Default::default(),
        }];

        // Before any delivery: no last-status yet.
        let summaries = summarize_configured_rules(&rules, tmp.path());
        assert_eq!(summaries[0].last_delivery_ts, None);
        assert_eq!(summaries[0].last_error, None);

        let report: Arc<dyn FlowSink> = Arc::new(NullSink);
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();
        sink.write(&record("dispatch start")).unwrap();
        assert!(wait_until(|| receiver.request_count() == 1, Duration::from_secs(3)));

        // Give the drainer a moment to persist the last-status file after the
        // successful POST.
        let ok = wait_until(
            || summarize_configured_rules(&rules, tmp.path())[0].last_delivery_ts.is_some(),
            Duration::from_secs(2),
        );
        assert!(ok, "last_delivery_ts populated after a successful delivery");
        let summaries = summarize_configured_rules(&rules, tmp.path());
        assert!(summaries[0].last_error.is_none(), "a successful delivery clears/omits last_error");
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
        {
            let guard = capture.0.lock().unwrap();
            let fired = guard.iter().find(|r| r.action == "hook.fired").unwrap();
            assert_eq!(fired.payload.as_ref().unwrap()["delivered_action"], "crawl.finding");
        }

        // The cursor must have ADVANCED past the delivered line: without
        // that, the drainer treats it as still-pending and redelivers it on
        // every poll. Give it several poll cycles' worth of time and assert
        // the request count never grows past 1 — this is what actually
        // proves the cursor advanced (a plain "== 1 eventually" check would
        // pass instantaneously and race right past a redelivery loop).
        std::thread::sleep(POLL_INTERVAL * 10);
        assert_eq!(receiver.request_count(), 1, "cursor must advance so the delivered line is never resent");
        assert_eq!(read_cursor(&sink.rules[0].rule.cursor_path), std::fs::read_to_string(&sink.rules[0].rule.outbox_path).unwrap().len() as u64);
    }

    #[test]
    fn down_receiver_does_not_block_write() {
        let tmp = tempfile::TempDir::new().unwrap();
        // A "black hole" receiver: bound (the TCP handshake completes via
        // the kernel's own listen backlog) but never `.accept()`'d, so
        // nothing ever reads the request or answers it. This — not a
        // REFUSED port — is what actually proves write() doesn't block on
        // the network: a refused connection fails near-instantly
        // regardless of whether the caller is sync or async, so it would
        // let a synchronous-POST-on-write() mutation slip through
        // undetected. Kept alive for the whole test (never accepted, never
        // dropped early) so the connect+write phases genuinely hang.
        let black_hole = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = black_hole.local_addr().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(format!("http://{addr}/unreachable")),
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

    // ─── (#2093 merge-gate finding 2) No redirects; explicit status ──────

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

    /// A redirect target the drainer must NEVER connect to — bound but with
    /// no accept loop running, so a pending-connection check after the fact
    /// proves nothing ever reached it (a refused connection would fail
    /// near-instantly either way; only an unaccepted-but-bound listener
    /// distinguishes "never even tried" from "tried and it happened to be
    /// unreachable").
    fn assert_never_contacted(listener: &std::net::TcpListener) {
        listener.set_nonblocking(true).unwrap();
        match listener.accept() {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {} // nothing pending — never contacted
            other => panic!("redirect target was contacted, expected nothing pending: {other:?}"),
        }
    }

    #[test]
    fn redirect_302_refused_as_permanent_failure_never_followed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let attacker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let attacker_addr = attacker.local_addr().unwrap();
        let receiver = HookReceiver::start()
            .with_status_sequence([302])
            .with_redirect_location(&format!("http://{attacker_addr}/evil"));
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            extras: Default::default(),
        }];
        let capture = Arc::new(CapturingSink::default());
        let report: Arc<dyn FlowSink> = capture.clone();
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();
        sink.write(&record("dispatch start")).unwrap();

        assert!(
            wait_until(|| capture.0.lock().unwrap().iter().any(|r| r.action == "hook.failed"), Duration::from_secs(3)),
            "a 3xx must be treated as a PERMANENT failure, not retried forever"
        );
        {
            let guard = capture.0.lock().unwrap();
            let failed = guard.iter().find(|r| r.action == "hook.failed").unwrap();
            let err = failed.payload.as_ref().unwrap()["error"].as_str().unwrap_or_default();
            assert!(err.contains("redirect refused"), "reason should name the redirect refusal: {err}");
            assert!(err.contains("302"), "reason should name the status: {err}");
        }
        // The line must never be retried after a redirect — cursor advanced.
        assert!(wait_until(
            || undelivered_line_count(&sink.rules[0].rule.outbox_path, read_cursor(&sink.rules[0].rule.cursor_path)) == 0,
            Duration::from_secs(2)
        ));
        assert_never_contacted(&attacker);
    }

    #[test]
    fn redirect_307_refused_as_permanent_failure_never_followed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let attacker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let attacker_addr = attacker.local_addr().unwrap();
        let receiver = HookReceiver::start()
            .with_status_sequence([307])
            .with_redirect_location(&format!("http://{attacker_addr}/evil"));
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            extras: Default::default(),
        }];
        let capture = Arc::new(CapturingSink::default());
        let report: Arc<dyn FlowSink> = capture.clone();
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();
        sink.write(&record("dispatch start")).unwrap();

        assert!(
            wait_until(|| capture.0.lock().unwrap().iter().any(|r| r.action == "hook.failed"), Duration::from_secs(3)),
            "307 must also be refused as a permanent failure"
        );
        assert_never_contacted(&attacker);
    }

    // ─── (#2093 merge-gate finding 16) nudge-mutex poison recovery + dead-drainer detection ─

    #[test]
    fn nudge_mutex_recovers_from_poison_instead_of_panicking() {
        // Simulate a drainer that panicked while holding the nudge lock —
        // exactly the scenario `.lock().unwrap()` would propagate as a
        // SECOND panic on the next locker. Tests the recovery PATTERN in
        // isolation (a `(Mutex<bool>, Condvar)` shaped exactly like
        // `HookSink`'s own `nudge` field) rather than injecting a panic
        // into the real drainer thread, which the production code has no
        // hook for.
        let nudge: Arc<(Mutex<bool>, Condvar)> = Arc::new((Mutex::new(false), Condvar::new()));
        let poison_nudge = nudge.clone();
        let joined = std::thread::spawn(move || {
            let (lock, _cvar) = &*poison_nudge;
            let _guard = lock.lock().unwrap();
            panic!("simulated drainer panic while holding the nudge lock");
        })
        .join();
        assert!(joined.is_err(), "the thread must have actually panicked, poisoning the mutex");

        // The SAME recovery pattern `hooks.rs` now uses at every nudge
        // lock site — must recover, not panic a second time.
        let (lock, _cvar) = &*nudge;
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        assert!(!*guard, "recovered the last-written value instead of panicking on poison");
    }

    #[test]
    fn drainer_alive_reports_running_then_false_after_stop() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            extras: Default::default(),
        }];
        let report: Arc<dyn FlowSink> = Arc::new(NullSink);
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();
        assert!(sink.drainer_alive(), "the drainer thread must be running right after construction");

        let info = sink.info();
        assert_eq!(info.config.get("drainer_alive").map(String::as_str), Some("true"));

        // Signal stop and join the drainer directly — the same mechanics
        // `Drop` uses — WITHOUT dropping the whole `sink`, so `drainer_alive()`
        // can actually be observed flipping to false once the thread has
        // genuinely stopped (a full `drop(sink)` would consume `sink`,
        // making it impossible to call anything on it afterward).
        sink.stop.store(true, Ordering::Release);
        {
            let (lock, cvar) = &*sink.nudge;
            *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
            cvar.notify_all();
        }
        let handle = sink.drainer.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap();
        handle.join().unwrap();
        assert!(!sink.drainer_alive(), "drainer_alive must report false once the thread has actually stopped");
    }

    // ─── (#2093 merge-gate finding 12) hook.fired/failed carry machine provenance ─

    #[test]
    fn emitted_hook_records_carry_machine_id_and_uid_like_every_other_producer() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            extras: Default::default(),
        }];
        let capture = Arc::new(CapturingSink::default());
        let report: Arc<dyn FlowSink> = capture.clone();
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();
        sink.write(&record("dispatch start")).unwrap();

        assert!(wait_until(|| capture.0.lock().unwrap().iter().any(|r| r.action == "hook.fired"), Duration::from_secs(3)));
        let guard = capture.0.lock().unwrap();
        let fired = guard.iter().find(|r| r.action == "hook.fired").unwrap();
        assert!(
            fired.machine_id.is_some(),
            "hook.fired must go through the same stamping path every other producer uses (machine_id present)"
        );
    }

    // ─── (#2093 merge-gate finding 5) bounded outbox ──────────────────────

    #[test]
    fn maybe_compact_outbox_rewrites_undelivered_tail_and_resets_cursor() {
        let tmp = tempfile::TempDir::new().unwrap();
        let outbox_path = tmp.path().join("0-x.outbox.jsonl");
        let cursor_path = tmp.path().join("0-x.cursor");

        // 5 delivered lines (before the cursor) + 3 undelivered lines
        // (after it). A small injectable threshold — well under the real
        // 8 MiB default — makes this test fast and deterministic.
        let delivered = "{\"n\":0}\n{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n{\"n\":4}\n";
        let undelivered = "{\"n\":5}\n{\"n\":6}\n{\"n\":7}\n";
        std::fs::write(&outbox_path, format!("{delivered}{undelivered}")).unwrap();
        write_cursor(&cursor_path, delivered.len() as u64).unwrap();

        maybe_compact_outbox(&outbox_path, &cursor_path, 10); // threshold: 10 bytes

        assert_eq!(read_cursor(&cursor_path), 0, "cursor resets to 0 — the compacted file starts fresh");
        let content = std::fs::read_to_string(&outbox_path).unwrap();
        assert_eq!(content, undelivered, "only the undelivered tail survives compaction");
        assert_eq!(undelivered_line_count(&outbox_path, 0), 3, "same 3 pending lines, just repacked into a smaller file");
    }

    #[test]
    fn maybe_compact_outbox_is_a_noop_below_threshold() {
        let tmp = tempfile::TempDir::new().unwrap();
        let outbox_path = tmp.path().join("0-x.outbox.jsonl");
        let cursor_path = tmp.path().join("0-x.cursor");
        std::fs::write(&outbox_path, "{\"n\":0}\n{\"n\":1}\n").unwrap();
        write_cursor(&cursor_path, 8).unwrap();

        maybe_compact_outbox(&outbox_path, &cursor_path, 10_000_000); // way above cursor

        assert_eq!(read_cursor(&cursor_path), 8, "cursor untouched below threshold");
        assert_eq!(std::fs::read_to_string(&outbox_path).unwrap(), "{\"n\":0}\n{\"n\":1}\n", "file untouched below threshold");
    }

    #[test]
    fn rule_over_cap_compares_undelivered_bytes_against_the_mib_cap() {
        let tmp = tempfile::TempDir::new().unwrap();
        let outbox_path = tmp.path().join("0-x.outbox.jsonl");
        let over_cap_body = "x".repeat(2 * 1024 * 1024); // 2 MiB of undelivered bytes
        std::fs::write(&outbox_path, format!("{{\"a\":\"{over_cap_body}\"}}\n")).unwrap();
        assert!(rule_over_cap(&outbox_path, 0, 1), "2 MiB of undelivered bytes must be over a 1 MiB cap");
        assert!(!rule_over_cap(&outbox_path, 0, 100), "must NOT be over a 100 MiB cap");
    }

    #[test]
    fn hook_write_drops_appends_past_the_cap_and_counts_them() {
        let tmp = tempfile::TempDir::new().unwrap();
        // A "black hole" target: bound but never accepted, so nothing is
        // ever delivered — every write stays undelivered, letting the
        // outbox grow past the cap deterministically.
        let black_hole = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = black_hole.local_addr().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(format!("http://{addr}/unreachable")),
            extras: Default::default(),
        }];

        let prev = std::env::var("DARKMUX_HOOKS_MAX_OUTBOX_MB").ok();
        // `HookSink::new` reads the cap ONCE, live, at construction —
        // matches `config_access`'s general "env read live per access"
        // rule, applied at the one place this sink reads it.
        unsafe {
            std::env::set_var("DARKMUX_HOOKS_MAX_OUTBOX_MB", "1");
        }
        let report: Arc<dyn FlowSink> = Arc::new(NullSink);
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOOKS_MAX_OUTBOX_MB", v),
                None => std::env::remove_var("DARKMUX_HOOKS_MAX_OUTBOX_MB"),
            }
        }

        // The cap check reads CURRENT undelivered bytes BEFORE this
        // write, so the first write that itself pushes the outbox over
        // the cap still lands (current undelivered was 0, under the
        // cap) — only a write that lands AFTER the outbox is already
        // over cap is dropped. Write one big (2 MiB) record first (goes
        // through, pushes undelivered to ~2 MiB), then one small record
        // (must be dropped, since undelivered is now already over the 1
        // MiB cap).
        let big_reasoning = "x".repeat(2 * 1024 * 1024);
        let mut big = record("work.big");
        big.reasoning = Some(big_reasoning);
        sink.write(&big).unwrap();
        assert_eq!(
            undelivered_line_count(&sink.rules[0].rule.outbox_path, 0),
            1,
            "the first (over-cap-pushing) write lands — the check is against bytes BEFORE this write"
        );

        sink.write(&record("work.small")).unwrap();
        assert_eq!(
            undelivered_line_count(&sink.rules[0].rule.outbox_path, 0),
            1,
            "the second write must be DROPPED — undelivered bytes are already over the 1 MiB cap"
        );
        assert_eq!(sink.rules[0].dropped_appends.load(Ordering::Relaxed), 1, "the drop must be counted");

        // (#2093 merge-gate finding 9) The drop must be visible to a
        // SEPARATE process invocation, not just this in-process counter
        // — `summarize_configured_rules` (what `flow hooks status` /
        // `doctor` actually call) reads it fresh from disk.
        let summaries = summarize_configured_rules(&rules, tmp.path());
        assert_eq!(summaries[0].dropped_appends, 1, "cross-process visible via the persisted counter");
    }

    // ─── (#2093 merge-gate finding 4) torn-line safety ───────────────────

    /// The reviewer's phase A/B scenario: phase A simulates a crash
    /// mid-write (a torn line with no trailing newline, written DIRECTLY
    /// to the outbox file, bypassing `append_outbox_line`); phase B
    /// constructs a `HookSink` on top of that pre-existing damage, then a
    /// normal `write()` appends one valid record. Expected: the torn
    /// fragment is quarantined (never delivered, never blocks the line
    /// after it), exactly one valid delivery happens, and no `hook.fired`
    /// is ever emitted for the torn line.
    #[test]
    fn torn_line_at_construction_is_quarantined_not_glued_or_delivered() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            extras: Default::default(),
        }];
        let key = rule_key(&rules[0].r#match.clone().unwrap_or_default(), &rules[0].http.clone().unwrap());
        let (outbox_path, _cursor_path) = outbox_paths(tmp.path(), &key);

        // Phase A — simulate a crash mid-write: a truncated JSON fragment
        // with NO trailing newline, written directly (not through
        // `append_outbox_line`).
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(&outbox_path, br#"{"action":"work.torn","unterminat"#).unwrap();

        // Phase B — construct a HookSink on top of the pre-existing
        // damage, then append one normal, valid record.
        let capture = Arc::new(CapturingSink::default());
        let report: Arc<dyn FlowSink> = capture.clone();
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();
        sink.write(&record("work.valid")).unwrap();

        assert!(wait_until(|| receiver.request_count() >= 1, Duration::from_secs(5)));
        // Give any (incorrect) further delivery attempt time to happen.
        std::thread::sleep(POLL_INTERVAL * 5);
        assert_eq!(receiver.request_count(), 1, "only the valid line was ever POSTed — the torn line must never reach the network");
        let delivered: serde_json::Value = serde_json::from_str(&receiver.bodies()[0]).unwrap();
        assert_eq!(delivered["action"], "work.valid");

        assert!(wait_until(|| capture.0.lock().unwrap().iter().any(|r| r.action == "hook.failed"), Duration::from_secs(3)));
        assert!(wait_until(|| capture.0.lock().unwrap().iter().any(|r| r.action == "hook.fired"), Duration::from_secs(3)));
        let guard = capture.0.lock().unwrap();
        let fired: Vec<_> = guard.iter().filter(|r| r.action == "hook.fired").collect();
        assert_eq!(fired.len(), 1, "exactly one hook.fired — never for the torn line");
        assert_eq!(fired[0].payload.as_ref().unwrap()["delivered_action"], "work.valid");
        let failed: Vec<_> = guard.iter().filter(|r| r.action == "hook.failed").collect();
        assert_eq!(failed.len(), 1, "exactly one hook.failed — the quarantined torn line");
        let reason = failed[0].payload.as_ref().unwrap()["error"].as_str().unwrap_or_default();
        assert_eq!(reason, "invalid outbox line");
        drop(guard);

        let quarantine_path = PathBuf::from(format!("{}.quarantine", outbox_path.display()));
        assert!(quarantine_path.exists(), "the torn line must be preserved in a quarantine file, not silently dropped");
        let quarantined = std::fs::read_to_string(&quarantine_path).unwrap();
        assert!(quarantined.contains("work.torn"), "quarantine file should contain the torn fragment: {quarantined}");
    }

    #[test]
    fn append_outbox_line_is_a_single_write_syscall_worth_of_bytes() {
        // A structural smoke test, not a proof of atomicity — no
        // in-process unit test can inject a kill between two syscalls.
        // `append_outbox_line` hands the OS one combined buffer (body +
        // trailing newline) rather than two separate `write_all` calls,
        // which shrinks the crash window from "between two syscalls" to
        // "mid one syscall" (a single `write(2)` to a local disk file is
        // effectively atomic for records this size). This test only
        // confirms the happy-path bytes are still correct after the
        // change — it will NOT go red if reverted to two calls, since the
        // two-call sequence produces the same final bytes when nothing
        // interrupts it. The real proof-by-recovery is
        // `torn_line_at_construction_is_quarantined_not_glued_or_delivered`,
        // which simulates the RESULT of a kill mid-append directly.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("x.outbox.jsonl");
        append_outbox_line(&path, r#"{"a":1}"#).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}\n");
    }

    // ─── (#2093 merge-gate finding 6) client-error attempts counted per line ─

    #[test]
    fn client_error_giveup_threshold_counts_only_client_errors_not_mixed_attempts() {
        let tmp = tempfile::TempDir::new().unwrap();
        // 500 (retryable) then 400 repeating forever — a mixed sequence.
        // After the first two responses, only ONE is a 4xx; the give-up
        // threshold (3) must count client errors, not total attempts.
        let receiver = HookReceiver::start().with_status_sequence([500, 400, 400]);
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            extras: Default::default(),
        }];
        let capture = Arc::new(CapturingSink::default());
        let report: Arc<dyn FlowSink> = capture.clone();
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();
        sink.write(&record("dispatch error")).unwrap();

        // After 500, 400 — exactly one client error so far, one retryable.
        assert!(wait_until(|| receiver.request_count() >= 2, Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            undelivered_line_count(&sink.rules[0].rule.outbox_path, read_cursor(&sink.rules[0].rule.cursor_path)),
            1,
            "must NOT give up after just one 4xx mixed in with a retryable failure"
        );
        assert!(
            !capture.0.lock().unwrap().iter().any(|r| r.action == "hook.failed"),
            "no hook.failed yet — only 1 of 3 required client errors observed (2 total attempts)"
        );

        // The sequence repeats its last entry (400) forever, so two more
        // requests reach the true 3-client-error threshold and give up.
        // Exponential backoff (1s, 2s, 4s between attempts) means this can
        // take several seconds — bound generously rather than tightening
        // the assertion window.
        assert!(
            wait_until(|| capture.0.lock().unwrap().iter().any(|r| r.action == "hook.failed"), Duration::from_secs(15)),
            "gives up once 3 client errors (not 3 total attempts) are observed"
        );
    }

    // ─── (#2093 merge-gate finding 7) 429/408 are retryable, not permanent ──

    #[test]
    fn status_429_is_retried_not_treated_as_client_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start().with_status_sequence([429, 429, 429, 200]);
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            extras: Default::default(),
        }];
        let capture = Arc::new(CapturingSink::default());
        let report: Arc<dyn FlowSink> = capture.clone();
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();
        sink.write(&record("dispatch start")).unwrap();

        // 3 consecutive 429s would exceed MAX_CLIENT_ERROR_ATTEMPTS (3) if
        // miscounted as client errors — it must NOT give up, and must
        // eventually succeed on the 4th (200) response.
        assert!(
            wait_until(|| capture.0.lock().unwrap().iter().any(|r| r.action == "hook.fired"), Duration::from_secs(8)),
            "429 is retryable — must eventually succeed, never give up as a client error"
        );
        assert!(!capture.0.lock().unwrap().iter().any(|r| r.action == "hook.failed"), "429 must never produce hook.failed");
    }

    #[test]
    fn status_408_is_retried_not_treated_as_client_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start().with_status_sequence([408, 200]);
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            extras: Default::default(),
        }];
        let capture = Arc::new(CapturingSink::default());
        let report: Arc<dyn FlowSink> = capture.clone();
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();
        sink.write(&record("dispatch start")).unwrap();

        assert!(wait_until(|| capture.0.lock().unwrap().iter().any(|r| r.action == "hook.fired"), Duration::from_secs(5)));
    }

    #[test]
    fn try_post_revalidates_the_url_before_every_send() {
        // Belt-and-braces (finding 1): `try_post` must refuse to send when
        // its URL argument fails `validate_loopback_http_url`, WITHOUT
        // attempting a network call. There is no listener at all behind
        // this URL — if `try_post` tried to actually connect, it would
        // hang on connection refused / DNS, not return promptly.
        let start = Instant::now();
        let outcome = try_post("http://evil.example.com/x", "{}");
        assert!(start.elapsed() < Duration::from_millis(500), "must refuse locally, never attempt the network");
        assert!(matches!(outcome, DeliveryOutcome::ClientError), "an invalid URL is a permanent, non-retryable failure");
    }

    // ─── (#2093 merge-gate finding 3) drain lock — no duplicate delivery ────

    #[test]
    fn two_sinks_draining_same_outbox_dont_duplicate_and_cursor_never_regresses() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            extras: Default::default(),
        }];
        let key = rule_key(&rules[0].r#match.clone().unwrap_or_default(), &rules[0].http.clone().unwrap());
        let (_outbox_path, cursor_path) = outbox_paths(tmp.path(), &key);

        // Monitor the cursor file concurrently with the run — proves it
        // never regresses, not just that its FINAL value is sane.
        let observed = Arc::new(Mutex::new(Vec::new()));
        let stop_monitor = Arc::new(AtomicBool::new(false));
        let mon_cursor_path = cursor_path.clone();
        let mon_observed = observed.clone();
        let mon_stop = stop_monitor.clone();
        let monitor = std::thread::spawn(move || {
            while !mon_stop.load(Ordering::Acquire) {
                mon_observed.lock().unwrap().push(read_cursor(&mon_cursor_path));
                std::thread::sleep(Duration::from_millis(3));
            }
        });

        let report1: Arc<dyn FlowSink> = Arc::new(NullSink);
        let report2: Arc<dyn FlowSink> = Arc::new(NullSink);
        // Two INDEPENDENT `HookSink`s, each with its own drainer thread,
        // pointed at the SAME outbox dir + same rule — the shape a
        // restarted-while-old-instance-still-shutting-down process, or two
        // cooperating processes, would produce.
        let sink1 = HookSink::new(&rules, tmp.path().to_path_buf(), report1).unwrap();
        let sink2 = HookSink::new(&rules, tmp.path().to_path_buf(), report2).unwrap();

        let n = 21;
        for i in 0..n {
            sink1.write(&record(&format!("work.item.{i}"))).unwrap();
        }

        assert!(wait_until(|| receiver.request_count() >= n, Duration::from_secs(15)));
        // Give any would-be duplicate delivery several more poll cycles to
        // show up before declaring victory.
        std::thread::sleep(POLL_INTERVAL * 10);
        stop_monitor.store(true, Ordering::Release);
        monitor.join().unwrap();
        drop(sink1);
        drop(sink2);

        assert_eq!(receiver.request_count(), n, "no duplicate deliveries from two concurrent drainers on one outbox");
        let bodies = receiver.bodies();
        let actions: std::collections::BTreeSet<String> = bodies
            .iter()
            .map(|b| serde_json::from_str::<serde_json::Value>(b).unwrap()["action"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(actions.len(), n, "{n} distinct actions delivered, none repeated");

        let seq = observed.lock().unwrap();
        for w in seq.windows(2) {
            assert!(w[0] <= w[1], "cursor regressed: {:?} at index {:?}", *seq, seq.iter().position(|x| *x == w[0]));
        }
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
        let key = rule_key(&rules[0].r#match.clone().unwrap_or_default(), &rules[0].http.clone().unwrap());
        let (outbox_path, cursor_path) = outbox_paths(tmp.path(), &key);
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

    // ─── Naming (#2093 merge-gate finding 15) ────────────────────────────

    #[test]
    fn outbox_and_cursor_paths_named_by_content_hash_not_index() {
        let dir = PathBuf::from("/tmp/x");
        let m = HookMatch { action: Some("crawl.*".to_string()), ..Default::default() };
        let key = rule_key(&m, "http://127.0.0.1:8790/events");
        let (outbox, cursor) = outbox_paths(&dir, &key);
        // Readable host prefix + a stable hash suffix, NOT an array index.
        assert!(outbox.to_string_lossy().starts_with("/tmp/x/127.0.0.1-8790-"), "{outbox:?}");
        assert!(outbox.to_string_lossy().ends_with(".outbox.jsonl"), "{outbox:?}");
        assert_eq!(cursor, PathBuf::from(outbox.to_string_lossy().replace(".outbox.jsonl", ".cursor")));

        // Deterministic — the SAME rule content always yields the SAME key.
        assert_eq!(rule_key(&m, "http://127.0.0.1:8790/events"), key);
        // A DIFFERENT match yields a DIFFERENT key, even at the same host.
        let m2 = HookMatch { action: Some("crawl.other".to_string()), ..Default::default() };
        assert_ne!(rule_key(&m2, "http://127.0.0.1:8790/events"), key);
    }

    /// (#2093 merge-gate finding 15) The bug index-based naming actually
    /// had: reordering rules in config (not removing — REORDERING)
    /// silently reassigns one rule's outbox/cursor/counters to whatever
    /// rule now sits at that array index. Content-hash keying is immune
    /// to this because the key is derived from the rule itself, not its
    /// position.
    #[test]
    fn rule_key_is_immune_to_reordering_unlike_the_old_index_scheme() {
        let rule_a = HookMatch { action: Some("crawl.*".to_string()), ..Default::default() };
        let rule_b = HookMatch { action: Some("dispatch.*".to_string()), ..Default::default() };
        let url = "http://127.0.0.1:8790/events";

        // Rule A first, rule B second — then reordered: B first, A second.
        let key_a_before = rule_key(&rule_a, url);
        let key_b_before = rule_key(&rule_b, url);
        let key_b_after = rule_key(&rule_b, url); // same rule, new position
        let key_a_after = rule_key(&rule_a, url);

        // The KEY (unlike the old `{index}-{host}` scheme) doesn't move —
        // it's derived from the rule, not the array position the caller
        // happens to iterate it at.
        assert_eq!(key_a_before, key_a_after, "rule A's key is stable across reordering");
        assert_eq!(key_b_before, key_b_after, "rule B's key is stable across reordering");
        assert_ne!(key_a_before, key_b_before, "distinct rules never collide");
    }

    /// (#2093 merge-gate finding 15) `resolve_rules` — which is what
    /// `HookSink::new` actually calls — derives the SAME key regardless
    /// of a rule's index in the array, end to end.
    #[test]
    fn resolve_rules_paths_are_stable_across_reordering() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rule_a = HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some("http://127.0.0.1:8790/a".to_string()),
            extras: Default::default(),
        };
        let rule_b = HookRule {
            r#match: Some(HookMatch { action: Some("dispatch.*".to_string()), ..Default::default() }),
            http: Some("http://127.0.0.1:8790/b".to_string()),
            extras: Default::default(),
        };
        let a_first = resolve_rules(&[rule_a.clone(), rule_b.clone()], tmp.path()).unwrap();
        let b_first = resolve_rules(&[rule_b, rule_a], tmp.path()).unwrap();

        let a_outbox_when_first = &a_first[0].outbox_path;
        let a_outbox_when_second = &b_first[1].outbox_path;
        assert_eq!(
            a_outbox_when_first, a_outbox_when_second,
            "rule A's outbox path must be the SAME file regardless of which index it's resolved at"
        );
    }

    /// (#2093 Self-QA gate — cost check) `write()` latency with hooks
    /// enabled (3 rules, one matching) vs disabled, 10k records each.
    /// `#[ignore]`d — a throwaway timing measurement, not a correctness
    /// assertion; run explicitly with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn cost_check_write_latency_hooks_enabled_vs_disabled() {
        let n = 10_000;

        // Disabled: a bare NullSink, no hooks in the chain at all.
        let disabled: Arc<dyn FlowSink> = Arc::new(NullSink);
        let start = Instant::now();
        for i in 0..n {
            disabled.write(&record(&format!("work.item.{i}"))).unwrap();
        }
        let disabled_elapsed = start.elapsed();

        // Enabled: 3 rules, one of which matches every record written below.
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![
            HookRule {
                r#match: Some(HookMatch { action: Some("work.*".to_string()), ..Default::default() }),
                http: Some("http://127.0.0.1:1/a".to_string()),
                extras: Default::default(),
            },
            HookRule {
                r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
                http: Some("http://127.0.0.1:1/b".to_string()),
                extras: Default::default(),
            },
            HookRule {
                r#match: Some(HookMatch { mission_id: Some("no-such-mission".to_string()), ..Default::default() }),
                http: Some("http://127.0.0.1:1/c".to_string()),
                extras: Default::default(),
            },
        ];
        let report: Arc<dyn FlowSink> = Arc::new(NullSink);
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();
        let start = Instant::now();
        for i in 0..n {
            sink.write(&record(&format!("work.item.{i}"))).unwrap();
        }
        let enabled_elapsed = start.elapsed();

        println!(
            "cost check: {n} writes — disabled: {disabled_elapsed:?} ({:.2}us/write) — \
             hooks enabled (3 rules, 1 matching): {enabled_elapsed:?} ({:.2}us/write)",
            disabled_elapsed.as_micros() as f64 / n as f64,
            enabled_elapsed.as_micros() as f64 / n as f64,
        );
    }
}
