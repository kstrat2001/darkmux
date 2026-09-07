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
//! `DARKMUX_MACHINE_ID` / `DARKMUX_REDIS_URL` env vars. Provides helpers
//! to:
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
//! ### Build-once across the six e2e test BINARIES (#1291)
//!
//! This file is `#[path]`-included into six separate `tests/e2e_*.rs`
//! integration-test binaries, each its own OS process. A per-process
//! `OnceLock` (as this module used to rely on alone) memoizes the build
//! within one binary but can't stop the other five binaries from each
//! running their own `cargo build --release`: up to six redundant
//! invocations per `cargo test`, most of them no-ops but each still
//! paying cargo's lock+fingerprint walk, and contending on cargo's own
//! target-dir lock under `--jobs`-parallel test-binary execution.
//! `build_darkmux_release()` now wraps the actual build in a
//! cross-process `flock(2)` (POSIX; same `FlockGuard` pattern as
//! `darkmux-lab`'s registry lock and `darkmux-flow`'s audit sink) on a
//! lock file under `target/`, so the six binaries serialize into at
//! most one real compile plus five fast blocked-then-no-op waits
//! instead of racing. Set `DARKMUX_E2E_BIN=<path>` to point at an
//! already-built binary and skip the build step entirely (e.g. a CI
//! job that built the release binary in an earlier step).
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
/// Subsequent calls in THIS process are no-ops (per-process `OnceLock`
/// memoization); the actual build is additionally serialized ACROSS
/// the six e2e test binaries via a cross-process `flock(2)`; see the
/// module doc's "Build-once across the six e2e test BINARIES" section.
/// Used by `FleetHarness::boot` so tests don't have to remember to do
/// this manually.
fn build_darkmux_release() -> Result<(), String> {
    static BUILD_RESULT: OnceLock<Mutex<Option<Result<(), String>>>> = OnceLock::new();
    let cell = BUILD_RESULT.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().expect("build-result mutex");
    if let Some(r) = guard.as_ref() {
        return r.clone();
    }
    let result = build_darkmux_release_uncached();
    *guard = Some(result.clone());
    result
}

/// The actual build step behind `build_darkmux_release`'s per-process
/// memoization. A `DARKMUX_E2E_BIN` override skips building entirely
/// (the caller, typically CI, has already produced a binary);
/// otherwise the build runs under a cross-process lock on POSIX so the
/// six sibling e2e binaries don't race `cargo build --release` against
/// each other.
fn build_darkmux_release_uncached() -> Result<(), String> {
    if std::env::var_os("DARKMUX_E2E_BIN").is_some() {
        return Ok(()); // caller-provided binary; nothing to build.
    }
    #[cfg(unix)]
    {
        run_release_build_locked()
    }
    #[cfg(not(unix))]
    {
        run_cargo_build_release()
    }
}

/// POSIX-only: acquire an exclusive `flock(2)` on a lock file under
/// `target/` before running the build, so the six e2e binaries
/// (each its own process; see module doc) serialize into at most one
/// real compile instead of contending on cargo's own target-dir lock.
/// Uses the shared `FlockGuard` (`darkmux_types::flock`) — the same type
/// `darkmux-lab`'s registry lock (`crates/darkmux-lab/src/lab/registry.rs`)
/// and `darkmux-flow`'s audit sink (`crates/darkmux-flow/src/integrity.rs`)
/// use, rather than a fifth hand-rolled copy.
#[cfg(unix)]
fn run_release_build_locked() -> Result<(), String> {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let lock_path = PathBuf::from(manifest).join("target/.e2e-release-build.lock");
    darkmux_types::flock::with_locked_file(&lock_path, |_file| {
        // Under the lock: whichever binary gets here first pays the real
        // compile; the other five block on flock, then run a fast no-op
        // `cargo build` (fingerprint check only) instead of racing a full
        // build against each other.
        run_cargo_build_release().map_err(anyhow::Error::msg)
    })
    .map_err(|e| e.to_string())
}

fn run_cargo_build_release() -> Result<(), String> {
    let out = Command::new("cargo")
        .args(["build", "--release", "--bin", "darkmux"])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .output();
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(format!(
            "cargo build --release failed: exit={:?}",
            o.status.code()
        )),
        Err(e) => Err(format!("cargo build --release spawn failed: {e}")),
    }
}

/// One node in the test fleet. Wraps the spawned `darkmux serve`
/// subprocess + the per-node env vars; helpers build CLI commands
/// pre-configured for the node's identity.
pub struct FleetNode {
    pub machine_id: String,
    pub daemon_port: u16,
    /// (#2184) This node's own scoped darkmux config root, set as
    /// `DARKMUX_HOME` on every command `cmd()` builds and on the daemon
    /// itself (`spawn_daemon`) — see `cmd()`'s doc for why this can never
    /// be left unset.
    pub home_dir: PathBuf,
    /// (#2184) This node's own scoped `HOME`, a SIBLING of `home_dir`.
    /// `DARKMUX_HOME` is not on its own sufficient: several paths resolve
    /// through `dirs::home_dir()` and never consult it —
    /// `config_access::fleet_file`, `config_access::cache_dir`,
    /// `crew::dispatch::ack_dir` (`~/.darkmux/acks`, WRITTEN on every
    /// dispatch ack) and `crew::dispatch`'s `identity.md`, plus the
    /// profile/mission-config/workload/skill search paths that include
    /// `~/.darkmux/...` as a candidate. Measured 2026-09-07 against a
    /// plain `cargo build` binary: with `DARKMUX_HOME` set to a tempdir,
    /// `darkmux machine add` still wrote `$HOME/.darkmux/fleet.json`.
    /// This node overrides two of those by name (`DARKMUX_FLEET_FILE`,
    /// `DARKMUX_CREW_DIR`); `HOME` closes the rest at once, and keeps
    /// closing the ones nobody has written yet.
    pub process_home: PathBuf,
    pub flows_dir: PathBuf,
    pub fleet_file: PathBuf,
    pub crew_root: PathBuf,
    pub redis_url: String,
    #[allow(dead_code)] // consumed by Wave-E.2+ scenarios
    pub lmstudio_base_url: String,
    daemon: Child,
}

impl FleetNode {
    /// Build a CLI command pre-configured with this node's env vars.
    /// Caller adds `.args([...])` and `.output()`/`.spawn()`.
    ///
    /// (#2184) `DARKMUX_HOME` is load-bearing, not cosmetic: the binary
    /// this spawns (`darkmux_release_binary()`) is a real `cargo build
    /// --release` artifact with no `test`/`test-support` cfg (see the
    /// module doc), so absent `DARKMUX_HOME` it resolves the operator's
    /// actual `~/.darkmux/config.json` — and a real `hooks` rule there
    /// gets a real POST for every flow record this command writes.
    /// Reproduced live 2026-08-31 (five records reaching a real
    /// crawl-tracker during an ordinary `cargo test` sweep). Every env var
    /// this method sets lives HERE, in the one place every caller goes
    /// through, so a future one can't forget it.
    pub fn cmd(&self) -> Command {
        let binary = darkmux_release_binary();
        let mut cmd = Command::new(binary);
        cmd.env("HOME", &self.process_home)
            .env("DARKMUX_HOME", &self.home_dir)
            .env("DARKMUX_MACHINE_ID", &self.machine_id)
            .env("DARKMUX_REDIS_URL", &self.redis_url)
            .env("DARKMUX_FLOWS_DIR", &self.flows_dir)
            .env("DARKMUX_FLEET_FILE", &self.fleet_file)
            .env("DARKMUX_CREW_DIR", &self.crew_root);
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

#[cfg(test)]
mod darkmux_home_isolation_tests {
    use super::*;

    /// (#2184) Every process this harness spawns runs the real RELEASE
    /// binary (`run_cargo_build_release` — a plain `cargo build --release`,
    /// deliberately outside any `cargo test` session, so it carries none of
    /// the `test`/`test-support` cfg that empties the config tier
    /// elsewhere in this workspace — see `darkmux_types::config_access`'s
    /// module doc). Nothing about being spawned FROM a test makes that
    /// binary read a test config: absent `DARKMUX_HOME`, it resolves
    /// `~/.darkmux/config.json` exactly like an operator's own shell
    /// would. Reproduced live 2026-08-31: with a real `hooks` rule
    /// configured there (a loopback crawl-tracker), five flow records
    /// this harness's daemon and `FleetNode::cmd()` wrote were POSTed to
    /// it during an ordinary `cargo test` sweep.
    ///
    /// The fix is `DARKMUX_HOME`, scoped under the node's own tempdir, on
    /// every spawned command. This asserts the invariant directly against
    /// `FleetNode::cmd()`'s actual env — not the incident, the guarantee:
    /// no command this harness builds may omit it.
    #[test]
    fn cmd_always_sets_darkmux_home_scoped_under_the_node_dir() {
        // A cheap real `Child` — `FleetNode` owns one for real, so this
        // exercises the actual struct rather than a stand-in.
        let daemon = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawning throwaway child for the test node");

        let node_dir = std::env::temp_dir().join("darkmux-2184-cmd-home-test");
        let home_dir = node_dir.join("home");
        let process_home = node_dir.join("process-home");
        let node = FleetNode {
            machine_id: "node-a".to_string(),
            daemon_port: 0,
            home_dir: home_dir.clone(),
            process_home: process_home.clone(),
            flows_dir: node_dir.join("flows"),
            fleet_file: node_dir.join("fleet.json"),
            crew_root: node_dir.join("crew"),
            redis_url: "redis://127.0.0.1:0".to_string(),
            lmstudio_base_url: "http://127.0.0.1:0".to_string(),
            daemon,
        };

        let cmd = node.cmd();
        let env_var = |name: &str| {
            cmd.get_envs()
                .find(|(k, _)| *k == std::ffi::OsStr::new(name))
                .and_then(|(_, v)| v)
                .map(|v| v.to_owned())
        };
        let darkmux_home = env_var("DARKMUX_HOME");

        // (#2184) The other half. `DARKMUX_HOME` is not sufficient on its
        // own: `config_access::fleet_file`, `config_access::cache_dir` and
        // `crew::dispatch::ack_dir` all resolve through `dirs::home_dir()`
        // and never read it. Measured 2026-09-07 against a plain `cargo
        // build` binary: with `DARKMUX_HOME` set to a tempdir, `darkmux
        // machine add` still wrote `$HOME/.darkmux/fleet.json`.
        assert_eq!(
            env_var("HOME"),
            Some(process_home.into_os_string()),
            "FleetNode::cmd() must also scope HOME to this node (#2184) — the paths that \
             resolve through `dirs::home_dir()` rather than `DARKMUX_HOME` otherwise land in \
             the operator's real ~/.darkmux, and the binary this harness spawns is a plain \
             release build with none of the workspace's cfg(test) isolation guards"
        );

        assert_eq!(
            darkmux_home,
            Some(home_dir.into_os_string()),
            "FleetNode::cmd() must set DARKMUX_HOME to THIS node's own scoped dir \
             (#2184) — without it, the real release binary this harness spawns falls \
             through to the operator's actual ~/.darkmux/config.json, and a real \
             `hooks` rule there gets a real POST from every flow record the command \
             writes"
        );
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

/// Configuration for one node in `FleetHarness::boot`. After #590 a node
/// is identified solely by its `machine_id` — machine-capacity tier no
/// longer routes work.
#[derive(Debug, Clone)]
pub struct NodeSpec {
    pub machine_id: String,
}

impl NodeSpec {
    pub fn new(id: &str) -> Self {
        Self {
            machine_id: id.to_string(),
        }
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
    if let Some(path) = std::env::var_os("DARKMUX_E2E_BIN") {
        return PathBuf::from(path);
    }
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

    // (#2184) This node's own darkmux config root — `DARKMUX_HOME` below
    // scopes the release binary to it instead of the operator's real
    // `~/.darkmux`. See `FleetNode::cmd()`'s doc for why this is not
    // optional.
    let home_dir = node_dir.join("home");
    std::fs::create_dir_all(&home_dir).map_err(|e| format!("home dir: {e}"))?;
    // (#2184) A SIBLING of the darkmux root, not its parent: see
    // `FleetNode::process_home`. `<process_home>/.darkmux` is deliberately
    // a different directory from `home_dir`, so a path resolved by the
    // `dirs::home_dir()` route is distinguishable from one resolved by the
    // `DARKMUX_HOME` route when a test needs to tell them apart.
    let process_home = node_dir.join("process-home");
    std::fs::create_dir_all(&process_home).map_err(|e| format!("process home dir: {e}"))?;

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
        .env("HOME", &process_home)
        .env("DARKMUX_HOME", &home_dir)
        .env("DARKMUX_MACHINE_ID", &spec.machine_id)
        .env("DARKMUX_REDIS_URL", redis_url)
        .env("DARKMUX_FLOWS_DIR", &flows_dir)
        .env("DARKMUX_FLEET_FILE", &fleet_file)
        .env("DARKMUX_CREW_DIR", &crew_root)
        // Point the internal runtime at our mock LMStudio.
        .env("OPENAI_BASE_URL", lmstudio_base_url)
        .env("DARKMUX_LMSTUDIO_BASE_URL", lmstudio_base_url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawning darkmux serve for {}: {e}", spec.machine_id))?;

    Ok(FleetNode {
        machine_id: spec.machine_id.clone(),
        daemon_port: port,
        home_dir,
        process_home,
        flows_dir,
        fleet_file,
        crew_root,
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
