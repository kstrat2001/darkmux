//! The ONE owner of what a caller that is neither this machine nor a token
//! holder may not read (#3071 for the console panels, extended to every read
//! route). A daemon's reads are open on the tailnet by design, so a response is
//! filtered here before it leaves: roster addresses, tailnet names, private and
//! addressing IPv4 literals, IPv6 literals, the fleet hub's host, and home
//! directories are host facts that stay on this machine ([`redact_text`] is the
//! rule set, one pass per string).
//!
//! Two entry points, one rule set ([`Redaction`]):
//!
//! - [`redact_reads`] is the layer every JSON route carries. For a remote
//!   caller it streams the body through [`crate::redaction_stream`], redacting each
//!   key and value as it passes, so a field no one thought to list (an operator-authored profile description, a path in
//!   an error line) is covered without naming it.
//! - [`redact_panel_stdout`] is for a console panel's terminal output, which needs
//!   its escape sequences split out first (see [`classify_escape`]).
//!
//! The streams redact each event line with [`Redaction::line`]. Local callers
//! and token holders see every value unchanged
//! ([`crate::caller_is_local_or_holds_token`]).

use axum::extract::{ConnectInfo, Request};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use darkmux_types::url_authority::UrlAuthority;
use crate::redaction_stream::StreamRedactor;
use darkmux_types::panel_audience::Withheld;
use std::net::SocketAddr;

/// Stands in for an address in a remote caller's output.
pub(crate) const ADDRESS_HIDDEN: &str = "(address hidden)";
/// How long a derived [`Redaction`] is reused. The key below catches a roster or
/// directory change at once; this bounds staleness for what it does not cover
/// (the fleet hub in the config, the machine id, a symlink repointed).
const REDACTION_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(10);

/// What a non-local caller must not read in a response, derived from the
/// daemon's own state when the request is served: every roster ADDRESS (and its
/// host part) that is not itself a public machine name, the fleet hub's host,
/// and the daemon user's home and `DARKMUX_HOME` directories. IP literals, any
/// `.ts.net` name and any account's home are hidden whether or not the roster
/// lists them ([`redact_text`]). Machine ids and names are
/// public (a 400 for a bad opt lists them): only the address behind one is
/// private.
pub(crate) struct Redaction {
    /// Roster addresses (and their hosts, and the hub's), then directory
    /// prefixes, each longest first within its kind.
    needles: Vec<Needle>,
}

/// What a derivation read: `fleet.json`'s mtime and length, and the two
/// directory variables (#3073).
#[derive(Clone, PartialEq, Eq)]
struct CacheKey {
    roster: Option<(std::time::SystemTime, u64)>,
    home: Option<String>,
    darkmux_home: Option<String>,
}

impl CacheKey {
    fn current() -> Self {
        let roster = std::fs::metadata(darkmux_fleet::roster_path()).ok().and_then(|m| Some((m.modified().ok()?, m.len())));
        Self { roster, home: std::env::var("HOME").ok(), darkmux_home: std::env::var("DARKMUX_HOME").ok() }
    }
}

/// The last derivation, so a remote request does not reload the roster and
/// canonicalize the directories every time (#3073).
#[derive(Default)]
struct RedactionCache {
    entry: Option<(CacheKey, std::time::Instant, std::sync::Arc<Redaction>)>,
}

impl RedactionCache {
    fn get(&mut self, key: CacheKey, now: std::time::Instant, derive: impl FnOnce() -> Redaction) -> std::sync::Arc<Redaction> {
        if let Some((k, at, r)) = &self.entry {
            if *k == key && now.saturating_duration_since(*at) < REDACTION_CACHE_TTL {
                return r.clone();
            }
        }
        let r = std::sync::Arc::new(derive());
        self.entry = Some((key, now, r.clone()));
        r
    }
}

static REDACTION_CACHE: std::sync::Mutex<Option<RedactionCache>> = std::sync::Mutex::new(None);

impl Redaction {
    /// [`Self::derive`], reused while `fleet.json`, HOME and DARKMUX_HOME are unchanged.
    pub(crate) fn derive_cached() -> std::sync::Arc<Self> {
        let key = CacheKey::current();
        let mut guard = REDACTION_CACHE.lock().unwrap_or_else(|p| p.into_inner());
        guard.get_or_insert_with(RedactionCache::default).get(key, std::time::Instant::now(), Self::derive)
    }
}

/// One thing to find, lowercased once.
struct Needle {
    /// As given, for the tests' view of the set.
    #[cfg(test)]
    text: String,
    lower: String,
    with: &'static str,
    /// A single label with no dot and no port (`studio`): not a host fact as a
    /// free word, only where it addresses (`://studio`, `studio:8765`). A
    /// roster address that is a name like "LM Studio" must not hide it.
    bare: bool,
}

impl Needle {
    fn address(a: &str) -> Self {
        let bare = !a.contains(['.', ':']);
        Self { #[cfg(test)] text: a.to_string(), lower: a.to_ascii_lowercase(), with: ADDRESS_HIDDEN, bare }
    }
}

impl Redaction {
    /// The roster is read from disk and the directories from the environment
    /// per call (see [`Self::derive_cached`]); there is no list to maintain. An unreadable roster hides
    /// nothing it cannot name, and the caller still gets no `stderr_tail`.
    pub(crate) fn derive() -> Self {
        let roster = darkmux_fleet::load_roster().ok();
        let machines: Vec<(&str, &str)> =
            roster.iter().flat_map(|r| r.machines.values().map(|m| (m.id.as_str(), m.address.as_str()))).collect();
        let mut public: Vec<String> = Vec::new();
        for m in roster.iter().flat_map(|r| r.machines.values()) {
            public.extend(m.current_name.clone());
        }
        public.extend(darkmux_flow::resolve_machine_id());
        let hub = darkmux_flow::redis_url().and_then(|u| url_host(u.expose_for_probe()));
        let home = std::env::var("HOME").ok();
        let darkmux_home = std::env::var("DARKMUX_HOME").ok();
        let hub = hub.filter(|h| !public.iter().any(|p| p.eq_ignore_ascii_case(h)));
        Self::build(&machines, &public, hub, home, darkmux_home)
    }

    /// `machines` is `(id, address)`; `public` are further machine names.
    #[cfg(test)]
    pub(crate) fn from_parts(
        machines: &[(&str, &str)],
        public: &[String],
        home: Option<String>,
        darkmux_home: Option<String>,
    ) -> Self {
        Self::build(machines, public, None, home, darkmux_home)
    }

    /// [`Self::from_parts`] plus the fleet hub's host.
    fn build(
        machines: &[(&str, &str)],
        public: &[String],
        hub: Option<String>,
        home: Option<String>,
        darkmux_home: Option<String>,
    ) -> Self {
        let is_public = |s: &str| {
            machines.iter().any(|(id, _)| id.eq_ignore_ascii_case(s)) || public.iter().any(|p| p.eq_ignore_ascii_case(s))
        };
        let mut all: Vec<String> = Vec::new();
        for addr in machines.iter().map(|(_, a)| a.trim()).filter(|a| !a.is_empty()) {
            if !is_public(addr) {
                all.push(addr.to_string());
            }
            if let Some(host) = address_host(addr).filter(|h| !is_public(h)) {
                all.push(host.to_string());
            }
        }
        all.extend(hub);
        all.sort_by_key(|a| std::cmp::Reverse(a.len()));
        all.dedup();
        // A directory as given and as the filesystem resolves it (`/tmp` is
        // `/private/tmp` on macOS): a verb may print either.
        let forms = |d: Option<String>| -> Vec<String> {
            let given = d.map(|d| d.trim_end_matches('/').to_string()).filter(|d| d.len() > 1);
            let canon = given
                .as_deref()
                .and_then(|g| std::fs::canonicalize(g).ok())
                .map(|c| c.to_string_lossy().trim_end_matches('/').to_string())
                .filter(|c| c.len() > 1);
            let mut all: Vec<String> = given.into_iter().chain(canon).collect();
            all.dedup();
            all
        };
        let homes = forms(home);
        let mut dirs: Vec<(String, &'static str)> = homes.iter().cloned().map(|h| (h, "~")).collect();
        // A DARKMUX_HOME inside HOME (at a path boundary: `/Users/kainx/dm` is
        // not inside `/Users/kain`) is covered by the HOME rewrite.
        for dh in forms(darkmux_home) {
            let inside = homes.iter().any(|h| dh == *h || dh.starts_with(&format!("{h}/")));
            if !inside {
                dirs.push((dh, "$DARKMUX_HOME"));
            }
        }
        dirs.sort_by_key(|(d, _)| std::cmp::Reverse(d.len()));
        let mut seen = std::collections::HashSet::new();
        dirs.retain(|(d, _)| seen.insert(d.clone()));
        let needles = all
            .iter()
            .map(|a| Needle::address(a))
            .chain(dirs.into_iter().map(|(d, with)| Needle { lower: d.to_ascii_lowercase(), #[cfg(test)] text: d.clone(), with, bare: false }))
            .collect();
        Self { needles }
    }
}

/// The host of a roster address (`name:8765`, `[::1]:8765`, a bare IP or name).
fn address_host(addr: &str) -> Option<&str> {
    if let Some(rest) = addr.strip_prefix('[') {
        return rest.split_once(']').map(|(host, _)| host);
    }
    match addr.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.chars().all(|c| c.is_ascii_digit()) => Some(host),
        _ => None,
    }
}

/// A character that continues a host name or a path component: a match
/// touching one is part of a longer word (`mac` in `macos`, `studio` in
/// `lmstudio-community`, `/Users/kain` in `/Users/kainx`), never the needle.
fn word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Whether the text beginning with `rest` continues a word: a word character,
/// or a `.` that is followed by one (`.net` in `host.net`). A `.` that ends the
/// text or is followed by anything else is punctuation, a boundary.
fn continues_forward(rest: &str) -> bool {
    let mut it = rest.chars();
    match it.next() {
        Some('.') => it.next().is_some_and(word_char),
        Some(c) => word_char(c),
        None => false,
    }
}

/// The mirror of [`continues_forward`] for the text ending with `before`.
fn continues_backward(before: &str) -> bool {
    let mut it = before.chars().rev();
    match it.next() {
        Some('.') => it.next().is_some_and(word_char),
        Some(c) => word_char(c),
        None => false,
    }
}

/// The host of a URL (`scheme://user:pw@host:port/path`), without userinfo,
/// port or path. The host comes from the same authority parser the flow
/// redactor uses (#3074).
fn url_host(url: &str) -> Option<String> {
    let hostport = UrlAuthority::parse(url)?.hostport();
    let host = address_host(hostport).unwrap_or(hostport);
    (!host.is_empty()).then(|| host.to_string())
}

/// The start of each non-overlapping ASCII-case-insensitive occurrence of
/// `needle` (already lowercase) in `text`, found without copying the text.
fn find_ci(text: &str, needle: &str) -> Vec<usize> {
    let (t, n) = (text.as_bytes(), needle.as_bytes());
    let mut out = Vec::new();
    let Some(&first) = n.first() else { return out };
    let mut i = 0;
    while i + n.len() <= t.len() {
        if t[i].to_ascii_lowercase() == first && t[i..i + n.len()].eq_ignore_ascii_case(n) {
            out.push(i);
            i += n.len();
        } else {
            i += 1;
        }
    }
    out
}

/// One byte range of a text and what it reads as.
struct Span {
    start: usize,
    end: usize,
    with: &'static str,
}

/// A character of a word for the IP-literal rules: unlike [`word_char`],
/// `-` is a boundary (`ip-100.64.7.8`, `100.64.7.7-tail`).
fn word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Whether the text from `at` on continues a word: a word byte, or a `.`
/// followed by one (`.example` after `1.2.3.4`).
fn word_continues_at(b: &[u8], at: usize) -> bool {
    match b.get(at) {
        Some(b'.') => b.get(at + 1).is_some_and(|c| word_byte(*c)),
        Some(c) => word_byte(*c),
        None => false,
    }
}

/// An IPv4 literal's octets: exactly four numeric parts of at most three
/// digits, each at most 255.
fn parse_v4(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut parts = s.split('.');
    for slot in &mut out {
        let p = parts.next().filter(|p| !p.is_empty() && p.len() <= 3)?;
        *slot = p.parse().ok()?;
    }
    parts.next().is_none().then_some(out)
}

/// Whether an IPv4 literal at `start..end` is a host fact: a private or
/// tailnet address (10/8, 100.64/10, 172.16/12, 192.168/16, 169.254/16), or
/// one in a URL authority or `host:port` position. Loopback and `0.x` never.
fn v4_is_host_fact(o: [u8; 4], text: &str, start: usize, end: usize) -> bool {
    if o[0] == 127 || o[0] == 0 {
        return false;
    }
    let private = o[0] == 10
        || (o[0] == 100 && (64..=127).contains(&o[1]))
        || (o[0] == 172 && (16..=31).contains(&o[1]))
        || (o[0] == 192 && o[1] == 168)
        || (o[0] == 169 && o[1] == 254);
    private || text[..start].ends_with("://") || text[..start].ends_with('@') || followed_by_port(&text[end..])
}

/// Whether the text begins `:<digit>`: a `host:port` position.
fn followed_by_port(rest: &str) -> bool {
    rest.strip_prefix(':').is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
}

fn ipv4_spans(text: &str, out: &mut Vec<Span>) {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
            i += 1;
        }
        let mut end = i;
        while end > start && b[end - 1] == b'.' {
            end -= 1;
        }
        if (start > 0 && word_byte(b[start - 1])) || word_continues_at(b, end) {
            continue;
        }
        if parse_v4(&text[start..end]).is_some_and(|o| v4_is_host_fact(o, text, start, end)) {
            out.push(Span { start, end, with: ADDRESS_HIDDEN });
        }
    }
}

/// Whether an IPv6 literal is a host fact: tailnet and other unique-local
/// (`fc00::/7`), global unicast (`2000::/3`) and link-local (`fe80::/10`).
/// Loopback, unspecified and the rest stay visible.
fn v6_is_host_fact(a: std::net::Ipv6Addr) -> bool {
    let first = a.segments()[0];
    (0xfc00..=0xfdff).contains(&first) || (0x2000..=0x3fff).contains(&first) || (0xfe80..=0xfebf).contains(&first)
}

fn ipv6_spans(text: &str, out: &mut Vec<Span>) {
    if !text.contains("::") && text.matches(':').count() < 7 {
        return;
    }
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !(b[i].is_ascii_hexdigit() || b[i] == b':') {
            i += 1;
            continue;
        }
        let start = i;
        i += b[i..].iter().take_while(|c| c.is_ascii_hexdigit() || **c == b':').count();
        if b.get(i).is_some_and(|c| word_byte(*c)) {
            continue;
        }
        let glued = start > 0 && word_byte(b[start - 1]);
        if let Some(mut span) = v6_in_run(text, start, i, glued) {
            if span.end == i {
                span.end += zone_len(&b[i..]);
                i = span.end;
            }
            out.push(span);
        }
    }
}

/// The bytes of an IPv6 zone at the start of `rest` (`%en0`, or `%25en0` in
/// a URL), which names this machine's interface and so goes with the
/// address; 0 when there is none.
fn zone_len(rest: &[u8]) -> usize {
    if rest.first() != Some(&b'%') {
        return 0;
    }
    let zone = rest[1..].iter().take_while(|c| c.is_ascii_alphanumeric() || matches!(**c, b'_' | b'-')).count();
    if zone > 0 { 1 + zone } else { 0 }
}

/// The host-fact IPv6 literal in the run `start..end`, if any: the whole run,
/// then each suffix after a `:` (`ip:fd7a::1`, `peer:fd7a:115c::53`, and
/// `bad:fd7a:...`, where the label's own letters are hex digits). A run glued
/// to a word on its left is tried only from a `:` on.
fn v6_in_run(text: &str, start: usize, end: usize, glued: bool) -> Option<Span> {
    let run = &text[start..end];
    let from_colons = run.match_indices(':').map(|(i, _)| i + 1);
    let offsets = (!glued).then_some(0).into_iter().chain(from_colons);
    for off in offsets {
        let cand = &run[off..];
        for t in [cand, cand.strip_suffix(':').unwrap_or(cand)] {
            if t.parse::<std::net::Ipv6Addr>().is_ok_and(v6_is_host_fact) {
                return Some(Span { start: start + off, end: start + off + t.len(), with: ADDRESS_HIDDEN });
            }
        }
    }
    None
}

/// Every `.ts.net` name, whole.
fn tailnet_name_spans(text: &str, out: &mut Vec<Span>) {
    for dot in find_ci(text, ".ts.net") {
        let end = dot + ".ts.net".len();
        if continues_forward(&text[end..]) {
            continue;
        }
        let label = text.as_bytes()[..dot].iter().rev().take_while(|c| c.is_ascii_alphanumeric() || **c == b'.' || **c == b'-').count();
        if label > 0 {
            out.push(Span { start: dot - label, end, with: ADDRESS_HIDDEN });
        }
    }
}

/// Another account's home (`/Users/<name>`, `/home/<name>`,
/// `/var/home/<name>`, `/root`) reads `~`, so a path written on another machine
/// hides its account name too, not only this daemon's own `HOME`.
fn home_spans(text: &str, out: &mut Vec<Span>) {
    if !text.contains('/') {
        return;
    }
    for prefix in ["/users/", "/var/home/", "/home/"] {
        for i in find_ci(text, prefix) {
            if continues_backward(&text[..i]) {
                continue;
            }
            let from = i + prefix.len();
            let name = text[from..].trim_end_matches('.').bytes().take_while(|c| c.is_ascii_alphanumeric() || matches!(*c, b'.' | b'_' | b'-')).count();
            let name = text[from..from + name].trim_end_matches('.').len();
            if name > 0 {
                out.push(Span { start: i, end: from + name, with: "~" });
            }
        }
    }
    for i in find_ci(text, "/root") {
        if !continues_backward(&text[..i]) && !continues_forward(&text[i + 5..]) {
            out.push(Span { start: i, end: i + 5, with: "~" });
        }
    }
}

/// Whole-token, ASCII-case-insensitive occurrences of one needle.
fn needle_spans(text: &str, n: &Needle, out: &mut Vec<Span>) {
    for start in find_ci(text, &n.lower) {
        let end = start + n.lower.len();
        if continues_backward(&text[..start]) || continues_forward(&text[end..]) {
            continue;
        }
        // A bare host (no dot, no port) is only a host fact where it is
        // addressing: `://studio`, `@studio`, `studio:8765`.
        if n.bare && !(text[..start].ends_with("://") || text[..start].ends_with('@') || followed_by_port(&text[end..])) {
            continue;
        }
        out.push(Span { start, end, with: n.with });
    }
}

/// Apply the spans: earliest first, the longer of two starting together, and a
/// span inside an earlier one dropped.
fn apply_spans<'a>(text: &'a str, mut spans: Vec<Span>) -> std::borrow::Cow<'a, str> {
    if spans.is_empty() {
        return std::borrow::Cow::Borrowed(text);
    }
    spans.sort_by_key(|s| (s.start, std::cmp::Reverse(s.end)));
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for s in spans.iter() {
        if s.start < at {
            continue;
        }
        out.push_str(&text[at..s.start]);
        out.push_str(s.with);
        at = s.end;
    }
    out.push_str(&text[at..]);
    std::borrow::Cow::Owned(out)
}

/// Redact one run of plain text in a single pass: every span any rule finds is
/// collected against the original text and applied together.
/// Unchanged text is returned borrowed.
pub(crate) fn redact_text<'a>(text: &'a str, r: &Redaction) -> std::borrow::Cow<'a, str> {
    // Every host fact has a `.`, a `:`, a `/` or an `@` in or beside it (a name
    // that is only a word is a fact solely as `host:port` or `://host`),
    // unless a renderer cut one short with `…` and that punctuation went.
    if text.len() < 4 || !(text.bytes().any(|b| matches!(b, b'.' | b':' | b'/' | b'@')) || text.contains(CUT)) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut spans = Vec::new();
    if text.contains('.') {
        ipv4_spans(text, &mut spans);
        tailnet_name_spans(text, &mut spans);
    }
    if text.contains(':') {
        ipv6_spans(text, &mut spans);
    }
    home_spans(text, &mut spans);
    for n in &r.needles {
        needle_spans(text, n, &mut spans);
    }
    if text.contains(CUT) {
        cut_needle_spans(text, r, &mut spans);
        cut_tailnet_spans(text, &mut spans);
    }
    apply_spans(text, spans)
}

/// What a truncating renderer puts where it cut a cell short.
const CUT: char = '…';

/// A roster address, its host, the hub's host or a directory, cut short by a
/// truncating renderer (`head…tail` or `head…`, see
/// `panel_audience::cut_spans`): what is left still names it. A bare host
/// (`studio`) is a fact only where it addresses, so a cut of one is not.
fn cut_needle_spans(text: &str, r: &Redaction, out: &mut Vec<Span>) {
    for n in r.needles.iter().filter(|n| !n.bare) {
        for (start, end) in darkmux_types::panel_audience::cut_spans(text, &n.lower) {
            out.push(Span { start, end, with: n.with });
        }
    }
}

/// A `.ts.net` name cut short: `box.tail-x…s.net` (a middle cut that kept at
/// least `s.net` of its end) or `box.tail-x.ts…` (an end cut that kept a
/// dotted name and at least `.t` of the suffix). The span covers the name's
/// letters on both sides of the `…`, never a `:port` after it. A cut that
/// keeps neither (`box.tail-x…`) is not recognizable as a tailnet name.
fn cut_tailnet_spans(text: &str, out: &mut Vec<Span>) {
    let name_byte = |c: &u8| c.is_ascii_alphanumeric() || matches!(*c, b'.' | b'-');
    let b = text.as_bytes();
    for (e, _) in text.match_indices(CUT) {
        let head = b[..e].iter().rev().take_while(|c| name_byte(c)).count();
        let after = e + CUT.len_utf8();
        let tail = b[after..].iter().take_while(|c| name_byte(c)).count();
        let (h, t) = (text[e - head..e].to_ascii_lowercase(), text[after..after + tail].to_ascii_lowercase());
        let middle = t.ends_with("s.net") && (head > 0 || t.contains(".ts.net"));
        let end_cut = tail == 0 && h.contains('.') && [".t", ".ts", ".ts.", ".ts.n", ".ts.ne"].iter().any(|s| h.ends_with(s));
        if middle || end_cut {
            out.push(Span { start: e - head, end: after + tail, with: ADDRESS_HIDDEN });
        }
    }
}

/// How many bytes of `rest` (which starts with an ESC or a C1 CSI, U+009B)
/// the escape at its start covers, and what of it is sent on. Only two forms
/// survive for a remote caller: SGR (`CSI digits;colons m`, rebuilt from its
/// parameters) and OSC 8 hyperlinks (rebuilt with no parameters, an ST
/// terminator and the target only when it names nothing private). Every other
/// escape is dropped whole; one that is malformed or unterminated loses only
/// its introducer, so what follows is ordinary text and gets redacted as such
/// (`\x1b[/Users/kain` must not be eaten as the sequence `\x1b[/U`).
fn classify_escape(rest: &str, r: &Redaction, w: &Withheld) -> (usize, String) {
    let bytes = rest.as_bytes();
    if rest.starts_with('\u{9b}') {
        return csi(rest, 2);
    }
    match bytes.get(1) {
        Some(b'[') => csi(rest, 2),
        Some(b']') => osc(rest, r, w),
        // A charset designation (`ESC ( B`) is three bytes.
        Some(b'(' | b')' | b'*' | b'+') if bytes.get(2).is_some_and(u8::is_ascii_alphanumeric) => (3, String::new()),
        _ => (1, String::new()),
    }
}

/// A CSI whose introducer is `intro` bytes long: kept only as SGR.
fn csi(rest: &str, intro: usize) -> (usize, String) {
    let body = &rest.as_bytes()[intro..];
    let params = body.iter().take_while(|b| b.is_ascii_digit() || **b == b';' || **b == b':').count();
    match body.get(params) {
        Some(b'm') => (intro + params + 1, format!("\x1b[{}m", &rest[intro..intro + params])),
        Some(b) if (0x40..=0x7e).contains(b) => (intro + params + 1, String::new()),
        _ => (intro, String::new()),
    }
}

/// An OSC: kept only as an OSC 8 hyperlink, rebuilt without its parameters.
/// Its target is dropped when it names a host fact or a value the console
/// withholds (`w`).
fn osc(rest: &str, r: &Redaction, w: &Withheld) -> (usize, String) {
    let body = &rest[2..];
    let bel = body.find('\x07').map(|p| (p, 1));
    let st = body.find("\x1b\\").map(|p| (p, 2));
    let Some((end, term_len)) = [bel, st].into_iter().flatten().min_by_key(|(p, _)| *p) else {
        return (2, String::new());
    };
    let consumed = 2 + end + term_len;
    let Some((_params, target)) = body[..end].strip_prefix("8;").and_then(|l| l.split_once(';')) else {
        return (consumed, String::new());
    };
    let hidden = target.chars().any(char::is_control) || redact_text(target, r) != target || w.hits(target);
    (consumed, format!("\x1b]8;;{}\x1b\\", if hidden { "" } else { target }))
}

/// A panel's stdout for a remote caller. The text is split into escape
/// sequences and plain runs FIRST, and each run is redacted on its own, so an
/// escape boundary is always a token boundary: panel children are forced to
/// color, and `\x1b[2m/Users/kain` must read as a path, not as a path glued to
/// the `m` that ends the escape. See [`classify_escape`] for which escapes
/// survive. Each run also loses every value in `w` (the console panels'
/// extra set, [`panel_withheld`]).
pub(crate) fn redact_panel_stdout(text: &str, r: &Redaction, w: &Withheld) -> String {
    let redact_run = |run: &str| w.scrub(&redact_text(run, r));
    let mut out = String::with_capacity(text.len());
    let mut run_start = 0;
    let mut i = 0;
    while i < text.len() {
        let ch = text[i..].chars().next().unwrap_or(' ');
        if ch != '\x1b' && ch != '\u{9b}' {
            i += ch.len_utf8();
            continue;
        }
        out.push_str(&redact_run(&text[run_start..i]));
        let (consumed, kept) = classify_escape(&text[i..], r, w);
        out.push_str(&kept);
        i += consumed;
        run_start = i;
    }
    out.push_str(&redact_run(&text[run_start..]));
    out
}

/// What a console panel withholds from a remote viewer beyond [`Redaction`]:
/// the addresses, paths, endpoint URLs and credential pointers this machine is
/// configured with, wherever a verb prints them. Read when the request is
/// served, from the same places the verbs read:
///
/// - every location a setting resolves to (`env > config.json > default`),
///   through the accessors the code itself reads it with
///   (`darkmux_types::config_access::LOCATION_ACCESSORS`; a test there fails
///   on an accessor that could name a location and is in no list), so a
///   setting that exists only in the environment is covered;
/// - `config.json` as written (`panel_audience::config_scrub_values`: every
///   value of a scrubbed kind, every unknown key's value and every value of
///   the wrong type);
/// - the profile registry's endpoints (where each lives, where its key is),
///   the lab fixture registry's paths, the Redis URL and the temp directory;
/// - every enum setting's value that is not one of its tokens, from
///   `config.json` or the environment (bad config, which doctor quotes).
///
/// A path under the home or `DARKMUX_HOME` directory is left to
/// [`Redaction`], which already reads it as `~` or `$DARKMUX_HOME`. A value
/// spelled like a public name (a roster machine, this machine, a profile or
/// an endpoint) is never withheld, as [`Redaction`] never hides one: a
/// credential pointer named after its endpoint must not hide the endpoint.
///
/// The verbs that shape their own remote form (`doctor`, `flow status`) use
/// the same set before they wrap their output.
pub fn panel_withheld() -> Withheld {
    use darkmux_types::panel_audience::config_scrub_values;
    let read_json = |p: &std::path::Path| -> Option<serde_json::Value> {
        serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
    };
    let config_path = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser).config;
    let from_config = read_json(&config_path).map(|c| config_scrub_values(&c)).unwrap_or_default();

    let path_str = |p: std::path::PathBuf| p.to_string_lossy().to_string();
    let mut locations = darkmux_types::config_access::resolved_locations();
    locations.extend(darkmux_lab::lab::registry::fixture_locations());
    locations.extend(darkmux_flow::redis_url().map(|u| u.expose_for_probe().to_string()));
    for d in [std::env::temp_dir(), std::env::temp_dir().canonicalize().unwrap_or_default()] {
        locations.push(path_str(d));
    }

    // The public names: never withheld (see the doc above).
    let mut public: Vec<String> = Vec::new();
    if let Ok(roster) = darkmux_fleet::load_roster() {
        for m in roster.machines.values() {
            public.push(m.id.clone());
            public.extend(m.current_name.clone());
        }
    }
    public.extend(darkmux_flow::resolve_machine_id());

    let credentials = registry_sets(&mut locations, &mut public);
    let outside = outside_the_homes;
    // An enum setting's value that is not one of its tokens is bad config,
    // which doctor's refusal quotes, whether it was set in `config.json` or
    // the environment: what it holds is not what the key means.
    let bad_enum = darkmux_types::config_enum::bad_values().into_iter().map(|b| b.raw);
    Withheld::from_values(outside(from_config))
        .merged(Withheld::from_locations(outside(locations)))
        .merged(Withheld::from_values(credentials))
        .merged(Withheld::from_values(outside(bad_enum.collect())))
        .sparing(&public)
}

/// The profile registry's part of [`panel_withheld`]: the registry's own
/// path and each endpoint's URL go into `locations`, its profile and
/// endpoint names into `public`; returns each endpoint's credential pointer.
fn registry_sets(locations: &mut Vec<String>, public: &mut Vec<String>) -> Vec<String> {
    let mut credentials: Vec<String> = Vec::new();
    let Some(reg_path) = darkmux_profiles::profiles::registry_path(None) else { return credentials };
    locations.push(reg_path.to_string_lossy().to_string());
    let registry: Option<serde_json::Value> =
        std::fs::read_to_string(&reg_path).ok().and_then(|t| serde_json::from_str(&t).ok());
    let Some(registry) = registry else { return credentials };
    for section in ["profiles", "endpoints"] {
        public.extend(registry.get(section).and_then(|m| m.as_object()).into_iter().flat_map(|m| m.keys().cloned()));
    }
    for ep in registry.get("endpoints").and_then(|e| e.as_object()).into_iter().flat_map(|m| m.values()) {
        locations.extend(ep.get("url").and_then(|u| u.as_str()).map(str::to_string));
        for key in ["keychain", "key_env"] {
            credentials.extend(ep.pointer(&format!("/auth/{key}")).and_then(|v| v.as_str()).map(str::to_string));
        }
    }
    credentials
}

/// `values` without those under `HOME` or `DARKMUX_HOME` (or spelled with
/// `~`), which the home-prefix redaction already rewrites.
fn outside_the_homes(values: Vec<String>) -> Vec<String> {
    let homes: Vec<String> = ["HOME", "DARKMUX_HOME"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .map(|h| h.trim_end_matches('/').to_string())
        .filter(|h| h.len() > 1)
        .collect();
    let under_a_home = |v: &String| v.starts_with('~') || homes.iter().any(|h| v == h || v.starts_with(&format!("{h}/")));
    values.into_iter().filter(|v| !under_a_home(v)).collect()
}

/// What [`panel_withheld`] read, so a cached set is reused only while it is
/// still the answer: each file's mtime and length, and every `DARKMUX_*`
/// variable plus `HOME` and `TMPDIR`.
#[derive(Clone, PartialEq, Eq)]
struct WithheldKey {
    files: Vec<Option<(std::time::SystemTime, u64)>>,
    env: Vec<(String, String)>,
}

impl WithheldKey {
    fn current() -> Self {
        let meta = |p: Option<std::path::PathBuf>| {
            p.and_then(|p| std::fs::metadata(p).ok()).and_then(|m| Some((m.modified().ok()?, m.len())))
        };
        let root = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser);
        let files = vec![
            meta(Some(root.config.clone())),
            meta(darkmux_profiles::profiles::registry_path(None)),
            meta(Some(root.root.join("lab-registry.json"))),
            meta(Some(darkmux_fleet::roster_path())),
        ];
        let mut env: Vec<(String, String)> =
            std::env::vars().filter(|(k, _)| k.starts_with("DARKMUX_") || k == "HOME" || k == "TMPDIR").collect();
        env.sort();
        Self { files, env }
    }
}

static PANEL_WITHHELD_CACHE: std::sync::Mutex<Option<(WithheldKey, std::time::Instant, std::sync::Arc<Withheld>)>> =
    std::sync::Mutex::new(None);

/// [`panel_withheld`], reused for at most [`REDACTION_CACHE_TTL`] while the
/// files it read and the environment are unchanged: a console on a 3s
/// auto-refresh would otherwise re-read four files and resolve every setting
/// per remote request.
pub(crate) fn panel_withheld_cached() -> std::sync::Arc<Withheld> {
    let key = WithheldKey::current();
    let now = std::time::Instant::now();
    let mut guard = PANEL_WITHHELD_CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((k, at, w)) = guard.as_ref() {
        if *k == key && now.saturating_duration_since(*at) < REDACTION_CACHE_TTL {
            return w.clone();
        }
    }
    let w = std::sync::Arc::new(panel_withheld());
    *guard = Some((key, now, w.clone()));
    w
}

#[cfg(test)]
impl Redaction {
    /// The addresses it hides, longest first.
    pub(crate) fn addresses(&self) -> Vec<&str> {
        self.needles.iter().filter(|n| n.with == ADDRESS_HIDDEN).map(|n| n.text.as_str()).collect()
    }

    /// The directories it rewrites, with what each reads as.
    pub(crate) fn dirs(&self) -> Vec<(&str, &str)> {
        self.needles.iter().filter(|n| n.with != ADDRESS_HIDDEN).map(|n| (n.text.as_str(), n.with)).collect()
    }
}

impl Redaction {
    /// An event line for the reader the stream was opened for: redacted when
    /// the reader is remote (`Some`), as built otherwise.
    pub(crate) fn for_reader(r: Option<&Redaction>, line: String) -> String {
        match r {
            Some(r) => r.line(&line),
            None => line,
        }
    }

    /// One event line (an SSE `data:` payload, a JSON record) or any other
    /// text for a remote caller. A replacement carries no quote or backslash,
    /// so a JSON line stays JSON.
    pub(crate) fn line(&self, line: &str) -> String {
        redact_text(line, self).into_owned()
    }
}

/// The layer every JSON read route carries: a caller that is this machine or
/// holds the token gets the response as built; anyone else gets it with host
/// facts redacted ([`Redaction`]). The body is redacted as it streams, token by
/// token for JSON and line by line for anything else on a JSON route (a
/// plain-text error), so memory is bounded by the largest single value or line
/// and no response is too big to serve; nothing passes through unfiltered
/// ([`crate::redaction_stream`], #3073).
pub(crate) async fn redact_reads(req: Request, next: Next) -> Response {
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0);
    let full_view = crate::caller_is_local_or_holds_token(peer, req.headers());
    let resp = next.run(req).await;
    if full_view {
        return resp;
    }
    let is_json = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.starts_with("application/json"));
    let (mut parts, body) = resp.into_parts();
    let Ok(r) = tokio::task::spawn_blocking(Redaction::derive_cached).await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "response withheld: it could not be redacted for a remote reader\n").into_response();
    };
    let redactor = if is_json { StreamRedactor::json(r) } else { StreamRedactor::text(r) };
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, axum::body::Body::from_stream(redacted_chunks(body, redactor)))
}

/// How much of an arriving chunk is redacted per output chunk, so a handler that
/// builds its body in one piece still leaves the layer in pieces, not as a
/// second copy of the response.
const REDACT_SLICE_BYTES: usize = 64 * 1024;

struct RedactState {
    chunks: std::pin::Pin<Box<dyn futures::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Send>>,
    redactor: StreamRedactor,
    held: axum::body::Bytes,
    done: bool,
}

/// The body's bytes, each slice redacted as it passes, then the flush of what
/// the last one left pending. A body error, or a JSON body that is not JSON
/// (fail closed, #3073), ends the stream with that error.
fn redacted_chunks(body: axum::body::Body, redactor: StreamRedactor) -> impl futures::Stream<Item = Result<Vec<u8>, axum::Error>> {
    use futures::StreamExt;
    let state = RedactState { chunks: Box::pin(body.into_data_stream()), redactor, held: Default::default(), done: false };
    futures::stream::unfold(state, |mut st| async move {
        loop {
            if st.done {
                return None;
            }
            let mut out = Vec::new();
            if !st.held.is_empty() {
                let slice = st.held.split_to(st.held.len().min(REDACT_SLICE_BYTES));
                if let Err(e) = st.redactor.feed(&slice, &mut out) {
                    st.done = true;
                    return Some((Err(axum::Error::new(e)), st));
                }
            } else {
                match st.chunks.next().await {
                    Some(Ok(bytes)) => st.held = bytes,
                    Some(Err(e)) => {
                        st.done = true;
                        return Some((Err(e), st));
                    }
                    None => {
                        st.done = true;
                        if let Err(e) = st.redactor.finish(&mut out) {
                            return Some((Err(axum::Error::new(e)), st));
                        }
                    }
                }
            }
            if !out.is_empty() {
                return Some((Ok(out), st));
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet_view::tests::{peer_says, scripted, entry, identity};
    use crate::fleet_view::{AcceptsState, CardOutcome, CardSource, FleetContext};
    use crate::machine_card::tests::sample_card;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tower::ServiceExt;

    const HUB_IP: &str = "100.64.9.9";
    const PEER_IP: &str = "100.64.7.7";
    const TAILNET_NAME: &str = concat!("example-hub.example", ".ts", ".net");
    const HOME: &str = "/Users/fixture-home";

    /// The strings no remote reader may find anywhere in a response.
    const SECRETS: [&str; 5] = [HUB_IP, PEER_IP, "example-hub", "fixture-home", "redis://"];

    /// Sets the env a host fact can come from and restores it on drop.
    struct HostFacts {
        _dir: tempfile::TempDir,
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl HostFacts {
        fn set() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let fleet = dir.path().join("fleet.json");
            std::fs::write(
                &fleet,
                format!(
                    r#"{{"version":"2","machines":{{"peerone":{{"id":"peerone","address":"{PEER_IP}:8765","description":"studio at {PEER_IP}","added_unix_ms":1}},"hubbox":{{"id":"hubbox","address":"{TAILNET_NAME}","added_unix_ms":2}}}}}}"#
                ),
            )
            .unwrap();
            let vars: [(&'static str, String); 3] = [
                ("DARKMUX_FLEET_FILE", fleet.display().to_string()),
                ("DARKMUX_REDIS_URL", format!("redis://user:pw@{HUB_IP}:6379")),
                ("HOME", HOME.to_string()),
            ];
            let saved = vars.iter().map(|(k, _)| (*k, std::env::var_os(k))).collect();
            for (k, v) in &vars {
                unsafe { std::env::set_var(k, v) };
            }
            Self { _dir: dir, saved }
        }
    }

    impl Drop for HostFacts {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(v) => unsafe { std::env::set_var(k, v) },
                    None => unsafe { std::env::remove_var(k) },
                }
            }
        }
    }

    fn peer(addr: &str) -> axum::extract::ConnectInfo<std::net::SocketAddr> {
        axum::extract::ConnectInfo(addr.parse().unwrap())
    }

    async fn get(app: axum::Router, path: &str, from: &str) -> (u16, String) {
        let mut req = Request::builder().uri(path).header("host", "localhost").body(Body::empty()).unwrap();
        req.extensions_mut().insert(peer(from));
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    const REMOTE: &str = "10.0.0.9:5555";
    const LOCAL: &str = "127.0.0.1:5555";

    /// A view whose peer card and roster entry carry host facts in free text.
    fn leaky_view_router() -> axum::Router {
        let mut roster_entry = entry("peerone");
        roster_entry.address = format!("{PEER_IP}:8765");
        roster_entry.description = Some(format!("studio at {PEER_IP}"));
        let s = scripted(identity("laptop", Some("LAPTOP-UID"), Some("n-laptop")), vec![roster_entry]);
        let mut card = sample_card();
        card.profiles[0].description = Some(format!("the server at {HUB_IP}, notes in {HOME}/notes"));
        card.specs.hub_configured = true;
        peer_says(&s, "peerone", 0, CardOutcome::Available { card: Box::new(card), source: CardSource::Listener }, AcceptsState::Unknown);
        crate::build_router_full(PathBuf::new(), None, None, FleetContext::with_sources(Arc::new(s)))
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn fleet_view_shows_a_remote_reader_no_address_or_home_path() {
        let _facts = HostFacts::set();
        let (status, body) = get(leaky_view_router(), "/fleet/view", REMOTE).await;
        assert_eq!(status, 200, "{body}");
        for secret in SECRETS {
            assert!(!body.contains(secret), "a remote reader of /fleet/view found `{secret}`");
        }
        assert!(body.contains(ADDRESS_HIDDEN), "the hidden addresses read as a placeholder: {body}");
        assert!(body.contains("peerone"), "machine ids stay public");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn fleet_view_shows_a_local_reader_the_full_values() {
        let _facts = HostFacts::set();
        let (_, body) = get(leaky_view_router(), "/fleet/view", LOCAL).await;
        for shown in [PEER_IP, HUB_IP, HOME] {
            assert!(body.contains(shown), "this machine reads `{shown}` unchanged");
        }
    }

    /// Flow records carrying the host facts a real day carries: a working
    /// directory, an endpoint URL, a peer address.
    fn seed_flows() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let today = darkmux_flow::day_utc_now();
        let rec = |action: &str, extra: serde_json::Value| {
            let mut r = serde_json::json!({
                "ts": format!("{today}T10:00:00Z"), "schema_version": "2.0.0", "action": action,
                "machine_id": "laptop", "session_id": "s-1", "mission_id": "m-1", "source": "darkmux",
                "cwd": format!("{HOME}/work"), "endpoint": format!("http://{HUB_IP}:1234/v1"),
                "alt_endpoint": "http://[fd7a:115c:a1e0::53]:1234/v1", "peer": format!("{PEER_IP}:8765"),
            });
            r.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            r.to_string()
        };
        let lines = [rec("dispatch.start", serde_json::json!({})), rec("dispatch.complete", serde_json::json!({"result": "stop"}))];
        std::fs::write(dir.path().join(format!("{today}.jsonl")), lines.join("\n") + "\n").unwrap();
        dir
    }

    const SECRETS_V6: &str = "fd7a";

    /// Every JSON route as a remote reader, over SEEDED flows: the table is the
    /// list, so a route added later is covered by being added. The routes with
    /// a path parameter get an id the seed carries.
    #[tokio::test]
    #[serial_test::serial]
    async fn no_json_route_shows_a_remote_reader_a_host_fact() {
        let _facts = HostFacts::set();
        unsafe { std::env::remove_var("DARKMUX_REDIS_URL") };
        let flows = seed_flows();
        let today = darkmux_flow::day_utc_now();
        let mut covered = 0;
        let mut flow_rows_seen = 0;
        for route in crate::routes::table() {
            let crate::routes::Reply::Json(_) = route.reply else { continue };
            if route.path.starts_with("/panel/") {
                continue;
            }
            let id = if route.path.starts_with("/flow-dispatch/") { "s-1" } else { "m-1" };
            let path = route.path.replace(":date", &today).replace(":id", id);
            let (_, body) = get(crate::build_router(flows.path().to_path_buf()), &path, REMOTE).await;
            for secret in SECRETS.iter().chain([&SECRETS_V6]) {
                assert!(!body.contains(secret), "{} showed a remote reader `{secret}`", route.path);
            }
            if body.contains("dispatch.complete") {
                flow_rows_seen += 1;
                assert!(body.contains(ADDRESS_HIDDEN), "{path}: the seeded rows read as hidden, not absent");
            }
            covered += 1;
        }
        assert!(covered >= 12, "the sweep must reach the daemon's JSON reads, reached {covered}");
        assert_eq!(flow_rows_seen, 3, "/flow/:date, /flow-mission/:id and /flow-dispatch/:id must each have read the seeded records");
    }

    /// The seeded day answers a local reader in full: proof the sweep above is
    /// looking at data that carries the facts.
    #[tokio::test]
    #[serial_test::serial]
    async fn the_seeded_flow_day_carries_the_facts_for_this_machine() {
        let _facts = HostFacts::set();
        unsafe { std::env::remove_var("DARKMUX_REDIS_URL") };
        let flows = seed_flows();
        let (_, body) = get(crate::build_router(flows.path().to_path_buf()), &format!("/flow/{}", darkmux_flow::day_utc_now()), LOCAL).await;
        for shown in [HUB_IP, "fd7a:115c:a1e0::53", HOME] {
            assert!(body.contains(shown), "this machine reads `{shown}`: {body}");
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn roster_addresses_read_in_full_on_this_machine_and_hidden_remotely() {
        let _facts = HostFacts::set();
        let (_, local) = get(crate::build_router(PathBuf::new()), "/fleet/roster", LOCAL).await;
        assert!(local.contains(PEER_IP) && local.contains(TAILNET_NAME), "{local}");
        let (_, remote) = get(crate::build_router(PathBuf::new()), "/fleet/roster", REMOTE).await;
        assert!(!remote.contains(PEER_IP) && !remote.contains("example-hub"), "{remote}");
        assert!(remote.contains("peerone"), "the machine id is public: {remote}");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn machine_specs_states_a_hub_is_configured_and_never_where() {
        let _facts = HostFacts::set();
        for from in [LOCAL, REMOTE] {
            let (_, body) = get(crate::build_router(PathBuf::new()), "/machine/specs", from).await;
            let json: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(json["hub_configured"], true, "{from}: {body}");
            assert!(json.get("redis_url_redacted").is_none(), "the URL is not on the wire: {body}");
            assert!(!body.contains(HUB_IP) && !body.contains("redis://"), "{from}: {body}");
        }
    }

    /// A new card field that carries a hub URL or an address fails here: the
    /// card is built from the machine's real gather, then every string in it
    /// is checked. Cards are fetched by peers, so the card itself (not the
    /// route's layer) must hold no host.
    #[test]
    #[serial_test::serial]
    fn a_machine_card_carries_no_redis_url_or_address() {
        let _facts = HostFacts::set();
        let mut card = serde_json::to_value(sample_card()).unwrap();
        card["specs"] = serde_json::to_value(crate::gather_specs()).unwrap();
        let text = serde_json::to_string(&card).unwrap();
        assert!(!text.contains("redis://"), "a card must not carry a redis URL: {text}");
        assert!(!text.contains(HUB_IP), "a card must not carry the hub's address: {text}");
        assert_eq!(card["specs"]["hub_configured"], true);
    }

    #[test]
    #[serial_test::serial]
    fn literal_addresses_the_roster_does_not_list_are_hidden() {
        let r = Redaction::from_parts(&[], &[], None, None);
        assert_eq!(r.line("hub 100.64.9.9:6379 and 127.0.0.1 and v4.0.0.1x"), format!("hub {ADDRESS_HIDDEN}:6379 and 127.0.0.1 and v4.0.0.1x"));
        assert_eq!(r.line("box.example.ts.net."), format!("{ADDRESS_HIDDEN}."));
    }

    #[test]
    fn another_accounts_home_prefix_reads_as_a_tilde() {
        let r = Redaction::from_parts(&[], &[], None, None);
        assert_eq!(r.line("see /Users/someone/notes and /home/ci-user/x"), "see ~/notes and ~/x");
        assert_eq!(r.line("/Users/ and /usr/Users/x and /home/"), "/Users/ and /usr/Users/x and /home/");
    }

    #[test]
    #[serial_test::serial]
    fn a_plain_name_hub_host_is_hidden_where_it_addresses() {
        let _facts = HostFacts::set();
        unsafe { std::env::set_var("DARKMUX_REDIS_URL", "redis://u:p@hubname:6379") };
        let r = Redaction::derive();
        assert_eq!(r.line("redis://hubname:6379 and hubname:7000"), format!("redis://{ADDRESS_HIDDEN}:6379 and {ADDRESS_HIDDEN}:7000"));
        assert_eq!(r.line("the hubname word"), "the hubname word", "a bare name is not a host fact as a free word");
    }

    /// (#3074) A hub password holding `#`, `/` or `?` must not make the host a
    /// password prefix: serve reads the URL through the same authority parser
    /// as the flow redactor, so both find the real host.
    #[test]
    fn url_host_is_the_real_host_whatever_the_password_holds() {
        for pw in ["p#ssw0rd", "pa/ss", "pa?ss", "a#b/c?d", "p@ss"] {
            let url = format!("redis://kain:{pw}@hubhost.example:6379/0");
            assert_eq!(url_host(&url).as_deref(), Some("hubhost.example"), "{pw}");
            let url = format!("redis://:{pw}@hubhost.example:6379");
            assert_eq!(url_host(&url).as_deref(), Some("hubhost.example"), "{pw}");
        }
        assert_eq!(url_host("redis+unix:///tmp/x.sock?pass=s3"), None, "a socket has no host");
    }

    // ── the rules, unit by unit ────────────────────────────────────────

    fn rules() -> Redaction {
        Redaction::from_parts(&[], &[], None, None)
    }

    /// The middle cut `run list` and `mission status` make: head, `…`, tail,
    /// `max` characters in all.
    fn middle_cut(s: &str, max: usize) -> String {
        let chars: Vec<char> = s.chars().collect();
        let keep = max - 1;
        let (head, tail) = (keep.div_ceil(2), keep - keep.div_ceil(2));
        format!("{}…{}", chars[..head].iter().collect::<String>(), chars[chars.len() - tail..].iter().collect::<String>())
    }

    /// (5.0 security re-review C5) A cell cut short with `…`, in the middle
    /// or at its end, still hides a roster address (and its host) and a
    /// `.ts.net` name the roster does not list: what the cut leaves of either
    /// still names it.
    #[test]
    fn a_cut_roster_address_or_tailnet_name_is_still_hidden() {
        let r = Redaction::from_parts(&[("peerone", "peerone.lan-fake.example:8765")], &[], None, None);
        for addr in ["peerone.lan-fake.example:8765", "peerone.lan-fake.example"] {
            for max in 6..addr.chars().count() {
                let cut = middle_cut(addr, max);
                assert_eq!(r.line(&format!("at {cut}  x")), format!("at {ADDRESS_HIDDEN}  x"), "{cut}");
            }
            let head: String = addr.chars().take(10).collect();
            assert_eq!(r.line(&format!("at {head}…")), format!("at {ADDRESS_HIDDEN}"), "{head}…");
        }
        // A tailnet name: cut in the middle with its `s.net` kept, or at its
        // end with its `.ts` kept.
        let name = "box.tailnet-example.ts.net";
        for max in 11..name.chars().count() {
            let cut = middle_cut(name, max);
            assert_eq!(r.line(&format!("peer {cut}:8765")), format!("peer {ADDRESS_HIDDEN}:8765"), "{cut}");
        }
        for head in ["box.tailnet-example.ts…", "box.tailnet-example.ts.n…"] {
            assert_eq!(r.line(&format!("peer {head} up")), format!("peer {ADDRESS_HIDDEN} up"), "{head}");
        }
        // Text that names neither keeps its `…`.
        assert_eq!(r.line("loading… done; v1.2…3.4"), "loading… done; v1.2…3.4");
    }

    #[test]
    fn ipv4_is_hidden_when_private_or_addressing_and_prose_versions_survive() {
        let r = rules();
        for hidden in ["100.64.7.7", "10.0.0.1", "172.16.0.1", "172.31.9.9", "192.168.1.5", "169.254.1.1"] {
            assert_eq!(r.line(&format!("at {hidden} now")), format!("at {ADDRESS_HIDDEN} now"), "{hidden}");
        }
        assert_eq!(r.line("http://8.8.8.8/x and 8.8.8.8:53 and me@8.8.8.8"), format!("http://{ADDRESS_HIDDEN}/x and {ADDRESS_HIDDEN}:53 and me@{ADDRESS_HIDDEN}"));
        // `-` is a boundary for an IP literal.
        assert_eq!(r.line("100.64.7.7-tail and ip-100.64.7.8"), format!("{ADDRESS_HIDDEN}-tail and ip-{ADDRESS_HIDDEN}"));
        for kept in ["LM Studio 0.3.30.1", "1.2.3.4 in prose", "172.32.0.1", "127.0.0.1", "v4.0.0.1x", "100.64.7.7.example", "1.2.3.4.5"] {
            assert_eq!(r.line(kept), kept, "{kept}");
        }
    }

    #[test]
    fn ipv6_literals_are_hidden_except_loopback() {
        let r = rules();
        assert_eq!(r.line("http://[fd7a:115c:a1e0::53]:1234/v1"), format!("http://[{ADDRESS_HIDDEN}]:1234/v1"));
        assert_eq!(r.line("at fd7a:115c:a1e0:ab12:4843:cd96:625b:1 now"), format!("at {ADDRESS_HIDDEN} now"));
        assert_eq!(r.line("fd7a:115c:a1e0::53: refused"), format!("{ADDRESS_HIDDEN}: refused"));
        // Glued to a label: the label's own letters can be hex digits (`bad`).
        assert_eq!(r.line("ip:fd7a::1"), format!("ip:{ADDRESS_HIDDEN}"));
        assert_eq!(r.line("peer:fd7a:115c::53"), format!("peer:{ADDRESS_HIDDEN}"));
        assert_eq!(r.line("bad:fd7a:115c:a1e0::53"), format!("bad:{ADDRESS_HIDDEN}"));
        for hidden in ["2001:db8::1", "fe80::1", "fc00::5"] {
            assert_eq!(r.line(hidden), ADDRESS_HIDDEN, "{hidden}");
        }
        for kept in ["::1", "[::1]:8765", "at 12:34:56 today", "darkmux::fleet", "a::b", "::"] {
            assert_eq!(r.line(kept), kept, "{kept}");
        }
    }

    /// (5.0 review item 8) An IPv6 address's zone (`%en0`, or `%25en0` in a
    /// URL) names this machine's interface: it goes with the address.
    #[test]
    fn an_ipv6_zone_goes_with_its_address() {
        let r = rules();
        assert_eq!(r.line("peer fe80::1%en0 up"), format!("peer {ADDRESS_HIDDEN} up"));
        assert_eq!(r.line("http://[fe80::1%25en0]:8765/x"), format!("http://[{ADDRESS_HIDDEN}]:8765/x"));
        assert_eq!(r.line("::1%lo0 stays"), "::1%lo0 stays", "loopback stays, zone and all");
    }

    /// (5.0 review item 8) An OSC 8 link whose target names a value the
    /// console withholds loses its target, as one naming a host fact does.
    #[test]
    fn a_link_to_a_withheld_location_loses_its_target() {
        let w = Withheld::from_values(["/opt/fake-fixtures/alpha".to_string()]);
        let text = "\x1b]8;;file:///opt/fake-fixtures/alpha\x1b\\alpha\x1b]8;;\x1b\\\n";
        assert_eq!(redact_panel_stdout(text, &rules(), &w), "\x1b]8;;\x1b\\alpha\x1b]8;;\x1b\\\n");
    }

    /// (5.0 review items 1, 2 and 5) What a console panel withholds is read
    /// from every place this machine names a location: a setting that exists
    /// only in the environment, the lab fixture registry, the profile
    /// registry's endpoints. A credential pointer spelled like a public name
    /// (an endpoint's Keychain item named after the endpoint, a key variable
    /// named after a roster machine) is not withheld: the name stays readable.
    #[test]
    #[serial_test::serial]
    fn panel_withheld_reads_every_location_and_spares_public_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("dm");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("lab-registry.json"),
            r#"{"fixtures":{"alpha":{"path":"/opt/fake-fixtures/alpha","content_hash":"h","hashed_at":"t","manifest_version":"1"},"beta":{"path":"/Volumes/FakeWork/beta","content_hash":"h","hashed_at":"t","manifest_version":"1"}}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("profiles.json"),
            r#"{"profiles":{},"endpoints":{"hosted":{"url":"https://hosted-secret.example.com/v1","auth":{"type":"bearer","keychain":"hosted"}},"relay":{"url":"https://relay-secret.example.com/v1","auth":{"type":"bearer","key_env":"LAPTOP"}},"other":{"url":"https://other-secret.example.com/v1","auth":{"type":"bearer","keychain":"other-key-item"}}}}"#,
        )
        .unwrap();
        std::fs::write(root.join("fleet.json"), r#"{"version":"2","machines":{"laptop":{"id":"laptop","address":"100.64.9.1","added_unix_ms":1}}}"#).unwrap();
        let vars = [
            ("DARKMUX_HOME", root.display().to_string()),
            ("HOME", dir.path().join("home").display().to_string()),
            ("DARKMUX_HOST_SOURCE_SCRIPT", "/opt/fake-hostsrc/scenario.json".to_string()),
            ("DARKMUX_MODS_DIR", "/opt/fake-mods".to_string()),
            // (5.0 security re-review N3) An enum setting holding a value that is
            // not one of its tokens is bad config, which doctor's refusal
            // quotes, wherever it was set.
            ("DARKMUX_FLEET_MODE", "/opt/fake-env-enum-garbage".to_string()),
        ];
        let saved: Vec<_> = vars.iter().map(|(k, _)| (*k, std::env::var_os(k))).chain([("DARKMUX_FLEET_FILE", std::env::var_os("DARKMUX_FLEET_FILE")), ("DARKMUX_PROFILES", std::env::var_os("DARKMUX_PROFILES"))]).collect();
        for (k, v) in &vars {
            unsafe { std::env::set_var(k, v) };
        }
        unsafe { std::env::remove_var("DARKMUX_FLEET_FILE") };
        unsafe { std::env::remove_var("DARKMUX_PROFILES") };
        let w = panel_withheld();
        for (k, v) in saved {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        let vals = w.values();
        for want in ["/opt/fake-env-enum-garbage", "/opt/fake-hostsrc/scenario.json", "/opt/fake-mods", "/opt/fake-fixtures/alpha", "/Volumes/FakeWork/beta", "https://hosted-secret.example.com/v1", "other-key-item"] {
            assert!(vals.iter().any(|v| v == want), "{want} in {vals:?}");
        }
        for public in ["hosted", "LAPTOP", "laptop", "relay"] {
            assert!(!vals.iter().any(|v| v.eq_ignore_ascii_case(public)), "the public name {public} is not withheld: {vals:?}");
        }
    }

    #[test]
    fn linux_and_other_homes_read_as_a_tilde() {
        let r = rules();
        assert_eq!(
            r.line("/Users/someone/a /home/ci-user/b /var/home/core/c /root/d /root"),
            "~/a ~/b ~/c ~/d ~"
        );
        for kept in ["/rootfs/x", "/usr/home/x", "/Users/", "src/Users/x", "/home/"] {
            assert_eq!(r.line(kept), kept, "{kept}");
        }
    }

    /// A bare roster host (`studio`) must not be found inside a longer word,
    /// and must be where it addresses, even as the whole string after `@`.
    #[test]
    fn a_bare_host_is_a_whole_word_and_an_at_sign_is_a_position() {
        let r = Redaction::from_parts(&[("peerone", "studio"), ("peertwo", "kain-studio")], &[], None, None);
        assert_eq!(r.line("lmstudio:1234 and lmstudio-community/x"), "lmstudio:1234 and lmstudio-community/x");
        assert_eq!(r.line("studio:1234"), format!("{ADDRESS_HIDDEN}:1234"));
        assert_eq!(r.line("me@kain-studio"), format!("me@{ADDRESS_HIDDEN}"));
    }

    /// Every route in the table states how a remote reader is kept from host
    /// facts. The self-redacting set is closed: adding to it is a deliberate
    /// edit here. Static routes are fetched as a remote reader and carry none.
    #[tokio::test]
    #[serial_test::serial]
    async fn every_table_route_is_layered_or_declared() {
        use crate::routes::RouteRedaction::*;
        let _facts = HostFacts::set();
        unsafe { std::env::remove_var("DARKMUX_REDIS_URL") };
        let mut handler: Vec<&str> = Vec::new();
        let flows = seed_flows();
        for route in crate::routes::table() {
            match route.redaction() {
                Layer => {}
                Handler => handler.push(route.path),
                Static => {
                    let path = route.path.replace(":date", &darkmux_flow::day_utc_now());
                    let (_, body) = get(crate::build_router(flows.path().to_path_buf()), &path, REMOTE).await;
                    for secret in SECRETS {
                        assert!(!body.contains(secret), "static route {} shows `{secret}`", route.path);
                    }
                }
            }
        }
        assert_eq!(handler, ["/flow/:date/stream", "/panel/:id"], "a route that redacts for itself must be declared here on purpose");
    }

    /// `CacheKey` follows a real `fleet.json` edit: a roster that changes between two
    /// `derive_cached` calls inside the TTL is read again (#3073).
    #[test]
    #[serial_test::serial]
    fn derive_cached_follows_an_edit_to_the_real_fleet_json() {
        let dir = tempfile::tempdir().unwrap();
        let fleet = dir.path().join("fleet.json");
        let roster = |address: &str| {
            format!(r#"{{"version":"2","machines":{{"peerone":{{"id":"peerone","address":"{address}","added_unix_ms":1}}}}}}"#)
        };
        let saved = std::env::var_os("DARKMUX_FLEET_FILE");
        unsafe { std::env::set_var("DARKMUX_FLEET_FILE", &fleet) };
        *REDACTION_CACHE.lock().unwrap_or_else(|p| p.into_inner()) = None;

        std::fs::write(&fleet, roster("alpha.example:8765")).unwrap();
        let first = Redaction::derive_cached();
        assert!(!first.line("see alpha.example").contains("alpha.example"), "the first roster's address is hidden");
        assert!(first.line("see beta.example").contains("beta.example"), "an address not in the roster is not");

        // A longer address, so the length half of the key differs even on a coarse mtime.
        std::fs::write(&fleet, roster("beta.example.longer:8765")).unwrap();
        let second = Redaction::derive_cached();
        assert!(!second.line("see beta.example.longer").contains("beta.example.longer"), "the edit is picked up inside the TTL");

        match saved {
            Some(v) => unsafe { std::env::set_var("DARKMUX_FLEET_FILE", v) },
            None => unsafe { std::env::remove_var("DARKMUX_FLEET_FILE") },
        }
        *REDACTION_CACHE.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    #[test]
    fn the_cache_derives_once_per_key_and_again_when_the_key_changes() {
        let derives = std::cell::Cell::new(0);
        let mut cache = RedactionCache::default();
        let key = |m: u64| CacheKey { roster: Some((std::time::UNIX_EPOCH, m)), home: Some("/h".into()), darkmux_home: None };
        let now = std::time::Instant::now();
        let derive = || { derives.set(derives.get() + 1); Redaction::from_parts(&[], &[], None, None) };
        cache.get(key(1), now, derive);
        cache.get(key(1), now, derive);
        assert_eq!(derives.get(), 1, "an unchanged roster, HOME and DARKMUX_HOME reuse the derivation");
        cache.get(key(2), now, derive);
        assert_eq!(derives.get(), 2, "a changed fleet.json derives again");
        let mut other_home = key(2);
        other_home.home = Some("/other".into());
        cache.get(other_home.clone(), now, derive);
        assert_eq!(derives.get(), 3, "a changed HOME derives again");
        cache.get(other_home, now + REDACTION_CACHE_TTL, derive);
        assert_eq!(derives.get(), 4, "an aged entry derives again, so a hub or machine-id change still lands");
    }

    #[tokio::test]
    async fn a_non_json_reply_on_a_json_route_is_redacted_as_text_for_a_remote_reader() {
        let app = axum::Router::new()
            .route("/t", axum::routing::get(|| async { ([("content-type", "text/plain")], "failed at 100.64.7.7 in /Users/someone/x") }))
            .layer(axum::middleware::from_fn(redact_reads));
        let (_, remote) = get(app.clone(), "/t", REMOTE).await;
        assert_eq!(remote, format!("failed at {ADDRESS_HIDDEN} in ~/x"));
        let (_, local) = get(app, "/t", LOCAL).await;
        assert_eq!(local, "failed at 100.64.7.7 in /Users/someone/x");
    }

    /// A remote read has no size cap: a 100 MB JSON body is served, redacted, where the
    /// parse-and-walk layer withheld anything over 80 MB (#3073).
    #[tokio::test]
    async fn a_remote_body_of_a_hundred_megabytes_is_served_redacted() {
        let record = format!(r#"{{"peer":"{PEER_IP}","note":"{}"}}"#, "x".repeat(1400));
        let n = 100_000_000 / record.len() + 1;
        let body = format!("[{}]", vec![record.as_str(); n].join(","));
        let app = axum::Router::new()
            .route("/t", axum::routing::get(move || { let b = body.clone(); async move { ([("content-type", "application/json")], b) } }))
            .layer(axum::middleware::from_fn(redact_reads));
        let (status, remote) = get(app.clone(), "/t", REMOTE).await;
        assert_eq!(status, 200);
        assert!(remote.len() > 100_000_000 && !remote.contains(PEER_IP), "served in full, redacted");
        let (_, local) = get(app, "/t", LOCAL).await;
        assert!(local.contains(PEER_IP), "a local reader sees it as built");
    }

    /// A handler that builds its body in one piece still leaves the layer in slices, so the
    /// redacted copy is never a second whole response (#3073). The slices are 64 KiB, not
    /// fragments: a body leaves in about its length over 64 KiB, so the redacted stream does
    /// not multiply the frames a reader has to take.
    #[tokio::test]
    async fn a_remote_body_leaves_the_layer_in_slices() {
        use futures::StreamExt;
        let record = format!(r#"{{"peer":"{PEER_IP}","note":"{}"}}"#, "x".repeat(1400));
        let body = format!("[{}]", vec![record.as_str(); 2000].join(","));
        let slices = body.len().div_ceil(64 * 1024);
        let app = axum::Router::new()
            .route("/t", axum::routing::get(move || { let b = body.clone(); async move { ([("content-type", "application/json")], b) } }))
            .layer(axum::middleware::from_fn(redact_reads));
        let mut req = Request::builder().uri("/t").header("host", "localhost").body(Body::empty()).unwrap();
        req.extensions_mut().insert(peer(REMOTE));
        let mut chunks = app.oneshot(req).await.unwrap().into_body().into_data_stream();
        let (mut largest, mut total, mut count) = (0, 0, 0);
        while let Some(c) = chunks.next().await {
            let c = c.unwrap();
            largest = largest.max(c.len());
            total += c.len();
            count += 1;
        }
        assert!(total > 2_000_000);
        assert!(largest <= REDACT_SLICE_BYTES + record.len(), "a {largest} byte chunk");
        // One output chunk per slice, plus the flush of what the last one left pending.
        assert!(count <= slices + 1, "{count} chunks for {slices} slices");
    }

    /// What a remote caller receives: the status, every byte that reached it, and whether the
    /// body ended in an error (a truncated response).
    async fn get_streamed(app: axum::Router, path: &str) -> (u16, String, bool) {
        use futures::StreamExt;
        let mut req = Request::builder().uri(path).header("host", "localhost").body(Body::empty()).unwrap();
        req.extensions_mut().insert(peer(REMOTE));
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let mut chunks = resp.into_body().into_data_stream();
        let (mut got, mut errored) = (Vec::new(), false);
        while let Some(c) = chunks.next().await {
            match c {
                Ok(b) => got.extend_from_slice(&b),
                Err(_) => errored = true,
            }
        }
        (status, String::from_utf8_lossy(&got).into_owned(), errored)
    }

    fn json_route(body: String) -> axum::Router {
        axum::Router::new()
            .route("/t", axum::routing::get(move || { let b = body.clone(); async move { ([("content-type", "application/json")], b) } }))
            .layer(axum::middleware::from_fn(redact_reads))
    }

    /// A body that claims to be JSON and is not fails closed: the stream errors (a truncated
    /// body) and nothing past the break reaches the caller, where text redaction missed
    /// escaped facts (#3073).
    #[tokio::test]
    async fn a_malformed_json_reply_ends_the_stream_for_a_remote_reader() {
        let (status, remote, errored) = get_streamed(json_route("{\"a\": 100.64.7.7 /Users/someone/x".into()), "/t").await;
        assert_eq!(status, 200);
        assert!(errored, "the stream ends in an error: {remote}");
        assert!(!remote.contains(PEER_IP) && !remote.contains("/Users/someone"), "{remote}");
    }

    /// The error that truncates a malformed JSON reply says why, so whoever reads it in a
    /// log learns the body was withheld for not being JSON, not that the stream broke (#3073).
    #[tokio::test]
    async fn a_malformed_json_reply_ends_in_an_error_that_names_why() {
        use futures::StreamExt;
        let mut req = Request::builder().uri("/t").header("host", "localhost").body(Body::empty()).unwrap();
        req.extensions_mut().insert(peer(REMOTE));
        let resp = json_route("{\"a\": 100.64.7.7".into()).oneshot(req).await.unwrap();
        let mut chunks = resp.into_body().into_data_stream();
        let mut error = None;
        while let Some(c) = chunks.next().await {
            if let Err(e) = c {
                error = Some(e.to_string());
            }
        }
        let error = error.expect("the stream ends in an error");
        assert!(error.contains("not valid JSON") && error.contains("withheld"), "{error:?}");
    }

    /// The reviewer's proof: a real `axum::Json` handler, 130 levels deep (past serde_json's
    /// 128), a string with escaped newline and tab before each fact. It stays JSON, every fact
    /// is hidden, and what arrives is valid JSON (#3073).
    #[tokio::test]
    async fn a_body_nested_past_128_with_escaped_facts_is_redacted_as_json() {
        let mut v = serde_json::json!({"line": "err\n/Users/someone/x\tat\t100.64.7.7"});
        for _ in 0..130 {
            v = serde_json::json!([v]);
        }
        let app = axum::Router::new()
            .route("/t", axum::routing::get(move || { let v = v.clone(); async move { axum::Json(v) } }))
            .layer(axum::middleware::from_fn(redact_reads));
        let (status, remote, errored) = get_streamed(app, "/t").await;
        assert_eq!(status, 200);
        assert!(!errored, "valid JSON of any depth is served");
        assert!(!remote.contains(PEER_IP) && !remote.contains("/Users/someone"), "{remote}");
        let inner = remote.strip_prefix(&"[".repeat(130)).and_then(|r| r.strip_suffix(&"]".repeat(130))).expect("130 levels kept");
        let line = serde_json::from_str::<serde_json::Value>(inner).expect("valid JSON")["line"].as_str().unwrap().to_string();
        assert!(line.starts_with("err\n") && line.contains(ADDRESS_HIDDEN), "{line}");
    }

    /// JavaScript accepts `["\ud800", ...]` where serde_json refuses the lone surrogate; the
    /// stream must not fall to a text pass that cannot read the escaped path (#3073).
    #[tokio::test]
    async fn a_body_serde_rejects_but_a_browser_reads_leaks_no_fact() {
        let body = r#"["\ud800","\/Users\/kfake\/x","\n100.64.7.7"]"#.to_string();
        let (_, remote, errored) = get_streamed(json_route(body), "/t").await;
        assert!(errored, "fails closed: {remote}");
        assert!(!remote.contains("kfake") && !remote.contains(PEER_IP) && !remote.contains("Users"), "{remote}");
    }

    /// A route registered anywhere but the route table escapes the layer. The
    /// daemon's router is built from `routes::table()`; the fleet listener's
    /// own router is the token-gated execution surface, not a read.
    #[test]
    fn every_route_is_registered_through_the_route_table() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !name.ends_with(".rs") || name.ends_with("_tests.rs") || ["routes.rs", "fleet_listener.rs", "redaction.rs"].contains(&name.as_str()) {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            // A file's test module comes last; only the code above it counts.
            let code = text.split("\n#[cfg(test)]\nmod tests").next().unwrap_or("");
            let registers = [".route(", ".nest(", ".route_service(", ".nest_service(", ".fallback("].iter().any(|p| code.contains(p));
            if registers || (code.contains(".merge(") && code.contains("Router")) {
                offenders.push(name);
            }
        }
        assert!(offenders.is_empty(), "routes must be declared in routes.rs's table so they carry the redaction layer: {offenders:?}");
    }

    // ── the streams, at the handler ────────────────────────────────────

    const LEAKY_LINE: &str = r#"{"cwd":"/Users/fixture-home/w","endpoint":"http://[fd7a:115c:a1e0::53]:1234/v1","peer":"100.64.7.7","marker":"end-of-line"}"#;

    async fn open_stream(from: &str, flows: &std::path::Path) -> axum::body::BodyDataStream {
        let date = darkmux_flow::day_utc_now();
        std::fs::write(flows.join(format!("{date}.jsonl")), "").unwrap();
        let app = crate::build_router(flows.to_path_buf());
        let mut req = Request::builder().uri(format!("/flow/{date}/stream")).header("host", "localhost").body(Body::empty()).unwrap();
        req.extensions_mut().insert(peer(from));
        app.oneshot(req).await.unwrap().into_body().into_data_stream()
    }

    async fn read_until(mut body: axum::body::BodyDataStream, marker: &str) -> String {
        use futures::StreamExt;
        let mut seen = String::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(Ok(chunk)) = body.next().await {
                seen.push_str(&String::from_utf8_lossy(&chunk));
                if seen.contains(marker) {
                    break;
                }
            }
        })
        .await;
        seen
    }

    fn append_flow_line(flows: &std::path::Path, line: &str) {
        use std::io::Write;
        let date = darkmux_flow::day_utc_now();
        let mut f = std::fs::OpenOptions::new().append(true).open(flows.join(format!("{date}.jsonl"))).unwrap();
        writeln!(f, "{line}").unwrap();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn the_flow_stream_redacts_each_line_for_a_remote_reader_and_not_for_this_machine() {
        let _facts = HostFacts::set();
        unsafe { std::env::remove_var("DARKMUX_REDIS_URL") };
        for (from, hidden) in [(REMOTE, true), (LOCAL, false)] {
            let flows = tempfile::tempdir().unwrap();
            let body = open_stream(from, flows.path()).await;
            append_flow_line(flows.path(), LEAKY_LINE);
            let seen = read_until(body, "end-of-line").await;
            assert!(seen.contains("end-of-line"), "{from}: the line arrived: {seen}");
            for secret in ["fixture-home", "fd7a", PEER_IP] {
                assert_eq!(!seen.contains(secret), hidden, "{from}: `{secret}` in {seen}");
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn the_live_channel_redacts_each_sample_for_a_remote_reader() {
        let _facts = HostFacts::set();
        unsafe { std::env::remove_var("DARKMUX_REDIS_URL") };
        for (from, hidden) in [(REMOTE, true), (LOCAL, false)] {
            let flows = tempfile::tempdir().unwrap();
            let body = open_stream(from, flows.path()).await;
            crate::live_hub::hub().send(std::sync::Arc::from(LEAKY_LINE.replace("end-of-line", "live-marker").as_str())).ok();
            let seen = read_until(body, "live-marker").await;
            assert!(seen.contains("live-marker"), "{from}: the sample arrived: {seen}");
            for secret in ["fixture-home", "fd7a", PEER_IP] {
                assert_eq!(!seen.contains(secret), hidden, "{from}: `{secret}` in {seen}");
            }
        }
    }

    /// The Redis tail and the file tail meet a reader in `redacted_events`;
    /// this feeds it the way the Redis tail does (lines, no file).
    #[tokio::test]
    #[serial_test::serial]
    async fn lines_from_any_source_are_redacted_at_the_one_join_point() {
        use futures::StreamExt;
        let _facts = HostFacts::set();
        let lines = futures::stream::iter(vec![LEAKY_LINE.to_string()]).boxed();
        let events = crate::redacted_events(lines, Some(Arc::new(Redaction::derive())));
        let body = axum::response::sse::Sse::new(events);
        let resp = axum::response::IntoResponse::into_response(body);
        let seen = read_until(resp.into_body().into_data_stream(), "end-of-line").await;
        assert!(seen.contains("end-of-line") && !seen.contains("fixture-home") && !seen.contains("fd7a") && !seen.contains(PEER_IP), "{seen}");
    }
}
