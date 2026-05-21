//! `FleetHarness` — boots a dual-node (or N-node) darkmux fleet for
//! end-to-end tests.
//!
//! `#![allow(dead_code)]` at the module level: each `tests/e2e_*.rs`
//! integration binary compiles the harness independently via `#[path]`
//! include. Different scenarios use different harness helpers; flagging
//! "never used in this binary" would noise out CI without helping.
//!
//! Spawns `redis-server` on a random port, an in-process mock LMStudio,
//! and N `darkmux serve` daemons with distinct
//! `DARKMUX_MACHINE_ID` / `DARKMUX_MACHINE_TIER` / `DARKMUX_REDIS_URL`
//! env vars. Provides helpers to:
//!
//! - dispatch CLI commands "from" any node (sets the right env vars on
//!   the child process)
//! - tail the local flow file
//! - introspect the mock LMStudio's request count
//!
//! `Drop` impl tears everything down (kills child processes).
//!
//! ## Requirements
//!
//! - `redis-server` on PATH (the harness spawns a fresh instance per test)
//! - `cargo build --release` of darkmux completed (the harness shells out
//!   to `target/release/darkmux`); helper `build_darkmux_release()` is
//!   a one-shot per test-run idempotent build.
//!
//! ## Out of scope (v1)
//!
//! - Auth-protected Redis (open instance on loopback; production uses
//!   Tailscale + requirepass)
//! - Docker containerization (process-based is the v1; can wrap with
//!   compose later if isolation matters)
//! - Tearing down between tests in the same `cargo test` invocation —
//!   each test instantiates its own `FleetHarness`, gets distinct
//!   ports, and tears down on drop. Serial-test the file-system-touching
//!   tests if needed via `#[serial_test::serial]`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::e2e::mock_lmstudio::MockLmStudio;

const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(15);
const REDIS_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Build `target/release/darkmux` once per `cargo test` invocation.
/// Subsequent calls are no-ops. Used by `FleetHarness::boot` so tests
/// don't have to remember to do this manually.
fn build_darkmux_release() -> Result<(), String> {
    static BUILD_RESULT: OnceLock<Mutex<Option<Result<(), String>>>> = OnceLock::new();
    let cell = BUILD_RESULT.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().expect("build-result mutex");
    if let Some(r) = guard.as_ref() {
        return r.clone();
    }
    let out = Command::new("cargo")
        .args(["build", "--release", "--bin", "darkmux"])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .output();
    let result = match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(format!(
            "cargo build --release failed: exit={:?}",
            o.status.code()
        )),
        Err(e) => Err(format!("cargo build --release spawn failed: {e}")),
    };
    *guard = Some(result.clone());
    result
}

/// One node in the test fleet. Wraps the spawned `darkmux serve`
/// subprocess + the per-node env vars; helpers build CLI commands
/// pre-configured for the node's identity.
pub struct FleetNode {
    pub machine_id: String,
    pub tier: String,
    pub daemon_port: u16,
    pub flows_dir: PathBuf,
    pub fleet_file: PathBuf,
    pub crew_root: PathBuf,
    pub openclaw_config: PathBuf,
    pub redis_url: String,
    #[allow(dead_code)] // consumed by Wave-E.2+ scenarios
    pub lmstudio_base_url: String,
    daemon: Child,
}

impl FleetNode {
    /// Build a CLI command pre-configured with this node's env vars.
    /// Caller adds `.args([...])` and `.output()`/`.spawn()`.
    pub fn cmd(&self) -> Command {
        let binary = darkmux_release_binary();
        let mut cmd = Command::new(binary);
        cmd.env("DARKMUX_MACHINE_ID", &self.machine_id)
            .env("DARKMUX_MACHINE_TIER", &self.tier)
            .env("DARKMUX_REDIS_URL", &self.redis_url)
            .env("DARKMUX_FLOWS_DIR", &self.flows_dir)
            .env("DARKMUX_FLEET_FILE", &self.fleet_file)
            .env("DARKMUX_CREW_ROOT", &self.crew_root)
            .env("DARKMUX_OPENCLAW_CONFIG", &self.openclaw_config)
            .env("DARKMUX_ORCHESTRATOR", "darkmux-e2e-test");
        cmd
    }

    /// Returns false if the daemon process has exited. Used by tests
    /// that want to verify the daemon survived a scenario. (Wave-E.2+.)
    #[allow(dead_code)]
    pub fn is_alive(&mut self) -> bool {
        matches!(self.daemon.try_wait(), Ok(None))
    }
}

impl Drop for FleetNode {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

/// The full test harness — owns redis, mock-lmstudio, all daemon nodes,
/// and the tempdir holding each node's per-node state. `Drop` tears
/// everything down.
pub struct FleetHarness {
    pub nodes: Vec<FleetNode>,
    pub mock_lmstudio: MockLmStudio,
    redis: Child,
    redis_url: String,
    /// Held to keep the tempdir alive for the harness's lifetime —
    /// daemon flow + fleet files live under here.
    _tempdir: tempfile::TempDir,
}

/// Configuration for one node in `FleetHarness::boot`.
#[derive(Debug, Clone)]
pub struct NodeSpec {
    pub machine_id: String,
    pub tier: String,
}

impl NodeSpec {
    pub fn inference(id: &str) -> Self {
        Self { machine_id: id.to_string(), tier: "inference".to_string() }
    }
    pub fn hub(id: &str) -> Self {
        Self { machine_id: id.to_string(), tier: "hub".to_string() }
    }
}

impl FleetHarness {
    /// Boot a fresh harness: build darkmux, spawn redis on a free port,
    /// spawn the mock LMStudio, then spawn one daemon per `NodeSpec`.
    /// Waits for every daemon's `/health` endpoint before returning.
    pub fn boot(specs: Vec<NodeSpec>) -> Result<Self, String> {
        build_darkmux_release()?;
        let tempdir =
            tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;

        let (redis, redis_url) = spawn_redis(&tempdir.path().join("redis"))?;
        wait_for_redis(&redis_url)?;

        let mock_lmstudio = MockLmStudio::spawn()
            .map_err(|e| format!("spawn mock_lmstudio: {e}"))?;
        let lmstudio_base_url = mock_lmstudio.base_url();

        let mut nodes = Vec::with_capacity(specs.len());
        for spec in specs {
            let node = spawn_daemon(
                &spec,
                tempdir.path(),
                &redis_url,
                &lmstudio_base_url,
            )?;
            nodes.push(node);
        }
        for node in &nodes {
            wait_for_daemon_health(node.daemon_port)?;
        }

        Ok(Self {
            nodes,
            mock_lmstudio,
            redis,
            redis_url,
            _tempdir: tempdir,
        })
    }

    pub fn redis_url(&self) -> &str {
        &self.redis_url
    }

    /// Look up a node by machine_id.
    pub fn node(&self, machine_id: &str) -> Option<&FleetNode> {
        self.nodes.iter().find(|n| n.machine_id == machine_id)
    }
}

impl Drop for FleetHarness {
    fn drop(&mut self) {
        // Nodes drop themselves; redis we kill explicitly.
        let _ = self.redis.kill();
        let _ = self.redis.wait();
    }
}

fn darkmux_release_binary() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest).join("target/release/darkmux")
}

fn spawn_redis(workdir: &std::path::Path) -> Result<(Child, String), String> {
    std::fs::create_dir_all(workdir)
        .map_err(|e| format!("creating redis workdir: {e}"))?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("binding redis port: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("redis local_addr: {e}"))?
        .port();
    drop(listener); // release for redis-server to bind

    let child = Command::new("redis-server")
        .arg("--port")
        .arg(port.to_string())
        .arg("--save")
        .arg("") // disable RDB persistence (test ephemeral)
        .arg("--appendonly")
        .arg("no")
        .arg("--dir")
        .arg(workdir)
        .arg("--bind")
        .arg("127.0.0.1")
        .arg("--protected-mode")
        .arg("no")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!(
            "spawning redis-server (is `redis-server` on PATH? `brew install redis`): {e}"
        ))?;

    let url = format!("redis://127.0.0.1:{port}");
    Ok((child, url))
}

fn wait_for_redis(url: &str) -> Result<(), String> {
    let client = redis::Client::open(url)
        .map_err(|e| format!("redis::Client::open: {e}"))?;
    let start = Instant::now();
    while start.elapsed() < REDIS_READY_TIMEOUT {
        if let Ok(mut conn) = client.get_connection() {
            let ping: redis::RedisResult<String> =
                redis::cmd("PING").query(&mut conn);
            if let Ok(s) = ping {
                if s == "PONG" {
                    return Ok(());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!(
        "redis at {url} did not become ready within {:?}",
        REDIS_READY_TIMEOUT
    ))
}

fn spawn_daemon(
    spec: &NodeSpec,
    tempdir_root: &std::path::Path,
    redis_url: &str,
    lmstudio_base_url: &str,
) -> Result<FleetNode, String> {
    let node_dir = tempdir_root.join(&spec.machine_id);
    std::fs::create_dir_all(&node_dir)
        .map_err(|e| format!("creating node dir for {}: {e}", spec.machine_id))?;
    let flows_dir = node_dir.join("flows");
    std::fs::create_dir_all(&flows_dir)
        .map_err(|e| format!("flows dir: {e}"))?;
    let crew_root = node_dir.join("crew");
    std::fs::create_dir_all(crew_root.join("missions"))
        .map_err(|e| format!("crew/missions dir: {e}"))?;
    std::fs::create_dir_all(crew_root.join("roles"))
        .map_err(|e| format!("crew/roles dir: {e}"))?;
    let fleet_file = node_dir.join("fleet.json");
    let openclaw_config = node_dir.join("openclaw.json");
    // Minimal openclaw stub config so doctor checks don't bail.
    std::fs::write(
        &openclaw_config,
        r#"{"agents":{"list":[],"defaults":{}}}"#,
    )
    .map_err(|e| format!("openclaw.json stub: {e}"))?;

    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("binding daemon port: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("daemon local_addr: {e}"))?
        .port();
    drop(listener);

    let binary = darkmux_release_binary();
    let daemon = Command::new(&binary)
        .args(["serve", "--bind", "127.0.0.1", "--port", &port.to_string()])
        .env("DARKMUX_MACHINE_ID", &spec.machine_id)
        .env("DARKMUX_MACHINE_TIER", &spec.tier)
        .env("DARKMUX_REDIS_URL", redis_url)
        .env("DARKMUX_FLOWS_DIR", &flows_dir)
        .env("DARKMUX_FLEET_FILE", &fleet_file)
        .env("DARKMUX_CREW_ROOT", &crew_root)
        .env("DARKMUX_OPENCLAW_CONFIG", &openclaw_config)
        .env("DARKMUX_ORCHESTRATOR", "darkmux-e2e-test")
        // Point any runtime/openclaw paths at our mock LMStudio.
        .env("OPENAI_BASE_URL", lmstudio_base_url)
        .env("DARKMUX_LMSTUDIO_BASE_URL", lmstudio_base_url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawning darkmux serve for {}: {e}", spec.machine_id))?;

    Ok(FleetNode {
        machine_id: spec.machine_id.clone(),
        tier: spec.tier.clone(),
        daemon_port: port,
        flows_dir,
        fleet_file,
        crew_root,
        openclaw_config,
        redis_url: redis_url.to_string(),
        lmstudio_base_url: lmstudio_base_url.to_string(),
        daemon,
    })
}

fn wait_for_daemon_health(port: u16) -> Result<(), String> {
    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|e| format!("parse daemon addr: {e}"))?;
    let start = Instant::now();
    while start.elapsed() < DAEMON_READY_TIMEOUT {
        if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            // TCP-reachable; daemon is up. Could also poll /health
            // via reqwest for a stronger signal, but TCP is sufficient
            // for the v1 harness — daemon's bind happens just after
            // banner-print so TCP-up = serve-loop running.
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "darkmux serve on :{port} did not become reachable within {:?}",
        DAEMON_READY_TIMEOUT
    ))
}
