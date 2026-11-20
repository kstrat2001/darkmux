use super::*;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::Path;

/// The viewer daemon's router serving on a real loopback listener, wired
/// the way `run` serves it; its address.
async fn viewer(flows: &Path) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    crate::record_bound_addr(Some(addr));
    let app = crate::build_router(flows.to_path_buf());
    tokio::spawn(async move { axum::serve(listener, make_service(app)).await });
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
