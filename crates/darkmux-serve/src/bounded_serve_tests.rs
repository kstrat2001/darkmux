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
