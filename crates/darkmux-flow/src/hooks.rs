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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// (#2183) Expand a leading `~` to the user's home directory for a
/// `hooks.rules[].file` directory value — the same behavior
/// `darkmux_types::paths::expand_tilde` gives `hooks.outbox_dir`, but that
/// helper is `pub(crate)` to `darkmux-types` (only `config_access` may
/// call it), so this is a small local copy rather than a new public API
/// surface on `darkmux-types` for one field. Pass-through for any other
/// shape; returns the input unchanged if no home dir is available.
fn expand_tilde_dir(s: &str) -> PathBuf {
    if let Some(stripped) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    } else if s == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(s)
}

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
    // (#1959) Payload predicates — `"payload.tool_name": "create_finding"`
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

/// (#2273 fix-round finding 1) Sibling of `dropped_appends_path` — where
/// the CUMULATIVE count of records this rule's receiver reported
/// rejecting is persisted.
///
/// Deliberately its OWN file rather than a field on the `.last` sidecar.
/// `.last` is a LAST-VALUE document: `write_last_status` derives a
/// complete fresh `LastStatus` from `RuleRuntime`'s own atomics and
/// truncate-replaces the whole file on every terminal outcome, so a
/// rejection recorded there is erased by the very next clean delivery —
/// seconds later, on a live rule. That shape is right for `stalled`
/// (genuinely CURRENT state, read live from an atomic) and wrong for a
/// rejection, which is a historical EVENT. `dropped_appends` and
/// `quarantined_lines` are the honest neighbors: both accumulate on
/// substrates nothing wholesale-overwrites (this file's sibling, and the
/// quarantine file's own line count), and this counter follows
/// `dropped_appends` exactly. `LastStatus::last_receiver_rejected` is
/// kept alongside it, but only as last-delivery CONTEXT — the signal
/// `darkmux doctor` keys on is this total.
fn receiver_rejected_path(outbox_dir: &Path, key: &str) -> PathBuf {
    outbox_dir.join(format!("{key}.rejected"))
}

/// Read one of the plain-text cumulative counter sidecars
/// (`<key>.dropped`, `<key>.rejected`). Absent or unparseable -> 0, so a
/// read-only reporting caller never fails on a rule that has never
/// counted one.
fn read_counter_sidecar(path: &Path) -> u64 {
    fs::read_to_string(path).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

/// Write via a sibling temp file + atomic `rename(2)` — same shape as
/// `write_cursor` — so a concurrent `read_counter_sidecar` never observes
/// the sidecar mid-truncate.
fn write_counter_sidecar_atomic(path: &Path, count: u64) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let tmp_path = PathBuf::from(format!("{}.tmp", path.display()));
    fs::write(&tmp_path, count.to_string()).with_context(|| format!("writing {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path).with_context(|| format!("renaming {} to {}", tmp_path.display(), path.display()))
}

/// (fix-round finding 2) Cross-process read-modify-write of one of the
/// cumulative counter sidecars, under the SAME `flock` an append to
/// `outbox_path` takes (`append_outbox_line`/`with_locked_file`) — so two
/// `HookSink` instances (two darkmux processes racing the same outbox)
/// can never both read the sidecar's stale value and each write back
/// their own single-process count, clobbering one another. Returns the
/// new persisted total (best-effort: on a lock/IO failure, falls back to
/// `+delta` off whatever was last read, so the return value is never
/// worse than the pre-fix single-process behavior).
///
/// The lock is taken on `outbox_path`, NOT on the counter file itself,
/// and that is load-bearing rather than incidental: this function
/// replaces the counter file's inode (temp + `rename`) on every write, so
/// locking it by its own path would leave two writers holding locks on
/// two DIFFERENT inodes the moment one of them renamed. `outbox_path` is
/// the stable per-rule file every other writer of this rule already
/// contends on.
fn add_counter_sidecar(outbox_path: &Path, counter_path: &Path, delta: u64, label: &str) -> u64 {
    let result = darkmux_types::flock::with_locked_file(outbox_path, |_file| {
        let count = read_counter_sidecar(counter_path) + delta;
        write_counter_sidecar_atomic(counter_path, count)?;
        Ok(count)
    });
    match result {
        Ok(count) => count,
        Err(e) => {
            eprintln!("flow::HookSink: failed to persist {label} count to {}: {e:#}", counter_path.display());
            read_counter_sidecar(counter_path) + delta
        }
    }
}

/// [`add_counter_sidecar`] for the `dropped_appends` counter — one
/// refused write at a time.
fn increment_dropped_appends(outbox_path: &Path, dropped_appends_path: &Path) -> u64 {
    add_counter_sidecar(outbox_path, dropped_appends_path, 1, "dropped-appends")
}

/// (#2273 fix-round finding 1) [`add_counter_sidecar`] for the cumulative
/// receiver-rejection counter — `n` is the count the receiver itself
/// reported for THIS delivery, so the running total counts records
/// reported rejected, not deliveries that carried a rejection.
fn add_receiver_rejected(outbox_path: &Path, receiver_rejected_path: &Path, n: u64) -> u64 {
    add_counter_sidecar(outbox_path, receiver_rejected_path, n, "receiver-rejected")
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
    /// (#2273) The receiver's own per-record rejection count from the
    /// LAST successful (2xx) delivery — `None` when that delivery was
    /// cleanly accepted, or when the last terminal outcome wasn't a
    /// success at all (a `ClientError`/`RedirectRefused`/quarantine has
    /// no receiver-rejection concept, so it's never set on those paths).
    /// This is what makes a receiver rejection visible to a SEPARATE
    /// `darkmux doctor` invocation after the fact — the `hook.fired` flow
    /// record it rode in on is a point-in-time event on the stream, not
    /// durable per-rule state. Lenient-on-read: absent in a sidecar
    /// written before this field existed, defaults to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_receiver_rejected: Option<u64>,
    /// (#2196) The receiver's own `results[].error` text for the LAST
    /// successful delivery's rejected record(s) — CONTEXT alongside
    /// `last_receiver_rejected`, same last-value shape and same erasure
    /// caveat (a later clean delivery truncate-replaces this sidecar, so
    /// this is never what a check keys on; see `receiver_rejected_total`
    /// for the cumulative count doctor/`flow status` use). Empty when the
    /// last delivery was clean, or when it was rejected but the receiver's
    /// body carried no `results` detail. Lenient-on-read: absent in a
    /// sidecar written before this field existed, defaults to empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    last_receiver_rejected_reasons: Vec<String>,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// (#2453) Truncate-and-replace the `.last` sidecar's ENTIRE content
/// under an exclusive `flock(2)` on the sidecar's OWN path — same "lock
/// the file itself, do the I/O through the locked handle" shape
/// `append_outbox_line`/`ensure_trailing_newline` already use for the
/// outbox, just an overwrite instead of an append. Used by
/// `write_last_status`, which always derives a complete fresh document
/// from `RuleRuntime`'s own atomics rather than reading the file first —
/// there's nothing for IT to read here, only something to serialize
/// against: a concurrent `write_cursor_write_status` sharing this same
/// path takes the SAME lock (`flock` contends on the open file
/// description / inode, not on which function opened it), so the two can
/// never interleave.
///
/// Locking the sidecar's own path (rather than a new `.last.lock`
/// sibling) keeps the per-rule file set exactly as it was — nothing new
/// to collide with `quarantine_path`/`drain_lock_path`/
/// `dropped_appends_path` — and a BRAND-NEW sidecar gets the same
/// `0o600` creation mode (#2259) every other locked file already gets,
/// for free. An EXISTING sidecar (pre-#2453, created at the process
/// umask's mode — typically `0o644`, world-readable) keeps that mode:
/// `lock_exclusive`'s `.mode()` only applies at creation, and this
/// function never `chmod`s it retroactively (see `lock_exclusive`'s own
/// doc for why that's deliberate).
fn write_status_sidecar_locked(path: &Path, json: &[u8]) -> Result<()> {
    darkmux_types::flock::with_locked_file(path, |file| {
        file.set_len(0).with_context(|| format!("truncating {}", path.display()))?;
        file.seek(SeekFrom::Start(0)).with_context(|| format!("seeking {}", path.display()))?;
        // (#2453 review red-prove seam) Widens the TRUNCATE-to-WRITE
        // window on demand — the window a concurrent READER tears in.
        // A SEPARATE map from `LAST_STATUS_RACE_HOOKS`, deliberately:
        // these two seams fire from different writers, and a test that
        // arms one must not be woken by the other. Sharing one map
        // deadlocks `concurrent_writers_do_not_lose_last_status_update`
        // outright — that test arms the rendezvous channel for the
        // READ seam and receives from it exactly once, so a second
        // firing point blocks forever on `send` with no receiver left,
        // while holding this file's exclusive lock. Compiled out of
        // every non-test build.
        #[cfg(test)]
        fire_last_status_truncate_hook(path);
        file.write_all(json).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    })
}

/// Write the TERMINAL delivery outcome (success or give-up) for a rule's
/// current line — `ok`/`error` describe that outcome. The cursor-write
/// bookkeeping fields (`cursor_write_failures`/`stalled`) are read from
/// `rt`'s own atomics rather than defaulted, so a terminal write here
/// never clobbers a stall recorded moments earlier by `advance_cursor`
/// (finding 1) — the two can legitimately coexist: the POST succeeded
/// (this call reports `ok: true`) even though the cursor write that was
/// supposed to record it durably is currently failing.
///
/// (#2273) Thin wrapper over [`write_last_status_full`] that always
/// writes `last_receiver_rejected: None` — every call site except the
/// `DeliveryOutcome::Success` one has no receiver-rejection outcome to
/// report (a give-up, a redirect refusal, or a quarantine never got a
/// 2xx response body to read one from).
fn write_last_status(rt: &RuleRuntime, ok: bool, error: Option<&str>) {
    write_last_status_full(rt, ok, error, None, Vec::new())
}

/// (#2273) Full form of [`write_last_status`] — takes the receiver's
/// per-record rejection count from the delivery this call reports on, so
/// it survives into the `.last` sidecar for a later `darkmux doctor` run
/// to surface, not just the `hook.fired` flow record the delivery already
/// emitted. `last_receiver_rejected_reasons` (#2196) is that same
/// delivery's `results[].error` text, empty when `last_receiver_rejected`
/// is `None` or the receiver's body carried no per-record detail.
fn write_last_status_full(
    rt: &RuleRuntime,
    ok: bool,
    error: Option<&str>,
    last_receiver_rejected: Option<u64>,
    last_receiver_rejected_reasons: Vec<String>,
) {
    let status = LastStatus {
        ts: schema::ts_utc_now(),
        ok,
        error: error.map(str::to_string),
        cursor_write_failures: rt.cursor_write_failures.load(Ordering::Acquire),
        stalled: rt.stalled.load(Ordering::Acquire),
        last_receiver_rejected,
        last_receiver_rejected_reasons,
    };
    if let Ok(json) = serde_json::to_string(&status) {
        // (#2453) Locked on the sidecar's own path — see
        // `write_status_sidecar_locked`'s doc — so this can never
        // interleave with a concurrent `write_cursor_write_status`
        // sharing the same file, whether that's a second thread or a
        // second darkmux process sharing this outbox directory.
        if let Err(e) = write_status_sidecar_locked(&rt.rule.last_status_path, json.as_bytes()) {
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
///
/// (#2453) The read and the write now happen inside ONE critical section
/// — a single exclusive `flock` on `path` held across both, acquired
/// through the SAME `with_locked_file` primitive `append_outbox_line`
/// uses for the outbox (`darkmux_types::flock`). Before this fix, the
/// read (`read_last_status`) and the write (`write_owner_only_file`)
/// were two independent, unlocked opens with an unguarded window between
/// them — a concurrent `write_last_status` landing in that window was
/// silently reverted the instant this function's stale read got written
/// back over it (a lost update, not merely a torn file). Locking on
/// `path` itself, rather than a separate sidecar lock file, means this
/// contends with `write_last_status`'s lock on the exact same path —
/// there is no third file to keep in sync and no risk of the two
/// functions locking two DIFFERENT files while believing they've
/// serialized against each other.
fn write_cursor_write_status(path: &Path, cursor_write_failures: u64, stalled: bool) {
    let result = darkmux_types::flock::with_locked_file(path, |file| {
        // (#2453 review) Read RAW BYTES, not a `String`. `read_to_string`
        // hard-errors on invalid UTF-8, which aborts this closure and
        // SKIPS the write — permanently, since this function is the only
        // thing that ever rewrites the file on the cursor-failure path.
        // The pre-fix code reached the same state through
        // `read_last_status(..).unwrap_or_else(default)`, which treated
        // any unreadable sidecar as absent and healed it on the next
        // write; parsing from bytes preserves exactly that. Reachable:
        // `error` echoes a delivery failure's body back (see
        // `write_last_status`), so a torn write can land mid-multi-byte
        // character and leave bytes that are not valid UTF-8.
        let mut content = Vec::new();
        file.seek(SeekFrom::Start(0)).with_context(|| format!("seeking {}", path.display()))?;
        file.read_to_end(&mut content).with_context(|| format!("reading {}", path.display()))?;
        let mut status = serde_json::from_slice::<LastStatus>(&content).unwrap_or_else(|_| LastStatus {
            ts: schema::ts_utc_now(),
            ok: true,
            error: None,
            cursor_write_failures: 0,
            stalled: false,
            last_receiver_rejected: None,
            last_receiver_rejected_reasons: Vec::new(),
        });
        // (#2453 red-prove seam) Widens the read-to-write window on
        // demand for the concurrency test — see
        // `fire_last_status_race_hook`'s doc. Deliberately
        // INSIDE the locked closure (unlike the pre-fix placement): the
        // whole point of the fix is that widening this window no longer
        // matters, because nothing else can observe or mutate `path`
        // until this closure returns and the lock releases.
        #[cfg(test)]
        fire_last_status_race_hook(path);
        status.cursor_write_failures = cursor_write_failures;
        status.stalled = stalled;
        let json = serde_json::to_string(&status).context("serializing cursor-write status")?;
        file.set_len(0).with_context(|| format!("truncating {}", path.display()))?;
        file.seek(SeekFrom::Start(0)).with_context(|| format!("seeking {}", path.display()))?;
        file.write_all(json.as_bytes()).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    });
    if let Err(e) = result {
        eprintln!("flow::HookSink: failed to write cursor-write status {}: {e:#}", path.display());
    }
}

/// (#2453) Read the `.last` sidecar under a SHARED `flock(2)` on the
/// same path the two writers take exclusively, so a reader can never
/// observe the file during a writer's truncate-then-write.
///
/// Not a theoretical window. Both writers replace the sidecar's contents
/// IN PLACE (`set_len(0)` -> `seek(0)` -> `write_all`) rather than by
/// atomic rename — which they must, because the writers' mutual
/// exclusion is a lock on this file's own inode, and a rename would swap
/// that inode out from under a concurrent writer's lock, silently
/// reopening the lost update this change exists to close. The sibling
/// sidecars (`.cursor` via `write_cursor`, `.dropped` via
/// `write_dropped_appends_atomic`) can and do use temp+rename precisely
/// because neither has a read-modify-write to serialize; this one pays
/// for its writer safety with a truncate window, and closes that window
/// on the READ side instead.
///
/// What the window costs if left open, measured rather than assumed: an
/// unlocked reader racing a writing rule observed an unparseable
/// (truncated) sidecar on ~37% of reads. Every one of those becomes
/// `None` here, and `summarize_configured_rules` renders `None` as
/// `stalled: false`, `last_error: None`, `last_delivery_ts: None` — a
/// failing, stalled rule reported to `doctor` (and republished by the
/// viewer's console lens) as healthy. The failure direction is the bad
/// one, and it is adversely correlated: the sicker a rule is, the more
/// often it writes status, the likelier an operator's `doctor` run reads
/// a tear.
///
/// Absent file -> `None`, exactly as before (`lock_shared_existing`
/// never creates, so reporting on a rule never materializes its state).
/// Unparseable bytes -> `None`, exactly as before: parsed from the raw
/// bytes rather than through a `String`, so a sidecar that isn't valid
/// UTF-8 is a parse miss and not a hard read error.
fn read_last_status(path: &Path) -> Option<LastStatus> {
    let mut guard = darkmux_types::flock::lock_shared_existing(path).ok()??;
    let mut buf = Vec::new();
    guard.file().read_to_end(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
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
    /// (#2273 fix-round finding 1) Where the CUMULATIVE receiver-rejection
    /// count is persisted — see `receiver_rejected_path`'s doc for why it
    /// is a counter sidecar and not a `.last` field.
    pub receiver_rejected_path: PathBuf,
    /// (#2135 option 2) Which URL policy this rule's target satisfied —
    /// re-validated (not merely cached) at every POST, same reasoning as
    /// `try_post`'s existing loopback re-validation. (#2183) `None` for a
    /// `file`-transport rule — there is no URL policy to satisfy.
    pub target_kind: Option<HookTargetKind>,
    /// (#2135 option 2) This rule's resolved HMAC signing secret, when
    /// `signing_secret_keychain_item` (or the `DARKMUX_HOOK_SECRET_<index>`
    /// env override) named one — `None` means every delivery for this rule
    /// goes out unsigned. Read ONCE here, at construction, same as the
    /// Redis/serve-token Keychain reads.
    pub signing_secret: Option<crate::RawHookSecret>,
    /// (#2183) `Some(dir)` for a `file`-transport rule (mutually exclusive
    /// with `target_kind`/real delivery) — see `resolve_rules`'s refusal
    /// of a rule naming BOTH `http` and `file`, or NEITHER.
    pub file_dir: Option<PathBuf>,
    /// (#2183) This rule's load-time-validated `transform` adapter, when
    /// `transform` names one. `None` → today's behavior, the record
    /// delivered verbatim.
    pub transform: Option<Arc<crate::hook_transform::LoadedAdapter>>,
    /// (#2183) This rule's resolved `headers`, in the SAME order as
    /// configured (a `BTreeMap` on the wire, so already name-sorted —
    /// deterministic delivery + test assertions). Each entry's `1` is
    /// `None` when a Keychain reference failed to resolve — dropped at
    /// delivery (see `resolve_hook_header_value`'s doc), never sent.
    pub headers: Vec<(String, bool, Option<crate::RawHookSecret>)>,
    /// (#2183) Whether this rule's deliveries carry the `X-Darkmux-*`
    /// attribution headers — `headers.attribution_headers`, default `true`.
    pub attribution_headers: bool,
}

/// Resolve + validate every rule against `outbox_dir`. Bails on the FIRST
/// rule missing a destination (`http` XOR `file`), whose `http` target
/// satisfies neither the loopback nor the tailnet URL policy, or whose
/// `transform` names an adapter that doesn't exist / doesn't parse — the
/// whole hooks sink is refused rather than silently dropping one bad rule,
/// so a config mistake is loud at construction, not a quietly-smaller rule
/// set. (#2183's own scoping: an adapter failure is documented as a
/// "load-time refusal for that RULE only" — that promise is kept by
/// `HookSink::new`, which is expected to construct rule-by-rule and treat
/// one bad rule's `resolve_rules` error as dropping only that rule; this
/// function itself stays fail-fast-on-first-error, matching every other
/// rule validation here (URL, empty-http), so a caller wanting per-rule
/// isolation resolves rules one at a time — see `HookSink::new`.)
pub fn resolve_rules(rules: &[HookRule], outbox_dir: &Path) -> Result<Vec<ResolvedRule>> {
    let mut out = Vec::with_capacity(rules.len());
    for (index, r) in rules.iter().enumerate() {
        out.push(resolve_one_rule(index, r, outbox_dir)?);
    }
    Ok(out)
}

/// Resolve + validate exactly one rule — split out of `resolve_rules` so a
/// caller (`HookSink::new`) can isolate one bad rule's refusal from the
/// rest of the sink (#2183's "load-time refusal for that RULE only").
pub fn resolve_one_rule(index: usize, r: &HookRule, outbox_dir: &Path) -> Result<ResolvedRule> {
    let http = r.http.clone().filter(|s| !s.trim().is_empty());
    let file = r.file.clone().filter(|s| !s.trim().is_empty());
    let (url, target_kind, file_dir) = match (&http, &file) {
        (Some(_), Some(_)) => {
            bail!("hook rule #{index} names BOTH `http` and `file` — a rule needs exactly one destination")
        }
        (None, None) => {
            bail!("hook rule #{index} has no destination — set exactly one of `http` or `file`")
        }
        (Some(u), None) => {
            let target_kind = validate_hook_target_url(u).with_context(|| format!("hook rule #{index}"))?;
            (u.clone(), Some(target_kind), None)
        }
        (None, Some(dir)) => {
            let expanded = expand_tilde_dir(dir);
            // A synthetic, never-dialed pseudo-URL — used ONLY for
            // `rule_key`/outbox-file naming and `HookRuleSummary.url`'s
            // display, exactly as `extract_host_port` already expects a
            // `host[:port]`-shaped tail; never passed to `try_post` (file
            // rules never call it) or re-validated as a real URL.
            (format!("file://{}", expanded.display()), None, Some(expanded))
        }
    };
    let match_ = r.r#match.clone().unwrap_or_default();
    let key = rule_key(&match_, &url);
    let (outbox_path, cursor_path) = outbox_paths(outbox_dir, &key);
    let last_status_path = last_status_path(outbox_dir, &key);
    let drain_lock_path = drain_lock_path(outbox_dir, &key);
    let dropped_appends_path = dropped_appends_path(outbox_dir, &key);
    let receiver_rejected_path = receiver_rejected_path(outbox_dir, &key);
    let signing_secret = crate::hook_signing_secret(index, r.signing_secret_keychain_item.as_deref());
    let transform = match &r.transform {
        Some(name) if !name.trim().is_empty() => {
            let adapters_dir = darkmux_types::config_access::hooks_adapters_dir();
            let loaded = crate::hook_transform::load_adapter(&adapters_dir, name)
                .with_context(|| format!("hook rule #{index}'s `transform`"))?;
            Some(Arc::new(loaded))
        }
        _ => None,
    };
    let headers = r
        .headers
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(|(name, v)| {
            let (is_secret, value) = crate::resolve_hook_header_value(&v);
            (name, is_secret, value)
        })
        .collect();
    let attribution_headers = r.attribution_headers.unwrap_or(true);
    Ok(ResolvedRule {
        index,
        match_,
        url,
        outbox_path,
        cursor_path,
        last_status_path,
        drain_lock_path,
        dropped_appends_path,
        receiver_rejected_path,
        target_kind,
        signing_secret,
        file_dir,
        transform,
        headers,
        attribution_headers,
    })
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
    /// (#2273) The receiver's own per-record rejection count from the
    /// LAST successful delivery — read from the PERSISTED `.last`
    /// sidecar, cross-process visible same as `cursor_write_failures`.
    /// `None` when the last delivery was cleanly accepted, or before any
    /// successful delivery has happened yet.
    ///
    /// CONTEXT ONLY — never the signal a reporting surface keys on. The
    /// `.last` sidecar is wholesale-overwritten by every terminal
    /// outcome, so this field self-erases on the next clean delivery:
    /// 400 rejections followed by one clean accept reads `None` here.
    /// `receiver_rejected_total` is the durable count.
    pub last_receiver_rejected: Option<u64>,
    /// (#2196) The receiver's own `results[].error` text for
    /// `last_receiver_rejected`'s rejected record(s) — same LAST-value,
    /// self-erasing shape and the same caveat (never the signal a check
    /// keys on). Empty when the last delivery was clean, or was rejected
    /// but the receiver's body carried no per-record detail to name.
    pub last_receiver_rejected_reasons: Vec<String>,
    /// (#2273 fix-round finding 1) CUMULATIVE count of records this
    /// rule's receiver reported rejecting, across every delivery and
    /// every process — read from the persisted `<key>.rejected` counter
    /// sidecar, exactly like `dropped_appends` reads `<key>.dropped`.
    /// Never reset by a later clean delivery, which is what makes it the
    /// field `darkmux doctor` and `darkmux flow status` key on. The
    /// records are still consumed either way (retrying a receiver-side
    /// content rejection would just repeat it forever); this is what
    /// keeps the rejection visible after the `hook.fired` flow record
    /// that first reported it has scrolled off the stream.
    pub receiver_rejected_total: u64,
    /// (#2093 merge-gate finding 15) This rule's stable filename key —
    /// see `rule_key`'s doc. Exposed so a caller (`darkmux doctor`) can
    /// diff the set of CURRENT rules' keys against what's actually on
    /// disk in `outbox_dir` and name any `*.outbox.jsonl` file that
    /// belongs to no current rule (a rule since removed from config, or
    /// — before this fix — the artifact of an index-based reassignment).
    pub key: String,
    /// (#2183) `true` for a `file`-transport rule — `url` then carries the
    /// synthetic `file://<dir>` display form (see `resolve_one_rule`'s
    /// doc), never a real HTTP target; `is_loopback`/`is_tailnet` are both
    /// `false` and `is_refused` is `false` (a valid `file` rule is not a
    /// URL-policy failure).
    pub is_file: bool,
    /// (#2183) The rule's `transform` NAME, when set.
    pub transform_name: Option<String>,
    /// (#2183) `Some(Ok(short_hash))` when `transform_name` is set and the
    /// adapter exists + parses (the hash makes a silently-changed adapter
    /// visible); `Some(Err(reason))` when it's set but failed to load
    /// (missing file, or doesn't parse as jq) — this is the load-time
    /// refusal `darkmux doctor` surfaces per-rule; `None` when no
    /// `transform` is configured.
    pub transform_status: Option<std::result::Result<String, String>>,
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
            let http = r.http.clone().filter(|s| !s.trim().is_empty());
            let file = r.file.clone().filter(|s| !s.trim().is_empty());
            let is_file = file.is_some() && http.is_none();
            // (#2183) Both-or-neither is a load-time refusal (see
            // `resolve_one_rule`); a summary never bails, so that state
            // renders as `is_refused` here too, same as a bad URL.
            let both_or_neither = (http.is_some() && file.is_some()) || (http.is_none() && file.is_none());
            let url = if is_file {
                let expanded = expand_tilde_dir(file.as_deref().unwrap_or_default());
                format!("file://{}", expanded.display())
            } else {
                http.clone().unwrap_or_default()
            };
            let target_kind = if is_file { None } else { validate_hook_target_url(&url).ok() };
            let is_loopback = !is_file && target_kind == Some(HookTargetKind::Loopback);
            let is_tailnet = !is_file && target_kind == Some(HookTargetKind::Tailnet);
            let is_refused = both_or_neither || (!is_file && target_kind.is_none());
            let signed = r.signing_secret_keychain_item.as_ref().is_some_and(|s| !s.trim().is_empty());
            let key = rule_key(&m, &url);
            let (outbox_path, cursor_path) = outbox_paths(outbox_dir, &key);
            let cursor = read_cursor(&cursor_path);
            let undelivered = undelivered_line_count(&outbox_path, cursor);
            let last = read_last_status(&last_status_path(outbox_dir, &key));
            let dropped_appends = read_counter_sidecar(&dropped_appends_path(outbox_dir, &key));
            let receiver_rejected_total = read_counter_sidecar(&receiver_rejected_path(outbox_dir, &key));
            let last_drainer_heartbeat = fs::read_to_string(heartbeat_path(&cursor_path)).ok();
            let quarantined_lines = undelivered_line_count(&quarantine_path(&outbox_path), 0);
            let transform_name = r.transform.clone().filter(|s| !s.trim().is_empty());
            let transform_status = transform_name.as_deref().map(|name| {
                let adapters_dir = darkmux_types::config_access::hooks_adapters_dir();
                crate::hook_transform::load_adapter(&adapters_dir, name)
                    .map(|a| a.short_hash)
                    .map_err(|e| format!("{e:#}"))
            });
            HookRuleSummary {
                index,
                match_desc: describe_match(&m),
                url,
                is_loopback,
                is_tailnet,
                is_refused,
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
                last_receiver_rejected: last.as_ref().and_then(|s| s.last_receiver_rejected),
                last_receiver_rejected_reasons: last
                    .as_ref()
                    .map(|s| s.last_receiver_rejected_reasons.clone())
                    .unwrap_or_default(),
                receiver_rejected_total,
                key,
                is_file,
                transform_name,
                transform_status,
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
    // (#2259) Owner-only on POSIX — a quarantined line is a raw copy of
    // whatever the outbox held (a torn write, potentially mid-record), so
    // it needs the same protection the outbox itself gets. Open-coded
    // here rather than routed through `write_owner_only_file` because
    // that helper truncates on every call; this is append-only across the
    // lifetime of the outbox (every subsequent invalid line joins the
    // same file), so `.mode()` is set inline instead.
    #[cfg(unix)]
    let opened = {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&path)
    };
    #[cfg(not(unix))]
    let opened = fs::OpenOptions::new().create(true).append(true).open(&path);
    match opened {
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

/// (#2453) Test-only seam that widens the `.last` sidecar's
/// read-modify-write race window on demand, keyed by path (never a
/// single global switch, same parallel-test-safety reason as
/// `FORCE_CURSOR_WRITE_FAILURE_PATHS` above) so unrelated tests running
/// concurrently in the same test binary never see each other's hook. A
/// test registers a `SyncSender<()>` for the sidecar path it's about to
/// race; `write_cursor_write_status`'s read-modify-write fires it (once,
/// right after capturing its read) then sleeps, giving a concurrent
/// `write_last_status` on the SAME path a wide, deterministic window to
/// land its own write mid-critical-section — reproducing #2453's
/// interleaving on every run rather than "one in fifty." Never compiled
/// into a release binary.
#[cfg(test)]
static LAST_STATUS_RACE_HOOKS: std::sync::OnceLock<Mutex<std::collections::HashMap<PathBuf, std::sync::mpsc::SyncSender<()>>>> =
    std::sync::OnceLock::new();

/// (#2453 review) Sibling of `LAST_STATUS_RACE_HOOKS` for the
/// TRUNCATE-to-write window in `write_status_sidecar_locked` — the
/// window a concurrent READER tears in, as opposed to the read-to-write
/// window a concurrent WRITER loses an update in. Kept as its own map so
/// arming one seam never fires the other; see the firing site for the
/// deadlock that sharing them causes.
#[cfg(test)]
static LAST_STATUS_TRUNCATE_HOOKS: std::sync::OnceLock<Mutex<std::collections::HashMap<PathBuf, std::sync::mpsc::SyncSender<()>>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
fn set_last_status_truncate_hook(path: &Path, tx: std::sync::mpsc::SyncSender<()>) {
    let map = LAST_STATUS_TRUNCATE_HOOKS.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    map.lock().unwrap().insert(path.to_path_buf(), tx);
}

#[cfg(test)]
fn clear_last_status_truncate_hook(path: &Path) {
    if let Some(map) = LAST_STATUS_TRUNCATE_HOOKS.get() {
        map.lock().unwrap().remove(path);
    }
}

#[cfg(test)]
fn fire_last_status_truncate_hook(path: &Path) {
    let tx = LAST_STATUS_TRUNCATE_HOOKS.get().and_then(|map| map.lock().unwrap().get(path).cloned());
    if let Some(tx) = tx {
        let _ = tx.send(());
        std::thread::sleep(Duration::from_millis(150));
    }
}

#[cfg(test)]
fn set_last_status_race_hook(path: &Path, tx: std::sync::mpsc::SyncSender<()>) {
    let map = LAST_STATUS_RACE_HOOKS.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    map.lock().unwrap().insert(path.to_path_buf(), tx);
}

#[cfg(test)]
fn clear_last_status_race_hook(path: &Path) {
    if let Some(map) = LAST_STATUS_RACE_HOOKS.get() {
        map.lock().unwrap().remove(path);
    }
}

#[cfg(test)]
fn fire_last_status_race_hook(path: &Path) {
    let tx = LAST_STATUS_RACE_HOOKS.get().and_then(|map| map.lock().unwrap().get(path).cloned());
    if let Some(tx) = tx {
        let _ = tx.send(());
        std::thread::sleep(Duration::from_millis(150));
    }
}

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
        // (#2259) The temp file BECOMES the outbox — `rename` replaces the
        // outbox's inode with this one, so the surviving mode is this
        // file's, not the 0o600 the creator gave the original. Written
        // owner-only so the undelivered records it holds are never
        // world-readable, not even in the window between the write and the
        // `set_permissions` below. That window is why this call is NOT
        // redundant with it — but it is also not pinned by a test: a
        // mode-at-rest assertion can only observe the file after both
        // statements have run. Do not "simplify" it away on the strength of
        // the tests staying green.
        write_owner_only_file(&tmp_path, &remaining)?;
        // `write_owner_only_file`'s `.mode()` only applies when it CREATES.
        // A `.compact.tmp` left behind at 0o644 by a pre-#2259 binary (or by
        // a crash between the write and the rename — the exact case this
        // temp+rename shape exists to survive) is reused, not recreated, so
        // set the mode explicitly too: otherwise one stale temp file
        // reintroduces the world-readable outbox this fix removes.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("setting owner-only mode on {}", tmp_path.display()))?;
        }
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
    /// `receiver_rejected_reasons` is the same body's `results[].error`
    /// text for entries the receiver marked `ok: false` (#2196 — the
    /// count alone told an operator SOMETHING was thrown away, never
    /// WHY, so finding out required replaying against a scratch receiver
    /// or reading the receiver's own log). Bounded and lenient-on-read:
    /// see [`extract_rejection_reasons`]. Empty when the body carries no
    /// `results` array, or none of its entries are marked rejected.
    Success { receiver_rejected: Option<u64>, receiver_rejected_reasons: Vec<String> },
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
    /// (#2183) Whether the `X-Darkmux-*` attribution headers above are
    /// actually sent — `hooks.rules[].attribution_headers`, default
    /// `true`. `false` for a SaaS receiver that rejects unknown headers.
    attribution: bool,
    /// (#2183) This rule's configured `headers`, resolved + sanitized,
    /// ready for `.set()` on the wire — a Keychain reference that failed
    /// to resolve is simply ABSENT here (dropped, never sent empty).
    extra: Vec<(String, String)>,
    /// (#2183) The SAME set, for a diagnostic surface (a flow record, a
    /// `file`-transport dump, `doctor`) — a header whose config value was
    /// a `{"keychain_item": ...}` reference renders as `"<redacted>"`
    /// here regardless of whether it actually resolved; a literal header
    /// renders its real (sanitized) value, same as what's on the wire.
    extra_redacted: Vec<(String, String)>,
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
#[allow(clippy::too_many_arguments)]
fn build_delivery_headers(
    line: &str,
    parsed: Option<&serde_json::Value>,
    delivery_id: &str,
    signing_secret: Option<&crate::RawHookSecret>,
    extra_headers: &[(String, bool, Option<crate::RawHookSecret>)],
    attribution: bool,
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
    // (#2183) `extra`/`extra_redacted` are built in lockstep — the SAME
    // name list, so a diagnostic surface can always be trusted to name
    // every header this rule configured even when a Keychain reference
    // dropped it from the wire set.
    let mut extra = Vec::with_capacity(extra_headers.len());
    let mut extra_redacted = Vec::with_capacity(extra_headers.len());
    for (name, is_secret, value) in extra_headers {
        if *is_secret {
            extra_redacted.push((name.clone(), "<redacted>".to_string()));
        }
        if let Some(v) = value {
            let sanitized = sanitize_header_value(v.expose_for_hmac());
            extra.push((name.clone(), sanitized.clone()));
            if !*is_secret {
                extra_redacted.push((name.clone(), sanitized));
            }
        }
        // `is_secret && value.is_none()`: an unresolved Keychain
        // reference — already recorded in `extra_redacted` above,
        // deliberately ABSENT from `extra` (never sent).
    }
    DeliveryHeaders {
        delivery_id,
        event,
        machine_id,
        machine_uid,
        sender,
        timestamp_ms,
        signature,
        attribution,
        extra,
        extra_redacted,
    }
}

/// (#2196) Cap on how many receiver-reported per-record rejection reason
/// strings ride the `hook.fired` payload / `.last` sidecar / `doctor` /
/// `flow status` surfaces. The receiver's `results[]` array is untrusted
/// input from outside the process; an observability surface must not
/// become a vector for an unbounded string dump from a receiver that
/// (deliberately or not) rejects everything with a huge error body.
const MAX_REJECTION_REASONS: usize = 3;

/// (#2196 fix-round 2, MUST FIX C) The total rendered COLUMN budget for
/// ONE reason exactly as `flow status`/`doctor` actually print it: quoted
/// and, if it contains `"`/`\`, backslash-escaped
/// ([`format_rejection_reasons_for_display`]) — see [`display_width`]
/// for what "column" means here (not bytes, not `char`s).
///
/// (#2196 fix-round MUST FIX 1) The first fix-round set this to 40,
/// reasoning from an assumed 80-column terminal minus the widest known
/// inline-label prefix. Reviewed again in fix-round 2: that bound
/// destroyed the disclosure the feature exists to provide. This PR's own
/// flagship example — `"payload field \"file\" must be a non-empty
/// string"` (47 columns) — lost the word naming the constraint
/// ("string") at 40 columns; a `body.findings[N].severity` validation
/// error loses the field index; a duplicate-key rejection loses the
/// fingerprint and run id, which is the whole actionable content. #2196
/// exists because the count alone told an operator SOMETHING was
/// discarded and never why — a 40-column cap recreates most of that gap.
/// The 40-column number was also buying little: at 40 columns the full
/// row (label + quotes + reason) is 75 columns and doesn't even wrap an
/// 80-column terminal, and what closes the exact-vocabulary forgery of an
/// INDENTED row is [`collapse_whitespace_and_trim`] plus stripping the
/// non-whitespace blank-rendering characters ([`is_stripped_for_display`]),
/// not this width bound — see those functions' docs. The FLUSH-LEFT rows
/// are closed separately, at the render site, by
/// [`rejection_reason_display_lines`] and its wrappers (#2196 fix-rounds
/// 3 and 4, MUST FIX G — `flow status`, then `doctor` and the delivery
/// stderr warning); no sanitizer can close those, because what makes
/// them forgeable is the row's wrap point, which the sanitizer cannot
/// see. The
/// bound below exists to cap payload size and keep a report from an
/// actively hostile receiver bounded, not to defeat forgery.
///
/// 120 is chosen as the upper end of "roughly 100-120": generous enough
/// that realistic receiver validation messages (measured: 47, 83, 108
/// columns for the reasons named above) survive whole, while still
/// bounding an actively hostile receiver's payload. [`REJECTION_REASON_TAIL_RESERVE`]
/// documents how the budget splits between a head and a tail when a
/// reason IS long enough to need cutting.
pub const MAX_REJECTION_REASON_DISPLAY_WIDTH: usize = 120;

/// The two literal `"` characters [`format_rejection_reasons_for_display`]
/// always wraps a reason in, reserved out of [`MAX_REJECTION_REASON_DISPLAY_WIDTH`]
/// so quoting can never itself push the rendered reason over the cap.
pub(crate) const REJECTION_REASON_QUOTE_OVERHEAD: usize = 2;

/// The raw (unquoted, sanitized) column budget every reason is bounded to
/// before quoting — [`MAX_REJECTION_REASON_DISPLAY_WIDTH`] minus the
/// quote overhead. Applied identically at write time
/// ([`extract_rejection_reasons`]) and as the first pass at render time
/// ([`format_rejection_reasons_for_display`]) — see
/// [`format_rejection_reasons_for_display`]'s doc for why a SECOND pass,
/// on the escaped text, is also needed (MUST FIX B/D: escaping must not
/// be able to grow a reason back past the cap).
pub(crate) const REJECTION_REASON_RAW_BUDGET: usize = MAX_REJECTION_REASON_DISPLAY_WIDTH - REJECTION_REASON_QUOTE_OVERHEAD;

/// (#2196 fix-round 2, MUST FIX C) When a reason IS long enough to need
/// cutting, this many columns are reserved for a TAIL kept after the
/// `…`, rather than cutting only the head off. A validation-style reason
/// (`"...severity must be one of low, medium, high, critical — got
/// \"catastrophic\""`) loses the offending value with a head-only cut;
/// a head+tail cut keeps both the constraint AND the value that failed
/// it — see [`bound_reason_width`]. 24 columns comfortably covers a
/// short trailing clause like the example above (19 columns) with a
/// little headroom.
pub(crate) const REJECTION_REASON_TAIL_RESERVE: usize = 24;

/// (#2196 fix-round 3, MUST FIX F) How many `char`s [`bound_reason_width`]
/// allows per column of its width budget — the LENGTH ceiling that rides
/// alongside the COLUMN ceiling.
///
/// This exists because fix-round 3 turned the width bound into a
/// width-ONLY bound and, in doing so, removed the last thing bounding
/// LENGTH. `unicode-width` is correct that a combining mark occupies ZERO
/// rendered columns, so under a pure column budget a combining mark costs
/// nothing: `bound_reason_width`'s head loop never trips on one, and
/// nothing else in the pipeline caps characters or bytes. Measured on the
/// real path (the receiver body is read through a 64 KiB `Read::take`, so
/// this is reachable, not theoretical): a single 2xx response of ~60 KB
/// put 20,000 combining marks — 40,000 bytes, nominal rendered width 0 —
/// into the `.last` sidecar, the `hook.fired` payload (which flows on to
/// the daily JSONL, the audit sink, and Redis `XADD`), the delivery
/// `eprintln!`, and both `flow status` and `doctor`. Twenty thousand
/// marks stacked on one cell is also a terminal-corruption primitive in
/// its own right, not merely volume.
///
/// The two earlier rounds bounded length only by accident: round 1 had a
/// flat 200-BYTE cap, and round 2's hand-rolled width function scored a
/// combining mark as 1 column, so its column budget doubled as a rough
/// character cap. Round 3's correct width function removed both. This
/// constant restores the ceiling EXPLICITLY, as a second budget the head
/// and tail loops honor alongside the column budget, so it cannot be lost
/// again by a width-function change.
///
/// 4 chars/column is deliberately generous rather than tight: legitimate
/// text really does spend several `char`s per rendered column — a
/// decomposed `é` is 2 chars for 1 column, a Devanagari or Thai cluster
/// can stack several combining marks on one, and a regional-indicator
/// flag pair is 2 chars for 2 columns. At [`REJECTION_REASON_RAW_BUDGET`]
/// that is 472 chars — no realistic reason comes close (the longest
/// measured real one is 108 columns / 108 chars), while the hostile
/// 20,000-char case above is cut by a factor of 42.
pub(crate) const REJECTION_REASON_CHARS_PER_COLUMN: usize = 4;

/// (#2196 fix-round 2, MUST FIX A) Is `c` in a Unicode GENERAL CATEGORY
/// that has no business appearing in a human-facing terminal report,
/// full stop?
///
/// The first fix-round enumerated individual code points and ranges
/// (bidi controls, ZWSP/ZWJ/BOM, line/paragraph separators) picked out by
/// hand. Reviewed again in fix-round 2: that approach cannot converge —
/// the verifier's second pass found FOUR MORE characters the enumeration
/// missed (U+061C ARABIC LETTER MARK, U+2060 WORD JOINER, U+2061–2064,
/// U+180E, U+FFF9–FFFB) on its very next look, and a denylist of
/// individual code points can never structurally close against a
/// character Unicode assigns tomorrow. This inverts the check: instead of
/// naming characters to drop, it names the CATEGORIES no receiver-supplied
/// prose has any legitimate reason to use, and drops everything the
/// Unicode Character Database currently assigns — or ever assigns in the
/// future — to one of them:
///
/// - `Cc` (Control) — C0/C1 controls: `\n`, `\r`, ESC, DEL, ...
/// - `Cf` (Format) — the ENTIRE format-character category in one check:
///   every bidi embedding/override/isolate control (the Trojan Source
///   class, CVE-2021-42574), every invisible-math/joining control (ZWSP,
///   ZWNJ, ZWJ, word joiner, the invisible operators), the BOM, the
///   interlinear-annotation triad, ARABIC LETTER MARK, MONGOLIAN VOWEL
///   SEPARATOR, and the TAG block (U+E0000–E007F) — verified against
///   Unicode's own `DerivedCoreProperties.txt`/general-category data,
///   not hand-picked. A future Unicode version's new format character is
///   caught automatically; the first fix-round's four misses would all
///   have been caught by this one check.
/// - `Co` (Private Use) — no character exists here by definition; its
///   rendering is whatever a receiver's chosen (unknowable) font happens
///   to map it to.
/// - `Cs` (Surrogate) — cannot occur in a real `char` at all (Rust's
///   `char` type structurally excludes surrogate code points), kept here
///   only so this function reads as a complete category audit.
/// - `Cn` (Unassigned) — no character is assigned here yet; a receiver
///   naming one is sending noise at best. (#2196 fix-round 3, CONSIDER)
///   This one arm is UCD-VERSION-COUPLED, and knowingly so: both crates
///   ship Unicode 16.0 tables, so a code point assigned in Unicode 17 or
///   later reads as `Cn` here and is silently deleted from an otherwise
///   legitimate reason. The direction is fail-safe (drop, never render
///   the unknown), and the blast radius is narrow — a message in a newly
///   encoded script loses those characters with no diagnostic. A crate
///   version bump picks up each new UCD edition; there is nothing to fix
///   in this function.
/// - `Zl`/`Zp` (Line/Paragraph Separator) — U+2028/U+2029, the same
///   row-forgery primitive as `\n` wearing a category `\n`'s own check
///   doesn't cover.
///
/// This is deliberately a category-level check, not a full "safe to
/// print" allowlist of individual characters — see [`is_stripped_for_display`]'s
/// doc for the two narrow, closed exceptions Unicode's category system
/// cannot express (a handful of characters assigned to ordinary
/// printable categories that nonetheless render as blank glyphs in every
/// mainstream terminal font).
fn is_denylisted_category(c: char) -> bool {
    use unicode_general_category::GeneralCategory as GC;
    matches!(
        unicode_general_category::get_general_category(c),
        GC::Control
            | GC::Format
            | GC::PrivateUse
            | GC::Surrogate
            | GC::Unassigned
            | GC::LineSeparator
            | GC::ParagraphSeparator
    )
}

/// (#2196 fix-round 2, MUST FIX A) A small, CLOSED set of characters
/// Unicode assigns to an ordinary printable general category (`Lo` —
/// Other Letter, or `So` — Other Symbol) — so [`is_denylisted_category`]
/// cannot reach them without also dropping thousands of legitimate
/// letters and symbols in the same category — that nonetheless render as
/// a BLANK glyph (no visible mark at all) in every mainstream terminal
/// font. This is the concrete case where a pure category allowlist
/// cannot work: Unicode's category system has no "renders blank"
/// property, so these five have to be named explicitly. They are
/// permanently fixed, single code points (not ranges, not blocks) tied
/// to old encoding-compatibility conventions Unicode will not extend:
///
/// - `U+115F` HANGUL CHOSEONG FILLER, `U+1160` HANGUL JUNGSEONG FILLER,
///   `U+3164` HANGUL FILLER, `U+FFA0` HALFWIDTH HANGUL FILLER — blank
///   placeholder jamo from the old Johab/compatibility Hangul encoding
///   model; category `Lo` because Unicode classifies all Hangul jamo as
///   letters, but the FILLER members of that set are defined to have no
///   visible glyph.
/// - `U+2800` BRAILLE PATTERN BLANK — the "all raised dots absent" cell
///   of the braille block; category `So` (Symbol) like every other
///   braille cell, but this one specific pattern is, by definition, the
///   blank one.
///
/// (#2196 fix-round 3, CONSIDER — investigated, NOT adopted) Four of the
/// five above (`U+115F`, `U+1160`, `U+3164`, `U+FFA0`) carry Unicode's
/// `Default_Ignorable_Code_Point` property, so filtering on that property
/// would subsume four fifths of this hand-maintained list and close the
/// remaining zero-width `Mn` survivors for free, leaving `U+2800` — a
/// real glyph that merely happens to render blank — as a one-character
/// exception. That is the better shape and it is worth revisiting.
/// **`unicode-general-category` 1.1.0 does not expose the property**
/// (verified: a case-insensitive grep for `default.?ignorable` across the
/// whole vendored crate returns nothing; its public surface is
/// `get_general_category` and the `GeneralCategory` enum, nothing else),
/// and neither does `unicode-width`. Hand-rolling the property's ~30
/// ranges here would be a SIXTH hand-maintained list — the exact thing
/// fix-round 2 inverted this module away from — so the five-element list
/// stands until a crate that ships the real property table is worth
/// adding as a third dependency.
fn is_blank_glyph_exception(c: char) -> bool {
    matches!(c, '\u{115F}' | '\u{1160}' | '\u{3164}' | '\u{FFA0}' | '\u{2800}')
}

/// (#2196 fix-round 2, MUST FIX A) Unicode Variation Selectors
/// (`U+FE00..=FE0F`) and Variation Selectors Supplement
/// (`U+E0100..=E01EF`) — two permanently closed, dedicated blocks.
/// Category `Mn` (Nonspacing Mark), the SAME category as a legitimate
/// combining accent (`café` = `e` + `U+0301`), so [`is_denylisted_category`]
/// can't reach them without also stripping real diacritics from
/// legitimate international prose. Left in, a variation selector is both
/// a zero-width character that can silently modify (or hide inside) the
/// glyph before it, and a documented steganographic channel (data
/// smuggled as a sequence of otherwise-invisible selectors) — a receiver
/// has no legitimate reason to send one in a rejection reason.
fn is_variation_selector(c: char) -> bool {
    matches!(c, '\u{FE00}'..='\u{FE0F}' | '\u{E0100}'..='\u{E01EF}')
}

/// (#2196 fix-round 2, MUST FIX A) The full "safe to print" test: `false`
/// for anything [`is_denylisted_category`], [`is_blank_glyph_exception`],
/// or [`is_variation_selector`] would drop. Named for what it decides,
/// not what it enumerates — the category check does the structural work;
/// the two named-exception checks cover what Unicode's category system
/// itself cannot express (see each function's doc).
fn is_stripped_for_display(c: char) -> bool {
    is_denylisted_category(c) || is_blank_glyph_exception(c) || is_variation_selector(c)
}

/// Strip control characters (the Unicode `Cc` category — C0 controls
/// including `\n`/`\r`, ESC, and DEL, plus the C1 range) from a
/// receiver-supplied reason string. This codebase already applies this
/// exact discipline twice: `sanitize_header_value` (above) allowlists
/// printable ASCII for HTTP header values specifically so CR/LF injection
/// is caught by the filter rather than a special case, and
/// `style::link` (`darkmux-types/src/style.rs`) strips control bytes from
/// a URL before embedding it in an OSC-8 escape, with the doc there
/// naming the goal precisely: making the corruption unreachable rather
/// than merely unlikely. A receiver's `results[].error` text is the same
/// class of untrusted input, and it rides straight into `flow status`,
/// `darkmux doctor`, and an `eprintln!` — all rendered to a real
/// terminal. Left unfiltered, an embedded `\n` forges extra status rows
/// indistinguishable from real ones, a `\r` overwrites the visible line,
/// and a raw ANSI escape (including a screen-clear) executes on whatever
/// terminal is watching. Printable non-ASCII text is kept (this is
/// prose an operator reads, not a wire-protocol value like a header), so
/// this deliberately doesn't reuse `sanitize_header_value`'s ASCII-only
/// allowlist.
///
/// (#2196 fix-round MUST FIX 1; inverted to an allowlist in fix-round 2,
/// MUST FIX A) `char::is_control()` alone only catches category `Cc` — it
/// let through the Trojan Source bidi-reordering set, invisible/zero-width
/// characters, and the Unicode line/paragraph separators on the first
/// pass, and four MORE format characters plus a family of blank-glyph
/// letters/symbols on the second. Rather than keep enumerating individual
/// characters a review happens to test, this keeps everything EXCEPT what
/// [`is_stripped_for_display`] names — see that function's doc for the
/// category-level check plus the two small closed exceptions Unicode's
/// category system can't express. The result is also passed through
/// [`collapse_whitespace_and_trim`], which defeats the exact-vocabulary
/// forgery of an INDENTED row — and only that. (#2196 fix-round 4) An
/// earlier revision of this line said it "actually defeats the
/// exact-vocabulary row forgery", full stop, which was the same
/// overstatement the whole premise rested on: all three surfaces that
/// render this text have FLUSH-LEFT rows the whitespace collapse cannot
/// protect (`flow status` has eight, `doctor` three, and every one of
/// this module's sixteen stderr rows). Those are closed at their render
/// sites instead, by [`rejection_reason_display_lines`] and its two
/// wrappers — see [`MAX_REJECTION_REASON_DISPLAY_WIDTH`]'s doc.
fn sanitize_reason_text(s: &str) -> String {
    let filtered: String = s.chars().filter(|c| !is_stripped_for_display(*c)).collect();
    strip_leading_zero_width(&collapse_whitespace_and_trim(&filtered))
}

/// (#2196 fix-round 3, CONSIDER) Drop any LEADING zero-column characters
/// left after [`collapse_whitespace_and_trim`].
///
/// That function trims WHITESPACE, and a combining mark is not
/// whitespace, so a reason beginning with one survives to the front of
/// the string — where [`format_rejection_reasons_for_display`] then puts
/// darkmux's own opening `"` immediately before it. A combining mark
/// applies to the character that PRECEDES it, so a reason starting with
/// U+0336 COMBINING LONG STROKE OVERLAY or U+20DD COMBINING ENCLOSING
/// CIRCLE renders the quote itself struck through or circled. That quote
/// is the single character carrying the attribution — the mark that tells
/// a reader "everything after this is the receiver's words, not
/// darkmux's" — so receiver-controlled text must not be able to deface
/// it. The marks are kept everywhere else in the string, where they are
/// ordinary prose (`café` decomposed is `e` + U+0301).
///
/// Zero-COLUMN rather than a category test on purpose: it is exactly the
/// characters that cost no cell of their own — and therefore land on the
/// preceding one — that can reach the quote. An empty result is fine;
/// [`extract_rejection_reasons`] drops an empty reason.
fn strip_leading_zero_width(s: &str) -> String {
    s.trim_start_matches(|c| display_width(c) == 0).to_string()
}

/// (#2196 fix-round MUST FIX 1 + MUST FIX 5) Collapse any run of Unicode
/// whitespace (regular space, NBSP, and every other `char::is_whitespace()`
/// code point — not just ASCII space) down to a single ASCII space, then
/// trim the ends.
///
/// This is part of the fix for the forged-row primitive, not just
/// cosmetic tidying: every darkmux-owned row in `flow status`'s per-rule
/// block (`status.rs`, e.g. `"      cursor-write failures: {} (recovered)"`,
/// `"      STALLED: ..."`, `"      last drainer heartbeat: ..."`) indents
/// with a RUN of six literal spaces. A receiver forging one of those rows
/// needs that exact run to land at column 0 of a wrapped continuation
/// line. Collapsing every whitespace run to one character means no
/// receiver-supplied text can ever contain six (or two, or any run
/// length ≥ 2) consecutive spaces after sanitization — an INDENTED row
/// cannot be constructed, independent of terminal width, wrap point, or
/// the length bound above.
///
/// (#2196 fix-round 3, MUST FIX G) That is the whole of what this
/// function buys, and an earlier revision of this doc overstated it as
/// the whole defense. It covers the INDENTED rows only. Eight rows in
/// `format_status_human` render FLUSH LEFT — the `flow status — {state}`
/// header, `Hooks`, `Disk`, `Redis`, `Schema`, `Warnings:`, `Failures:`,
/// and `Redis: not configured (set DARKMUX_REDIS_URL to enable)` — and
/// none of them needs leading whitespace to look genuine, so collapsing
/// whitespace runs does nothing for them at all. Those are closed at the
/// render site instead, by
/// [`format_rejection_reasons_as_indented_lines`], which takes the
/// wrapping away from the terminal so no continuation line exists to
/// carry receiver text to column 0.
///
/// Trimming also closes MUST FIX 5 for free: a reason that is empty, or
/// made of nothing but control/whitespace characters the filter above
/// just stripped, becomes `""` here, which [`extract_rejection_reasons`]
/// then drops entirely rather than let an empty-parens
/// `"... on the last delivery ()"` line ship.
fn collapse_whitespace_and_trim(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_was_space {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(c);
            last_was_space = false;
        }
    }
    out.trim().to_string()
}

/// (#2196 fix-round MUST FIX 1; replaced with the `unicode-width` crate
/// in fix-round 2, MUST FIX B) Rendered column width of `c` on a typical
/// terminal.
///
/// The first fix-round hand-rolled this as a handful of "wide script"
/// ranges (CJK, Hangul, fullwidth forms) plus "everything else is 1".
/// Reviewed again in fix-round 2, the verifier measured up to a 2x
/// overshoot from that approach — some from EXOTIC input (supplementary-
/// plane emoji, regional indicators), but two of the three misses needed
/// nothing exotic at all: CJK Compatibility Forms (`U+FE30..=FE6F`) and
/// Hangul Jamo Extended-B (`U+D7B0..=D7FF`) are ordinary, assigned
/// Unicode blocks the hand-rolled range list simply never named. A
/// hand-rolled table can only ever cover the blocks someone thought to
/// list; `unicode-width` (the crate the wider Rust ecosystem — `ripgrep`,
/// `bat`, `clap`'s wrapping — already relies on for exactly this
/// question) is generated from the real Unicode East-Asian-Width and
/// general-category data, so it's correct for every block including the
/// two above, and correct for the *next* Unicode version's new
/// assignments without this crate's own code changing. It also scores
/// combining marks and variation selectors at their real width (0) —
/// the hand-rolled version scored a combining accent as 1, silently
/// UNDER-counting available budget (safe, but needlessly conservative);
/// see this crate's doc comment for why this bound is defense in depth
/// rather than the whole defense in either direction.
///
/// Zero dependencies of its own (verified: `cargo tree` under this
/// crate shows no transitive deps) — see this crate's module-level dep
/// list for why that matters here.
fn display_width(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Rendered column width of a whole string (see [`display_width`]).
///
/// Exported so a consumer in another crate can assert the geometry of a
/// row it prints WITHOUT taking its own `unicode-width` dependency — the
/// width question has exactly one answer in this workspace, and a second
/// dependency edge would be a second place for it to drift.
pub fn display_columns(s: &str) -> usize {
    s.chars().map(display_width).sum()
}

/// Truncate already-SANITIZED text `s` to at most `budget` rendered
/// columns (see [`display_width`]) AND at most
/// `budget * `[`REJECTION_REASON_CHARS_PER_COLUMN`] characters
/// (#2196 fix-round 3, MUST FIX F — a column budget alone bounds nothing
/// a receiver cares about, because a combining mark is correctly scored
/// at ZERO columns; see that constant's doc for the measured 60 KB /
/// 20,000-character payload that reached five surfaces on a row of
/// nominal width 10). Both ceilings are enforced on the same walk, and
/// either one alone triggers a cut, preserving both a HEAD and a short
/// TAIL separated by a single `…` when a cut is needed, rather than
/// chopping the tail off outright (#2196 fix-round 2, MUST FIX C). For a
/// validation-style reason (`"...severity must be one of low, medium,
/// high, critical — got \"catastrophic\""`), a head-only cut loses
/// exactly the value that failed; a head+tail cut keeps both the
/// constraint AND the offending value — see
/// [`REJECTION_REASON_TAIL_RESERVE`]'s doc for the split. Returns `s`
/// unchanged when it already fits, so calling this again on
/// already-bounded text is a no-op — the idempotence
/// [`format_rejection_reasons_for_display`]'s doc relies on.
///
/// Walks `char_indices` from both ends — every candidate cut point is
/// therefore already a UTF-8 char boundary BY CONSTRUCTION, eliminating
/// the panic class a raw byte-offset slice invites (#2196 fix-round MUST
/// FIX 2: a naive `&s[..N]` on a byte offset that lands mid-character
/// panics with "byte index N is not a char boundary").
fn bound_reason_width(s: &str, budget: usize) -> String {
    // (#2196 fix-round 3, MUST FIX F) Both budgets are enforced: columns
    // AND characters. A width-only bound does not bound anything a
    // receiver cares about — see [`REJECTION_REASON_CHARS_PER_COLUMN`].
    let char_budget = budget.saturating_mul(REJECTION_REASON_CHARS_PER_COLUMN);
    let total_width: usize = s.chars().map(display_width).sum();
    let total_chars = s.chars().count();
    if total_width <= budget && total_chars <= char_budget {
        return s.to_string();
    }
    let ellipsis_width = display_width('…');
    if budget <= ellipsis_width {
        // A budget too small to hold even the ellipsis — never reached
        // with this module's own constants, but a defensive floor for
        // any future caller passing a tiny budget.
        return "…".to_string();
    }
    let tail_budget = REJECTION_REASON_TAIL_RESERVE.min((budget - ellipsis_width) / 2);
    let head_budget = budget - ellipsis_width - tail_budget;
    let head_char_budget = head_budget.saturating_mul(REJECTION_REASON_CHARS_PER_COLUMN);
    let tail_char_budget = tail_budget.saturating_mul(REJECTION_REASON_CHARS_PER_COLUMN);

    let mut head_end = s.len();
    let mut w = 0usize;
    for (n, (idx, c)) in s.char_indices().enumerate() {
        let cw = display_width(c);
        if w + cw > head_budget || n + 1 > head_char_budget {
            head_end = idx;
            break;
        }
        w += cw;
    }

    let mut tail_start = s.len();
    let mut w = 0usize;
    for (n, (idx, c)) in s.char_indices().rev().enumerate() {
        let cw = display_width(c);
        if w + cw > tail_budget || n + 1 > tail_char_budget {
            break;
        }
        w += cw;
        tail_start = idx;
    }

    if tail_start <= head_end {
        // The head and tail windows would overlap — a defensive floor,
        // not a live branch. (#2196 fix-round 3) The character ceiling
        // added above does NOT make it live, which is worth writing down
        // because it looks like it should: a first pass at this claimed
        // zero-width text would let the windows meet, and the test
        // written to pin that disproved it. Both arms are closed at the
        // shipped constants (head 93 cols / 372 chars, tail 24 cols / 96
        // chars, budget 118 cols / 472 chars):
        //   * Overlap implies `total_chars <= 372 + 96 = 468`, but a
        //     CHARACTER-triggered cut needs `total_chars > 472`. 468 < 472.
        //   * Overlap also implies the two windows cover the whole string
        //     (counting the overlap twice), so `total_width <= 93 + 24 =
        //     117`, but a WIDTH-triggered cut needs `total_width > 118`.
        // No input can satisfy either, so no test is written against this
        // arm — a test that cannot fail is worse than none. Kept anyway
        // so a future constant change degrades to a correct head-only cut
        // instead of re-emitting the overlap region twice.
        format!("{}…", &s[..head_end])
    } else {
        format!("{}…{}", &s[..head_end], &s[tail_start..])
    }
}

/// Sanitize `s` (see [`sanitize_reason_text`]) and then bound it to
/// [`REJECTION_REASON_RAW_BUDGET`] rendered columns (see
/// [`bound_reason_width`]). Sanitizing before bounding (not after)
/// matters for two reasons: the bound is a promise about what actually
/// rides downstream, and cutting before filtering could remove a
/// multi-byte sequence at the wrong point relative to the stripped
/// characters.
fn truncate_reason(s: &str) -> String {
    bound_reason_width(&sanitize_reason_text(s), REJECTION_REASON_RAW_BUDGET)
}

/// Extract up to [`MAX_REJECTION_REASONS`] per-record rejection reason
/// strings from a receiver's 2xx JSON response body (#2196). The local
/// tracker's contract shape is `results: [{"ok": false, "error": "..."}]`
/// alongside the record's own `rejected` count; this pulls the `error`
/// text from every entry explicitly marked `"ok": false`.
///
/// Lenient-on-read, deliberately: a `results` entry missing `ok`/`error`,
/// an `error` that isn't a string, a non-array `results`, or no `results`
/// key at all all yield no reasons for that entry rather than an error —
/// darkmux describes what the receiver told it and never invents detail
/// the receiver didn't provide (this project's "describes, never
/// adjudicates" stance applies to absence of data too).
///
/// (#2196 fix-round MUST FIX 5) A reason that sanitizes to empty (an
/// empty string, or one made of nothing but control/whitespace
/// characters `sanitize_reason_text` just stripped) is dropped here
/// rather than kept as `""` — an empty-string "reason" is not a reason;
/// letting it through produced `"... on the last delivery ()"` in
/// `doctor` and a bare, contentless `"last rejection reason(s): "` in
/// `flow status`, leaving the operator unable to tell whether the
/// receiver gave no reason at all or darkmux lost one it was given.
///
/// (#2196 fix-round 2, CONSIDER) The empty-after-sanitize filter runs
/// BEFORE [`MAX_REJECTION_REASONS`]'s `.take` — not after, as the first
/// fix-round had it. A body whose first entries are blank/all-control
/// `error` strings ahead of a real one used to consume the cap on
/// nothing and then drop every one of them at the trailing filter,
/// yielding `reasons: []` — count disclosed, reason silently gone. The
/// width bound ([`bound_reason_width`]) is applied AFTER the take, since
/// it's a per-reason concern that doesn't affect which entries survive.
fn extract_rejection_reasons(body: &serde_json::Value) -> Vec<String> {
    body.get("results")
        .and_then(serde_json::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter(|e| e.get("ok").and_then(serde_json::Value::as_bool) == Some(false))
                .filter_map(|e| e.get("error").and_then(serde_json::Value::as_str))
                .map(sanitize_reason_text)
                .filter(|s| !s.is_empty())
                .take(MAX_REJECTION_REASONS)
                .map(|s| bound_reason_width(&s, REJECTION_REASON_RAW_BUDGET))
                .collect()
        })
        .unwrap_or_default()
}

/// (#2196 fix-round MUST FIX 1 + CONSIDER) Render receiver-supplied
/// rejection reason(s) for a human-facing terminal surface (`flow
/// status`, `darkmux doctor`, and the delivery `eprintln!` at the
/// `DeliveryOutcome::Success` call site) — each reason double-quoted and
/// semicolon-joined, and re-sanitized/re-bounded via [`truncate_reason`]
/// rather than trusting the caller already did.
///
/// Two independent reasons this exists as its own function instead of a
/// bare `.join("; ")` at each call site:
///
/// - **Attribution.** Quoting makes the text unambiguously the
///   RECEIVER'S words, never darkmux's own voice — a reader scanning for
///   a closing quote mark can tell where receiver-controlled text ends,
///   which a bare inline string cannot offer. Internal `"` and `\` are
///   backslash-escaped (the familiar quoted-string convention) so a
///   reason that itself contains a literal `"` can never look like the
///   closing quote and spill unquoted text onto the row.
/// - **Defense in depth.** Sanitization today lives only at the
///   producer (`truncate_reason`, called from `extract_rejection_reasons`
///   at write time). A `.last` sidecar already on disk from before this
///   fix (this fix's own first commit wrote reasons unsanitized), or any
///   future producer that forgets to call `truncate_reason`, would
///   otherwise render raw. Calling `truncate_reason` again here is one
///   call and closes that window — sanitization and the display-width
///   bound are both idempotent, so re-applying them to already-clean
///   text is a no-op.
///
/// (#2196 fix-round 2, MUST FIX B/D) The escaping happens AFTER
/// `truncate_reason`'s width bound, so a reason made mostly of `"`/`\`
/// characters grows past the bound here: each escaped character costs
/// TWO rendered columns instead of one, and the bound never accounted
/// for that — a stored reason at exactly the raw budget in quote
/// characters renders noticeably wider once escaped, restoring the wrap
/// precondition the width bound exists to prevent (this is the padding
/// the fix-round-2 verifier's own proof used). Closing this needs a
/// SECOND bounding pass on the escaped text itself: `bound_reason_width`
/// is called again here, on the (by now plain-ASCII, since sanitization
/// already ran) escaped string, before the wrapping quotes go on. This
/// composes with the idempotence note above rather than breaking it —
/// the common case (no `"`/`\` in the reason) never triggers a second
/// cut at all, since the escaped text is byte-identical to the
/// already-bounded input.
pub fn format_rejection_reasons_for_display(reasons: &[String]) -> String {
    reasons
        .iter()
        .map(|r| {
            let escaped = truncate_reason(r).replace('\\', "\\\\").replace('"', "\\\"");
            let bounded = bound_reason_width(&escaped, REJECTION_REASON_RAW_BUDGET);
            format!("\"{bounded}\"")
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// (#2196 fix-round 3, MUST FIX G) The narrowest terminal width the
/// rejection-reason rows are guaranteed against — no line
/// [`format_rejection_reasons_as_indented_lines`] emits ever reaches this
/// column, so no terminal at least this wide has anything to wrap.
///
/// A "narrowest supported width" has to be a NUMBER somewhere, because
/// the guarantee is only as good as the assumption it names. 60 is the
/// narrowest width anyone actually reviews `flow status` at; a terminal
/// narrower than that wraps darkmux's own rows too, so the forged row
/// stops being distinguishable from ordinary damage.
pub const REJECTION_REASON_MIN_TERMINAL_WIDTH: usize = 60;

/// (#2196 fix-round 3, MUST FIX G) Columns of indent every line
/// [`format_rejection_reasons_as_indented_lines`] emits carries. Deeper
/// than the six-space indent of the per-rule block it sits inside, so a
/// reason line reads as subordinate to the `last rejection reason(s):`
/// label above it.
pub(crate) const REJECTION_REASON_LINE_INDENT: usize = 8;

/// (#2196 fix-round 3, MUST FIX G) Render receiver-supplied rejection
/// reasons as a list of COMPLETE, ALREADY-INDENTED output lines, none of
/// which can reach column [`REJECTION_REASON_MIN_TERMINAL_WIDTH`].
///
/// This exists because the anti-forgery premise the earlier rounds relied
/// on is FALSE, and was stated as an absolute. That premise: "every
/// darkmux status row indents with a run of spaces, and
/// [`collapse_whitespace_and_trim`] guarantees receiver text can never
/// contain such a run, so a wrapped continuation line can never be
/// mistaken for a darkmux row." Enumerating every `writeln!` in
/// `status::format_status_human` shows twenty indented rows and EIGHT
/// FLUSH-LEFT ones: the `flow status — {state}` header, `Hooks`, `Disk`,
/// `Redis`, `Schema`, `Warnings:`, `Failures:`, and `Redis: not
/// configured (set DARKMUX_REDIS_URL to enable)`. None of the eight needs
/// leading whitespace to be plausible, so the whitespace collapse offers
/// them ZERO protection — proven with 46 filler characters, one space,
/// and the verbatim row text: no whitespace run, no blank glyph, no
/// escape expansion, 100 columns against a 118-column budget, surviving
/// truncation whole, landing at column 0 of the wrapped continuation with
/// the genuine identical row four lines above. `Warnings:` and
/// `Failures:` are the two that matter — they change how an operator
/// reads everything printed below them.
///
/// The fix is at the RENDER site, not in the sanitizer, because the
/// sanitizer cannot see the thing that makes the attack work: the row's
/// own wrap point. darkmux takes the wrapping back from the terminal —
/// every line is emitted whole, pre-indented, and short enough that no
/// terminal at the supported width has a continuation to produce. With no
/// continuation line, there is no column-0 receiver text, and the
/// forgery is unreachable by construction rather than by vocabulary —
/// which also means it holds for the flush-left rows the collapse never
/// covered, and for any row a future revision adds.
///
/// Wrapping prefers a space boundary and falls back to a hard character
/// split for a single token wider than the budget (a hostile receiver's
/// unbroken run, or a legitimate long identifier), so a real multi-word
/// reason stays readable while an adversarial one is still bounded.
pub fn format_rejection_reasons_as_indented_lines(reasons: &[String]) -> Vec<String> {
    let indent = " ".repeat(REJECTION_REASON_LINE_INDENT);
    rejection_reason_display_lines(reasons, REJECTION_REASON_LINE_INDENT)
        .into_iter()
        .map(|line| format!("{indent}{line}"))
        .collect()
}

/// (#2196 fix-round 4) Columns of prefix `darkmux doctor` puts in front
/// of a hint line — `"        \u{2192} "`, eight spaces plus the arrow plus a
/// space, matching `render_check_block`'s own `HINT_HEAD`. A hint
/// CONTINUATION is padded to the same width, so every line of a hint is
/// indented by exactly this much.
pub const REJECTION_REASON_HINT_INDENT: usize = 10;

/// Never wrap receiver text below this many columns, however deep the
/// caller's prefix. A budget near zero would emit one character per line
/// — technically bounded, useless to read — so a caller whose prefix
/// leaves no room gets a line that is wider than the minimum terminal
/// width instead of a column of confetti. No caller in this workspace is
/// anywhere near it (the deepest prefix is doctor's 10), and a future one
/// that is has a layout problem this function cannot fix for it.
const REJECTION_REASON_MIN_CONTENT_BUDGET: usize = 20;

/// (#2196 fix-round 4) The receiver's rejection reasons, rendered and
/// wrapped to lines that fit UNDER [`REJECTION_REASON_MIN_TERMINAL_WIDTH`]
/// once the caller's own `prefix_columns` of indentation go in front of
/// them. Returned WITHOUT that prefix, because the two consumers attach
/// it differently: `flow status` prepends spaces itself
/// ([`format_rejection_reasons_as_indented_lines`]), while `doctor` hands
/// the lines to its own hint renderer, which supplies `"        \u{2192} "`
/// for the first and ten spaces for the rest.
///
/// Taking the prefix as a parameter is what makes the guarantee travel.
/// The budget is a property of the FINISHED line, not of this function,
/// so a helper that hard-coded one indent would silently over-run for any
/// caller that indents deeper — and the whole defense is that no printed
/// line reaches the minimum terminal width.
///
/// The `+ 1` keeps a full line one column SHORT of that width: terminals
/// disagree about whether a line that exactly fills the last column wraps
/// immediately or defers, and the guarantee should not depend on which
/// behavior the operator's terminal happens to have.
pub fn rejection_reason_display_lines(reasons: &[String], prefix_columns: usize) -> Vec<String> {
    let content_budget = REJECTION_REASON_MIN_TERMINAL_WIDTH
        .saturating_sub(prefix_columns + 1)
        .max(REJECTION_REASON_MIN_CONTENT_BUDGET);
    wrap_to_display_width(&format_rejection_reasons_for_display(reasons), content_budget)
}

/// Greedy word wrap of `s` to at most `budget` rendered columns per line
/// (see [`display_width`]), hard-splitting any single token wider than
/// `budget`. Input is expected to be already sanitized, so the only
/// whitespace it can contain is the single ASCII spaces
/// [`collapse_whitespace_and_trim`] leaves behind plus the `"; "` joiner
/// [`format_rejection_reasons_for_display`] adds.
fn wrap_to_display_width(s: &str, budget: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for word in s.split(' ') {
        let word_w: usize = word.chars().map(display_width).sum();
        if !cur.is_empty() && cur_w + 1 + word_w > budget {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        if word_w <= budget {
            if !cur.is_empty() {
                cur.push(' ');
                cur_w += 1;
            }
            cur.push_str(word);
            cur_w += word_w;
        } else {
            for c in word.chars() {
                let cw = display_width(c);
                if cur_w + cw > budget && !cur.is_empty() {
                    lines.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                cur.push(c);
                cur_w += cw;
            }
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// (#2196 fix-round 4, MUST FIX G at the stderr surface) The lines the
/// delivery path writes to stderr when a receiver accepts the request but
/// reports rejecting records inside it.
///
/// A `Vec` of whole lines rather than one interpolated string, for the
/// same reason `flow status` stopped rendering the reason inline: the
/// reason used to sit in the middle of a single line that no code
/// bounded, so the terminal wrapped it and the continuation began at
/// column 0.
///
/// This surface's row inventory is the shortest of the three, and it
/// makes the fix stronger here than anywhere else: **every** line this
/// module writes to stderr is flush-left and begins with the literal
/// `flow::HookSink: ` — 16 sites, ZERO indented ones. So an indented line
/// cannot be mistaken for a darkmux row no matter what it says, and the
/// defense needs no vocabulary list at all. It also needed the fix most:
/// unlike `doctor`, nothing here caps the line's width, so the inline
/// form wrapped on EVERY terminal rather than only on one narrower than
/// the renderer assumed.
///
/// Split out as a pure function so the geometry is testable without
/// capturing a process's stderr — the behavior under test is the shape of
/// the lines, not the IO.
fn receiver_rejection_stderr_lines(url: &str, n: u64, total: u64, reasons: &[String]) -> Vec<String> {
    let mut lines = vec![format!(
        "flow::HookSink: receiver at {url} accepted the request but rejected {n} record(s) inside it \
         ({total} so far) — see hook.fired.receiver_rejected"
    )];
    // Empty in, nothing out: `format_rejection_reasons_as_indented_lines`
    // yields no lines for no reasons, so a rejection the receiver gave no
    // reason for prints exactly the one header line it always did.
    lines.extend(format_rejection_reasons_as_indented_lines(reasons));
    lines
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
    let mut req = agent.post(url).set("Content-Type", "application/json");
    // (#2183) `attribution_headers: false` drops every `X-Darkmux-*`
    // header — a SaaS receiver may reject unknown headers.
    if headers.attribution {
        req = req
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
    }
    // (#2183) This rule's configured `headers` — literal values pass
    // through, Keychain-resolved values arrive here already resolved (and
    // sanitized); an unresolved Keychain reference is simply absent from
    // `extra`. An operator-named header always wins over an attribution
    // header of the SAME name (`.set()` overwrites, and `extra` is
    // applied last) — deliberate: the operator's own `headers` entry is a
    // more specific instruction than the default attribution set.
    for (name, value) in &headers.extra {
        req = req.set(name, value);
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
                let parsed_body = serde_json::from_str::<serde_json::Value>(&buf).ok();
                let receiver_rejected =
                    parsed_body.as_ref().and_then(|v| v.get("rejected").and_then(serde_json::Value::as_u64));
                // (#2196) A body that isn't JSON at all, or is JSON with
                // no `results` array, yields no reasons — darkmux
                // describes what the receiver told it, never invents
                // detail the receiver didn't provide.
                let receiver_rejected_reasons =
                    parsed_body.as_ref().map(extract_rejection_reasons).unwrap_or_default();
                DeliveryOutcome::Success { receiver_rejected, receiver_rejected_reasons }
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

/// (#2183) Write `bytes` to `path` with mode `0o600` on POSIX (owner
/// read/write only — the #2156 lesson: outbox files are world-readable
/// today, and a `file`-transport dump can carry a literal header value or
/// sensitive record content, so this one starts out right). Non-POSIX:
/// plain write, no mode (matches this crate's existing POSIX-only carve
/// -outs, e.g. `AuditFileSink`).
fn write_owner_only_file(path: &Path, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("opening {} for owner-only write", path.display()))?;
        file.write_all(bytes).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
    }
}

/// (#2183) The `file` transport — the no-network testing tier. Writes
/// ONE JSON file per delivery into `dir`: `{delivery_id,
/// target_would_be, headers, body}`. `target_would_be` is `rule.url`'s
/// synthetic `file://<dir>` form's ORIGINAL intent inverted — callers
/// pass the rule's true configured `file` directory display string, not
/// a URL, since there IS no URL for this transport (naming kept for
/// wire-shape clarity, matching what an operator reads in the dump).
/// `headers` renders the SAME redacted view `doctor`/flow records use —
/// a Keychain-resolved header value NEVER appears here, even though this
/// path never touches the network at all.
fn write_file_delivery(dir: &Path, delivery_id: &str, target_would_be: &str, headers: &DeliveryHeaders, body: &str) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating file-transport dir {}", dir.display()))?;
    let mut headers_obj = serde_json::Map::new();
    if headers.attribution {
        headers_obj.insert("X-Darkmux-Delivery".to_string(), serde_json::Value::String(headers.delivery_id.clone()));
        headers_obj.insert("X-Darkmux-Event".to_string(), serde_json::Value::String(headers.event.clone()));
        headers_obj.insert("X-Darkmux-Sender".to_string(), serde_json::Value::String(headers.sender.clone()));
        headers_obj
            .insert("X-Darkmux-Timestamp".to_string(), serde_json::Value::String(headers.timestamp_ms.to_string()));
        if let Some(m) = &headers.machine_id {
            headers_obj.insert("X-Darkmux-Machine-Id".to_string(), serde_json::Value::String(m.clone()));
        }
        if let Some(u) = &headers.machine_uid {
            headers_obj.insert("X-Darkmux-Machine-Uid".to_string(), serde_json::Value::String(u.clone()));
        }
        if let Some(sig) = &headers.signature {
            headers_obj.insert("X-Darkmux-Signature".to_string(), serde_json::Value::String(sig.clone()));
        }
    }
    for (name, value) in &headers.extra_redacted {
        headers_obj.insert(name.clone(), serde_json::Value::String(value.clone()));
    }
    let dump = serde_json::json!({
        "delivery_id": delivery_id,
        "target_would_be": target_would_be,
        "headers": headers_obj,
        "body": body,
    });
    let bytes = serde_json::to_vec_pretty(&dump).context("serializing file-transport dump")?;
    let path = dir.join(format!("{delivery_id}.json"));
    write_owner_only_file(&path, &bytes)
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
        let headers = build_delivery_headers(&line, parsed.as_ref(), &delivery_id, None, &[], true);
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
    /// (security review, 2026-08-31, #2183) The count of this rule's
    /// currently-orphaned (timed-out, still-running) transform-evaluation
    /// threads — see `hook_transform::apply_transform`'s doc for why a
    /// leaked thread is not harmless and must be bounded. Shared (via
    /// `Arc`, cloned into every `apply_transform` call for this rule) so
    /// an orphaned thread can decrement it itself once it eventually
    /// finishes, regardless of how long after this `RuleRuntime` moved on.
    orphaned_transforms: Arc<AtomicU32>,
    /// (security review round 2, 2026-08-31) Consecutive `Busy` outcomes
    /// for this rule — reset to 0 on any OTHER outcome (a successful
    /// delivery, a transform error, a normal retryable failure). Past
    /// `MAX_CONSECUTIVE_BUSY_BEFORE_STALL`, the rule is promoted into the
    /// existing `stalled` state: "the orphan cap has been full for a
    /// while" is not a transient blip the way one 5xx is — it means every
    /// timed-out evaluation for this rule so far has NEVER finished (the
    /// canonical case: `def rec: rec; rec`, which by construction never
    /// returns), so waiting for "an orphan to finish and decrement" is
    /// not a plan, it is a hope. `doctor`/`flow status` surface `stalled`
    /// today; this reuses that surface rather than inventing a second one.
    consecutive_busy: AtomicU32,
    /// (security review round 2) Rate-limits the `hook.failed` emitted
    /// while a rule is `Busy` — same pattern as `last_drop_warning`.
    last_busy_warning: Mutex<Option<Instant>>,
}

/// (security review round 2, 2026-08-31) After this many CONSECUTIVE
/// `Busy` outcomes, a rule is promoted into `stalled` — see
/// `RuleRuntime::consecutive_busy`'s doc.
const MAX_CONSECUTIVE_BUSY_BEFORE_STALL: u32 = 3;

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
    emit_hook_record_with(report_sink, success, rt, delivered_line, attempt, error, None, &[], delivery_id)
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
    receiver_rejected_reasons: &[String],
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
    let rejected_count = receiver_rejected.filter(|n| *n > 0);
    if let Some(n) = rejected_count {
        payload["receiver_rejected"] = serde_json::Value::from(n);
    }
    // (#2196) The receiver's own `results[].error` text — WHY, not just
    // how many. Rides alongside `receiver_rejected` only (never on a
    // clean delivery, and never invented when the receiver's body
    // carried no such detail); an empty array is never emitted so an
    // older reader that doesn't know this key sees no difference from
    // before.
    if !receiver_rejected_reasons.is_empty() {
        payload["receiver_rejected_reasons"] = serde_json::Value::from(receiver_rejected_reasons.to_vec());
    }

    let action = if success { "hook.fired" } else { "hook.failed" };
    // (#2273) Three outcomes, not two: transport FAILURE (`Error`),
    // transport success with a clean receiver accept (`Info`), and
    // transport success where the receiver's own response body reported
    // it rejected some or all of the delivered content (`Warn`). Folding
    // the third into the second — as this used to do by keying only on
    // the transport-level `success` bool — logged a receiver rejection at
    // the SAME level as a routine delivery, so it silently scrolled past
    // anyone watching the stream for problems. The record is still
    // consumed either way (see the `DeliveryOutcome::Success` call site's
    // doc for why retry would be wrong here), so this is the loud half of
    // that decision.
    let level = if !success {
        Level::Error
    } else if rejected_count.is_some() {
        Level::Warn
    } else {
        Level::Info
    };
    let rec = FlowRecord {
        ts: schema::ts_utc_now(),
        level,
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

/// (#2183) Emit `hook.dry_run` (Info) for a successful `file`-transport
/// write — the `file` transport's counterpart to `emit_hook_record`'s
/// `hook.fired`, distinct action so a consumer never confuses "actually
/// delivered" with "wrote a would-have-been dump to disk."
fn emit_dry_run_record(report_sink: &dyn FlowSink, rt: &RuleRuntime, delivered_line: &str, delivery_id: &str, dump_path: &Path) {
    let parsed: Option<serde_json::Value> = serde_json::from_str(delivered_line).ok();
    let action_val = parsed.as_ref().and_then(|v| v.get("action")).and_then(|v| v.as_str()).map(str::to_string);
    let payload = serde_json::json!({
        "rule_index": rt.rule.index,
        "delivered_action": action_val.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null),
        "delivery_id": delivery_id,
        "dump_path": dump_path.display().to_string(),
    });
    let rec = FlowRecord {
        ts: schema::ts_utc_now(),
        level: Level::Info,
        category: Category::Machinery,
        tier: Tier::Local,
        stage: Stage::Ship,
        action: "hook.dry_run".to_string(),
        handle: rt.rule.url.clone(),
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
    if let Err(e) = crate::record_to(report_sink, rec) {
        eprintln!("flow::HookSink: failed to emit hook.dry_run: {e:#}");
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

/// (security review round 2, 2026-08-31) Emit a rate-limited `hook.failed`
/// while a rule is `Busy` — its per-rule orphan cap
/// (`hook_transform::MAX_ORPHANED_TRANSFORM_THREADS_PER_RULE`) is full, so
/// deliveries are backed off rather than attempted. Same rate-limit shape
/// as `maybe_warn_dropped`: this is an ONGOING condition, not a one-shot
/// failure, so it must not turn into one `hook.failed` per poll cycle —
/// but it must not be SILENT either (the bug this fixes: the drainer used
/// to back off on `Busy` with no status write and no emitted record at
/// all, so a permanently backlogged rule read identically to a quiet,
/// healthy one).
fn maybe_warn_busy(rt: &RuleRuntime, report_sink: &dyn FlowSink, orphan_count: u32) {
    const WARNING_INTERVAL: Duration = Duration::from_secs(60);
    let now = Instant::now();
    {
        let mut last = rt.last_busy_warning.lock().unwrap();
        if let Some(prev) = *last {
            if now.duration_since(prev) < WARNING_INTERVAL {
                return;
            }
        }
        *last = Some(now);
    }
    let reason = format!("transform backlogged: {orphan_count} timed-out evaluation(s) still running");
    write_last_status(rt, false, Some(&reason));
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
            "orphaned_transforms": orphan_count,
        })),
        work_id: None,
        attempt: None,
    };
    if let Err(e) = crate::record_to(report_sink, rec) {
        eprintln!("flow::HookSink: failed to emit hook.failed (busy warning): {e:#}");
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
                // (security review round 2, 2026-08-31) A rule can also
                // be `stalled` because its transform orphan cap has been
                // full for `MAX_CONSECUTIVE_BUSY_BEFORE_STALL` cycles
                // running (see `RuleRuntime::consecutive_busy`'s doc) —
                // NOT a cursor-write problem, so the cursor-writability
                // probe below would trivially succeed and incorrectly
                // clear the stall while the backlog is still real. Skip
                // the probe entirely (stay backed off, try again next
                // cycle) until the orphan count itself drops back under
                // the cap.
                if rt.orphaned_transforms.load(Ordering::Acquire)
                    >= crate::hook_transform::MAX_ORPHANED_TRANSFORM_THREADS_PER_RULE
                {
                    apply_backoff(rt);
                    continue;
                }
                let probe_cursor = read_cursor(&rt.rule.cursor_path);
                if write_cursor(&rt.rule.cursor_path, probe_cursor).is_err() {
                    apply_backoff(rt);
                    continue;
                }
                rt.stalled.store(false, Ordering::Release);
                rt.cursor_write_failures.store(0, Ordering::Release);
                rt.consecutive_busy.store(0, Ordering::Release);
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
            // (#2183) Apply this rule's `transform`, if configured —
            // evaluated at DELIVERY time (never enqueue), so the outbox
            // keeps raw records and a corrected adapter re-drains the
            // SAME lines (see hook_transform's module doc). A jq error,
            // timeout, oversize output, or a non-object/non-string
            // result is TERMINAL: quarantine + `hook.failed`, never
            // `RetryableFailure` (the #2178 wedge lesson — a
            // construction-class error must route to the give-up path,
            // not retry an unfixable line forever).
            let body: String = if let Some(adapter) = &rt.rule.transform {
                match crate::hook_transform::apply_transform(
                    &adapter.source,
                    &line,
                    Duration::from_millis(darkmux_types::config_access::hooks_jq_timeout_ms()),
                    darkmux_types::config_access::hooks_jq_max_output_bytes() as usize,
                    &rt.orphaned_transforms,
                ) {
                    crate::hook_transform::TransformOutcome::Body(b) => {
                        // A non-Busy outcome — the backlog (if there was
                        // one) is not blocking THIS attempt, so the
                        // consecutive-Busy count that would promote the
                        // rule to `stalled` resets.
                        rt.consecutive_busy.store(0, Ordering::Release);
                        b
                    }
                    crate::hook_transform::TransformOutcome::Error(e) => {
                        rt.consecutive_busy.store(0, Ordering::Release);
                        quarantine_line(&rt.rule.outbox_path, &line);
                        advance_cursor(rt, new_cursor);
                        let reason = format!("transform `{}` failed: {e}", adapter.path.display());
                        write_last_status(rt, false, Some(&reason));
                        emit_hook_record(report_sink.as_ref(), false, rt, &line, 1, Some(&reason), &delivery_id);
                        continue;
                    }
                    // (security review round 2, 2026-08-31) NOT a claim
                    // about this line/adapter — this rule already has
                    // too many timed-out transform threads still running
                    // in the background (see hook_transform's module
                    // doc). Back off and retry the SAME undelivered line
                    // later (never quarantine — an otherwise-fine
                    // adapter must not be permanently disabled because
                    // the process is momentarily backed up). Unlike the
                    // first cut, this is no longer SILENT: the status is
                    // written and a rate-limited `hook.failed` names the
                    // reason, and — since the canonical orphan
                    // (`def rec: rec; rec`) never finishes, so the
                    // backlog can persist for the life of the process —
                    // `MAX_CONSECUTIVE_BUSY_BEFORE_STALL` consecutive
                    // `Busy` outcomes promote the rule into the existing
                    // `stalled` state, so `doctor` surfaces it as
                    // ongoing, not transient.
                    crate::hook_transform::TransformOutcome::Busy => {
                        let orphan_count = rt.orphaned_transforms.load(Ordering::Acquire);
                        let busy_count = rt.consecutive_busy.fetch_add(1, Ordering::AcqRel) + 1;
                        if busy_count >= MAX_CONSECUTIVE_BUSY_BEFORE_STALL && !rt.stalled.swap(true, Ordering::AcqRel) {
                            // Just transitioned into `stalled` — persist
                            // it to the `.last` sidecar IMMEDIATELY,
                            // never waiting on `maybe_warn_busy`'s own
                            // rate limit: a cross-process-visible STATE
                            // TRANSITION (what `doctor`/`flow status`
                            // read) must never lag a full warning
                            // interval behind the in-memory flag.
                            write_cursor_write_status(
                                &rt.rule.last_status_path,
                                rt.cursor_write_failures.load(Ordering::Acquire),
                                true,
                            );
                        }
                        maybe_warn_busy(rt, report_sink.as_ref(), orphan_count);
                        apply_backoff(rt);
                        continue;
                    }
                }
            } else {
                line.clone()
            };
            // (#2183) Sign/derive-metadata-headers-from `parsed` (the TRUE
            // record — so `hook.fired`'s attribution stays tied to what
            // actually happened), but pass `&body` as the signed payload:
            // the signature must cover what's ACTUALLY on the wire, not
            // the pre-transform record, or a receiver verifying against
            // the body it received would never match.
            let headers = build_delivery_headers(
                &body,
                Some(&parsed),
                &delivery_id,
                rt.rule.signing_secret.as_ref(),
                &rt.rule.headers,
                rt.rule.attribution_headers,
            );
            // (#2183) The `file` transport — no network, ever. Mutually
            // exclusive with `http` at load time (`resolve_one_rule`), so
            // exactly one of `file_dir`/a real `try_post` call applies.
            if let Some(dir) = &rt.rule.file_dir {
                match write_file_delivery(dir, &delivery_id, &rt.rule.url, &headers, &body) {
                    Ok(()) => {
                        advance_cursor(rt, new_cursor);
                        write_last_status(rt, true, None);
                        let dump_path = dir.join(format!("{delivery_id}.json"));
                        emit_dry_run_record(report_sink.as_ref(), rt, &line, &delivery_id, &dump_path);
                    }
                    Err(e) => {
                        // A write failure (unwritable dir, full disk) is
                        // the `file` transport's counterpart of a
                        // transient network failure — back off and retry,
                        // never quarantine (the RECORD is fine; the
                        // destination is momentarily unwritable).
                        eprintln!("flow::HookSink: file-transport write to {} failed: {e:#}", dir.display());
                        apply_backoff(rt);
                    }
                }
                continue;
            }
            match try_post(&rt.rule.url, &body, &headers) {
                DeliveryOutcome::Success { receiver_rejected, receiver_rejected_reasons } => {
                    let attempt = {
                        let mut c = rt.attempt_count.lock().unwrap();
                        *c += 1;
                        *c
                    };
                    advance_cursor(rt, new_cursor);
                    // (#2273) Persist the rejection count durably (not
                    // just onto the `hook.fired` flow record) so a later
                    // `darkmux doctor` invocation can still see it after
                    // the flow event itself has scrolled past. TWO
                    // writes, deliberately: the `.last` sidecar carries
                    // the LAST delivery's count as context (and is
                    // wholesale-overwritten by the next delivery, clean
                    // or not), while the `<key>.rejected` counter sidecar
                    // ACCUMULATES and is never reset — that one is what
                    // doctor / `flow status` key on, so one clean
                    // delivery seconds later cannot erase the signal.
                    //
                    // (#2196) `reasons_for_status` rides the SAME gate as
                    // the count (`rejected_for_status`) rather than an
                    // independent one keyed on `receiver_rejected_reasons`
                    // being non-empty — a receiver's `results[]` entries
                    // without a top-level `rejected` count is not a shape
                    // this contract defines, so reasons are only surfaced
                    // as context for a rejection the count already named.
                    let rejected_for_status = receiver_rejected.filter(|n| *n > 0);
                    let reasons_for_status: Vec<String> =
                        if rejected_for_status.is_some() { receiver_rejected_reasons } else { Vec::new() };
                    write_last_status_full(rt, true, None, rejected_for_status, reasons_for_status.clone());
                    if let Some(n) = rejected_for_status {
                        let total =
                            add_receiver_rejected(&rt.rule.outbox_path, &rt.rule.receiver_rejected_path, n);
                        // (#2196 fix-round MUST FIX 1) Quoted +
                        // re-sanitized; (#2196 fix-round 4, MUST FIX G)
                        // and on its own INDENTED line(s) rather than
                        // inline, because this string reaches a real
                        // terminal, the same as `flow status` and
                        // `doctor` — see `receiver_rejection_stderr_lines`.
                        for line in receiver_rejection_stderr_lines(
                            &rt.rule.url,
                            n,
                            total,
                            &reasons_for_status,
                        ) {
                            eprintln!("{line}");
                        }
                    }
                    emit_hook_record_with(
                        report_sink.as_ref(),
                        true,
                        rt,
                        &line,
                        attempt,
                        None,
                        receiver_rejected,
                        &reasons_for_status,
                        &delivery_id,
                    );
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
        Self::new_with_max_outbox_mb_override(rules, outbox_dir, report_sink, None)
    }

    /// (#2643) Test-only: inject the outbox size cap directly instead of
    /// through `DARKMUX_HOOKS_MAX_OUTBOX_MB`. The two capping tests below
    /// used to mutate that env var — a process-global config knob EVERY
    /// `HookSink::new` call in this file's whole test suite reads once at
    /// construction, regardless of which test constructed it — which
    /// raced every other concurrently-running test in this file that
    /// builds a sink. Unlike the rule-index collision `hook_signing_secret`
    /// had (fixed by giving the signing test its own index), there is no
    /// spare dimension to move THIS collision off of: it's one scalar
    /// knob, read the same way by every construction. So the two tests
    /// that need a non-default cap now call this instead of mutating
    /// global state at all — nothing races because nothing is mutated.
    #[cfg(test)]
    fn new_for_test_with_max_outbox_mb(
        rules: &[HookRule],
        outbox_dir: PathBuf,
        report_sink: Arc<dyn FlowSink>,
        max_outbox_mb: u64,
    ) -> Result<Self> {
        Self::new_with_max_outbox_mb_override(rules, outbox_dir, report_sink, Some(max_outbox_mb))
    }

    fn new_with_max_outbox_mb_override(
        rules: &[HookRule],
        outbox_dir: PathBuf,
        report_sink: Arc<dyn FlowSink>,
        max_outbox_mb_override: Option<u64>,
    ) -> Result<Self> {
        // (#2183) Resolved rule-by-rule (not the batch `resolve_rules`)
        // so a `transform` that fails to load can be isolated to THAT
        // rule alone — "a missing or unparseable adapter is a load-time
        // refusal for that RULE only (the rest of the sink still
        // works)" is the issue's own spec, deliberately narrower than
        // every OTHER validation failure here (a bad/missing
        // destination), which still refuses the WHOLE sink, unchanged
        // pre-#2183 behavior (see `resolve_rules_refuses_on_first_non_
        // loopback`).
        let mut resolved = Vec::with_capacity(rules.len());
        for (index, r) in rules.iter().enumerate() {
            match resolve_one_rule(index, r, &outbox_dir) {
                Ok(rr) => resolved.push(rr),
                Err(e) => {
                    let destination_ok = {
                        let http = r.http.clone().filter(|s| !s.trim().is_empty());
                        let file = r.file.clone().filter(|s| !s.trim().is_empty());
                        match (&http, &file) {
                            (Some(u), None) => validate_hook_target_url(u).is_ok(),
                            (None, Some(_)) => true,
                            _ => false,
                        }
                    };
                    if destination_ok && r.transform.as_ref().is_some_and(|t| !t.trim().is_empty()) {
                        eprintln!(
                            "flow::HookSink: rule #{index} disabled — its `transform` failed to load; the rest of \
                             the sink still works: {e:#}"
                        );
                        continue;
                    }
                    return Err(e);
                }
            }
        }
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
                    orphaned_transforms: Arc::new(AtomicU32::new(0)),
                    consecutive_busy: AtomicU32::new(0),
                    last_busy_warning: Mutex::new(None),
                })
            })
            .collect();

        let stop = Arc::new(AtomicBool::new(false));
        let nudge = Arc::new((Mutex::new(false), Condvar::new()));
        // (#2093 merge-gate finding 5) Read live, once, at construction —
        // matches every other config accessor's "env wins live" contract
        // at the one point this sink consults it; a running sink doesn't
        // re-poll config on every write. (#2643) `max_outbox_mb_override`
        // — test-only, see `new_for_test_with_max_outbox_mb` above — wins
        // when present, so a test can pin a cap without mutating the
        // process-global env var every OTHER concurrently-running test's
        // own `HookSink::new` call would also observe.
        let max_outbox_mb =
            max_outbox_mb_override.unwrap_or_else(darkmux_types::config_access::hooks_max_outbox_mb);

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

        /// (#2273 fix-round finding 1) Swap the 2xx response body
        /// mid-test, so ONE receiver can answer a per-record rejection
        /// first and a clean accept afterwards. That sequence — rejections
        /// followed by a clean delivery — is precisely what a last-value
        /// status field silently erases, and it cannot be staged with the
        /// consuming `with_response_body` builder alone.
        pub fn set_response_body(&self, body: Option<&str>) {
            *self.response_body.lock().unwrap() = body.map(str::to_string);
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

    /// (#2183) A `FlowSink` for tests that need a valid `report_sink` but
    /// assert nothing about what lands in it — every `hook.fired`/
    /// `hook.failed`/`hook.dry_run` write is simply discarded. Deliberately
    /// NOT `LocalFileSink`, same isolation reasoning as `CapturingSink`'s
    /// own doc above.
    struct NoopSink;
    impl FlowSink for NoopSink {
        fn write(&self, _record: &FlowRecord) -> Result<()> {
            Ok(())
        }
        fn info(&self) -> SinkInfo {
            SinkInfo { kind: "Noop".into(), config: Default::default(), children: vec![], raw_url: None }
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
        r.payload = Some(serde_json::json!({"tool_name": "create_finding", "ok": true}));
        let m = payload_match(&[("tool_name", serde_json::json!("create_finding"))]);
        assert!(hook_match(&m, &r));

        let m = payload_match(&[("tool_name", serde_json::json!("bash"))]);
        assert!(!hook_match(&m, &r));
    }

    #[test]
    fn payload_predicate_distinguishes_ok_true_from_ok_false() {
        let mut r = record("dispatch.tool");
        r.payload = Some(serde_json::json!({"tool_name": "create_finding", "ok": true}));
        assert!(hook_match(&payload_match(&[("ok", serde_json::json!(true))]), &r));
        assert!(!hook_match(&payload_match(&[("ok", serde_json::json!(false))]), &r));

        r.payload = Some(serde_json::json!({"tool_name": "create_finding", "ok": false}));
        assert!(hook_match(&payload_match(&[("ok", serde_json::json!(false))]), &r));
        assert!(!hook_match(&payload_match(&[("ok", serde_json::json!(true))]), &r));
    }

    #[test]
    fn payload_predicate_on_a_missing_key_never_matches() {
        let mut r = record("dispatch.tool");
        r.payload = Some(serde_json::json!({"tool_name": "create_finding"}));
        // `outcome` isn't in this payload at all.
        assert!(!hook_match(&payload_match(&[("outcome", serde_json::json!("ok"))]), &r));

        // Nor does a record with no payload whatsoever.
        r.payload = None;
        assert!(!hook_match(&payload_match(&[("tool_name", serde_json::json!("create_finding"))]), &r));
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
                e.insert("payload.tool_name".to_string(), serde_json::json!("create_finding"));
                e.insert("payload.ok".to_string(), serde_json::json!(true));
                e
            },
            ..Default::default()
        };
        let desc = describe_match(&m);
        assert!(desc.contains("action=dispatch.tool"), "{desc}");
        assert!(desc.contains("payload.ok=true"), "{desc}");
        assert!(desc.contains("payload.tool_name=\"create_finding\""), "{desc}");
        // Sorted by path: "ok" before "tool_name".
        assert!(desc.find("payload.ok").unwrap() < desc.find("payload.tool_name").unwrap(), "{desc}");
    }

    #[test]
    fn payload_predicate_combines_with_action_and_every_other_field_anded() {
        let mut r = record("dispatch.tool");
        r.payload = Some(serde_json::json!({"tool_name": "create_finding", "ok": true}));
        let m = HookMatch {
            action: Some("dispatch.tool".to_string()),
            extras: {
                let mut e = serde_json::Map::new();
                e.insert("payload.tool_name".to_string(), serde_json::json!("create_finding"));
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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

    /// (#2643 fix-round, MUST FIX 4) RAII clear of `DARKMUX_HOOK_SECRET_0`
    /// for `delivery_carries_the_contract_headers_and_no_signature_when_unsigned`
    /// below — this test's own rule is index 0, and the env override wins
    /// over the Keychain regardless of whether that rule names one (see
    /// `hook_signing_secret`'s doc). A first pass at this fix-round
    /// deleted the defensive clear outright on the theory that #2643
    /// moving the SIGNING test off index 0 (to index 2) removed the only
    /// source of that env key inside this file's own suite — true for
    /// the intra-suite race, but the clear was never about that race
    /// alone: it also guards against an OPERATOR's ambient
    /// `DARKMUX_HOOK_SECRET_0` (e.g. hook signing configured in their
    /// shell). Reproduced: setting that var in the environment before
    /// running this test made it fail — the unsigned rule picked up the
    /// ambient secret and grew a signature it must never carry. RAII
    /// (not the raw remove-and-forget the original had) so this reads
    /// the value once, restores it via `Drop`, and survives a panicking
    /// assertion cleanly — same shape as `paths::ClearDarkmuxHomeGuard`.
    struct ClearHookSecret0Guard {
        prev: Option<String>,
    }

    impl ClearHookSecret0Guard {
        fn new() -> Self {
            let prev = std::env::var("DARKMUX_HOOK_SECRET_0").ok();
            unsafe {
                std::env::remove_var("DARKMUX_HOOK_SECRET_0");
            }
            Self { prev }
        }
    }

    impl Drop for ClearHookSecret0Guard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("DARKMUX_HOOK_SECRET_0", v),
                    None => std::env::remove_var("DARKMUX_HOOK_SECRET_0"),
                }
            }
        }
    }

    #[test]
    // (#2135 option 2 / #2643) #2643 moved the signing test's signed rule
    // to index 2 (see that test's own doc comment), so nothing left in
    // this file's suite mutates rule-index 0's signing-secret env key —
    // `#[serial_test::serial]` against that intra-suite race is correctly
    // gone. The defensive `ClearHookSecret0Guard` above is restored
    // (MUST FIX 4): it guards a different hazard — an operator's own
    // ambient `DARKMUX_HOOK_SECRET_0` — that #2643's index move does
    // nothing to close.
    fn delivery_carries_the_contract_headers_and_no_signature_when_unsigned() {
        let _clear_secret = ClearHookSecret0Guard::new();
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
    // (#2643) Deliberately does NOT need `#[serial_test::serial]` any more.
    // It used to occupy rule-index 0 and mutate `DARKMUX_HOOK_SECRET_0` —
    // the SAME env key every other single-rule test in this file also
    // reads (`resolve_one_rule` calls `hook_signing_secret(index, ..)` for
    // every configured rule at construction time, regardless of whether
    // that rule ever matches an event), which raced roughly 30 other
    // unguarded tests in this module. Reproduced directly: with a
    // temporary 400ms sleep widening the window after `set_var` and the
    // sibling `..._when_unsigned` test's `#[serial]` removed, both landed
    // in the same window in 5/5 runs and the unsigned test failed with
    // exactly the predicted shape — its own unsigned rule (also index 0)
    // picked up THIS test's leaked "top-secret-key" and grew a spurious
    // `x-darkmux-signature` header. Restored immediately after capturing
    // that failure (see the PR body for the exact repro).
    //
    // The structural fix, rather than adding `#[serial]` to ~30 readers
    // (which would serialize a meaningful slice of this file's suite for
    // no reason those tests care about signing at all): give THIS test's
    // signed rule an index nothing else in the file's non-`#[ignore]`d
    // tests ever occupies. Index 1 alone wasn't enough — a re-run of the
    // env-audit sweep after that first attempt caught
    // `resolve_rules_paths_are_stable_across_reordering` below, which
    // builds its OWN 2-rule fixtures and reads both index 0 AND index 1
    // (it swaps a 2-rule vec's order, so both positions get resolved in
    // one run or the other). Two leading decoy rules that can never match
    // push the real, signed rule to index 2 instead, so the env key this
    // test mutates is `DARKMUX_HOOK_SECRET_2` — checked against every
    // OTHER rules-vec literal in this file (including that reordering
    // test and the one `#[ignore]`d 3-rule cost-check, which the default
    // suite never runs) before picking it.
    fn delivery_carries_a_signature_the_receiver_can_recompute_when_signed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start();
        let decoy = |path: &str| HookRule {
            r#match: Some(HookMatch {
                action: Some("darkmux-2643-decoy-never-matches".to_string()),
                ..Default::default()
            }),
            http: Some(receiver.url(path)),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        };
        let rules = vec![
            // Decoys at index 0 and 1 — each resolves cleanly (a valid
            // destination is required) but can never match the
            // "crawl.finding" record this test writes, so neither ever
            // delivers or competes with the real assertions below. Exist
            // ONLY to push the signed rule off the two shared env keys
            // other tests in this file do read.
            decoy("/decoy-never-fires-0"),
            decoy("/decoy-never-fires-1"),
            HookRule {
                r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
                http: Some(receiver.url("/events")),
                signing_secret_keychain_item: Some("darkmux-hook-test-2".to_string()),
                file: None,
                transform: None,
                headers: None,
                attribution_headers: None,
                extras: Default::default(),
            },
        ];
        let prev = std::env::var("DARKMUX_HOOK_SECRET_2").ok();
        // The env override wins over the Keychain item on every platform
        // (see `crate::hook_signing_secret`'s doc) — the portable path a
        // sandboxed test can actually exercise without a real Keychain.
        unsafe {
            std::env::set_var("DARKMUX_HOOK_SECRET_2", "top-secret-key");
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
                Some(v) => std::env::set_var("DARKMUX_HOOK_SECRET_2", v),
                None => std::env::remove_var("DARKMUX_HOOK_SECRET_2"),
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
        let mut headers = build_delivery_headers("{}", None, &delivery_id_for_line("{}"), None, &[], true);
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
            orphaned_transforms: Arc::new(AtomicU32::new(0)),
            consecutive_busy: AtomicU32::new(0),
            last_busy_warning: Mutex::new(None),
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

    // ─── (#2453) `.last` sidecar cross-process race ────────────────────

    /// (#2453 review) The READ side of the same race. `write_last_status`
    /// replaces the sidecar in place — `set_len(0)` then `write_all` —
    /// so between those two syscalls the file on disk is EMPTY. An
    /// unlocked reader landing there parses nothing, returns `None`, and
    /// `summarize_configured_rules` renders `None` as `stalled: false` /
    /// `last_error: None`: a failing rule reported to `doctor` as
    /// healthy.
    ///
    /// Made deterministic with the same seam the writer-race test uses,
    /// fired here from inside `write_status_sidecar_locked` between the
    /// truncate and the write. The reader thread is released exactly
    /// then, so on unlocked-reader code it reads the empty file every
    /// time; with the shared lock it blocks for the rest of the writer's
    /// critical section and reads the COMPLETE new document.
    #[test]
    fn a_reader_never_observes_the_sidecar_mid_rewrite() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some("http://127.0.0.1:1/unused".to_string()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let rule = resolve_rules(&rules, tmp.path()).unwrap().into_iter().next().unwrap();
        let last_status_path = rule.last_status_path.clone();
        let rt = rule_runtime_for(rule);

        // Seed a complete document so "the reader saw nothing" can only
        // mean it observed the rewrite, never "the file didn't exist".
        write_last_status(&rt, true, None);
        assert!(read_last_status(&last_status_path).is_some(), "seed write must have landed");

        let (tx, rx) = std::sync::mpsc::sync_channel::<()>(0);
        set_last_status_truncate_hook(&last_status_path, tx);

        let path_for_reader = last_status_path.clone();
        let reader = std::thread::spawn(move || {
            // Released the instant the writer has truncated and not yet
            // written — no timing guess.
            rx.recv().expect("the writer must fire the seam mid-rewrite");
            read_last_status(&path_for_reader)
        });

        write_last_status(&rt, false, Some("boom"));
        let observed = reader.join().unwrap();
        clear_last_status_truncate_hook(&last_status_path);

        let observed = observed.expect(
            "a reader that lands inside the writer's truncate-to-write window must never see a torn \
             sidecar — `None` here is what `doctor` renders as `stalled: false` / no last error, i.e. \
             a failing rule reported as healthy",
        );
        assert_eq!(
            observed.error.as_deref(),
            Some("boom"),
            "having waited out the writer, the reader must see the COMPLETE new document — got {observed:?}"
        );
    }

    /// (#2453 review) `write_cursor_write_status` must still SELF-HEAL a
    /// sidecar it cannot parse, exactly as the pre-#2453 code did via
    /// `read_last_status(..).unwrap_or_else(default)`.
    ///
    /// Routing the read through the locked handle made it a
    /// `read_to_string`, which hard-errors on invalid UTF-8 — aborting
    /// the closure and skipping the write, permanently: nothing else on
    /// the cursor-failure path ever rewrites this file, so a stalled rule
    /// could never persist its `stalled` flag again and `doctor` would
    /// keep reporting it healthy. Reachable because `error` echoes a
    /// delivery failure's body back, so a torn write can land mid-
    /// multi-byte character.
    #[test]
    fn an_unparseable_sidecar_still_self_heals() {
        let tmp = tempfile::TempDir::new().unwrap();

        // (a) valid UTF-8, invalid JSON — healed before this fix too.
        let truncated_json = tmp.path().join("truncated.last");
        std::fs::write(&truncated_json, br#"{"ts":"2020-01-01T00:00:00Z","ok":tr"#).unwrap();
        write_cursor_write_status(&truncated_json, 3, true);
        let healed = read_last_status(&truncated_json).expect("invalid JSON must heal into a fresh document");
        assert_eq!(healed.cursor_write_failures, 3);
        assert!(healed.stalled);

        // (b) not valid UTF-8 at all — a torn write that landed mid
        // multi-byte character.
        let torn_utf8 = tmp.path().join("torn.last");
        std::fs::write(&torn_utf8, b"{\"ts\":\"2020-01-01T00:00:00Z\",\"ok\":false,\"error\":\"caf\xc3").unwrap();
        assert!(std::fs::read_to_string(&torn_utf8).is_err(), "precondition: those bytes are not valid UTF-8");
        write_cursor_write_status(&torn_utf8, 9, true);
        let healed = read_last_status(&torn_utf8)
            .expect("a non-UTF-8 sidecar must heal too — otherwise this rule's status is wedged forever");
        assert_eq!(healed.cursor_write_failures, 9);
        assert!(healed.stalled);
    }

    /// Builds a `RuleRuntime` around `rule` with the same "quiet, unused
    /// http target" shape `advance_cursor_backs_off_never_resets_on_write_failure`
    /// uses — none of these tests ever let a real delivery attempt run.
    fn rule_runtime_for(rule: ResolvedRule) -> RuleRuntime {
        RuleRuntime {
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
            orphaned_transforms: Arc::new(AtomicU32::new(0)),
            consecutive_busy: AtomicU32::new(0),
            last_busy_warning: Mutex::new(None),
        }
    }

    /// (#2453) Real concurrency proof, not a shape assertion: two
    /// `RuleRuntime`s resolved against the SAME outbox dir/rule (so they
    /// share one `last_status_path` but have their own, independent
    /// atomics) model the module doc's "two darkmux processes share this
    /// outbox directory" scenario — `flock` contends identically across
    /// threads and processes, since it's keyed on the open file
    /// description / inode, never the PID, so a thread-based race here
    /// exercises the exact same kernel primitive a real second process
    /// would.
    ///
    /// The race: seed the sidecar via `write_last_status(rt_a, ok: true,
    /// ..)`. Arm the `#2453` red-prove seam so `write_cursor_write_status`
    /// (running on a second thread, standing in for a second process
    /// advancing ITS OWN cursor) fires the moment it has read that seed,
    /// then sleeps 150ms before writing its merged result back. While it
    /// sleeps, THIS thread — synchronized on the same channel, so there
    /// is no timing guess involved — calls `write_last_status(rt_a, ok:
    /// false, error: "boom", ..)`, a genuinely new terminal delivery
    /// outcome. That write is a single fast syscall pair and completes
    /// well inside the 150ms window.
    ///
    /// Any CORRECT serialization of the two calls (A-then-B or B-then-A)
    /// produces one of exactly two states: pure A (if A runs last) or a
    /// merge that still carries A's `ok: false` / `error: "boom"` forward
    /// (if B runs last, per `write_cursor_write_status`'s own "preserve
    /// the last known delivery outcome" contract). The unlocked bug
    /// produces a THIRD state that neither ordering can produce: B's
    /// stale read (captured before A's write) silently overwrites A's
    /// already-completed write with the pre-A snapshot, discarding "ok:
    /// false / error: boom" outright — the lost update #2453 describes.
    #[test]
    fn concurrent_writers_do_not_lose_last_status_update() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some("http://127.0.0.1:1/unused".to_string()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let rule_a = resolve_rules(&rules, tmp.path()).unwrap().into_iter().next().unwrap();
        let rule_b = resolve_rules(&rules, tmp.path()).unwrap().into_iter().next().unwrap();
        let last_status_path = rule_a.last_status_path.clone();
        assert_eq!(
            last_status_path, rule_b.last_status_path,
            "both runtimes must resolve to the SAME sidecar file for this to model two processes sharing it"
        );

        let rt_a = rule_runtime_for(rule_a);
        let rt_b = rule_runtime_for(rule_b);

        // Seed a known baseline delivery outcome.
        write_last_status(&rt_a, true, None);
        assert_eq!(read_last_status(&last_status_path).map(|s| s.ok), Some(true), "seed write must have landed");

        let (tx, rx) = std::sync::mpsc::sync_channel::<()>(0);
        set_last_status_race_hook(&last_status_path, tx);

        let path_for_b = last_status_path.clone();
        let b = std::thread::spawn(move || {
            write_cursor_write_status(&path_for_b, 42, true);
        });

        // Block until B's read has genuinely fired the hook (no sleep
        // guessing) — THEN, while B is still asleep inside its widened
        // window, land A's fresh terminal outcome.
        rx.recv().expect("B's read must fire the race hook before A writes");
        write_last_status(&rt_a, false, Some("boom"));

        b.join().unwrap();
        clear_last_status_race_hook(&last_status_path);

        let rt_b_unused = &rt_b; // keep B's runtime alive through the join for clarity; never touched otherwise
        let _ = rt_b_unused;

        let final_status = read_last_status(&last_status_path)
            .expect("sidecar must contain valid, parseable JSON after the race — a torn write is also a bug this test catches");

        assert_eq!(
            final_status.error.as_deref(),
            Some("boom"),
            "A's delivery-failure outcome must survive B's concurrent cursor-write-status update — got {final_status:?}, \
             which means B's stale read clobbered A's already-completed write (the lost-update race #2453 describes)"
        );
        assert!(
            !final_status.ok,
            "the surviving status must reflect A's terminal outcome (ok: false), not the stale pre-A seed — got {final_status:?}"
        );
    }

    /// (#2453) A brand-new `.last` sidecar now goes through
    /// `lock_exclusive`'s creator (same as the outbox, the fleet roster,
    /// etc.), so it lands at `0o600` — the SAME mode `write_owner_only_file`
    /// already gave it pre-fix, just via a different creator. Routing
    /// through the lock must not have quietly widened it.
    #[cfg(unix)]
    #[test]
    fn write_last_status_creates_a_fresh_sidecar_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some("http://127.0.0.1:1/unused".to_string()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let rule = resolve_rules(&rules, tmp.path()).unwrap().into_iter().next().unwrap();
        let last_status_path = rule.last_status_path.clone();
        assert!(!last_status_path.exists(), "precondition: no sidecar yet");
        let rt = rule_runtime_for(rule);

        write_last_status(&rt, true, None);

        let mode = std::fs::metadata(&last_status_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a freshly created `.last` sidecar must land owner-only; got {mode:o}");
    }

    /// (#2453) The other half: a sidecar ALREADY on disk at the pre-#2259
    /// world-readable default (`0o644` — as a pre-#2259 binary, or a
    /// hand-edited file, would leave it) must keep working through BOTH
    /// locked write paths, and its mode must NOT be silently tightened —
    /// `lock_exclusive`'s `.mode()` only applies at creation, and neither
    /// `write_status_sidecar_locked` nor `write_cursor_write_status`
    /// calls `set_permissions`. Silently tightening permissions on a file
    /// an operator may have intentionally widened is a separate, louder
    /// decision than "make new files safe by default" (see
    /// `lock_exclusive`'s own doc).
    #[cfg(unix)]
    #[test]
    fn preexisting_world_readable_sidecar_keeps_its_mode_and_keeps_working() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some("http://127.0.0.1:1/unused".to_string()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let rule = resolve_rules(&rules, tmp.path()).unwrap().into_iter().next().unwrap();
        let last_status_path = rule.last_status_path.clone();

        // Simulate a sidecar left behind by a pre-#2259 binary: created
        // at the umask default rather than owner-only.
        std::fs::write(&last_status_path, br#"{"ts":"2020-01-01T00:00:00Z","ok":true}"#).unwrap();
        std::fs::set_permissions(&last_status_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let rt = rule_runtime_for(rule);

        // Both writer paths must still work against it.
        write_last_status(&rt, false, Some("boom"));
        assert_eq!(read_last_status(&last_status_path).and_then(|s| s.error), Some("boom".to_string()));
        let mode_after_write_last_status = std::fs::metadata(&last_status_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode_after_write_last_status, 0o644, "write_last_status must not tighten a pre-existing sidecar's mode");

        write_cursor_write_status(&last_status_path, 7, true);
        let status = read_last_status(&last_status_path).expect("cursor-write-status write must have produced valid JSON");
        assert_eq!(status.cursor_write_failures, 7);
        assert!(status.stalled);
        assert_eq!(status.error.as_deref(), Some("boom"), "cursor-write-status must have preserved the prior delivery outcome");
        let mode_after_cursor_status = std::fs::metadata(&last_status_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode_after_cursor_status, 0o644, "write_cursor_write_status must not tighten a pre-existing sidecar's mode either");
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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

    // ─── (#2643 fix-round, MUST FIX 1) the default cap is still wired ───

    /// Mutation check (self-QA gate): both capping tests below now inject
    /// the cap via `new_for_test_with_max_outbox_mb`, which left nothing
    /// in this file exercising `HookSink::new`'s DEFAULT fallthrough to
    /// `config_access::hooks_max_outbox_mb()` — a reviewer's mutation
    /// (`max_outbox_mb_override.unwrap_or(256)` in place of
    /// `.unwrap_or_else(darkmux_types::config_access::hooks_max_outbox_mb)`)
    /// left this whole suite green. Proving the wire is live needs the
    /// live config value to actually differ from the built-in fallback —
    /// asserting the bare default (256) can't distinguish "read from
    /// config_access" from "hardcoded", since they'd coincide.
    ///
    /// This is the one place in this file that still has to mutate
    /// `DARKMUX_HOOKS_MAX_OUTBOX_MB` process-globally to prove that (the
    /// same class of hazard the two tests above just got rid of) — so the
    /// mutated value is chosen to make the interference PROVABLY inert,
    /// not just spot-checked: `999_999` MiB is LARGER than the built-in
    /// default (256), never smaller. Every other `HookSink::new()` caller
    /// in this file stays comfortably under 256 MiB of undelivered bytes
    /// (the two tests that intentionally exercise cap-DROPPING behavior
    /// inject their own tiny cap via `new_for_test_with_max_outbox_mb`
    /// and never read this env var at all) — so a reader that races this
    /// window and observes 999,999 instead of 256 is STILL under any cap
    /// it could ever observe. A smaller substitute value (e.g. picking
    /// something under 256) would only be "currently" safe, contingent on
    /// no other test crossing it; a larger one is safe by construction,
    /// monotonically, for any test this file could ever grow. `#[serial]`
    /// (matching `wall_clock_and_output_caps_are_wired_from_config_
    /// defaults` below, which needs it for the same class of reason)
    /// still applies against any FUTURE serial mutator of this same key —
    /// it does not, and cannot, protect against a concurrently-running
    /// NON-serial reader, which is exactly why the safety argument above
    /// has to hold regardless of interleaving, not because of the
    /// annotation.
    #[test]
    #[serial_test::serial]
    fn default_outbox_cap_reaches_the_sink_from_config_access() {
        let prev = std::env::var("DARKMUX_HOOKS_MAX_OUTBOX_MB").ok();
        unsafe {
            std::env::set_var("DARKMUX_HOOKS_MAX_OUTBOX_MB", "999999");
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let report: Arc<dyn FlowSink> = Arc::new(NullSink);
        let sink = HookSink::new(&[], tmp.path().to_path_buf(), report).unwrap();
        let got = sink.max_outbox_mb;
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOOKS_MAX_OUTBOX_MB", v),
                None => std::env::remove_var("DARKMUX_HOOKS_MAX_OUTBOX_MB"),
            }
        }
        drop(sink);
        assert_eq!(
            got, 999999,
            "HookSink::new's default path must still reach config_access::hooks_max_outbox_mb(), not a hardcoded fallback"
        );
    }

    // ─── (fix-round finding 2) dropped-appends counter is cross-process ───

    /// Mutation check (self-QA gate): reverting `increment_dropped_appends`
    /// back to the old "read the in-process atomic, write that" shape
    /// makes this test read `1` (instance 3's own fresh atomic, fetch_add
    /// to 1) instead of `3` — proving the fix is what makes the count
    /// survive across separate `HookSink` instances (simulating separate
    /// processes sharing the same outbox directory).
    #[test]
    // (#2643) Used to mutate the process-global `DARKMUX_HOOKS_MAX_OUTBOX_MB`
    // env var (and need `#[serial_test::serial]` against every other test
    // in this file that constructs a `HookSink`, since construction reads
    // that SAME knob unconditionally). Now injects the cap directly via
    // `new_for_test_with_max_outbox_mb` — nothing races because nothing
    // mutates shared state.
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];

        // "Process 1": push the outbox over the 1 MiB cap, then drop one
        // append of its own.
        {
            let report: Arc<dyn FlowSink> = Arc::new(NullSink);
            let sink = HookSink::new_for_test_with_max_outbox_mb(&rules, tmp.path().to_path_buf(), report, 1).unwrap();
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
            let sink = HookSink::new_for_test_with_max_outbox_mb(&rules, tmp.path().to_path_buf(), report, 1).unwrap();
            sink.write(&record("work.drop.2")).unwrap();
        }
        let after_2 = summarize_configured_rules(&rules, tmp.path())[0].dropped_appends;

        // "Process 3": same again.
        {
            let report: Arc<dyn FlowSink> = Arc::new(NullSink);
            let sink = HookSink::new_for_test_with_max_outbox_mb(&rules, tmp.path().to_path_buf(), report, 1).unwrap();
            sink.write(&record("work.drop.3")).unwrap();
        }
        let after_3 = summarize_configured_rules(&rules, tmp.path())[0].dropped_appends;

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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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

    /// (#2259) Compaction must PRESERVE the outbox's owner-only mode.
    /// `maybe_compact_outbox` replaces the outbox by `rename`-ing a
    /// sibling temp file over it, so the surviving inode is the TEMP
    /// file's — its mode, not the original outbox's. A temp written with
    /// plain `fs::write` lands at the umask default (`0o644`), so a
    /// single compaction silently reverted a `0o600` outbox to
    /// world-readable, taking the still-undelivered records (whole flow
    /// records; a crawl finding's `evidence` is a verbatim source line
    /// from the operator's repository) with it.
    #[cfg(unix)]
    #[test]
    fn maybe_compact_outbox_preserves_owner_only_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let outbox_path = tmp.path().join("0-x.outbox.jsonl");
        let cursor_path = tmp.path().join("0-x.cursor");

        // Create the outbox exactly as production does — through the
        // locking creator, which lands it at 0o600.
        {
            let _guard = darkmux_types::flock::lock_exclusive(&outbox_path).unwrap();
        }
        assert_eq!(
            std::fs::metadata(&outbox_path).unwrap().permissions().mode() & 0o777,
            0o600,
            "precondition: the creator lands the outbox at 0o600"
        );
        // `fs::write` onto the ALREADY-created file truncates without
        // touching its mode, so the 0o600 above is what compaction sees.
        let delivered = "{\"n\":0}\n{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n{\"n\":4}\n";
        let undelivered = "{\"n\":5}\n{\"n\":6}\n{\"n\":7}\n";
        std::fs::write(&outbox_path, format!("{delivered}{undelivered}")).unwrap();
        write_cursor(&cursor_path, delivered.len() as u64).unwrap();

        maybe_compact_outbox(&outbox_path, &cursor_path, 10); // threshold: 10 bytes

        assert_eq!(
            std::fs::read_to_string(&outbox_path).unwrap(),
            undelivered,
            "setup guard: compaction must actually have run"
        );
        let mode = std::fs::metadata(&outbox_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o600,
            "compaction must not widen the outbox's mode — undelivered records survive the rewrite; got {mode:o}"
        );
        // The transient temp file must be owner-only too while it exists;
        // it holds the same undelivered bytes.
        let tmp_path = std::path::PathBuf::from(format!("{}.compact.tmp", outbox_path.display()));
        assert!(!tmp_path.exists(), "the compaction temp file must not survive the rename");
    }

    /// (#2259) The stale-temp half of the above. `write_owner_only_file`'s
    /// `.mode()` applies only when it CREATES the file — a `.compact.tmp`
    /// left behind at `0o644` (by a pre-#2259 binary, or by a crash between
    /// the write and the rename, which is the very case this temp+rename
    /// shape exists to survive) is REUSED. Without an explicit
    /// `set_permissions`, that one stale file renames a world-readable
    /// outbox back into place.
    #[cfg(unix)]
    #[test]
    fn maybe_compact_outbox_owner_only_even_over_a_stale_world_readable_temp() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let outbox_path = tmp.path().join("0-x.outbox.jsonl");
        let cursor_path = tmp.path().join("0-x.cursor");
        {
            let _guard = darkmux_types::flock::lock_exclusive(&outbox_path).unwrap();
        }
        let delivered = "{\"n\":0}\n{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n{\"n\":4}\n";
        let undelivered = "{\"n\":5}\n{\"n\":6}\n{\"n\":7}\n";
        std::fs::write(&outbox_path, format!("{delivered}{undelivered}")).unwrap();
        write_cursor(&cursor_path, delivered.len() as u64).unwrap();

        // Leftover temp from an interrupted earlier compaction, at the
        // umask default rather than owner-only.
        let stale_tmp = std::path::PathBuf::from(format!("{}.compact.tmp", outbox_path.display()));
        std::fs::write(&stale_tmp, b"stale").unwrap();
        std::fs::set_permissions(&stale_tmp, std::fs::Permissions::from_mode(0o644)).unwrap();

        maybe_compact_outbox(&outbox_path, &cursor_path, 10);

        assert_eq!(
            std::fs::read_to_string(&outbox_path).unwrap(),
            undelivered,
            "setup guard: compaction must actually have run over the stale temp"
        );
        let mode = std::fs::metadata(&outbox_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a stale world-readable temp must not become a world-readable outbox; got {mode:o}");
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
    // (#2643) Used to mutate the process-global `DARKMUX_HOOKS_MAX_OUTBOX_MB`
    // env var and race
    // `dropped_appends_counter_accumulates_across_separate_hook_sink_instances`
    // (and every other `HookSink::new` in this file) without `#[serial]`.
    // Now injects the cap directly via `new_for_test_with_max_outbox_mb` —
    // see that constructor's doc comment.
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];

        let report: Arc<dyn FlowSink> = Arc::new(NullSink);
        let sink = HookSink::new_for_test_with_max_outbox_mb(&rules, tmp.path().to_path_buf(), report, 1).unwrap();

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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
        let headers = build_delivery_headers("{}", None, &delivery_id_for_line("{}"), None, &[], true);
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        };
        let rule_b = HookRule {
            r#match: Some(HookMatch { action: Some("dispatch.*".to_string()), ..Default::default() }),
            http: Some("http://127.0.0.1:8790/b".to_string()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
                file: None,
                transform: None,
                headers: None,
                attribution_headers: None,
                extras: Default::default(),
            },
            HookRule {
                r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
                http: Some("http://127.0.0.1:1/b".to_string()),
                signing_secret_keychain_item: None,
                file: None,
                transform: None,
                headers: None,
                attribution_headers: None,
                extras: Default::default(),
            },
            HookRule {
                r#match: Some(HookMatch { mission_id: Some("no-such-mission".to_string()), ..Default::default() }),
                http: Some("http://127.0.0.1:1/c".to_string()),
                signing_secret_keychain_item: None,
                file: None,
                transform: None,
                headers: None,
                attribution_headers: None,
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
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
        // (#2273) A receiver rejection is a THIRD outcome, distinct from
        // both a routine delivery (Info) and a transport failure
        // (Error) — it must log loud enough to stand out from the
        // routine `hook.fired` traffic around it, or it reads as a
        // clean delivery to anyone scanning the stream by level.
        assert_eq!(level_wire(fired.level), "warn", "{fired:?}");
        // (#2273) Also persisted into the `.last` sidecar — not just the
        // flow record — so a SEPARATE `darkmux doctor` invocation can
        // still see the rejection after this event has scrolled off the
        // stream.
        let m = HookMatch { action: Some("crawl.*".to_string()), ..Default::default() };
        let key = rule_key(&m, &receiver.url("/events"));
        let last = read_last_status(&last_status_path(tmp.path(), &key)).expect("`.last` sidecar must exist");
        assert_eq!(last.last_receiver_rejected, Some(1), "{last:?}");
        // (#2273 fix-round finding 1) …and into the CUMULATIVE counter
        // sidecar, which is the one `doctor` / `flow status` key on.
        let summary = summarize_configured_rules(&rules, tmp.path()).remove(0);
        assert_eq!(summary.receiver_rejected_total, 1, "{summary:?}");
        drop(sink);
    }

    /// (#2273 inverted case) The companion to the test above: a receiver
    /// that accepts EVERYTHING must stay quiet on both axes a
    /// mutation-only red-prove could otherwise slip past — Info level
    /// (not Warn) on the flow record, and no `last_receiver_rejected` in
    /// the persisted sidecar. Without this, deleting the `rejected_count
    /// = ... .filter(...)` guard and just always warning would still
    /// pass the rejection test above.
    #[test]
    fn hook_fired_stays_info_and_quiet_when_receiver_accepts_cleanly() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
        assert!(
            fired.payload.as_ref().and_then(|p| p.get("receiver_rejected")).is_none(),
            "a clean accept must never carry a receiver_rejected field: {fired:?}"
        );
        assert_eq!(level_wire(fired.level), "info", "{fired:?}");
        let m = HookMatch { action: Some("crawl.*".to_string()), ..Default::default() };
        let key = rule_key(&m, &receiver.url("/events"));
        let last = read_last_status(&last_status_path(tmp.path(), &key)).expect("`.last` sidecar must exist");
        assert_eq!(last.last_receiver_rejected, None, "{last:?}");
        let summary = summarize_configured_rules(&rules, tmp.path()).remove(0);
        assert_eq!(summary.receiver_rejected_total, 0, "a clean accept must never count one: {summary:?}");
        drop(sink);
    }

    /// (#2273 fix-round finding 1) The BLOCKER this round fixes: a
    /// rejection recorded ONLY as a last-value field erases itself on the
    /// next clean delivery, which on a live rule is seconds later — so
    /// `darkmux doctor` reports the rule clean while records have in fact
    /// been lost. Rejections must accumulate on a substrate no later
    /// delivery overwrites.
    ///
    /// Sequence: one delivery the receiver reports rejecting, then one it
    /// accepts cleanly. The `.last` sidecar's `last_receiver_rejected`
    /// legitimately goes back to `None` (it is a LAST-value field and this
    /// test pins that, so nobody "fixes" the erasure by making that field
    /// sticky and calling it a cumulative count) — while
    /// `receiver_rejected_total` must still read 1.
    #[test]
    fn receiver_rejected_total_survives_a_later_clean_delivery() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start().with_response_body(r#"{"ok":true,"accepted":0,"rejected":1}"#);
        let m = HookMatch { action: Some("crawl.*".to_string()), ..Default::default() };
        let url = receiver.url("/events");
        let rules = vec![HookRule {
            r#match: Some(m.clone()),
            http: Some(url.clone()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let report: Arc<dyn FlowSink> = Arc::new(NullSink);
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();
        let key = rule_key(&m, &url);
        let last_path = last_status_path(tmp.path(), &key);

        sink.write(&record("crawl.finding")).unwrap();
        assert!(
            wait_until(
                || read_last_status(&last_path).and_then(|s| s.last_receiver_rejected) == Some(1),
                Duration::from_secs(5)
            ),
            "the rejected delivery must land first"
        );
        assert_eq!(summarize_configured_rules(&rules, tmp.path())[0].receiver_rejected_total, 1);

        // The receiver starts accepting cleanly, and one more record goes
        // out. Waiting for `last_receiver_rejected` to go back to `None`
        // is the rendezvous: it can only happen once the SECOND delivery's
        // terminal status write has landed.
        receiver.set_response_body(None);
        sink.write(&record("crawl.finding")).unwrap();
        assert!(
            wait_until(
                || read_last_status(&last_path).is_some_and(|s| s.last_receiver_rejected.is_none()),
                Duration::from_secs(5)
            ),
            "the clean delivery must land second"
        );

        let summary = summarize_configured_rules(&rules, tmp.path()).remove(0);
        assert_eq!(
            summary.receiver_rejected_total, 1,
            "one clean delivery must not erase a recorded rejection — a `.last`-only signal does exactly \
             that, seconds after the loss: {summary:?}"
        );
        drop(sink);
    }

    // ─── (#2196) the receiver's own rejection REASON, not just the count ──

    /// (#2196) One of many: a receiver's `results[]` can carry both
    /// accepted and rejected entries in the same 2xx response. The
    /// rejected entry's `error` text must ride the `hook.fired` payload,
    /// the `.last` sidecar, and the summary `doctor`/`flow status` read —
    /// so an operator learns WHY, not just that something was thrown
    /// away.
    #[test]
    fn hook_fired_surfaces_the_receivers_rejection_reason_alongside_the_count() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start().with_response_body(
            r#"{"ok":true,"accepted":2,"rejected":1,"results":[{"ok":true},{"ok":true},{"ok":false,"error":"payload field \"file\" must be a non-empty string"}]}"#,
        );
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
        let reasons: Vec<String> = fired
            .payload
            .as_ref()
            .and_then(|p| p.get("receiver_rejected_reasons"))
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        // (#2196 fix-round 2, MUST FIX C) The receiver's raw text is 47
        // display columns — well under the fix-round-2 budget
        // (`REJECTION_REASON_RAW_BUDGET`, 118) — so it now survives
        // WHOLE: this is the PR's own flagship example of the 40-column
        // cap destroying the disclosure the feature exists to provide
        // (it used to lose the word "string", the actual constraint
        // named in the message). These assertions check what's actually
        // stored, before the render layer's separate quoting/escaping.
        assert_eq!(
            reasons,
            vec!["payload field \"file\" must be a non-empty string".to_string()],
            "{fired:?}"
        );
        assert_eq!(level_wire(fired.level), "warn", "{fired:?}");

        let m = HookMatch { action: Some("crawl.*".to_string()), ..Default::default() };
        let key = rule_key(&m, &receiver.url("/events"));
        let last = read_last_status(&last_status_path(tmp.path(), &key)).expect("`.last` sidecar must exist");
        assert_eq!(
            last.last_receiver_rejected_reasons,
            vec!["payload field \"file\" must be a non-empty string".to_string()],
            "{last:?}"
        );

        let summary = summarize_configured_rules(&rules, tmp.path()).remove(0);
        assert_eq!(
            summary.last_receiver_rejected_reasons,
            vec!["payload field \"file\" must be a non-empty string".to_string()],
            "{summary:?}"
        );
        drop(sink);
    }

    /// (#2196 boundary) Every record in the delivery rejected — the
    /// `results` array has no `ok: true` entries at all. Every reason
    /// (up to the bound) must still surface, not just the first.
    #[test]
    fn hook_fired_surfaces_every_reason_when_the_whole_delivery_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start().with_response_body(
            r#"{"ok":true,"accepted":0,"rejected":2,"results":[{"ok":false,"error":"reason A"},{"ok":false,"error":"reason B"}]}"#,
        );
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let report: Arc<dyn FlowSink> = Arc::new(NullSink);
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();
        let m = HookMatch { action: Some("crawl.*".to_string()), ..Default::default() };
        let key = rule_key(&m, &receiver.url("/events"));
        let last_path = last_status_path(tmp.path(), &key);
        sink.write(&record("crawl.finding")).unwrap();
        assert!(
            wait_until(
                || read_last_status(&last_path).is_some_and(|s| s.last_receiver_rejected == Some(2)),
                Duration::from_secs(5)
            ),
            "the fully-rejected delivery must land"
        );
        let last = read_last_status(&last_path).unwrap();
        assert_eq!(
            last.last_receiver_rejected_reasons,
            vec!["reason A".to_string(), "reason B".to_string()],
            "{last:?}"
        );
        drop(sink);
    }

    /// (#2196 boundary) A 2xx body the sink cannot parse as JSON at all —
    /// no `rejected` count, no reasons, and the delivery must still read
    /// as an ordinary clean accept (Info level, nothing persisted). The
    /// sink must never crash on garbage in a 2xx body, and must never
    /// invent a rejection where the receiver never claimed one.
    #[test]
    fn hook_fired_stays_clean_when_a_2xx_body_is_not_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start().with_response_body("not valid json at all {{{");
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
        assert_eq!(level_wire(fired.level), "info", "{fired:?}");
        assert!(
            fired.payload.as_ref().and_then(|p| p.get("receiver_rejected")).is_none(),
            "an unparseable body must never be read as a rejection: {fired:?}"
        );
        assert!(
            fired.payload.as_ref().and_then(|p| p.get("receiver_rejected_reasons")).is_none(),
            "no reasons can exist without a parseable body: {fired:?}"
        );
        let m = HookMatch { action: Some("crawl.*".to_string()), ..Default::default() };
        let key = rule_key(&m, &receiver.url("/events"));
        let summary = summarize_configured_rules(&rules, tmp.path()).remove(0);
        assert_eq!(summary.receiver_rejected_total, 0, "{summary:?}");
        assert!(summary.last_receiver_rejected_reasons.is_empty(), "{summary:?}");
        let _ = key;
        drop(sink);
    }

    /// (#2196 boundary) A receiver that answers 2xx with a body carrying
    /// no per-record detail whatsoever (no `rejected`, no `results`) —
    /// darkmux has nothing to disclose, so nothing is disclosed. Distinct
    /// from the unparseable-body case above: this body IS valid JSON,
    /// just not the rejection-reporting contract.
    #[test]
    fn hook_fired_stays_clean_when_a_2xx_body_carries_no_per_record_detail() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start().with_response_body(r#"{"ok":true}"#);
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let report: Arc<dyn FlowSink> = Arc::new(NullSink);
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), report).unwrap();
        let m = HookMatch { action: Some("crawl.*".to_string()), ..Default::default() };
        let key = rule_key(&m, &receiver.url("/events"));
        let last_path = last_status_path(tmp.path(), &key);
        sink.write(&record("crawl.finding")).unwrap();
        assert!(
            wait_until(|| read_last_status(&last_path).is_some(), Duration::from_secs(3)),
            "the clean delivery must land"
        );
        let last = read_last_status(&last_path).unwrap();
        assert_eq!(last.last_receiver_rejected, None, "{last:?}");
        assert!(last.last_receiver_rejected_reasons.is_empty(), "{last:?}");
        let summary = summarize_configured_rules(&rules, tmp.path()).remove(0);
        assert_eq!(summary.receiver_rejected_total, 0, "{summary:?}");
        drop(sink);
    }

    /// (#2196 fix-round MUST FIX 2 — the real gate) The prior boundary
    /// test above only exercises `{"ok":true}` — no `results` array at
    /// all, so `extract_rejection_reasons` returns empty trivially and
    /// the `if rejected_for_status.is_some() { reasons } else { vec![] }`
    /// gate at the delivery call site is never actually exercised. THIS
    /// is the real boundary: a `results[]` entry explicitly marked
    /// `"ok": false` with an `error` string (so `extract_rejection_reasons`
    /// DOES produce a non-empty reason list), but with NO top-level
    /// `rejected` count — a shape the local tracker's own contract never
    /// produces (a rejected entry always accompanies a non-zero
    /// `rejected`), but the sink must not assume that; the gate exists
    /// specifically because this shape is not the defined contract.
    ///
    /// Deleting that `if` (making `reasons_for_status` unconditional)
    /// leaves every existing test green — proven by running this exact
    /// mutation before writing this test — while producing a `hook.fired`
    /// carrying a rejection reason with NO count (an Info-level record,
    /// since `rejected_count` in `emit_hook_record_with` is computed
    /// independently from the raw `receiver_rejected` and stays `None`
    /// here), and a `.last` sidecar with reasons but
    /// `last_receiver_rejected: None` — a clean delivery, falsely marked.
    #[test]
    fn hook_fired_stays_clean_when_results_reject_without_a_top_level_rejected_count() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start().with_response_body(
            r#"{"ok":true,"results":[{"ok":false,"error":"CLEAN-MISMARK"}]}"#,
        );
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
        let m = HookMatch { action: Some("crawl.*".to_string()), ..Default::default() };
        let key = rule_key(&m, &receiver.url("/events"));
        let last_path = last_status_path(tmp.path(), &key);
        sink.write(&record("crawl.finding")).unwrap();
        assert!(
            wait_until(|| read_last_status(&last_path).is_some(), Duration::from_secs(3)),
            "the delivery must land"
        );
        assert!(
            wait_until(
                || capture.0.lock().unwrap().iter().any(|r| r.action == "hook.fired"),
                Duration::from_secs(3)
            ),
            "hook.fired must land"
        );
        let fired = capture.0.lock().unwrap().iter().find(|r| r.action == "hook.fired").cloned().unwrap();
        assert!(
            fired.payload.as_ref().and_then(|p| p.get("receiver_rejected")).is_none(),
            "no top-level `rejected` key in the body means no count to disclose: {fired:?}"
        );
        assert!(
            fired.payload.as_ref().and_then(|p| p.get("receiver_rejected_reasons")).is_none(),
            "a reason must never ride without the count that names it: {fired:?}"
        );
        assert_eq!(level_wire(fired.level), "info", "{fired:?}");

        let last = read_last_status(&last_path).unwrap();
        assert_eq!(last.last_receiver_rejected, None, "{last:?}");
        assert!(
            last.last_receiver_rejected_reasons.is_empty(),
            "the sidecar must not carry a reason with no matching count: {last:?}"
        );

        let summary = summarize_configured_rules(&rules, tmp.path()).remove(0);
        assert_eq!(summary.receiver_rejected_total, 0, "{summary:?}");
        assert!(summary.last_receiver_rejected_reasons.is_empty(), "{summary:?}");
        drop(sink);
    }

    /// (#2196 fix-round MUST FIX 1) `truncate_reason` (the single choke
    /// point every `results[].error` string passes through — see
    /// `extract_rejection_reasons`) must strip every terminal-corruption
    /// primitive BEFORE a receiver's text reaches `flow status`,
    /// `darkmux doctor`, or the `eprintln!` at the `DeliveryOutcome::Success`
    /// call site — all of which render the string directly to a real
    /// terminal — AND collapse whitespace runs, which is what actually
    /// defeats the exact-vocabulary row forgery (see
    /// [`MAX_REJECTION_REASON_DISPLAY_WIDTH`]'s doc).
    ///
    /// This REPLACES the PR's original version of this test, which
    /// asserted the OLD, defective behavior verbatim: it expected the
    /// newline-forgery input's six-space indentation to survive fully
    /// intact (`"ok      stalled: ..."`) — exactly the primitive the
    /// terminal-wrap row forgery in MUST FIX 1 depends on. That
    /// assertion encoded the defect as expected behavior; it's gone now,
    /// and whitespace runs collapse to one space.
    #[test]
    fn truncate_reason_strips_forgery_primitives_and_collapses_whitespace() {
        // (#2196 fix-round 2, MUST FIX C) The collapsed string is 63
        // columns — well under the fix-round-2 raw budget (118) — so it
        // now survives WHOLE, unlike under the first fix-round's 40
        // column cap. The whitespace-collapse property is still what
        // this probe pins: the forged six-space indentation must not
        // survive even though nothing here is long enough to need
        // truncating.
        let newline_forgery = "ok\n      stalled: no drainer heartbeat for 9999s\n      quarantined lines: 0";
        let out = truncate_reason(newline_forgery);
        assert!(!out.contains('\n'), "{out:?}");
        assert!(!out.contains("  "), "no run of 2+ spaces may survive sanitization: {out:?}");
        assert_eq!(out, "ok stalled: no drainer heartbeat for 9999s quarantined lines: 0", "{out:?}");

        let ansi = "\x1b[2J\x1b[31mFATAL: darkmux is corrupt\x1b[0m";
        let out = truncate_reason(ansi);
        assert!(!out.contains('\x1b'), "{out:?}");
        assert_eq!(out, "[2J[31mFATAL: darkmux is corrupt[0m");

        let cr = "real\rFAKE";
        let out = truncate_reason(cr);
        assert!(!out.contains('\r'), "{out:?}");
        assert_eq!(out, "realFAKE");

        // Printable non-ASCII text (this is prose, not a header value)
        // must survive — the filter targets control/forgery characters,
        // not anything outside plain ASCII.
        assert_eq!(truncate_reason("café \u{2014} rejected"), "café \u{2014} rejected");

        // (#2196 fix-round MUST FIX 1) Trojan Source class (CVE-2021-42574):
        // RLO reorders the VISUAL rendering of everything after it
        // without touching logical byte order — this exact payload would
        // otherwise display as an approval-reading string.
        let rlo = "delivered OK \u{202e})deppord( deriuqer dleif";
        let out = truncate_reason(rlo);
        assert!(!out.contains('\u{202e}'), "{out:?}");

        // Directional isolates + RLM — the rest of the Trojan Source set.
        for c in ['\u{2066}', '\u{2067}', '\u{2069}', '\u{200f}'] {
            let poisoned = format!("ok{c}text");
            assert!(!truncate_reason(&poisoned).contains(c), "{c:?} must be stripped");
        }

        // Invisible / zero-width characters.
        for c in ['\u{200b}', '\u{feff}', '\u{00ad}'] {
            let poisoned = format!("ok{c}text");
            assert!(!truncate_reason(&poisoned).contains(c), "{c:?} must be stripped");
        }

        // Unicode line/paragraph separators — the same row-forgery
        // primitive as `\n`, wearing a category `is_control()` misses.
        for c in ['\u{2028}', '\u{2029}'] {
            let poisoned = format!("ok{c}      stalled: fake");
            let out = truncate_reason(&poisoned);
            assert!(!out.contains(c), "{c:?} must be stripped: {out:?}");
            assert!(!out.contains("  "), "whitespace run must still collapse: {out:?}");
        }
    }

    /// (#2196 fix-round MUST FIX 1, forgery pin — short case) A reason
    /// SHORT ENOUGH to survive `truncate_reason`'s width cap WHOLE (no
    /// ellipsis) must still have an embedded darkmux-row-shaped
    /// indentation collapsed — proving the fix isn't merely "the width
    /// cap cuts off anything long enough to reach a wrap point", which
    /// would leave a reason deliberately engineered to fit inside the
    /// cap free to forge a row. `"      cursor-write failures: 0
    /// (recovered)"` alone is 43 columns — already longer than the
    /// entire cap — so a forgery attempt reproducing that exact row
    /// verbatim can never survive truncation at all; this is the
    /// complementary case where the forged text is short enough that
    /// truncation isn't what saves it.
    #[test]
    fn truncate_reason_collapses_a_forged_row_prefix_even_when_it_fits_whole() {
        let short_forgery = "x      cursor-write failures: 0";
        let out = truncate_reason(short_forgery);
        assert!(!out.ends_with('…'), "must survive whole, not truncated: {out:?}");
        assert!(!out.contains("  "), "the forged indentation must collapse: {out:?}");
        assert_eq!(out, "x cursor-write failures: 0");
    }

    /// (#2196 fix-round MUST FIX 2, extended in fix-round 2 for the
    /// head+tail split MUST FIX C introduced) `bound_reason_width` walks
    /// `char_indices` from BOTH ends — forward for the head cut, in
    /// reverse for the tail cut — so every candidate cut point is a
    /// UTF-8 char boundary BY CONSTRUCTION in both directions. Pins that
    /// against exactly the inputs a naive byte-offset slice panics on: a
    /// multi-byte character straddling the HEAD cut point, and an
    /// all-wide-CJK reason (every character multi-byte) straddling BOTH
    /// cut points at once. Numbers are derived from the real constants,
    /// not hardcoded, so this stays correct if the budget/tail-reserve is
    /// ever retuned. Red-proved: reverting to a raw byte-offset slice on
    /// either cut panics on these inputs with "byte index N is not a
    /// char boundary".
    #[test]
    fn truncate_reason_never_panics_on_a_multibyte_char_straddling_the_cut() {
        let ellipsis_w = display_width('…');
        let head_budget = REJECTION_REASON_RAW_BUDGET - ellipsis_w - REJECTION_REASON_TAIL_RESERVE;

        // `head_budget` ASCII columns, then one 3-byte character (総,
        // U+7DCF, display width 2) straddling the head cut point, then
        // enough trailing filler to push the total past the overall
        // budget so truncation actually engages.
        let straddle =
            format!("{}{}{}", "a".repeat(head_budget), '\u{7dcf}', "b".repeat(REJECTION_REASON_TAIL_RESERVE + 10));
        let out = truncate_reason(&straddle);
        assert!(out.is_char_boundary(out.len()), "{out:?}");
        assert!(out.contains('…'), "{out:?}");

        // All-wide CJK: every character is 2 display columns, so BOTH
        // the head cut (walked forward) and the tail cut (walked
        // backward) land on a multi-byte character — 201 bytes total,
        // nowhere near an ASCII byte boundary anywhere.
        let all_cjk = "一".repeat(67); // 201 bytes, 67 chars, 134 display columns
        let out = truncate_reason(&all_cjk);
        assert!(out.is_char_boundary(out.len()), "{out:?}");
        assert!(out.contains('…'), "{out:?}");
        assert!(!out.ends_with('…'), "a middle cut must leave a real tail: {out:?}");
        // Every surviving character (head + tail) must be an intact 一 —
        // no partial byte sequence anywhere — and the count matches what
        // the same width-2-per-char arithmetic the production walk uses
        // predicts for the head and tail budgets.
        assert!(out.chars().all(|c| c == '一' || c == '…'), "{out:?}");
        let tail_budget = REJECTION_REASON_TAIL_RESERVE.min((REJECTION_REASON_RAW_BUDGET - ellipsis_w) / 2);
        let expected_head_chars = head_budget / 2;
        let expected_tail_chars = tail_budget / 2;
        assert_eq!(
            out.chars().filter(|c| *c == '一').count(),
            expected_head_chars + expected_tail_chars,
            "{out:?}"
        );
    }

    /// (#2196 fix-round 2, MUST FIX A) The verifier's second pass found
    /// four MORE `Cf` format characters (U+061C, U+2060–2064, U+180E,
    /// U+FFF9–FFFB) plus a family of characters Unicode assigns to an
    /// ordinary PRINTABLE category (`Lo`/`So`) that nonetheless render as
    /// a blank glyph in every mainstream terminal font (the Hangul
    /// filler jamo, `U+2800` BRAILLE PATTERN BLANK) — none caught by the
    /// first fix-round's enumerated denylist. `is_stripped_for_display`
    /// closes the format-character class STRUCTURALLY (by general
    /// category — see its doc), and names the small closed set of
    /// blank-glyph/variation-selector exceptions the category check
    /// can't reach. This pins that every character actually named in the
    /// finding is stripped with NO trace and NO gap left behind — not
    /// just that the category check compiles.
    #[test]
    fn sanitize_reason_text_strips_the_expanded_format_and_blank_glyph_set() {
        let cases: &[(char, &str)] = &[
            ('\u{2800}', "BRAILLE PATTERN BLANK"),
            ('\u{3164}', "HANGUL FILLER"),
            ('\u{115F}', "HANGUL CHOSEONG FILLER"),
            ('\u{1160}', "HANGUL JUNGSEONG FILLER"),
            ('\u{FFA0}', "HALFWIDTH HANGUL FILLER"),
            ('\u{061C}', "ARABIC LETTER MARK"),
            ('\u{2060}', "WORD JOINER"),
            ('\u{2061}', "FUNCTION APPLICATION"),
            ('\u{2062}', "INVISIBLE TIMES"),
            ('\u{2063}', "INVISIBLE SEPARATOR"),
            ('\u{2064}', "INVISIBLE PLUS"),
            ('\u{180E}', "MONGOLIAN VOWEL SEPARATOR"),
            ('\u{FFF9}', "INTERLINEAR ANNOTATION ANCHOR"),
            ('\u{FFFA}', "INTERLINEAR ANNOTATION SEPARATOR"),
            ('\u{FFFB}', "INTERLINEAR ANNOTATION TERMINATOR"),
            ('\u{FE00}', "VARIATION SELECTOR-1"),
            ('\u{FE0F}', "VARIATION SELECTOR-16"),
            ('\u{E0100}', "VARIATION SELECTOR-17 (supplement, first)"),
            ('\u{E01EF}', "VARIATION SELECTOR SUPPLEMENT (last)"),
            ('\u{E0000}', "TAG (block start)"),
            ('\u{E0001}', "LANGUAGE TAG"),
            ('\u{E007F}', "CANCEL TAG (block end)"),
        ];
        for (c, name) in cases {
            let poisoned = format!("real{c}text");
            let out = sanitize_reason_text(&poisoned);
            assert_eq!(out, "realtext", "{name} (U+{:06X}) must be stripped with no gap left behind: {out:?}", *c as u32);
        }
    }

    /// (#2196 fix-round 2, inverted case) [`is_denylisted_category`]
    /// shares a general category (`Lo`, `So`, or `Mn`) with every
    /// exception the previous test pins as stripped — a category check
    /// that accidentally dropped the WHOLE category instead of the
    /// specific closed exception would pass that test for the wrong
    /// reason. This proves ordinary international prose, a real
    /// combining accent, and an ordinary emoji all survive untouched.
    #[test]
    fn sanitize_reason_text_keeps_legitimate_letters_symbols_and_combining_marks() {
        assert_eq!(sanitize_reason_text("café — 総 例 プ 你好"), "café — 総 例 プ 你好");
        // A real combining accent — category `Mn`, the SAME category as
        // the variation selectors the previous test proves get stripped.
        assert_eq!(sanitize_reason_text("cafe\u{0301}"), "cafe\u{0301}");
        // An ordinary emoji — category `So`, the SAME category as
        // BRAILLE PATTERN BLANK, which the previous test proves gets
        // stripped.
        assert_eq!(sanitize_reason_text("done \u{2705}"), "done \u{2705}");
    }

    /// (#2196 fix-round 2, MUST FIX B) `display_width` now delegates to
    /// the `unicode-width` crate instead of a hand-rolled range table.
    /// Pins the delegation itself — using the crate's real API on every
    /// call, not silently falling back to a stale default — against the
    /// exact characters the verifier's second pass found overshooting:
    /// two ordinary, non-exotic Unicode blocks (CJK Compatibility Forms
    /// `U+FE30..=FE6F` and Hangul Jamo Extended-B `U+D7B0..=D7FF`) the
    /// hand-rolled table never named at all, plus supplementary-plane
    /// emoji it scored narrow. These are `unicode-width` 0.2.2's actual
    /// classifications (verified against the pinned crate version) —
    /// this pins correct DELEGATION, not a re-derivation of the crate's
    /// own East-Asian-Width table.
    #[test]
    fn display_width_delegates_to_unicode_width_for_blocks_the_old_table_missed() {
        let cases: &[(char, usize, &str)] = &[
            ('\u{1F600}', 2, "emoji GRINNING FACE — supplementary plane, outside every range the old table listed"),
            ('\u{1F4A5}', 2, "emoji COLLISION — same gap"),
            ('\u{FE35}', 2, "CJK COMPATIBILITY FORMS (U+FE30..=FE6F) — an ordinary block the old table never named"),
            ('\u{D7B0}', 0, "HANGUL JAMO EXTENDED-B (U+D7B0..=D7FF) — another ordinary block the old table never named"),
            ('\u{231A}', 2, "WATCH — East-Asian-Width Wide"),
            ('\u{2B50}', 2, "STAR — East-Asian-Width Wide"),
            ('\u{0301}', 0, "COMBINING ACUTE ACCENT — a real zero-width mark, scored 1 (over-conservative) by the old table"),
            ('a', 1, "ordinary ASCII — the baseline"),
        ];
        for (c, expected, why) in cases {
            assert_eq!(display_width(*c), *expected, "U+{:06X}: {why}", *c as u32);
        }
    }

    /// (#2196 fix-round 2, MUST FIX D) No existing test exercised a
    /// reason containing `"`/`\` through the RENDER path — before this
    /// fix, deleting `format_rejection_reasons_for_display`'s
    /// `.replace('\\', "\\\\").replace('"', "\\\"")` left BOTH
    /// `-p darkmux-flow --lib hooks::` and `--lib status::` fully green.
    /// The reason below mirrors the verifier's own proof: a
    /// semicolon-joined SECOND "reason" smuggled inside the first, using
    /// darkmux's own reason-list format, plus a Windows-style path to
    /// cover the backslash half of the escape independently. Asserted
    /// against an exact expected literal (not by re-deriving the escape
    /// with the same replace calls) so the assertion can't pass
    /// tautologically.
    #[test]
    fn format_rejection_reasons_for_display_escapes_embedded_quotes_and_backslashes() {
        let reason = r#"bad" ; "dropped: 0 (path C:\Users\test)"#;
        let rendered = format_rejection_reasons_for_display(&[reason.to_string()]);
        let expected = r#""bad\" ; \"dropped: 0 (path C:\\Users\\test)""#;
        assert_eq!(rendered, expected, "{rendered:?}");
    }

    /// (#2196 fix-round 2, MUST FIX A + D, forgery re-proof) Re-runs the
    /// verifier's own row-forgery proof against the fixed pipeline: the
    /// four blank-rendering characters from MUST FIX A, AND the
    /// escape-expansion path from MUST FIX D (which needed no exotic
    /// input at all — a purely-ASCII reason made of quote characters
    /// restored the wrap precondition once escaped). None may produce a
    /// row containing two-or-more real rendered blank/space columns once
    /// through `format_rejection_reasons_for_display` — the same
    /// property [`collapse_whitespace_and_trim`]'s own forgery test pins
    /// for whitespace, extended here to the non-whitespace blank
    /// primitives and to escaping.
    #[test]
    fn rejection_reason_pipeline_defeats_the_verifiers_blank_glyph_and_escape_forgeries() {
        // MUST FIX A: each blank-glyph character, repeated enough to
        // simulate a padded forgery attempt, must vanish entirely rather
        // than surface as blank columns.
        for c in ['\u{2800}', '\u{3164}', '\u{115F}', '\u{FFA0}'] {
            let padded = format!("real{}", c.to_string().repeat(20));
            let rendered = format_rejection_reasons_for_display(&[padded]);
            assert!(!rendered.contains(c), "U+{:06X} must not survive to the rendered row: {rendered:?}", c as u32);
            // No run of 2+ rendered columns can come from what used to
            // be blank-glyph padding — the visible text must be exactly
            // the real word, quoted.
            assert_eq!(rendered, "\"real\"", "U+{:06X}: {rendered:?}", c as u32);
        }

        // MUST FIX D: a purely-ASCII reason made mostly of quote
        // characters must not, once escaped, grow past the raw budget —
        // this is the padding the verifier's own proof used to restore
        // the wrap precondition without any exotic Unicode at all.
        let quote_heavy: String = "\"".repeat(200);
        let rendered = format_rejection_reasons_for_display(&[quote_heavy]);
        let real_width: usize = rendered.chars().map(display_width).sum();
        assert!(
            real_width <= MAX_REJECTION_REASON_DISPLAY_WIDTH,
            "escaping must never grow the rendered reason past the total budget: {real_width} > \
             {MAX_REJECTION_REASON_DISPLAY_WIDTH} in {rendered:?}"
        );
    }

    /// (#2196 fix-round 3, MUST FIX F) The width bound must bound LENGTH
    /// as well as COLUMNS.
    ///
    /// Fix-round 3 swapped the hand-rolled width function for
    /// `unicode-width`, which is CORRECT that a combining mark occupies
    /// zero rendered columns — and in doing so removed the only thing
    /// bounding length anywhere in this pipeline. Round 1 had a flat
    /// 200-BYTE cap; round 2's hand-rolled function scored a combining
    /// mark as 1 column, so its column budget doubled as a rough
    /// character cap at ~40. Round 3 had neither: zero-width characters
    /// cost nothing against the budget, so the head loop's
    /// `w + cw > head_budget` never tripped on them, and nothing else
    /// capped characters or bytes. Measured on the round-3 code:
    ///
    /// ```text
    /// pure combining : in_chars=20000 out_chars=20000 out_bytes=40000 out_width=0
    /// mixed          : out_chars=20008 out_bytes=40008 out_width=8
    /// ```
    ///
    /// Reachable, not extrapolated: the response body is read through a
    /// 64 KiB `Read::take`, so ONE 2xx response under that cap carries
    /// the whole payload into the `.last` sidecar, the `hook.fired`
    /// record (and from there the daily JSONL, the audit sink, and Redis
    /// `XADD MAXLEN ~10000`), the delivery `eprintln!`, and both
    /// `flow status` and `doctor` — on a row of nominal width 10. Twenty
    /// thousand marks stacked on one cell is also a terminal-corruption
    /// primitive, not merely volume.
    ///
    /// Red-proves by name: delete `|| n + 1 > head_char_budget` from
    /// `bound_reason_width`'s head loop, or drop the
    /// `&& total_chars <= char_budget` term from its early return, and
    /// the character assertions below fail. The trailing positive control
    /// keeps the ceiling from being "passed" by a bound that eats real
    /// reasons.
    #[test]
    fn rejection_reason_bounds_length_not_only_rendered_width() {
        let char_ceiling = REJECTION_REASON_RAW_BUDGET * REJECTION_REASON_CHARS_PER_COLUMN;

        // The exact shape the finding measured: 20,000 U+0301 COMBINING
        // ACUTE ACCENT, each 2 bytes on the wire and 0 rendered columns,
        // with a leading real word so `strip_leading_zero_width` (the
        // separate CONSIDER fix) cannot be what closes this — the
        // character ceiling has to.
        let mixed = format!("rejected{}", "\u{0301}".repeat(20_000));
        let body = serde_json::json!({
            "rejected": 1,
            "results": [{"ok": false, "error": &mixed}],
        });
        // Honesty check on the "reachable through the real 64 KiB body
        // cap" claim: this is ONE response that fits under it.
        assert!(
            body.to_string().len() < 64 * 1024,
            "the proof body must fit the real 64 KiB read cap to be reachable: {} bytes",
            body.to_string().len()
        );

        let reasons = extract_rejection_reasons(&body);
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        let out = &reasons[0];
        let out_chars = out.chars().count();
        assert!(
            out_chars <= char_ceiling,
            "{out_chars} chars survived a {char_ceiling}-char ceiling (in: {} chars)",
            mixed.chars().count()
        );
        // Bytes follow from chars, but assert them too — the finding was
        // reported in bytes, and `char`s alone would let a
        // 4-byte-per-char payload back in at 4x this size.
        assert!(out.len() <= char_ceiling * 4, "{} bytes survived (ceiling {} bytes)", out.len(), char_ceiling * 4);

        // And the ceiling must hold through the RENDER path, which
        // bounds a second time after escaping.
        let rendered = format_rejection_reasons_for_display(&reasons);
        assert!(
            rendered.chars().count() <= char_ceiling + REJECTION_REASON_QUOTE_OVERHEAD,
            "rendered row is {} chars",
            rendered.chars().count()
        );

        // The all-combining variant the finding also measured
        // (`in_chars=20000 out_chars=20000`) is now dropped OUTRIGHT
        // rather than merely bounded: every character is zero-width, so
        // `strip_leading_zero_width` empties it and
        // `extract_rejection_reasons`'s empty filter discards it. Pinned
        // so a later change that stops dropping it has to re-argue the
        // ceiling for that shape too.
        let pure: String = "\u{0301}".repeat(20_000);
        let dropped = extract_rejection_reasons(&serde_json::json!({
            "rejected": 1,
            "results": [{"ok": false, "error": pure}],
        }));
        assert!(dropped.is_empty(), "an all-zero-width reason is not a reason: {dropped:?}");

        // Positive control: the ceiling must not be reachable by any
        // realistic reason, or a "passing" test here would just mean the
        // bound eats real disclosure.
        let realistic = "payload field \"file\" must be a non-empty string";
        let kept = extract_rejection_reasons(&serde_json::json!({
            "rejected": 1,
            "results": [{"ok": false, "error": realistic}],
        }));
        assert_eq!(kept, vec![realistic.to_string()], "a realistic reason must survive whole: {kept:?}");
    }

    /// (#2196 fix-round 3, CONSIDER) A reason beginning with a
    /// zero-width combining mark lands that mark on darkmux's OWN opening
    /// quote — the single character carrying the attribution that
    /// everything after it is the receiver's words. `collapse_whitespace_and_trim`
    /// trims whitespace, and a combining mark is not whitespace, so it
    /// survived to the front of the string and rendered the quote struck
    /// through (U+0336 COMBINING LONG STROKE OVERLAY) or circled (U+20DD
    /// COMBINING ENCLOSING CIRCLE).
    ///
    /// Red-proves by name: delete `strip_leading_zero_width`'s call in
    /// `sanitize_reason_text` and the first assertion fails — the
    /// rendered row's second character is the mark, not `r`. The
    /// interior-mark case is the inverted control: legitimate decomposed
    /// prose (`café` as `e` + U+0301) must be untouched.
    #[test]
    fn a_leading_zero_width_mark_never_lands_on_the_attribution_quote() {
        for mark in ['\u{0336}', '\u{20DD}', '\u{0301}'] {
            let reason = format!("{mark}rule must be a string");
            let rendered = format_rejection_reasons_for_display(&[reason]);
            assert_eq!(
                rendered, "\"rule must be a string\"",
                "U+{:04X} must not survive in front of the opening quote: {rendered:?}",
                mark as u32
            );
        }

        // Inverted case: the same mark INSIDE the text is ordinary prose
        // and must survive — this is what stops the fix from being
        // "strip all combining marks".
        let decomposed = "cafe\u{0301} is not a valid value";
        let rendered = format_rejection_reasons_for_display(&[decomposed.to_string()]);
        assert_eq!(rendered, format!("\"{decomposed}\""), "{rendered:?}");
    }

    /// (#2196 fix-round 4) Every line this module writes to stderr,
    /// enumerated: **16 `eprintln!` sites, all FLUSH LEFT, every one
    /// beginning with the literal `flow::HookSink: `, and ZERO indented
    /// ones.** That inventory is what makes this surface's defense the
    /// simplest of the three — an indented line cannot be mistaken for a
    /// darkmux row whatever it says, so no vocabulary list is needed.
    ///
    /// The `{}`-bearing prefixes, verbatim, as the forgery payloads.
    const STDERR_ROWS: &[&str] = &[
        "flow::HookSink: receiver at ",
        "flow::HookSink: failed to persist ",
        "flow::HookSink: failed to write last-status ",
        "flow::HookSink: failed to write cursor-write status ",
        "flow::HookSink: failed to quarantine invalid outbox line into ",
        "flow::HookSink: failed to open quarantine file ",
        "flow::HookSink: outbox compaction failed for ",
        "flow::HookSink: try_post refusing to send — URL failed re-validation: ",
        "flow::HookSink: rule #0 failed to persist delivery cursor to ",
        "flow::HookSink: failed to emit hook.dry_run: ",
        "flow::HookSink: failed to emit hook.failed (dropped-append warning): ",
        "flow::HookSink: failed to emit hook.failed (busy warning): ",
        "flow::HookSink: file-transport write to ",
        "flow::HookSink: failed to check/fix trailing newline on ",
        "flow::HookSink: rule #0 disabled — its `transform` failed to load; the rest of the sink still works: ",
        "flow::HookSink: rule #0 outbox append failed: ",
    ];

    /// Simulate a terminal `width` columns wide wrapping `lines`, and
    /// return every VISUAL line that is a CONTINUATION.
    fn stderr_wrapped_continuations(lines: &[String], width: usize) -> Vec<String> {
        let mut out = Vec::new();
        for line in lines {
            let chars: Vec<char> = line.chars().collect();
            let mut start = width;
            while start < chars.len() {
                out.push(chars[start..].iter().take(width).collect::<String>());
                start += width;
            }
        }
        out
    }

    /// (#2196 fix-round 4, MUST FIX G at the stderr surface) The delivery
    /// warning used to carry the receiver's reason INLINE in one long
    /// line that nothing bounded — no `output_width()` cap, no wrap of
    /// its own — so it wrapped on EVERY terminal, not merely one narrower
    /// than a renderer assumed, and the continuation began at column 0
    /// where all 16 of this module's stderr rows live.
    ///
    /// SELF-PROVING, the same shape as the `flow status` and `doctor`
    /// tests: for each row and width it first SEARCHES for a filler that
    /// makes the forgery genuinely land at column 0 in the INLINE form,
    /// asserts at least one exists, then asserts the shipped builder
    /// produces no such continuation at any width.
    ///
    /// Red-proves by name: replace `receiver_rejection_stderr_lines`'s
    /// body with the pre-fix single interpolated line and the post-fix
    /// assertion fails on every pair the precondition found.
    #[test]
    fn stderr_rejection_warning_cannot_forge_a_flow_hooksink_row() {
        let widths = [60usize, 72, 80, 100, 120];
        let url = "http://127.0.0.1:8790/events";
        let mut proven: Vec<(String, usize, usize)> = Vec::new();

        // The pre-fix shape, reconstructed from the shipped header so it
        // stays correct as the surrounding wording evolves.
        let inline = |reason: &String| -> Vec<String> {
            let header = receiver_rejection_stderr_lines(url, 1, 1, &[]).remove(0);
            vec![format!("{header} — {}", format_rejection_reasons_for_display(std::slice::from_ref(reason)))]
        };

        for row in STDERR_ROWS {
            // Matched on the TRIMMED prefix: `collapse_whitespace_and_trim`
            // strips a reason's trailing space, so a payload ending in one
            // never survives verbatim. Trimming here keeps the forgery
            // honest — the row is still recognizable without its trailing
            // space, and demanding the space would have made every pair
            // look unforgeable for a reason that has nothing to do with
            // the defense.
            let row = row.trim_end();
            for width in widths {
                let max_filler = MAX_REJECTION_REASON_DISPLAY_WIDTH.saturating_sub(row.chars().count() + 3);
                for filler in 0..=max_filler {
                    let reason = format!("{} {row}", "z".repeat(filler));
                    if stderr_wrapped_continuations(&inline(&reason), width).iter().any(|c| c.starts_with(row)) {
                        proven.push((row.to_string(), width, filler));
                        break;
                    }
                }
            }
        }

        assert!(
            !proven.is_empty(),
            "the precondition found no forgeable (row, width, filler) at all — this test would prove nothing"
        );
        println!("stderr inline forgeries proven (row, terminal width, filler): {} pairs", proven.len());

        for (row, _width, filler) in &proven {
            let reason = format!("{} {row}", "z".repeat(*filler));
            let lines = receiver_rejection_stderr_lines(url, 1, 1, std::slice::from_ref(&reason));
            for w in widths {
                for continuation in stderr_wrapped_continuations(&lines, w) {
                    for candidate in STDERR_ROWS {
                        assert!(
                            !continuation.starts_with(candidate.trim_end()),
                            "width {w}: a wrapped continuation forges the stderr row {candidate:?} \
                             (row {row:?}, filler {filler}): {continuation:?}"
                        );
                    }
                }
            }
        }
    }

    /// (#2196 fix-round 4, the mechanism asserted independently of any
    /// forged vocabulary) Every stderr line carrying receiver text is
    /// indented and strictly under the narrowest supported terminal
    /// width; the HEADER line, which is darkmux's own words only, is the
    /// inverted case — it stays flush-left, as all 16 rows here do.
    ///
    /// Red-proves by name: revert `receiver_rejection_stderr_lines` to
    /// the single interpolated line and the indent assertion fails.
    #[test]
    fn every_stderr_reason_line_is_indented_and_narrower_than_the_supported_width() {
        let url = "http://127.0.0.1:8790/events";
        for reason in [
            "q".repeat(400),
            "漢".repeat(200),
            "payload field \"file\" must be a non-empty string".to_string(),
        ] {
            let lines = receiver_rejection_stderr_lines(url, 1, 1, std::slice::from_ref(&reason));
            assert!(
                lines[0].starts_with("flow::HookSink: receiver at "),
                "the header must stay flush-left and keep its row vocabulary: {:?}",
                lines[0]
            );
            assert!(lines.len() > 1, "the reason must actually produce its own line(s): {lines:?}");
            for line in &lines[1..] {
                assert!(line.starts_with("        "), "stderr reason line is not indented: {line:?}");
                let w = display_columns(line);
                assert!(
                    w < REJECTION_REASON_MIN_TERMINAL_WIDTH,
                    "stderr reason line is {w} columns, must stay under {}: {line:?}",
                    REJECTION_REASON_MIN_TERMINAL_WIDTH
                );
            }
        }

        // Inverted case: no reasons means no extra lines at all, so a
        // rejection the receiver gave no reason for still prints exactly
        // the one warning line it always did.
        assert_eq!(receiver_rejection_stderr_lines(url, 1, 1, &[]).len(), 1);
    }

    /// (#2196 fix-round MUST FIX 3) A receiver answering `"rejected": 0`
    /// EXPLICITLY (as opposed to omitting the key entirely, which
    /// `hook_fired_stays_clean_when_results_reject_without_a_top_level_rejected_count`
    /// above covers) must be treated the same as "no count to disclose".
    /// `receiver_rejected.filter(|n| *n > 0)` at the
    /// `DeliveryOutcome::Success` call site already does this correctly
    /// — this test is what was missing, not a code change. Red-proved:
    /// deleting that `.filter(|n| *n > 0)` reaches the identical failure
    /// shape this PR wrote the sibling test above for: `hook.fired` at
    /// Info carrying a reason with a zero count, plus the stderr line
    /// `"rejected 0 record(s) inside it (0 so far)"`.
    #[test]
    fn hook_fired_stays_clean_when_top_level_rejected_is_explicitly_zero() {
        let tmp = tempfile::TempDir::new().unwrap();
        let receiver = HookReceiver::start().with_response_body(
            r#"{"ok":true,"rejected":0,"results":[{"ok":false,"error":"ZERO-COUNT-LEAK"}]}"#,
        );
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
        let m = HookMatch { action: Some("crawl.*".to_string()), ..Default::default() };
        let key = rule_key(&m, &receiver.url("/events"));
        let last_path = last_status_path(tmp.path(), &key);
        sink.write(&record("crawl.finding")).unwrap();
        assert!(
            wait_until(|| read_last_status(&last_path).is_some(), Duration::from_secs(3)),
            "the delivery must land"
        );
        assert!(
            wait_until(
                || capture.0.lock().unwrap().iter().any(|r| r.action == "hook.fired"),
                Duration::from_secs(3)
            ),
            "hook.fired must land"
        );
        let fired = capture.0.lock().unwrap().iter().find(|r| r.action == "hook.fired").cloned().unwrap();
        assert!(
            fired.payload.as_ref().and_then(|p| p.get("receiver_rejected")).is_none(),
            "rejected: 0 is not a count to disclose: {fired:?}"
        );
        assert!(
            fired.payload.as_ref().and_then(|p| p.get("receiver_rejected_reasons")).is_none(),
            "a reason must never ride when the receiver's own count was zero: {fired:?}"
        );
        assert_eq!(level_wire(fired.level), "info", "{fired:?}");

        let last = read_last_status(&last_path).unwrap();
        assert_eq!(last.last_receiver_rejected, None, "{last:?}");
        assert!(last.last_receiver_rejected_reasons.is_empty(), "{last:?}");

        let summary = summarize_configured_rules(&rules, tmp.path()).remove(0);
        assert_eq!(summary.receiver_rejected_total, 0, "{summary:?}");
        drop(sink);
    }

    /// (#2196 fix-round MUST FIX 4) `extract_rejection_reasons` filters
    /// on `"ok": false` explicitly — an entry marked `"ok": true` must
    /// never contribute its `error` text, even when one happens to carry
    /// one. Every existing fixture's `ok: true` entries carry no `error`
    /// key at all, so `filter_map` swallows them for a reason unrelated
    /// to the `ok` filter — this is the case that actually exercises it.
    /// Red-proved: deleting
    /// `.filter(|e| e.get("ok").and_then(as_bool) == Some(false))`
    /// reports BOTH strings below instead of just the real one.
    #[test]
    fn extract_rejection_reasons_never_reports_an_accepted_records_text() {
        let body = serde_json::json!({
            "rejected": 1,
            "results": [
                {"ok": true, "error": "LEAKED-FROM-AN-ACCEPTED-RECORD"},
                {"ok": false, "error": "the real rejection"},
            ]
        });
        let reasons = extract_rejection_reasons(&body);
        assert_eq!(reasons, vec!["the real rejection".to_string()], "{reasons:?}");
    }

    /// (#2196 fix-round MUST FIX 5) A reason that sanitizes to empty — an
    /// empty string, or one made of nothing but control/bidi/whitespace
    /// characters `truncate_reason` strips — must never appear in the
    /// returned vector as `""`. Letting it through produced `"... on the
    /// last delivery ()"` in `doctor` and a bare, contentless "last
    /// rejection reason(s): " line in `flow status`, leaving an operator
    /// unable to tell whether the receiver gave no reason at all or
    /// darkmux lost one it was given.
    #[test]
    fn extract_rejection_reasons_drops_reasons_that_sanitize_to_empty() {
        let body = serde_json::json!({
            "rejected": 2,
            "results": [
                {"ok": false, "error": ""},
                {"ok": false, "error": "\u{0}\u{1}\u{202e}"},
                {"ok": false, "error": "real reason"},
            ]
        });
        let reasons = extract_rejection_reasons(&body);
        assert_eq!(reasons, vec!["real reason".to_string()], "{reasons:?}");
    }

    /// (#2196 fix-round 2, CONSIDER) The empty-after-sanitize filter must
    /// run BEFORE `MAX_REJECTION_REASONS`'s `.take`, not after. With more
    /// blank entries ahead of the real ones than the cap allows, a
    /// filter running AFTER `.take` consumes the entire cap on blanks and
    /// then drops every one of them, yielding `reasons: []` — count
    /// disclosed, reason silently gone. Red-proved: swapping the
    /// `.filter`/`.take` order in `extract_rejection_reasons` back to
    /// take-then-filter reproduces exactly that on this fixture.
    #[test]
    fn extract_rejection_reasons_does_not_let_leading_blanks_consume_the_cap() {
        let body = serde_json::json!({
            "rejected": 6,
            "results": [
                {"ok": false, "error": ""},
                {"ok": false, "error": "\u{0}"},
                {"ok": false, "error": "\u{202e}"},
                {"ok": false, "error": "\u{2800}"},
                {"ok": false, "error": "real reason A"},
                {"ok": false, "error": "real reason B"},
            ]
        });
        let reasons = extract_rejection_reasons(&body);
        assert_eq!(
            reasons,
            vec!["real reason A".to_string(), "real reason B".to_string()],
            "leading blanks must not consume the cap and silently erase the real reasons: {reasons:?}"
        );
    }

    /// (#2196 fix-round MUST FIX 1, end-to-end) The unit test above pins
    /// the helper directly; this proves the sanitization actually rides
    /// the real delivery pipeline — through a live loopback receiver,
    /// into the `hook.fired` payload AND the `.last` sidecar — not just
    /// the function in isolation.
    #[test]
    fn hook_fired_and_sidecar_never_carry_control_characters_from_the_receiver() {
        let tmp = tempfile::TempDir::new().unwrap();
        let poisoned = "ok\r\n\x1b[31mFATAL: darkmux is corrupt\x1b[0m\nquarantined lines: 0";
        let receiver = HookReceiver::start().with_response_body(&format!(
            r#"{{"ok":true,"accepted":0,"rejected":1,"results":[{{"ok":false,"error":{}}}]}}"#,
            serde_json::Value::String(poisoned.to_string())
        ));
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
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
        let m = HookMatch { action: Some("crawl.*".to_string()), ..Default::default() };
        let key = rule_key(&m, &receiver.url("/events"));
        let last_path = last_status_path(tmp.path(), &key);
        sink.write(&record("crawl.finding")).unwrap();
        assert!(wait_until(|| read_last_status(&last_path).is_some(), Duration::from_secs(3)));
        assert!(wait_until(
            || capture.0.lock().unwrap().iter().any(|r| r.action == "hook.fired"),
            Duration::from_secs(3)
        ));
        let fired = capture.0.lock().unwrap().iter().find(|r| r.action == "hook.fired").cloned().unwrap();
        let reasons: Vec<String> = fired
            .payload
            .as_ref()
            .and_then(|p| p.get("receiver_rejected_reasons"))
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        assert_eq!(reasons.len(), 1, "{fired:?}");
        assert!(!reasons[0].contains('\n'), "{reasons:?}");
        assert!(!reasons[0].contains('\r'), "{reasons:?}");
        assert!(!reasons[0].contains('\x1b'), "{reasons:?}");

        let last = read_last_status(&last_path).unwrap();
        assert_eq!(last.last_receiver_rejected_reasons.len(), 1, "{last:?}");
        assert!(!last.last_receiver_rejected_reasons[0].contains('\n'), "{last:?}");
        assert!(!last.last_receiver_rejected_reasons[0].contains('\r'), "{last:?}");
        assert!(!last.last_receiver_rejected_reasons[0].contains('\x1b'), "{last:?}");
        drop(sink);
    }

    /// (#2196) Unit-level pin on the extraction + bounding helpers, so the
    /// cap and the truncation are each independently provable without a
    /// live receiver round-trip.
    #[test]
    fn extract_rejection_reasons_is_bounded_and_truncates_long_entries() {
        let long = "x".repeat(500);
        let body = serde_json::json!({
            "rejected": 5,
            "results": [
                {"ok": false, "error": "r1"},
                {"ok": true},
                {"ok": false, "error": "r2"},
                {"ok": false, "error": &long},
                {"ok": false, "error": "r4 — never reached, the cap is 3"},
            ]
        });
        let reasons = extract_rejection_reasons(&body);
        assert_eq!(reasons.len(), MAX_REJECTION_REASONS, "{reasons:?}");
        assert_eq!(reasons[0], "r1");
        assert_eq!(reasons[1], "r2");
        // (#2196 fix-round 2, MUST FIX C) `bound_reason_width` keeps the
        // total (head + `…` + tail) AT the budget, never over it — unlike
        // the first fix-round's straight cut, which appended the
        // ellipsis ON TOP of the budget (budget + 1). A middle cut on an
        // all-'x' string also always leaves a non-empty tail after the
        // ellipsis (the tail is real content here, not just a cosmetic
        // marker), so this also pins that both halves survive.
        assert_eq!(
            reasons[2].chars().count(),
            REJECTION_REASON_RAW_BUDGET,
            "must truncate to the raw budget, ellipsis included: {reasons:?}"
        );
        assert!(reasons[2].contains('…'), "{reasons:?}");
        assert!(!reasons[2].ends_with('…'), "a middle cut must leave a real tail after the ellipsis: {reasons:?}");
    }

    // ─── (#2183) jq transforms + Keychain headers + the `file` transport ──

    /// Point `hooks_adapters_dir()` at a fresh tempdir (via `DARKMUX_HOME`)
    /// and write one adapter file into it. Returns the tempdir — kept
    /// alive by the caller for the test's duration — and the outbox dir a
    /// `HookSink` in the SAME test should use (`<tmp>/hooks`, matching
    /// what a real deployment resolves both dirs from).
    fn with_adapter(name: &str, source: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()) };
        let adapters_dir = darkmux_types::config_access::hooks_adapters_dir();
        fs::create_dir_all(&adapters_dir).unwrap();
        fs::write(adapters_dir.join(name), source).unwrap();
        let outbox_dir = darkmux_types::config_access::hooks_outbox_dir();
        (tmp, outbox_dir)
    }

    fn clear_darkmux_home() {
        unsafe { std::env::remove_var("DARKMUX_HOME") };
    }

    #[test]
    #[serial_test::serial]
    fn transform_absent_delivers_record_verbatim_byte_identical_to_today() {
        let receiver = HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            ..Default::default()
        }];
        let tmp = tempfile::TempDir::new().unwrap();
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), Arc::new(NoopSink)).unwrap();
        let mut rec = record("dispatch.tool");
        rec.payload = Some(serde_json::json!({"tool_name": "create_finding"}));
        sink.write(&rec).unwrap();
        assert!(wait_until(|| receiver.request_count() >= 1, Duration::from_secs(3)));
        drop(sink);
        let bodies = receiver.bodies();
        let delivered: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
        // Byte-identical to the record's own JSON shape — no transform
        // ran, so the wire body IS the record, verbatim.
        assert_eq!(delivered["action"], "dispatch.tool");
        assert_eq!(delivered["payload"]["tool_name"], "create_finding");
    }

    #[test]
    #[serial_test::serial]
    fn transform_applied_delivers_transformed_body() {
        let (_tmp, outbox_dir) = with_adapter("shape.jq", r#"{summary: .payload.tool_name}"#);
        let receiver = HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            transform: Some("shape.jq".to_string()),
            ..Default::default()
        }];
        let sink = HookSink::new(&rules, outbox_dir, Arc::new(NoopSink)).unwrap();
        let mut rec = record("dispatch.tool");
        rec.payload = Some(serde_json::json!({"tool_name": "create_finding"}));
        sink.write(&rec).unwrap();
        assert!(wait_until(|| receiver.request_count() >= 1, Duration::from_secs(3)));
        drop(sink);
        clear_darkmux_home();
        let bodies = receiver.bodies();
        let delivered: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
        assert_eq!(delivered, serde_json::json!({"summary": "create_finding"}), "transformed body, not the raw record");
    }

    #[test]
    #[serial_test::serial]
    fn transform_jq_error_quarantines_and_never_retries_forever() {
        let (_tmp, outbox_dir) = with_adapter("boom.jq", r#"error("adapter boom")"#);
        let receiver = HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            transform: Some("boom.jq".to_string()),
            ..Default::default()
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
        let sink = HookSink::new(&rules, outbox_dir, report).unwrap();
        sink.write(&record("dispatch.tool")).unwrap();
        assert!(wait_until(
            || capture.0.lock().unwrap().iter().any(|r| r.action == "hook.failed"),
            Duration::from_secs(3)
        ));
        // Give the drainer several extra poll cycles — a bug that
        // re-queues the SAME line would keep re-emitting hook.failed or
        // (worse) eventually reach the receiver.
        std::thread::sleep(Duration::from_millis(300));
        drop(sink);
        clear_darkmux_home();
        assert_eq!(receiver.request_count(), 0, "a jq error must never reach the network");
        let failed: Vec<_> = capture.0.lock().unwrap().iter().filter(|r| r.action == "hook.failed").cloned().collect();
        assert_eq!(failed.len(), 1, "quarantined once, never retried: {failed:?}");
        let err = failed[0].payload.as_ref().and_then(|p| p.get("error")).and_then(|v| v.as_str()).unwrap_or("");
        assert!(err.contains("adapter boom"), "{err}");
    }

    #[test]
    #[serial_test::serial]
    fn transform_never_sees_a_keychain_resolved_header_value() {
        // The transform's ONLY input is the raw record line — headers are
        // resolved on a completely separate path (`build_delivery_headers`,
        // called AFTER `apply_transform` in the drain loop; see hooks.rs's
        // drainer_loop). Prove it structurally with a real end-to-end
        // delivery: configure a `headers` entry carrying a distinctive
        // sentinel, point `transform` at an adapter that dumps its ENTIRE
        // input verbatim, and confirm the sentinel reaches the wire as a
        // header while never appearing in the transformed body. A literal
        // header resolves through the exact same `RawHookSecret` wrapper +
        // the exact same `build_delivery_headers` call a Keychain-resolved
        // one does (`resolve_hook_header_value`'s two branches both return
        // `RawHookSecret`) — the transform's call site never branches on
        // where a header's value came from, so this proves separation for
        // both without a unit test needing real macOS Keychain access.
        let (_tmp, outbox_dir) = with_adapter("echo.jq", r#"{dump: (. | tostring)}"#);
        let receiver = HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            transform: Some("echo.jq".to_string()),
            headers: Some({
                let mut m = BTreeMap::new();
                m.insert(
                    "Authorization".to_string(),
                    darkmux_types::config::HeaderValue::Literal("TOTALLY-SECRET-SHOULD-NEVER-LEAK".to_string()),
                );
                m
            }),
            ..Default::default()
        }];
        let sink = HookSink::new(&rules, outbox_dir, Arc::new(NoopSink)).unwrap();
        let mut rec = record("dispatch.tool");
        rec.payload = Some(serde_json::json!({"tool_name": "create_finding"}));
        sink.write(&rec).unwrap();
        assert!(wait_until(|| receiver.request_count() >= 1, Duration::from_secs(3)));
        drop(sink);
        clear_darkmux_home();
        let bodies = receiver.bodies();
        assert!(!bodies[0].contains("TOTALLY-SECRET"), "the transform's output must never carry the header value: {}", bodies[0]);
        let headers = receiver.headers();
        assert_eq!(
            headers[0].get("authorization").map(String::as_str),
            Some("TOTALLY-SECRET-SHOULD-NEVER-LEAK"),
            "the header value DOES reach the wire — just never through the transform's input"
        );
    }

    #[test]
    #[serial_test::serial]
    fn adapter_name_with_dotdot_or_absolute_refused_at_load() {
        let tmp = tempfile::TempDir::new().unwrap();
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()) };
        let outbox_dir = darkmux_types::config_access::hooks_outbox_dir();
        for bad_name in ["../escape.jq", "/etc/passwd", "sub/dir.jq"] {
            let rules = vec![HookRule {
                r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
                http: Some("http://127.0.0.1:1/events".to_string()),
                transform: Some(bad_name.to_string()),
                ..Default::default()
            }];
            let err = resolve_rules(&rules, &outbox_dir).unwrap_err();
            assert!(format!("{err:#}").contains("adapter"), "{bad_name}: {err:#}");
        }
        clear_darkmux_home();
    }

    #[test]
    #[serial_test::serial]
    fn missing_adapter_disables_only_that_rule_the_rest_of_the_sink_still_works() {
        let tmp = tempfile::TempDir::new().unwrap();
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()) };
        let outbox_dir = darkmux_types::config_access::hooks_outbox_dir();
        let receiver = HookReceiver::start();
        let rules = vec![
            HookRule {
                r#match: Some(HookMatch { action: Some("broken.*".to_string()), ..Default::default() }),
                http: Some(receiver.url("/events")),
                transform: Some("does-not-exist.jq".to_string()),
                ..Default::default()
            },
            HookRule {
                r#match: Some(HookMatch { action: Some("fine.*".to_string()), ..Default::default() }),
                http: Some(receiver.url("/events")),
                ..Default::default()
            },
        ];
        // Construction must NOT fail — the bad-adapter rule is dropped,
        // the healthy rule still runs.
        let sink = HookSink::new(&rules, outbox_dir, Arc::new(NoopSink)).unwrap();
        sink.write(&record("fine.thing")).unwrap();
        assert!(wait_until(|| receiver.request_count() >= 1, Duration::from_secs(3)));
        drop(sink);
        clear_darkmux_home();
    }

    #[test]
    #[serial_test::serial]
    fn file_transport_writes_body_and_redacted_headers_and_emits_dry_run() {
        let (_tmp, outbox_dir) = with_adapter("shape.jq", r#"{summary: .payload.tool_name}"#);
        let dryrun_dir = outbox_dir.join("dryrun-out");
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            file: Some(dryrun_dir.to_string_lossy().into_owned()),
            transform: Some("shape.jq".to_string()),
            headers: Some({
                let mut m = BTreeMap::new();
                m.insert("X-Literal".to_string(), darkmux_types::config::HeaderValue::Literal("plain".to_string()));
                m.insert(
                    "Authorization".to_string(),
                    darkmux_types::config::HeaderValue::Keychain { keychain_item: "darkmux-hook-test-nonexistent".to_string() },
                );
                m
            }),
            ..Default::default()
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
        let sink = HookSink::new(&rules, outbox_dir, report).unwrap();
        let mut rec = record("dispatch.tool");
        rec.payload = Some(serde_json::json!({"tool_name": "create_finding"}));
        sink.write(&rec).unwrap();
        assert!(wait_until(
            || capture.0.lock().unwrap().iter().any(|r| r.action == "hook.dry_run"),
            Duration::from_secs(3)
        ));
        drop(sink);
        clear_darkmux_home();
        let entries: Vec<_> = fs::read_dir(&dryrun_dir).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(entries.len(), 1, "one dump file per delivery");
        let dump: serde_json::Value = serde_json::from_str(&fs::read_to_string(entries[0].path()).unwrap()).unwrap();
        assert_eq!(dump["body"], "{\"summary\":\"create_finding\"}");
        assert_eq!(dump["headers"]["X-Literal"], "plain", "a literal header is NOT redacted");
        assert_eq!(dump["headers"]["Authorization"], "<redacted>", "a Keychain-referenced header IS redacted");
        assert!(dump["delivery_id"].is_string());
        assert!(dump["target_would_be"].as_str().unwrap().starts_with("file://"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(entries[0].path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "dry-run dump must be owner-only");
        }
    }

    /// (security review, 2026-08-31, MUST FIX 3) Pins the doc/PR-body
    /// example's own honesty: a `headers` + `transform` rule targeting a
    /// real external SaaS endpoint over `https` is STILL refused at load
    /// — this packet did not touch `validate_hook_target_url`'s policy
    /// (loopback or tailnet, `http://` only). A copy-pasted doc example
    /// using `https://<site>.atlassian.net/...` must fail exactly like
    /// this, not silently construct a broken sink.
    #[test]
    fn https_saas_target_still_refused_even_with_headers_and_transform() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some("https://your-site.atlassian.net/rest/api/3/issue".to_string()),
            headers: Some({
                let mut m = BTreeMap::new();
                m.insert(
                    "Authorization".to_string(),
                    darkmux_types::config::HeaderValue::Keychain { keychain_item: "darkmux-hook-jira".to_string() },
                );
                m
            }),
            transform: Some("jira-issue.jq".to_string()),
            ..Default::default()
        }];
        let err = resolve_rules(&rules, tmp.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("neither a loopback target"),
            "an https SaaS target must still be refused by the unchanged URL policy: {err:#}"
        );
    }

    #[test]
    fn file_and_http_together_or_neither_is_refused_at_load() {
        let tmp = tempfile::TempDir::new().unwrap();
        let both = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some("http://127.0.0.1:1/events".to_string()),
            file: Some("/tmp/somewhere".to_string()),
            ..Default::default()
        }];
        assert!(resolve_rules(&both, tmp.path()).is_err());
        let neither = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            ..Default::default()
        }];
        assert!(resolve_rules(&neither, tmp.path()).is_err());
    }

    #[test]
    #[serial_test::serial]
    fn attribution_headers_false_drops_x_darkmux_headers() {
        let receiver = HookReceiver::start();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some(receiver.url("/events")),
            attribution_headers: Some(false),
            ..Default::default()
        }];
        let tmp = tempfile::TempDir::new().unwrap();
        let sink = HookSink::new(&rules, tmp.path().to_path_buf(), Arc::new(NoopSink)).unwrap();
        sink.write(&record("dispatch.tool")).unwrap();
        assert!(wait_until(|| receiver.request_count() >= 1, Duration::from_secs(3)));
        drop(sink);
        let headers = receiver.headers();
        assert!(!headers[0].contains_key("x-darkmux-delivery"), "{:?}", headers[0]);
        assert!(!headers[0].contains_key("x-darkmux-signature"), "{:?}", headers[0]);
    }

    #[test]
    // (#2643) `orphan_backlog_stalls_the_rule_and_warns_instead_of_going_
    // silent` below mutates `DARKMUX_HOOKS_JQ_TIMEOUT_MS` — reproduced
    // directly: this test failed (`left: 100, right: 5_000`) when the two
    // landed in the same window under the default parallel test harness.
    // Genuinely nothing to restructure here (it asserts the DEFAULT, so
    // there's no override to inject in place of the env mutation the way
    // the outbox-cap tests got); `#[serial]` is the right, cheap fix.
    #[serial_test::serial]
    fn wall_clock_and_output_caps_are_wired_from_config_defaults() {
        // The knobs themselves (`apply_transform`'s enforcement) are
        // exhaustively covered in `hook_transform`'s own tests; this just
        // proves the accessor defaults this module reads at drain time.
        assert_eq!(darkmux_types::config_access::hooks_jq_timeout_ms(), 5_000);
        assert_eq!(darkmux_types::config_access::hooks_jq_max_output_bytes(), 1_048_576);
    }

    /// (security review round 2, 2026-08-31, MUST FIX a+b) The bug this
    /// pins: a rule whose orphan cap fills with GENUINELY non-terminating
    /// evaluations (the canonical `def rec: rec; rec`, which by
    /// construction never sends on its channel) used to back off SILENTLY
    /// forever — no `hook.failed`, no status write, `stalled` never set —
    /// because the orphan-decrementing side only runs when an orphaned
    /// thread FINISHES, and this class of orphan never does. Proves all
    /// three parts of the fix: (a) a rate-limited `hook.failed` names the
    /// backlog, (b) `MAX_CONSECUTIVE_BUSY_BEFORE_STALL` consecutive
    /// `Busy` outcomes promote the rule into `stalled` (visible via
    /// `summarize_configured_rules`, the same surface `doctor` reads),
    /// and — the reviewer's own falsifying assertion — the orphan counter
    /// is STILL PINNED at the cap well after the timeouts that produced
    /// it have elapsed, because those threads never finish.
    #[test]
    #[serial_test::serial]
    fn orphan_backlog_stalls_the_rule_and_warns_instead_of_going_silent() {
        let prev_timeout = std::env::var("DARKMUX_HOOKS_JQ_TIMEOUT_MS").ok();
        unsafe { std::env::set_var("DARKMUX_HOOKS_JQ_TIMEOUT_MS", "100") };
        let (_tmp, outbox_dir) = with_adapter("loops.jq", "def rec: rec; rec");
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
            http: Some("http://127.0.0.1:1/unused".to_string()),
            transform: Some("loops.jq".to_string()),
            ..Default::default()
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
        let sink = HookSink::new(&rules, outbox_dir.clone(), report).unwrap();
        // Five records: the first two each spawn a genuinely
        // non-terminating evaluation, time out individually (Error,
        // quarantined — that's `hook_transform`'s own terminal-failure
        // contract, unrelated to this test), and pin the orphan counter
        // at the cap (2). Every record after that returns `Busy`
        // immediately (never spawns), and the SAME undelivered line is
        // what accumulates `consecutive_busy` toward the stall threshold.
        for _ in 0..5 {
            sink.write(&record("dispatch.tool")).unwrap();
        }
        // (a) A rate-limited hook.failed names the backlog — fires on
        // the FIRST Busy outcome (no prior warning to rate-limit against).
        assert!(
            wait_until(
                || capture
                    .0
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|r| r.action == "hook.failed"
                        && r.payload.as_ref().and_then(|p| p.get("error")).and_then(|v| v.as_str())
                            .is_some_and(|e| e.contains("transform backlogged"))),
                Duration::from_secs(5)
            ),
            "expected a hook.failed naming the transform backlog"
        );
        // (b) MAX_CONSECUTIVE_BUSY_BEFORE_STALL consecutive Busy outcomes
        // promote the rule to `stalled`, visible on the SAME read-only
        // surface `doctor`/`flow status` use.
        assert!(
            wait_until(
                || summarize_configured_rules(&rules, &outbox_dir).first().is_some_and(|s| s.stalled),
                Duration::from_secs(10)
            ),
            "expected the rule to be promoted to `stalled` after repeated Busy outcomes"
        );
        // The falsifying assertion: wait well past the point any of this
        // round's timeouts could still be "about to finish," then confirm
        // the orphan counter has NOT drained — the two `def rec: rec;
        // rec` threads are still running (they never stop), so "until an
        // orphan finishes and decrements" was never going to happen for
        // this rule.
        std::thread::sleep(Duration::from_secs(3));
        assert_eq!(
            sink.rules[0].orphaned_transforms.load(Ordering::Acquire),
            crate::hook_transform::MAX_ORPHANED_TRANSFORM_THREADS_PER_RULE,
            "the orphan counter must still be pinned at the cap — these threads never finish"
        );
        drop(sink);
        clear_darkmux_home();
        unsafe {
            match prev_timeout {
                Some(v) => std::env::set_var("DARKMUX_HOOKS_JQ_TIMEOUT_MS", v),
                None => std::env::remove_var("DARKMUX_HOOKS_JQ_TIMEOUT_MS"),
            }
        }
    }
}
