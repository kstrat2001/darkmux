use super::*;
use futures::future::BoxFuture;
use std::io::{Read, Write};

/// How a test starts a listener: a listener, an app and a stop switch in,
/// the serving out.
type Start = Box<
    dyn FnOnce(
            tokio::net::TcpListener,
            Router,
            tokio::sync::watch::Receiver<bool>,
        ) -> BoxFuture<'static, ()>
        + Send,
>;

/// `start` serving `app` on its own runtime thread, on a fresh loopback
/// port: its address, the switch that stops it, and a receiver told once
/// it has returned.
fn serving_with(
    start: Start,
    app: Router,
) -> (
    SocketAddr,
    tokio::sync::watch::Sender<bool>,
    std::sync::mpsc::Receiver<()>,
) {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let addr = std_listener.local_addr().unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let (done_tx, done) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let l = tokio::net::TcpListener::from_std(std_listener).unwrap();
            start(l, app, rx).await;
        });
        let _ = done_tx.send(());
    });
    (addr, stop, done)
}

/// [`serve_kept_alive`] within `limits`, as [`serving_with`] starts it.
fn serving(
    app: Router,
    limits: KeptAliveLimits,
) -> (
    SocketAddr,
    tokio::sync::watch::Sender<bool>,
    std::sync::mpsc::Receiver<()>,
) {
    serving_with(
        Box::new(move |l, app, rx| Box::pin(serve_kept_alive(l, app, limits, rx))),
        app,
    )
}

fn ok_app() -> Router {
    Router::new().route("/ok", axum::routing::get(|| async { "ok" }))
}

fn limits(max_conns: usize, per_ip: usize, header_read_timeout: Duration) -> KeptAliveLimits {
    KeptAliveLimits {
        label: "test listener",
        max_conns,
        per_ip,
        judged_share: 0,
        header_read_timeout,
    }
}

/// A connection the other end has already reset reads as closed: on macOS
/// the reset makes even setting a read timeout fail (ECONNRESET), which a
/// stand-in under CI load hit and panicked on.
#[test]
fn closed_within_counts_a_connection_reset_before_it_looks() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut c = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let (s, _) = l.accept().unwrap();
    reset(s);
    std::thread::sleep(Duration::from_millis(100));
    assert!(closed_within(&mut c, Duration::from_secs(2)));
}

/// The stand-in's count of its held connections the other end closed counts
/// one reset before it looked as closed and reset, however macOS reports it.
#[test]
fn closed_count_counts_a_connection_reset_before_it_looks() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let c = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let (s, _) = l.accept().unwrap();
    reset(s);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(closed_count(&[c], 1, Duration::from_secs(2)), (1, 1));
}

/// Whether `e` says the other end reset the connection. macOS reports a
/// reset that arrived first as EINVAL from `connect` and from setting a
/// socket option, and as ECONNRESET from a read or, sometimes, an option:
/// a server that resets a refused connection at once, under load, gets
/// there before the stand-in looks.
fn reset_by_peer(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::ConnectionReset || e.raw_os_error() == Some(libc::EINVAL)
}

/// Whether a connection that may have been reset while it was opening
/// (`connecting`) is closed within `d`: a reset at connect is a close.
fn closed_or_reset_at_connect(
    connecting: std::io::Result<std::net::TcpStream>,
    d: Duration,
) -> bool {
    match connecting {
        Ok(mut s) => closed_within(&mut s, d),
        Err(e) => {
            assert!(reset_by_peer(&e), "connecting: {e}");
            true
        }
    }
}

/// Whether the other end closes `s` within `d` (whatever it sent first).
fn closed_within(s: &mut std::net::TcpStream, d: Duration) -> bool {
    let until = std::time::Instant::now() + d;
    let mut b = [0u8; 256];
    loop {
        let left = until.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        // A reset that arrived first fails even this (see `reset_by_peer`).
        if let Err(e) = s.set_read_timeout(Some(left)) {
            assert!(reset_by_peer(&e), "setting a read timeout: {e}");
            return true;
        }
        match s.read(&mut b) {
            Ok(0) => return true,
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return true,
            Err(_) => return false,
        }
    }
}

/// `n` connections to `addr` that send nothing at all.
fn idle(addr: SocketAddr, n: usize) -> Vec<std::net::TcpStream> {
    let held: Vec<_> = (0..n)
        .map(|_| std::net::TcpStream::connect(addr).unwrap())
        .collect();
    std::thread::sleep(Duration::from_millis(150));
    held
}

/// One GET on `s`, kept alive: the answer's text (head and body).
fn get_on(s: &mut std::net::TcpStream, path: &str) -> String {
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut got = Vec::new();
    let mut b = [0u8; 512];
    while !String::from_utf8_lossy(&got).ends_with("ok") {
        match s.read(&mut b) {
            Ok(0) | Err(_) => break,
            Ok(n) => got.extend_from_slice(&b[..n]),
        }
    }
    String::from_utf8_lossy(&got).to_string()
}

/// Connections that are held open and send nothing (what exhausting a
/// listener's descriptors takes) are bounded: one past the cap is closed
/// as it is accepted, the held ones are closed at the header deadline, and
/// once their slots are free a request is served.
#[test]
fn idle_connections_are_capped_and_closed_at_the_header_deadline() {
    let (addr, _stop, _done) = serving(ok_app(), limits(4, 8, Duration::from_millis(800)));
    let mut held = idle(addr, 4);
    let mut extra = std::net::TcpStream::connect(addr).unwrap();
    assert!(
        closed_within(&mut extra, Duration::from_millis(400)),
        "a connection past the cap must be closed on accept"
    );
    for s in held.iter_mut() {
        assert!(
            closed_within(s, Duration::from_secs(3)),
            "an idle connection must be closed at the header deadline"
        );
    }
    std::thread::sleep(Duration::from_millis(100));
    let body = ureq::get(&format!("http://{addr}/ok"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert_eq!(body, "ok");
}

/// One address at its own cap has its next connection closed, while the
/// listener as a whole still has room.
#[test]
fn one_address_past_its_cap_is_closed_on_accept() {
    let (addr, _stop, _done) = serving(ok_app(), limits(8, 2, Duration::from_secs(5)));
    let _held = idle(addr, 2);
    let mut third = std::net::TcpStream::connect(addr).unwrap();
    assert!(
        closed_within(&mut third, Duration::from_millis(400)),
        "a third connection from one address must be closed"
    );
}

/// A connection is kept alive between requests (two answered on one
/// socket), and one left idle after an answer is closed at the header
/// deadline.
#[test]
fn a_kept_alive_connection_is_closed_once_idle_past_the_header_deadline() {
    let (addr, _stop, _done) = serving(ok_app(), limits(4, 4, Duration::from_millis(800)));
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    assert!(
        get_on(&mut s, "/ok").starts_with("HTTP/1.1 200"),
        "first request"
    );
    assert!(
        get_on(&mut s, "/ok").starts_with("HTTP/1.1 200"),
        "a second request on the same connection"
    );
    assert!(
        closed_within(&mut s, Duration::from_secs(3)),
        "an idle kept-alive connection must be closed at the header deadline"
    );
}

/// The header deadline bounds a request's head, never its answer: a
/// stream that runs well past it (an SSE feed) is delivered whole.
#[test]
fn a_streaming_answer_outlives_the_header_deadline() {
    let app = Router::new().route(
        "/stream",
        axum::routing::get(|| async {
            let ticks = futures::stream::unfold(0u8, |n| async move {
                if n == 5 {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
                Some((
                    Ok::<_, std::convert::Infallible>(format!("tick{n}\n")),
                    n + 1,
                ))
            });
            axum::body::Body::from_stream(ticks)
        }),
    );
    let (addr, _stop, _done) = serving(app, limits(4, 4, Duration::from_millis(400)));
    let body = ureq::get(&format!("http://{addr}/stream"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert_eq!(body, "tick0\ntick1\ntick2\ntick3\ntick4\n");
}

/// Stopping: nothing more is accepted, an idle connection is closed at
/// once, the answer in flight is finished, and the server returns only
/// after it has.
#[test]
fn stopping_finishes_the_answer_in_flight_and_closes_idle_connections() {
    let app = Router::new().route(
        "/slow",
        axum::routing::get(|| async {
            tokio::time::sleep(Duration::from_millis(700)).await;
            "ok"
        }),
    );
    let (addr, stop, done) = serving(app, limits(4, 4, Duration::from_secs(30)));
    let mut idle_one = std::net::TcpStream::connect(addr).unwrap();
    let mut busy = std::net::TcpStream::connect(addr).unwrap();
    busy.write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    stop.send(true).unwrap();
    assert!(
        closed_within(&mut idle_one, Duration::from_millis(400)),
        "an idle connection closes when the listener stops"
    );
    assert!(
        done.try_recv().is_err(),
        "returned with an answer still in flight"
    );
    busy.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut got = String::new();
    let _ = busy.read_to_string(&mut got);
    assert!(
        got.starts_with("HTTP/1.1 200") && got.ends_with("ok"),
        "the answer in flight was cut: {got:?}"
    );
    assert!(
        done.recv_timeout(Duration::from_secs(3)).is_ok(),
        "the server did not return once its last connection closed"
    );
}

/// The client listener's cap keeps every connection's descriptors, two for
/// a proxied preview request, within half of launchd's soft limit of 256;
/// both listeners close an idle connection within ten seconds.
#[test]
fn the_production_limits_fit_their_processes() {
    const { assert!(CLIENT.max_conns * 2 <= 128) };
    assert_eq!(
        CLIENT.per_ip, CLIENT.max_conns,
        "the client listener sees only 127.0.0.1"
    );
    const { assert!(VIEWER.per_ip < VIEWER.max_conns) };
    for l in [VIEWER, CLIENT] {
        assert!(l.header_read_timeout <= Duration::from_secs(10), "{l:?}");
        assert!(
            l.judged_share > 0 && l.judged_share < l.per_ip,
            "a share kept for judged connections, and an open one: {l:?}"
        );
    }
}

/// The viewer and the client listener each serve within their caps: with
/// `per_ip` connections held open from one address, the next is closed as
/// it is accepted.
#[test]
fn the_viewer_and_the_client_listener_cap_their_connections() {
    let _ = crate::raise_open_file_limit(4096);
    let starts: [(&str, usize, Start); 2] = [
        (
            "viewer",
            VIEWER.per_ip,
            Box::new(|l, app, rx| Box::pin(crate::serve_viewer(l, app, rx))),
        ),
        (
            "client",
            CLIENT.per_ip,
            Box::new(|l, app, rx| {
                Box::pin(crate::client_pairing::serve_client_listener(l, app, rx))
            }),
        ),
    ];
    for (name, per_ip, start) in starts {
        let (addr, _stop, _done) = serving_with(start, ok_app());
        let _held = idle(addr, per_ip);
        let mut extra = std::net::TcpStream::connect(addr).unwrap();
        assert!(
            closed_within(&mut extra, Duration::from_secs(1)),
            "{name}: a connection past {per_ip} from one address must be closed on accept"
        );
    }
}

/// An app whose `/up` switches protocols and keeps the upgraded stream
/// open until the other end hangs up; `/ok` answers plainly.
fn upgrading_app() -> Router {
    ok_app().route(
        "/up",
        axum::routing::get(|mut req: axum::extract::Request| async move {
            let on = hyper::upgrade::on(&mut req);
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let Ok(up) = on.await else { return };
                let mut io = hyper_util::rt::TokioIo::new(up);
                let mut b = [0u8; 64];
                while io.read(&mut b).await.is_ok_and(|n| n > 0) {}
            });
            axum::http::Response::builder()
                .status(101)
                .header("connection", "upgrade")
                .header("upgrade", "test")
                .body(axum::body::Body::empty())
                .unwrap()
        }),
    )
}

/// `n` connections to `addr`, each switched to another protocol (a
/// WebSocket's shape) and held open.
fn upgraded(addr: SocketAddr, n: usize) -> Vec<std::net::TcpStream> {
    (0..n)
        .map(|_| {
            let mut s = std::net::TcpStream::connect(addr).unwrap();
            s.write_all(
                b"GET /up HTTP/1.1\r\nHost: x\r\nConnection: upgrade\r\nUpgrade: test\r\n\r\n",
            )
            .unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                assert_eq!(s.read(&mut b).unwrap(), 1, "the upgrade was not answered");
                head.push(b[0]);
            }
            assert!(
                head.starts_with(b"HTTP/1.1 101"),
                "{}",
                String::from_utf8_lossy(&head)
            );
            s
        })
        .collect()
}

/// An upgraded connection (a WebSocket) keeps its slot for as long as its
/// stream is open: with the cap held by upgraded streams a new connection
/// is closed on accept, and stopping waits for them; once they close,
/// connections are admitted again and the server returns.
/// The header deadline (30 s) is far past the 5 s the refused connection
/// is given to close, so an admitted one could never pass for refused,
/// however slowly a loaded machine gets to the accept.
#[test]
fn an_upgraded_connection_holds_its_slot_until_its_stream_closes() {
    let (addr, stop, done) = serving(upgrading_app(), limits(4, 8, Duration::from_secs(30)));
    let held = upgraded(addr, 4);
    std::thread::sleep(Duration::from_millis(150));
    let mut extra = std::net::TcpStream::connect(addr).unwrap();
    assert!(
        closed_within(&mut extra, Duration::from_secs(5)),
        "a connection past a cap held by upgraded streams must be closed on accept"
    );
    drop(held);
    // A slot frees only once the server sees its stream end, so wait for a
    // plain request to be served before opening another upgrade.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline
        && ureq::get(&format!("http://{addr}/ok")).call().is_err()
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    let body = ureq::get(&format!("http://{addr}/ok"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert_eq!(
        body, "ok",
        "slots came back once the upgraded streams closed"
    );
    let mut again = upgraded(addr, 1);
    stop.send(true).unwrap();
    assert!(
        done.recv_timeout(Duration::from_millis(500)).is_err(),
        "returned while an upgraded stream was still open"
    );
    let _ = again[0].shutdown(std::net::Shutdown::Both);
    again.clear();
    assert!(
        done.recv_timeout(Duration::from_secs(3)).is_ok(),
        "did not return once the upgraded stream closed"
    );
}

/// This machine is a loopback peer however spelled, any peer of a listener
/// bound to loopback (whatever source address it chose), and a peer at the
/// listener's own address; another machine is not. All of this machine
/// counts as one address, so binding another source address adds no open
/// share; a listener with no judged share (the fleet listener's) judges
/// nothing.
#[test]
fn this_machine_is_every_peer_of_a_loopback_listener_and_counts_as_one() {
    let at = |s: &str| -> SocketAddr { s.parse().unwrap() };
    let lo = Some(at("127.0.0.1:8799"));
    for peer in ["127.0.0.1:50000", "[::ffff:127.0.0.1]:50000", "[::1]:50000"] {
        assert!(from_this_machine(at(peer), None), "{peer}");
    }
    assert!(
        from_this_machine(at("192.168.1.5:50000"), lo),
        "this Mac's LAN address, to a loopback listener"
    );
    assert!(from_this_machine(at("127.0.0.2:50000"), lo));
    let lan = Some(at("192.168.1.5:8799"));
    assert!(
        from_this_machine(at("192.168.1.5:50000"), lan),
        "this machine reaching its own address"
    );
    assert!(
        !from_this_machine(at("192.168.1.9:50000"), lan),
        "another machine"
    );
    assert!(!from_this_machine(at("100.64.0.7:50000"), None));
    let one: std::net::IpAddr = "127.0.0.1".parse().unwrap();
    assert_eq!(counted_as(at("192.168.1.5:50000"), true), one);
    assert_eq!(counted_as(at("[::1]:50000"), true), one);
    assert_eq!(
        counted_as(at("100.64.0.7:50000"), false),
        "100.64.0.7".parse::<std::net::IpAddr>().unwrap()
    );
    let fleet = Admission::new("fleet", 8, 4);
    assert_eq!(fleet.open_share, 4, "judges nothing");
    assert!(!fleet.judging, "and counts this machine's addresses apart");
    assert!(!Admission::new("x", 8, 4).with_judged_share(0).judging);
    assert!(Admission::new("x", 8, 4).with_judged_share(1).judging);
    assert_eq!(
        Admission::new("viewer", 8, 4)
            .with_judged_share(1)
            .open_share,
        3
    );
}

/// Not a test: a stand-in for a container engine's forwarder, run from a
/// copy of this test binary named `com.docker.backend` ([`forwarder`],
/// [`churning_forwarder`]). Without `DARKMUX_TEST_FORWARDER` it does
/// nothing. Its modes:
///
/// - `hold <addr> <n> <closing> <source>`: opens `n` connections to `addr`
///   (from this machine's address `source`, or `-` for the default), prints
///   `closed <k>` once the other end has closed `closing` of them (or 8 s
///   pass: within the production listeners' 10 s header deadline, so a
///   connection that deadline closes is never counted as refused), and
///   holds the rest until its stdin closes.
/// - `flick <addr> <hold> <threads> <secs>`: holds `hold` connections,
///   prints `churning`, and for `secs` has each of `threads` threads connect
///   and reset it at once (SO_LINGER 0), so the peer is gone before the
///   judgment ends. A reset leaves no closed connection waiting on this
///   machine's ports, as an ordinary close by so many would.
/// - `tag <addr> <hold> <port>`: holds `hold` connections, opens one more
///   (past the open share: a round names this forwarder and closes it),
///   then one from source port `port` (closed as a known forwarder's),
///   prints whether each was closed, and exits.
/// - `drop-one <addr> <open> <port>`: holds the open share, one of its
///   connections from source port `port`; opens one more (a round names
///   this forwarder and closes it) and another (a read of this process's
///   connections, which sees the one from `port`, refuses it); then resets
///   the connection from `port`, prints `ready` and holds the rest.
/// - `churn <addr> <hold> <threads> <secs>`: holds `hold` connections, prints
///   `churning`, and for `secs` has each of `threads` threads connect, wait
///   for the other end to close, and connect again (a container reopening
///   every connection it loses), then holds until its stdin closes.
#[test]
#[ignore]
fn container_forwarder_stand_in() {
    let Ok(spec) = std::env::var("DARKMUX_TEST_FORWARDER") else {
        return;
    };
    let parts: Vec<&str> = spec.split(' ').collect();
    let addr: SocketAddr = parts[1].parse().unwrap();
    let num = |i: usize| -> usize { parts[i].parse().unwrap() };
    let _held = match parts[0] {
        "hold" => {
            let source = parts[4].parse().ok();
            let opened: Vec<_> = (0..num(2)).map(|_| connect_from(addr, source)).collect();
            // Reset while still opening: closed and reset already.
            let at_connect = opened.iter().filter(|c| c.is_none()).count();
            let held: Vec<_> = opened.into_iter().flatten().map(abortive).collect();
            let (closed, reset) = closed_count(
                &held,
                num(3).saturating_sub(at_connect),
                Duration::from_secs(8),
            );
            println!(
                "closed {} reset {}",
                closed + at_connect,
                reset + at_connect
            );
            held
        }
        "flick" => flick(addr, num(2), num(3), Duration::from_secs(num(4) as u64)),
        "tag" => {
            tag(addr, num(2), num(3) as u16);
            return;
        }
        "drop-one" => drop_one(addr, num(2), num(3) as u16),
        _ => churn(addr, num(2), num(3), Duration::from_secs(num(4) as u64)),
    };
    let _ = std::io::stdin().read_to_end(&mut Vec::new());
}

/// A connection to `addr` from this machine's address `source` (the
/// default one when `None`).
fn connect_from(addr: SocketAddr, source: Option<std::net::IpAddr>) -> Option<std::net::TcpStream> {
    let Some(source) = source else {
        return match std::net::TcpStream::connect(addr) {
            Ok(s) => Some(s),
            Err(e) if reset_by_peer(&e) => None,
            Err(e) => panic!("connecting: {e}"),
        };
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    let s = rt.block_on(async {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind(SocketAddr::new(source, 0)).unwrap();
        socket.connect(addr).await
    });
    let s = match s {
        Ok(s) => s.into_std().unwrap(),
        Err(e) if reset_by_peer(&e) => return None,
        Err(e) => panic!("connecting from {source}: {e}"),
    };
    s.set_nonblocking(false).unwrap();
    Some(s)
}

/// [`container_forwarder_stand_in`]'s `flick` mode: the held connections.
fn flick(
    addr: SocketAddr,
    hold: usize,
    threads: usize,
    for_: Duration,
) -> Vec<std::net::TcpStream> {
    let held: Vec<_> = (0..hold).map(|_| resetting(addr)).collect();
    let until = std::time::Instant::now() + for_;
    for _ in 0..threads {
        std::thread::spawn(move || {
            while std::time::Instant::now() < until {
                let Ok(c) = std::net::TcpStream::connect(addr) else {
                    continue;
                };
                {
                    use std::os::fd::AsRawFd;
                    let off = libc::linger {
                        l_onoff: 1,
                        l_linger: 0,
                    };
                    // SAFETY: a socket this thread owns, and a `linger` of
                    // the size given.
                    unsafe {
                        libc::setsockopt(
                            c.as_raw_fd(),
                            libc::SOL_SOCKET,
                            libc::SO_LINGER,
                            (&off as *const libc::linger).cast(),
                            std::mem::size_of::<libc::linger>() as libc::socklen_t,
                        )
                    };
                }
            }
        });
    }
    println!("churning");
    held
}

/// [`container_forwarder_stand_in`]'s `tag` mode.
/// Resets `s` (SO_LINGER 0) as it is dropped: no closed connection is left
/// waiting on this machine's ports.
fn reset(s: std::net::TcpStream) {
    drop(abortive(s));
}

/// A connection to `addr` that resets when it is closed ([`abortive`]).
fn resetting(addr: SocketAddr) -> std::net::TcpStream {
    try_resetting(addr).unwrap()
}

/// [`resetting`], its connect's error returned (a stand-in's connection the
/// server may reset while it opens).
fn try_resetting(addr: SocketAddr) -> std::io::Result<std::net::TcpStream> {
    std::net::TcpStream::connect(addr).map(abortive)
}

/// `s`, set to reset rather than close gracefully when it is dropped or its
/// process ends, so tests that open thousands of connections leave none in
/// TIME_WAIT on this machine (the side that closes first keeps one unless
/// it is reset).
fn abortive(s: std::net::TcpStream) -> std::net::TcpStream {
    use std::os::fd::AsRawFd;
    let off = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: a socket this function owns, and a `linger` of the size given.
    unsafe {
        libc::setsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&off as *const libc::linger).cast(),
            std::mem::size_of::<libc::linger>() as libc::socklen_t,
        )
    };
    s
}

/// [`container_forwarder_stand_in`]'s `drop-one` mode: the held connections.
fn drop_one(addr: SocketAddr, open: usize, port: u16) -> Vec<std::net::TcpStream> {
    let held: Vec<_> = (1..open).map(|_| resetting(addr)).collect();
    let tagged = connect_from_port(addr, port);
    std::thread::sleep(Duration::from_millis(200));
    let named = try_resetting(addr);
    println!(
        "named {}",
        closed_or_reset_at_connect(named, Duration::from_secs(6))
    );
    let read = try_resetting(addr);
    println!(
        "read {}",
        closed_or_reset_at_connect(read, Duration::from_secs(6))
    );
    reset(tagged);
    println!("ready");
    held
}

fn tag(addr: SocketAddr, hold: usize, port: u16) {
    let _held: Vec<_> = (0..hold).map(|_| resetting(addr)).collect();
    std::thread::sleep(Duration::from_millis(200));
    let extra = try_resetting(addr);
    println!(
        "named {}",
        closed_or_reset_at_connect(extra, Duration::from_secs(6))
    );
    let tagged = try_connect_from_port(addr, port);
    println!(
        "tagged {}",
        closed_or_reset_at_connect(tagged, Duration::from_secs(6))
    );
}

/// A connection to `addr` from 127.0.0.1's port `port`, which may still be
/// in an earlier connection's close (`SO_REUSEADDR`).
fn connect_from_port(addr: SocketAddr, port: u16) -> std::net::TcpStream {
    try_connect_from_port(addr, port).unwrap()
}

/// [`connect_from_port`], its connect's error returned (a stand-in's
/// connection the server may reset while it opens).
fn try_connect_from_port(addr: SocketAddr, port: u16) -> std::io::Result<std::net::TcpStream> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    let s = rt.block_on(async {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.set_reuseaddr(true).unwrap();
        socket
            .bind(SocketAddr::new([127, 0, 0, 1].into(), port))
            .unwrap();
        socket.connect(addr).await
    })?;
    let s = s.into_std()?;
    s.set_nonblocking(false)?;
    Ok(abortive(s))
}

/// A container engine's forwarder holding `hold` connections to `addr` and,
/// on `threads` threads for `for_secs`, connecting and hanging up at once
/// ([`container_forwarder_stand_in`]'s `flick`
/// mode), once it has begun.
fn flicking_forwarder(addr: SocketAddr, hold: usize, threads: usize, for_secs: u64) -> Forwarder {
    let (held, mut lines) = stand_in(&format!("flick {addr} {hold} {threads} {for_secs}"));
    assert!(
        lines.any(|l| l.contains("churning")),
        "the stand-in began; its stderr: {}",
        held.stderr()
    );
    held
}

/// [`container_forwarder_stand_in`]'s `churn` mode: the held connections.
fn churn(
    addr: SocketAddr,
    hold: usize,
    threads: usize,
    for_: Duration,
) -> Vec<std::net::TcpStream> {
    let held: Vec<_> = (0..hold).map(|_| resetting(addr)).collect();
    let until = std::time::Instant::now() + for_;
    for _ in 0..threads {
        std::thread::spawn(move || {
            while std::time::Instant::now() < until {
                if let Ok(mut s) = std::net::TcpStream::connect(addr) {
                    let _ = s.set_read_timeout(Some(Duration::from_secs(9)));
                    let _ = s.read(&mut [0u8; 1]);
                    reset(s);
                }
            }
        });
    }
    println!("churning");
    held
}

/// How many of `conns` the other end has closed, once `want` have been or
/// `within` has passed, and how many of those it reset rather than closed.
fn closed_count(conns: &[std::net::TcpStream], want: usize, within: Duration) -> (usize, usize) {
    for c in conns {
        c.set_nonblocking(true).unwrap();
    }
    let mut seen: Vec<Option<bool>> = vec![None; conns.len()];
    let until = std::time::Instant::now() + within;
    loop {
        for (c, how) in conns.iter().zip(seen.iter_mut()) {
            *how = how.or_else(|| close_seen(c));
        }
        let closed = seen.iter().flatten().count();
        if closed >= want || std::time::Instant::now() >= until {
            return (
                closed,
                seen.iter().flatten().filter(|reset| **reset).count(),
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// How the other end of nonblocking `c` ended it: `Some(true)` reset,
/// `Some(false)` closed, `None` still open. A reset is told by the first
/// read after it, so each connection is read until its end is seen once.
fn close_seen(mut c: &std::net::TcpStream) -> Option<bool> {
    match c.read(&mut [0u8; 1]) {
        Ok(0) => Some(false),
        Ok(_) => None,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => None,
        Err(e) => Some(e.kind() == std::io::ErrorKind::ConnectionReset),
    }
}

/// A running [`container_forwarder_stand_in`], stopped when dropped.
struct Forwarder {
    child: std::process::Child,
    exe_dir: tempfile::TempDir,
}

impl Forwarder {
    /// What the stand-in wrote to stderr so far (its panic, when it died),
    /// for a failure message: without it a stand-in that dies says nothing.
    fn stderr(&self) -> String {
        std::fs::read_to_string(stand_in_stderr(self.exe_dir.path())).unwrap_or_default()
    }
}

/// Where a stand-in copied into `dir` writes its stderr.
fn stand_in_stderr(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("stderr.log")
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A container engine's forwarder opening `n` connections to `addr`: this
/// test binary copied as `com.docker.backend` (the name is what tells a
/// forwarder) and running [`container_forwarder_stand_in`]. Returns once
/// `closing` of them have been closed (or 8 s passed), with how many were
/// and how many of those were reset.
fn forwarder(addr: SocketAddr, n: usize, closing: usize) -> (Forwarder, usize, usize) {
    forwarder_from(addr, n, closing, None)
}

/// [`forwarder`], its connections from this machine's address `source`.
fn forwarder_from(
    addr: SocketAddr,
    n: usize,
    closing: usize,
    source: Option<std::net::IpAddr>,
) -> (Forwarder, usize, usize) {
    let source = source.map_or("-".to_string(), |s| s.to_string());
    let (held, mut lines) = stand_in(&format!("hold {addr} {n} {closing} {source}"));
    // libtest prints the test's name on the same line first.
    let report = lines
        .find_map(|l| l.rsplit_once("closed ").map(|(_, r)| r.to_string()))
        .unwrap_or_else(|| {
            panic!(
                "the stand-in reports what it holds; its stderr: {}",
                held.stderr()
            )
        });
    let (closed, reset) = report.split_once(" reset ").expect("closed k reset r");
    (
        held,
        closed.trim().parse().unwrap(),
        reset.trim().parse().unwrap(),
    )
}

/// A container engine's forwarder holding `hold` connections to `addr` and
/// reopening every other one it loses on `threads` threads for `for_secs`
/// ([`container_forwarder_stand_in`]'s `churn` mode), once it has begun.
fn churning_forwarder(addr: SocketAddr, hold: usize, threads: usize, for_secs: u64) -> Forwarder {
    let (held, mut lines) = stand_in(&format!("churn {addr} {hold} {threads} {for_secs}"));
    assert!(
        lines.any(|l| l.contains("churning")),
        "the stand-in began churning; its stderr: {}",
        held.stderr()
    );
    held
}

/// [`container_forwarder_stand_in`] started with `spec`, from a copy of this
/// test binary named `com.docker.backend`, and the lines it prints.
fn stand_in(spec: &str) -> (Forwarder, impl Iterator<Item = String>) {
    let dir = tempfile::TempDir::new().unwrap();
    let exe = dir.path().join("com.docker.backend");
    std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
    let mut child = spawn_copied(&exe, spec);
    let out = std::io::BufReader::new(child.stdout.take().unwrap());
    let lines = std::io::BufRead::lines(out).map_while(Result::ok);
    let held = Forwarder {
        child,
        exe_dir: dir,
    };
    (held, lines)
}

/// Starts the stand-in at `exe`. A copy just written can be busy for a
/// moment (ETXTBSY: a process another test forked meanwhile still holds
/// it open for writing), so that one error is waited out.
fn spawn_copied(exe: &std::path::Path, spec: &str) -> std::process::Child {
    let until = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let started = std::process::Command::new(exe)
            .args([
                "--exact",
                "bounded_serve::tests::container_forwarder_stand_in",
                "--ignored",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env("DARKMUX_TEST_FORWARDER", spec)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::fs::File::create(stand_in_stderr(exe.parent().unwrap())).unwrap())
            .spawn();
        match started {
            Err(e) if e.raw_os_error() == Some(26) && std::time::Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(20));
            }
            other => return other.unwrap(),
        }
    }
}

/// Rounds that read this machine's socket table but cannot see a holder
/// named `hidden` (as a listener's socket table cannot show tailscaled,
/// which runs as root and forwards `tailscale serve`), with no host
/// sandbox running.
fn not_seeing(hidden: &'static str) -> Arc<crate::local_peer::Rounds> {
    crate::local_peer::Rounds::new(
        Arc::new(move |local| {
            let platform = darkmux_platform::current();
            platform.tcp_peers_of(local).map(|peers| {
                peers
                    .into_iter()
                    .filter(|(_, pid)| platform.process_name(*pid).as_deref() != Some(hidden))
                    .collect()
            })
        }),
        Arc::new(|| Some(Vec::new())),
        Duration::from_secs(5),
    )
}

/// [`serve_kept_alive_reading`] within `limits`, its connections judged
/// from `rounds`, as [`serving_with`] starts it.
fn serving_judged(
    app: Router,
    limits: KeptAliveLimits,
    rounds: Arc<crate::local_peer::Rounds>,
) -> (
    SocketAddr,
    tokio::sync::watch::Sender<bool>,
    std::sync::mpsc::Receiver<()>,
) {
    serving_with(
        Box::new(move |l, app, rx| Box::pin(serve_kept_alive_reading(l, app, limits, rounds, rx))),
        app,
    )
}

/// `/ok`'s status as `curl` gets it, sent the way `tailscale serve`
/// forwards a request (with `X-Forwarded-For`); `000` when refused.
fn status_from_curl(addr: SocketAddr) -> String {
    let out = std::process::Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-m", "10"])
        .args(["-H", "X-Forwarded-For: 100.64.0.7"])
        .arg(format!("http://{addr}/ok"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The judged share of 127.0.0.1's slots is out of a container's reach: its
/// forwarder, opening twice the address's cap, holds only the open share
/// (every connection past it is judged and closed), while the operator's
/// own process holds connections in the judged share and a peer this user
/// cannot see (tailscaled, forwarding `tailscale serve`) is served there.
#[test]
fn a_container_holds_only_the_open_share_and_the_operator_and_tailscale_are_served() {
    let lim = KeptAliveLimits {
        label: "test listener",
        max_conns: 64,
        per_ip: 8,
        judged_share: 4,
        header_read_timeout: Duration::from_secs(30),
    };
    let open = lim.per_ip - lim.judged_share;
    let (addr, _stop, _done) = serving_judged(ok_app(), lim, not_seeing("curl"));
    let (_container, closed, reset) = forwarder(addr, 2 * lim.per_ip, 2 * lim.per_ip - open);
    assert_eq!(
        closed,
        2 * lim.per_ip - open,
        "the container holds only the open share ({open})"
    );
    assert_eq!(
        reset, closed,
        "each one refused, judged or past the cap, is reset: no TIME_WAIT left on this machine"
    );
    let mut mine: Vec<_> = (1..lim.judged_share).map(|_| resetting(addr)).collect();
    for (i, s) in mine.iter_mut().enumerate() {
        let got = get_on(s, "/ok");
        assert!(
            got.starts_with("HTTP/1.1 200"),
            "the operator's #{i}: {got:?}"
        );
    }
    assert_eq!(
        status_from_curl(addr),
        "200",
        "tailscale serve's connection, in the last judged slot"
    );
}

/// The operator's own connections are never counted as a container's: past
/// the open share each is judged the operator's and served, up to the
/// address's cap, and so is one whose lookup gives no answer (a failure to
/// tell never refuses). The cap still closes the next connection.
#[test]
fn the_operators_own_connections_fill_the_judged_share_and_the_cap_still_holds() {
    let no_answer = crate::local_peer::Rounds::new(
        Arc::new(|_| None),
        Arc::new(|| Some(Vec::new())),
        Duration::from_secs(5),
    );
    for (what, rounds, operators) in [
        ("judged", not_seeing(""), true),
        ("no answer", no_answer, false),
    ] {
        let lim = KeptAliveLimits {
            label: "test listener",
            max_conns: 64,
            per_ip: 8,
            judged_share: 4,
            header_read_timeout: Duration::from_secs(30),
        };
        let (addr, _stop, _done) = serving_judged(ok_app(), lim, rounds);
        let mut mine: Vec<_> = (0..lim.per_ip).map(|_| resetting(addr)).collect();
        for (i, s) in mine.iter_mut().enumerate() {
            let got = get_on(s, "/ok");
            assert!(got.starts_with("HTTP/1.1 200"), "{what}: #{i}: {got:?}");
            assert_eq!(
                crate::local_peer::is_operator(s.local_addr().unwrap()),
                operators,
                "{what}: #{i} was judged from these rounds"
            );
        }
        let mut extra = std::net::TcpStream::connect(addr).unwrap();
        assert!(
            closed_within(&mut extra, Duration::from_secs(1)),
            "{what}: one past the address's cap is closed"
        );
    }
}

/// The viewer and the client listener keep their judged share from a
/// container: with a container's forwarder holding all it can from
/// 127.0.0.1, this process's request is still served.
#[test]
fn the_viewer_and_the_client_listener_keep_a_share_no_container_can_take() {
    let _ = crate::raise_open_file_limit(4096);
    let starts: [(&str, KeptAliveLimits, Start); 2] = [
        (
            "viewer",
            VIEWER,
            Box::new(|l, app, rx| Box::pin(crate::serve_viewer(l, app, rx))),
        ),
        (
            "client",
            CLIENT,
            Box::new(|l, app, rx| {
                Box::pin(crate::client_pairing::serve_client_listener(l, app, rx))
            }),
        ),
    ];
    for (name, lim, start) in starts {
        let (addr, _stop, _done) = serving_with(start, ok_app());
        let (_container, closed, reset) = forwarder(addr, lim.per_ip, lim.judged_share);
        assert_eq!(
            closed, lim.judged_share,
            "{name}: the container's judged connections are closed"
        );
        assert_eq!(reset, closed, "{name}: each one reset");
        let body = ureq::get(&format!("http://{addr}/ok"))
            .call()
            .map(|r| r.into_string().unwrap());
        assert_eq!(
            body.ok().as_deref(),
            Some("ok"),
            "{name}: the operator is served"
        );
    }
}

/// How many of `n` requests, each on a connection of its own (a browser's
/// new connection), `/ok` answers, `gap` apart.
fn served_of(addr: SocketAddr, n: usize, gap: Duration) -> usize {
    let mut served = 0;
    for _ in 0..n {
        if let Ok(mut s) = std::net::TcpStream::connect(addr) {
            s.set_read_timeout(Some(Duration::from_secs(6))).unwrap();
            if s.write_all(b"GET /ok HTTP/1.1\r\nHost: x\r\n\r\n").is_ok()
                && get_answer(&mut s).starts_with("HTTP/1.1 200")
            {
                served += 1;
            }
            reset(s);
        }
        std::thread::sleep(gap);
    }
    served
}

/// The answer read from `s` up to its `ok` body, or what came before it
/// closed.
fn get_answer(s: &mut std::net::TcpStream) -> String {
    let mut got = Vec::new();
    let mut b = [0u8; 512];
    while !String::from_utf8_lossy(&got).ends_with("ok") {
        match s.read(&mut b) {
            Ok(0) | Err(_) => break,
            Ok(n) => got.extend_from_slice(&b[..n]),
        }
    }
    String::from_utf8_lossy(&got).into_owned()
}

/// A container that reopens every connection it loses cannot crowd the
/// operator out of the viewer: with its forwarder holding the open share
/// and 64 threads (as many as the judged share) reconnecting as fast as
/// they are closed, every one of this process's new connections is served.
/// Once a round
/// has named the forwarder, its connections past the open share are refused
/// as they are accepted, without waiting for a round while holding a slot.
#[test]
fn a_reconnecting_container_never_crowds_the_operator_out_of_the_viewer() {
    let _ = crate::raise_open_file_limit(8192);
    let (addr, _stop, _done) = serving_with(
        Box::new(|l, app, rx| Box::pin(crate::serve_viewer(l, app, rx))),
        ok_app(),
    );
    let open = VIEWER.per_ip - VIEWER.judged_share;
    let _container = churning_forwarder(addr, open, 64, 4);
    std::thread::sleep(Duration::from_millis(300));
    let served = served_of(addr, 10, Duration::from_millis(100));
    assert_eq!(
        served, 10,
        "64 reconnecting threads: the operator served {served} of 10"
    );
}

/// This machine is every peer of a listener bound to loopback, whatever
/// source address it connects from, and all of them share one count: a
/// container's forwarder connecting from this Mac's LAN address as well as
/// from 127.0.0.1 holds only the open share between them, and the operator
/// is served. The listener has the client listener's shape (its per-address
/// cap is its whole cap, so a second source address with an open share of
/// its own would fill it) and a header deadline far past the test, so a
/// held connection never closes on its own while it runs.
#[test]
fn a_container_connecting_from_another_address_of_this_machine_shares_the_open_share() {
    let lan = {
        let u = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        u.connect("192.0.2.1:9")
            .expect("a route off this machine, to name its LAN address");
        u.local_addr().unwrap().ip()
    };
    let lim = KeptAliveLimits {
        label: "test listener",
        max_conns: 8,
        per_ip: 8,
        judged_share: 4,
        header_read_timeout: Duration::from_secs(60),
    };
    let open = lim.per_ip - lim.judged_share;
    let (addr, _stop, _done) = serving_judged(ok_app(), lim, crate::local_peer::this_machine());
    let (_from_lan, closed, _) = forwarder_from(addr, lim.per_ip, lim.judged_share, Some(lan));
    assert_eq!(
        closed, lim.judged_share,
        "from {lan}: the open share ({open}) is held"
    );
    let (_from_loopback, closed, _) = forwarder(addr, open, open);
    assert_eq!(
        closed, open,
        "from 127.0.0.1 too: the open share is already held"
    );
    assert_eq!(
        served_of(addr, 4, Duration::from_millis(50)),
        4,
        "the operator is served"
    );
}

/// A stop ends a connection still waiting for its judgment at once, never
/// after the lookup's bound.
#[test]
fn a_stop_ends_a_connection_waiting_for_its_judgment() {
    let hung = crate::local_peer::Rounds::new(
        Arc::new(|_| {
            std::thread::sleep(Duration::from_secs(4));
            None
        }),
        Arc::new(|| Some(Vec::new())),
        Duration::from_secs(5),
    );
    let lim = KeptAliveLimits {
        label: "test listener",
        max_conns: 8,
        per_ip: 4,
        judged_share: 4,
        header_read_timeout: Duration::from_secs(30),
    };
    let (addr, stop, done) = serving_judged(ok_app(), lim, hung);
    let _waiting = std::net::TcpStream::connect(addr).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    stop.send(true).unwrap();
    assert!(
        done.recv_timeout(Duration::from_secs(2)).is_ok(),
        "the server returned while the judgment was still running"
    );
}

/// A listener that keeps no judged share still judges its loopback peers'
/// trust on their first request: the operator's connection reads as the
/// operator's, which needs the listener's own end of it.
#[test]
fn a_listener_with_no_judged_share_still_knows_the_operators_connection() {
    let app = Router::new().route(
        "/me",
        axum::routing::get(
            |axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<SocketAddr>| async move {
                crate::local_peer::is_operator(peer).to_string()
            },
        ),
    );
    let (addr, _stop, _done) =
        serving_judged(app, limits(4, 4, Duration::from_secs(30)), not_seeing(""));
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.write_all(b"GET /me HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    s.set_read_timeout(Some(Duration::from_secs(6))).unwrap();
    let mut got = Vec::new();
    let mut b = [0u8; 512];
    while !String::from_utf8_lossy(&got).contains("true")
        && !String::from_utf8_lossy(&got).contains("false")
    {
        match s.read(&mut b) {
            Ok(0) | Err(_) => break,
            Ok(n) => got.extend_from_slice(&b[..n]),
        }
    }
    let got = String::from_utf8_lossy(&got);
    assert!(got.ends_with("true"), "judged the operator's: {got:?}");
}

/// A container that connects and hangs up at once cannot crowd the operator
/// out of the viewer either: such a connection is gone before any process
/// can be seen holding it, so it is never refused as a container's, but it
/// gives its slots back as soon as the reset arrives, never after its
/// judgment. With the forwarder holding the open share and 64 threads (as
/// many as the judged share) resetting as fast as they connect, every one of
/// this process's new connections is served. (A request sent and then
/// closed is a hang-up too: [`hung_up_notices_a_close_behind_unread_data`].)
#[test]
fn a_container_that_connects_and_hangs_up_never_crowds_the_operator_out_of_the_viewer() {
    let _ = crate::raise_open_file_limit(8192);
    let (addr, _stop, _done) = serving_with(
        Box::new(|l, app, rx| Box::pin(crate::serve_viewer(l, app, rx))),
        ok_app(),
    );
    let open = VIEWER.per_ip - VIEWER.judged_share;
    let _container = flicking_forwarder(addr, open, 64, 4);
    std::thread::sleep(Duration::from_millis(300));
    let served = served_of(addr, 10, Duration::from_millis(100));
    assert_eq!(
        served, 10,
        "64 threads resetting: the operator served {served} of 10"
    );
}

/// A forwarder's cached connection end never refuses a later connection
/// from the same port: a container's forwarder is named, its connection
/// from one port is closed as a known forwarder's, and it exits. The
/// operator, holding the open share itself, then connects from that port
/// and is served.
#[test]
fn a_known_forwarders_old_connection_never_refuses_a_later_one_from_its_port() {
    let _ = crate::raise_open_file_limit(4096);
    let (addr, _stop, _done) = serving_with(
        Box::new(|l, app, rx| Box::pin(crate::serve_viewer(l, app, rx))),
        ok_app(),
    );
    let open = VIEWER.per_ip - VIEWER.judged_share;
    let port = darkmux_types::test_ports::fixture_port();
    let (container, lines) = stand_in(&format!("tag {addr} {open} {port}"));
    let said: Vec<String> = lines.collect();
    let stderr = container.stderr();
    drop(container);
    assert!(
        said.iter().any(|l| l.contains("named true"))
            && said.iter().any(|l| l.contains("tagged true")),
        "the forwarder was named and its connection from {port} closed: {said:?}; its stderr: {stderr}"
    );
    std::thread::sleep(Duration::from_millis(300));
    let _mine: Vec<_> = (0..open).map(|_| resetting(addr)).collect();
    std::thread::sleep(Duration::from_millis(200));
    let mut again = connect_from_port(addr, port);
    again
        .set_read_timeout(Some(Duration::from_secs(6)))
        .unwrap();
    again
        .write_all(b"GET /ok HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    let got = get_answer(&mut again);
    assert!(
        got.starts_with("HTTP/1.1 200"),
        "the operator's connection from {port}: {got:?}"
    );
}

/// The forwarder machine these tests read through: Docker Desktop's
/// forwarder (pid 68948) holds the ends `theirs`, and every read of it is
/// counted.
fn forwarder_machine(theirs: Vec<SocketAddr>) -> &'static darkmux_platform::FakePlatform {
    Box::leak(Box::new(darkmux_platform::FakePlatform {
        process_names: vec![(68948, "com.docker.backend".into())],
        tcp_peers: theirs.into_iter().map(|end| (end, 68948)).collect(),
        ..Default::default()
    }))
}

/// Rounds that know Docker Desktop's forwarder (pid 68948) and read no
/// table of their own.
fn knowing_the_forwarder() -> Arc<crate::local_peer::Rounds> {
    let rounds =
        crate::local_peer::Rounds::new_for_test(Arc::new(|_| None), Duration::from_secs(1));
    rounds.remember(vec![(68948, "com.docker.backend".into())]);
    rounds
}

/// A real accepted connection on a fresh loopback listener, as admission
/// hands it over (`judge_first` as given), and the client's end of it.
async fn admitted(judge_first: bool) -> (Admitted, tokio::net::TcpStream) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (stream, peer) = listener.accept().await.unwrap();
    let mut conn = Admission::new("test", 4, 4).admit(stream, peer).unwrap();
    conn.local = conn.stream.local_addr().ok();
    conn.judge_first = judge_first;
    (conn, client)
}

/// Every connection accepted unjudged takes its own end out of the
/// forwarders' cache, so a later connection reusing its port is not taken
/// for it; one judged first is refused as the forwarder's.
#[tokio::test]
async fn an_accepted_connection_takes_its_end_out_of_the_cache() {
    let (conn, _client) = admitted(false).await;
    let local = conn.local.unwrap();
    let (judged, _judged_client) = admitted(true).await;
    let m = forwarder_machine(vec![conn.peer, judged.peer]);
    let rounds = knowing_the_forwarder();
    let unrelated: SocketAddr = "127.0.0.1:1".parse().unwrap();
    assert!(!rounds.held_by_a_known_forwarder(m, unrelated, local).await);
    assert!(
        !refused_on_accept(&rounds, m, &conn).await,
        "unjudged, so served"
    );
    assert_eq!(
        rounds.take_end(conn.peer, local),
        None,
        "and its end left the cache"
    );
    assert!(
        refused_on_accept(&rounds, m, &judged).await,
        "the forwarder's, judged first"
    );
}

/// A connection judged first that its peer has already reset is closed as
/// it is accepted with no read of the forwarders; a live one is read for.
#[tokio::test]
async fn a_reset_connection_is_closed_without_a_read() {
    use std::sync::atomic::Ordering::SeqCst;
    let (conn, client) = admitted(true).await;
    let m = forwarder_machine(vec![]);
    let rounds = knowing_the_forwarder();
    let client = client.into_std().unwrap();
    {
        use std::os::fd::AsRawFd;
        let off = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        // SAFETY: a socket this test owns, and a `linger` of the size given.
        unsafe {
            libc::setsockopt(
                client.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                (&off as *const libc::linger).cast(),
                std::mem::size_of::<libc::linger>() as libc::socklen_t,
            )
        };
    }
    drop(client);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        refused_on_accept(&rounds, m, &conn).await,
        "gone, so closed"
    );
    assert_eq!(m.tcp_ends_reads.load(SeqCst), 0, "with no read");
    let (live, _client) = admitted(true).await;
    assert!(
        !refused_on_accept(&rounds, m, &live).await,
        "not the forwarder's"
    );
    assert_eq!(m.tcp_ends_reads.load(SeqCst), 1, "read for");
}

/// A second miss on one listener waits for its turn: the next read begins
/// only once the last has been over for at least the read gap.
#[tokio::test]
async fn a_second_miss_waits_its_turn_to_read() {
    use std::sync::atomic::Ordering::SeqCst;
    let m = forwarder_machine(vec![]);
    let rounds = knowing_the_forwarder();
    let at = |s: &str| -> SocketAddr { s.parse().unwrap() };
    let local = at("127.0.0.1:8799");
    assert!(
        !rounds
            .held_by_a_known_forwarder(m, at("127.0.0.1:50001"), local)
            .await
    );
    let began = tokio::time::Instant::now();
    assert!(
        !rounds
            .held_by_a_known_forwarder(m, at("127.0.0.1:50002"), local)
            .await
    );
    assert!(
        began.elapsed() >= Duration::from_millis(10),
        "waited its turn: {:?}",
        began.elapsed()
    );
    assert_eq!(m.tcp_ends_reads.load(SeqCst), 2);
}

/// A peer's hang-up is noticed while a connection waits for its judgment,
/// behind unread data too: a request sent and then closed, and a reset,
/// end the wait at once; an open connection with an unread request does
/// not.
#[tokio::test]
async fn hung_up_notices_a_close_behind_unread_data() {
    use tokio::io::AsyncWriteExt;
    let pair = || async {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = tokio::net::TcpStream::connect(l.local_addr().unwrap())
            .await
            .unwrap();
        (l.accept().await.unwrap().0, client)
    };
    let soon = Duration::from_millis(500);
    let (server, mut client) = pair().await;
    client
        .write_all(b"GET /ok HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    drop(client);
    assert!(
        tokio::time::timeout(soon, hung_up(&server)).await.is_ok(),
        "a request sent, then closed"
    );
    let (server, mut client) = pair().await;
    client
        .write_all(b"GET /ok HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), hung_up(&server))
            .await
            .is_err(),
        "open, its request unread"
    );
    let client = client.into_std().unwrap();
    {
        use std::os::fd::AsRawFd;
        let off = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        // SAFETY: a socket this test owns, and a `linger` of the size given.
        unsafe {
            libc::setsockopt(
                client.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                (&off as *const libc::linger).cast(),
                std::mem::size_of::<libc::linger>() as libc::socklen_t,
            )
        };
    }
    drop(client);
    assert!(
        tokio::time::timeout(soon, hung_up(&server)).await.is_ok(),
        "reset"
    );
}

/// A read of a known forwarder's connections never caches the end of one
/// this listener already holds: the forwarder holds the open share, one
/// connection from a fixture port among them; a later read sees them all;
/// then that one is reset while the forwarder keeps running. The operator
/// takes the freed slot, then connects from the same port, and is served.
#[test]
fn a_held_connections_end_is_never_cached_to_refuse_a_later_one() {
    let _ = crate::raise_open_file_limit(4096);
    let (addr, _stop, _done) = serving_with(
        Box::new(|l, app, rx| Box::pin(crate::serve_viewer(l, app, rx))),
        ok_app(),
    );
    let open = VIEWER.per_ip - VIEWER.judged_share;
    let port = darkmux_types::test_ports::fixture_port();
    let (container, lines) = stand_in(&format!("drop-one {addr} {open} {port}"));
    let said: Vec<String> = lines.take_while(|l| !l.contains("ready")).collect();
    assert!(
        said.iter().any(|l| l.contains("named true"))
            && said.iter().any(|l| l.contains("read true")),
        "the forwarder was named and read: {said:?}; its stderr: {}",
        container.stderr()
    );
    std::thread::sleep(Duration::from_millis(200));
    let served = |s: &mut std::net::TcpStream| {
        s.set_read_timeout(Some(Duration::from_secs(6))).unwrap();
        let _ = s.write_all(b"GET /ok HTTP/1.1\r\nHost: x\r\n\r\n");
        get_answer(s)
    };
    let mut first = std::net::TcpStream::connect(addr).unwrap();
    assert!(
        served(&mut first).starts_with("HTTP/1.1 200"),
        "the freed slot"
    );
    let mut again = connect_from_port(addr, port);
    let got = served(&mut again);
    assert!(
        got.starts_with("HTTP/1.1 200"),
        "the operator's connection from {port}, past the open share: {got:?}"
    );
    reset(first);
    reset(again);
}

/// A connection judged first that its peer has already reset still takes
/// its end out of the forwarders' cache as it is closed.
#[tokio::test]
async fn a_reset_connection_takes_its_end_out_of_the_cache() {
    let (conn, client) = admitted(true).await;
    let local = conn.local.unwrap();
    let m = forwarder_machine(vec![conn.peer]);
    let rounds = knowing_the_forwarder();
    let unrelated: SocketAddr = "127.0.0.1:1".parse().unwrap();
    assert!(!rounds.held_by_a_known_forwarder(m, unrelated, local).await);
    reset(client.into_std().unwrap());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        refused_on_accept(&rounds, m, &conn).await,
        "gone, so closed"
    );
    assert_eq!(
        rounds.take_end(conn.peer, local),
        None,
        "its end went with it"
    );
}
