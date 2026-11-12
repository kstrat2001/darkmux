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
use darkmux_sandbox::{Access, Having, Limits, Mount, Network, Query, SandboxBackend, SandboxHandle, SandboxPolicy};
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

/// What a case needs to know about a backend's host beyond the trait: the
/// image it runs, and the shell lines that probe its network, which differ
/// between a Linux image and a macOS host.
pub struct Host {
    pub image: &'static str,
    /// Prints something when the sandbox has a route off this machine, and
    /// nothing when it has none.
    pub route_probe: &'static str,
    /// Serves one line, `served`, on `port` on the sandbox's loopback, over
    /// and over.
    pub serve: fn(u16) -> String,
    /// Prints `connected` when a TCP connection to the sandbox's own
    /// `127.0.0.1:port` succeeds.
    pub tcp_probe: fn(u16) -> String,
    /// Prints `connected` when a connection to the unix socket at the path
    /// succeeds.
    pub unix_probe: fn(&str) -> String,
    /// Serves one line, `served`, on `port` on EVERY address (`0.0.0.0`),
    /// over and over.
    pub serve_any: fn(u16) -> String,
    /// (#3559) Whether a server binding every address in this backend's
    /// default network is reachable at this host's LAN address: never in a
    /// container (only the published loopback port reaches it); in a
    /// sandbox on the host, recorded as allowed, since Seatbelt cannot tell a
    /// loopback bind from any other.
    pub every_address_reaches_the_lan: bool,
    /// Runs `script` in a process group of its own (`setsid` in a Linux
    /// image, `setpgrp` on macOS), in the background of the caller's shell.
    pub new_group: fn(&str) -> String,
    /// Whether a process that moved to a group of its own outlives the
    /// sandbox's program: never in a container (its init's exit ends every
    /// process in it); in a sandbox on the host, it does (the launcher can
    /// end only its own group), recorded, not wanted.
    pub a_new_group_outlives_the_program: bool,
    /// The host process group a running sandbox's processes are in, when
    /// they are host processes (a sandbox on the host): `None` for a
    /// container, whose processes are gone with it.
    pub host_group: fn(&str) -> Option<u32>,
}

/// How many live processes are in host process group `group`, from the
/// whole process table (a killed one not yet reaped, a zombie, runs nothing
/// and is not counted).
fn group_members(group: u32) -> usize {
    let ps = std::process::Command::new("/bin/ps").args(["-A", "-o", "pgid=,stat="]).output().unwrap();
    String::from_utf8_lossy(&ps.stdout)
        .lines()
        .filter(|l| {
            let mut f = l.split_whitespace();
            f.next() == Some(group.to_string().as_str()) && !f.next().is_some_and(|stat| stat.starts_with('Z'))
        })
        .count()
}

/// Whether host group `group` is empty within `within` (its guard runs a
/// moment after its launcher ends).
fn group_empties(group: u32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while group_members(group) > 0 {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

/// Where the sandbox sees `tree` when it is mounted at `/workspace`.
fn ws(backend: &dyn SandboxBackend, tree: &Path) -> String {
    backend.place(&Mount { host: tree.to_path_buf(), at: "/workspace".to_string(), access: Access::ReadWrite })
}

/// `sh -c script` in `image`, no network, the tree read-write at
/// `/workspace` (wherever the backend places it, its working directory),
/// labeled for this suite.
fn policy(backend: &dyn SandboxBackend, image: &str, tree: &Path, script: &str) -> SandboxPolicy {
    let name = a_name();
    SandboxPolicy {
        labels: vec![(LABEL.to_string(), name.clone())],
        name,
        image: image.to_string(),
        network: Network::None,
        limits: Limits { memory: Some("256m".to_string()), pids: Some(64) },
        mounts: vec![Mount { host: tree.to_path_buf(), at: "/workspace".to_string(), access: Access::ReadWrite }],
        ports: Vec::new(),
        stdin: false,
        host_alias: None,
        user: None,
        env: Vec::new(),
        workdir: Some(ws(backend, tree)),
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
fn sees_the_mounted_tree_and_nothing_else(backend: &dyn SandboxBackend, host: &Host) {
    let image = host.image;
    let root = tempfile::TempDir::new().unwrap();
    let tree = root.path().join("tree");
    std::fs::create_dir(&tree).unwrap();
    std::fs::write(tree.join("a.txt"), "hello").unwrap();
    std::fs::write(root.path().join("secret.txt"), "keep out").unwrap();
    let at = ws(backend, &tree);
    let mut script = format!("cat '{at}/a.txt' && test ! -e '{}'", root.path().join("secret.txt").display());
    // A backend that cannot place a mount sees the tree at its own path.
    if Path::new(&at) != tree.canonicalize().unwrap() {
        script.push_str(&format!(" && test ! -e '{}'", tree.join("a.txt").display()));
    }
    assert_eq!(run_ok(backend, &policy(backend, image, &tree, &script)), "hello");
}

/// A read-only mount refuses a write, alone or layered over a read-write
/// one (a checkout's `.git` over its tree), and the read-write tree beneath
/// still takes one.
fn read_only_mounts_refuse_writes(backend: &dyn SandboxBackend, host: &Host) {
    let image = host.image;
    let root = tempfile::TempDir::new().unwrap();
    let tree = root.path().join("tree");
    let ro = root.path().join("ro");
    std::fs::create_dir_all(tree.join(".git")).unwrap();
    std::fs::create_dir(&ro).unwrap();
    let git = Mount { host: tree.join(".git"), at: "/workspace/.git".to_string(), access: Access::ReadOnly };
    let ro_mount = Mount { host: ro.clone(), at: "/ro".to_string(), access: Access::ReadOnly };
    let (at_git, at_ro) = (backend.place(&git), backend.place(&ro_mount));
    let script = format!("touch written && ! touch '{at_git}/x' 2>/dev/null && ! touch '{at_ro}/x' 2>/dev/null && echo refused");
    let mut p = policy(backend, image, &tree, &script);
    p.mounts.push(git);
    p.mounts.push(ro_mount);
    assert_eq!(run_ok(backend, &p).trim(), "refused");
    assert!(tree.join("written").is_file(), "the read-write tree took the write");
    assert!(!tree.join(".git/x").exists() && !ro.join("x").exists(), "a read-only mount took a write");
}

/// No network means no route anywhere: the routing table is empty (the
/// loopback's routes are not in it). The same probe on the default network
/// finds a route, so it can tell the two apart.
fn network_none_has_no_network(backend: &dyn SandboxBackend, host: &Host) {
    let image = host.image;
    let tree = tempfile::TempDir::new().unwrap();
    let probe = host.route_probe;
    let none = run_ok(backend, &policy(backend, image, tree.path(), probe));
    assert_eq!(none.trim(), "", "a sandbox with no network has a route");
    let default = run_ok(backend, &SandboxPolicy { network: Network::Default, ..policy(backend, image, tree.path(), probe) });
    assert!(!default.trim().is_empty(), "the probe cannot see a route on the default network");
}

/// Nothing of the caller's environment reaches the sandbox: the test
/// process's own cargo variables are not there.
fn the_hosts_environment_does_not_leak_in(backend: &dyn SandboxBackend, host: &Host) {
    let tree = tempfile::TempDir::new().unwrap();
    assert!(std::env::var_os("CARGO_PKG_NAME").is_some(), "the probe needs a variable the caller has");
    let env = run_ok(backend, &policy(backend, host.image, tree.path(), "env"));
    assert!(!env.contains("CARGO_PKG_NAME="), "the caller's environment leaked in: {env}");
}

/// With no network, the host's own loopback services are out of reach: a
/// listener on this host's `127.0.0.1` gets no connection.
fn network_none_cannot_reach_the_hosts_loopback(backend: &dyn SandboxBackend, host: &Host) {
    let tree = tempfile::TempDir::new().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let said = run_ok(backend, &policy(backend, host.image, tree.path(), &(host.tcp_probe)(port)));
    assert!(!said.contains("connected"), "a sandbox with no network reached the host's loopback: {said}");
    assert!(listener.accept().is_err(), "the host's listener took a connection from a sandbox with no network");
}

/// On the default network, a unix socket outside the sandbox's mounts (the
/// Docker daemon's, a database's) is out of reach.
fn the_default_network_reaches_no_outside_unix_socket(backend: &dyn SandboxBackend, host: &Host) {
    let tree = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    let path = outside.path().join("s.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let p = SandboxPolicy { network: Network::Default, ..policy(backend, host.image, tree.path(), &(host.unix_probe)(&path.display().to_string())) };
    let said = run_ok(backend, &p);
    assert!(!said.contains("connected"), "a sandbox reached a unix socket outside its mounts: {said}");
    assert!(listener.accept().is_err(), "the outside socket took a connection from a sandbox");
}

/// A read-only mount nested two levels into a read-write one stays read
/// only even when the sandbox renames the directory above it.
fn a_deep_read_only_mount_survives_renaming_its_parent(backend: &dyn SandboxBackend, host: &Host) {
    let tree = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(tree.path().join("sub/.git")).unwrap();
    std::fs::write(tree.path().join("sub/.git/config"), "kept\n").unwrap();
    let git = Mount { host: tree.path().join("sub/.git"), at: "/workspace/sub/.git".to_string(), access: Access::ReadOnly };
    let script = "mv sub sub2 2>/dev/null; echo pwn >> sub2/.git/config 2>/dev/null; echo pwn >> sub/.git/config 2>/dev/null; mv sub2 sub 2>/dev/null; true";
    let mut p = policy(backend, host.image, tree.path(), script);
    p.mounts.push(git);
    run_ok(backend, &p);
    assert_eq!(std::fs::read_to_string(tree.path().join("sub/.git/config")).unwrap(), "kept\n", "a read-only mount took a write");
}

/// A run past its deadline is cut short, and removing its sandbox ends the
/// whole tree: a background child that appends to a file in the tree stops
/// appending, and nothing is left listed. It is listed while it runs, so
/// "not listed" is not a listing that sees nothing, and the child was seen
/// appending, so "stopped" is not a child that never ran.
fn a_deadline_kills_the_whole_tree(backend: &dyn SandboxBackend, host: &Host) {
    let image = host.image;
    let tree = tempfile::TempDir::new().unwrap();
    let beat = tree.path().join("beat");
    let p = policy(backend, image, tree.path(), "(while :; do echo x >> beat; sleep 0.2; done) & sleep 300 & wait");
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
fn a_reaper_finds_an_orphan_by_its_label(backend: &dyn SandboxBackend, host: &Host) {
    let image = host.image;
    let tree = tempfile::TempDir::new().unwrap();
    let neighbor = policy(backend, image, tree.path(), "sleep 300");
    let neighbor = SandboxPolicy { labels: vec![(OTHER.to_string(), neighbor.name.clone())], ..neighbor };
    let (mut cmd, handle) = backend.run(&neighbor);
    let _neighbor = Removed(backend, handle);
    let mut neighbor_client = cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
    assert!(until_listed_by(backend, OTHER, &neighbor.name, true), "{} never listed", neighbor.name);
    let p = policy(backend, image, tree.path(), "sleep 300");
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
fn a_created_sandbox_is_listed_then_runs_when_started(backend: &dyn SandboxBackend, host: &Host) {
    let image = host.image;
    let tree = tempfile::TempDir::new().unwrap();
    let p = policy(backend, image, tree.path(), "echo started");
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
fn a_stop_lets_the_sandbox_end_itself(backend: &dyn SandboxBackend, host: &Host) {
    let image = host.image;
    let tree = tempfile::TempDir::new().unwrap();
    let p = policy(
        backend,
        image,
        tree.path(),
        "trap 'echo bye > bye; exit 0' TERM; echo up > up; while :; do sleep 0.1; done",
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

/// A published port answers on the host's loopback, at its own number, and
/// not on the host's outward address. The sandbox serves one line on it;
/// the host reads that line back, so an accepting proxy with nothing behind
/// it does not pass.
fn a_published_port_answers_on_loopback(backend: &dyn SandboxBackend, host: &Host) {
    let image = host.image;
    use std::io::Read;
    let tree = tempfile::TempDir::new().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let script = (host.serve)(port);
    let p = SandboxPolicy { network: Network::Default, ports: vec![port], ..policy(backend, image, tree.path(), &script) };
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
    assert_eq!(got.trim(), "served", "nothing answered on 127.0.0.1:{port}");
    if let Some(lan) = this_hosts_lan_address() {
        let mut other = String::new();
        if let Ok(mut conn) = std::net::TcpStream::connect_timeout(&(lan, port).into(), Duration::from_secs(2)) {
            let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = conn.read_to_string(&mut other);
        }
        assert_ne!(other.trim(), "served", "the port answered on {lan}, not only on loopback");
    }
    let _ = client.kill();
    let _ = client.wait();
}

/// (#3559) A server that binds every address: what the host reaches.
/// Docker: the published port on 127.0.0.1, never the host's LAN address.
/// Seatbelt: the bind succeeds and the LAN address reaches it (recorded,
/// not wanted: SBPL's `local ip` filter knows only `*` and `localhost`, and
/// `localhost` matches every local address), so the gap stays visible.
/// Landlock: the same, recorded too (it holds a bind to ports, not to an
/// address).
fn a_server_on_every_address_reaches_only_what_the_backend_allows(backend: &dyn SandboxBackend, host: &Host) {
    let tree = tempfile::TempDir::new().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let p = SandboxPolicy { network: Network::Default, ports: vec![port], ..policy(backend, host.image, tree.path(), &(host.serve_any)(port)) };
    let (mut cmd, handle) = backend.run(&p);
    let _removed = Removed(backend, handle);
    let mut client = cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
    let read_line = |ip: std::net::IpAddr| -> String {
        use std::io::Read;
        let mut got = String::new();
        if let Ok(mut conn) = std::net::TcpStream::connect_timeout(&(ip, port).into(), Duration::from_secs(2)) {
            let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = conn.read_to_string(&mut got);
        }
        got.trim().to_string()
    };
    // The server serves one connection at a time and restarts: keep asking
    // until it answers or the time is up.
    let served_within = |ip: std::net::IpAddr, within: Duration| {
        let deadline = Instant::now() + within;
        loop {
            if read_line(ip) == "served" {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    assert!(served_within(std::net::IpAddr::from([127, 0, 0, 1]), Duration::from_secs(30)), "nothing answered on 127.0.0.1:{port}");
    if let Some(lan) = this_hosts_lan_address() {
        let reached = served_within(lan, Duration::from_secs(3));
        assert_eq!(reached, host.every_address_reaches_the_lan, "a server on every address, reached at {lan}:{port}");
    }
    let _ = client.kill();
    let _ = client.wait();
}

/// A sandbox ends with its program, as a container ends with its init's
/// child: a background process the program left behind is gone once the
/// program exits, so it neither writes again nor keeps a port; so is one
/// that keeps forking a new pid and exiting the old (a fork-hopper, which a
/// single snapshot of the group misses). A process that moved to a process
/// group of its own is gone in a container and outlives a sandbox on the
/// host: what each backend does is asserted, not hoped.
fn a_sandbox_ends_with_its_program(backend: &dyn SandboxBackend, host: &Host) {
    let tree = tempfile::TempDir::new().unwrap();
    let grew = |name: &str| -> bool {
        let before = len(&tree.path().join(name));
        std::thread::sleep(Duration::from_millis(1200));
        len(&tree.path().join(name)) > before
    };
    // Eight at once, so one snapshot of the group almost surely misses one.
    let hopper = "for n in 1 2 3 4 5 6 7 8; do (h() { echo x >> hops; (h) & exit 0; }; h) & done;";
    let grouped = (host.new_group)("echo $$ > group.pid; while :; do echo x >> group; sleep 0.1; done");
    let script = format!("(sleep 2; echo outlived > left) & {hopper} {grouped} sleep 0.5; echo started");
    let p = policy(backend, host.image, tree.path(), &script);
    assert_eq!(run_ok(backend, &p).trim(), "started");
    std::thread::sleep(Duration::from_secs(3));
    let (left, hops, group) = (tree.path().join("left").exists(), grew("hops"), grew("group"));
    if let Ok(pid) = std::fs::read_to_string(tree.path().join("group.pid")) {
        if host.a_new_group_outlives_the_program {
            let _ = std::process::Command::new("kill").args(["-KILL", pid.trim()]).status();
        }
    }
    assert!(len(&tree.path().join("hops")) > 0 && len(&tree.path().join("group")) > 0, "the children never ran");
    assert!(!left, "a background process outlived its sandbox's program");
    assert!(!hops, "a fork-hopper outlived its sandbox's program");
    assert_eq!(group, host.a_new_group_outlives_the_program, "a process in a group of its own, after the program ended");
}

/// A sandbox whose client was killed outright (SIGKILL: no handler runs)
/// still ends, its background processes with it, once it is removed, as a
/// contained run removes one cut short. Nothing outlives it looping.
fn a_sandbox_whose_client_was_killed_ends_on_removal(backend: &dyn SandboxBackend, host: &Host) {
    let tree = tempfile::TempDir::new().unwrap();
    let beat = tree.path().join("beat");
    let p = policy(backend, host.image, tree.path(), "(while :; do echo x >> beat; sleep 0.1; done) & sleep 300");
    let (mut cmd, handle) = backend.run(&p);
    let removed = Removed(backend, handle);
    let mut client = cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while len(&beat) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(len(&beat) > 0, "the background loop never ran");
    let group = (host.host_group)(&p.name);
    client.kill().unwrap();
    client.wait().unwrap();
    drop(removed);
    std::thread::sleep(Duration::from_millis(500));
    let after = len(&beat);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(len(&beat), after, "a background loop outlived its sandbox's removal after its client was killed");
    if let Some(group) = group {
        assert!(group_empties(group, Duration::from_secs(2)), "processes left in group {group}");
    }
}

/// Removing a running sandbox ends fork-hoppers in it (eight at once), not
/// only the processes one look at it would list.
fn a_removal_ends_fork_hoppers(backend: &dyn SandboxBackend, host: &Host) {
    // Three rounds: a drain that is only sometimes enough fails one of them.
    for _ in 0..3 {
        a_removal_ends_fork_hoppers_once(backend, host);
    }
}

fn a_removal_ends_fork_hoppers_once(backend: &dyn SandboxBackend, host: &Host) {
    let tree = tempfile::TempDir::new().unwrap();
    let hops = tree.path().join("hops");
    // Thirty-two at once: a single kill of the group, or two, leaves one
    // that forked in between often enough to see.
    let script = "n=0; while [ $n -lt 32 ]; do (h() { echo x >> hops; (h) & exit 0; }; h) & n=$((n + 1)); done; sleep 300";
    let p = policy(backend, host.image, tree.path(), script);
    let (mut cmd, handle) = backend.run(&p);
    let mut client = cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while len(&hops) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(len(&hops) > 0, "the hoppers never ran");
    let group = (host.host_group)(&p.name);
    let removed = run_bounded(backend.remove(&handle), Duration::from_secs(30));
    assert!(matches!(removed, Bounded::Finished { success: true, .. }), "{}", describe(&removed));
    // The removal itself empties the group: checked as soon as it returns.
    if let Some(group) = group {
        assert_eq!(group_members(group), 0, "the removal returned with processes left in its group {group}");
    }
    let _ = client.kill();
    let _ = client.wait();
    std::thread::sleep(Duration::from_millis(500));
    let after = len(&hops);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(len(&hops), after, "a fork-hopper outlived its sandbox's removal");
}

/// A server on a port the policy does not name is never reached from the
/// host: a container publishes only the named ports, and a sandbox on the
/// host may bind only those (any address, #3559, but no other port). The
/// named port answers, so a probe that reaches nothing is not passing.
fn a_server_on_an_unassigned_port_is_never_reached(backend: &dyn SandboxBackend, host: &Host) {
    let tree = tempfile::TempDir::new().unwrap();
    let free = || std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let (named, other) = (free(), free());
    let script = format!("({}) & {}", (host.serve_any)(other), (host.serve_any)(named));
    let p = SandboxPolicy { network: Network::Default, ports: vec![named], ..policy(backend, host.image, tree.path(), &script) };
    let (mut cmd, handle) = backend.run(&p);
    let _removed = Removed(backend, handle);
    let mut client = cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
    let served = |ip: std::net::IpAddr, port: u16, within: Duration| {
        use std::io::Read;
        let deadline = Instant::now() + within;
        loop {
            let mut got = String::new();
            if let Ok(mut conn) = std::net::TcpStream::connect_timeout(&(ip, port).into(), Duration::from_secs(1)) {
                let _ = conn.set_read_timeout(Some(Duration::from_secs(1)));
                let _ = conn.read_to_string(&mut got);
            }
            if got.trim() == "served" {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    let loopback = std::net::IpAddr::from([127, 0, 0, 1]);
    assert!(served(loopback, named, Duration::from_secs(30)), "the named port never answered");
    assert!(!served(loopback, other, Duration::from_secs(2)), "a port the policy does not name answered on the loopback");
    if let Some(lan) = this_hosts_lan_address() {
        assert!(!served(lan, other, Duration::from_secs(2)), "a port the policy does not name answered at {lan}");
    }
    let _ = client.kill();
    let _ = client.wait();
}

/// This host's address on its outward interface, when it has one: the
/// address a connected UDP socket would send from (nothing is sent).
fn this_hosts_lan_address() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

/// A kill's success is proof the sandbox was running: it fails for one that
/// was never made, succeeds for a running one, and that one is gone after.
fn a_kill_succeeds_only_against_a_running_sandbox(backend: &dyn SandboxBackend, host: &Host) {
    let image = host.image;
    let tree = tempfile::TempDir::new().unwrap();
    let never = SandboxHandle { backend: backend.kind(), id: a_name() };
    let ghost = run_bounded(backend.kill(&never), Duration::from_secs(30));
    assert!(!matches!(ghost, Bounded::Finished { success: true, .. }), "killing a sandbox never made succeeded");
    let p = policy(backend, image, tree.path(), "sleep 300");
    let (mut cmd, handle) = backend.run(&p);
    let _removed = Removed(backend, handle.clone());
    let mut client = cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
    assert!(until_listed(backend, &p.name, true), "{} never listed", p.name);
    let killed = run_bounded(backend.kill(&handle), Duration::from_secs(30));
    assert!(matches!(killed, Bounded::Finished { success: true, .. }), "{}", describe(&killed));
    let _ = client.wait();
    assert!(until_listed(backend, &p.name, false), "{} outlived its kill", p.name);
}

/// A stop ends a sandbox that ignores SIGTERM once the grace is up: it is
/// gone well before the sleep it was running would have ended.
fn a_stop_ends_a_sandbox_that_ignores_it_after_the_grace(backend: &dyn SandboxBackend, host: &Host) {
    let image = host.image;
    let tree = tempfile::TempDir::new().unwrap();
    let p = policy(backend, image, tree.path(), "trap '' TERM; echo up > up; while :; do sleep 0.1; done");
    let (mut cmd, handle) = backend.run(&p);
    let _removed = Removed(backend, handle.clone());
    let mut client = cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !tree.path().join("up").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(tree.path().join("up").exists(), "{} never came up", p.name);
    let asked = Instant::now();
    let _ = run_bounded(backend.stop(&handle, Duration::from_secs(2)), Duration::from_secs(30));
    assert!(until_listed(backend, &p.name, false), "{} outlived its stop", p.name);
    assert!(asked.elapsed() < Duration::from_secs(20), "the stop took {:?}", asked.elapsed());
    let _ = client.wait();
}

/// A created sandbox that is removed before it starts never runs: a later
/// start fails, and nothing it would have written appears.
fn a_sandbox_removed_before_it_starts_never_runs(backend: &dyn SandboxBackend, host: &Host) {
    let image = host.image;
    let tree = tempfile::TempDir::new().unwrap();
    let p = policy(backend, image, tree.path(), "echo ran > ran");
    let (create, handle) = backend.create(&p);
    let _removed = Removed(backend, handle.clone());
    assert!(matches!(run_bounded(create, Duration::from_secs(60)), Bounded::Finished { success: true, .. }));
    assert!(until_listed(backend, &p.name, true), "{} not listed once created", p.name);
    let _ = run_bounded(backend.remove(&handle), Duration::from_secs(30));
    let started = run_bounded(backend.start(&handle), Duration::from_secs(30));
    assert!(!matches!(started, Bounded::Finished { success: true, .. }), "a removed sandbox started: {}", describe(&started));
    assert!(!tree.path().join("ran").exists(), "a removed sandbox ran");
}

/// A listing by name is literal: `<base>.a` finds that sandbox and not a
/// neighbor `<base>Xa`, by exact name and by prefix alike (Docker's name
/// filter is a regular expression, where `.` is any character; #3552
/// review). The watchdog's probe and the reapers list this way.
fn a_listing_by_name_matches_the_name_literally(backend: &dyn SandboxBackend, host: &Host) {
    let image = host.image;
    let tree = tempfile::TempDir::new().unwrap();
    let base = a_name();
    let mut created = Vec::new();
    for name in [format!("{base}.a"), format!("{base}Xa")] {
        let mut p = policy(backend, image, tree.path(), "true");
        p.name = name;
        let (create, handle) = backend.create(&p);
        created.push(Removed(backend, handle));
        let made = run_bounded(create, Duration::from_secs(60));
        assert!(matches!(made, Bounded::Finished { success: true, .. }), "{}", describe(&made));
    }
    let names = |having: Having| -> Vec<String> {
        let query = Query { having, stopped_too: true, read: Vec::new() };
        let Bounded::Finished { success: true, stdout, .. } = run_bounded(backend.list(&query), Duration::from_secs(30)) else {
            panic!("the backend could not list its sandboxes");
        };
        backend.parse_list(&query, &String::from_utf8_lossy(&stdout)).into_iter().map(|l| l.handle.id).collect()
    };
    assert_eq!(names(Having::Name(format!("{base}.a"))), vec![format!("{base}.a")], "by exact name");
    assert_eq!(names(Having::NamePrefix(format!("{base}."))), vec![format!("{base}.a")], "by prefix");
}

macro_rules! conformance {
    ($backend:ident, $make:expr, $host:expr, $needs:literal) => {
        mod $backend {
            #[test]
            #[ignore = $needs]
            fn sees_the_mounted_tree_and_nothing_else() {
                super::sees_the_mounted_tree_and_nothing_else(&$make, &$host)
            }
            #[test]
            #[ignore = $needs]
            fn read_only_mounts_refuse_writes() {
                super::read_only_mounts_refuse_writes(&$make, &$host)
            }
            #[test]
            #[ignore = $needs]
            fn network_none_has_no_network() {
                super::network_none_has_no_network(&$make, &$host)
            }
            #[test]
            #[ignore = $needs]
            fn a_deadline_kills_the_whole_tree() {
                super::a_deadline_kills_the_whole_tree(&$make, &$host)
            }
            #[test]
            #[ignore = $needs]
            fn a_reaper_finds_an_orphan_by_its_label() {
                super::a_reaper_finds_an_orphan_by_its_label(&$make, &$host)
            }
            #[test]
            #[ignore = $needs]
            fn a_kill_succeeds_only_against_a_running_sandbox() {
                super::a_kill_succeeds_only_against_a_running_sandbox(&$make, &$host)
            }
            #[test]
            #[ignore = $needs]
            fn a_stop_ends_a_sandbox_that_ignores_it_after_the_grace() {
                super::a_stop_ends_a_sandbox_that_ignores_it_after_the_grace(&$make, &$host)
            }
            #[test]
            #[ignore = $needs]
            fn a_sandbox_removed_before_it_starts_never_runs() {
                super::a_sandbox_removed_before_it_starts_never_runs(&$make, &$host)
            }
            #[test]
            #[ignore = $needs]
            fn a_created_sandbox_is_listed_then_runs_when_started() {
                super::a_created_sandbox_is_listed_then_runs_when_started(&$make, &$host)
            }
            #[test]
            #[ignore = $needs]
            fn a_stop_lets_the_sandbox_end_itself() {
                super::a_stop_lets_the_sandbox_end_itself(&$make, &$host)
            }
            #[test]
            #[ignore = $needs]
            fn a_published_port_answers_on_loopback() {
                super::a_published_port_answers_on_loopback(&$make, &$host)
            }
            #[test]
            #[ignore = $needs]
            fn the_hosts_environment_does_not_leak_in() {
                super::the_hosts_environment_does_not_leak_in(&$make, &$host)
            }
            #[test]
            #[ignore = $needs]
            fn network_none_cannot_reach_the_hosts_loopback() {
                super::network_none_cannot_reach_the_hosts_loopback(&$make, &$host)
            }
            #[test]
            #[ignore = $needs]
            fn the_default_network_reaches_no_outside_unix_socket() {
                super::the_default_network_reaches_no_outside_unix_socket(&$make, &$host)
            }
            #[test]
            #[ignore = $needs]
            fn a_deep_read_only_mount_survives_renaming_its_parent() {
                super::a_deep_read_only_mount_survives_renaming_its_parent(&$make, &$host)
            }
            #[test]
            #[ignore = $needs]
            fn a_server_on_every_address_reaches_only_what_the_backend_allows() {
                super::a_server_on_every_address_reaches_only_what_the_backend_allows(&$make, &$host)
            }
            #[test]
            #[ignore = $needs]
            fn a_sandbox_ends_with_its_program() {
                super::a_sandbox_ends_with_its_program(&$make, &$host)
            }
            #[test]
            #[ignore = $needs]
            fn a_server_on_an_unassigned_port_is_never_reached() {
                super::a_server_on_an_unassigned_port_is_never_reached(&$make, &$host)
            }
            #[test]
            #[ignore = $needs]
            fn a_sandbox_whose_client_was_killed_ends_on_removal() {
                super::a_sandbox_whose_client_was_killed_ends_on_removal(&$make, &$host)
            }
            #[test]
            #[ignore = $needs]
            fn a_removal_ends_fork_hoppers() {
                super::a_removal_ends_fork_hoppers(&$make, &$host)
            }
            #[test]
            #[ignore = $needs]
            fn a_listing_by_name_matches_the_name_literally() {
                super::a_listing_by_name_matches_the_name_literally(&$make, &$host)
            }
        }
    };
}

conformance!(
    docker,
    darkmux_sandbox::DockerBackend,
    super::Host {
        image: "alpine:3.22",
        route_probe: "tail -n +2 /proc/net/route",
        // A listen that fails (the port refused or taken) waits before it
        // tries again, never spinning.
        serve: |port| format!("while :; do echo served | nc -l -p {port} || sleep 1; done"),
        tcp_probe: |port| format!("nc -w 2 127.0.0.1 {port} </dev/null >/dev/null 2>&1 && echo connected; true"),
        unix_probe: |path| format!("nc -U '{path}' </dev/null >/dev/null 2>&1 && echo connected; true"),
        serve_any: |port| format!("while :; do echo served | nc -l -p {port} || sleep 1; done"),
        every_address_reaches_the_lan: false,
        new_group: |script| format!("setsid sh -c '{script}' &"),
        a_new_group_outlives_the_program: false,
        host_group: |_| None,
    },
    "needs Docker and alpine:3.22"
);

// (#3112 step 2, spike) macOS only: there is no image, and the probes are
// the host's. A route off the machine is a connection to a public resolver.
conformance!(
    seatbelt,
    darkmux_sandbox::SeatbeltBackend,
    super::Host {
        image: "unused",
        route_probe: "nc -z -G 2 1.1.1.1 53 2>/dev/null && echo route; true",
        serve: |port| format!("while :; do echo served | nc -l 127.0.0.1 {port} || sleep 1; done"),
        tcp_probe: |port| format!("nc -z -G 2 127.0.0.1 {port} >/dev/null 2>&1 && echo connected; true"),
        unix_probe: |path| format!("nc -U '{path}' </dev/null >/dev/null 2>&1 && echo connected; true"),
        serve_any: |port| format!("while :; do echo served | nc -l {port} || sleep 1; done"),
        // #3559: recorded, not wanted.
        every_address_reaches_the_lan: true,
        new_group: |script| format!("/usr/bin/perl -e 'setpgrp(0, 0); exec @ARGV' sh -c '{script}' &"),
        // Recorded, not wanted: the launcher ends only its own group.
        a_new_group_outlives_the_program: true,
        host_group: |name| {
            let pid = std::fs::read_to_string(darkmux_sandbox::seatbelt::state_root().join(name).join("pid")).ok()?;
            pid.trim().parse().ok()
        },
    },
    "needs macOS sandbox-exec"
);

// (#3112 step 2) Linux only: there is no image, and the probes are the
// host's own Python (no netcat is assumed). This test binary is the
// sandbox's helper, as `darkmux` is in production, so it hands its
// arguments to the helper before `main`.
darkmux_platform::confine_before_main!(darkmux_sandbox::landlock::helper_if_asked);

conformance!(
    landlock,
    darkmux_sandbox::LandlockBackend,
    super::Host {
        image: "unused",
        route_probe: "tail -n +2 /proc/net/route",
        serve: |port| format!(
            "while :; do python3 -c \"import socket; s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); s.bind(('127.0.0.1', {port})); s.listen(1); c, _ = s.accept(); c.sendall(b'served\\n'); c.close()\"; done"
        ),
        tcp_probe: |port| format!("python3 -c \"import socket; socket.create_connection(('127.0.0.1', {port}), 2)\" 2>/dev/null && echo connected; true"),
        unix_probe: |path| format!("python3 -c \"import socket; socket.socket(socket.AF_UNIX).connect('{path}')\" 2>/dev/null && echo connected; true"),
        serve_any: |port| format!(
            "while :; do python3 -c \"import socket; s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); s.bind(('0.0.0.0', {port})); s.listen(1); c, _ = s.accept(); c.sendall(b'served\\n'); c.close()\"; done"
        ),
        // Recorded, not wanted: the default network is the host's, and
        // Landlock holds a bind to the named ports but not to an address.
        every_address_reaches_the_lan: true,
        new_group: |script| format!("setsid sh -c '{script}' &"),
        // A pid namespace of its own: its init's exit ends every process in
        // it, as a container's does.
        a_new_group_outlives_the_program: false,
        host_group: |_| None,
    },
    "needs Linux with Landlock ABI 6 and unprivileged user namespaces"
);

/// (#3112 step 2) What the Landlock backend enforces beyond the shared
/// suite: Docker and Seatbelt differ here (Seatbelt cannot do several), so
/// these run against Landlock alone.
mod landlock_only {
    use super::*;
    use darkmux_sandbox::LandlockBackend;

    fn run_script(network: Network, script: &str) -> String {
        let tree = tempfile::TempDir::new().unwrap();
        run_ok(&LandlockBackend, &SandboxPolicy { network, ..policy(&LandlockBackend, "unused", tree.path(), script) })
    }

    /// No new privileges, ever: a setuid program gains nothing.
    #[test]
    #[ignore = "needs Linux with Landlock ABI 6 and unprivileged user namespaces"]
    fn the_sandbox_cannot_gain_privileges() {
        assert_eq!(run_script(Network::None, "grep NoNewPrivs /proc/self/status").split_whitespace().last(), Some("1"));
    }

    /// Another process's arguments (this test's own) cannot be read: the
    /// sandbox's `/proc` holds only its own processes.
    #[test]
    #[ignore = "needs Linux with Landlock ABI 6 and unprivileged user namespaces"]
    fn another_processs_arguments_cannot_be_read() {
        let me = std::process::id();
        let said = run_script(Network::Default, &format!("cat /proc/{me}/cmdline 2>/dev/null; ls /proc | grep -c '^[0-9]'"));
        assert!(!said.contains("sandbox_conformance"), "the sandbox read this test's arguments: {said}");
        let pids: u32 = said.trim().lines().last().unwrap_or("").parse().unwrap_or(u32::MAX);
        assert!(pids < 10, "the sandbox sees {pids} processes, not only its own");
    }

    /// `/etc`'s secrets (this machine's identifier among them) are not
    /// there; the rest of `/etc` is.
    #[test]
    #[ignore = "needs Linux with Landlock ABI 6 and unprivileged user namespaces"]
    fn etcs_secrets_are_not_there() {
        assert!(Path::new("/etc/machine-id").exists(), "the probe needs a host with /etc/machine-id");
        let said = run_script(Network::None, "test -e /etc/machine-id && echo id; test -e /etc/shadow && echo shadow; test -e /etc/passwd && echo passwd; true");
        assert_eq!(said.trim(), "passwd");
    }

    /// A descendant that leaves the process group (`setsid`) still ends
    /// with the sandbox: its init's exit ends the whole pid namespace.
    #[test]
    #[ignore = "needs Linux with Landlock ABI 6 and unprivileged user namespaces"]
    fn a_descendant_that_left_the_group_ends_with_the_sandbox() {
        let tree = tempfile::TempDir::new().unwrap();
        let beat = tree.path().join("beat");
        let p = policy(&LandlockBackend, "unused", tree.path(), "setsid sh -c 'while :; do echo x >> beat; sleep 0.2; done' & sleep 300");
        let (mut cmd, handle) = LandlockBackend.run(&p);
        let removed = Removed(&LandlockBackend, handle);
        let mut client = cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while len(&beat) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(len(&beat) > 0, "the escaping child never ran");
        drop(removed);
        let _ = client.wait();
        std::thread::sleep(Duration::from_millis(500));
        let after = len(&beat);
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(len(&beat), after, "a child that left the group outlived its sandbox's removal");
    }

    /// A stop's grace is the sandbox's: a TERM handler that takes a second
    /// to finish still finishes, because nothing between the launcher and
    /// the program ends with the TERM and takes the program down early.
    #[test]
    #[ignore = "needs Linux with Landlock ABI 6 and unprivileged user namespaces"]
    fn a_slow_term_handler_finishes_within_the_grace() {
        let tree = tempfile::TempDir::new().unwrap();
        let p = policy(&LandlockBackend, "unused", tree.path(), "trap 'sleep 1; echo bye > bye; exit 0' TERM; echo up > up; while :; do sleep 0.1; done");
        let (create, handle) = LandlockBackend.create(&p);
        let _removed = Removed(&LandlockBackend, handle.clone());
        assert!(matches!(run_bounded(create, Duration::from_secs(60)), Bounded::Finished { success: true, .. }));
        let start = LandlockBackend.start(&handle);
        let attached = std::thread::spawn(move || run_bounded(start, Duration::from_secs(60)));
        let deadline = Instant::now() + Duration::from_secs(30);
        while !tree.path().join("up").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        let stopped = run_bounded(LandlockBackend.stop(&handle, Duration::from_secs(5)), Duration::from_secs(30));
        assert!(matches!(stopped, Bounded::Finished { success: true, .. }), "{}", describe(&stopped));
        let _ = attached.join();
        assert_eq!(std::fs::read_to_string(tree.path().join("bye")).unwrap_or_default().trim(), "bye", "the TERM handler was cut short");
    }

    /// With no network the sandbox still has a loopback of its own: it can
    /// serve and reach itself there.
    #[test]
    #[ignore = "needs Linux with Landlock ABI 6 and unprivileged user namespaces"]
    fn no_network_keeps_a_loopback_of_its_own() {
        let script = "python3 -c \"import socket; s = socket.socket(); s.bind(('127.0.0.1', 0)); s.listen(1); socket.create_connection(s.getsockname(), 2); print('self')\"";
        assert_eq!(run_script(Network::None, script).trim(), "self");
    }

    /// On the default network, an abstract unix socket outside the sandbox
    /// (a desktop session's, which no file view can hide) is out of reach.
    #[test]
    #[ignore = "needs Linux with Landlock ABI 6 and unprivileged user namespaces"]
    fn the_default_network_reaches_no_outside_abstract_socket() {
        use std::io::{BufRead, Read};
        // The outside listener is a host python3 (abstract names are a Linux
        // API this file does not name): it says `ready`, then whether a
        // connection came within 3 s.
        let name = format!("darkmux-conformance-{}", std::process::id());
        let listen = format!(
            "import socket; s = socket.socket(socket.AF_UNIX); s.bind(b'\\0{name}'); s.listen(1); s.settimeout(3); print('ready', flush=True)\n\
             try:\n    s.accept(); print('accepted')\nexcept OSError:\n    print('none')"
        );
        let mut outside = std::process::Command::new("python3").arg("-c").arg(listen).stdout(std::process::Stdio::piped()).spawn().unwrap();
        let mut out = std::io::BufReader::new(outside.stdout.take().unwrap());
        let mut ready = String::new();
        out.read_line(&mut ready).unwrap();
        assert_eq!(ready.trim(), "ready");
        let script = format!("python3 -c \"import socket; socket.socket(socket.AF_UNIX).connect(b'\\\\0{name}')\" 2>/dev/null && echo connected; true");
        let said = run_script(Network::Default, &script);
        assert!(!said.contains("connected"), "a sandbox reached an abstract socket outside it: {said}");
        let mut verdict = String::new();
        out.read_to_string(&mut verdict).unwrap();
        outside.wait().unwrap();
        assert_eq!(verdict.trim(), "none", "the outside abstract socket took a connection from a sandbox");
    }

    /// On the default network a sandbox binds only the ports its policy
    /// publishes.
    #[test]
    #[ignore = "needs Linux with Landlock ABI 6 and unprivileged user namespaces"]
    fn the_default_network_binds_only_published_ports() {
        let free = || std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let (published, other) = (free(), free());
        let bind = |port: u16| format!("python3 -c \"import socket; socket.socket().bind(('127.0.0.1', {port}))\" 2>/dev/null && echo bound-{port}; ");
        let tree = tempfile::TempDir::new().unwrap();
        let script = format!("{}{}true", bind(published), bind(other));
        let p = SandboxPolicy { network: Network::Default, ports: vec![published], ..policy(&LandlockBackend, "unused", tree.path(), &script) };
        assert_eq!(run_ok(&LandlockBackend, &p).trim(), format!("bound-{published}"));
    }
}
