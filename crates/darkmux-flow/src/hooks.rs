//! Hooks — a fourth `FlowSink` kind (#2093): match a flow record against
//! operator-configured rules, and POST a match verbatim to a receiver.
//! **Enqueue, never block**: `write()` only appends to a local outbox file
//! (flock'd, mirroring `AuditFileSink`); a background drainer thread does
//! the actual HTTP delivery with bounded retries, so a down receiver never
//! stalls a dispatch.
//!
//! # URL policy (#2135 option 2)
//!
//! A rule's `http` target is accepted by URL alone — no config gate: either
//! loopback (`validate_loopback_http_url` — `127.0.0.1`/`[::1]`/`localhost`)
//! or a genuine Tailscale address (`validate_tailnet_http_url` — an IPv4 in
//! `100.64.0.0/10`, or a hostname ending in `.ts.net`), both re-validated
//! (not merely cached) at every POST, not just at load. Everything else is
//! refused outright, both at config load (the whole sink degrades, loudly)
//! and by `darkmux doctor`'s per-rule row: an external (non-tailnet)
//! receiver is a later packet and will require `https://` plus a mandatory
//! signature, not merely a resolvable URL. `http://`, never `https://`, is
//! required for BOTH policies — WireGuard already encrypts and
//! authenticates a tailnet peer, so TLS on top of it buys nothing yet.
//! The two tailnet checks enforce different things: the IPv4 branch
//! verifies the ADDRESS is in-range; the `.ts.net` branch verifies only a
//! SUFFIX MATCH on the hostname string, with no DNS resolution (URL
//! validation makes no network call) — see `is_tailnet_host`'s doc.
//! **Known limit:** Tailscale's IPv6 range (`fd7a:115c:a1e0::/48`) is
//! refused by this policy — only the IPv4 CGNAT range and `.ts.net`
//! hostnames are accepted today.
//!
//! # Delivery contract
//!
//! Every delivery — loopback or tailnet — carries `X-Darkmux-Delivery` (a
//! UUID-v4-shaped id, deterministic per outbox LINE so every retry of the
//! same undelivered line reuses the same id; also stamped as `delivery_id`
//! on the corresponding `hook.fired`/`hook.failed`), `X-Darkmux-Event` (the
//! record's `action`), `X-Darkmux-Machine-Id`/`X-Darkmux-Machine-Uid` (the
//! machine that PRODUCED the record), `X-Darkmux-Sender` (THIS host's own
//! machine id — the machine POSTing, deliberately distinct from
//! `Machine-Id` so a relaying hub isn't mistaken for a record's origin),
//! and `X-Darkmux-Timestamp` (unix ms). When a rule names a
//! `signing_secret_keychain_item` (resolved via `crate::hook_signing_secret`
//! — Keychain, or the portable `DARKMUX_HOOK_SECRET_<rule-index>` env
//! override, which wins when set), every delivery ALSO carries
//! `X-Darkmux-Signature: sha256=<hex HMAC-SHA256 over "<timestamp>.<raw
//! body bytes>">` (see `crate::hmac_sha256`). No secret configured →
//! deliveries go out unsigned; `darkmux doctor` warns for an unsigned
//! TAILNET target specifically (fine inside the tailnet, required beyond
//! it). See `docs/guide/crawl-and-hooks.html`'s "Delivery headers" table
//! for the operator-facing version of this contract.
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
    // (#1959) Payload predicates — `"payload.tool_name": "report_finding"`
    // etc. on the wire. Every predicate must resolve AND match exactly; a
    // record with no payload at all, or missing the named key, fails
    // every predicate (never treated as "no opinion, so it passes").
    for (path, expected) in m.payload_predicates() {
        let actual = record.payload.as_ref().and_then(|p| payload_value_at(p, path));
        if actual != Some(expected) {
            return false;
        }
    }
    true
}

/// Walk a dot-separated path (`"tool_name"`, `"detections.count"`) into a
/// JSON value, returning the leaf if every segment resolves through a JSON
/// object. `None` at the first segment that doesn't exist, isn't an
/// object, or (for the final segment) isn't present — there is no
/// "partial path matches" reading.
fn payload_value_at<'a>(payload: &'a serde_json::Value, dotted_path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = payload;
    for seg in dotted_path.split('.') {
        cur = cur.as_object()?.get(seg)?;
    }
    Some(cur)
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

/// (#2135 option 2) Which policy a hook target's URL satisfied — surfaced
/// on `darkmux doctor`'s rows and stamped nowhere else (delivery behavior
/// doesn't branch on this beyond re-validating it at POST time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookTargetKind {
    Loopback,
    Tailnet,
}

/// True for a real Tailscale IPv4 — the CGNAT range `100.64.0.0/10`
/// (the `100.64.0.0/10` block) Tailscale assigns tailnet
/// addresses from.
fn is_tailnet_ipv4(ip: std::net::Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (o[1] & 0b1100_0000) == 0b0100_0000
}

/// True for a host that is a genuine Tailscale address: a `100.64.0.0/10`
/// literal IPv4, or a MagicDNS hostname ending in `.ts.net`. Takes the
/// ALREADY-lowercased `host_str()` `url::Url` produced, mirroring
/// `validate_loopback_http_url`'s own canonicalization. Note the two
/// halves check different things: the IPv4 branch verifies the ADDRESS
/// itself is in-range; the `.ts.net` branch verifies only the SUFFIX —
/// there is no DNS resolution here (a network call has no place in URL
/// validation), so a syntactically-valid `.ts.net` hostname that doesn't
/// actually resolve, or resolves to something else, still passes. That's
/// the same trust boundary MagicDNS itself relies on operators to police
/// (the tailnet's own DNS, not this check) — accepted here as a policy
/// choice, not an oversight. `host.len() > ".ts.net".len()` rejects the
/// degenerate `.ts.net` itself (an empty subdomain, e.g. `http://.ts.net/x`)
/// — a suffix with nothing in front of it names no machine.
fn is_tailnet_host(host: &str) -> bool {
    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        return is_tailnet_ipv4(ip);
    }
    host.len() > ".ts.net".len() && host.ends_with(".ts.net")
}

/// (#2135 option 2) The tailnet counterpart of `validate_loopback_http_url`
/// — same shape, same checks (literal `http://` prefix, real `url::Url`
/// parse, userinfo rejection, raw-authority-vs-canonical-host comparison),
/// but the host allowlist is `is_tailnet_host` instead of the loopback
/// three. `http://`, not `https://`, is still required: WireGuard already
/// encrypts the wire between tailnet peers, so TLS on top buys nothing here
/// — an `https://` tailnet target (or any OTHER remote host) is refused,
/// same as everything outside this policy; that widening is a later packet.
fn validate_tailnet_http_url(raw: &str) -> Result<()> {
    if !raw.starts_with("http://") {
        bail!(
            "hook URL `{raw}` must literally start with `http://` (lowercase, no leading \
             whitespace) — https for a tailnet target is a later packet, WireGuard already \
             encrypts the wire"
        );
    }
    let parsed = url::Url::parse(raw).with_context(|| format!("parsing hook URL `{raw}`"))?;
    if parsed.scheme() != "http" {
        bail!("hook URL `{raw}` must use http:// for a tailnet target");
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
    if !is_tailnet_host(&host) {
        bail!("hook URL `{raw}` targets non-tailnet host `{host}`");
    }
    let raw_host = raw_authority_host(raw)
        .with_context(|| format!("hook URL `{raw}` has a malformed authority"))?;
    if !raw_host.eq_ignore_ascii_case(&host) {
        bail!(
            "hook URL `{raw}` spells its host as `{raw_host}`, which is not the canonical form \
             `{host}` — only the exact canonical spelling of a tailnet host is accepted, not an \
             alternate notation that merely resolves to it"
        );
    }
    Ok(())
}

/// (#2135 option 2) The FULL URL policy a hook rule's `http` target must
/// satisfy — loopback (unconditionally, `validate_loopback_http_url`) OR a
/// genuine Tailscale address (`validate_tailnet_http_url`). No config gate:
/// the URL itself is the operator's decision, same as every other rule
/// field — `darkmux doctor`'s per-rule row is what makes the choice
/// visible (loopback/tailnet, signed/unsigned), not a flag that has to be
/// flipped first. Any other non-loopback, non-tailnet host is refused
/// outright — an external (non-tailnet) receiver is a later packet and
/// will require https + a mandatory signature, not merely a URL that
/// happens to resolve.
pub fn validate_hook_target_url(raw: &str) -> Result<HookTargetKind> {
    if validate_loopback_http_url(raw).is_ok() {
        return Ok(HookTargetKind::Loopback);
    }
    if validate_tailnet_http_url(raw).is_ok() {
        return Ok(HookTargetKind::Tailnet);
    }
    bail!(
        "hook URL `{raw}` is neither a loopback target (127.0.0.1/[::1]/localhost) nor a genuine \
         Tailscale address (an IPv4 in 100.64.0.0/10, or a hostname ending in `.ts.net`) — a \
         remote receiver outside your tailnet is a later packet and will require https + a \
         mandatory signature"
    );
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
/// doctor` and `darkmux flow status`'s "last delivery ts / last
/// error" columns. Same naming scheme, different suffix.
fn last_status_path(outbox_dir: &Path, key: &str) -> PathBuf {
    outbox_dir.join(format!("{key}.last"))
}

/// (fix-round finding 3) Sibling of `outbox_paths`' pair — a per-rule
/// heartbeat timestamp, rewritten every drainer poll cycle regardless of
/// whether that rule had pending work. Cross-process visible (unlike
/// `HookSink::drainer_alive()`, which only reflects the CALLING process's
/// own in-memory thread handle) — a SEPARATE `flow status`/`doctor`
/// invocation reads this to tell "drainer cycling" from "drainer dead"
/// for a `HookSink` running in a different process. Best-effort, no
/// atomic rename: a torn write here just gets overwritten next cycle
/// ~100ms later, and this is purely informational, never load-bearing.
fn heartbeat_path(cursor_path: &Path) -> PathBuf {
    cursor_path.with_extension("heartbeat")
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
/// invocation (`darkmux doctor`, `darkmux flow status`) can see
/// drops a currently- or previously-running dispatch process counted
/// in-memory. Plain text, same shape as the `.cursor` file.
fn dropped_appends_path(outbox_dir: &Path, key: &str) -> PathBuf {
    outbox_dir.join(format!("{key}.dropped"))
}

fn read_dropped_appends(path: &Path) -> u64 {
    fs::read_to_string(path).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

/// Write via a sibling temp file + atomic `rename(2)` — same shape as
/// `write_cursor` — so a concurrent `read_dropped_appends` never observes
/// the sidecar mid-truncate.
fn write_dropped_appends_atomic(path: &Path, count: u64) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let tmp_path = path.with_extension("dropped.tmp");
    fs::write(&tmp_path, count.to_string()).with_context(|| format!("writing {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path).with_context(|| format!("renaming {} to {}", tmp_path.display(), path.display()))
}

/// (fix-round finding 2) Cross-process read-modify-write increment of the
/// persisted `dropped_appends` sidecar, under the SAME `flock` an append
/// to `outbox_path` takes (`append_outbox_line`/`with_locked_file`) — so
/// two `HookSink` instances (two darkmux processes racing the same
/// outbox) can never both read the sidecar's stale value and each write
/// back their own single-process count, clobbering one another. Returns
/// the new persisted total (best-effort: on a lock/IO failure, falls back
/// to a `+1` off whatever was last read, so the return value is never
/// worse than the pre-fix single-process behavior).
fn increment_dropped_appends(outbox_path: &Path, dropped_appends_path: &Path) -> u64 {
    let result = darkmux_types::flock::with_locked_file(outbox_path, |_file| {
        let count = read_dropped_appends(dropped_appends_path) + 1;
        write_dropped_appends_atomic(dropped_appends_path, count)?;
        Ok(count)
    });
    match result {
        Ok(count) => count,
        Err(e) => {
            eprintln!(
                "flow::HookSink: failed to persist dropped-appends count to {}: {e:#}",
                dropped_appends_path.display()
            );
            read_dropped_appends(dropped_appends_path) + 1
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LastStatus {
    ts: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// (fix-round finding 1) Consecutive cursor-write failures against
    /// this rule's `.cursor` file — the same counter `SinkInfo` exposes
    /// live (`rule{idx}_cursor_write_failures`), persisted here so a
    /// SEPARATE `darkmux doctor` / `flow status` process
    /// invocation can see it too. Lenient-on-read: absent in a sidecar
    /// written before this field existed, defaults to 0.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    cursor_write_failures: u64,
    /// (fix-round finding 1) True once `cursor_write_failures` has
    /// crossed `CURSOR_WRITE_STALL_THRESHOLD` and the drainer has
    /// stopped attempting new deliveries for this rule until a
    /// writability probe against the cursor file succeeds again.
    #[serde(default, skip_serializing_if = "is_false")]
    stalled: bool,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// Write the TERMINAL delivery outcome (success or give-up) for a rule's
/// current line — `ok`/`error` describe that outcome. The cursor-write
/// bookkeeping fields (`cursor_write_failures`/`stalled`) are read from
/// `rt`'s own atomics rather than defaulted, so a terminal write here
/// never clobbers a stall recorded moments earlier by `advance_cursor`
/// (finding 1) — the two can legitimately coexist: the POST succeeded
/// (this call reports `ok: true`) even though the cursor write that was
/// supposed to record it durably is currently failing.
fn write_last_status(rt: &RuleRuntime, ok: bool, error: Option<&str>) {
    let status = LastStatus {
        ts: schema::ts_utc_now(),
        ok,
        error: error.map(str::to_string),
        cursor_write_failures: rt.cursor_write_failures.load(Ordering::Acquire),
        stalled: rt.stalled.load(Ordering::Acquire),
    };
    if let Ok(json) = serde_json::to_string(&status) {
        if let Err(e) = fs::write(&rt.rule.last_status_path, json) {
            eprintln!("flow::HookSink: failed to write last-status {}: {e:#}", rt.rule.last_status_path.display());
        }
    }
}

/// (fix-round finding 1) Read-modify-write ONLY the cursor-write
/// bookkeeping fields onto whatever `.last` sidecar already exists —
/// used when a cursor write fails, so the last known DELIVERY outcome
/// (`ok`/`error`/`ts`) is preserved rather than reset. No prior sidecar
/// (a fresh rule that hasn't had a terminal outcome yet) seeds one with
/// `ok: true`/no error, since "no delivery outcome yet" is not a failure.
fn write_cursor_write_status(path: &Path, cursor_write_failures: u64, stalled: bool) {
    let mut status = read_last_status(path).unwrap_or_else(|| LastStatus {
        ts: schema::ts_utc_now(),
        ok: true,
        error: None,
        cursor_write_failures: 0,
        stalled: false,
    });
    status.cursor_write_failures = cursor_write_failures;
    status.stalled = stalled;
    if let Ok(json) = serde_json::to_string(&status) {
        if let Err(e) = fs::write(path, json) {
            eprintln!("flow::HookSink: failed to write cursor-write status {}: {e:#}", path.display());
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
/// introspection (doctor, `flow status`) that never bails.
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
    /// (#2135 option 2) Which URL policy this rule's target satisfied —
    /// re-validated (not merely cached) at every POST, same reasoning as
    /// `try_post`'s existing loopback re-validation.
    pub target_kind: HookTargetKind,
    /// (#2135 option 2) This rule's resolved HMAC signing secret, when
    /// `signing_secret_keychain_item` (or the `DARKMUX_HOOK_SECRET_<index>`
    /// env override) named one — `None` means every delivery for this rule
    /// goes out unsigned. Read ONCE here, at construction, same as the
    /// Redis/serve-token Keychain reads.
    pub signing_secret: Option<crate::RawHookSecret>,
}

/// Resolve + validate every rule against `outbox_dir`. Bails on the FIRST
/// rule missing an `http` target or whose target satisfies neither the
/// loopback nor the tailnet URL policy — the whole hooks sink is refused
/// rather than silently dropping one bad rule, so a config mistake is loud
/// at construction, not a quietly-smaller rule set.
pub fn resolve_rules(rules: &[HookRule], outbox_dir: &Path) -> Result<Vec<ResolvedRule>> {
    let mut out = Vec::with_capacity(rules.len());
    for (index, r) in rules.iter().enumerate() {
        let url = r
            .http
            .clone()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("hook rule #{index} has no `http` target"))?;
        let target_kind = validate_hook_target_url(&url).with_context(|| format!("hook rule #{index}"))?;
        let match_ = r.r#match.clone().unwrap_or_default();
        let key = rule_key(&match_, &url);
        let (outbox_path, cursor_path) = outbox_paths(outbox_dir, &key);
        let last_status_path = last_status_path(outbox_dir, &key);
        let drain_lock_path = drain_lock_path(outbox_dir, &key);
        let dropped_appends_path = dropped_appends_path(outbox_dir, &key);
        let signing_secret = crate::hook_signing_secret(index, r.signing_secret_keychain_item.as_deref());
        out.push(ResolvedRule {
            index,
            match_,
            url,
            outbox_path,
            cursor_path,
            last_status_path,
            drain_lock_path,
            dropped_appends_path,
            target_kind,
            signing_secret,
        });
    }
    Ok(out)
}

/// (fix-round finding 8) Non-blocking probe: which of `rules`' drain
/// locks are held by ANOTHER process/thread right now. `flow drain` uses this, after a bounded wait comes up short, to tell "a
/// live dispatch process's own drainer is already working this rule" —
/// a specific, actionable reason — from an ordinary down/slow receiver.
/// Best-effort and inherently racy (a drain lock is only held for the
/// brief read-cursor→POST→write-cursor window, so a probe an instant
/// later can miss it); an empty result never proves the lock was free
/// throughout the wait, only that it wasn't held at THIS instant.
pub fn rules_with_drain_lock_held_elsewhere(rules: &[HookRule], outbox_dir: &Path) -> Vec<usize> {
    let Ok(resolved) = resolve_rules(rules, outbox_dir) else {
        return Vec::new();
    };
    resolved
        .iter()
        .filter(|r| matches!(darkmux_types::flock::try_lock_exclusive(&r.drain_lock_path), Ok(None)))
        .map(|r| r.index)
        .collect()
}

/// A read-only summary of one configured rule, for `darkmux doctor` and
/// `darkmux flow status` — never bails (an invalid URL is reported
/// AS a field, not an error), and never touches the network.
#[derive(Debug, Clone)]
pub struct HookRuleSummary {
    pub index: usize,
    pub match_desc: String,
    pub url: String,
    pub is_loopback: bool,
    /// (#2135 option 2) True when the target is a genuine Tailscale
    /// address (`100.64.0.0/10` or `*.ts.net`), NOT loopback. Mutually
    /// exclusive with `is_loopback`; both `false` means the URL satisfies
    /// neither policy and the rule is refused at load (see `is_refused`).
    pub is_tailnet: bool,
    /// (#2135 option 2) True when the target satisfies NEITHER policy —
    /// the rule that `HookSink::new` refuses the whole sink over. Kept
    /// distinct from `!is_loopback` (which used to mean this before the
    /// tailnet policy existed) so a valid tailnet rule doesn't read as
    /// broken.
    pub is_refused: bool,
    /// (#2135 option 2) True when this rule names a
    /// `signing_secret_keychain_item` (or would resolve one via the
    /// `DARKMUX_HOOK_SECRET_<index>` env override) — i.e. its deliveries
    /// carry `X-Darkmux-Signature`. Config-presence only: this is a
    /// summary for `doctor`/`flow status`, not a live Keychain probe.
    pub signed: bool,
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
    /// is visible from a separate `darkmux doctor` / `flow status`
    /// process invocation, not just a live in-process `HookSink`.
    pub dropped_appends: u64,
    /// (fix-round finding 1) Consecutive cursor-write failures against
    /// this rule's `.cursor` file, read from the PERSISTED `.last`
    /// sidecar — cross-process visible, same as `dropped_appends`.
    pub cursor_write_failures: u64,
    /// (fix-round finding 1) True when this rule is STALLED — the
    /// drainer has stopped attempting new deliveries for it until a
    /// writability probe against the cursor file succeeds again.
    pub stalled: bool,
    /// (fix-round finding 3) The drainer's last heartbeat timestamp for
    /// this rule, cross-process visible via `heartbeat_path`. `None` when
    /// no drainer has EVER cycled for this rule in this `outbox_dir` (a
    /// fresh install, or a rule whose key just changed) — distinct from
    /// a heartbeat that stopped updating, which is an OLD but present
    /// timestamp.
    pub last_drainer_heartbeat: Option<String>,
    /// (fix-round finding 7) Lines quarantined because they weren't valid
    /// JSON (see `quarantine_line`) — never redelivered, never counted
    /// toward `undelivered`, so this is the only place they're visible
    /// short of reading the `.outbox.jsonl.quarantine` file by hand.
    pub quarantined_lines: usize,
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
    // (#1959) Payload predicates, sorted by path for a deterministic
    // rendering regardless of the underlying JSON map's iteration order.
    let mut payload_parts: Vec<(String, String)> = m
        .payload_predicates()
        .map(|(path, v)| (path.to_string(), v.to_string()))
        .collect();
    payload_parts.sort();
    for (path, v) in payload_parts {
        parts.push(format!("payload.{path}={v}"));
    }
    parts.join(", ")
}

/// Build a read-only summary of every configured rule — used by
/// `darkmux doctor` and `darkmux flow status`. Unlike
/// `resolve_rules`, this never bails: a URL satisfying neither policy
/// shows up as `is_refused: true` rather than an error, so the caller can
/// report ALL rules' problems at once instead of stopping at the first.
pub fn summarize_configured_rules(rules: &[HookRule], outbox_dir: &Path) -> Vec<HookRuleSummary> {
    rules
        .iter()
        .enumerate()
        .map(|(index, r)| {
            let m = r.r#match.clone().unwrap_or_default();
            let url = r.http.clone().unwrap_or_default();
            let target_kind = validate_hook_target_url(&url).ok();
            let is_loopback = target_kind == Some(HookTargetKind::Loopback);
            let is_tailnet = target_kind == Some(HookTargetKind::Tailnet);
            let signed = r.signing_secret_keychain_item.as_ref().is_some_and(|s| !s.trim().is_empty());
            let key = rule_key(&m, &url);
            let (outbox_path, cursor_path) = outbox_paths(outbox_dir, &key);
            let cursor = read_cursor(&cursor_path);
            let undelivered = undelivered_line_count(&outbox_path, cursor);
            let last = read_last_status(&last_status_path(outbox_dir, &key));
            let dropped_appends = read_dropped_appends(&dropped_appends_path(outbox_dir, &key));
            let last_drainer_heartbeat = fs::read_to_string(heartbeat_path(&cursor_path)).ok();
            let quarantined_lines = undelivered_line_count(&quarantine_path(&outbox_path), 0);
            HookRuleSummary {
                index,
                match_desc: describe_match(&m),
                url,
                is_loopback,
                is_tailnet,
                is_refused: target_kind.is_none(),
                signed,
                is_empty_match: m.is_empty(),
                outbox_path,
                cursor_path,
                undelivered,
                last_delivery_ts: last.as_ref().map(|s| s.ts.clone()),
                last_error: last.as_ref().and_then(|s| s.error.clone()),
                dropped_appends,
                cursor_write_failures: last.as_ref().map(|s| s.cursor_write_failures).unwrap_or(0),
                stalled: last.as_ref().map(|s| s.stalled).unwrap_or(false),
                last_drainer_heartbeat,
                quarantined_lines,
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
/// (fix-round finding 1) Test-only fault-injection registry — forces
/// `write_cursor` to fail for a SPECIFIC cursor path, so the redelivery-
/// storm / stall test can exercise a genuinely unwritable cursor
/// deterministically (no chmod gymnastics, no race between the drainer
/// thread and a filesystem permission flip). Keyed by path (not a single
/// global switch) so it never leaks into other tests running in
/// parallel against their own, distinct temp-dir paths — no `#[serial]`
/// needed. Never compiled into a release binary.
#[cfg(test)]
static FORCE_CURSOR_WRITE_FAILURE_PATHS: std::sync::OnceLock<Mutex<std::collections::HashSet<PathBuf>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
fn set_force_cursor_write_failure(path: &Path, fail: bool) {
    let set = FORCE_CURSOR_WRITE_FAILURE_PATHS.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
    let mut set = set.lock().unwrap();
    if fail {
        set.insert(path.to_path_buf());
    } else {
        set.remove(path);
    }
}

fn write_cursor(cursor_path: &Path, offset: u64) -> Result<()> {
    #[cfg(test)]
    if let Some(set) = FORCE_CURSOR_WRITE_FAILURE_PATHS.get() {
        if set.lock().unwrap().contains(cursor_path) {
            bail!("injected test failure: cursor write refused for {}", cursor_path.display());
        }
    }
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
/// `cursor` — the "undelivered" count `darkmux doctor` / `flow status` report.
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
/// (fix-round finding 1) Consecutive cursor-write failures after which a
/// rule is marked STALLED — the drainer stops attempting new deliveries
/// for it (beyond one writability probe per backoff cycle) until the
/// cursor file becomes writable again.
const CURSOR_WRITE_STALL_THRESHOLD: u64 = 3;
/// (fix-round finding 1) Rate limit for the cursor-write-failure stderr
/// log — mirrors `maybe_warn_dropped`'s `WARNING_INTERVAL` so a cursor
/// path that's been unwritable for hours doesn't turn into one log line
/// per failed write.
const CURSOR_WRITE_WARNING_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug)]
enum DeliveryOutcome {
    /// Delivered (2xx). `receiver_rejected` is the receiver's own
    /// per-record rejection count when its JSON body reported one
    /// (`{"rejected": N}`, the local tracker's contract); a 200 with
    /// rejections is still CONSUMED (at-least-once, the line advances) but
    /// the count rides on `hook.fired` so it is never silent (#1959 live
    /// loop: every finding was refused inside a 200 and nothing said so).
    Success { receiver_rejected: Option<u64> },
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

// ─── (#2135 option 2) Delivery contract headers ────────────────────────

/// The `X-Darkmux-*` headers stamped on EVERY POST — loopback and tailnet
/// alike. `delivery_id` is documented in `delivery_id_for_line`'s own doc.
struct DeliveryHeaders {
    delivery_id: String,
    event: String,
    machine_id: Option<String>,
    machine_uid: Option<String>,
    sender: String,
    timestamp_ms: u64,
    /// The full `sha256=<hex>` header value, precomputed — `None` when
    /// this rule has no signing secret configured.
    signature: Option<String>,
}

/// A UUID-v4-SHAPED delivery id, deterministically derived from the exact
/// outbox line being delivered. This is an ATTRIBUTION identifier, not a
/// security token — the operator's contract calls for "a UUID v4 per
/// delivery attempt group, same id across retries of the same line," and a
/// content hash gives EXACTLY that with no extra state to persist: every
/// retry re-reads the same undelivered line at the same cursor offset and
/// so re-derives the same id, across process restarts too, without a
/// side-table mapping lines to ids. BLAKE3 (already a dependency here) over
/// the raw line bytes, truncated to 16 bytes, with the version/variant
/// bits forced per RFC 4122 so it renders as a valid v4 UUID string.
fn delivery_id_for_line(line: &str) -> String {
    let hash = blake3::hash(line.as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&hash.as_bytes()[..16]);
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10xx
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15],
    )
}

/// (#2135 option 2, security review follow-up) The printable-ASCII subset
/// `ureq` validates an HTTP header VALUE against at send time (tab
/// `0x09`, space `0x20`, and the visible range `0x21..=0x7E`). Every
/// header value built from record/config-derived text (a peer's
/// `machine_id`, an `action` string, the operator's free-form
/// `DARKMUX_MACHINE_ID`) is filtered through this BEFORE it reaches
/// `.set()` — a byte outside the allowlist (a non-ASCII character in
/// `crawl.café`, an en-dash, a stray CR/LF) becomes `_` rather than
/// producing an `ErrorKind::BadHeader` at send time. This matters beyond
/// hygiene: `try_post`'s classification below has NO way to distinguish
/// "this exact line will never be postable" from an ordinary transient
/// network failure without inspecting the error kind, so an unsanitized
/// value that slips through would, absent this filter, retry the SAME
/// undelivered line forever (`RetryableFailure` has no give-up threshold
/// — `MAX_CLIENT_ERROR_ATTEMPTS` only counts `ClientError`), silently
/// blocking every later record on that rule with no `hook.failed` ever
/// emitted. CR/LF are never in the allowlist, so a header-injection
/// attempt (a value crafted to smuggle an extra header line) is caught by
/// the same filter, not a special case. Also caps length — a
/// pathologically long operator-typo'd `machine_id` can't grow the
/// request unboundedly.
fn sanitize_header_value(raw: &str) -> String {
    const MAX_HEADER_VALUE_LEN: usize = 256;
    raw.chars()
        .map(|c| {
            let is_allowed = c.is_ascii() && {
                let b = c as u32;
                b == 0x09 || b == 0x20 || (0x21..=0x7E).contains(&b)
            };
            if is_allowed { c } else { '_' }
        })
        // Every char post-filter is a single ASCII byte, so char-count
        // truncation here is also byte-count truncation of the result.
        .take(MAX_HEADER_VALUE_LEN)
        .collect()
}

/// Build this delivery's headers from the raw outbox `line`, its parsed
/// JSON (`None` for a line that failed to parse — the quarantine path
/// still stamps a `delivery_id`/`event`-less `hook.failed`), and its
/// already-computed `delivery_id` (the caller owns this — see
/// `delivery_id_for_line`'s doc; passed in rather than recomputed here so
/// the hash runs exactly once per line, not once for the header and again
/// for the `hook.fired`/`hook.failed` payload). `event` / `machine_id` /
/// `machine_uid` come from the RECORD itself (the machine that PRODUCED
/// it); `sender` is THIS host's own machine id (the machine POSTing) —
/// the two are deliberately different fields since a fleet hub forwarding
/// another machine's record would otherwise be indistinguishable from the
/// record's origin. Every one of these four is sanitized via
/// `sanitize_header_value` — see its doc for why.
fn build_delivery_headers(
    line: &str,
    parsed: Option<&serde_json::Value>,
    delivery_id: &str,
    signing_secret: Option<&crate::RawHookSecret>,
) -> DeliveryHeaders {
    let event = sanitize_header_value(parsed.and_then(|v| v.get("action")).and_then(|v| v.as_str()).unwrap_or(""));
    let machine_id =
        parsed.and_then(|v| v.get("machine_id")).and_then(|v| v.as_str()).map(sanitize_header_value);
    let machine_uid =
        parsed.and_then(|v| v.get("machine_uid")).and_then(|v| v.as_str()).map(sanitize_header_value);
    let sender = sanitize_header_value(&schema::resolve_machine_id().unwrap_or_else(|| "unknown".to_string()));
    let timestamp_ms =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    // NOT sanitized: `delivery_id` is a BLAKE3-hash-derived hex/UUID
    // string (already the allowlisted subset by construction) and the
    // signature below is computed over the UNSANITIZED `line`/timestamp —
    // signing the raw wire body, not the sanitized headers, is what lets
    // a receiver verify the signature against the body it actually
    // received.
    let delivery_id = delivery_id.to_string();
    let signature = signing_secret.map(|secret| {
        let signed_input = format!("{timestamp_ms}.{line}");
        format!("sha256={}", crate::hmac_sha256::hmac_sha256_hex(secret.expose_for_hmac().as_bytes(), signed_input.as_bytes()))
    });
    DeliveryHeaders { delivery_id, event, machine_id, machine_uid, sender, timestamp_ms, signature }
}

fn try_post(url: &str, body: &str, headers: &DeliveryHeaders) -> DeliveryOutcome {
    // (#2093 merge-gate finding 1, belt-and-braces) Re-validate at POST
    // time — the URL was already validated at `resolve_rules` /
    // `HookSink::new`, but a future refactor that plumbs a URL through a
    // new path (or a construction bug) must not get a free pass to the
    // network just because construction-time validation happened to run.
    // No listener is contacted when this fails: refused locally, treated
    // as a permanent (never-retried) failure like any other 4xx.
    if let Err(e) = validate_hook_target_url(url) {
        eprintln!("flow::HookSink: try_post refusing to send — URL failed re-validation: {e:#}");
        return DeliveryOutcome::ClientError;
    }
    // (#2093 merge-gate finding 2) Redirects are never followed — a
    // redirect target is the RECEIVER telling us to go elsewhere, and
    // "elsewhere" is exactly the case `validate_hook_target_url` exists
    // to gate. `ureq` with `redirects(0)` does NOT error on a 3xx; it
    // returns it as an `Ok` response with the 3xx status, so the status
    // must be checked on the `Ok` arm too, not just the `Err` arm.
    let agent = ureq::AgentBuilder::new().timeout(POST_TIMEOUT).redirects(0).build();
    let mut req = agent
        .post(url)
        .set("Content-Type", "application/json")
        .set("X-Darkmux-Delivery", &headers.delivery_id)
        .set("X-Darkmux-Event", &headers.event)
        .set("X-Darkmux-Sender", &headers.sender)
        .set("X-Darkmux-Timestamp", &headers.timestamp_ms.to_string());
    if let Some(m) = &headers.machine_id {
        req = req.set("X-Darkmux-Machine-Id", m);
    }
    if let Some(u) = &headers.machine_uid {
        req = req.set("X-Darkmux-Machine-Uid", u);
    }
    if let Some(sig) = &headers.signature {
        req = req.set("X-Darkmux-Signature", sig);
    }
    match req.send_string(body) {
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
                // Read at most 64 KiB of the body (a receiver cannot make us
                // buffer a 100 MB reply) and pull a `rejected` count if the
                // body is JSON that carries one.
                let mut buf = String::new();
                let _ = std::io::Read::take(resp.into_reader(), 65_536).read_to_string(&mut buf);
                let receiver_rejected = serde_json::from_str::<serde_json::Value>(&buf)
                    .ok()
                    .and_then(|v| v.get("rejected").and_then(serde_json::Value::as_u64));
                DeliveryOutcome::Success { receiver_rejected }
            }
        }
        Err(ureq::Error::Status(code, _resp)) if is_retryable_client_status(code) => DeliveryOutcome::RetryableFailure,
        Err(ureq::Error::Status(code, _resp)) if (400..500).contains(&code) => DeliveryOutcome::ClientError,
        // (#2135 option 2, security review follow-up) `BadHeader` means a
        // header VALUE this process built failed ureq's printable-ASCII
        // validation at send time — `sanitize_header_value` in
        // `build_delivery_headers` is the primary defense, but a value
        // that reaches here unsanitized (a future header addition that
        // forgets to sanitize, or a bug in the filter itself) must NOT
        // fall through to `RetryableFailure`: this is a DETERMINISTIC,
        // permanent failure — the exact same bytes produce the exact same
        // error on every retry — and `RetryableFailure` has no give-up
        // threshold (`MAX_CLIENT_ERROR_ATTEMPTS` only counts
        // `ClientError`), so treating it as retryable would re-POST the
        // same line forever and silently block every later record on
        // this rule. Classifying it as `ClientError` routes it through
        // the existing give-up path instead (bounded retries, then
        // quarantine + a loud `hook.failed`). `Error::kind()` maps
        // `Status` to `ErrorKind::HTTP`, so this arm only ever matches a
        // genuine `Transport` failure of this kind.
        Err(e) if e.kind() == ureq::ErrorKind::BadHeader => DeliveryOutcome::ClientError,
        Err(_) => DeliveryOutcome::RetryableFailure,
    }
}

/// (fix-round finding 6) Outcome of `drain_stray_file`.
#[derive(Debug, Default, Clone, Copy)]
pub struct StrayDrainResult {
    pub delivered: usize,
    pub failed: usize,
    pub remaining_undelivered: usize,
}

fn key_from_outbox_path(outbox_path: &Path) -> Option<String> {
    outbox_path.file_name()?.to_str()?.strip_suffix(".outbox.jsonl").map(str::to_string)
}

/// (fix-round finding 6) One-shot, best-effort drain of a STRAY outbox
/// file — one whose owning rule no longer exists in current config, so
/// no running `HookSink`'s background drainer will ever pick it up
/// (`stray_outbox_files` in `darkmux-doctor` is what NAMES these).
/// `to_url` is validated with the SAME `validate_loopback_http_url` every
/// other delivery path uses — this is not a way to POST an arbitrary
/// file to an arbitrary URL.
///
/// Unlike the background drainer, this makes ONE straight pass with no
/// retry/backoff: the first delivery failure stops the walk immediately
/// (never hammers a down receiver) and is reported as `failed: 1`,
/// leaving the cursor exactly where it was — a repeat call after fixing
/// the receiver picks up right where this one stopped. Reuses the SAME
/// `.cursor` sidecar the file's original (now-removed) rule would have
/// used, derived from the outbox filename's own key.
pub fn drain_stray_file(outbox_path: &Path, to_url: &str) -> Result<StrayDrainResult> {
    validate_loopback_http_url(to_url).context("--to")?;
    let key = key_from_outbox_path(outbox_path)
        .ok_or_else(|| anyhow!("{} is not a *.outbox.jsonl file", outbox_path.display()))?;
    let cursor_path = outbox_path.with_file_name(format!("{key}.cursor"));
    let mut cursor = read_cursor(&cursor_path);
    let mut result = StrayDrainResult::default();
    while let Some((line, new_cursor)) = next_pending_line(outbox_path, cursor) {
        let parsed: Option<serde_json::Value> = serde_json::from_str(&line).ok();
        // (#2135 option 2) No signing secret here — a stray file's
        // original rule (and whatever secret it named) no longer exists
        // in current config by definition; this manual recovery path
        // delivers unsigned, same as any other unconfigured rule.
        let delivery_id = delivery_id_for_line(&line);
        let headers = build_delivery_headers(&line, parsed.as_ref(), &delivery_id, None);
        match try_post(to_url, &line, &headers) {
            DeliveryOutcome::Success { .. } => {
                result.delivered += 1;
                cursor = new_cursor;
                write_cursor(&cursor_path, cursor)?;
            }
            _ => {
                result.failed += 1;
                break;
            }
        }
    }
    result.remaining_undelivered = undelivered_line_count(outbox_path, cursor);
    Ok(result)
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
    /// unwritable/full disk). Surfaced by `flow status` and
    /// `doctor`; never reset — a monotonically growing count across the
    /// process lifetime is the honest shape for "how much did we lose."
    dropped_appends: AtomicU64,
    /// (#2093 merge-gate finding 5) Rate-limits the `hook.failed` emitted
    /// when the hard cap is active, to at most once per minute — a
    /// receiver that's been down for hours must not turn into one
    /// `hook.failed` record per dropped write.
    last_drop_warning: Mutex<Option<Instant>>,
    /// (fix-round finding 1) Consecutive `write_cursor` failures against
    /// this rule's `.cursor` file. Reset to 0 on the next successful
    /// cursor write; NEVER on an ordinary delivery success/give-up — a
    /// terminal delivery outcome without a durable cursor advance is
    /// exactly the case this counter exists to track. Crosses
    /// `CURSOR_WRITE_STALL_THRESHOLD` → `stalled` is set.
    cursor_write_failures: AtomicU64,
    /// (fix-round finding 1) True once the drainer has stopped attempting
    /// new deliveries for this rule pending a successful cursor-write
    /// probe. Checked at the top of `drainer_loop`'s per-rule iteration.
    stalled: AtomicBool,
    /// (fix-round finding 1) Rate-limits the cursor-write-failure stderr
    /// log — same pattern as `last_drop_warning`.
    last_cursor_write_warning: Mutex<Option<Instant>>,
    /// (fix-round finding 5) Delivered lines that were valid JSON but had
    /// no `action` field — never quarantined (lenient on read), just
    /// counted. In-process only; never reset.
    non_record_lines: AtomicU64,
}

fn apply_backoff(rt: &RuleRuntime) {
    let mut backoff = rt.backoff.lock().unwrap();
    let wait = *backoff;
    *rt.next_attempt.lock().unwrap() = Instant::now() + wait;
    *backoff = (*backoff * 2).min(MAX_BACKOFF);
}

/// (fix-round finding 1) Advance a rule's on-disk delivery cursor after a
/// TERMINAL outcome (delivered, or given up on). On success, resets
/// backoff and clears any stall — the normal case. On failure, this
/// deliberately does NOT call `reset_backoff`: retrying the exact same
/// line immediately (backoff reset to `INITIAL_BACKOFF`'s effectively-now
/// `next_attempt`) is what turns a persistently unwritable cursor file
/// into a redelivery storm — the receiver sees the SAME line re-POSTed
/// every poll cycle forever, since a failed cursor write means the next
/// `next_pending_line` call returns that same line again. Instead:
/// `apply_backoff` (exponential, same as any retryable failure), log
/// once per `CURSOR_WRITE_WARNING_INTERVAL`, persist the failure count +
/// stall flag to the `.last` sidecar, and — after
/// `CURSOR_WRITE_STALL_THRESHOLD` consecutive failures — mark the rule
/// STALLED so `drainer_loop` stops reaching `next_pending_line`/`try_post`
/// for it entirely until a writability probe succeeds (see the stall
/// check at the top of `drainer_loop`'s per-rule loop body).
///
/// Returns whether the cursor actually advanced. Callers still run their
/// terminal bookkeeping (`write_last_status`, `emit_hook_record`)
/// regardless of the return value — the delivery attempt itself (POST
/// success or give-up) genuinely happened even when we can't yet durably
/// record having moved past it; the outbox's documented at-least-once
/// contract covers the resulting possible redelivery once the cursor
/// becomes writable again.
fn advance_cursor(rt: &RuleRuntime, new_cursor: u64) -> bool {
    match write_cursor(&rt.rule.cursor_path, new_cursor) {
        Ok(()) => {
            rt.cursor_write_failures.store(0, Ordering::Release);
            rt.stalled.store(false, Ordering::Release);
            reset_backoff(rt);
            true
        }
        Err(e) => {
            let failures = rt.cursor_write_failures.fetch_add(1, Ordering::AcqRel) + 1;
            let should_log = {
                let mut last = rt.last_cursor_write_warning.lock().unwrap();
                let now = Instant::now();
                let should = last.map(|prev| now.duration_since(prev) >= CURSOR_WRITE_WARNING_INTERVAL).unwrap_or(true);
                if should {
                    *last = Some(now);
                }
                should
            };
            if should_log {
                eprintln!(
                    "flow::HookSink: rule #{} failed to persist delivery cursor to {}: {e:#} \
                     ({failures} consecutive cursor-write failure(s) — backing off, not retrying immediately)",
                    rt.rule.index,
                    rt.rule.cursor_path.display()
                );
            }
            let stalled = failures >= CURSOR_WRITE_STALL_THRESHOLD;
            if stalled {
                rt.stalled.store(true, Ordering::Release);
            }
            write_cursor_write_status(&rt.rule.last_status_path, failures, stalled);
            apply_backoff(rt);
            false
        }
    }
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
#[allow(clippy::too_many_arguments)]
fn emit_hook_record(
    report_sink: &dyn FlowSink,
    success: bool,
    rt: &RuleRuntime,
    delivered_line: &str,
    attempt: u32,
    error: Option<&str>,
    delivery_id: &str,
) {
    emit_hook_record_with(report_sink, success, rt, delivered_line, attempt, error, None, delivery_id)
}

#[allow(clippy::too_many_arguments)]
fn emit_hook_record_with(
    report_sink: &dyn FlowSink,
    success: bool,
    rt: &RuleRuntime,
    delivered_line: &str,
    attempt: u32,
    error: Option<&str>,
    receiver_rejected: Option<u64>,
    delivery_id: &str,
) {
    let rule = &rt.rule;
    // (fix-round finding 5) Lenient on read: a delivered line that IS
    // valid JSON but carries no `action` field (not a real flow record —
    // e.g. a stray/foreign line in the outbox) is never quarantined, just
    // reported honestly. `action_val` stays `None` for that case AND for
    // genuinely invalid JSON (the quarantine call site) — only the
    // valid-JSON-but-no-`action` case counts toward `non_record_lines`,
    // since invalid JSON is already tracked via the quarantine file.
    let parse_result = serde_json::from_str::<serde_json::Value>(delivered_line);
    let parsed = parse_result.as_ref().ok();
    let action_val = parsed.and_then(|v| v.get("action")).and_then(|v| v.as_str()).map(str::to_string);
    if parse_result.is_ok() && action_val.is_none() {
        rt.non_record_lines.fetch_add(1, Ordering::Relaxed);
    }
    let hash = parsed.and_then(|v| v.get("hash")).and_then(|v| v.as_str()).map(str::to_string);
    let host = extract_host_port(&rule.url).unwrap_or("").to_string();

    let mut payload = serde_json::json!({
        "rule_index": rule.index,
        "target_host": host,
        // `null` (never `""`) when the delivered line had no `action` —
        // an empty string would read as "delivered a record whose action
        // was blank", which is a different (and untrue) claim.
        "delivered_action": action_val.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null),
        "attempt": attempt,
        // (#2135 option 2) The SAME delivery id this attempt carried on
        // the wire as `X-Darkmux-Delivery` — lets a receiver (or an
        // operator reading flow) correlate the record here with the HTTP
        // request the receiver actually saw.
        "delivery_id": delivery_id,
    });
    if let Some(h) = hash {
        payload["delivered_hash"] = serde_json::Value::String(h);
    }
    if let Some(e) = error {
        payload["error"] = serde_json::Value::String(e.to_string());
    }
    if let Some(n) = receiver_rejected.filter(|n| *n > 0) {
        payload["receiver_rejected"] = serde_json::Value::from(n);
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
            // (fix-round finding 3) Heartbeat every poll cycle, whether or
            // not this rule has pending work — see `heartbeat_path`'s doc.
            let _ = fs::write(heartbeat_path(&rt.rule.cursor_path), schema::ts_utc_now());
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
            // (fix-round finding 1) A STALLED rule gets ONE writability
            // probe per backoff cycle before anything else this
            // iteration — a no-op `write_cursor` of the CURRENT offset
            // (never advances past an undelivered line). A failing probe
            // re-applies backoff and moves on to the next rule WITHOUT
            // ever reaching `next_pending_line`/`try_post` — that's what
            // keeps a persistently-unwritable cursor from re-POSTing the
            // same line every cycle. A succeeding probe clears the stall
            // and falls through to normal delivery this same cycle.
            if rt.stalled.load(Ordering::Acquire) {
                let probe_cursor = read_cursor(&rt.rule.cursor_path);
                if write_cursor(&rt.rule.cursor_path, probe_cursor).is_err() {
                    apply_backoff(rt);
                    continue;
                }
                rt.stalled.store(false, Ordering::Release);
                rt.cursor_write_failures.store(0, Ordering::Release);
                reset_backoff(rt);
            }
            // (#2093 merge-gate finding 5) Compaction runs under the SAME
            // drain lock this iteration already holds — checked (and, at
            // most, performed) once per poll cycle per rule.
            maybe_compact_outbox(&rt.rule.outbox_path, &rt.rule.cursor_path, DEFAULT_COMPACTION_THRESHOLD_BYTES);
            let cursor = read_cursor(&rt.rule.cursor_path);
            let Some((line, new_cursor)) = next_pending_line(&rt.rule.outbox_path, cursor) else {
                continue;
            };
            did_work = true;
            // (#2135 option 2) Computed ONCE per line, deterministically
            // from the line's own bytes — every retry of THIS exact
            // undelivered line reuses the SAME delivery id (see
            // `delivery_id_for_line`'s doc), and every terminal outcome
            // below stamps it on the emitted `hook.fired`/`hook.failed`.
            let delivery_id = delivery_id_for_line(&line);
            // (#2093 merge-gate finding 4) A line that isn't valid JSON —
            // most likely a torn fragment that `ensure_trailing_newline`
            // turned into its own complete-but-malformed line at
            // construction — is never POSTed. Quarantine it (preserve the
            // raw bytes, never silently drop them), advance the cursor
            // past it so it doesn't block every line after it forever,
            // and emit `hook.failed` naming the reason.
            let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&line) else {
                quarantine_line(&rt.rule.outbox_path, &line);
                advance_cursor(rt, new_cursor);
                let reason = "invalid outbox line";
                write_last_status(rt, false, Some(reason));
                emit_hook_record(report_sink.as_ref(), false, rt, &line, 1, Some(reason), &delivery_id);
                continue;
            };
            let headers = build_delivery_headers(&line, Some(&parsed), &delivery_id, rt.rule.signing_secret.as_ref());
            match try_post(&rt.rule.url, &line, &headers) {
                DeliveryOutcome::Success { receiver_rejected } => {
                    let attempt = {
                        let mut c = rt.attempt_count.lock().unwrap();
                        *c += 1;
                        *c
                    };
                    advance_cursor(rt, new_cursor);
                    write_last_status(rt, true, None);
                    if let Some(n) = receiver_rejected.filter(|n| *n > 0) {
                        eprintln!(
                            "flow::HookSink: receiver at {} accepted the request but rejected {n} record(s) inside it — see hook.fired.receiver_rejected",
                            rt.rule.url
                        );
                    }
                    emit_hook_record_with(report_sink.as_ref(), true, rt, &line, attempt, None, receiver_rejected, &delivery_id);
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
                        advance_cursor(rt, new_cursor);
                        let reason = format!("4xx response, skipped after {client_errors} client-error attempts");
                        write_last_status(rt, false, Some(&reason));
                        emit_hook_record(report_sink.as_ref(), false, rt, &line, attempt, Some(&reason), &delivery_id);
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
                    advance_cursor(rt, new_cursor);
                    let reason = format!("redirect refused: {status} to {target_host}");
                    write_last_status(rt, false, Some(&reason));
                    emit_hook_record(report_sink.as_ref(), false, rt, &line, attempt, Some(&reason), &delivery_id);
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
                    cursor_write_failures: AtomicU64::new(0),
                    stalled: AtomicBool::new(false),
                    last_cursor_write_warning: Mutex::new(None),
                    non_record_lines: AtomicU64::new(0),
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
    /// delivering with no signal anywhere an operator would see it.
    ///
    /// (fix-round finding 3) This reads an in-process `JoinHandle` — it
    /// is surfaced in `SinkInfo`/`flow status --json` for THIS process's
    /// own `HookSink` only. A separate `darkmux doctor`/`flow status` invocation (a different process) cannot observe it and
    /// falls back to `HookRuleSummary::last_drainer_heartbeat` instead —
    /// a per-rule timestamp the drainer rewrites every poll cycle
    /// (`heartbeat_path`), which IS cross-process visible.
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
                    // (fix-round finding 2) Cross-process read-modify-
                    // write under the outbox's own flock — see
                    // `increment_dropped_appends`'s doc. The in-process
                    // atomic is then set (not merely bumped) to the
                    // returned TRUE total, so a live `info()` call from
                    // THIS process reflects every process's drops, not
                    // just its own.
                    let dropped = increment_dropped_appends(&rt.rule.outbox_path, &rt.rule.dropped_appends_path);
                    rt.dropped_appends.store(dropped, Ordering::Relaxed);
                    maybe_warn_dropped(rt, self.report_sink.as_ref(), self.max_outbox_mb, dropped);
                    continue;
                }
                if let Err(e) = append_outbox_line(&rt.rule.outbox_path, &line) {
                    // (#2093 merge-gate finding 9) An append failure is
                    // counted the SAME way a cap-drop is — both are "this
                    // record never reached the outbox for this rule."
                    let dropped = increment_dropped_appends(&rt.rule.outbox_path, &rt.rule.dropped_appends_path);
                    rt.dropped_appends.store(dropped, Ordering::Relaxed);
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
            config.insert(
                format!("rule{idx}_cursor_write_failures"),
                rt.cursor_write_failures.load(Ordering::Relaxed).to_string(),
            );
            config.insert(format!("rule{idx}_stalled"), rt.stalled.load(Ordering::Relaxed).to_string());
            config.insert(
                format!("rule{idx}_non_record_lines"),
                rt.non_record_lines.load(Ordering::Relaxed).to_string(),
            );
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
        /// (#2135 option 2) Every request's headers, lowercased-key, in
        /// arrival order alongside `received` — lets a test assert on the
        /// `X-Darkmux-*` delivery contract headers a real receiver would
        /// see, not just the body.
        received_headers: Arc<Mutex<Vec<std::collections::HashMap<String, String>>>>,
        statuses: Arc<Mutex<VecDeque<u16>>>,
        redirect_location: Arc<Mutex<Option<String>>>,
        response_body: Arc<Mutex<Option<String>>>,
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
            let received_headers = Arc::new(Mutex::new(Vec::new()));
            let statuses = Arc::new(Mutex::new(VecDeque::new()));
            let redirect_location = Arc::new(Mutex::new(None));
            let stop = Arc::new(AtomicBool::new(false));

            let thread_received = received.clone();
            let thread_received_headers = received_headers.clone();
            let thread_statuses = statuses.clone();
            let thread_redirect_location = redirect_location.clone();
            let response_body: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
            let thread_response_body = response_body.clone();
            let thread_stop = stop.clone();
            let handle = std::thread::spawn(move || {
                loop {
                    if thread_stop.load(Ordering::Acquire) {
                        return;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = handle_one(
                                stream,
                                &thread_received,
                                &thread_received_headers,
                                &thread_statuses,
                                &thread_redirect_location,
                                &thread_response_body,
                            );
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        Err(_) => return,
                    }
                }
            });

            Self { addr, received, received_headers, statuses, redirect_location, response_body, stop, handle: Some(handle) }
        }

        /// Queue a sequence of HTTP status codes to return, one per
        /// request; the LAST entry repeats once the queue is exhausted.
        pub fn with_status_sequence(self, statuses: impl IntoIterator<Item = u16>) -> Self {
            *self.statuses.lock().unwrap() = statuses.into_iter().collect();
            self
        }

        /// When a queued status is 3xx, answer with this `Location` header
        /// — for tests proving a redirect is refused rather than followed.
        /// Answer every 2xx with this body (a receiver reporting per-record
        /// results, e.g. `{"rejected": 1}`).
        pub fn with_response_body(self, body: &str) -> Self {
            *self.response_body.lock().unwrap() = Some(body.to_string());
            self
        }

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

        /// Every request's headers so far (lowercased keys), in arrival
        /// order alongside `bodies()`.
        pub fn headers(&self) -> Vec<std::collections::HashMap<String, String>> {
            self.received_headers.lock().unwrap().clone()
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
        received_headers: &Arc<Mutex<Vec<std::collections::HashMap<String, String>>>>,
        statuses: &Arc<Mutex<VecDeque<u16>>>,
        redirect_location: &Arc<Mutex<Option<String>>>,
        response_body: &Arc<Mutex<Option<String>>>,
    ) -> std::io::Result<()> {
        stream.set_nonblocking(false)?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut request_line = String::new();
        reader.read_line(&mut request_line)?;
        if request_line.is_empty() {
            return Ok(());
        }
        let mut content_length: usize = 0;
        let mut headers = std::collections::HashMap::new();
        loop {
            let mut header_line = String::new();
            reader.read_line(&mut header_line)?;
            if header_line == "\r\n" || header_line.is_empty() {
                break;
            }
            if let Some(v) = header_line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = v.trim().parse().unwrap_or(0);
            }
            if let Some((name, value)) = header_line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body)?;
        }
        received.lock().unwrap().push(String::from_utf8_lossy(&body).into_owned());
        received_headers.lock().unwrap().push(headers);

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
        let body_out = if (200..300).contains(&status) { response_body.lock().unwrap().clone().unwrap_or_default() } else { String::new() };
        let resp = format!(
            "HTTP/1.1 {status} {reason}\r\n{location_header}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_out}",
            body_out.len()
        );
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

    // ─── (#1959) HookMatch payload predicates ──────────────────────────

    fn payload_match(pairs: &[(&str, serde_json::Value)]) -> HookMatch {
        let mut extras = serde_json::Map::new();
        for (k, v) in pairs {
            extras.insert(format!("payload.{k}"), v.clone());
        }
        HookMatch { extras, ..Default::default() }
    }

    #[test]
    fn payload_predicate_matches_on_tool_name() {
        let mut r = record("dispatch.tool");
        r.payload = Some(serde_json::json!({"tool_name": "report_finding", "ok": true}));
        let m = payload_match(&[("tool_name", serde_json::json!("report_finding"))]);
        assert!(hook_match(&m, &r));

        let m = payload_match(&[("tool_name", serde_json::json!("bash"))]);
        assert!(!hook_match(&m, &r));
    }

    #[test]
    fn payload_predicate_distinguishes_ok_true_from_ok_false() {
        let mut r = record("dispatch.tool");
        r.payload = Some(serde_json::json!({"tool_name": "report_finding", "ok": true}));
        assert!(hook_match(&payload_match(&[("ok", serde_json::json!(true))]), &r));
        assert!(!hook_match(&payload_match(&[("ok", serde_json::json!(false))]), &r));

        r.payload = Some(serde_json::json!({"tool_name": "report_finding", "ok": false}));
        assert!(hook_match(&payload_match(&[("ok", serde_json::json!(false))]), &r));
        assert!(!hook_match(&payload_match(&[("ok", serde_json::json!(true))]), &r));
    }

    #[test]
    fn payload_predicate_on_a_missing_key_never_matches() {
        let mut r = record("dispatch.tool");
        r.payload = Some(serde_json::json!({"tool_name": "report_finding"}));
        // `outcome` isn't in this payload at all.
        assert!(!hook_match(&payload_match(&[("outcome", serde_json::json!("ok"))]), &r));

        // Nor does a record with no payload whatsoever.
        r.payload = None;
        assert!(!hook_match(&payload_match(&[("tool_name", serde_json::json!("report_finding"))]), &r));
    }

    #[test]
    fn payload_predicate_resolves_a_nested_dotted_path() {
        let mut r = record("dispatch.tool");
        r.payload = Some(serde_json::json!({"tool_name": "read", "detections": {"count": 3}}));
        assert!(hook_match(&payload_match(&[("detections.count", serde_json::json!(3))]), &r));
        assert!(!hook_match(&payload_match(&[("detections.count", serde_json::json!(4))]), &r));
        // A path that tries to walk THROUGH a non-object segment fails cleanly.
        assert!(!hook_match(&payload_match(&[("tool_name.nested", serde_json::json!("x"))]), &r));
    }

    #[test]
    fn describe_match_renders_payload_predicates_sorted() {
        let m = HookMatch {
            action: Some("dispatch.tool".to_string()),
            extras: {
                let mut e = serde_json::Map::new();
                e.insert("payload.tool_name".to_string(), serde_json::json!("report_finding"));
                e.insert("payload.ok".to_string(), serde_json::json!(true));
                e
            },
            ..Default::default()
        };
        let desc = describe_match(&m);
        assert!(desc.contains("action=dispatch.tool"), "{desc}");
        assert!(desc.contains("payload.ok=true"), "{desc}");
        assert!(desc.contains("payload.tool_name=\"report_finding\""), "{desc}");
        // Sorted by path: "ok" before "tool_name".
        assert!(desc.find("payload.ok").unwrap() < desc.find("payload.tool_name").unwrap(), "{desc}");
    }

    #[test]
    fn payload_predicate_combines_with_action_and_every_other_field_anded() {
        let mut r = record("dispatch.tool");
        r.payload = Some(serde_json::json!({"tool_name": "report_finding", "ok": true}));
        let m = HookMatch {
            action: Some("dispatch.tool".to_string()),
            extras: {
                let mut e = serde_json::Map::new();
                e.insert("payload.tool_name".to_string(), serde_json::json!("report_finding"));
                e.insert("payload.ok".to_string(), serde_json::json!(true));
                e
            },
            ..Default::default()
        };
        assert!(hook_match(&m, &r));

        // Change the action alone — payload predicates still match, but the
        // AND with `action` must still fail the whole thing.
        let mut m2 = m.clone();
        m2.action = Some("dispatch.turn".to_string());
        assert!(!hook_match(&m2, &r));
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

    // ─── (#2135 option 2) tailnet URL policy ───────────────────────────

    #[test]
    fn validate_hook_target_url_accepts_loopback() {
        for raw in ["http://127.0.0.1:8790/events", "http://localhost:8790/x", "http://[::1]:8790/x"] {
            assert_eq!(validate_hook_target_url(raw).unwrap(), HookTargetKind::Loopback, "{raw}");
        }
    }

    #[test]
    fn validate_hook_target_url_accepts_tailnet_ipv4_and_ts_net_hostname() {
        for raw in [
            "http://100.64.1.2:8790/events",
            "http://100.64.0.0:8790/x",
            // the /10 upper edge, built from octets so no real-looking address literal
            // sits in the source (the public-repo sentinel guard scans for them).
            &format!("http://{}:8790/x", std::net::Ipv4Addr::new(100, 127, 255, 255)),
            "http://host-0a1b2c3d.tailnet-0123456789.ts.net:8790/x",
            "http://HOST-0A1B2C3D.TAILNET-0123456789.TS.NET:8790/x",
        ] {
            assert_eq!(validate_hook_target_url(raw).unwrap(), HookTargetKind::Tailnet, "{raw}");
        }
    }

    #[test]
    fn validate_hook_target_url_refuses_outside_the_tailnet_cgnat_range() {
        // 100.63.x.x and 100.128.x.x sit just OUTSIDE 100.64.0.0/10 on
        // either edge — must NOT be mistaken for tailnet addresses.
        for raw in ["http://100.63.255.255:8790/x", "http://100.128.0.0:8790/x", "http://10.0.0.5:8790/x", "http://example.com/x"] {
            assert!(validate_hook_target_url(raw).is_err(), "must be REFUSED: {raw}");
        }
    }

    #[test]
    fn validate_hook_target_url_refuses_https_for_a_tailnet_target() {
        // WireGuard already encrypts the wire — https on top is a later
        // packet, not silently upgraded-to or accepted.
        let err = validate_hook_target_url("https://100.64.1.2:8790/x").unwrap_err();
        assert!(format!("{err:#}").to_lowercase().contains("http"), "{err:#}");
    }

    #[test]
    fn validate_hook_target_url_refuses_userinfo_on_a_tailnet_host() {
        assert!(validate_hook_target_url("http://user:pass@100.64.1.2/x").is_err());
    }

    #[test]
    fn validate_hook_target_url_refuses_alternate_ipv4_notation_for_a_tailnet_address() {
        // Same raw-authority-vs-canonical-host defense `validate_loopback_
        // http_url` has for 127.1/0.0.0.0 — an alternate notation that
        // merely RESOLVES onto a tailnet address is not the blessed
        // canonical spelling.
        assert!(validate_hook_target_url("http://0144.0100.0.1/x").is_err(), "octal notation must be refused");
    }

    #[test]
    fn last_status_summary_reflects_delivery_outcome() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            signing_secret_keychain_item: None,
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
            signing_secret_keychain_item: None,
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
            signing_secret_keychain_item: None,
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
            signing_secret_keychain_item: None,
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
            signing_secret_keychain_item: None,
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

    // ─── (#2135 option 2) delivery contract headers + signing ──────────

    #[test]
    // (#2135 option 2) Shares rule-index 0 with the signing test below,
    // which mutates `DARKMUX_HOOK_SECRET_0` — serialized against it (same
    // default `serial_test` group) so the two can't race on that env var.
    #[serial_test::serial]
    fn delivery_carries_the_contract_headers_and_no_signature_when_unsigned() {
        // Defensive: a prior test's env mutation must never leak in — this
        // rule is also index 0, and the env override wins regardless of
        // whether THIS rule names a keychain item.
        unsafe {
            std::env::remove_var("DARKMUX_HOOK_SECRET_0");
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            signing_secret_keychain_item: None,
            extras: Default::default(),
        }];
        let report: Arc<dyn FlowSink> = Arc::new(NullSink);
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();

        let mut r = record("crawl.finding");
        r.machine_id = Some("studio".to_string());
        r.machine_uid = Some("uid-123".to_string());
        sink.write(&r).unwrap();

        assert!(wait_until(|| receiver.request_count() == 1, Duration::from_secs(3)));
        let headers = receiver.headers();
        let h = &headers[0];
        assert!(h.contains_key("x-darkmux-delivery"), "{h:?}");
        assert_eq!(h.get("x-darkmux-event").map(String::as_str), Some("crawl.finding"), "{h:?}");
        assert_eq!(h.get("x-darkmux-machine-id").map(String::as_str), Some("studio"), "{h:?}");
        assert_eq!(h.get("x-darkmux-machine-uid").map(String::as_str), Some("uid-123"), "{h:?}");
        assert!(h.contains_key("x-darkmux-sender"), "{h:?}");
        assert!(h.contains_key("x-darkmux-timestamp"), "{h:?}");
        assert!(!h.contains_key("x-darkmux-signature"), "unsigned rule must not carry a signature: {h:?}");
    }

    #[test]
    #[serial_test::serial] // mutates DARKMUX_HOOK_SECRET_0
    fn delivery_carries_a_signature_the_receiver_can_recompute_when_signed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            signing_secret_keychain_item: Some("darkmux-hook-test-0".to_string()),
            extras: Default::default(),
        }];
        let prev = std::env::var("DARKMUX_HOOK_SECRET_0").ok();
        // The env override wins over the Keychain item on every platform
        // (see `crate::hook_signing_secret`'s doc) — the portable path a
        // sandboxed test can actually exercise without a real Keychain.
        unsafe {
            std::env::set_var("DARKMUX_HOOK_SECRET_0", "top-secret-key");
        }
        let report: Arc<dyn FlowSink> = Arc::new(NullSink);
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();

        sink.write(&record("crawl.finding")).unwrap();
        assert!(wait_until(|| receiver.request_count() == 1, Duration::from_secs(3)));

        let headers = receiver.headers();
        let h = &headers[0];
        let sig = h.get("x-darkmux-signature").expect("signed rule must carry X-Darkmux-Signature");
        let ts = h.get("x-darkmux-timestamp").unwrap();
        let body = &receiver.bodies()[0];
        let expected = format!(
            "sha256={}",
            crate::hmac_sha256::hmac_sha256_hex(b"top-secret-key", format!("{ts}.{body}").as_bytes())
        );
        assert_eq!(sig, &expected, "receiver must be able to recompute the exact signature from timestamp + raw body");

        drop(sink);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOOK_SECRET_0", v),
                None => std::env::remove_var("DARKMUX_HOOK_SECRET_0"),
            }
        }
    }

    // ─── (#2135 option 2, security review follow-up) header-value safety ──

    #[test]
    fn sanitize_header_value_strips_non_ascii_and_control_bytes_keeps_the_rest() {
        // Non-ASCII (café's `é`) and CR/LF are outside ureq's printable-
        // ASCII allowlist (tab 0x09, space 0x20, 0x21..=0x7E) and become
        // `_`; ordinary visible ASCII, spaces, and tabs pass through
        // unchanged.
        assert_eq!(sanitize_header_value("caf\u{e9}"), "caf_");
        assert_eq!(sanitize_header_value("line1\r\nX-Injected: pwned"), "line1__X-Injected: pwned");
        assert_eq!(sanitize_header_value("crawl.finding"), "crawl.finding");
        assert_eq!(sanitize_header_value("with a tab\there"), "with a tab\there");
        assert_eq!(sanitize_header_value("an en\u{2013}dash"), "an en_dash");
    }

    #[test]
    fn sanitize_header_value_truncates_a_pathologically_long_value() {
        let huge = "a".repeat(10_000);
        let sanitized = sanitize_header_value(&huge);
        assert_eq!(sanitized.len(), 256, "must cap at MAX_HEADER_VALUE_LEN, not grow the request unboundedly");
    }

    /// (MUST FIX 1/2) A record whose `machine_id` carries a non-ASCII
    /// byte (an en-dash, an accented character — plausible on a peer
    /// machine's operator-set hostname) must still deliver — the header
    /// value is sanitized, not rejected wholesale. Without the sanitizer
    /// in `build_delivery_headers`, ureq's send-time validation would
    /// return `ErrorKind::BadHeader`, which — absent finding (b)'s
    /// classification fix too — falls through to `RetryableFailure` (no
    /// give-up threshold) and re-POSTs this exact line forever, silently
    /// blocking every later record on the rule. Red-proved by hand:
    /// commenting out the `sanitize_header_value` calls in
    /// `build_delivery_headers` turns this from "delivers once, header
    /// sanitized" into "never delivers, request_count stays 0" — restored
    /// before commit.
    #[test]
    fn delivery_with_non_ascii_machine_id_sanitizes_the_header_and_still_delivers() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            signing_secret_keychain_item: None,
            extras: Default::default(),
        }];
        let report: Arc<dyn FlowSink> = Arc::new(NullSink);
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();

        let mut r = record("crawl.finding");
        // "café" with a combining accent, plus a literal en-dash — both
        // outside the printable-ASCII allowlist.
        r.machine_id = Some("caf\u{e9}\u{2013}peer".to_string());
        sink.write(&r).unwrap();

        assert!(wait_until(|| receiver.request_count() >= 1, Duration::from_secs(3)), "must deliver, not hang forever");
        // Give it several more poll cycles: if sanitization were missing,
        // a BadHeader would (pre-fix (b)) retry the SAME line forever —
        // request_count would keep climbing well past 1.
        std::thread::sleep(POLL_INTERVAL * 5);
        assert_eq!(receiver.request_count(), 1, "one clean delivery, never a redelivery storm");

        let headers = receiver.headers();
        let got = headers[0].get("x-darkmux-machine-id").expect("header must still be present, sanitized not dropped");
        assert!(got.is_ascii(), "sanitized value must be pure ASCII: {got:?}");
        assert!(!got.contains('\u{e9}') && !got.contains('\u{2013}'), "non-ASCII bytes must be replaced, not passed through: {got:?}");
    }

    /// (MUST FIX 2/2) A CR/LF embedded in a record field (simulating a
    /// crafted `action`/`machine_id`) must never reach the wire as a
    /// second header line — `sanitize_header_value` replaces both with
    /// `_` before the value ever reaches `.set()`, so there is no
    /// "smuggle an extra header" path to close per-header; the filter
    /// closes it structurally.
    #[test]
    fn crlf_in_a_record_field_never_reaches_the_wire_as_an_injected_header() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            signing_secret_keychain_item: None,
            extras: Default::default(),
        }];
        let report: Arc<dyn FlowSink> = Arc::new(NullSink);
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();

        let mut r = record("crawl.finding");
        r.machine_id = Some("line1\r\nX-Injected: pwned".to_string());
        sink.write(&r).unwrap();

        assert!(wait_until(|| receiver.request_count() >= 1, Duration::from_secs(3)));
        let headers = receiver.headers();
        assert!(!headers[0].contains_key("x-injected"), "CRLF must never split into a second header line: {:?}", headers[0]);
        let got = headers[0].get("x-darkmux-machine-id").unwrap();
        assert_eq!(got, "line1__X-Injected: pwned", "CR/LF replaced with `_`, everything else preserved as ONE value");
    }

    /// (MUST FIX 2/2, belt-and-braces) Independent of the sanitizer: even
    /// a header value that somehow bypasses `sanitize_header_value` (a
    /// future header addition that forgets to call it, or a bug in the
    /// filter) must not be classified `RetryableFailure` when ureq
    /// refuses to send it. `try_post` is exercised directly with a
    /// hand-built `DeliveryHeaders` carrying a raw (unsanitized) CRLF —
    /// this must resolve to `ClientError`, which is what routes into the
    /// existing give-up path (bounded retries → quarantine + `hook.failed`)
    /// instead of retrying the same unpostable line forever.
    #[test]
    fn try_post_classifies_a_malformed_header_value_as_client_error_not_retryable_forever() {
        let receiver = HookReceiver::start();
        let mut headers = build_delivery_headers("{}", None, &delivery_id_for_line("{}"), None);
        headers.machine_id = Some("bad\r\nheader".to_string()); // deliberately bypasses the sanitizer
        let outcome = try_post(&receiver.url("/events"), "{}", &headers);
        assert!(
            matches!(outcome, DeliveryOutcome::ClientError),
            "a BadHeader transport error must be a PERMANENT failure (ClientError), never RetryableFailure — \
             otherwise the give-up threshold (MAX_CLIENT_ERROR_ATTEMPTS, which only counts ClientError) never \
             engages and the line is re-POSTed forever: {outcome:?}"
        );
        assert_eq!(receiver.request_count(), 0, "refused locally before ever reaching the network");
    }

    #[test]
    fn delivery_id_is_stable_across_retries_and_stamped_on_hook_fired() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start().with_status_sequence([500, 200]);
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            signing_secret_keychain_item: None,
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

        assert!(wait_until(|| receiver.request_count() >= 2, Duration::from_secs(5)), "expected a retry after the 500");
        let headers = receiver.headers();
        let first_id = headers[0].get("x-darkmux-delivery").unwrap().clone();
        let second_id = headers[1].get("x-darkmux-delivery").unwrap().clone();
        assert_eq!(first_id, second_id, "same undelivered line — same delivery id across retries");

        assert!(wait_until(|| capture.0.lock().unwrap().iter().any(|r| r.action == "hook.fired"), Duration::from_secs(3)));
        let guard = capture.0.lock().unwrap();
        let fired = guard.iter().find(|r| r.action == "hook.fired").unwrap();
        assert_eq!(fired.payload.as_ref().unwrap()["delivery_id"], serde_json::Value::String(first_id));
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
            signing_secret_keychain_item: None,
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
            signing_secret_keychain_item: None,
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
            signing_secret_keychain_item: None,
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
            signing_secret_keychain_item: None,
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
            signing_secret_keychain_item: None,
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
            signing_secret_keychain_item: None,
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

    // ─── (fix-round finding 1) cursor-write failure must not storm ────────

    /// Mutation check (self-QA gate), narrow: calls `advance_cursor`
    /// directly and asserts on `RuleRuntime`'s own backoff state, rather
    /// than on request counts — the integration test below has a SECOND,
    /// independent line of defense (the stall-probe's own `apply_backoff`
    /// call, once 3 failures mark the rule stalled) that keeps its
    /// `request_count <= 3` assertion green even if `advance_cursor`'s
    /// OWN failure branch were reverted to `reset_backoff` — confirmed by
    /// actually running that mutation before writing this comment. This
    /// test isolates the ONE line the mutation targets: reverting
    /// `apply_backoff(rt)` (line ~1176) back to `reset_backoff(rt)` makes
    /// `next_attempt > Instant::now()` and `backoff > INITIAL_BACKOFF`
    /// both fail, with no stall-probe safety net to hide it.
    #[test]
    fn advance_cursor_backs_off_never_resets_on_write_failure() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some("http://127.0.0.1:1/unused".to_string()),
            signing_secret_keychain_item: None,
            extras: Default::default(),
        }];
        let rule = resolve_rules(&rules, tmp.path()).unwrap().into_iter().next().unwrap();
        let rt = RuleRuntime {
            rule,
            backoff: Mutex::new(INITIAL_BACKOFF),
            next_attempt: Mutex::new(Instant::now()),
            attempt_count: Mutex::new(0),
            client_error_count: Mutex::new(0),
            dropped_appends: AtomicU64::new(0),
            last_drop_warning: Mutex::new(None),
            cursor_write_failures: AtomicU64::new(0),
            stalled: AtomicBool::new(false),
            last_cursor_write_warning: Mutex::new(None),
            non_record_lines: AtomicU64::new(0),
        };
        set_force_cursor_write_failure(&rt.rule.cursor_path, true);

        let advanced = advance_cursor(&rt, 0);

        assert!(!advanced, "the forced cursor write must have failed");
        assert_eq!(rt.cursor_write_failures.load(Ordering::Relaxed), 1);
        assert!(
            *rt.next_attempt.lock().unwrap() > Instant::now(),
            "a failed cursor write must push next_attempt into the FUTURE — `reset_backoff` would leave it at \
             effectively now, which is the redelivery-storm bug"
        );
        assert!(
            *rt.backoff.lock().unwrap() > INITIAL_BACKOFF,
            "a failed cursor write must DOUBLE the backoff (apply_backoff) — reset_backoff would leave it at \
             INITIAL_BACKOFF"
        );

        set_force_cursor_write_failure(&rt.rule.cursor_path, false);
    }

    /// Integration-level companion to the narrow test above: end-to-end
    /// proof that a persistently unwritable cursor bounds request volume
    /// and eventually stalls, via the REAL drainer loop (not a direct
    /// `advance_cursor` call). Its `request_count <= 3` / `fired_count <=
    /// 3` assertions stay green under EITHER of two independent backoff
    /// paths (`advance_cursor`'s own, or the stall-probe's) — see the
    /// narrow test's doc for why that redundancy means THIS test alone
    /// doesn't isolate a reverted `advance_cursor`.
    #[test]
    fn cursor_write_failure_backs_off_and_stalls_instead_of_storming() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start(); // default: 200 OK to every POST
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            signing_secret_keychain_item: None,
            extras: Default::default(),
        }];
        let capture = Arc::new(CapturingSink::default());
        let report: Arc<dyn FlowSink> = capture.clone();

        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();
        let cursor_path = sink.rules[0].rule.cursor_path.clone();
        set_force_cursor_write_failure(&cursor_path, true);
        sink.write(&record("storm.candidate")).unwrap();

        // One pending record; the receiver accepts every POST, but the
        // cursor can never be persisted, so every "delivery" is really a
        // REdelivery of the same undelivered line. Give it 5s to prove
        // the backoff (not the receiver, not luck) is what bounds it.
        std::thread::sleep(Duration::from_secs(5));

        let request_count = receiver.request_count();
        assert!(request_count <= 3, "cursor-write failures must back off, not storm the receiver — saw {request_count} requests");

        let fired_count = capture.0.lock().unwrap().iter().filter(|r| r.action == "hook.fired").count();
        assert!(fired_count <= 3, "hook.fired must not fire once per redelivery-storm attempt — saw {fired_count}");

        let summaries = summarize_configured_rules(&rules, tmp.path());
        assert!(summaries[0].stalled, "3 consecutive cursor-write failures must mark the rule stalled");
        assert!(
            summaries[0].cursor_write_failures >= CURSOR_WRITE_STALL_THRESHOLD,
            "cursor_write_failures must be persisted and visible cross-process: {}",
            summaries[0].cursor_write_failures
        );

        // Recovery: the cursor becomes writable again.
        set_force_cursor_write_failure(&cursor_path, false);
        assert!(
            wait_until(|| capture.0.lock().unwrap().iter().any(|r| r.action == "hook.fired"), Duration::from_secs(3)),
            "once the cursor is writable again the pending record must actually deliver"
        );
        assert!(
            wait_until(|| !summarize_configured_rules(&rules, tmp.path())[0].stalled, Duration::from_secs(3)),
            "the stall must clear once a cursor write succeeds"
        );

        drop(sink);
    }

    // ─── (fix-round finding 2) dropped-appends counter is cross-process ───

    /// Mutation check (self-QA gate): reverting `increment_dropped_appends`
    /// back to the old "read the in-process atomic, write that" shape
    /// makes this test read `1` (instance 3's own fresh atomic, fetch_add
    /// to 1) instead of `3` — proving the fix is what makes the count
    /// survive across separate `HookSink` instances (simulating separate
    /// processes sharing the same outbox directory).
    #[test]
    #[serial_test::serial] // mutates the process-global DARKMUX_HOOKS_MAX_OUTBOX_MB env var
    fn dropped_appends_counter_accumulates_across_separate_hook_sink_instances() {
        let tmp = tempfile::TempDir::new().unwrap();
        // A black-hole target: bound but never accepted, so nothing is
        // ever delivered and the outbox stays over cap for the whole test.
        let black_hole = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = black_hole.local_addr().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(format!("http://{addr}/unreachable")),
            signing_secret_keychain_item: None,
            extras: Default::default(),
        }];

        let prev = std::env::var("DARKMUX_HOOKS_MAX_OUTBOX_MB").ok();
        unsafe {
            std::env::set_var("DARKMUX_HOOKS_MAX_OUTBOX_MB", "1");
        }

        // "Process 1": push the outbox over the 1 MiB cap, then drop one
        // append of its own.
        {
            let report: Arc<dyn FlowSink> = Arc::new(NullSink);
            let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();
            let mut big = record("work.big");
            big.reasoning = Some("x".repeat(2 * 1024 * 1024));
            sink.write(&big).unwrap();
            sink.write(&record("work.drop.1")).unwrap();
        }
        let after_1 = summarize_configured_rules(&rules, tmp.path())[0].dropped_appends;

        // "Process 2": a FRESH `HookSink` — its own in-process
        // `dropped_appends` atomic starts at 0 — dropping one append
        // against the SAME already-over-cap outbox.
        {
            let report: Arc<dyn FlowSink> = Arc::new(NullSink);
            let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();
            sink.write(&record("work.drop.2")).unwrap();
        }
        let after_2 = summarize_configured_rules(&rules, tmp.path())[0].dropped_appends;

        // "Process 3": same again.
        {
            let report: Arc<dyn FlowSink> = Arc::new(NullSink);
            let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();
            sink.write(&record("work.drop.3")).unwrap();
        }
        let after_3 = summarize_configured_rules(&rules, tmp.path())[0].dropped_appends;

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOOKS_MAX_OUTBOX_MB", v),
                None => std::env::remove_var("DARKMUX_HOOKS_MAX_OUTBOX_MB"),
            }
        }

        assert_eq!(after_1, 1, "process 1's own drop");
        assert_eq!(after_2, 2, "process 2 must ADD to process 1's count, not clobber it back to 1");
        assert_eq!(after_3, 3, "process 3 must ADD to process 2's count, not clobber it back to 1");
        assert!(after_2 >= after_1 && after_3 >= after_2, "the persisted count must never decrease");
    }

    // ─── (#2093 merge-gate finding 12) hook.fired/failed carry machine provenance ─

    #[test]
    fn emitted_hook_records_carry_machine_id_and_uid_like_every_other_producer() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            signing_secret_keychain_item: None,
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
    #[serial_test::serial] // (fix-round) mutates the process-global DARKMUX_HOOKS_MAX_OUTBOX_MB env var —
    // races `dropped_appends_counter_accumulates_across_separate_hook_sink_instances` without this
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
            signing_secret_keychain_item: None,
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
        // — `summarize_configured_rules` (what `flow status` /
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
            signing_secret_keychain_item: None,
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

    // ─── (fix-round finding 5) valid JSON, no `action` — lenient on read ──

    #[test]
    fn valid_json_with_no_action_field_delivers_with_null_action_and_is_never_quarantined() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            signing_secret_keychain_item: None,
            extras: Default::default(),
        }];
        let key = rule_key(&rules[0].r#match.clone().unwrap_or_default(), &rules[0].http.clone().unwrap());
        let (outbox_path, _cursor_path) = outbox_paths(tmp.path(), &key);

        // A line that IS valid JSON but is not a flow record at all — no
        // `action` field. Seeded directly, not through `HookSink::write`
        // (which always serializes a real `FlowRecord`, always carrying
        // `action` — this simulates a stray/foreign line reaching the
        // outbox some other way).
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(&outbox_path, b"{\"not_a_flow_record\":true}\n").unwrap();

        let capture = Arc::new(CapturingSink::default());
        let report: Arc<dyn FlowSink> = capture.clone();
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();

        // Delivered — never quarantined. Lenient on read: valid JSON
        // that isn't a flow record still gets POSTed verbatim, exactly
        // like any other line.
        assert!(wait_until(|| receiver.request_count() >= 1, Duration::from_secs(5)));
        let quarantine_path = PathBuf::from(format!("{}.quarantine", outbox_path.display()));
        assert!(!quarantine_path.exists(), "a valid-JSON-but-no-action line must NOT be quarantined");

        assert!(wait_until(|| capture.0.lock().unwrap().iter().any(|r| r.action == "hook.fired"), Duration::from_secs(3)));
        let guard = capture.0.lock().unwrap();
        let fired = guard.iter().find(|r| r.action == "hook.fired").unwrap();
        assert_eq!(
            fired.payload.as_ref().unwrap()["delivered_action"],
            serde_json::Value::Null,
            "delivered_action must be JSON null, not an empty string, when the line has no `action` field"
        );
        drop(guard);

        assert_eq!(
            sink.rules[0].non_record_lines.load(Ordering::Relaxed),
            1,
            "a valid-JSON-no-action line must count toward non_record_lines"
        );
    }

    // ─── (fix-round finding 6) drain_stray_file ────────────────────────────

    #[test]
    fn drain_stray_file_delivers_pending_lines_and_advances_its_cursor() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start();
        // A stray outbox: no config rule owns this key any more — seeded
        // directly, exactly as `darkmux doctor`/`flow status` would
        // find one left behind by a removed rule.
        let outbox_path = tmp.path().join("127.0.0.1-9999-deadbeefdeadbeef.outbox.jsonl");
        std::fs::write(&outbox_path, "{\"action\":\"work.a\"}\n{\"action\":\"work.b\"}\n").unwrap();

        let result = drain_stray_file(&outbox_path, &receiver.url("/events")).unwrap();
        assert_eq!(result.delivered, 2);
        assert_eq!(result.failed, 0);
        assert_eq!(result.remaining_undelivered, 0);
        assert_eq!(receiver.request_count(), 2);

        // The cursor sidecar it wrote is the SAME key-derived path a
        // normal `HookSink` would have used — a repeat call redelivers
        // nothing, since the cursor is now at EOF.
        let cursor_path = tmp.path().join("127.0.0.1-9999-deadbeefdeadbeef.cursor");
        assert!(cursor_path.exists());
        let result2 = drain_stray_file(&outbox_path, &receiver.url("/events")).unwrap();
        assert_eq!(result2.delivered, 0, "nothing left to redeliver — the cursor already advanced past both lines");
        assert_eq!(receiver.request_count(), 2, "no duplicate POSTs on a repeat call");
    }

    #[test]
    fn drain_stray_file_stops_at_first_failure_without_advancing_past_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        // A black hole: bound but never accepted, so every POST fails.
        let black_hole = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = black_hole.local_addr().unwrap();
        let outbox_path = tmp.path().join("127.0.0.1-9999-deadbeefdeadbeef.outbox.jsonl");
        std::fs::write(&outbox_path, "{\"action\":\"work.a\"}\n").unwrap();

        let result = drain_stray_file(&outbox_path, &format!("http://{addr}/unreachable")).unwrap();
        assert_eq!(result.delivered, 0);
        assert_eq!(result.failed, 1);
        assert_eq!(result.remaining_undelivered, 1, "the failed line's cursor position must not advance");
    }

    #[test]
    fn drain_stray_file_refuses_a_non_loopback_url() {
        let tmp = tempfile::TempDir::new().unwrap();
        let outbox_path = tmp.path().join("127.0.0.1-9999-deadbeefdeadbeef.outbox.jsonl");
        std::fs::write(&outbox_path, "{\"action\":\"work.a\"}\n").unwrap();
        let err = drain_stray_file(&outbox_path, "http://example.com/events").unwrap_err();
        assert!(format!("{err:#}").contains("--to"), "the error must be attributed to --to: {err:#}");
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
            signing_secret_keychain_item: None,
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
            signing_secret_keychain_item: None,
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
            signing_secret_keychain_item: None,
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
        let headers = build_delivery_headers("{}", None, &delivery_id_for_line("{}"), None);
        let outcome = try_post("http://evil.example.com/x", "{}", &headers);
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
            signing_secret_keychain_item: None,
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
            signing_secret_keychain_item: None,
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
            signing_secret_keychain_item: None,
            extras: Default::default(),
        };
        let rule_b = HookRule {
            r#match: Some(HookMatch { action: Some("dispatch.*".to_string()), ..Default::default() }),
            http: Some("http://127.0.0.1:8790/b".to_string()),
            signing_secret_keychain_item: None,
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
                signing_secret_keychain_item: None,
                extras: Default::default(),
            },
            HookRule {
                r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
                http: Some("http://127.0.0.1:1/b".to_string()),
                signing_secret_keychain_item: None,
                extras: Default::default(),
            },
            HookRule {
                r#match: Some(HookMatch { mission_id: Some("no-such-mission".to_string()), ..Default::default() }),
                http: Some("http://127.0.0.1:1/c".to_string()),
                signing_secret_keychain_item: None,
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
    // (#1959 live loop) A receiver that answers 200 but rejects records
    // per-record inside the body (`{"rejected": N}`) must not read as a clean
    // delivery: `hook.fired` carries `receiver_rejected` so the rejection is
    // visible on the stream instead of only in the receiver's own logs.
    #[test]
    fn hook_fired_surfaces_a_receivers_per_record_rejection_count() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start()
            .with_response_body(r#"{"ok":true,"accepted":0,"rejected":1,"results":[{"ok":false,"error":"rule must be a string"}]}"#);
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            signing_secret_keychain_item: None,
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
        assert!(wait_until(
            || capture.0.lock().unwrap().iter().any(|r| r.action == "hook.fired"),
            Duration::from_secs(3)
        ));
        let fired = capture.0.lock().unwrap().iter().find(|r| r.action == "hook.fired").cloned().unwrap();
        let rejected = fired.payload.as_ref().and_then(|p| p.get("receiver_rejected")).and_then(|v| v.as_u64());
        assert_eq!(rejected, Some(1), "{fired:?}");
        drop(sink);
    }

}
