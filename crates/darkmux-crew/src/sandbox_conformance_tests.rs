//! (#3112) What every sandbox backend must do, written once and run against
//! each backend. Each case takes the backend and an image to run; the
//! `conformance!` lines at the end instantiate the suite per backend. The
//! cases start real sandboxes, so they are opt-in like the repo's other
//! real-container tests: `cargo nextest run -p darkmux-crew
//! sandbox_conformance --run-ignored only`.
//!
//! Runs are bounded the way production bounds them ([`run_bounded`]), and
//! a sandbox whose client was cut short is removed through the backend, the
//! way a contained run removes one.

use crate::bounded_command::{run_bounded, Bounded};
use darkmux_sandbox::{Access, Limits, Mount, Network, Query, SandboxBackend, SandboxHandle, SandboxPolicy};
use std::path::Path;
use std::time::{Duration, Instant};

/// The label every conformance sandbox carries, its value the sandbox's
/// name, so a listing finds only this suite's sandboxes.
const LABEL: &str = "darkmux.sandbox.conformance";

/// A second key, for a neighbor a listing by [`LABEL`] must not return.
const OTHER: &str = "darkmux.sandbox.conformance-other";

fn a_name() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("darkmux-conformance-{}-{n}", std::process::id())
}

/// `sh -c script` in `image`, no network, the tree read-write at
/// `/workspace`, labeled for this suite.
fn policy(image: &str, tree: &Path, script: &str) -> SandboxPolicy {
    let name = a_name();
    SandboxPolicy {
        labels: vec![(LABEL.to_string(), name.clone())],
        name,
        image: image.to_string(),
        network: Network::None,
        limits: Limits { memory: Some("256m".to_string()), pids: Some(64) },
        mounts: vec![Mount { host: tree.to_path_buf(), at: "/workspace".to_string(), access: Access::ReadWrite }],
        ports: Vec::new(),
        user: None,
        env: Vec::new(),
        workdir: Some("/workspace".to_string()),
        entrypoint: Some("sh".to_string()),
        command: vec!["-c".to_string(), script.to_string()],
    }
}

/// Removes its sandbox when dropped, so a failed assertion leaves nothing
/// running.
struct Removed<'a>(&'a dyn SandboxBackend, SandboxHandle);

impl Drop for Removed<'_> {
    fn drop(&mut self) {
        let _ = run_bounded(self.0.remove(&self.1), Duration::from_secs(30));
    }
}

/// Run `policy` to completion; its stdout on success, else a panic naming
/// what it said.
fn run_ok(backend: &dyn SandboxBackend, policy: &SandboxPolicy) -> String {
    let (cmd, handle) = backend.run(policy);
    let _removed = Removed(backend, handle);
    match run_bounded(cmd, Duration::from_secs(60)) {
        Bounded::Finished { success: true, stdout, .. } => String::from_utf8_lossy(&stdout).into_owned(),
        other => panic!("{} did not succeed: {}", policy.name, describe(&other)),
    }
}

fn describe(ran: &Bounded) -> String {
    match ran {
        Bounded::Finished { code, stdout, stderr, .. } => format!(
            "exit {code:?}, stdout {:?}, stderr {:?}",
            String::from_utf8_lossy(stdout),
            String::from_utf8_lossy(stderr)
        ),
        other => format!("{other:?}"),
    }
}

/// The values of `key` on the sandboxes the backend lists by it.
fn listed(backend: &dyn SandboxBackend, key: &str) -> Vec<String> {
    let query = Query::labeled(key);
    let Bounded::Finished { success: true, stdout, .. } = run_bounded(backend.list(&query), Duration::from_secs(30)) else {
        panic!("the backend could not list its sandboxes");
    };
    backend.parse_list(&query, &String::from_utf8_lossy(&stdout)).into_iter().filter_map(|l| l.labels[0].clone()).collect()
}

/// Wait up to 30 s for `name` to be listed by `key` or not, as `want` says.
fn until_listed_by(backend: &dyn SandboxBackend, key: &str, name: &str, want: bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let found = listed(backend, key).iter().any(|v| v == name);
        if found == want || Instant::now() > deadline {
            return found == want;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn until_listed(backend: &dyn SandboxBackend, name: &str, want: bool) -> bool {
    until_listed_by(backend, LABEL, name, want)
}

/// The length of `path`, 0 while it does not exist.
fn len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// The mounted tree is there, and the host path beside it, and the tree's
/// own host path, are not.
fn sees_the_mounted_tree_and_nothing_else(backend: &dyn SandboxBackend, image: &str) {
    let root = tempfile::TempDir::new().unwrap();
    let tree = root.path().join("tree");
    std::fs::create_dir(&tree).unwrap();
    std::fs::write(tree.join("a.txt"), "hello").unwrap();
    std::fs::write(root.path().join("secret.txt"), "keep out").unwrap();
    let script = format!(
        "cat /workspace/a.txt && test ! -e '{}' && test ! -e '{}'",
        root.path().join("secret.txt").display(),
        tree.join("a.txt").display()
    );
    assert_eq!(run_ok(backend, &policy(image, &tree, &script)), "hello");
}

/// A read-only mount refuses a write, alone or layered over a read-write
/// one (a checkout's `.git` over its tree), and the read-write tree beneath
/// still takes one.
fn read_only_mounts_refuse_writes(backend: &dyn SandboxBackend, image: &str) {
    let root = tempfile::TempDir::new().unwrap();
    let tree = root.path().join("tree");
    let ro = root.path().join("ro");
    std::fs::create_dir_all(tree.join(".git")).unwrap();
    std::fs::create_dir(&ro).unwrap();
    let mut p = policy(
        image,
        &tree,
        "touch /workspace/written && ! touch /workspace/.git/x 2>/dev/null && ! touch /ro/x 2>/dev/null && echo refused",
    );
    p.mounts.push(Mount { host: tree.join(".git"), at: "/workspace/.git".to_string(), access: Access::ReadOnly });
    p.mounts.push(Mount { host: ro.clone(), at: "/ro".to_string(), access: Access::ReadOnly });
    assert_eq!(run_ok(backend, &p).trim(), "refused");
    assert!(tree.join("written").is_file(), "the read-write tree took the write");
    assert!(!tree.join(".git/x").exists() && !ro.join("x").exists(), "a read-only mount took a write");
}

/// No network means no route anywhere: the routing table is empty (the
/// loopback's routes are not in it). The same probe on the default network
/// finds a route, so it can tell the two apart.
fn network_none_has_no_network(backend: &dyn SandboxBackend, image: &str) {
    let tree = tempfile::TempDir::new().unwrap();
    let probe = "tail -n +2 /proc/net/route";
    let none = run_ok(backend, &policy(image, tree.path(), probe));
    assert_eq!(none.trim(), "", "a sandbox with no network has a route");
    let default = run_ok(backend, &SandboxPolicy { network: Network::Default, ..policy(image, tree.path(), probe) });
    assert!(!default.trim().is_empty(), "the probe cannot see a route on the default network");
}

/// A run past its deadline is cut short, and removing its sandbox ends the
/// whole tree: a background child that appends to a file in the tree stops
/// appending, and nothing is left listed. It is listed while it runs, so
/// "not listed" is not a listing that sees nothing, and the child was seen
/// appending, so "stopped" is not a child that never ran.
fn a_deadline_kills_the_whole_tree(backend: &dyn SandboxBackend, image: &str) {
    let tree = tempfile::TempDir::new().unwrap();
    let beat = tree.path().join("beat");
    let p = policy(image, tree.path(), "(while :; do echo x >> /workspace/beat; sleep 0.2; done) & sleep 300 & wait");
    let (cmd, handle) = backend.run(&p);
    let removed = Removed(backend, handle);
    let bounded = std::thread::spawn(move || run_bounded(cmd, Duration::from_secs(8)));
    assert!(until_listed(backend, &p.name, true), "{} never listed while running", p.name);
    let ran = bounded.join().unwrap();
    assert!(matches!(ran, Bounded::TimedOut { .. }), "{}", describe(&ran));
    assert!(len(&beat) > 0, "the background child never ran");
    drop(removed);
    assert!(until_listed(backend, &p.name, false), "{} outlived its deadline", p.name);
    let after = len(&beat);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(len(&beat), after, "a background child outlived its sandbox's removal");
}

/// A sandbox whose client is gone (its owner killed) is still found by its
/// label, so a reaper can remove it, after which it is gone. A running
/// neighbor under another label is not in that listing, so a backend that
/// lists everything fails.
fn a_reaper_finds_an_orphan_by_its_label(backend: &dyn SandboxBackend, image: &str) {
    let tree = tempfile::TempDir::new().unwrap();
    let neighbor = policy(image, tree.path(), "sleep 300");
    let neighbor = SandboxPolicy { labels: vec![(OTHER.to_string(), neighbor.name.clone())], ..neighbor };
    let (mut cmd, handle) = backend.run(&neighbor);
    let _neighbor = Removed(backend, handle);
    let mut neighbor_client = cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
    assert!(until_listed_by(backend, OTHER, &neighbor.name, true), "{} never listed", neighbor.name);
    let p = policy(image, tree.path(), "sleep 300");
    let (mut cmd, handle) = backend.run(&p);
    let removed = Removed(backend, handle);
    let mut client = cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
    assert!(until_listed(backend, &p.name, true), "{} never listed", p.name);
    assert!(!listed(backend, LABEL).contains(&neighbor.name), "a listing by one label returned a sandbox without it");
    client.kill().unwrap();
    client.wait().unwrap();
    assert!(until_listed(backend, &p.name, true), "the orphan is found by its label after its client is gone");
    drop(removed);
    assert!(until_listed(backend, &p.name, false), "{} survived its removal", p.name);
    neighbor_client.kill().unwrap();
    neighbor_client.wait().unwrap();
}

/// A created sandbox is listed before it starts, so a stop from then on
/// finds it; starting it runs the policy attached, its output the client's.
fn a_created_sandbox_is_listed_then_runs_when_started(backend: &dyn SandboxBackend, image: &str) {
    let tree = tempfile::TempDir::new().unwrap();
    let p = policy(image, tree.path(), "echo started");
    let (create, handle) = backend.create(&p);
    let _removed = Removed(backend, handle.clone());
    let made = run_bounded(create, Duration::from_secs(60));
    assert!(matches!(made, Bounded::Finished { success: true, .. }), "{}", describe(&made));
    assert!(until_listed(backend, &p.name, true), "{} not listed once created", p.name);
    match run_bounded(backend.start(&handle), Duration::from_secs(60)) {
        Bounded::Finished { success: true, stdout, .. } => assert_eq!(String::from_utf8_lossy(&stdout).trim(), "started"),
        other => panic!("{} did not start: {}", p.name, describe(&other)),
    }
}

/// A stop asks first: the sandbox's own SIGTERM handler runs within the
/// grace, and the sandbox is gone after.
fn a_stop_lets_the_sandbox_end_itself(backend: &dyn SandboxBackend, image: &str) {
    let tree = tempfile::TempDir::new().unwrap();
    let p = policy(
        image,
        tree.path(),
        "trap 'echo bye > /workspace/bye; exit 0' TERM; echo up > /workspace/up; while :; do sleep 0.1; done",
    );
    let (create, handle) = backend.create(&p);
    let _removed = Removed(backend, handle.clone());
    assert!(matches!(run_bounded(create, Duration::from_secs(60)), Bounded::Finished { success: true, .. }));
    let start = backend.start(&handle);
    let attached = std::thread::spawn(move || run_bounded(start, Duration::from_secs(60)));
    let deadline = Instant::now() + Duration::from_secs(30);
    while !tree.path().join("up").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(tree.path().join("up").exists(), "{} never came up", p.name);
    let stopped = run_bounded(backend.stop(&handle, Duration::from_secs(5)), Duration::from_secs(30));
    assert!(matches!(stopped, Bounded::Finished { success: true, .. }), "{}", describe(&stopped));
    let _ = attached.join();
    assert_eq!(std::fs::read_to_string(tree.path().join("bye")).unwrap_or_default().trim(), "bye", "the TERM handler did not run");
    assert!(until_listed(backend, &p.name, false), "{} outlived its stop", p.name);
}

/// A published port answers on the host's loopback, at its own number. The
/// sandbox serves one line on it; the host reads that line back, so an
/// accepting proxy with nothing behind it does not pass.
fn a_published_port_answers_on_loopback(backend: &dyn SandboxBackend, image: &str) {
    use std::io::Read;
    let tree = tempfile::TempDir::new().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let script = format!("while :; do echo served | nc -l -p {port}; done");
    let p = SandboxPolicy { network: Network::Default, ports: vec![port], ..policy(image, tree.path(), &script) };
    let (mut cmd, handle) = backend.run(&p);
    let _removed = Removed(backend, handle);
    let mut client = cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut got = String::new();
    while got.trim() != "served" && Instant::now() < deadline {
        got.clear();
        if let Ok(mut conn) = std::net::TcpStream::connect(("127.0.0.1", port)) {
            let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = conn.read_to_string(&mut got);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = client.kill();
    let _ = client.wait();
    assert_eq!(got.trim(), "served", "nothing answered on 127.0.0.1:{port}");
}

macro_rules! conformance {
    ($backend:ident, $make:expr, $image:literal, $needs:literal) => {
        mod $backend {
            #[test]
            #[ignore = $needs]
            fn sees_the_mounted_tree_and_nothing_else() {
                super::sees_the_mounted_tree_and_nothing_else(&$make, $image)
            }
            #[test]
            #[ignore = $needs]
            fn read_only_mounts_refuse_writes() {
                super::read_only_mounts_refuse_writes(&$make, $image)
            }
            #[test]
            #[ignore = $needs]
            fn network_none_has_no_network() {
                super::network_none_has_no_network(&$make, $image)
            }
            #[test]
            #[ignore = $needs]
            fn a_deadline_kills_the_whole_tree() {
                super::a_deadline_kills_the_whole_tree(&$make, $image)
            }
            #[test]
            #[ignore = $needs]
            fn a_reaper_finds_an_orphan_by_its_label() {
                super::a_reaper_finds_an_orphan_by_its_label(&$make, $image)
            }
            #[test]
            #[ignore = $needs]
            fn a_created_sandbox_is_listed_then_runs_when_started() {
                super::a_created_sandbox_is_listed_then_runs_when_started(&$make, $image)
            }
            #[test]
            #[ignore = $needs]
            fn a_stop_lets_the_sandbox_end_itself() {
                super::a_stop_lets_the_sandbox_end_itself(&$make, $image)
            }
            #[test]
            #[ignore = $needs]
            fn a_published_port_answers_on_loopback() {
                super::a_published_port_answers_on_loopback(&$make, $image)
            }
        }
    };
}

conformance!(docker, darkmux_sandbox::DockerBackend, "alpine:3.22", "needs Docker and alpine:3.22");
