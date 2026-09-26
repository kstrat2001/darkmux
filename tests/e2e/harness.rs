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
//! ### Why `Drop` is not the whole story (#2716)
//!
//! The `Drop` impls below cover the case where a test FINISHES. They
//! covered nothing else, and the runs that leak are precisely the runs
//! where `Drop` never executes — a `Ctrl-C` on `cargo test`, a `SIGKILL`ed
//! runner, an aborting harness. Measured before the fix: 63 `redis-server`
//! processes on one developer machine, 60 orphaned on ephemeral ports, the
//! oldest 76 days old. Every fixture this module spawns now also goes
//! through [`FixtureGroup`], which makes the children die with the parent
//! (a pipe-EOF watchdog owning their process group) and reaps anything
//! that still got through at the NEXT startup. See
//! [`crate::e2e::fixture_reaper`]'s module doc for the mechanism, and for
//! why its sweep cannot reach a server the harness did not start.
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

use crate::e2e::fixture_reaper::FixtureGroup;
use crate::e2e::mock_lmstudio::MockLmStudio;

const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(15);
const REDIS_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// (#2898) How many times a fixture that EXITS during startup is respawned
/// on a fresh port before boot gives up. A startup death is almost always a
/// lost port (see [`pick_fixture_port`]); a fixture that is genuinely broken
/// dies every time and still fails boot, naming each attempt's exit.
const FIXTURE_START_ATTEMPTS: usize = 4;

/// (#2898) Fixture ports come from 20000-32767, below every OS ephemeral
/// range (Linux assigns 32768-60999, macOS 49152-65535). The old way, bind
/// port 0, read it, drop the listener, spawn, handed out a port FROM the
/// ephemeral range and freed it; in the gap, an outgoing connection (this
/// process polls redis and the daemons connect to it, all the time) could
/// take it as its source port, and the fixture then failed to bind and
/// exited at once. Boot reported that as "fixture pid N was already gone
/// when it was registered". Ports in this range are never assigned as a
/// source port, so only another fixture choosing the same number can
/// collide, and a startup death retries (see `FIXTURE_START_ATTEMPTS`).
const FIXTURE_PORT_RANGE: std::ops::Range<u16> = 20_000..32_768;

fn pick_fixture_port() -> Result<u16, String> {
    use std::hash::{BuildHasher, Hasher};
    let span = u64::from(FIXTURE_PORT_RANGE.end - FIXTURE_PORT_RANGE.start);
    for _ in 0..64 {
        // `RandomState` is freshly keyed per instance, which is all the
        // randomness a port choice needs, with no new dependency.
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u32(std::process::id());
        let port = FIXTURE_PORT_RANGE.start + (h.finish() % span) as u16;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    Err(format!("no free fixture port in {FIXTURE_PORT_RANGE:?} after 64 tries"))
}

/// (#2898) Why a fixture did not come up.
enum StartFailure {
    /// It exited during startup: retry on a fresh port.
    Exited(String),
    /// It is running but never answered, or it could not be recorded:
    /// not a port problem, so no retry.
    Other(String),
}

/// (#2898) Registers a just-spawned fixture and waits for `ready` to report
/// it answering, noticing if it EXITS meanwhile. The exit is what the flake
/// looked like; before this, a fixture that died on a stolen port either
/// failed registration or, worse, left readiness polling a port something
/// else now owned.
fn start_fixture(
    child: &mut Child,
    fixtures: &mut FixtureGroup,
    timeout: Duration,
    log: &std::path::Path,
    mut ready: impl FnMut() -> bool,
) -> Result<(), StartFailure> {
    let exited = |child: &mut Child| -> Option<String> {
        match child.try_wait() {
            Ok(Some(status)) => Some(format!("exited ({status}) during startup; {}", log_tail(log))),
            _ => None,
        }
    };
    if let Err(e) = fixtures.register(child) {
        return Err(match exited(child) {
            Some(why) => StartFailure::Exited(why),
            None => StartFailure::Other(e),
        });
    }
    // Registered from here on, so a death also takes its registry entry
    // with it: the replacement gets its own.
    let mut died = |child: &mut Child, why: String| -> StartFailure {
        match fixtures.forget(child.id()) {
            Ok(()) => StartFailure::Exited(why),
            Err(e) => StartFailure::Other(format!("{why}; and then {e}")),
        }
    };
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Some(why) = exited(child) {
            return Err(died(child, why));
        }
        if ready() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(match exited(child) {
        Some(why) => died(child, why),
        None => StartFailure::Other(format!("did not answer within {timeout:?}; {}", log_tail(log))),
    })
}

/// The last few lines of a fixture's log, for an error message.
fn log_tail(path: &std::path::Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let lines: Vec<&str> = text.lines().rev().take(4).collect();
            if lines.is_empty() {
                format!("{} is empty", path.display())
            } else {
                format!("{} ends: {}", path.display(), lines.into_iter().rev().collect::<Vec<_>>().join(" | "))
            }
        }
        Err(e) => format!("no log at {} ({e})", path.display()),
    }
}

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
    let lock_path = resolved_target_dir().join(".e2e-release-build.lock");
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
    /// `DARKMUX_CREW_DIR`).
    ///
    /// (#2710) This doc used to end "…`HOME` closes the rest at once, and
    /// keeps closing the ones nobody has written yet." **That was false**,
    /// and it is the identical claim #2704 spent two review rounds
    /// disproving for the isolated-roots helper one file over. `HOME` is
    /// the LAST tier of every resolver that has a `DARKMUX_*` override:
    /// `crew::loader::user_state_root` consults `DARKMUX_CREW_DIR` first,
    /// `config_access::audit_enabled` is true on the mere PRESENCE of
    /// `DARKMUX_AUDIT_DIR`, and so on down the list in
    /// `darkmux_types::test_isolation::PINNED_STATE_VARS`. An exported
    /// override BEATS the `HOME` pinned here; it does not get closed by
    /// it. Seven variables were pinned on every spawn and ten were left to
    /// be inherited — on a binary that, per the module doc, carries no
    /// `test`/`test-support` cfg, so none of the workspace's in-test
    /// safety nets apply to it either.
    ///
    /// What `HOME` actually closes is the family of accessors that have NO
    /// env override and reach `dirs::home_dir()` directly (the ones listed
    /// above). That is a real and necessary job, and it is the only one it
    /// does. Everything else is closed by
    /// [`darkmux_release_cmd`]'s `neutralize_state_vars` call.
    pub process_home: PathBuf,
    pub flows_dir: PathBuf,
    pub fleet_file: PathBuf,
    pub crew_root: PathBuf,
    pub redis_url: String,
    /// (#2727) `Some` only when this node's harness was booted via
    /// [`FleetHarness::boot_sharing_redis`] — several harnesses' worth of
    /// nodes then share ONE physical `redis-server`, and this is what
    /// keeps their flow records apart: each harness's nodes write into
    /// their OWN stream (`DARKMUX_REDIS_STREAM`) rather than the shared
    /// default `darkmux:flow`. `None` (the `boot()` path) leaves the env
    /// var unset, so a dedicated-redis node's behavior is byte-identical
    /// to before this field existed.
    pub redis_stream: Option<String>,
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
    ///
    /// (#2710) The claim this doc used to make about `HOME` — that it
    /// "closes the rest at once, and keeps closing the ones nobody has
    /// written yet" — was FALSE, and is corrected on
    /// [`FleetNode::process_home`]. `HOME` is the LAST tier of every
    /// resolver that has a `DARKMUX_*` override, so an exported override
    /// beats it. Seven variables were pinned here and ten were left to be
    /// inherited, `DARKMUX_AUDIT_DIR` among them. The construction now
    /// goes through [`darkmux_release_cmd`], which clears the whole set
    /// first; the seven pins below then re-apply on top, which is the
    /// documented `neutralize, then pin` order.
    pub fn cmd(&self) -> Command {
        let mut cmd = darkmux_release_cmd();
        cmd.env("HOME", &self.process_home)
            .env("DARKMUX_HOME", &self.home_dir)
            .env("DARKMUX_MACHINE_ID", &self.machine_id)
            .env("DARKMUX_REDIS_URL", &self.redis_url)
            .env("DARKMUX_FLOWS_DIR", &self.flows_dir)
            .env("DARKMUX_FLEET_FILE", &self.fleet_file)
            .env("DARKMUX_CREW_DIR", &self.crew_root);
        // (#2727) Must match whatever the daemon itself is running under
        // (`spawn_daemon` sets the identical var from the identical
        // field) — a one-shot CLI command's flow record and the daemon's
        // own records have to land in the SAME stream, or the two halves
        // of one node's own state disagree with each other, isolation
        // question aside.
        if let Some(stream) = &self.redis_stream {
            cmd.env("DARKMUX_REDIS_STREAM", stream);
        }
        cmd
    }

    /// Returns false if the daemon process has exited. Used by tests
    /// that want to verify the daemon survived a scenario. (Wave-E.2+.)
    #[allow(dead_code)]
    pub fn is_alive(&mut self) -> bool {
        matches!(self.daemon.try_wait(), Ok(None))
    }

    /// This node's daemon pid, for the fixture registry and for the
    /// hard-kill regression test (#2716).
    pub fn daemon_pid(&self) -> u32 {
        self.daemon.id()
    }

    /// (#2716) Kill the daemon now rather than at field-drop time.
    /// `FleetHarness::drop` calls this for every node BEFORE it stands the
    /// watchdog down, so the window in which the harness has disarmed its
    /// own die-with-parent guard but not yet killed its children is empty.
    /// Field drop still runs `Drop` below; a second `kill` on an already
    /// reaped child is a no-op error this discards.
    pub fn kill_daemon(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

impl Drop for FleetNode {
    fn drop(&mut self) {
        self.kill_daemon();
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
            redis_stream: None,
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

        // (#2710) The third half. `HOME` + `DARKMUX_HOME` + five named
        // pins left TEN state variables to be inherited from whatever
        // shell ran `cargo test`, and the doc on `process_home` used to
        // claim `HOME` closed them. It does not: every one of them
        // OUTRANKS `HOME` in its own resolver, and `DARKMUX_AUDIT_DIR`
        // does not even name a destination — its presence turns the
        // hash-chained sink on.
        //
        // Asserted off the two lists rather than written out, so a
        // destination added to `test_isolation` later is covered here the
        // moment it lands. A variable this node pins explicitly must be
        // pinned UNDER the node dir; every other one must be an explicit
        // removal, never an inherit.
        use darkmux_types::test_isolation::{CLEARED_STATE_VARS, PINNED_STATE_VARS};
        for (var, _) in PINNED_STATE_VARS {
            let observed = cmd
                .get_envs()
                .find(|(k, _)| *k == std::ffi::OsStr::new(*var))
                .map(|(_, v)| v);
            match observed {
                None => panic!(
                    "{var} is a darkmux write destination and this harness spawns a plain \
                     release binary with no test-support cfg, so leaving it to be inherited \
                     points the child at whatever the ambient shell names — the operator's \
                     own tree in an ordinary terminal. It must be removed or pinned under \
                     the node dir (#2710)."
                ),
                Some(None) => {}
                Some(Some(value)) => assert!(
                    std::path::Path::new(value).starts_with(&node_dir),
                    "{var} is pinned to {value:?}, which is outside this node's own dir {}",
                    node_dir.display()
                ),
            }
        }
        // (#2736 item 2) `DARKMUX_REDIS_URL` is a genuine exception to the
        // loop below, found live while landing that item: `CLEARED_STATE_
        // VARS` says "never re-set" for every OTHER entry, but this
        // harness's whole method is running each simulated fleet node
        // against its OWN ephemeral Redis instance — `cmd()` explicitly
        // re-pins it to `self.redis_url` immediately after
        // `darkmux_release_cmd()` clears it, the same shape as the
        // PINNED_STATE_VARS loop above (an explicit, node-scoped re-pin),
        // just not a path so it cannot share that loop's `starts_with`
        // check. Asserted directly instead of folded into either loop.
        assert_eq!(
            env_var("DARKMUX_REDIS_URL"),
            Some(std::ffi::OsString::from(&node.redis_url)),
            "FleetNode::cmd() must re-pin DARKMUX_REDIS_URL to THIS node's own ephemeral \
             test Redis instance after darkmux_release_cmd() clears it — without the \
             re-pin every node would either inherit the ambient shell's value or, worse, \
             silently share one node's Redis with every other node in the fleet (#2736 \
             item 2)"
        );
        for var in CLEARED_STATE_VARS {
            if *var == "DARKMUX_REDIS_URL" {
                continue; // asserted separately above — this harness deliberately re-pins it
            }
            assert_eq!(
                cmd.get_envs()
                    .find(|(k, _)| *k == std::ffi::OsStr::new(*var))
                    .map(|(_, v)| v),
                Some(None),
                "{var}'s PRESENCE changes behavior, so an inherited value is not merely a \
                 misplaced write. DARKMUX_AUDIT_DIR is the sharp one: it turns the \
                 hash-chained audit sink on for a real release binary, and chained records \
                 cannot be removed without breaking the chain (#2710)."
            );
        }
    }
}

/// (#2727) Who is responsible for this harness's redis-server.
///
/// `Owned` is the original, still-default shape: this harness spawned its
/// own dedicated `redis-server` and `Drop` kills it, same as always.
///
/// `Shared` is new: this harness's nodes were pointed at a `redis-server`
/// that OUTLIVES this one harness — spawned once per test-binary PROCESS
/// (see `shared_redis_url`) and reaped only when that whole process exits,
/// by the same die-with-parent watchdog every fixture already uses. A
/// `Shared` harness therefore must NOT kill it on `Drop`: a sibling test
/// running concurrently in another thread of the same process (`cargo
/// test`'s default) may still be using it.
enum RedisOwnership {
    Owned(Child),
    Shared,
}

/// The full test harness — owns redis, mock-lmstudio, all daemon nodes,
/// and the tempdir holding each node's per-node state. `Drop` tears
/// everything down (except a `Shared` redis — see `RedisOwnership`).
pub struct FleetHarness {
    pub nodes: Vec<FleetNode>,
    pub mock_lmstudio: MockLmStudio,
    redis: RedisOwnership,
    redis_url: String,
    /// (#2716) Owns the pipe-EOF watchdog and the process group every
    /// fixture below is spawned into, plus this run's registry file.
    /// Armed BEFORE the first fixture starts so nothing can be spawned
    /// outside the guard.
    fixtures: FixtureGroup,
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
    /// Boot a fresh harness: build darkmux, spawn a DEDICATED redis on a
    /// free port, spawn the mock LMStudio, then spawn one daemon per
    /// `NodeSpec`. Waits for every daemon's `/health` endpoint before
    /// returning.
    ///
    /// This is the default and the right choice whenever a test's
    /// assertions depend on anything OTHER than the flow stream living on
    /// an isolated redis: presence beats (`darkmux:presence:<hw-uid>`,
    /// keyed on real hardware identity, not on anything this harness
    /// controls) and the fleet work-queue (`darkmux:work`, a fixed name —
    /// see `darkmux-fleet::queue::WORK_STREAM`) are NOT namespaced per
    /// test, so two harnesses on the SAME redis would collide on them.
    /// `boot_sharing_redis` is for the narrower case where neither is in
    /// play.
    pub fn boot(specs: Vec<NodeSpec>) -> Result<Self, String> {
        Self::boot_inner(specs, RedisSource::Dedicated)
    }

    /// Boot a harness whose nodes share a per-TEST-BINARY-PROCESS redis
    /// instead of spawning a dedicated one (#2727).
    ///
    /// `stream` becomes `DARKMUX_REDIS_STREAM` for every node this harness
    /// spawns, so this harness's flow records live in their own stream on
    /// the shared server — structurally unreachable from any sibling
    /// harness using a different `stream` value, the same way two
    /// dedicated redis instances are unreachable from each other today.
    /// Pass something that cannot collide with a sibling test's own
    /// `stream` argument — the enclosing test function's name is the
    /// simplest thing that is guaranteed unique by the compiler (two
    /// `#[test] fn`s in one module cannot share a name).
    ///
    /// **Only safe when the test's assertions never touch presence or the
    /// fleet work-queue** — see `boot`'s doc for why those two are NOT
    /// namespaced by stream. Every current call site of this constructor
    /// is a validation-rejection or roster/`--deep` test that reaches
    /// neither (verified by reading, not assumed — see PR description).
    pub fn boot_sharing_redis(specs: Vec<NodeSpec>, stream: &str) -> Result<Self, String> {
        Self::boot_inner(specs, RedisSource::Shared(stream.to_string()))
    }

    fn boot_inner(specs: Vec<NodeSpec>, redis_source: RedisSource) -> Result<Self, String> {
        build_darkmux_release()?;
        let tempdir =
            tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;

        // (#2716) Armed first: it sweeps whatever a dead previous run left
        // behind, and every fixture spawned after this point is placed in
        // its process group and written into its registry. Anything
        // spawned before it would be outside both halves of the guard.
        let mut fixtures = FixtureGroup::arm();

        // (#2716) Every `?` from here on is an ERROR PATH that leaves a
        // spawned fixture behind: `redis` below is a plain
        // `std::process::Child`, which has no killing `Drop`. What reaps it
        // is `fixtures` dropping WITHOUT a stand-down — the watchdog sees
        // EOF and TERMs the group. That is why `FixtureGroup` deliberately
        // has no `Drop` impl; see the comment in `fixture_reaper.rs`.
        //
        // (#2727) In the `Shared` case there is nothing to spawn here at
        // all — `shared_redis_url()` does that once per PROCESS, the
        // first time any harness in this binary asks for it, and every
        // later call (this one included) just reads back the same URL.
        let (redis, redis_url, stream) = match redis_source {
            RedisSource::Dedicated => {
                let (redis, redis_url) =
                    spawn_redis(&tempdir.path().join("redis"), &mut fixtures)?;
                (RedisOwnership::Owned(redis), redis_url, None)
            }
            RedisSource::Shared(stream) => {
                let redis_url = shared_redis_url()?;
                (RedisOwnership::Shared, redis_url, Some(stream))
            }
        };

        let mock_lmstudio = MockLmStudio::spawn()
            .map_err(|e| format!("spawn mock_lmstudio: {e}"))?;
        let lmstudio_base_url = mock_lmstudio.base_url();

        let mut nodes = Vec::with_capacity(specs.len());
        for spec in specs {
            let node = spawn_daemon(
                &spec,
                tempdir.path(),
                &redis_url,
                stream.as_deref(),
                &lmstudio_base_url,
                &mut fixtures,
            )?;
            nodes.push(node);
        }

        Ok(Self {
            nodes,
            mock_lmstudio,
            redis,
            redis_url,
            fixtures,
            _tempdir: tempdir,
        })
    }

    /// (#2716) This run's fixture registry file — the startup-sweep half
    /// of the guard. Exposed for the test that asserts every fixture
    /// actually reaches it.
    pub fn registry_path(&self) -> &std::path::Path {
        self.fixtures.registry_path()
    }

    /// (#2716) Every OS process this harness owns: the redis fixture
    /// (when `Owned` — a `Shared` redis is NOT this harness's to claim;
    /// see `RedisOwnership`) and one daemon per node. Used by the
    /// hard-kill regression test, which has to assert from OUTSIDE this
    /// process that none of them survived it.
    pub fn fixture_pids(&self) -> Vec<u32> {
        let mut pids = Vec::new();
        if let RedisOwnership::Owned(redis) = &self.redis {
            pids.push(redis.id());
        }
        pids.extend(self.nodes.iter().map(FleetNode::daemon_pid));
        pids
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
        // (#2716) Order matters. Kill every child FIRST — the nodes
        // explicitly rather than leaving them to field-drop, which runs
        // after this body — and only then stand the watchdog down. Standing
        // down first would open a window where the die-with-parent guard is
        // disarmed and the children are still running, which is the exact
        // state this issue is about.
        for node in &mut self.nodes {
            node.kill_daemon();
        }
        // (#2727) A `Shared` redis outlives this one harness — a sibling
        // test in another thread of this same process may still be using
        // it. Only an `Owned` redis is this harness's to kill; the shared
        // instance is reaped once, at PROCESS exit, by its own dedicated
        // watchdog (see `shared_redis_url`).
        if let RedisOwnership::Owned(redis) = &mut self.redis {
            let _ = redis.kill();
            let _ = redis.wait();
        }
        self.fixtures.stand_down();
    }
}

/// (#2727) Which redis a harness should use — the argument to
/// `FleetHarness::boot_inner`. Not `pub`: callers pick one of the two
/// named constructors (`boot` / `boot_sharing_redis`) instead of
/// constructing this directly.
enum RedisSource {
    Dedicated,
    Shared(String),
}

/// (#2727) One `redis-server` per test-BINARY PROCESS, shared by every
/// harness in that process that opts in via `boot_sharing_redis`.
///
/// Spawned at most once per process (`OnceLock::get_or_init` — the same
/// one-shot-across-concurrent-callers guarantee `build_darkmux_release`
/// already relies on) and never explicitly killed: unlike every other
/// fixture this module spawns, whose `Drop` path kills it promptly, this
/// one is deliberately allowed to outlive every individual harness that
/// uses it, because `cargo test` runs a binary's tests as THREADS of one
/// process (unlike nextest's process-per-test model) and a sibling test
/// may still be mid-boot when another one's harness drops.
///
/// It still dies with the process, and by the SAME mechanism every other
/// fixture uses: `FixtureGroup::arm()` below spawns this shared redis's
/// own die-with-parent watchdog, which never gets `stand_down()`'d, so it
/// stays armed for as long as the process lives. When this test-binary
/// process exits — whether all its tests finished normally, or the whole
/// `cargo test` run was `Ctrl-C`'d or `SIGKILL`ed — the pipe the watchdog
/// is reading closes, exactly as it would for any other harness's
/// watchdog, and it TERMs the group. There is deliberately no explicit
/// "last user closes it" refcounting: refcounting to zero mid-run would
/// mean whichever test happens to finish last (an ordering `cargo test`
/// does not promise) tears down a fixture a NEW test could still be about
/// to request, which reintroduces exactly the kind of timing-dependent
/// fixture lifecycle #2716 exists to get away from. One spawn, one
/// teardown, both at process boundaries.
fn shared_redis_url() -> Result<String, String> {
    struct SharedRedis {
        url: String,
        // Held only to keep the child and its tempdir alive for the
        // process's lifetime — never explicitly killed (see doc above).
        // `cargo` warns these fields are never read; that is the point.
        #[allow(dead_code)]
        child: Child,
        #[allow(dead_code)]
        workdir: tempfile::TempDir,
    }
    static SHARED: OnceLock<Result<SharedRedis, String>> = OnceLock::new();
    SHARED
        .get_or_init(|| {
            let workdir = tempfile::tempdir().map_err(|e| format!("shared redis tempdir: {e}"))?;
            let mut fixtures = FixtureGroup::arm();
            let (child, url) = spawn_redis(&workdir.path().join("redis"), &mut fixtures)?;
            // `fixtures` (and its watchdog) is intentionally leaked into
            // this closure's return value having nowhere to go — it is
            // NOT stored on `SharedRedis` because nothing ever needs to
            // call a method on it again; keeping the watchdog `Child`
            // alive is all that matters, and it stays alive because
            // `fixtures` itself is never dropped (see `mem::forget`
            // note below).
            std::mem::forget(fixtures);
            Ok(SharedRedis { url, child, workdir })
        })
        .as_ref()
        .map(|s| s.url.clone())
        .map_err(|e| e.clone())
}

// ===== DARKMUX-SPAWN-HELPERS: BEGIN (#2710) ==========================
//
// The only place this harness resolves and constructs a darkmux command.
// `FleetNode::cmd()` and `spawn_daemon()` both come through
// `darkmux_release_cmd()`, so neither can forget the neutralization.
//
// Load-bearing here in a way it is not elsewhere: this binary is a plain
// `cargo build --release` artifact with NO `test` / `test-support` cfg
// (see the module doc and `darkmux_home_isolation_tests`), so
// `config_access::config()` reads the operator's real
// `~/.darkmux/config.json` and none of the workspace's in-test guards
// apply. That is not hypothetical — 2026-08-31, five flow records this
// harness wrote reached a real crawl-tracker through a `hooks` rule in
// that file.
//
// `DARKMUX_HOME` is cleared here like the rest and re-pinned by both
// callers immediately after; see `neutralize_state_vars`'s ordering note.

/// The workspace's target directory, honoring `CARGO_TARGET_DIR` exactly
/// the way `cargo build` itself does. (#2735) `run_cargo_build_release`
/// shells out to `cargo`, which reads this env var live — so a build made
/// under an override lands at `$CARGO_TARGET_DIR/release/darkmux`, not
/// `<manifest>/target/release/darkmux`. Every caller that needs "the
/// target dir this test run's build actually used" (the binary path, the
/// cross-process build lock) goes through this one function so the two
/// can never drift apart again.
///
/// `scripts/test-lane.sh` sets this for every lane, and CLAUDE.md
/// documents lanes as the recommended way to background a test run — so
/// the previous hardcoded path was wrong for exactly the sweep it needed
/// to be right for.
fn resolved_target_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CARGO_TARGET_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target")
}

fn darkmux_release_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("DARKMUX_E2E_BIN") {
        return PathBuf::from(path);
    }
    resolved_target_dir().join("release/darkmux")
}

/// A `Command` for the release binary with every darkmux state variable
/// stripped. Callers pin their node's own roots AFTER this returns —
/// `Command` applies `.env` and `.env_remove` in call order, so a pin
/// applied first would be erased.
fn darkmux_release_cmd() -> Command {
    let mut cmd = Command::new(darkmux_release_binary());
    darkmux_types::test_isolation::neutralize_state_vars(&mut cmd);
    cmd
}

// ===== DARKMUX-SPAWN-HELPERS: END (#2710) ============================

fn spawn_redis(
    workdir: &std::path::Path,
    fixtures: &mut FixtureGroup,
) -> Result<(Child, String), String> {
    spawn_redis_on(workdir, fixtures, &mut pick_fixture_port)
}

/// [`spawn_redis`] with the port source injected, so a test can hand it a
/// port that is already taken (#2898). Returns once redis answers PING; a
/// redis that exits during startup is respawned on the next port.
fn spawn_redis_on(
    workdir: &std::path::Path,
    fixtures: &mut FixtureGroup,
    next_port: &mut dyn FnMut() -> Result<u16, String>,
) -> Result<(Child, String), String> {
    std::fs::create_dir_all(workdir)
        .map_err(|e| format!("creating redis workdir: {e}"))?;
    let log = workdir.join("redis.log");
    let mut deaths = Vec::new();
    for _ in 0..FIXTURE_START_ATTEMPTS {
        let port = next_port()?;
        // (#2716) Built rather than chained so the fixture group can place
        // it in the watchdog's process group before it is spawned.
        let mut cmd = Command::new("redis-server");
        cmd.arg("--port")
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
            // (#2898) Its own log, so a startup death can say why.
            .arg("--logfile")
            .arg(&log)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        fixtures.place(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| {
            format!("spawning redis-server (is `redis-server` on PATH? `brew install redis`): {e}")
        })?;
        let url = format!("redis://127.0.0.1:{port}");
        match start_fixture(&mut child, fixtures, REDIS_READY_TIMEOUT, &log, || redis_answers(&url)) {
            Ok(()) => return Ok((child, url)),
            Err(StartFailure::Exited(why)) => deaths.push(format!("port {port}: {why}")),
            Err(StartFailure::Other(why)) => return Err(format!("redis-server on port {port}: {why}")),
        }
    }
    Err(format!(
        "redis-server exited during startup {FIXTURE_START_ATTEMPTS} times:\n  {}",
        deaths.join("\n  ")
    ))
}

/// One PING, bounded at every step. (#2898) Deliberately raw TCP rather than
/// redis-rs: the client's `get_connection_with_timeout` bounds only the TCP
/// connect, then runs its own setup exchange with NO read timeout, so a port
/// held by anything that accepts and never replies blocked readiness forever
/// (sampled: `get_connection_with_timeout` -> `setup_connection` -> `read`).
fn redis_answers(url: &str) -> bool {
    use std::io::{Read, Write};
    let Some(port) = url.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()) else {
        return false;
    };
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    if stream.write_all(b"PING\r\n").is_err() {
        return false;
    }
    let mut reply = [0u8; 7];
    matches!(stream.read(&mut reply), Ok(n) if reply[..n].starts_with(b"+PONG"))
}

fn spawn_daemon(
    spec: &NodeSpec,
    tempdir_root: &std::path::Path,
    redis_url: &str,
    redis_stream: Option<&str>,
    lmstudio_base_url: &str,
    fixtures: &mut FixtureGroup,
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

    // (#2898) Same port and startup handling as `spawn_redis`: a port from
    // outside the ephemeral range, readiness that notices the daemon
    // exiting, and a respawn on a fresh port if it does. Readiness is a real
    // `GET /health` answered 200, not a bare TCP connect, which succeeds
    // against whatever happens to hold the port.
    let log = node_dir.join("daemon.log");
    let mut deaths = Vec::new();
    let mut started = None;
    for _ in 0..FIXTURE_START_ATTEMPTS {
        let port = pick_fixture_port()?;
        let stderr = std::fs::File::create(&log).map_err(|e| format!("daemon log: {e}"))?;
        // (#2710) Through the one constructor, so the daemon — which is
        // long-lived and writes flow records for the whole scenario — gets
        // the same neutralization every `FleetNode::cmd()` child gets.
        let mut daemon_cmd = darkmux_release_cmd();
        daemon_cmd
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
            // (#2898) Kept, so a startup death can say why.
            .stderr(stderr);
        // (#2727) Only set for a `boot_sharing_redis` harness — see
        // `FleetNode::cmd()`'s matching pin for why the daemon and this
        // node's own one-shot CLI commands must agree on the same value.
        if let Some(stream) = redis_stream {
            daemon_cmd.env("DARKMUX_REDIS_STREAM", stream);
        }
        // (#2716) The daemon is the SECOND child with the leaked-orphan
        // shape, not just redis — same `Drop`-only teardown, same outcome
        // under a hard kill. It goes through the same guard.
        fixtures.place(&mut daemon_cmd);
        let mut daemon = daemon_cmd
            .spawn()
            .map_err(|e| format!("spawning darkmux serve for {}: {e}", spec.machine_id))?;
        match start_fixture(&mut daemon, fixtures, DAEMON_READY_TIMEOUT, &log, || daemon_healthy(port)) {
            Ok(()) => {
                started = Some((daemon, port));
                break;
            }
            Err(StartFailure::Exited(why)) => deaths.push(format!("port {port}: {why}")),
            Err(StartFailure::Other(why)) => {
                return Err(format!("darkmux serve for {} on port {port}: {why}", spec.machine_id));
            }
        }
    }
    let Some((daemon, port)) = started else {
        return Err(format!(
            "darkmux serve for {} exited during startup {FIXTURE_START_ATTEMPTS} times:\n  {}",
            spec.machine_id,
            deaths.join("\n  ")
        ));
    };

    Ok(FleetNode {
        machine_id: spec.machine_id.clone(),
        daemon_port: port,
        home_dir,
        process_home,
        flows_dir,
        fleet_file,
        crew_root,
        redis_url: redis_url.to_string(),
        redis_stream: redis_stream.map(str::to_string),
        lmstudio_base_url: lmstudio_base_url.to_string(),
        daemon,
    })
}

/// One bounded `GET /health`: ready means the daemon answered 200.
fn daemon_healthy(port: u16) -> bool {
    use std::io::{Read, Write};
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(1000)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    if stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut head = [0u8; 16];
    let Ok(n) = stream.read(&mut head) else { return false };
    let head = String::from_utf8_lossy(&head[..n]);
    head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200")
}

#[cfg(test)]
mod fixture_port_tests {
    use super::*;

    /// Same contract as the e2e targets' own `redis_available`: skip where
    /// redis-server is absent, fail where the run requires it (#1662).
    fn redis_available() -> bool {
        let ok = Command::new("redis-server")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok && std::env::var("DARKMUX_E2E_REQUIRED").is_ok() {
            panic!("redis-server is not on PATH, but DARKMUX_E2E_REQUIRED is set");
        }
        ok
    }

    /// (#2898) Fixture ports never come from an OS ephemeral range, where an
    /// outgoing connection could take one as its source port before the
    /// fixture binds it.
    #[test]
    fn fixture_ports_stay_below_every_ephemeral_range() {
        for _ in 0..200 {
            let port = pick_fixture_port().expect("a free fixture port");
            assert!(
                FIXTURE_PORT_RANGE.contains(&port) && port < 32_768,
                "port {port} is outside {FIXTURE_PORT_RANGE:?} or inside Linux's ephemeral range"
            );
        }
    }

    /// (#2898) The fleet e2e flake: a fixture's port is freed before the
    /// fixture binds it, and something else can take it in between (an
    /// outgoing connection's source port comes from the same ephemeral
    /// range). The fixture then fails to bind and exits at once, and boot
    /// failed with "fixture pid N was already gone when it was registered".
    /// Here the collision is forced: the first port handed out is held open,
    /// and boot must still produce a redis that answers.
    #[test]
    fn a_redis_fixture_whose_first_port_is_taken_still_starts() {
        if !redis_available() {
            eprintln!("skipping: redis-server not on PATH");
            return;
        }
        let holder = std::net::TcpListener::bind("127.0.0.1:0").expect("holding a port");
        let taken = holder.local_addr().expect("held port").port();
        let workdir = tempfile::tempdir().expect("tempdir");
        let mut fixtures = FixtureGroup::arm();
        let mut handed_out = 0usize;
        let mut ports = || -> Result<u16, String> {
            handed_out += 1;
            if handed_out == 1 {
                return Ok(taken);
            }
            let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
            Ok(l.local_addr().map_err(|e| e.to_string())?.port())
        };
        let result = spawn_redis_on(workdir.path(), &mut fixtures, &mut ports)
            .and_then(|(child, url)| {
                if redis_answers(&url) {
                    Ok((child, url))
                } else {
                    Err(format!("{url} does not answer PING"))
                }
            });
        let (mut child, url) = match result {
            Ok(ok) => ok,
            Err(e) => {
                fixtures.stand_down();
                panic!("boot must survive a fixture port that is already taken: {e}");
            }
        };
        assert!(
            !url.ends_with(&format!(":{taken}")),
            "the redis that answered must be on a fresh port, not the taken {taken}: {url}"
        );
        let _ = child.kill();
        let _ = child.wait();
        drop(holder);
        fixtures.stand_down();
    }
}
