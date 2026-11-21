use super::*;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::Path;

/// The viewer daemon's router serving on a real loopback listener, through
/// the entry point `run` serves it with (`serve_viewer`); its address.
async fn viewer(flows: &Path) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    crate::record_bound_addr(Some(addr));
    let app = crate::build_router(flows.to_path_buf());
    let (stop, stopped) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let _running = stop;
        crate::serve_viewer(listener, app, stopped).await
    });
    addr
}

/// A probe app served through the client listener's entry point
/// (`client_pairing::serve_client_listener`): `/health` answers whether
/// the request's peer was judged the operator's. Its address.
async fn client_listener_probe() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new().route(
        "/health",
        axum::routing::get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move {
            axum::Json(serde_json::json!({ "operator": is_operator(peer) }))
        }),
    );
    let (stop, stopped) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let _running = stop;
        crate::client_pairing::serve_client_listener(listener, app, stopped).await
    });
    addr
}

/// The JSON body of an HTTP response, or `Null` when there is none.
fn body_of(raw: &str) -> serde_json::Value {
    raw.split_once("\r\n\r\n")
        .map_or(raw, |(_, b)| b)
        .trim()
        .parse()
        .unwrap_or(serde_json::Value::Null)
}

/// `/health` asked by this test process itself, connecting to `host` (a
/// loopback literal, bracketed for IPv6) with a `Host` naming the daemon.
async fn health_from_here(host: &str, port: u16) -> serde_json::Value {
    let target = format!("{host}:{port}");
    let raw = tokio::task::spawn_blocking(move || {
        let mut s = std::net::TcpStream::connect(target.as_str()).unwrap();
        write!(
            s,
            "GET /health HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    })
    .await
    .unwrap();
    body_of(&raw)
}

/// `/health` asked by `curl`, a child of this test process connecting to
/// `host`. With `sandbox`, the child is started the way a Seatbelt launcher
/// is: leading a session of its own, its pid recorded in that sandbox's
/// directory before its program runs, and the program its child.
async fn health_from_child(host: &str, port: u16, sandbox: Option<&Path>) -> serde_json::Value {
    let url = format!("http://{host}:{port}/health");
    let dir = sandbox
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(r#"[ -z "$1" ] || echo $$ > "$1/pid"; curl -s -m 10 -H "Host: localhost:$2" "$3"; echo"#)
        .args(["sh", &dir, &port.to_string(), &url]);
    if sandbox.is_some() {
        darkmux_platform::in_a_session_of_its_own(&mut cmd);
    }
    let out = tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let body = body_of(&text);
    assert!(
        body.is_object(),
        "curl reached the daemon: {text:?} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    body
}

/// A host sandbox's state directory under `root`, as Seatbelt writes one
/// (its profile there), with no pid yet.
fn sandbox_dir(root: &Path) -> std::path::PathBuf {
    let d = root.join("dispatch-1");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("profile.sb"), "(version 1)").unwrap();
    d
}

/// Sandbox state read from `root`, restored afterwards.
struct StateDir(Option<std::ffi::OsString>);

impl StateDir {
    fn at(root: &Path) -> Self {
        let prev = std::env::var_os(darkmux_sandbox::seatbelt::STATE_DIR_ENV);
        unsafe { std::env::set_var(darkmux_sandbox::seatbelt::STATE_DIR_ENV, root) };
        StateDir(prev)
    }
}

impl Drop for StateDir {
    fn drop(&mut self) {
        match self.0.take() {
            Some(v) => unsafe { std::env::set_var(darkmux_sandbox::seatbelt::STATE_DIR_ENV, v) },
            None => unsafe { std::env::remove_var(darkmux_sandbox::seatbelt::STATE_DIR_ENV) },
        }
    }
}

/// The operator's own processes keep the no-token path: this test process,
/// and a process it starts outside any sandbox (the CLI from a shell), over
/// 127.0.0.1 and over the IPv4-mapped address alike.
#[tokio::test]
#[serial_test::serial]
async fn this_machines_own_processes_are_local() {
    let flows = tempfile::TempDir::new().unwrap();
    let root = tempfile::TempDir::new().unwrap();
    let _state = StateDir::at(root.path());
    let _running = sandbox_dir(root.path());
    let addr = viewer(flows.path()).await;
    let here = health_from_here("127.0.0.1", addr.port()).await;
    assert!(
        here["fleet_token_set"].is_boolean(),
        "this process is this machine: {here}"
    );
    let mapped = health_from_here("[::ffff:127.0.0.1]", addr.port()).await;
    assert!(
        mapped["fleet_token_set"].is_boolean(),
        "mapped, still this process: {mapped}"
    );
    let child = health_from_child("127.0.0.1", addr.port(), None).await;
    assert!(
        child["fleet_token_set"].is_boolean(),
        "a process outside any sandbox: {child}"
    );
}

/// A process in a host sandbox's session is not this machine: it gets the
/// answer a remote reader gets, whatever `Host` it sends, over 127.0.0.1
/// and over the IPv4-mapped address alike.
#[tokio::test]
#[serial_test::serial]
async fn a_process_in_a_host_sandbox_is_not_local() {
    let flows = tempfile::TempDir::new().unwrap();
    let root = tempfile::TempDir::new().unwrap();
    let _state = StateDir::at(root.path());
    let dir = sandbox_dir(root.path());
    let addr = viewer(flows.path()).await;
    let direct = health_from_child("127.0.0.1", addr.port(), Some(&dir)).await;
    assert!(
        direct["fleet_token_set"].is_null(),
        "sandboxed work is remote: {direct}"
    );
    let mapped = health_from_child("[::ffff:127.0.0.1]", addr.port(), Some(&dir)).await;
    assert!(
        mapped["fleet_token_set"].is_null(),
        "and over the mapped address: {mapped}"
    );
}

/// The client listener judges a loopback peer as the viewer does: this
/// process and a process outside any sandbox are the operator's, a process
/// in a host sandbox's session is not.
#[tokio::test]
#[serial_test::serial]
async fn the_client_listener_judges_a_loopback_peer_as_the_viewer_does() {
    let root = tempfile::TempDir::new().unwrap();
    let _state = StateDir::at(root.path());
    let dir = sandbox_dir(root.path());
    let addr = client_listener_probe().await;
    let here = health_from_here("127.0.0.1", addr.port()).await;
    assert_eq!(here["operator"], true, "this process: {here}");
    let child = health_from_child("127.0.0.1", addr.port(), None).await;
    assert_eq!(
        child["operator"], true,
        "a process outside any sandbox: {child}"
    );
    let sandboxed = health_from_child("127.0.0.1", addr.port(), Some(&dir)).await;
    assert_eq!(
        sandboxed["operator"], false,
        "sandboxed work is not the operator: {sandboxed}"
    );
}

/// A fake machine: the operator's CLI (pid 51), a host sandbox led by 100
/// with its program (101), and Docker Desktop's forwarder (68948).
fn machine() -> darkmux_platform::FakePlatform {
    darkmux_platform::FakePlatform {
        process_table: Some(vec![
            (50, 1, 50, 50),
            (51, 50, 51, 50),
            (100, 50, 100, 100),
            (101, 100, 100, 100),
        ]),
        process_names: vec![
            (51, "darkmux".into()),
            (100, "sh".into()),
            (101, "curl".into()),
            (68948, "com.docker.backend".into()),
            (7, "docker-proxy".into()),
        ],
        ..Default::default()
    }
}

/// A connection held by a container engine's forwarder is a container's,
/// whether or not a host sandbox runs: the engine forwards every
/// container's `host.docker.internal` to this machine's loopback.
#[test]
fn a_container_engines_forwarder_is_never_the_operator() {
    let m = machine();
    assert_eq!(
        judge(&m, &[68948], Some(&[])),
        PeerProcess::ContainerForwarder("com.docker.backend".into())
    );
    assert_eq!(
        judge(&m, &[68948], Some(&[100])),
        PeerProcess::ContainerForwarder("com.docker.backend".into())
    );
    assert_eq!(
        judge(&m, &[7], Some(&[])),
        PeerProcess::ContainerForwarder("docker-proxy".into())
    );
    assert_eq!(
        judge(&m, &[51, 68948], Some(&[])),
        PeerProcess::ContainerForwarder("com.docker.backend".into()),
        "any holder"
    );
}

/// The operator's process is the operator's with or without a sandbox
/// running; a sandbox's processes are sandboxed work.
#[test]
fn a_host_sandboxs_processes_are_sandboxed_work_and_the_operators_are_not() {
    let m = machine();
    assert_eq!(judge(&m, &[51], Some(&[])), PeerProcess::Operator);
    assert_eq!(judge(&m, &[51], Some(&[100])), PeerProcess::Operator);
    assert_eq!(judge(&m, &[101], Some(&[100])), PeerProcess::SandboxedWork);
    assert_eq!(
        judge(&m, &[100], Some(&[100])),
        PeerProcess::SandboxedWork,
        "the launcher itself"
    );
    assert_eq!(
        judge(&m, &[51, 101], Some(&[100])),
        PeerProcess::SandboxedWork,
        "any holder"
    );
}

/// What cannot be read is never the operator: no holder this user can see
/// (another user's process), a holder with no name, unreadable sandbox
/// state, an unreadable process table while a sandbox runs.
#[test]
fn what_cannot_be_read_is_unknown_never_the_operator() {
    let m = machine();
    let unknown = |p: PeerProcess| matches!(p, PeerProcess::Unknown(_));
    assert!(unknown(judge(&m, &[], Some(&[]))), "no holder");
    assert!(unknown(judge(&m, &[999], Some(&[]))), "no name");
    assert!(unknown(judge(&m, &[51], None)), "no sandbox state");
    let blind = darkmux_platform::FakePlatform {
        process_table: None,
        ..machine()
    };
    assert!(
        unknown(judge(&blind, &[51], Some(&[100]))),
        "no process table"
    );
    assert_eq!(
        judge(&blind, &[51], Some(&[])),
        PeerProcess::Operator,
        "no sandbox runs, so no table is needed"
    );
}

/// A round's holders of a peer are found however the address is spelled.
#[test]
fn a_peers_holders_are_found_by_its_canonical_address() {
    let v4: SocketAddr = "127.0.0.1:50000".parse().unwrap();
    let mapped: SocketAddr = "[::ffff:127.0.0.1]:50000".parse().unwrap();
    let other: SocketAddr = "127.0.0.1:50001".parse().unwrap();
    let round = [(v4, 9), (other, 8), (mapped, 4), (v4, 3), (v4, 9)];
    assert_eq!(holders_in(&round, mapped), vec![3, 4, 9]);
    assert_eq!(holders_in(&round, v4), vec![3, 4, 9], "each once");
    assert_eq!(
        holders_in(&round, "127.0.0.1:1".parse().unwrap()),
        Vec::<u32>::new()
    );
}

/// A peer is the operator's only while a connection judged so is open,
/// under either spelling of its address.
#[test]
fn a_peer_is_the_operators_only_while_its_connection_lasts() {
    let peer: SocketAddr = "127.0.0.1:61234".parse().unwrap();
    assert!(!is_operator(peer), "no connection judged it");
    let verdict = Verdict {
        remote: peer,
        judged: tokio::sync::OnceCell::new_with(Some(PeerProcess::Operator)),
    };
    hold(peer);
    assert!(is_operator("[::ffff:127.0.0.1]:61234".parse().unwrap()));
    drop(verdict);
    assert!(!is_operator(peer), "its connection ended");
    let other = Verdict {
        remote: peer,
        judged: tokio::sync::OnceCell::new_with(Some(PeerProcess::SandboxedWork)),
    };
    drop(other);
    assert!(!is_operator(peer));
}

/// `is_local_at` asks the verdict: a loopback peer with every header this
/// machine sends is not local until its connection is judged the
/// operator's.
#[test]
#[serial_test::serial]
fn a_loopback_peer_is_local_only_once_judged_the_operators() {
    let peer: SocketAddr = "127.0.0.1:61235".parse().unwrap();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(axum::http::header::HOST, "localhost".parse().unwrap());
    assert!(!crate::is_local_at(Some(peer), &headers, None), "unjudged");
    state_operator(peer);
    assert!(crate::is_local_at(Some(peer), &headers, None));
    release(peer);
}

/// A request a proxy forwarded is never this machine's, so its connection
/// costs no lookup of this machine's connections.
#[tokio::test]
#[serial_test::serial]
async fn a_proxied_request_costs_no_lookup() {
    let flows = tempfile::TempDir::new().unwrap();
    let addr = viewer(flows.path()).await;
    let port = addr.port();
    let raw = tokio::task::spawn_blocking(move || {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET /health HTTP/1.1\r\nHost: localhost:{port}\r\nX-Forwarded-For: 100.64.0.7\r\nConnection: close\r\n\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    })
    .await
    .unwrap();
    assert!(
        body_of(&raw)["fleet_token_set"].is_null(),
        "proxied is remote: {raw}"
    );
    assert!(!ROUNDS.has_read(addr), "no round ran");
    let here = health_from_here("127.0.0.1", port).await;
    assert!(here["fleet_token_set"].is_boolean(), "{here}");
    assert!(
        ROUNDS.has_read(addr),
        "the inverse: a request that may be local is looked up"
    );
}

/// A peer at 127.0.0.1:50000 held by pid 9, read by `read` after `delay`.
fn reader(
    delay: std::time::Duration,
    stuck_at: Option<SocketAddr>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
) -> PeersReader {
    Arc::new(move |local| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(if Some(local) == stuck_at {
            std::time::Duration::from_secs(6)
        } else {
            delay
        });
        Some(vec![("127.0.0.1:50000".parse().unwrap(), 9)])
    })
}

/// A read that hangs is no answer once the bound passes, for the connection
/// that started it and for one waiting on it alike, and it holds up no other
/// listener's round meanwhile.
#[tokio::test]
async fn a_hung_read_is_no_answer_within_the_bound_and_holds_up_no_other_listener() {
    let (stuck, free): (SocketAddr, SocketAddr) = (
        "127.0.0.1:1".parse().unwrap(),
        "127.0.0.1:2".parse().unwrap(),
    );
    let peer: SocketAddr = "127.0.0.1:50000".parse().unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let rounds = Rounds::new_for_test(
        reader(std::time::Duration::ZERO, Some(stuck), calls),
        std::time::Duration::from_millis(400),
    );
    let begun = Instant::now();
    let (a, b) = (rounds.clone(), rounds.clone());
    let first = tokio::spawn(async move { a.holders_of(stuck, peer, begun).await });
    let waiter = tokio::spawn(async move { b.holders_of(stuck, peer, begun).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        rounds.holders_of(free, peer, Instant::now()).await,
        Some(vec![9]),
        "another listener answers"
    );
    assert!(
        begun.elapsed() < std::time::Duration::from_secs(3),
        "while the hung read runs: {:?}",
        begun.elapsed()
    );
    assert_eq!(first.await.unwrap(), None, "the hung read is no answer");
    assert_eq!(waiter.await.unwrap(), None, "nor for one waiting on it");
    assert!(
        begun.elapsed() < std::time::Duration::from_secs(4),
        "within the bound: {:?}",
        begun.elapsed()
    );
}

/// Connections accepted before a round began share its one read.
#[tokio::test]
async fn connections_waiting_together_share_one_read() {
    let local: SocketAddr = "127.0.0.1:3".parse().unwrap();
    let peer: SocketAddr = "127.0.0.1:50000".parse().unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let rounds = Rounds::new_for_test(
        reader(std::time::Duration::from_millis(100), None, calls.clone()),
        std::time::Duration::from_secs(5),
    );
    let accepted = Instant::now();
    let waiting: Vec<_> = (0..5)
        .map(|_| {
            let r = rounds.clone();
            tokio::spawn(async move { r.holders_of(local, peer, accepted).await })
        })
        .collect();
    for w in waiting {
        assert_eq!(w.await.unwrap(), Some(vec![9]));
    }
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one read"
    );
    assert_eq!(
        rounds.holders_of(local, peer, Instant::now()).await,
        Some(vec![9])
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "a connection accepted after it gets a read of its own"
    );
}

/// The host's sandbox launchers are read once per round, so connections
/// waiting together share that look too, and a look that hangs is no
/// answer within the bound: the caller is remote, never left waiting.
#[tokio::test]
async fn a_round_reads_the_sandbox_launchers_once_and_a_hung_read_is_no_answer() {
    let local: SocketAddr = "127.0.0.1:4".parse().unwrap();
    let peer: SocketAddr = "127.0.0.1:50000".parse().unwrap();
    let looks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = looks.clone();
    let launchers: LaunchersReader = Arc::new(move || {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(100));
        Some(vec![100])
    });
    let peers = reader(
        std::time::Duration::ZERO,
        None,
        Arc::new(Default::default()),
    );
    let rounds = Rounds::new(peers.clone(), launchers, std::time::Duration::from_secs(5));
    let accepted = Instant::now();
    let waiting: Vec<_> = (0..5)
        .map(|_| {
            let r = rounds.clone();
            tokio::spawn(async move { r.look(local, peer, accepted).await })
        })
        .collect();
    for w in waiting {
        let looked = w.await.unwrap().expect("an answer");
        assert_eq!(
            (looked.holders, looked.launchers),
            (vec![9], Some(vec![100]))
        );
    }
    assert_eq!(
        looks.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one look at the launchers"
    );
    let hung: LaunchersReader = Arc::new(|| {
        std::thread::sleep(std::time::Duration::from_secs(6));
        Some(vec![])
    });
    let stuck = Rounds::new(peers, hung, std::time::Duration::from_millis(300));
    let begun = Instant::now();
    assert!(stuck.look(local, peer, begun).await.is_none(), "no answer");
    assert!(
        begun.elapsed() < std::time::Duration::from_secs(4),
        "{:?}",
        begun.elapsed()
    );
}

/// Only work darkmux contains is kept out of the judged slots: what could
/// not be told is not contained, so a failed lookup never shuts the
/// operator, or a peer this user cannot see, out.
#[test]
fn only_contained_work_is_kept_out_of_the_judged_slots() {
    assert!(PeerProcess::SandboxedWork.is_contained());
    assert!(PeerProcess::ContainerForwarder("com.docker.backend".into()).is_contained());
    assert!(!PeerProcess::Operator.is_contained());
    assert!(!PeerProcess::Unknown("no process this user can see").is_contained());
}

/// A remembered forwarder's connections are known from one read of its
/// descriptors: the connections it held then are answered from that read
/// with no other, each end once (a later connection reusing its port is
/// another connection), the operator's is not its, and a miss reads again.
#[tokio::test]
async fn one_read_of_a_known_forwarder_answers_every_connection_it_holds() {
    use std::sync::atomic::Ordering::SeqCst;
    let at = |s: &str| -> SocketAddr { s.parse().unwrap() };
    let (a, b, mine) = (
        at("127.0.0.1:50001"),
        at("127.0.0.1:50002"),
        at("127.0.0.1:50003"),
    );
    let local = at("127.0.0.1:8799");
    let m: &'static darkmux_platform::FakePlatform =
        Box::leak(Box::new(darkmux_platform::FakePlatform {
            tcp_peers: vec![(a, 68948), (b, 68948), (mine, 51)],
            ..machine()
        }));
    assert_eq!(
        forwarders_among(m, &[51, 68948]),
        vec![(68948, "com.docker.backend".to_string())]
    );
    let rounds = Rounds::new_for_test(Arc::new(|_| None), std::time::Duration::from_secs(1));
    assert!(
        !rounds.held_by_a_known_forwarder(m, a, local).await,
        "none named yet"
    );
    assert_eq!(m.tcp_ends_reads.load(SeqCst), 0, "and nothing to read");
    rounds.remember(forwarders_among(m, &[68948]));
    assert!(rounds.held_by_a_known_forwarder(m, a, local).await);
    assert_eq!(m.tcp_ends_reads.load(SeqCst), 1);
    assert!(
        rounds
            .held_by_a_known_forwarder(m, b, at("[::ffff:127.0.0.1]:8799"))
            .await,
        "however the listener's address is spelled"
    );
    assert_eq!(
        m.tcp_ends_reads.load(SeqCst),
        1,
        "answered by the same read"
    );
    assert_eq!(rounds.take_end(a, local), None, "an end is used once");
    assert!(
        !rounds.held_by_a_known_forwarder(m, mine, local).await,
        "the operator's"
    );
    assert_eq!(m.tcp_ends_reads.load(SeqCst), 2, "a miss reads again");
}

/// A cached end answers only while its pid still runs the forwarder it was
/// read for: one whose pid now runs something else is forgotten, with every
/// end cached for it, so it never refuses another process's connection.
#[tokio::test]
async fn a_cached_end_answers_only_while_its_forwarder_runs() {
    let at = |s: &str| -> SocketAddr { s.parse().unwrap() };
    let (a, b, c) = (
        at("127.0.0.1:50001"),
        at("127.0.0.1:50002"),
        at("127.0.0.1:50004"),
    );
    let local = at("127.0.0.1:8799");
    let m: &'static darkmux_platform::FakePlatform =
        Box::leak(Box::new(darkmux_platform::FakePlatform {
            tcp_peers: vec![(a, 68948), (b, 68948), (c, 68948)],
            ..machine()
        }));
    let rounds = Rounds::new_for_test(Arc::new(|_| None), std::time::Duration::from_secs(1));
    rounds.remember(forwarders_among(m, &[68948]));
    assert!(rounds.held_by_a_known_forwarder(m, a, local).await);
    let reused: &'static darkmux_platform::FakePlatform =
        Box::leak(Box::new(darkmux_platform::FakePlatform {
            process_names: vec![(68948, "zsh".into())],
            ..machine()
        }));
    assert!(
        !rounds.held_by_a_known_forwarder(reused, b, local).await,
        "its pid runs something else"
    );
    assert!(
        rounds.forwarders.lock().unwrap().pids.is_empty(),
        "forgotten"
    );
    assert_eq!(
        rounds.take_end(c, local),
        None,
        "with every end cached for it"
    );
}

/// A read of the forwarders for a listener may begin only once the last
/// one has been over for as long as it took, and at least
/// [`MIN_READ_GAP`]: reading takes at most half of the accept loop's time.
#[test]
fn reads_take_turns_with_the_accept_loop() {
    let t = Instant::now();
    let ms = std::time::Duration::from_millis;
    assert_eq!(next_read_at(None), None, "no read yet");
    assert_eq!(next_read_at(Some((t, ms(2)))), Some(t + MIN_READ_GAP));
    assert_eq!(next_read_at(Some((t, ms(50)))), Some(t + ms(50)));
}

/// The rounds remember at most a handful of forwarders, newest kept, each
/// once.
#[test]
fn the_known_forwarders_are_bounded() {
    let rounds = Rounds::new_for_test(Arc::new(|_| None), std::time::Duration::from_secs(1));
    rounds.remember((0..20).map(|pid| (pid, "vpnkit".to_string())).collect());
    rounds.remember(vec![(19, "vpnkit".to_string())]);
    let known = rounds.forwarders.lock().unwrap().pids.clone();
    assert_eq!(known.len(), KNOWN_FORWARDERS);
    assert_eq!(known.last(), Some(&(19, "vpnkit".to_string())));
    assert_eq!(known.iter().filter(|(p, _)| *p == 19).count(), 1);
}

/// A round names the forwarders among every process in its table, each
/// process looked at once.
#[test]
fn a_round_looks_at_each_process_once() {
    let at = |s: &str| -> SocketAddr { s.parse().unwrap() };
    let peers = [
        (at("127.0.0.1:1"), 9),
        (at("127.0.0.1:2"), 3),
        (at("127.0.0.1:3"), 9),
    ];
    assert_eq!(pids_in(&peers), vec![3, 9]);
}

/// While the listener holds a connection, no read caches its end; once the
/// connection is closed, a forwarder's later connection from that end is
/// read again.
#[tokio::test]
async fn a_held_end_is_left_out_of_reads_until_its_connection_closes() {
    let at = |s: &str| -> SocketAddr { s.parse().unwrap() };
    let (theirs, local) = (at("127.0.0.1:50001"), at("127.0.0.1:8799"));
    let m: &'static darkmux_platform::FakePlatform =
        Box::leak(Box::new(darkmux_platform::FakePlatform {
            tcp_peers: vec![(theirs, 68948)],
            ..machine()
        }));
    let rounds = Rounds::new_for_test(Arc::new(|_| None), std::time::Duration::from_secs(1));
    rounds.remember(forwarders_among(m, &[68948]));
    let held = rounds.holding(theirs, local);
    assert!(
        !rounds.held_by_a_known_forwarder(m, theirs, local).await,
        "held, so never cached"
    );
    drop(held);
    assert!(
        rounds.held_by_a_known_forwarder(m, theirs, local).await,
        "closed, so a later connection from it is read"
    );
}
