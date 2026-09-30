//! (#2916 stage 2) Two daemons, one machine: `darkmux dispatch --profile
//! <profile>@<machine>` from one node reaches the OTHER node's fleet
//! listener and runs there, on the receiver's own profile.
//!
//! A defect between two services needs a test that runs both. The unit tests
//! drive each half with the other mocked; this one spawns two real `darkmux
//! serve` daemons, each with its own `DARKMUX_HOME`, and sends a real
//! dispatch from a real CLI process:
//!
//! - `alpha` sends; its roster places `beta` at 127.0.0.1.
//! - `beta` runs a fleet listener on 127.0.0.1 and trusts `alpha` for the
//!   `cloud` profile and the `radio-host` role.
//! - Each node's `fleet.identity.bin` is a fake identity tool (a shell
//!   script answering `status --json` / `whois --json`) that names the
//!   OTHER node as the one at 127.0.0.1, which is what the real overlay
//!   network's tool answers across two machines.
//! - `beta`'s `cloud` profile is a hosted endpoint pointed at a mock chat
//!   server in this process, and `radio-host` has no tools, so the
//!   receiver runs the real dispatch path without Docker or a model.
//!
//! The same two daemons also prove the fleet view and the machine card end to
//! end: `alpha`'s `GET /fleet/view` shows `beta`'s real card (fetched by a real
//! HTTP request from one daemon to the other), a peer with no `/machine/card`
//! route reads "card unavailable", slow peers are asked in parallel, and
//! `beta`'s `GET /machine/card` carries the caller-scoped `accepts` block only
//! for a request holding the fleet token from an allow-listed node.
//!
//! Needs a binary built with the test-only `e2e-fleet-loopback` feature (a
//! production listener refuses loopback): [`bin`] builds one. The target
//! declares `required-features`, so it runs only when asked for:
//!
//! ```sh
//! cargo nextest run -p darkmux --features e2e-fleet-loopback --test fleet_profile_address_two_daemons
//! ```

#![cfg(unix)]

#[path = "e2e/mod.rs"]
mod e2e;

use e2e::fixture_reaper;

use darkmux_flow::FlowAction;
use darkmux_types::session_id::{SessionId, SessionKind};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TOKEN: &str = "e2e-fleet-token-2916";
const MOCK_REPLY: &str = "the beta seat answered";


/// The binary under test: a plain `cargo build` of `darkmux` WITH the
/// `e2e-fleet-loopback` feature, in its own target directory.
///
/// Not `CARGO_BIN_EXE_darkmux`: cargo builds that one with this crate's
/// dev-dependency features, and `darkmux-types/test-support` makes every
/// config read return an empty config, so a daemon built that way ignores
/// the `config.json` this test writes. Its own target directory, so it
/// never replaces the `target/debug` or `target/release` binary another
/// test drives. `DARKMUX_E2E_LOOPBACK_BIN` names a prebuilt one (CI).
fn bin() -> PathBuf {
    static BIN: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BIN.get_or_init(|| {
        if let Some(p) = std::env::var_os("DARKMUX_E2E_LOOPBACK_BIN") {
            return PathBuf::from(p);
        }
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"))
            .join("e2e-fleet-loopback");
        let status = Command::new(env!("CARGO"))
            .args(["build", "--bin", "darkmux", "--features", "e2e-fleet-loopback", "--target-dir"])
            .arg(&target)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stdout(Stdio::null())
            .status()
            .expect("running cargo build");
        assert!(status.success(), "building darkmux with --features e2e-fleet-loopback failed");
        target.join("debug").join("darkmux")
    })
    .clone()
}

/// A port nothing holds right now (bind 0, read, release).
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// A minimal OpenAI-compatible chat server: every POST gets one completion
/// after `delay_ms`. Counts the requests it served.
struct MockChat {
    port: u16,
    delay_ms: Arc<AtomicU64>,
    served: Arc<AtomicUsize>,
}

impl MockChat {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let delay_ms = Arc::new(AtomicU64::new(0));
        let served = Arc::new(AtomicUsize::new(0));
        let (d, n) = (delay_ms.clone(), served.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let (d, n) = (d.clone(), n.clone());
                std::thread::spawn(move || {
                    let _ = read_request(&mut s);
                    std::thread::sleep(Duration::from_millis(d.load(Ordering::SeqCst)));
                    let body = serde_json::json!({
                        "id": "mock-1", "object": "chat.completion", "created": 0, "model": "mock-model",
                        "choices": [{"index": 0, "message": {"role": "assistant", "content": MOCK_REPLY}, "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
                    })
                    .to_string();
                    let _ = write!(
                        s,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    n.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        Self { port, delay_ms, served }
    }
}

/// Read one HTTP request (headers, then Content-Length bytes).
fn read_request(s: &mut TcpStream) -> String {
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut got = Vec::new();
    let mut b = [0u8; 65536];
    loop {
        let n = s.read(&mut b).unwrap_or(0);
        if n == 0 {
            return String::from_utf8_lossy(&got).to_string();
        }
        got.extend_from_slice(&b[..n]);
        let text = String::from_utf8_lossy(&got).to_string();
        if let Some(h) = text.find("\r\n\r\n") {
            let len = text[..h]
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                .unwrap_or(0);
            if got.len() >= h + 4 + len {
                return text;
            }
        }
    }
}

/// A fake identity tool: `status --json` names `me`, and every address is
/// `other` for `whois --json`. Both at 127.0.0.1.
fn write_identity_tool(dir: &Path, me: (&str, &str), other: (&str, &str)) -> PathBuf {
    let node = |id: &str, name: &str| {
        serde_json::json!({"ID": id, "DNSName": format!("{name}.fleet.test."), "HostName": name, "TailscaleIPs": ["127.0.0.1"], "Online": true})
    };
    let status = serde_json::json!({"BackendState": "Running", "Self": node(me.0, me.1), "Peer": {"p": node(other.0, other.1)}});
    let whois = serde_json::json!({"Node": {"StableID": other.0, "Name": format!("{}.fleet.test.", other.1), "Addresses": ["127.0.0.1/32"]}});
    std::fs::write(dir.join("status.json"), status.to_string()).unwrap();
    std::fs::write(dir.join("whois.json"), whois.to_string()).unwrap();
    let tool = dir.join("identity-tool");
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in\n  status) cat '{}' ;;\n  whois) cat '{}' ;;\n  *) echo \"unknown: $1\" >&2; exit 2 ;;\nesac\n",
        dir.join("status.json").display(),
        dir.join("whois.json").display()
    );
    std::fs::write(&tool, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Run it once now: macOS scans a freshly written executable on its first
    // run, which can outlast the provider's 3 s bound and send the listener
    // into its 30 s retry.
    let out = Command::new(&tool).args(["status", "--json"]).output().unwrap();
    assert!(out.status.success(), "the fake identity tool does not run");
    tool
}

struct Node {
    name: &'static str,
    home: PathBuf,
    process_home: PathBuf,
    flows: PathBuf,
    fleet_file: PathBuf,
}

impl Node {
    fn new(root: &Path, name: &'static str) -> Self {
        let dir = root.join(name);
        let (home, process_home, flows) = (dir.join("home"), dir.join("process-home"), dir.join("flows"));
        for d in [&home, &process_home, &flows] {
            std::fs::create_dir_all(d).unwrap();
        }
        Self { name, home, process_home, flows, fleet_file: dir.join("fleet.json") }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(bin());
        darkmux_types::test_isolation::neutralize_state_vars(&mut cmd);
        cmd.env("HOME", &self.process_home)
            .env("DARKMUX_HOME", &self.home)
            .env("DARKMUX_MACHINE_ID", self.name)
            .env("DARKMUX_FLOWS_DIR", &self.flows)
            .env("DARKMUX_FLEET_FILE", &self.fleet_file)
            .env("DARKMUX_PROFILES", self.home.join("profiles.json"))
            .env("DARKMUX_SERVE_TOKEN", TOKEN)
            .env_remove("DARKMUX_REDIS_URL");
        cmd
    }

    fn flow_lines(&self) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(&self.flows).into_iter().flatten().flatten() {
            if e.path().extension().is_some_and(|x| x == "jsonl") {
                for l in std::fs::read_to_string(e.path()).unwrap_or_default().lines() {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
                        out.push(v);
                    }
                }
            }
        }
        out
    }
}

/// Both daemons, killed on drop; the fixture group reaps them even when the
/// test process is killed hard.
struct Fleet {
    alpha: Node,
    beta: Node,
    /// The viewer ports, by node.
    alpha_port: u16,
    beta_port: u16,
    daemons: Vec<Child>,
    group: fixture_reaper::FixtureGroup,
    mock: MockChat,
    _root: tempfile::TempDir,
}

impl Drop for Fleet {
    fn drop(&mut self) {
        for d in &mut self.daemons {
            let _ = d.kill();
            let _ = d.wait();
        }
        self.group.stand_down();
    }
}

fn wait_for_port(port: u16, what: &str, log: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(200)).is_err() {
        assert!(
            Instant::now() < deadline,
            "{what} never listened on {port}; daemon log:\n{}",
            std::fs::read_to_string(log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn boot(busy_policy: &str) -> Fleet {
    boot_with(busy_policy, &[])
}

/// [`boot`], with `extra_roster` (id, address) entries also in alpha's roster.
fn boot_with(busy_policy: &str, extra_roster: &[(&str, String)]) -> Fleet {
    let root = tempfile::tempdir().unwrap();
    let (alpha, beta) = (Node::new(root.path(), "alpha"), Node::new(root.path(), "beta"));
    let mock = MockChat::spawn();
    let fleet_port = free_port();
    let (alpha_port, beta_port) = (free_port(), free_port());

    // alpha: sender. Its roster places beta at 127.0.0.1; its identity tool
    // says that address is beta's node.
    let alpha_tool = write_identity_tool(&alpha.home, ("nALPHA", "alpha"), ("nBETA", "beta"));
    std::fs::write(
        alpha.home.join("config.json"),
        serde_json::json!({
            "schema_version": darkmux_types::config::CONFIG_SCHEMA_VERSION,
            "machine_id": "alpha",
            "fleet": {"identity": {"provider": "tailscale", "bin": alpha_tool}, "listener": {"enabled": false, "port": fleet_port}}
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(alpha.home.join("profiles.json"), r#"{"profiles":{}}"#).unwrap();
    // beta's entry names its viewer port too: alpha reads beta's card there.
    // (Work submission drops the port and uses the fleet listener's.)
    let mut machines = serde_json::Map::new();
    let mut add = |id: &str, address: String| {
        machines.insert(id.to_string(), serde_json::json!({"id": id, "address": address, "added_unix_ms": 1, "loopback_intended": true}));
    };
    add("beta", format!("127.0.0.1:{beta_port}"));
    for (id, address) in extra_roster {
        add(id, address.clone());
    }
    std::fs::write(&alpha.fleet_file, serde_json::json!({"version": "2", "machines": machines}).to_string()).unwrap();

    // beta: receiver. Trusts alpha's node for `cloud` + `radio-host`; its
    // `cloud` profile is a hosted endpoint on the mock chat server.
    let beta_tool = write_identity_tool(&beta.home, ("nBETA", "beta"), ("nALPHA", "alpha"));
    std::fs::write(
        beta.home.join("config.json"),
        serde_json::json!({
            "schema_version": darkmux_types::config::CONFIG_SCHEMA_VERSION,
            "machine_id": "beta",
            "fleet": {
                "identity": {"provider": "tailscale", "bin": beta_tool},
                "listener": {"enabled": true, "port": fleet_port},
                "accept_work": {"alpha": {"node_id": "nALPHA", "profiles": ["cloud"], "roles": ["radio-host"], "workspace": false}},
                "busy_policy": busy_policy
            },
            "remote": {"concurrent_cap": 1},
            "power": {"refuse_start_below_min": false, "pause_running_below_min": false}
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        beta.home.join("profiles.json"),
        serde_json::json!({
            "profiles": {
                "cloud": {"models": [{"id": "mock-model", "n_ctx": 32000, "endpoint": "mock"}]}
            },
            "endpoints": {"mock": {"url": format!("http://127.0.0.1:{}/v1", mock.port)}},
            "default_profile": "cloud"
        })
        .to_string(),
    )
    .unwrap();

    let mut group = fixture_reaper::FixtureGroup::arm();
    let mut daemons = Vec::new();
    for (node, port) in [(&alpha, alpha_port), (&beta, beta_port)] {
        let log = node.home.join("daemon.log");
        let mut cmd = node.cmd();
        cmd.args(["serve", "--bind", "127.0.0.1", "--port", &port.to_string()])
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap());
        group.place(&mut cmd);
        let child = cmd.spawn().expect("spawning darkmux serve");
        group.register(&child).unwrap();
        daemons.push(child);
        wait_for_port(port, &format!("{}'s viewer", node.name), &log);
    }
    let fleet = Fleet { alpha, beta, alpha_port, beta_port, daemons, group, mock, _root: root };
    wait_for_port(fleet_port, "beta's fleet listener", &fleet.beta.home.join("daemon.log"));
    fleet
}

fn dispatch(from: &Node, profile: &str, extra: &[&str]) -> Output {
    let mut cmd = from.cmd();
    cmd.args(["dispatch", "radio-host", "--profile", profile]).args(extra).arg("what is running?");
    cmd.stdin(Stdio::null());
    cmd.output().expect("running darkmux dispatch")
}

fn text(o: &Output) -> String {
    format!("exit={:?}\nstdout:\n{}\nstderr:\n{}", o.status.code(), String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

fn actions(lines: &[serde_json::Value]) -> Vec<String> {
    lines.iter().filter_map(|v| v["action"].as_str().map(str::to_string)).collect()
}

/// The whole path, both processes, and each side's records.
#[test]
fn an_addressed_dispatch_runs_on_the_owning_machine_and_each_side_records_its_half() {
    let f = boot("refuse");

    let out = dispatch(&f.alpha, "cloud@beta", &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains(MOCK_REPLY), "beta's answer reached alpha: {}", text(&out));
    assert_eq!(f.mock.served.load(Ordering::SeqCst), 1, "beta called its hosted endpoint once");

    // beta ran it under its own session id, with dispatch liveness bookends.
    let beta = f.beta.flow_lines();
    let starts: Vec<_> = beta
        .iter()
        .filter(|v| v["action"] == FlowAction::DispatchStart.as_str() && relayed_from_alpha(v))
        .collect();
    assert_eq!(starts.len(), 1, "beta's dispatch start: {:?}", actions(&beta));
    let sid = starts[0]["session_id"].as_str().unwrap();
    assert!(
        beta.iter().any(|v| v["session_id"] == sid && v["action"] == FlowAction::DispatchComplete.as_str()),
        "beta's dispatch complete for {sid}: {:?}",
        actions(&beta)
    );

    // alpha recorded where it sent the work, and no tokens: the machine that
    // ran the model counts them (#2916 decision 6). The mock reports usage,
    // so beta has a usage record for its session and alpha has none.
    let alpha = f.alpha.flow_lines();
    let route = alpha.iter().find(|v| v["action"] == FlowAction::DispatchRoute.as_str()).unwrap_or_else(|| panic!("{:?}", actions(&alpha)));
    assert_eq!(route["payload"]["profile_address"], "cloud@beta", "{route}");
    assert_eq!(route["payload"]["target_machine"], "beta", "{route}");
    assert!(
        beta.iter().any(|v| v["action"] == FlowAction::TelemetryTokens.as_str() && v["session_id"] == sid),
        "the receiver counted the tokens of the model it ran: {:?}",
        actions(&beta)
    );
    assert!(
        !alpha.iter().any(|v| v["action"] == FlowAction::TelemetryTokens.as_str()),
        "the sender recorded token usage for work another machine ran: {:?}",
        actions(&alpha)
    );
}

/// Refusals come back from the receiver at once, in its own words.
#[test]
fn the_receiver_refuses_at_once_and_the_sender_shows_why() {
    let f = boot("refuse");

    // A profile beta does not define: refused by name, never beta's default.
    let started = Instant::now();
    let out = dispatch(&f.alpha, "nope@beta", &[]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("profile nope is not defined on beta"), "{}", text(&out));
    assert!(started.elapsed() < Duration::from_secs(20), "a refusal is answered at once");
    assert_eq!(f.mock.served.load(Ordering::SeqCst), 0, "nothing ran on beta");
    assert_eq!(beta_dispatch_starts(&f), 0, "beta started a dispatch for a refused job");

    // Busy on the hosted seat (beta's remote.concurrent_cap is 1).
    f.mock.delay_ms.store(4_000, Ordering::SeqCst);
    let first = dispatch(&f.alpha, "cloud@beta", &["--no-wait"]);
    assert!(first.status.success(), "{}", text(&first));
    let second = dispatch(&f.alpha, "cloud@beta", &[]);
    assert!(!second.status.success(), "{}", text(&second));
    let why = text(&second);
    assert!(why.contains("busy: beta") && why.contains("remote.concurrent_cap"), "{why}");
    // Only the first job ever started on beta: the busy one never did.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(beta_dispatch_starts(&f), 1, "beta started a dispatch for the refused busy job");
}

/// `dispatch start` records beta wrote for jobs alpha sent.
/// Whether a record on beta is under beta's relay of a session alpha sent.
fn relayed_from_alpha(v: &serde_json::Value) -> bool {
    v["session_id"]
        .as_str()
        .and_then(|s| SessionId::parse(s).ok())
        .is_some_and(|s| matches!(s.kind(), SessionKind::Relay { peer, .. } if peer == "alpha"))
}

fn beta_dispatch_starts(f: &Fleet) -> usize {
    f.beta
        .flow_lines()
        .iter()
        .filter(|v| v["action"] == FlowAction::DispatchStart.as_str() && relayed_from_alpha(v))
        .count()
}

/// With `fleet.busy_policy = queue` the second job waits for the seat, the
/// sender is told so while it waits, and then gets its result.
#[test]
fn a_queued_job_is_announced_then_runs() {
    let f = boot("queue");
    f.mock.delay_ms.store(3_000, Ordering::SeqCst);
    let first = dispatch(&f.alpha, "cloud@beta", &["--no-wait"]);
    assert!(first.status.success(), "{}", text(&first));
    let second = dispatch(&f.alpha, "cloud@beta", &[]);
    assert!(second.status.success(), "{}", text(&second));
    let t = text(&second);
    assert!(t.contains("the job is queued"), "the sender heard it was queued: {t}");
    assert!(String::from_utf8_lossy(&second.stdout).contains(MOCK_REPLY), "{t}");
    assert_eq!(f.mock.served.load(Ordering::SeqCst), 2);
}


// ── the fleet view and the machine card ────────────────────────────────

/// GET `path` from a daemon's viewer port, as a loopback caller with the
/// given extra headers: the status and the JSON body.
fn http_get(port: u16, path: &str, headers: &[(&str, &str)]) -> (u16, serde_json::Value) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").expect("an HTTP response");
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap_or(serde_json::Value::Null))
}

/// A darkmux of an older version: `/health` answers, every other path (the
/// card route included) is a 404 after `delay_ms`.
fn old_peer(delay_ms: u64) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            std::thread::spawn(move || {
                let req = read_request(&mut s);
                let (status, body) = if req.starts_with("GET /health") {
                    ("200 OK", r#"{"darkmux_version":"4.9.1"}"#)
                } else {
                    std::thread::sleep(Duration::from_millis(delay_ms));
                    ("404 Not Found", "{}")
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    port
}

fn row<'a>(view: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    view["machines"].as_array().and_then(|rows| rows.iter().find(|r| r["entry"]["id"] == id)).unwrap_or_else(|| panic!("no {id} row in {view}"))
}

/// The promise: alpha's view of beta is beta's OWN card, fetched over HTTP.
#[test]
fn a_daemons_fleet_view_shows_the_other_daemons_real_card() {
    let f = boot("refuse");
    let (status, view) = http_get(f.alpha_port, "/fleet/view", &[]);
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["local_machine_id"], "alpha");
    let beta = row(&view, "beta");
    assert_eq!(beta["card"]["state"], "available", "{beta}");
    let card = &beta["card"]["card"];
    assert_eq!(card["specs"]["machine_id"], "beta", "the card is beta's own, not alpha's: {card}");
    let cloud = card["profiles"].as_array().unwrap().iter().find(|p| p["name"] == "cloud").expect("beta's cloud profile");
    assert_eq!(cloud["endpoint_kind"], "unmanaged", "beta's hosted profile: {cloud}");
    assert_eq!(card["default_profile"], "cloud");
    assert_eq!(card["seats"]["busy_policy"], "refuse", "beta's listener is running: {card}");
    assert_eq!(card["seats"]["hosted"]["cap"], 1);
    assert!(card.get("accepts").is_none(), "alpha sent no fleet token to a loopback peer: {card}");
    assert_eq!(view["cache_ttl_ms"], 5000);
}

/// Seats are live: a job running on beta's hosted seat shows as held in
/// beta's card.
#[test]
fn a_running_job_shows_as_a_held_seat_in_the_peers_card() {
    let f = boot("refuse");
    f.mock.delay_ms.store(4_000, Ordering::SeqCst);
    let first = dispatch(&f.alpha, "cloud@beta", &["--no-wait"]);
    assert!(first.status.success(), "{}", text(&first));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, card) = http_get(f.beta_port, "/machine/card", &[]);
        if card["seats"]["hosted"]["held"] == 1 {
            assert_eq!(card["seats"]["hosted"]["free"], 0);
            break;
        }
        assert!(Instant::now() < deadline, "beta never showed the held seat: {card}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// `accepts` is beta's allow-list entry for the caller, and only with the
/// fleet token from a node the network names (the fake tool names 127.0.0.1
/// as alpha).
#[test]
fn the_card_carries_accepts_only_for_the_token_holding_allow_listed_caller() {
    let f = boot("refuse");
    let (_, plain) = http_get(f.beta_port, "/machine/card", &[]);
    assert!(plain.get("accepts").is_none(), "no token: {plain}");
    let (_, wrong) = http_get(f.beta_port, "/machine/card", &[("Authorization", "Bearer nope")]);
    assert!(wrong.get("accepts").is_none(), "a wrong token: {wrong}");
    let auth = format!("Bearer {TOKEN}");
    let (_, card) = http_get(f.beta_port, "/machine/card", &[("Authorization", &auth)]);
    assert_eq!(card["accepts"]["peer_name"], "alpha", "{card}");
    assert_eq!(card["accepts"]["profiles"], serde_json::json!(["cloud"]));
    assert_eq!(card["accepts"]["roles"], serde_json::json!(["radio-host"]));
    assert_eq!(card["accepts"]["workspace"], false);
}

/// A peer on an older darkmux shows as "card unavailable", and two slow
/// peers are asked at the same time, not one after the other.
#[test]
fn an_old_peer_is_card_unavailable_and_slow_peers_are_asked_in_parallel() {
    let (slow1, slow2) = (old_peer(1_200), old_peer(1_200));
    let f = boot_with("refuse", &[("slow1", format!("127.0.0.1:{slow1}")), ("slow2", format!("127.0.0.1:{slow2}"))]);
    let started = Instant::now();
    let (status, view) = http_get(f.alpha_port, "/fleet/view", &[]);
    let took = started.elapsed();
    assert_eq!(status, 200, "{view}");
    for id in ["slow1", "slow2"] {
        let r = row(&view, id);
        assert_eq!(r["card"]["state"], "unavailable", "{r}");
        assert_eq!(r["card"]["peer_version"], "4.9.1", "the version its /health reports: {r}");
    }
    assert_eq!(row(&view, "beta")["card"]["state"], "available");
    assert!(took < Duration::from_millis(2_300), "two 1.2 s peers took {took:?}: asked one after the other");
}
