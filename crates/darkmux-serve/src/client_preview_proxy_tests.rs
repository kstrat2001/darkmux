//! (#3560, part 4) Tests for `client_preview_proxy`: the pure decisions, and
//! the whole flow driven through the real router with the pairing tests'
//! harness (this file is a module of those tests). The preview is a stand-in
//! on loopback that records each request it is sent and answers it, or
//! switches to an echo for an upgrade; no dev server runs.

use super::*;
use crate::client_preview::Serving;
use crate::client_preview_proxy::*;
use axum::http::HeaderName;

// ─── The pure decisions ───────────────────────────────────────────────────

fn tailnet(login: Option<&str>) -> crate::client_tailnet::Arrival {
    crate::client_tailnet::Arrival::Tailnet {
        login: login.map(str::to_string),
        from: PHONE_V4.parse().unwrap(),
    }
}

/// Only a request through the entry, on a path that is not darkmux's, goes
/// to a preview. `/client` and below stay darkmux's, a request from this
/// Mac or from anywhere else never reaches the proxy.
#[test]
fn only_a_tailnet_request_outside_client_reaches_a_preview() {
    use crate::client_tailnet::Arrival;
    for path in [
        "/",
        "/src/main.tsx",
        "/@vite/client",
        "/clientele",
        "//client/whoami",
        "/%63lient/whoami",
    ] {
        assert!(
            matches!(door(tailnet(Some(SAM)), path), Door::Preview { .. }),
            "{path}"
        );
    }
    for path in [
        "/client",
        "/client/",
        "/client/whoami",
        "/client/preview-link/abc",
    ] {
        assert_eq!(door(tailnet(Some(SAM)), path), Door::Darkmux, "{path}");
    }
    for arrival in [Arrival::Local, Arrival::Foreign] {
        assert_eq!(door(arrival, "/"), Door::Darkmux);
    }
    assert_eq!(
        door(tailnet(None), "/"),
        Door::Preview {
            login: None,
            from: PHONE_V4.parse().unwrap()
        },
        "a missing user is the proxy's to refuse"
    );
}

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        h.append(
            HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.parse().unwrap(),
        );
    }
    h
}

/// The preview never sees what would let it act as the device: darkmux's
/// cookie, the token, the tailnet's identity and forwarding headers. `Host`
/// and `Origin` name its loopback address; its own cookies and every other
/// header pass.
#[test]
fn the_preview_gets_no_credential_and_its_own_loopback_address() {
    let sent = headers(&[
        ("host", TAILNET_HOST),
        ("origin", "https://studio.example.ts.net:8767"),
        ("authorization", "Bearer secret-token"),
        ("cookie", "theirs=1; __Host-darkmux_preview=sess"),
        ("cookie", "__HOST-DARKMUX_PREVIEW=other; mine=2"),
        ("tailscale-user-login", SAM),
        ("tailscale-user-name", "Sam"),
        ("x-forwarded-for", PHONE_V4),
        ("x-forwarded-proto", "https"),
        ("x-forwarded-host", TAILNET_HOST),
        ("accept", "text/html"),
        ("upgrade", "websocket"),
        ("connection", "Upgrade"),
    ]);
    let out = upstream_headers(&sent, 5173);
    assert_eq!(out["host"], "127.0.0.1:5173");
    assert_eq!(out["origin"], "http://127.0.0.1:5173");
    assert_eq!(
        out.get_all("cookie").iter().collect::<Vec<_>>(),
        ["theirs=1; mine=2"]
    );
    for gone in [
        "authorization",
        "tailscale-user-login",
        "tailscale-user-name",
        "x-forwarded-for",
        "x-forwarded-proto",
        "x-forwarded-host",
    ] {
        assert!(
            !out.contains_key(gone),
            "{gone} reached the preview: {out:?}"
        );
    }
    for kept in ["accept", "upgrade", "connection"] {
        assert_eq!(out[kept], sent[kept], "{kept}");
    }
    let bare = upstream_headers(
        &headers(&[
            ("host", TAILNET_HOST),
            ("cookie", "__Host-darkmux_preview=sess"),
        ]),
        5173,
    );
    assert!(
        !bare.contains_key("origin"),
        "no Origin is made up: {bare:?}"
    );
    assert!(
        !bare.contains_key("cookie"),
        "a request with only darkmux's cookie sends none: {bare:?}"
    );
}

/// Only darkmux's cookie is read as the session, every value of it sent.
#[test]
fn the_session_is_read_from_darkmuxs_cookie_only() {
    let h = headers(&[
        ("cookie", "a=1; __Host-darkmux_preview=one"),
        ("cookie", " __host-darkmux_preview = two ;b=2"),
    ]);
    assert_eq!(session_cookies(&h), ["one", "two"]);
    assert!(session_cookies(&headers(&[(
        "cookie",
        "darkmux_preview=x; __Host-darkmux_previewx=y"
    )]))
    .is_empty());
}

/// A preview never sets darkmux's cookie, under any case or spacing, nor as
/// a nameless cookie whose value would read as it, nor any cookie with a
/// `Domain` (it would reach the tailnet's other hosts). The cookies it sets
/// for its own host reach the browser.
#[test]
fn the_preview_cannot_set_darkmuxs_cookie_or_one_for_other_hosts() {
    let mut h = headers(&[
        (
            "set-cookie",
            "__Host-darkmux_preview=planted; Path=/; Secure",
        ),
        ("set-cookie", "  __host-DARKMUX_preview =planted"),
        ("set-cookie", "=__Host-darkmux_preview=nameless; Path=/"),
        ("set-cookie", " = __Host-darkmux_preview=nameless"),
        ("set-cookie", "wide=1; Path=/; Domain=example.ts.net"),
        ("set-cookie", "wide=2; domain = .studio.example.ts.net"),
        ("set-cookie", "theirs=1; Path=/"),
        ("set-cookie", "x; __Host-darkmux_preview=not-a-name"),
        ("set-cookie", "plain=1; Path=/domain"),
        ("content-type", "text/html"),
        ("cache-control", "public, max-age=31536000, immutable"),
        ("service-worker-allowed", "/"),
    ]);
    answer_headers(&mut h);
    assert_eq!(
        h.get_all("set-cookie").iter().collect::<Vec<_>>(),
        [
            "theirs=1; Path=/",
            "x; __Host-darkmux_preview=not-a-name",
            "plain=1; Path=/domain"
        ]
    );
    assert_eq!(h["content-type"], "text/html");
    assert_eq!(
        h.get_all("cache-control").iter().collect::<Vec<_>>(),
        ["no-store"]
    );
    assert!(!h.contains_key("service-worker-allowed"));
}

/// The browser's own headers decide first: a service worker, another
/// origin, or another site is refused; the entry's own origin (any case),
/// `none` and `same-origin` pass, and so does a request that names neither.
#[test]
fn a_browsers_request_from_another_origin_or_a_service_worker_is_refused() {
    let origin = "https://studio.example.ts.net:8767";
    for refused in [
        vec![("service-worker", "script")],
        vec![("sec-fetch-dest", "ServiceWorker")],
        vec![("origin", "https://peer.example.ts.net:8767")],
        vec![("origin", "null")],
        vec![
            ("origin", origin),
            ("origin", "https://peer.example.ts.net:8767"),
        ],
        vec![("sec-fetch-site", "same-site")],
        vec![("sec-fetch-site", "cross-site")],
        vec![("sec-fetch-site", "a-future-value")],
        vec![
            ("sec-fetch-site", "same-origin"),
            ("sec-fetch-site", "cross-site"),
        ],
    ] {
        assert!(
            browser_refusal(&headers(&refused), origin).is_some(),
            "{refused:?}"
        );
    }
    for allowed in [
        vec![],
        vec![("origin", "HTTPS://Studio.Example.ts.net:8767")],
        vec![("sec-fetch-site", "none")],
        vec![
            ("sec-fetch-site", "same-origin"),
            ("sec-fetch-dest", "script"),
        ],
    ] {
        assert_eq!(
            browser_refusal(&headers(&allowed), origin),
            None,
            "{allowed:?}"
        );
    }
    let on_443 = darkmux_fleet::overlay::ServeEntry::for_port("studio.example.ts.net", 443);
    assert_eq!(entry_origin(&on_443), "https://studio.example.ts.net");
    let on_8767 = darkmux_fleet::overlay::ServeEntry::for_port("studio.example.ts.net", 8767);
    assert_eq!(entry_origin(&on_8767), origin);
}

/// `clients.preview.enabled` is read at most once per second of the clock,
/// and a failure to read it is never kept.
#[test]
fn the_preview_switch_is_read_once_a_second_and_failures_are_not_kept() {
    let s = PreviewSessions::default();
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let answer: Arc<Mutex<Result<bool, String>>> = Arc::new(Mutex::new(Ok(true)));
    let (r, a) = (reads.clone(), answer.clone());
    let read: crate::client_pairing::PreviewSwitch = Arc::new(move || {
        r.fetch_add(1, Ordering::SeqCst);
        a.lock().unwrap().clone()
    });
    assert_eq!(s.previews_on(&read, 10), Ok(true));
    *answer.lock().unwrap() = Ok(false);
    assert_eq!(s.previews_on(&read, 10), Ok(true), "the same second");
    assert_eq!(s.previews_on(&read, 11), Ok(false), "the next");
    *answer.lock().unwrap() = Err("unreadable".into());
    assert!(s.previews_on(&read, 12).is_err());
    *answer.lock().unwrap() = Ok(true);
    assert_eq!(s.previews_on(&read, 12), Ok(true), "a failure was not kept");
    assert_eq!(reads.load(Ordering::SeqCst), 4);
}

#[test]
fn the_session_cookie_is_httponly_secure_strict_at_the_root() {
    assert_eq!(
        session_set_cookie("abc"),
        "__Host-darkmux_preview=abc; HttpOnly; Secure; SameSite=Strict; Path=/"
    );
}

#[test]
fn an_upgrade_is_an_upgrade_header_with_connection_upgrade() {
    assert!(wants_upgrade(&headers(&[
        ("upgrade", "websocket"),
        ("connection", "keep-alive, Upgrade")
    ])));
    assert!(!wants_upgrade(&headers(&[("upgrade", "websocket")])));
    assert!(!wants_upgrade(&headers(&[("connection", "upgrade")])));
}

/// A link may be opened for [`LINK_TTL_SECS`] after it was issued, and
/// not at that second or later. The clock is frozen: each distance from
/// the issue is asserted, never inherited from when the suite runs.
#[test]
fn a_link_is_in_time_for_its_ttl_and_not_after() {
    let issued = 1_000_000;
    assert!(link_in_time(issued, issued));
    assert!(link_in_time(issued, issued + LINK_TTL_SECS - 1));
    assert!(!link_in_time(issued, issued + LINK_TTL_SECS));
    assert!(
        link_in_time(issued, issued - 5),
        "a clock stepped back never ages a link"
    );
}

fn a_binding() -> Binding {
    Binding {
        client_id: "c-1".into(),
        workspace: "wt1".into(),
        identity: phone_as(SAM),
        serving: Serving {
            run: "r1".into(),
            port: 5173,
        },
    }
}

fn a_client(id: &str, tailnet: Option<crate::client_tailnet::TailnetIdentity>) -> ApprovedClient {
    ApprovedClient {
        name: FIRST_CLIENT.into(),
        id: id.into(),
        created: 1,
        token_hash: "00".into(),
        auto_approved: None,
        tailnet,
    }
}

/// A session stands while its client is paired with the identity it was
/// issued to and its preview serves the same run on the same port.
#[test]
fn a_session_stands_only_while_its_client_and_preview_do() {
    let b = a_binding();
    let serving = b.serving.clone();
    let paired = [a_client("c-1", Some(phone_as(SAM)))];
    assert!(still_stands(&b, &paired, Some(&serving)));
    assert!(!still_stands(&b, &[], Some(&serving)), "revoked");
    assert!(
        !still_stands(&b, &[a_client("c-2", Some(phone_as(SAM)))], Some(&serving)),
        "another client"
    );
    assert!(
        !still_stands(&b, &[a_client("c-1", None)], Some(&serving)),
        "no identity"
    );
    assert!(
        !still_stands(&b, &[a_client("c-1", Some(phone_as(ALEX)))], Some(&serving)),
        "another identity"
    );
    assert!(!still_stands(&b, &paired, None), "the preview stopped");
    assert!(
        !still_stands(
            &b,
            &paired,
            Some(&Serving {
                run: "r2".into(),
                port: 5173
            })
        ),
        "restarted on the same port"
    );
    assert!(
        !still_stands(
            &b,
            &paired,
            Some(&Serving {
                run: "r1".into(),
                port: 5174
            })
        ),
        "another port"
    );
}

/// A code opens once, in time; a new link replaces the client's last one to
/// the same preview; a session is found only by its own cookie and ends
/// when told to.
#[test]
fn codes_open_once_and_sessions_are_found_by_their_cookie() {
    let s = PreviewSessions::default();
    let now = 1_000_000;
    let code = s.issue(a_binding(), now).unwrap();
    assert_eq!(code.len(), 64, "32 random bytes, hex");
    assert_eq!(s.redeem("not-the-code", now), None);
    assert_eq!(s.redeem(&code, now), Some(a_binding()));
    assert_eq!(s.redeem(&code, now), None, "used up");
    let late = s.issue(a_binding(), now).unwrap();
    assert_eq!(s.redeem(&late, now + LINK_TTL_SECS), None, "too late");
    assert_eq!(
        s.redeem(&late, now),
        None,
        "a late presentation uses it up too"
    );
    let first = s.issue(a_binding(), now).unwrap();
    let second = s.issue(a_binding(), now).unwrap();
    assert_eq!(s.redeem(&first, now), None, "replaced by the second");
    assert_eq!(s.redeem(&second, now), Some(a_binding()));

    let cookie = s.open(a_binding()).unwrap();
    assert_ne!(cookie, code);
    assert!(s.find(&["nope".into()]).is_none());
    let found = s.find(&["nope".into(), cookie.clone()]).unwrap();
    assert_eq!(found.binding, a_binding());
    assert_ne!(
        found.key, cookie,
        "the key is the cookie's hash, never the cookie"
    );
    let again = s.open(a_binding()).unwrap();
    assert!(
        s.find(std::slice::from_ref(&cookie)).is_none(),
        "replaced by the next session to the same preview"
    );
    let key = s.find(std::slice::from_ref(&again)).unwrap().key;
    s.end(&key);
    assert!(s.find(&[again]).is_none(), "ended");
}

// ─── Through the router ───────────────────────────────────────────────────

/// A stand-in preview on loopback: every request's head, as sent, and the
/// answers it gives. A plain request gets 200 with its own head as the body
/// and two cookies set, one of them named as darkmux's; an upgrade gets 101
/// and then every byte echoed back.
struct StandIn {
    port: u16,
    heads: Arc<Mutex<Vec<String>>>,
    /// `/hold` requests wait for one of these each.
    release: Arc<tokio::sync::Semaphore>,
    /// `/hold` requests waiting now, and the most ever at once.
    holding: Arc<std::sync::atomic::AtomicUsize>,
    most: Arc<std::sync::atomic::AtomicUsize>,
}

impl StandIn {
    async fn start() -> StandIn {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let heads = Arc::new(Mutex::new(Vec::new()));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let holding = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let most = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hold = Hold {
            release: release.clone(),
            holding: holding.clone(),
            most: most.clone(),
        };
        let seen = heads.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(stand_in_answer(stream, seen.clone(), hold.clone()));
            }
        });
        StandIn {
            port,
            heads,
            release,
            holding,
            most,
        }
    }

    fn last(&self) -> String {
        self.heads
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }

    fn count(&self) -> usize {
        self.heads.lock().unwrap().len()
    }
}

/// What a `/hold` request waits on, and the count of those waiting.
#[derive(Clone)]
struct Hold {
    release: Arc<tokio::sync::Semaphore>,
    holding: Arc<std::sync::atomic::AtomicUsize>,
    most: Arc<std::sync::atomic::AtomicUsize>,
}

async fn stand_in_answer(
    mut stream: tokio::net::TcpStream,
    seen: Arc<Mutex<Vec<String>>>,
    hold: Hold,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).await.unwrap_or(0) == 0 {
            return;
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).to_string();
    seen.lock().unwrap().push(head.clone());
    if head
        .to_ascii_lowercase()
        .contains("\r\nupgrade: websocket\r\n")
    {
        let _ = stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\n\r\n").await;
        let (mut r, mut w) = stream.split();
        let _ = tokio::io::copy(&mut r, &mut w).await;
        return;
    }
    if head.starts_with("GET /hold") {
        let now = hold.holding.fetch_add(1, Ordering::SeqCst) + 1;
        hold.most.fetch_max(now, Ordering::SeqCst);
        hold.release.acquire().await.unwrap().forget();
        hold.holding.fetch_sub(1, Ordering::SeqCst);
    }
    if head.starts_with("GET /slowbody") {
        // The head at once, the body once released.
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\n")
            .await;
        hold.release.acquire().await.unwrap().forget();
        let _ = stream.write_all(b"done").await;
        return;
    }
    if head.starts_with("GET /hang") {
        // A dev server that never answers: the connection stays open.
        let _ = stream.read(&mut [0u8; 1]).await;
        return;
    }
    let answer = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nset-cookie: __Host-darkmux_preview=planted; Path=/; Secure\r\n\
         set-cookie: =__Host-darkmux_preview=nameless; Path=/\r\nset-cookie: wide=1; Domain=example.ts.net; Path=/\r\n\
         set-cookie: theirs=1; Path=/\r\ncache-control: public, max-age=31536000, immutable\r\n\
         service-worker-allowed: /\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{head}",
        head.len()
    );
    let _ = stream.write_all(answer.as_bytes()).await;
}

/// A response's status, headers and body, whatever it is.
async fn sent(h: &Harness, mut req: Request<Body>) -> (u16, HeaderMap, String) {
    if req
        .extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .is_none()
    {
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(crate::local_peer::operator(SocketAddr::from(([127, 0, 0, 1], 5555)))));
    }
    let resp = router(h.state.clone()).oneshot(req).await.unwrap();
    let (status, headers) = (resp.status().as_u16(), resp.headers().clone());
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, headers, String::from_utf8_lossy(&body).to_string())
}

/// A JSON refusal's status and code.
fn coded(answer: &(u16, HeaderMap, String)) -> (u16, String) {
    let v: Value =
        serde_json::from_str(&answer.2).unwrap_or_else(|e| panic!("not JSON ({e}): {}", answer.2));
    (answer.0, v["code"].as_str().unwrap_or_default().to_string())
}

/// Sam's phone, paired over the tailnet, its workspace `wt1` previewed by a
/// stand-in, with `client serve` on the tailnet. Its token, and the stand-in.
async fn a_previewing_phone(h: &mut Harness) -> (String, StandIn) {
    let token = paired(h).await;
    paired_as(h, FIRST_CLIENT, SAM);
    on_the_tailnet(h);
    let preview = StandIn::start().await;
    h.state.previews.ready_for_test("wt1", preview.port);
    (token, preview)
}

/// What the phone sends through the entry, from `addr`, as `login`.
fn phone_at(addr: &'static str, login: Option<&'static str>) -> Vec<(&'static str, &'static str)> {
    from_addr(forwarded(login), addr)
}

/// The link the phone gets for `wt1` through the entry.
async fn link_for(h: &Harness, token: &str) -> String {
    let auth = bearer(token);
    let req = adding(
        post(
            "/client/workspaces/wt1/preview/link",
            None,
            serde_json::json!({}),
        ),
        &with_token(&forwarded(Some(SAM)), Some(&auth)),
    );
    let answer = sent(h, req).await;
    assert_eq!(answer.0, 200, "{}", answer.2);
    let v: Value = serde_json::from_str(&answer.2).unwrap();
    assert_eq!(v["expires_in_secs"], LINK_TTL_SECS);
    v["url"].as_str().unwrap().to_string()
}

/// The path of a link's URL, as the browser asks the entry for it.
fn path_of(url: &str) -> String {
    let rest = url
        .strip_prefix("https://studio.example.ts.net:8767")
        .unwrap_or_else(|| panic!("{url}"));
    assert!(rest.starts_with("/client/preview-link/"), "{url}");
    rest.to_string()
}

/// The browser opens `url` through the entry, from `from`.
async fn open_link(h: &Harness, url: &str, from: &[(&str, &str)]) -> (u16, HeaderMap, String) {
    sent(h, get(&path_of(url), from)).await
}

/// The session cookie a 303 set, as the browser sends it back.
fn session_from(answer: &(u16, HeaderMap, String)) -> String {
    assert_eq!(answer.0, 303, "{}", answer.2);
    let set = answer.1["set-cookie"].to_str().unwrap();
    set.split(';').next().unwrap().to_string()
}

/// The phone's browser, holding `cookie`, asks the entry for `path`.
async fn browse(
    h: &Harness,
    path: &str,
    cookie: &str,
    from: &[(&str, &str)],
) -> (u16, HeaderMap, String) {
    let mut all = from.to_vec();
    all.push(("cookie", cookie));
    sent(h, get(path, &all)).await
}

/// The whole flow: the app asks for a link through the entry, the browser
/// opens it and gets an HttpOnly, Secure, Strict session at `/`, and every
/// path outside `/client/` is then the preview's, sent there without the
/// device's credentials and with its own loopback `Host` and `Origin`.
#[tokio::test]
async fn a_phone_opens_its_preview_through_the_entry() {
    let mut h = harness();
    let (token, preview) = a_previewing_phone(&mut h).await;
    let url = link_for(&h, &token).await;
    let opened = open_link(&h, &url, &forwarded(Some(SAM))).await;
    assert_eq!(opened.1["location"], "/");
    assert_eq!(opened.1["cache-control"], "no-store");
    assert_eq!(opened.1["referrer-policy"], "no-referrer");
    let set = opened.1["set-cookie"].to_str().unwrap();
    assert!(
        set.ends_with("; HttpOnly; Secure; SameSite=Strict; Path=/")
            && set.starts_with("__Host-darkmux_preview="),
        "{set}"
    );
    let cookie = session_from(&opened);
    let auth = bearer(&token);
    let mut from = forwarded(Some(SAM));
    from.extend([
        ("origin", "https://studio.example.ts.net:8767"),
        ("authorization", auth.as_str()),
    ]);
    let page = browse(
        &h,
        "/src/main.tsx?t=1",
        &format!("{cookie}; theirs=1"),
        &from,
    )
    .await;
    assert_eq!(page.0, 200, "{}", page.2);
    let head = preview.last().to_ascii_lowercase();
    assert!(
        head.starts_with("get /src/main.tsx?t=1 http/1.1\r\n"),
        "{head}"
    );
    let port = preview.port;
    assert!(
        head.contains(&format!("\r\nhost: 127.0.0.1:{port}\r\n")),
        "{head}"
    );
    assert!(
        head.contains(&format!("\r\norigin: http://127.0.0.1:{port}\r\n")),
        "{head}"
    );
    assert!(head.contains("\r\ncookie: theirs=1\r\n"), "{head}");
    for leak in [
        "darkmux_preview",
        "authorization",
        "tailscale-",
        "x-forwarded-",
        "studio.example",
        &token,
    ] {
        assert!(
            !head.contains(&leak.to_ascii_lowercase()),
            "{leak} reached the preview: {head}"
        );
    }
    assert_eq!(
        page.1.get_all("set-cookie").iter().collect::<Vec<_>>(),
        ["theirs=1; Path=/"],
        "the planted cookie is dropped"
    );
}

/// A link opens once: the second time, and once its minute is up, it is
/// unknown. The clock is frozen and moved by hand.
#[tokio::test]
async fn a_link_opens_once_and_only_within_its_ttl() {
    let mut h = harness();
    let (token, _preview) = a_previewing_phone(&mut h).await;
    let url = link_for(&h, &token).await;
    assert_eq!(open_link(&h, &url, &forwarded(Some(SAM))).await.0, 303);
    let again = open_link(&h, &url, &forwarded(Some(SAM))).await;
    assert_eq!(coded(&again), (404, "not_found".to_string()));
    let url = link_for(&h, &token).await;
    h.clock.fetch_add(LINK_TTL_SECS - 1, Ordering::SeqCst);
    assert_eq!(
        open_link(&h, &url, &forwarded(Some(SAM))).await.0,
        303,
        "a second before its time is up"
    );
    let url = link_for(&h, &token).await;
    h.clock.fetch_add(LINK_TTL_SECS, Ordering::SeqCst);
    assert_eq!(
        coded(&open_link(&h, &url, &forwarded(Some(SAM))).await),
        (404, "not_found".to_string())
    );
}

/// A link is for a device paired over the tailnet, through the entry: asked
/// for or opened from this Mac it is `tailnet_only`, and a code shown to this
/// Mac is not used up by it.
#[tokio::test]
async fn a_link_is_asked_for_and_opened_only_through_the_entry() {
    let mut h = harness();
    let (token, _preview) = a_previewing_phone(&mut h).await;
    let local = sent(
        &h,
        adding(
            post(
                "/client/workspaces/wt1/preview/link",
                Some(&token),
                serde_json::json!({}),
            ),
            &[("host", BOUND)],
        ),
    )
    .await;
    assert_eq!(
        coded(&local),
        (403, "tailnet_only".to_string()),
        "{}",
        local.2
    );
    let url = link_for(&h, &token).await;
    let here = sent(&h, get(&path_of(&url), &[("host", BOUND)])).await;
    assert_eq!(coded(&here), (403, "tailnet_only".to_string()));
    assert_eq!(
        open_link(&h, &url, &forwarded(Some(SAM))).await.0,
        303,
        "still good through the entry"
    );
}

/// A token paired on this Mac, or a preview that is not ready, gets no link;
/// nor does another app's workspace.
#[tokio::test]
async fn only_a_ready_preview_of_a_remotely_paired_devices_own_gets_a_link() {
    let mut h = harness();
    let (token, _preview) = a_previewing_phone(&mut h).await;
    let auth = bearer(&token);
    let ask = |name: &str| {
        adding(
            post(
                &format!("/client/workspaces/{name}/preview/link"),
                None,
                serde_json::json!({}),
            ),
            &with_token(&forwarded(Some(SAM)), Some(&auth)),
        )
    };
    assert!(h.state.previews.stop("wt1"));
    assert_eq!(
        coded(&sent(&h, ask("wt1")).await),
        (409, "conflict".to_string())
    );
    // (#3683) A folder darkmux made for no app is nobody's to preview.
    std::fs::create_dir_all(h.base.join("theirs")).unwrap();
    h.state.previews.ready_for_test("theirs", 1);
    assert_eq!(
        coded(&sent(&h, ask("theirs")).await),
        (403, "forbidden".to_string())
    );
    *h.preview_on.lock().unwrap() = false;
    h.state.previews.ready_for_test("wt1", 1);
    assert_eq!(
        coded(&sent(&h, ask("wt1")).await),
        (403, "preview_disabled".to_string())
    );
}

/// The link opens only for the tailnet user and device it was issued to;
/// a wrong one uses it up all the same.
#[tokio::test]
async fn a_link_opens_only_for_the_device_and_user_it_was_made_for() {
    let mut h = harness();
    let (token, _preview) = a_previewing_phone(&mut h).await;
    h.devices.place(&laptop_node(), &[LAPTOP_V4]);
    h.devices.place(
        &node_of("nALEX1CNTRL", "alexs-phone", ALEX_V4, ALEX),
        &[ALEX_V4],
    );
    let url = link_for(&h, &token).await;
    let laptop = open_link(&h, &url, &phone_at(LAPTOP_V4, Some(SAM))).await;
    assert_eq!(coded(&laptop), (403, "remote_device_mismatch".to_string()));
    assert_eq!(
        coded(&open_link(&h, &url, &forwarded(Some(SAM))).await),
        (404, "not_found".to_string()),
        "used up"
    );
    let url = link_for(&h, &token).await;
    let alex = open_link(&h, &url, &phone_at(ALEX_V4, Some(ALEX))).await;
    assert_eq!(coded(&alex), (403, "remote_identity_mismatch".to_string()));
    let url = link_for(&h, &token).await;
    let untagged = open_link(&h, &url, &forwarded(None)).await;
    assert_eq!(
        coded(&untagged),
        (403, "remote_identity_missing".to_string())
    );
}

/// A link made before its preview restarted, or its device was revoked, no
/// longer opens.
#[tokio::test]
async fn a_link_whose_preview_restarted_no_longer_opens() {
    let mut h = harness();
    let (token, preview) = a_previewing_phone(&mut h).await;
    let url = link_for(&h, &token).await;
    h.state.previews.ready_for_test("wt1", preview.port);
    assert_eq!(
        coded(&open_link(&h, &url, &forwarded(Some(SAM))).await),
        (409, "conflict".to_string())
    );
    let url = link_for(&h, &token).await;
    assert!(revoke(&h.store, FIRST_CLIENT).unwrap());
    assert_eq!(
        coded(&open_link(&h, &url, &forwarded(Some(SAM))).await),
        (409, "conflict".to_string())
    );
}

/// Outside `/client/`, a request through the entry with no session, or a
/// made-up one, is refused and never proxied, and costs no `whois`.
#[tokio::test]
async fn no_session_is_refused_and_never_proxied() {
    let mut h = harness();
    let (_token, preview) = a_previewing_phone(&mut h).await;
    let asked = h.devices.asked();
    for cookie in [
        "",
        "__Host-darkmux_preview=",
        "__Host-darkmux_preview=0123",
        "theirs=1",
    ] {
        let answer = browse(&h, "/", cookie, &forwarded(Some(SAM))).await;
        assert_eq!(
            coded(&answer),
            (403, "no_preview_session".to_string()),
            "{cookie:?}"
        );
    }
    assert_eq!(preview.count(), 0, "nothing reached the preview");
    assert_eq!(
        h.devices.asked(),
        asked,
        "no whois for a request with no session"
    );
}

/// A session is used only from the user and device it was opened for.
#[tokio::test]
async fn a_session_works_only_from_its_device_and_user() {
    let mut h = harness();
    let (token, preview) = a_previewing_phone(&mut h).await;
    h.devices.place(&laptop_node(), &[LAPTOP_V4]);
    let cookie = session_of(&h, &token).await;
    let laptop = browse(&h, "/", &cookie, &phone_at(LAPTOP_V4, Some(SAM))).await;
    assert_eq!(coded(&laptop), (403, "remote_device_mismatch".to_string()));
    let alex = browse(&h, "/", &cookie, &forwarded(Some(ALEX))).await;
    assert_eq!(coded(&alex), (403, "remote_identity_mismatch".to_string()));
    let none = browse(&h, "/", &cookie, &forwarded(None)).await;
    assert_eq!(coded(&none), (403, "remote_identity_missing".to_string()));
    assert_eq!(preview.count(), 0);
    assert_eq!(
        browse(&h, "/", &cookie, &from_addr(forwarded(Some(SAM)), PHONE_V6))
            .await
            .0,
        200,
        "its other address"
    );
}

/// The phone opens a new link to `wt1`: the session cookie it then holds.
async fn session_of(h: &Harness, token: &str) -> String {
    session_from(&open_link(h, &link_for(h, token).await, &forwarded(Some(SAM))).await)
}

/// A session ends with its preview (stopped, or restarted even on the same
/// port), with its device's pairing, and with `client serve`.
#[tokio::test]
async fn a_session_ends_with_its_preview_its_device_and_client_serve() {
    let mut h = harness();
    let (token, preview) = a_previewing_phone(&mut h).await;
    let dead = |answer: (u16, HeaderMap, String)| {
        assert_eq!(coded(&answer), (403, "no_preview_session".to_string()))
    };

    let cookie = session_of(&h, &token).await;
    assert_eq!(browse(&h, "/", &cookie, &forwarded(Some(SAM))).await.0, 200);
    assert!(h.state.previews.stop("wt1"));
    dead(browse(&h, "/", &cookie, &forwarded(Some(SAM))).await);
    h.state.previews.ready_for_test("wt1", preview.port);
    dead(browse(&h, "/", &cookie, &forwarded(Some(SAM))).await);

    let cookie = session_of(&h, &token).await;
    h.state.previews.ready_for_test("wt1", preview.port);
    dead(browse(&h, "/", &cookie, &forwarded(Some(SAM))).await);

    let cookie = session_of(&h, &token).await;
    h.state.preview_sessions = Arc::new(PreviewSessions::default());
    dead(browse(&h, "/", &cookie, &forwarded(Some(SAM))).await);

    let cookie = session_of(&h, &token).await;
    assert!(revoke(&h.store, FIRST_CLIENT).unwrap());
    dead(browse(&h, "/", &cookie, &forwarded(Some(SAM))).await);
    let reached = preview.count();
    assert_eq!(
        reached, 1,
        "only the first request, while its session stood"
    );
}

/// While `client serve` stops, a session's request is refused.
#[tokio::test]
async fn a_session_is_refused_while_client_serve_stops() {
    let mut h = harness();
    let (token, preview) = a_previewing_phone(&mut h).await;
    let cookie = session_of(&h, &token).await;
    h.state.previews.begin_shutdown();
    assert_eq!(
        coded(&browse(&h, "/", &cookie, &forwarded(Some(SAM))).await),
        (503, "unavailable".to_string())
    );
    assert_eq!(preview.count(), 0);
}

/// From this Mac a path outside `/client/` is the router's JSON 404 as
/// before, whatever cookie it carries; from anywhere else, `not_forwarded`.
#[tokio::test]
async fn off_the_tailnet_nothing_reaches_a_preview() {
    let mut h = harness();
    let (token, preview) = a_previewing_phone(&mut h).await;
    let cookie = session_of(&h, &token).await;
    let local = browse(&h, "/", &cookie, &[("host", BOUND)]).await;
    assert_eq!(coded(&local), (404, "not_found".to_string()));
    assert_eq!(
        serde_json::from_str::<Value>(&local.2).unwrap()["error"],
        "no such route"
    );
    let mut foreign = forwarded(Some(SAM));
    foreign.push(("via", "1.1 another-proxy"));
    assert_eq!(
        coded(&browse(&h, "/", &cookie, &foreign).await),
        (403, "not_forwarded".to_string())
    );
    assert_eq!(preview.count(), 0);
}

/// `router` served as `client serve` serves it
/// ([`crate::client_pairing::serve_client_listener`]) on a real loopback
/// listener (an upgrade needs a real connection); its address.
async fn listening(h: &Harness) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(h.state.clone());
    let (stop, stopped) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let _running = stop;
        crate::client_pairing::serve_client_listener(listener, app, stopped).await
    });
    addr
}

/// The phone's browser asks `addr` through the entry for a WebSocket on `/`,
/// holding `cookie`, with `extra` header lines: the connection and the
/// answer's head.
async fn socket(addr: SocketAddr, cookie: &str, extra: &str) -> (tokio::net::TcpStream, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut head = String::from(
        "GET /?token=vite HTTP/1.1\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-version: 13\r\n\
         sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
    );
    for (k, v) in forwarded(Some(SAM)) {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!("cookie: {cookie}\r\n{extra}\r\n"));
    s.write_all(head.as_bytes()).await.unwrap();
    let mut got = Vec::new();
    let mut byte = [0u8; 1];
    while !got.ends_with(b"\r\n\r\n") {
        if s.read(&mut byte).await.unwrap_or(0) == 0 {
            break;
        }
        got.push(byte[0]);
    }
    (s, String::from_utf8_lossy(&got).to_string())
}

/// Whether `s` still echoes: `true` when the bytes come back, `false` when
/// the connection is closed instead (within ten seconds either way).
async fn still_echoes(s: &mut tokio::net::TcpStream) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if s.write_all(b"hmr ping").await.is_err() {
        return false;
    }
    let mut echoed = [0u8; 8];
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        s.read_exact(&mut echoed),
    )
    .await;
    match read.expect("neither echoed nor closed within ten seconds") {
        Ok(_) => {
            assert_eq!(&echoed, b"hmr ping");
            true
        }
        Err(_) => false,
    }
}

/// Waits up to `within` for `s` to be closed by the other end; panics if it
/// is not. Anything still in flight before the close is read and dropped.
async fn closes_within(s: &mut tokio::net::TcpStream, within: std::time::Duration) {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 64];
    let closed = tokio::time::timeout(within, async {
        while s.read(&mut buf).await.is_ok_and(|n| n > 0) {}
    });
    closed.await.expect("the socket was not closed");
}

/// A WebSocket upgrade (Vite's live reload) goes through to the preview and
/// its bytes both ways, over a real listener.
#[tokio::test]
async fn a_websocket_upgrade_reaches_the_preview_and_echoes() {
    let mut h = harness();
    let (token, preview) = a_previewing_phone(&mut h).await;
    let cookie = session_of(&h, &token).await;
    let addr = listening(&h).await;
    let (mut s, answer) = socket(
        addr,
        &cookie,
        "origin: https://studio.example.ts.net:8767\r\n",
    )
    .await;
    assert!(answer.starts_with("HTTP/1.1 101"), "{answer}");
    assert!(still_echoes(&mut s).await);
    let seen = preview.last().to_ascii_lowercase();
    assert!(
        seen.contains(&format!(
            "\r\norigin: http://127.0.0.1:{}\r\n",
            preview.port
        )),
        "{seen}"
    );
    assert!(
        seen.contains("\r\nupgrade: websocket\r\n") && !seen.contains("darkmux_preview"),
        "{seen}"
    );
}

/// (#3560 review) An open WebSocket ends with its session: when the device
/// is revoked, its preview stops or restarts, previews are turned off, or
/// `client serve` begins to stop: the socket closes within
/// [`RECHECK_EVERY`] (it is checked again that often), and bytes written
/// after are not echoed.
#[tokio::test]
async fn an_open_websocket_ends_when_its_session_does() {
    type End = fn(&mut Harness, u16);
    let ends: [(&str, End); 5] = [
        ("revoked", |h, _| {
            assert!(revoke(&h.store, FIRST_CLIENT).unwrap())
        }),
        ("stopped", |h, _| assert!(h.state.previews.stop("wt1"))),
        ("restarted", |h, port| {
            h.state.previews.ready_for_test("wt1", port);
        }),
        ("previews off", |h, _| {
            *h.preview_on.lock().unwrap() = false;
            h.clock.fetch_add(1, Ordering::SeqCst);
        }),
        ("client serve stopping", |h, _| {
            h.state.previews.begin_shutdown()
        }),
    ];
    for (why, end) in ends {
        let mut h = harness();
        let (token, preview) = a_previewing_phone(&mut h).await;
        let cookie = session_of(&h, &token).await;
        let addr = listening(&h).await;
        let (mut s, answer) = socket(addr, &cookie, "").await;
        assert!(answer.starts_with("HTTP/1.1 101"), "{why}: {answer}");
        assert!(still_echoes(&mut s).await, "{why}: open");
        end(&mut h, preview.port);
        closes_within(&mut s, RECHECK_EVERY * 3).await;
        assert!(
            !still_echoes(&mut s).await,
            "{why}: the socket outlived its session"
        );
    }
}

/// An answer the preview is still streaming (a long download, a dev
/// server's event stream) ends with its session, as an open WebSocket
/// does: when the device is revoked, its preview stops or restarts,
/// previews are turned off, or `client serve` begins to stop, the body
/// fails within [`RECHECK_EVERY`] instead of streaming on.
#[tokio::test]
async fn a_streaming_answer_ends_when_its_session_does() {
    type End = fn(&mut Harness, u16);
    let ends: [(&str, End); 5] = [
        ("revoked", |h, _| {
            assert!(revoke(&h.store, FIRST_CLIENT).unwrap())
        }),
        ("stopped", |h, _| assert!(h.state.previews.stop("wt1"))),
        ("restarted", |h, port| {
            h.state.previews.ready_for_test("wt1", port);
        }),
        ("previews off", |h, _| {
            *h.preview_on.lock().unwrap() = false;
            h.clock.fetch_add(1, Ordering::SeqCst);
        }),
        ("client serve stopping", |h, _| {
            h.state.previews.begin_shutdown()
        }),
    ];
    for (why, end) in ends {
        let mut h = harness();
        let (token, preview) = a_previewing_phone(&mut h).await;
        let cookie = session_of(&h, &token).await;
        let mut from = forwarded(Some(SAM));
        from.push(("cookie", cookie.as_str()));
        let mut req = get("/slowbody", &from);
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(SocketAddr::from((
                [127, 0, 0, 1],
                5555,
            ))));
        let res = router(h.state.clone()).oneshot(req).await.unwrap();
        assert_eq!(res.status(), 200, "{why}: its head came at once");
        end(&mut h, preview.port);
        let body =
            tokio::time::timeout(RECHECK_EVERY * 3, axum::body::to_bytes(res.into_body(), 64))
                .await;
        assert!(
            matches!(body, Ok(Err(_))),
            "{why}: the answer outlived its session: {body:?}"
        );
    }
}

/// A link is issued only to a request through the entry from a client paired
/// over the tailnet.
#[test]
fn a_link_is_issued_only_through_the_entry_to_a_remotely_paired_client() {
    let entry = darkmux_fleet::overlay::ServeEntry::for_port("studio.example.ts.net", 8767);
    let sam = phone_as(SAM);
    assert_eq!(
        link_asker(Some(&entry), RequestVia::Tailnet, Some(&sam)),
        Some((&entry, sam.clone()))
    );
    assert_eq!(
        link_asker(Some(&entry), RequestVia::Local, Some(&sam)),
        None,
        "from this Mac"
    );
    assert_eq!(
        link_asker(None, RequestVia::Tailnet, Some(&sam)),
        None,
        "no entry"
    );
    assert_eq!(
        link_asker(Some(&entry), RequestVia::Tailnet, None),
        None,
        "paired on this Mac"
    );
}

/// A preview that stopped answering on its port is a coded 502, naming no
/// address the device could use.
#[tokio::test]
async fn a_preview_that_does_not_answer_is_a_bad_gateway() {
    let mut h = harness();
    let (token, _preview) = a_previewing_phone(&mut h).await;
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let serving = h.state.previews.ready_for_test("wt1", port);
    let cookie = session_of(&h, &token).await;
    let answer = browse(&h, "/", &cookie, &forwarded(Some(SAM))).await;
    assert_eq!(coded(&answer), (502, "unavailable".to_string()));
    assert!(
        !answer.2.contains(&serving.port.to_string()),
        "{}",
        answer.2
    );
}

/// The real preview path: a preview started from its recipe, ready, then
/// opened through a link, serves its own page at the entry's root.
#[tokio::test]
async fn a_preview_started_from_its_recipe_is_served_at_the_entrys_root() {
    let mut h = harness();
    let token = paired(&h).await;
    paired_as(&h, FIRST_CLIENT, SAM);
    let (mine, name, _ws) = served_workspace(&h, &token).await;
    recipe(
        &mine,
        serde_json::json!({"command": serve(), "env": {"PORT": "{port}"}, "ready": {"http": "/"}}),
    );
    assert_eq!(start_preview(&h, &token, &name).await.0, 202);
    assert_eq!(
        preview_until(&h, &token, &name, &["ready", "failed"]).await["status"],
        "ready"
    );
    on_the_tailnet(&mut h);
    let auth = bearer(&token);
    let req = adding(
        post(
            &format!("/client/workspaces/{name}/preview/link"),
            None,
            serde_json::json!({}),
        ),
        &with_token(&forwarded(Some(SAM)), Some(&auth)),
    );
    let link = sent(&h, req).await;
    assert_eq!(link.0, 200, "{}", link.2);
    let url = serde_json::from_str::<Value>(&link.2).unwrap()["url"]
        .as_str()
        .unwrap()
        .to_string();
    let cookie = session_from(&open_link(&h, &url, &forwarded(Some(SAM))).await);
    let page = browse(&h, "/", &cookie, &forwarded(Some(SAM))).await;
    assert_eq!((page.0, page.2.as_str()), (200, "hello preview"));
    assert!(h.state.previews.stop(&name));
    assert_eq!(
        coded(&browse(&h, "/", &cookie, &forwarded(Some(SAM))).await),
        (403, "no_preview_session".to_string())
    );
}

// ─── (#3560 review) One origin, many previews ─────────────────────────────

/// A browser's request for a service worker script is refused, never sent
/// to the preview: a worker would outlive this preview and serve the next.
#[tokio::test]
async fn a_service_worker_script_is_refused() {
    let mut h = harness();
    let (token, preview) = a_previewing_phone(&mut h).await;
    let cookie = session_of(&h, &token).await;
    for header in [
        ("service-worker", "script"),
        ("sec-fetch-dest", "serviceworker"),
    ] {
        let mut from = forwarded(Some(SAM));
        from.push(header);
        assert_eq!(
            coded(&browse(&h, "/sw.js", &cookie, &from).await),
            (403, "forbidden".to_string()),
            "{header:?}"
        );
    }
    assert_eq!(preview.count(), 0);
}

/// A preview's answer is never cached and never widens a service worker's
/// scope, whatever it says.
#[tokio::test]
async fn a_previews_answer_is_never_cached_and_names_no_worker_scope() {
    let mut h = harness();
    let (token, _preview) = a_previewing_phone(&mut h).await;
    let cookie = session_of(&h, &token).await;
    let page = browse(&h, "/src/main.tsx", &cookie, &forwarded(Some(SAM))).await;
    assert_eq!(page.0, 200);
    assert_eq!(
        page.1.get_all("cache-control").iter().collect::<Vec<_>>(),
        ["no-store"]
    );
    assert!(
        !page.1.contains_key("service-worker-allowed"),
        "{:?}",
        page.1
    );
}

/// Opening a link clears what the last preview left in this browser for the
/// address (its cache and storage, service workers included), never its
/// cookies: that would clear the session it sets.
#[tokio::test]
async fn opening_a_link_clears_the_last_previews_cache_and_storage() {
    let mut h = harness();
    let (token, _preview) = a_previewing_phone(&mut h).await;
    let opened = open_link(&h, &link_for(&h, &token).await, &forwarded(Some(SAM))).await;
    assert_eq!(opened.0, 303);
    assert_eq!(
        opened.1["clear-site-data"],
        r#""cache", "storage", "executionContexts""#
    );
    assert!(!opened.1["clear-site-data"]
        .to_str()
        .unwrap()
        .contains("cookies"));
}

/// Only the entry's own origin reaches a preview, rewritten to the
/// preview's: another tailnet host is "same site" (`ts.net` is a public
/// suffix), so SameSite does not stop it. `Sec-Fetch-Site` must be `none` or
/// `same-origin` when sent. A WebSocket is held to the same.
#[tokio::test]
async fn only_the_entrys_own_origin_reaches_a_preview() {
    let mut h = harness();
    let (token, preview) = a_previewing_phone(&mut h).await;
    let cookie = session_of(&h, &token).await;
    let with = |extra: &[(&'static str, &'static str)]| {
        let mut from = forwarded(Some(SAM));
        from.extend_from_slice(extra);
        from
    };
    for refused in [
        vec![("origin", "https://peer.example.ts.net:8767")],
        vec![("origin", "http://studio.example.ts.net:8767")],
        vec![("origin", "https://studio.example.ts.net:9999")],
        vec![("origin", "null")],
        vec![("sec-fetch-site", "same-site")],
        vec![("sec-fetch-site", "cross-site")],
        vec![
            ("origin", "https://studio.example.ts.net:8767"),
            ("sec-fetch-site", "same-site"),
        ],
    ] {
        assert_eq!(
            coded(&browse(&h, "/", &cookie, &with(&refused)).await),
            (403, "forbidden".to_string()),
            "{refused:?}"
        );
    }
    assert_eq!(preview.count(), 0, "nothing refused reached the preview");
    for allowed in [
        vec![
            ("origin", "https://STUDIO.example.ts.net:8767"),
            ("sec-fetch-site", "same-origin"),
        ],
        vec![("sec-fetch-site", "none")],
        vec![],
    ] {
        assert_eq!(
            browse(&h, "/", &cookie, &with(&allowed)).await.0,
            200,
            "{allowed:?}"
        );
    }
    let addr = listening(&h).await;
    let (_s, answer) = socket(
        addr,
        &cookie,
        "origin: https://peer.example.ts.net:8767\r\n",
    )
    .await;
    assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
}

/// Viewing a preview counts as asking about it: the idle check never stops
/// a preview a device is looking at.
#[tokio::test]
async fn viewing_a_preview_keeps_it_from_idling() {
    let mut h = harness();
    let (token, _preview) = a_previewing_phone(&mut h).await;
    let cookie = session_of(&h, &token).await;
    h.clock.fetch_add(14 * 60, Ordering::SeqCst);
    assert_eq!(browse(&h, "/", &cookie, &forwarded(Some(SAM))).await.0, 200);
    h.clock.fetch_add(2 * 60, Ordering::SeqCst);
    assert_eq!(
        h.state.previews.stop_idle(15),
        Vec::<String>::new(),
        "asked two minutes ago, by the page"
    );
    h.clock.fetch_add(15 * 60, Ordering::SeqCst);
    assert_eq!(
        h.state.previews.stop_idle(15),
        ["wt1"],
        "unviewed, it idles as before"
    );
}

/// Turning previews off ends a session and refuses its requests; turning
/// them on again does not bring it back. The setting is read at most once
/// per second of the clock.
#[tokio::test]
async fn turning_previews_off_ends_sessions() {
    let mut h = harness();
    let (token, preview) = a_previewing_phone(&mut h).await;
    let cookie = session_of(&h, &token).await;
    assert_eq!(browse(&h, "/", &cookie, &forwarded(Some(SAM))).await.0, 200);
    *h.preview_on.lock().unwrap() = false;
    h.clock.fetch_add(1, Ordering::SeqCst);
    assert_eq!(
        coded(&browse(&h, "/", &cookie, &forwarded(Some(SAM))).await),
        (403, "no_preview_session".to_string())
    );
    *h.preview_on.lock().unwrap() = true;
    h.clock.fetch_add(1, Ordering::SeqCst);
    assert_eq!(
        coded(&browse(&h, "/", &cookie, &forwarded(Some(SAM))).await),
        (403, "no_preview_session".to_string())
    );
    assert_eq!(preview.count(), 1);
}

// ─── (#3560 review) Bounds ─────────────────────────────────────────────────

/// Ending a session closes what its sockets wait on at once: ended,
/// replaced by a new session to the same preview, or every session ended.
#[tokio::test]
async fn ending_a_session_closes_its_sockets_at_once() {
    let s = PreviewSessions::default();
    let within = std::time::Duration::from_secs(5);
    for end in ["end", "replace", "end_all"] {
        let cookie = s.open(a_binding()).unwrap();
        let mut alive = s
            .find(std::slice::from_ref(&cookie))
            .unwrap()
            .alive_for_test();
        match end {
            "end" => s.end(&s.find(std::slice::from_ref(&cookie)).unwrap().key),
            "replace" => drop(s.open(a_binding()).unwrap()),
            _ => s.end_all(),
        }
        let closed = tokio::time::timeout(within, alive.changed())
            .await
            .expect("never closed");
        assert!(
            closed.is_err(),
            "{end}: the session's sockets were not told"
        );
    }
}

/// A session has its preview answer at most [`REQUESTS_PER_SESSION`]
/// requests at once; the rest wait their turn and are answered once one
/// frees, never refused.
#[tokio::test]
async fn a_session_has_six_requests_at_the_preview_at_once_and_the_rest_wait() {
    let mut h = harness();
    let (token, preview) = a_previewing_phone(&mut h).await;
    let cookie = session_of(&h, &token).await;
    let mut asked = Vec::new();
    for _ in 0..REQUESTS_PER_SESSION + 3 {
        let mut from = forwarded(Some(SAM));
        from.push(("cookie", cookie.as_str()));
        let mut req = get("/hold", &from);
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(crate::local_peer::operator(SocketAddr::from(([127, 0, 0, 1], 5555)))));
        asked.push(tokio::spawn(router(h.state.clone()).oneshot(req)));
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while preview.holding.load(Ordering::SeqCst) < REQUESTS_PER_SESSION
        && std::time::Instant::now() < deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        preview.holding.load(Ordering::SeqCst),
        REQUESTS_PER_SESSION,
        "the rest wait"
    );
    preview.release.add_permits(REQUESTS_PER_SESSION + 3);
    for a in asked {
        assert_eq!(a.await.unwrap().unwrap().status(), 200);
    }
    assert_eq!(preview.most.load(Ordering::SeqCst), REQUESTS_PER_SESSION);
    assert_eq!(preview.count(), REQUESTS_PER_SESSION + 3);
}

/// A session holds at most [`UPGRADES_PER_SESSION`] WebSockets; the next is
/// refused `too_many_requests`, and one closing frees its place.
#[tokio::test]
async fn a_session_holds_two_websockets_at_most() {
    let mut h = harness();
    let (token, _preview) = a_previewing_phone(&mut h).await;
    let cookie = session_of(&h, &token).await;
    let addr = listening(&h).await;
    let mut open = Vec::new();
    for _ in 0..UPGRADES_PER_SESSION {
        let (s, answer) = socket(addr, &cookie, "").await;
        assert!(answer.starts_with("HTTP/1.1 101"), "{answer}");
        open.push(s);
    }
    let (mut refused, answer) = socket(addr, &cookie, "").await;
    assert!(answer.starts_with("HTTP/1.1 429"), "{answer}");
    let length = answer
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|n| n.trim().parse::<usize>().unwrap())
        })
        .expect("a refusal says its length");
    let mut body = vec![0u8; length];
    tokio::io::AsyncReadExt::read_exact(&mut refused, &mut body)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&body).contains(r#""code":"too_many_requests""#),
        "{}",
        String::from_utf8_lossy(&body)
    );
    drop(open.pop());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let (_s, answer) = socket(addr, &cookie, "").await;
        if answer.starts_with("HTTP/1.1 101") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the closed socket's place never freed: {answer}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// A preview that never begins to answer is a coded 504 once the wait for
/// its answer's head is up, and the turn it held is free again.
#[tokio::test]
async fn a_preview_that_never_answers_times_out_and_frees_its_turn() {
    let mut h = harness();
    h.state.preview_sessions = Arc::new(
        PreviewSessions::default().with_head_within(std::time::Duration::from_millis(300)),
    );
    let (token, _preview) = a_previewing_phone(&mut h).await;
    let cookie = session_of(&h, &token).await;
    let mut asked = Vec::new();
    for _ in 0..REQUESTS_PER_SESSION + 1 {
        let mut from = forwarded(Some(SAM));
        from.push(("cookie", cookie.as_str()));
        let mut req = get("/hang", &from);
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(crate::local_peer::operator(SocketAddr::from(([127, 0, 0, 1], 5555)))));
        asked.push(tokio::spawn(router(h.state.clone()).oneshot(req)));
    }
    for a in asked {
        let res = tokio::time::timeout(std::time::Duration::from_secs(10), a)
            .await
            .expect("never timed out")
            .unwrap()
            .unwrap();
        assert_eq!(res.status(), 504);
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["code"],
            "unavailable"
        );
    }
    assert_eq!(
        browse(&h, "/", &cookie, &forwarded(Some(SAM))).await.0,
        200,
        "every turn was given back"
    );
}

/// A new session to the same preview (the app opened another link) closes
/// the old session's WebSockets at once, though the old binding still stands.
#[tokio::test]
async fn a_new_session_closes_the_old_ones_websockets() {
    let mut h = harness();
    let (token, _preview) = a_previewing_phone(&mut h).await;
    let cookie = session_of(&h, &token).await;
    let addr = listening(&h).await;
    let (mut s, answer) = socket(addr, &cookie, "").await;
    assert!(answer.starts_with("HTTP/1.1 101"), "{answer}");
    assert!(still_echoes(&mut s).await);
    let _newer = session_of(&h, &token).await;
    closes_within(&mut s, RECHECK_EVERY * 3).await;
}

/// A request's turn is held until its answer's body is done, not only its
/// head: six slow bodies hold all six, and the next request waits for one.
#[tokio::test]
async fn a_turn_is_held_until_the_answers_body_is_done() {
    let mut h = harness();
    let (token, preview) = a_previewing_phone(&mut h).await;
    let cookie = session_of(&h, &token).await;
    let ask = |path: &str| {
        let mut from = forwarded(Some(SAM));
        from.push(("cookie", cookie.as_str()));
        let mut req = get(path, &from);
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(crate::local_peer::operator(SocketAddr::from(([127, 0, 0, 1], 5555)))));
        router(h.state.clone()).oneshot(req)
    };
    let mut slow = Vec::new();
    for _ in 0..REQUESTS_PER_SESSION {
        let res = ask("/slowbody").await.unwrap();
        assert_eq!(res.status(), 200, "its head came at once");
        slow.push(res);
    }
    let next = tokio::spawn(ask("/"));
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !next.is_finished(),
        "a seventh request went ahead while six bodies were still coming"
    );
    preview.release.add_permits(REQUESTS_PER_SESSION);
    for res in slow {
        assert_eq!(
            axum::body::to_bytes(res.into_body(), 64).await.unwrap(),
            "done"
        );
    }
    let next = tokio::time::timeout(std::time::Duration::from_secs(10), next)
        .await
        .expect("never answered");
    assert_eq!(next.unwrap().unwrap().status(), 200);
}
