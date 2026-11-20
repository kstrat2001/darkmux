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

/// Whether the other end closes `s` within `d` (whatever it sent first).
fn closed_within(s: &mut std::net::TcpStream, d: Duration) -> bool {
    let until = std::time::Instant::now() + d;
    let mut b = [0u8; 256];
    loop {
        let left = until.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        s.set_read_timeout(Some(left)).unwrap();
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
            let held: Vec<_> = (0..num(2)).map(|_| connect_from(addr, source)).collect();
            let closed = closed_count(&held, num(3), Duration::from_secs(8));
            println!("closed {closed}");
            held
        }
        _ => churn(addr, num(2), num(3), Duration::from_secs(num(4) as u64)),
    };
    let _ = std::io::stdin().read_to_end(&mut Vec::new());
}

/// A connection to `addr` from this machine's address `source` (the
/// default one when `None`).
fn connect_from(addr: SocketAddr, source: Option<std::net::IpAddr>) -> std::net::TcpStream {
    let Some(source) = source else {
        return std::net::TcpStream::connect(addr).unwrap();
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    let s = rt.block_on(async {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind(SocketAddr::new(source, 0)).unwrap();
        socket.connect(addr).await.unwrap()
    });
    let s = s.into_std().unwrap();
    s.set_nonblocking(false).unwrap();
    s
}

/// [`container_forwarder_stand_in`]'s `churn` mode: the held connections.
fn churn(
    addr: SocketAddr,
    hold: usize,
    threads: usize,
    for_: Duration,
) -> Vec<std::net::TcpStream> {
    let held: Vec<_> = (0..hold)
        .map(|_| std::net::TcpStream::connect(addr).unwrap())
        .collect();
    let until = std::time::Instant::now() + for_;
    for _ in 0..threads {
        std::thread::spawn(move || {
            while std::time::Instant::now() < until {
                if let Ok(mut s) = std::net::TcpStream::connect(addr) {
                    let _ = s.set_read_timeout(Some(Duration::from_secs(9)));
                    let _ = s.read(&mut [0u8; 1]);
                }
            }
        });
    }
    println!("churning");
    held
}

/// How many of `conns` the other end has closed, once `want` have been or
/// `within` has passed.
fn closed_count(conns: &[std::net::TcpStream], want: usize, within: Duration) -> usize {
    for c in conns {
        c.set_nonblocking(true).unwrap();
    }
    let until = std::time::Instant::now() + within;
    loop {
        let closed = conns.iter().filter(|c| is_closed(c)).count();
        if closed >= want || std::time::Instant::now() >= until {
            return closed;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether the other end of nonblocking `c` has closed it.
fn is_closed(mut c: &std::net::TcpStream) -> bool {
    match c.read(&mut [0u8; 1]) {
        Ok(n) => n == 0,
        Err(e) => e.kind() != std::io::ErrorKind::WouldBlock,
    }
}

/// A running [`container_forwarder_stand_in`], stopped when dropped.
struct Forwarder {
    child: std::process::Child,
    _exe_dir: tempfile::TempDir,
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
/// `closing` of them have been closed (or 8 s passed), with how many were.
fn forwarder(addr: SocketAddr, n: usize, closing: usize) -> (Forwarder, usize) {
    forwarder_from(addr, n, closing, None)
}

/// [`forwarder`], its connections from this machine's address `source`.
fn forwarder_from(
    addr: SocketAddr,
    n: usize,
    closing: usize,
    source: Option<std::net::IpAddr>,
) -> (Forwarder, usize) {
    let source = source.map_or("-".to_string(), |s| s.to_string());
    let (held, mut lines) = stand_in(&format!("hold {addr} {n} {closing} {source}"));
    // libtest prints the test's name on the same line first.
    let closed = lines
        .find_map(|l| {
            l.rsplit_once("closed ")
                .map(|(_, k)| k.trim().parse().unwrap())
        })
        .expect("the stand-in reports what it holds");
    (held, closed)
}

/// A container engine's forwarder holding `hold` connections to `addr` and
/// reopening every other one it loses on `threads` threads for `for_secs`
/// ([`container_forwarder_stand_in`]'s `churn` mode), once it has begun.
fn churning_forwarder(addr: SocketAddr, hold: usize, threads: usize, for_secs: u64) -> Forwarder {
    let (held, mut lines) = stand_in(&format!("churn {addr} {hold} {threads} {for_secs}"));
    assert!(
        lines.any(|l| l.contains("churning")),
        "the stand-in began churning"
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
        _exe_dir: dir,
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
            .stderr(std::process::Stdio::null())
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
    let (_container, closed) = forwarder(addr, 2 * lim.per_ip, 2 * lim.per_ip - open);
    assert_eq!(
        closed,
        2 * lim.per_ip - open,
        "the container holds only the open share ({open})"
    );
    let mut mine: Vec<_> = (1..lim.judged_share)
        .map(|_| std::net::TcpStream::connect(addr).unwrap())
        .collect();
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
        let mut mine: Vec<_> = (0..lim.per_ip)
            .map(|_| std::net::TcpStream::connect(addr).unwrap())
            .collect();
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
        let (_container, closed) = forwarder(addr, lim.per_ip, lim.judged_share);
        assert_eq!(
            closed, lim.judged_share,
            "{name}: the container's judged connections are closed"
        );
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
/// and 64, then 128, threads reconnecting as fast as they are closed,
/// every one of this process's new connections is served. Once a round
/// has named the forwarder, its connections past the open share are refused
/// as they are accepted, without waiting for a round while holding a slot.
#[test]
fn a_reconnecting_container_never_crowds_the_operator_out_of_the_viewer() {
    let _ = crate::raise_open_file_limit(8192);
    for threads in [64, 128] {
        let (addr, _stop, _done) = serving_with(
            Box::new(|l, app, rx| Box::pin(crate::serve_viewer(l, app, rx))),
            ok_app(),
        );
        let open = VIEWER.per_ip - VIEWER.judged_share;
        let _container = churning_forwarder(addr, open, threads, 8);
        std::thread::sleep(Duration::from_millis(300));
        let served = served_of(addr, 20, Duration::from_millis(100));
        assert_eq!(
            served, 20,
            "{threads} reconnecting threads: the operator served {served} of 20"
        );
    }
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
    let (_from_lan, closed) = forwarder_from(addr, lim.per_ip, lim.judged_share, Some(lan));
    assert_eq!(
        closed, lim.judged_share,
        "from {lan}: the open share ({open}) is held"
    );
    let (_from_loopback, closed) = forwarder(addr, open, open);
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
