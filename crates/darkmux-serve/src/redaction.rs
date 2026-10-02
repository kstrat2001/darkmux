//! The ONE owner of what a caller that is neither this machine nor a token
//! holder may not read (#3071 for the console panels, extended to every read
//! route). A daemon's reads are open on the tailnet by design, so a response is
//! filtered here before it leaves: roster addresses, tailnet names, IPv4
//! literals, the fleet hub's host, and the daemon user's home directory are
//! host facts that stay on this machine.
//!
//! Two entry points, one rule set ([`Redaction`]):
//!
//! - [`redact_reads`] is the layer every JSON route carries. For a remote
//!   caller it parses the body and redacts each string value, so a field no
//!   one thought to list (an operator-authored profile description, a path in
//!   an error line) is covered without naming it.
//! - [`redact_stdout`] is for a console panel's terminal output, which needs
//!   its escape sequences split out first (see [`classify_escape`]).
//!
//! The streams redact each event line with [`Redaction::line`]. Local callers
//! and token holders see every value unchanged
//! ([`crate::caller_is_local_or_holds_token`]).

use axum::extract::{ConnectInfo, Request};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::net::SocketAddr;

/// Stands in for an address in a remote caller's output.
pub(crate) const ADDRESS_HIDDEN: &str = "(address hidden)";
/// The largest JSON body the layer will re-read to redact; a bigger one is
/// withheld, never passed through unfiltered.
const MAX_REDACTED_BODY_BYTES: usize = 64 * 1024 * 1024;

/// What a non-local caller must not read in a response, derived from the
/// daemon's own state when the request is served: every roster ADDRESS (and its
/// host part) that is not itself a public machine name, the fleet hub's host,
/// and the daemon user's home and `DARKMUX_HOME` directories. Any IPv4 literal
/// and any `.ts.net` name is hidden whether or not the roster lists it
/// ([`hide_literals`]). Machine ids and names are
/// public (a 400 for a bad opt lists them): only the address behind one is
/// private.
pub(crate) struct Redaction {
    /// Longest first, so an address is replaced whole before its host part.
    pub(crate) addresses: Vec<String>,
    /// Directory prefixes and what each reads as, longest first.
    pub(crate) dirs: Vec<(String, &'static str)>,
}

impl Redaction {
    /// The roster is read from disk and the directories from the environment
    /// per call; there is no list to maintain. An unreadable roster hides
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
        let mut r = Self::from_parts(&machines, &public, home, darkmux_home);
        if let Some(h) = hub.filter(|h| !public.iter().any(|p| p.eq_ignore_ascii_case(h))) {
            r.addresses.push(h);
            r.addresses.sort_by_key(|a| std::cmp::Reverse(a.len()));
            r.addresses.dedup();
        }
        r
    }

    /// `machines` is `(id, address)`; `public` are further machine names.
    pub(crate) fn from_parts(
        machines: &[(&str, &str)],
        public: &[String],
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
        Self { addresses: all, dirs }
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

/// Replace each ASCII-case-insensitive occurrence of `needle` in `text` that
/// is a whole token: neither side may continue a word.
fn replace_token(text: &str, needle: &str, with: &str) -> String {
    if needle.is_empty() {
        return text.to_string();
    }
    let (hay, pat) = (text.to_ascii_lowercase(), needle.to_ascii_lowercase());
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut from = 0;
    while let Some(rel) = hay[from..].find(&pat) {
        let (start, end) = (from + rel, from + rel + pat.len());
        if !continues_backward(&text[..start]) && !continues_forward(&text[end..]) {
            out.push_str(&text[last..start]);
            out.push_str(with);
            last = end;
        }
        from = end;
    }
    out.push_str(&text[last..]);
    out
}

/// The host of a URL (`scheme://user:pw@host:port/path`), without userinfo,
/// port or path.
fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = address_host(hostport).unwrap_or(hostport);
    (!host.is_empty()).then(|| host.to_string())
}

/// Whether a whole token is an IPv4 literal other than loopback.
fn is_hidden_literal(tok: &str) -> bool {
    let parts: Vec<&str> = tok.split('.').collect();
    let ipv4 = parts.len() == 4 && parts.iter().all(|p| !p.is_empty() && p.len() <= 3 && p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u16>().is_ok_and(|n| n <= 255));
    (ipv4 && parts[0] != "127") || tok.to_ascii_lowercase().ends_with(".ts.net")
}

/// Replace every IPv4 literal (loopback excepted) and `.ts.net` name with
/// [`ADDRESS_HIDDEN`], whether or not the roster lists it: the fleet hub's
/// address, or one an operator wrote into a description by hand, is a host
/// fact all the same. Tokens are maximal runs of `[A-Za-z0-9.-]`; a trailing
/// `.` is punctuation.
fn hide_literals(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-')).unwrap_or(rest.len());
        if end == 0 {
            let ch = rest.chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let tok = &rest[..end];
        let core = tok.trim_end_matches('.');
        if is_hidden_literal(core) {
            out.push_str(ADDRESS_HIDDEN);
            out.push_str(&tok[core.len()..]);
        } else {
            out.push_str(tok);
        }
        i += end;
    }
    out
}

/// Another account's home (`/Users/<name>`, `/home/<name>`) reads `~`, so a path
/// an operator wrote into a description on another machine hides its account
/// name too, not only this daemon's own `HOME`. Applied to JSON values and
/// event lines; a panel's terminal output keeps [`redact_stdout`]'s exact
/// boundary rules.
fn hide_home_prefixes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = ["/Users/", "/home/"].iter().filter_map(|p| rest.find(p).map(|i| (i, p.len()))).min() {
        let (i, plen) = at;
        let name_len = rest[i + plen..].find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))).unwrap_or(rest.len() - i - plen);
        let at_boundary = !continues_backward(&rest[..i]);
        out.push_str(&rest[..i]);
        if at_boundary && name_len > 0 {
            out.push('~');
        } else {
            out.push_str(&rest[i..i + plen + name_len]);
        }
        rest = &rest[i + plen + name_len..];
    }
    out.push_str(rest);
    out
}

/// Redact one run of plain text: literals, roster addresses, then this
/// daemon's directories.
pub(crate) fn redact_text(text: &str, r: &Redaction) -> String {
    let mut out = hide_literals(text);
    for addr in &r.addresses {
        out = replace_token(&out, addr, ADDRESS_HIDDEN);
    }
    for (dir, reads_as) in &r.dirs {
        out = replace_token(&out, dir, reads_as);
    }
    out
}

/// How many bytes of `rest` (which starts with an ESC or a C1 CSI, U+009B)
/// the escape at its start covers, and what of it is sent on. Only two forms
/// survive for a remote caller: SGR (`CSI digits;colons m`, rebuilt from its
/// parameters) and OSC 8 hyperlinks (rebuilt with no parameters, an ST
/// terminator and the target only when it names nothing private). Every other
/// escape is dropped whole; one that is malformed or unterminated loses only
/// its introducer, so what follows is ordinary text and gets redacted as such
/// (`\x1b[/Users/kain` must not be eaten as the sequence `\x1b[/U`).
fn classify_escape(rest: &str, r: &Redaction) -> (usize, String) {
    let bytes = rest.as_bytes();
    if rest.starts_with('\u{9b}') {
        return csi(rest, 2);
    }
    match bytes.get(1) {
        Some(b'[') => csi(rest, 2),
        Some(b']') => osc(rest, r),
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
fn osc(rest: &str, r: &Redaction) -> (usize, String) {
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
    let hidden = target.chars().any(char::is_control) || redact_text(target, r) != target;
    (consumed, format!("\x1b]8;;{}\x1b\\", if hidden { "" } else { target }))
}

/// A panel's stdout for a remote caller. The text is split into escape
/// sequences and plain runs FIRST, and each run is redacted on its own, so an
/// escape boundary is always a token boundary: panel children are forced to
/// color, and `\x1b[2m/Users/kain` must read as a path, not as a path glued to
/// the `m` that ends the escape. See [`classify_escape`] for which escapes
/// survive.
pub(crate) fn redact_stdout(text: &str, r: &Redaction) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run_start = 0;
    let mut i = 0;
    while i < text.len() {
        let ch = text[i..].chars().next().unwrap_or(' ');
        if ch != '\x1b' && ch != '\u{9b}' {
            i += ch.len_utf8();
            continue;
        }
        out.push_str(&redact_text(&text[run_start..i], r));
        let (consumed, kept) = classify_escape(&text[i..], r);
        out.push_str(&kept);
        i += consumed;
        run_start = i;
    }
    out.push_str(&redact_text(&text[run_start..], r));
    out
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

    /// One event line (an SSE `data:` payload, a JSON record) for a remote
    /// caller. A replacement carries no quote or backslash, so a JSON line
    /// stays JSON.
    pub(crate) fn line(&self, line: &str) -> String {
        hide_home_prefixes(&redact_text(line, self))
    }

    /// Every string VALUE in a JSON document, redacted in place. Keys are
    /// field names, never host facts.
    pub(crate) fn json(&self, v: &mut serde_json::Value) {
        match v {
            serde_json::Value::String(s) => *s = hide_home_prefixes(&redact_text(s, self)),
            serde_json::Value::Array(a) => a.iter_mut().for_each(|x| self.json(x)),
            serde_json::Value::Object(o) => o.values_mut().for_each(|x| self.json(x)),
            _ => {}
        }
    }
}

/// The layer every JSON read route carries: a caller that is this machine or
/// holds the token gets the response as built; anyone else gets it with host
/// facts redacted ([`Redaction`]). A body it cannot parse or bound is withheld
/// (500), never passed through.
pub(crate) async fn redact_reads(req: Request, next: Next) -> Response {
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0);
    let full_view = crate::caller_is_local_or_holds_token(peer, req.headers());
    let resp = next.run(req).await;
    if full_view {
        return resp;
    }
    let is_json = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.starts_with("application/json"));
    if !is_json {
        return resp;
    }
    let (parts, body) = resp.into_parts();
    let withheld = || (StatusCode::INTERNAL_SERVER_ERROR, "response withheld: it could not be redacted for a remote reader\n").into_response();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_REDACTED_BODY_BYTES).await else { return withheld() };
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return withheld() };
    let r = tokio::task::spawn_blocking(Redaction::derive).await;
    let Ok(r) = r else { return withheld() };
    r.json(&mut value);
    let Ok(out) = serde_json::to_vec(&value) else { return withheld() };
    let mut parts = parts;
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, axum::body::Body::from(out))
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
        let bytes = to_bytes(resp.into_body(), 16 * 1024 * 1024).await.unwrap();
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

    /// Every JSON route without a path parameter, as a remote reader: the
    /// table is the list, so a route added later is covered by being added.
    #[tokio::test]
    #[serial_test::serial]
    async fn no_json_route_shows_a_remote_reader_a_host_fact() {
        let _facts = HostFacts::set();
        let mut covered = 0;
        for route in crate::routes::table() {
            let crate::routes::Reply::Json(_) = route.reply else { continue };
            if route.path.contains(':') || route.path.starts_with("/panel/") {
                continue;
            }
            let (_, body) = get(crate::build_router(PathBuf::new()), route.path, REMOTE).await;
            for secret in SECRETS {
                assert!(!body.contains(secret), "{} showed a remote reader `{secret}`", route.path);
            }
            covered += 1;
        }
        assert!(covered >= 12, "the sweep must reach the daemon's JSON reads, reached {covered}");
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
    fn the_hub_host_is_hidden_even_when_it_is_a_plain_name() {
        let _facts = HostFacts::set();
        unsafe { std::env::set_var("DARKMUX_REDIS_URL", "redis://u:p@hubname:6379") };
        let r = Redaction::derive();
        assert_eq!(r.line("redis at hubname now"), format!("redis at {ADDRESS_HIDDEN} now"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn the_event_stream_redacts_each_line_for_a_remote_reader() {
        let _facts = HostFacts::set();
        let r = Redaction::derive();
        let line = format!(r#"{{"cwd":"{HOME}/x","peer":"{PEER_IP}"}}"#);
        let remote = Redaction::for_reader(Some(&r), line.clone());
        assert!(serde_json::from_str::<serde_json::Value>(&remote).is_ok(), "{remote}");
        assert!(!remote.contains("fixture-home") && !remote.contains(PEER_IP), "{remote}");
        assert_eq!(Redaction::for_reader(None, line.clone()), line);
    }
}
