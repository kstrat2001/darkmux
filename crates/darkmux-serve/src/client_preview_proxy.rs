//! (#3560, part 4 of 4) A paired device's preview, through `client serve`'s
//! tailnet entry: one preview per browser, chosen by a session cookie, at
//! the root of the entry's address.
//!
//! A preview's dev server listens on this Mac's loopback only, and serves
//! its assets at root paths (`/src/main.tsx`, `/@vite/client`), so it is
//! proxied at the root of `https://<machine>.<tailnet>.ts.net:<port>/`
//! rather than under a prefix every recipe would have to know.
//!
//! **The flow** an app follows (ask for a link, open it, browse) is in
//! `docs/CLIENTS.md`. Here: [`link_handler`] issues a one-time link against
//! the device's token, [`open_handler`] uses up its code and sets the
//! session cookie ([`COOKIE`]), and [`front_door`] sends a request through
//! the entry on any path outside `/client/` to the session's preview, or
//! refuses it. A request from this machine never reaches the proxy.
//!
//! **What authenticates.** Any process on this Mac can send the tailnet
//! headers to loopback, so they are an extra check, never the authority:
//! the link is only issued against a paired device's token, and the session
//! cookie is the token's stand-in, a 32-byte random secret kept only as its
//! BLAKE3 hash and compared in constant time, like the token. The code in
//! the link is in a URL, which a browser keeps in its history, so it is
//! used up when first presented and lives [`LINK_TTL_SECS`].
//!
//! **What the preview sees.** Model-written code runs in the preview, and it
//! can reach this listener on loopback, so it never sees what would let it
//! act as the device: darkmux's cookie, `Authorization`, and the
//! `Tailscale-*` and `X-Forwarded-*` headers are removed, and `Host` and
//! `Origin` name the preview's loopback address (so a dev server's host
//! check passes without editing anyone's repo). It cannot set darkmux's
//! cookie either: a `Set-Cookie` of that name in its answer is dropped. The
//! cookie is `HttpOnly`, so the preview's scripts in the browser cannot read
//! it.
//!
//! **Lifetime.** In memory only. A session ends when its preview stops or
//! restarts (a new run, [`crate::client_preview::Previews::serving`]), when
//! its device is revoked (its client gone from the store, or recorded with
//! another identity), and when `client serve` stops; a request on an ended
//! session is refused. A browser keeps one darkmux cookie per address, so it
//! shows one preview at a time; a new link replaces the client's earlier
//! link and session for the same preview.
//!
//! **What stays open.** Browsers send a cookie to every port of a host, so
//! a service on another port of this Mac's tailnet name (one the operator
//! publishes there) receives this cookie too. Such a service runs on this
//! Mac and could replay it to loopback with forged headers, as it could
//! any header. The session lives no longer than its preview.

use axum::extract::{Request, State};
use axum::http::header::{
    AUTHORIZATION, CONNECTION, COOKIE as COOKIE_HEADER, HOST, LOCATION, ORIGIN, SET_COOKIE, UPGRADE,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use darkmux_types::client_code::ClientCode;
use std::net::IpAddr;
use std::sync::Mutex;

use crate::client_pairing::{
    authenticate, err, hash_hex, internal, owned_workspace, previews_allowed, random_hex, refuse,
    secret_matches, whois, ApprovedClient, ClientPath, ClientServerState, Forwarded, Refused,
};
use crate::client_preview::Serving;
use crate::client_tailnet::{Arrival, TailnetIdentity};
use crate::client_wire::{PreviewLink, RequestVia};

/// The preview session cookie. The `__Host-` prefix: a browser keeps it only
/// when it was set over HTTPS, with `Path=/` and no `Domain`, so no other
/// host's page can set one for this address.
pub const COOKIE: &str = "__Host-darkmux_preview";

/// How long a preview link waits to be opened. The app opens the link as soon
/// as it has it: it hands the URL to the device's browser, which asks for it
/// through the tailnet entry at once, a matter of seconds even while
/// Tailscale sets up a relayed path for that first request. A minute covers
/// that, and a link someone saw (it is in a URL, which a browser keeps in
/// its history) is good for no longer.
pub const LINK_TTL_SECS: u64 = 60;

/// The route a preview link opens.
pub(crate) const LINK_ROUTE: &str = "/client/preview-link/:code";

/// What a preview link, and the session opened with it, is bound to: the
/// client that asked, which of its previews, the tailnet identity the
/// request came from, and the run and port that preview was serving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Binding {
    pub(crate) client_id: String,
    pub(crate) workspace: String,
    pub(crate) identity: TailnetIdentity,
    pub(crate) serving: Serving,
}

impl Binding {
    /// Whether `other` is the same client's link or session to the same
    /// preview, which a new one replaces.
    fn same_preview(&self, other: &Binding) -> bool {
        self.client_id == other.client_id && self.workspace == other.workspace
    }
}

/// A link waiting to be opened: its code's hash, never the code.
struct Link {
    code_hash: String,
    binding: Binding,
    issued: u64,
}

/// An open session: its cookie's hash, never the cookie.
struct Session {
    cookie_hash: String,
    binding: Binding,
}

/// The preview links waiting to be opened and the sessions they opened.
#[derive(Default)]
pub struct PreviewSessions {
    links: Mutex<Vec<Link>>,
    sessions: Mutex<Vec<Session>>,
}

/// `m` locked, whatever a panicking holder left.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Whether a link issued at `issued` (Unix seconds) may still be opened at
/// `now`.
pub(crate) fn link_in_time(issued: u64, now: u64) -> bool {
    now.saturating_sub(issued) < LINK_TTL_SECS
}

/// Which of `hashes` `presented` hashes to: every one compared, each in
/// constant time ([`secret_matches`]), the first match kept.
fn matching<'a>(hashes: impl Iterator<Item = &'a str>, presented: &str) -> Option<usize> {
    hashes
        .enumerate()
        .filter(|(_, h)| secret_matches(h, presented))
        .fold(None, |hit, (i, _)| hit.or(Some(i)))
}

impl PreviewSessions {
    /// A new one-time code for `binding`, issued at `now`. It replaces the
    /// client's earlier link to the same preview, and links past their time
    /// go.
    pub(crate) fn issue(&self, binding: Binding, now: u64) -> anyhow::Result<String> {
        let code = random_hex(32)?;
        let mut links = lock(&self.links);
        links.retain(|l| link_in_time(l.issued, now) && !l.binding.same_preview(&binding));
        links.push(Link {
            code_hash: hash_hex(&code),
            binding,
            issued: now,
        });
        Ok(code)
    }

    /// What `code` was issued for, once: a presented code is removed
    /// whether or not it is still in time. `None` for a code that is unknown,
    /// used, or past [`LINK_TTL_SECS`].
    pub(crate) fn redeem(&self, code: &str, now: u64) -> Option<Binding> {
        let mut links = lock(&self.links);
        let at = matching(links.iter().map(|l| l.code_hash.as_str()), code)?;
        let link = links.swap_remove(at);
        link_in_time(link.issued, now).then_some(link.binding)
    }

    /// A new session cookie for `binding`: it replaces the client's earlier
    /// session on the same preview.
    pub(crate) fn open(&self, binding: Binding) -> anyhow::Result<String> {
        let cookie = random_hex(32)?;
        let mut sessions = lock(&self.sessions);
        sessions.retain(|s| !s.binding.same_preview(&binding));
        sessions.push(Session {
            cookie_hash: hash_hex(&cookie),
            binding,
        });
        Ok(cookie)
    }

    /// The session one of `presented` (cookie values) opens: its key (the
    /// cookie's hash) and what it is bound to.
    pub(crate) fn find(&self, presented: &[String]) -> Option<(String, Binding)> {
        let sessions = lock(&self.sessions);
        let hit = presented
            .iter()
            .find_map(|p| matching(sessions.iter().map(|s| s.cookie_hash.as_str()), p))?;
        let s = &sessions[hit];
        Some((s.cookie_hash.clone(), s.binding.clone()))
    }

    /// Ends the session `key` names.
    pub(crate) fn end(&self, key: &str) {
        lock(&self.sessions).retain(|s| s.cookie_hash != key);
    }
}

// ─── Which way a request goes ─────────────────────────────────────────────

/// Where the front door sends a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Door {
    /// To darkmux's own routes, behind their gate.
    Darkmux,
    /// To the preview proxy: through the tailnet entry, on a path outside
    /// `/client/`, as tailnet user `login`, for the device at `from`.
    Preview { login: Option<String>, from: IpAddr },
}

/// Where a request that arrived as `arrival`, for `path`, goes. Only a
/// request the tailnet entry forwarded can reach a preview, and only on a
/// path that is not darkmux's (`/client` and below).
pub(crate) fn door(arrival: Arrival, path: &str) -> Door {
    match arrival {
        Arrival::Tailnet { login, from } if !is_darkmux_path(path) => Door::Preview { login, from },
        _ => Door::Darkmux,
    }
}

/// Whether `path` is darkmux's: `/client` and everything under it.
pub(crate) fn is_darkmux_path(path: &str) -> bool {
    path == "/client" || path.starts_with("/client/")
}

// ─── Cookies and headers ──────────────────────────────────────────────────

/// Whether a cookie named `name` is darkmux's (any case: a name a browser
/// might read as the same one is never left to the preview).
fn is_ours(name: &str) -> bool {
    name.trim().eq_ignore_ascii_case(COOKIE)
}

/// Each `name=value` pair in the request's `Cookie` headers.
fn cookie_pairs(headers: &HeaderMap) -> impl Iterator<Item = (&str, &str)> {
    headers
        .get_all(COOKIE_HEADER)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.split_once('='))
}

/// Each value of darkmux's cookie the request carries.
pub(crate) fn session_cookies(headers: &HeaderMap) -> Vec<String> {
    cookie_pairs(headers)
        .filter(|(name, _)| is_ours(name))
        .map(|(_, value)| value.trim().to_string())
        .collect()
}

/// The request's other cookies, as one `Cookie` value; `None` when there are
/// none. A cookie header that is not text is dropped whole: darkmux cannot
/// tell which cookies it carries.
fn other_cookies(headers: &HeaderMap) -> Option<HeaderValue> {
    let kept: Vec<String> = cookie_pairs(headers)
        .filter(|(name, _)| !is_ours(name))
        .map(|(name, value)| format!("{}={}", name.trim(), value.trim()))
        .collect();
    (!kept.is_empty())
        .then(|| HeaderValue::from_str(&kept.join("; ")).ok())
        .flatten()
}

/// Whether a request header is one the preview never sees as sent.
fn withheld(name: &str) -> bool {
    let rewritten = [
        AUTHORIZATION.as_str(),
        COOKIE_HEADER.as_str(),
        HOST.as_str(),
        ORIGIN.as_str(),
    ];
    rewritten.contains(&name) || name.starts_with("tailscale-") || name.starts_with("x-forwarded-")
}

/// The headers a request goes to the preview on `port` with: `headers`
/// without darkmux's cookie, `Authorization`, `Tailscale-*` and
/// `X-Forwarded-*`, and with `Host` (and `Origin`, when sent) naming the
/// preview's loopback address.
pub(crate) fn upstream_headers(headers: &HeaderMap, port: u16) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers.iter().filter(|(name, _)| !withheld(name.as_str())) {
        out.append(name.clone(), value.clone());
    }
    if let Some(cookies) = other_cookies(headers) {
        out.insert(COOKIE_HEADER, cookies);
    }
    out.insert(
        HOST,
        HeaderValue::from_str(&format!("127.0.0.1:{port}"))
            .expect("a loopback address is a header value"),
    );
    if headers.contains_key(ORIGIN) {
        out.insert(
            ORIGIN,
            HeaderValue::from_str(&format!("http://127.0.0.1:{port}"))
                .expect("a loopback origin is a header value"),
        );
    }
    out
}

/// Whether a `Set-Cookie` value sets darkmux's cookie: its name is what
/// comes before the first `=` of the part before the first `;`.
fn sets_ours(value: &HeaderValue) -> bool {
    let bytes = value.as_bytes();
    let pair = bytes.split(|b| *b == b';').next().unwrap_or_default();
    let name = pair.split(|b| *b == b'=').next().unwrap_or_default();
    name.trim_ascii().eq_ignore_ascii_case(COOKIE.as_bytes())
}

/// A preview's answer's headers with every `Set-Cookie` of darkmux's cookie
/// dropped: the preview never sets the session.
pub(crate) fn drop_our_cookie(headers: &mut HeaderMap) {
    let kept: Vec<HeaderValue> = headers
        .get_all(SET_COOKIE)
        .iter()
        .filter(|v| !sets_ours(v))
        .cloned()
        .collect();
    headers.remove(SET_COOKIE);
    for value in kept {
        headers.append(SET_COOKIE, value);
    }
}

/// The `Set-Cookie` that opens session `cookie` in a browser.
pub(crate) fn session_set_cookie(cookie: &str) -> String {
    format!("{COOKIE}={cookie}; HttpOnly; Secure; SameSite=Strict; Path=/")
}

/// Whether a request asks to switch protocols (a WebSocket): an `Upgrade`
/// header and `upgrade` among its `Connection` tokens.
pub(crate) fn wants_upgrade(headers: &HeaderMap) -> bool {
    let connection = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok());
    headers.contains_key(UPGRADE)
        && connection
            .flat_map(|v| v.split(','))
            .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
}

// ─── Whether a session still stands ──────────────────────────────────────

/// Whether `binding` still stands: its client is still paired with the
/// tailnet identity it was issued to (in `clients`, the store now), and its
/// preview still serves that run on that port (`serving`, now).
pub(crate) fn still_stands(
    binding: &Binding,
    clients: &[ApprovedClient],
    serving: Option<&Serving>,
) -> bool {
    let paired = clients
        .iter()
        .any(|c| c.id == binding.client_id && c.tailnet.as_ref() == Some(&binding.identity));
    paired && serving == Some(&binding.serving)
}

/// Whether `binding` stands now, as the store and the previews say; `Err`
/// when the store cannot be read.
fn stands_now(state: &ClientServerState, binding: &Binding) -> Result<bool, Refused> {
    let clients = state
        .store
        .read()
        .map_err(|e| Box::new(internal(e)))?
        .clients;
    Ok(still_stands(
        binding,
        &clients,
        state.previews.serving(&binding.workspace).as_ref(),
    ))
}

/// The request through the entry, as tailnet user `login` from the device
/// at `from`, is the user and device `binding` was issued to: the user
/// first, then the device `whois` names there (fail closed). `url` is the
/// entry's, for a remedy.
async fn same_device(
    state: &ClientServerState,
    binding: &Binding,
    (login, from): (Option<&str>, IpAddr),
) -> Result<(), Refused> {
    let forbidden = |(code, why): (ClientCode, String)| refuse(StatusCode::FORBIDDEN, code, why);
    let url = state
        .tailnet_entry
        .as_ref()
        .map(darkmux_fleet::overlay::ServeEntry::url)
        .unwrap_or_default();
    let identity = crate::client_tailnet::identity_checked(Some(&binding.identity), login, &url)
        .map_err(forbidden)?;
    let found = whois(state, from).await;
    crate::client_tailnet::device_refusal(&identity.device, found, &identity.login)
        .map_or(Ok(()), |r| Err(forbidden(r)))
}

// ─── Refusals ─────────────────────────────────────────────────────────────

fn no_session() -> Refused {
    refuse(
        StatusCode::FORBIDDEN,
        ClientCode::NoPreviewSession,
        "no preview session in this browser, or it has ended (its preview stopped or restarted, its device was \
         unpaired, or darkmux client serve restarted): open the preview from the app again",
    )
}

fn tailnet_only() -> Refused {
    refuse(
        StatusCode::FORBIDDEN,
        ClientCode::TailnetOnly,
        "a preview link is for a device paired over the tailnet, asked for and opened through the tailnet entry; \
         an app on this Mac opens the preview's own loopback address",
    )
}

fn unknown_link() -> Refused {
    refuse(
        StatusCode::NOT_FOUND,
        ClientCode::NotFound,
        format!(
            "this preview link is unknown, already used, or older than {LINK_TTL_SECS} seconds: open the preview \
             from the app again"
        ),
    )
}

fn no_longer_stands() -> Refused {
    refuse(
        StatusCode::CONFLICT,
        ClientCode::Conflict,
        "since this link was made, its preview stopped or restarted, or its device was unpaired: open the preview \
         from the app again",
    )
}

fn stopping() -> Refused {
    refuse(
        StatusCode::SERVICE_UNAVAILABLE,
        ClientCode::Unavailable,
        "darkmux client serve is stopping",
    )
}

// ─── Asking for a link ────────────────────────────────────────────────────

/// `POST /client/workspaces/{name}/preview/link`: a one-time link to this
/// app's ready preview of `name`, for the device that asks through the
/// tailnet entry (never from this Mac: `tailnet_only`).
pub(crate) async fn link_handler(
    State(state): State<ClientServerState>,
    ClientPath(name): ClientPath<String>,
    via: Option<Extension<RequestVia>>,
    headers: HeaderMap,
) -> Response {
    let via = via.map_or(RequestVia::Local, |v| v.0);
    answer(
        authenticate(&state, &headers)
            .and_then(|client| issued(&state, client, &name, via))
            .map(Json),
    )
}

/// A handler's answer: the reply, or the refusal.
fn answer(result: Result<impl IntoResponse, Refused>) -> Response {
    result.map_or_else(|r| *r, IntoResponse::into_response)
}

/// Whom a link may be issued to: a request through the tailnet entry
/// (`via`), from a client paired over the tailnet (`paired`, its recorded
/// identity). The entry, for the link's URL, and that identity.
pub(crate) fn link_asker<'a>(
    entry: Option<&'a darkmux_fleet::overlay::ServeEntry>,
    via: RequestVia,
    paired: Option<&TailnetIdentity>,
) -> Option<(&'a darkmux_fleet::overlay::ServeEntry, TailnetIdentity)> {
    let entry = entry.filter(|_| via == RequestVia::Tailnet)?;
    paired.map(|identity| (entry, identity.clone()))
}

/// The link [`link_handler`] answers, or its refusal.
fn issued(
    state: &ClientServerState,
    client: ApprovedClient,
    name: &str,
    via: RequestVia,
) -> Result<PreviewLink, Refused> {
    let (entry, identity) = link_asker(state.tailnet_entry.as_ref(), via, client.tailnet.as_ref())
        .ok_or_else(tailnet_only)?;
    let serving = ready_preview(state, &client, name)?;
    let device = crate::client_tailnet::described(&identity);
    let binding = Binding {
        client_id: client.id,
        workspace: name.to_string(),
        identity,
        serving,
    };
    let code = state
        .preview_sessions
        .issue(binding, (state.now)())
        .map_err(|e| Box::new(internal(e)))?;
    // The code is the link's secret: never logged.
    eprintln!("darkmux client serve: preview {name}: link → issued for {device}");
    Ok(PreviewLink {
        url: format!("{}client/preview-link/{code}", entry.url()),
        expires_in_secs: LINK_TTL_SECS,
    })
}

/// What `client`'s preview of `name` serves, when previews are on, the
/// workspace is the client's own and its preview is ready.
fn ready_preview(
    state: &ClientServerState,
    client: &ApprovedClient,
    name: &str,
) -> Result<Serving, Refused> {
    let not_ready = || {
        refuse(
            StatusCode::CONFLICT,
            ClientCode::Conflict,
            format!("{name} has no ready preview: start it, and ask for a link once it is ready"),
        )
    };
    previews_allowed(state)
        .and_then(|()| owned_workspace(state, client, name))
        .and_then(|_| state.previews.serving(name).ok_or_else(not_ready))
}

// ─── Opening a link ───────────────────────────────────────────────────────

/// `GET /client/preview-link/{code}`: a browser through the tailnet entry
/// opens the link. The code is used up; then the tailnet user and device
/// must be the ones it was issued to, and its preview and client must still
/// stand. The browser gets the session cookie and a 303 to `/`. From this
/// Mac it is refused (`tailnet_only`), and the code kept.
pub(crate) async fn open_handler(
    State(state): State<ClientServerState>,
    ClientPath(code): ClientPath<String>,
    forwarded: Option<Extension<Forwarded>>,
) -> Response {
    let Some(Extension(f)) = forwarded else {
        return *tailnet_only();
    };
    answer(
        opened(&state, &code, &f)
            .await
            .map(|cookie| see_other(&cookie)),
    )
}

/// The session cookie opening `code` through the entry, as `f` names the
/// caller, gives; or the refusal.
async fn opened(state: &ClientServerState, code: &str, f: &Forwarded) -> Result<String, Refused> {
    let binding = state
        .preview_sessions
        .redeem(code, (state.now)())
        .ok_or_else(unknown_link)?;
    same_device(state, &binding, (Some(&f.login), f.from)).await?;
    new_session(state, binding)
}

/// A session for `binding`, once it still stands.
fn new_session(state: &ClientServerState, binding: Binding) -> Result<String, Refused> {
    let workspace = binding.workspace.clone();
    standing(state, &binding, no_longer_stands)
        .and_then(|()| {
            state
                .preview_sessions
                .open(binding)
                .map_err(|e| Box::new(internal(e)))
        })
        .inspect(|_| eprintln!("darkmux client serve: preview {workspace}: link → opened"))
}

/// `binding` stands now, or `refused`.
fn standing(
    state: &ClientServerState,
    binding: &Binding,
    refused: fn() -> Refused,
) -> Result<(), Refused> {
    stands_now(state, binding)?
        .then_some(())
        .ok_or_else(refused)
}

/// The 303 to `/` that sets session `cookie`. Kept out of caches, and the
/// link's URL is never sent on as a referrer.
fn see_other(cookie: &str) -> Response {
    let mut res = StatusCode::SEE_OTHER.into_response();
    let headers = res.headers_mut();
    headers.insert(LOCATION, HeaderValue::from_static("/"));
    headers.insert(
        SET_COOKIE,
        HeaderValue::from_str(&session_set_cookie(cookie)).expect("hex is a header value"),
    );
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    headers.insert(
        axum::http::header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    res
}

// ─── The proxy ────────────────────────────────────────────────────────────

/// In front of the client listener's gate: a request through the tailnet
/// entry on a path outside `/client/` goes to the preview its session names
/// ([`door`]); every other request goes on to the routes as before.
pub(crate) async fn front_door(
    State(state): State<ClientServerState>,
    req: Request,
    next: axum::middleware::Next,
) -> Response {
    match door(arrival_of(&state, &req), req.uri().path()) {
        Door::Darkmux => next.run(req).await,
        Door::Preview { login, from } => proxied(&state, req, (login.as_deref(), from)).await,
    }
}

/// How `req` reached the listener ([`crate::client_tailnet::arrival`]).
fn arrival_of(state: &ClientServerState, req: &Request) -> Arrival {
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0);
    crate::client_tailnet::arrival(
        peer,
        req.headers(),
        state.bound,
        state.tailnet_entry.as_ref(),
    )
}

/// `req`, through the entry as `caller`, proxied to its session's preview;
/// or the refusal.
async fn proxied(
    state: &ClientServerState,
    req: Request,
    caller: (Option<&str>, IpAddr),
) -> Response {
    match session_port(state, req.headers(), caller).await {
        Ok(port) => forward(port, req).await,
        Err(r) => *r,
    }
}

/// The app port of the preview the request's session names, once that
/// session still stands and the request comes from its user and device.
/// The cookie is looked at first, so a request with none asks Tailscale
/// nothing.
async fn session_port(
    state: &ClientServerState,
    headers: &HeaderMap,
    caller: (Option<&str>, IpAddr),
) -> Result<u16, Refused> {
    let binding = live_session(state, headers)?;
    same_device(state, &binding, caller).await?;
    (!state.previews.stopping())
        .then_some(binding.serving.port)
        .ok_or_else(stopping)
}

/// What the session the request's cookie opens is bound to, while it still
/// stands; one that no longer stands is ended.
fn live_session(state: &ClientServerState, headers: &HeaderMap) -> Result<Binding, Refused> {
    let (key, binding) = state
        .preview_sessions
        .find(&session_cookies(headers))
        .ok_or_else(no_session)?;
    let ended = || {
        state.preview_sessions.end(&key);
        no_session()
    };
    stands_now(state, &binding)?
        .then_some(binding)
        .ok_or_else(ended)
}

/// `req` sent to the preview on `port`, with its headers as
/// [`upstream_headers`] makes them, and the answer back with
/// [`drop_our_cookie`]'s. A WebSocket upgrade the preview accepts is joined
/// end to end.
async fn forward(port: u16, mut req: Request) -> Response {
    let from_browser = wants_upgrade(req.headers()).then(|| hyper::upgrade::on(&mut req));
    match send(port, upstream_request(req, port)).await {
        Ok(res) => answered(res, from_browser),
        Err(why) => {
            eprintln!("darkmux client serve: preview on 127.0.0.1:{port}: not answering ({why})");
            err(
                StatusCode::BAD_GATEWAY,
                ClientCode::Unavailable,
                "the preview is not answering: check its status in the app",
            )
        }
    }
}

/// `req` as it goes to the preview on `port`: its method, path and query,
/// [`upstream_headers`], and its body.
fn upstream_request(req: Request, port: u16) -> Request {
    let (parts, body) = req.into_parts();
    let mut out = Request::new(body);
    *out.method_mut() = parts.method;
    *out.uri_mut() = parts
        .uri
        .path_and_query()
        .map_or(Uri::from_static("/"), |pq| Uri::from(pq.clone()));
    *out.headers_mut() = upstream_headers(&parts.headers, port);
    out
}

/// One HTTP/1.1 exchange with the preview on 127.0.0.1:`port`, on a
/// connection of its own that can switch protocols.
async fn send(
    port: u16,
    req: Request,
) -> Result<axum::http::Response<hyper::body::Incoming>, String> {
    let stream = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .map_err(|e| e.to_string())?;
    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(conn.with_upgrades());
    sender.send_request(req).await.map_err(|e| e.to_string())
}

/// The preview's answer as the browser gets it; a 101 to an upgrade the
/// browser asked for is joined end to end once both sides switch.
fn answered(
    mut res: axum::http::Response<hyper::body::Incoming>,
    from_browser: Option<hyper::upgrade::OnUpgrade>,
) -> Response {
    if let (StatusCode::SWITCHING_PROTOCOLS, Some(browser)) = (res.status(), from_browser) {
        tokio::spawn(joined(browser, hyper::upgrade::on(&mut res)));
    }
    drop_our_cookie(res.headers_mut());
    res.map(axum::body::Body::new)
}

/// Copies bytes both ways between the browser's switched connection and the
/// preview's until either ends.
async fn joined(browser: hyper::upgrade::OnUpgrade, preview: hyper::upgrade::OnUpgrade) {
    if let (Ok(b), Ok(p)) = tokio::join!(browser, preview) {
        let (mut b, mut p) = (
            hyper_util::rt::TokioIo::new(b),
            hyper_util::rt::TokioIo::new(p),
        );
        let _ = tokio::io::copy_bidirectional(&mut b, &mut p).await;
    }
}

// Tests: `client_preview_proxy_tests.rs`, a module of `client_pairing`'s
// tests, so the flow runs through the real router with their harness.
