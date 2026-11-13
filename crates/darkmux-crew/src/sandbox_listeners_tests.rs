use super::*;
use darkmux_platform::{FakePlatform, PortListener};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn at(addr: &str, pid: u32) -> PortListener {
    PortListener { addr: Some(addr.parse().unwrap()), text: addr.to_string(), scope: None, pid: Some(pid) }
}

/// A look interval short enough that a test waits milliseconds, not the
/// production second; nothing below depends on the wall clock otherwise.
const FAST: Duration = Duration::from_millis(5);

/// Starts a watch of leader 100 on `fake`, as Seatbelt's, and returns what
/// `stop` was called with (each call recorded) once the watch ends or
/// `within` passes.
fn watched(fake: FakePlatform, within: Duration) -> Vec<String> {
    let fake = Arc::new(fake);
    let stops = Arc::new(Mutex::new(Vec::new()));
    let watch = {
        let (fake, stops) = (Arc::clone(&fake), Arc::clone(&stops));
        ListenerWatch::start(FAST, move || look(&*fake, BackendKind::Seatbelt, "a dispatch", 100), move |why| stops.lock().unwrap().push(why))
    };
    let deadline = Instant::now() + within;
    while stops.lock().unwrap().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    drop(watch);
    let got = stops.lock().unwrap().clone();
    got
}

/// (#3559) A listener beyond the loopback stops the sandbox, once, naming
/// the address, the port and the pid holding it, and the fix.
#[test]
fn a_listener_beyond_the_loopback_stops_the_sandbox_naming_it() {
    let fake = FakePlatform { held_listeners: vec![(100, vec![(4100, at("127.0.0.1", 7)), (4321, at("0.0.0.0", 8))])], ..Default::default() };
    let stops = watched(fake, Duration::from_secs(5));
    assert_eq!(stops.len(), 1, "{stops:?}");
    for part in ["0.0.0.0:4321 (pid 8)", "A dispatch in the `seatbelt` sandbox", "#3559", "127.0.0.1"] {
        assert!(stops[0].contains(part), "{part} not in {}", stops[0]);
    }
    assert!(!stops[0].contains(":4100"), "a loopback listener named: {}", stops[0]);
}

/// (#3559) Listeners on the loopback only (a test server, a dev server for
/// a browser test) never stop it, look after look.
#[test]
fn loopback_listeners_never_stop_the_sandbox() {
    let fake = FakePlatform { held_listeners: vec![(100, vec![(4100, at("127.0.0.1", 7)), (4101, at("::1", 7))])], ..Default::default() };
    assert_eq!(watched(fake, Duration::from_millis(200)), Vec::<String>::new());
}

/// (#3559) Another sandbox's (another leader's) listener beyond the
/// loopback is not this sandbox's.
#[test]
fn another_leaders_listener_does_not_stop_this_sandbox() {
    let fake = FakePlatform { held_listeners: vec![(200, vec![(4321, at("0.0.0.0", 9))])], ..Default::default() };
    assert_eq!(watched(fake, Duration::from_millis(200)), Vec::<String>::new());
}

/// (#3559) A listing that cannot be read twice running stops the sandbox
/// (it cannot be vouched for), saying why and the way back to Docker; one
/// miss alone does not.
#[test]
fn an_unreadable_listing_twice_running_stops_the_sandbox() {
    let once = FakePlatform::default();
    once.held_listeners_unreadable_reads.store(1, Ordering::SeqCst);
    assert_eq!(watched(once, Duration::from_millis(200)), Vec::<String>::new());
    let twice = FakePlatform::default();
    twice.held_listeners_unreadable_reads.store(2, Ordering::SeqCst);
    let stops = watched(twice, Duration::from_secs(5));
    assert_eq!(stops.len(), 1, "{stops:?}");
    for part in ["cannot read", "seatbelt", "#3559", "docker"] {
        assert!(stops[0].contains(part), "{part} not in {}", stops[0]);
    }
}

/// (#3559) A watch ends when it is dropped, at once (it does not sleep out
/// its interval), and never calls `stop` after.
#[test]
fn a_dropped_watch_ends_at_once() {
    let looks = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let watch = {
        let looks = Arc::clone(&looks);
        ListenerWatch::start(Duration::from_secs(3600), move || {
            looks.fetch_add(1, Ordering::SeqCst);
            Look::Clear
        }, |_| panic!("no stop"))
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while looks.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    let dropped = Instant::now();
    drop(watch);
    assert!(dropped.elapsed() < Duration::from_secs(1), "{:?}", dropped.elapsed());
    assert_eq!(looks.load(Ordering::SeqCst), 1, "it looked once, then waited");
}

/// (#3559) A container is not watched: only its published loopback ports
/// reach it. A sandbox on the host is.
#[test]
fn only_a_sandbox_on_the_host_is_watched() {
    let handle = |backend| darkmux_sandbox::SandboxHandle { backend, id: "darkmux-dispatch-x".into() };
    assert!(watch(&handle(BackendKind::Docker), 1234, "a dispatch", |_| {}).is_none());
    assert!(watch(&handle(BackendKind::Seatbelt), std::process::id(), "a dispatch", |_| {}).is_some());
    assert!(watch(&handle(BackendKind::Landlock), std::process::id(), "a dispatch", |_| {}).is_some());
}

/// (#3559) One listener held by several of the sandbox's processes (a
/// server's workers share its socket) is named once, with its holders.
#[test]
fn a_shared_listener_is_named_once_with_its_holders() {
    let why = exposure_reason(BackendKind::Landlock, "a dispatch", &[(80, at("::", 3)), (80, at("::", 4)), (81, at("192.168.1.5", 4))]).unwrap();
    assert_eq!(why.matches("[::]:80").count(), 1, "{why}");
    assert!(why.contains("[::]:80 (pids 3, 4)") && why.contains("192.168.1.5:81 (pid 4)") && why.contains("landlock"), "{why}");
    assert_eq!(exposure_reason(BackendKind::Landlock, "a dispatch", &[(80, at("127.0.0.1", 3))]), None);
}
