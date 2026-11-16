//! (#2898) Ports for the processes a test spawns: one allocator for the
//! fleet harness and every root integration test that hands a port number
//! to a child process to bind.
//!
//! Binding port 0, reading the number, dropping the listener and handing the
//! number to a child leaves a gap: until the child binds, the port is free,
//! and it is an EPHEMERAL port, the kind the OS gives to every other
//! `bind(0)` and uses as the source port of every outgoing connection. Under
//! a parallel run something else takes it in that gap, the child exits with
//! "Address already in use", and a readiness check that only connects can
//! even mistake the thief for the child.
//!
//! [`pick_fixture_port`] closes the gap for everything a test suite does:
//!
//! - The port comes from [`FIXTURE_PORT_RANGE`], below every OS ephemeral
//!   range (Linux assigns 32768-60999, macOS 49152-65535), so no `bind(0)`
//!   and no outgoing connection is ever given it.
//! - It is reserved with an exclusive `flock` on a per-port lock file, held
//!   until this process exits. `flock` conflicts between two opens of the
//!   same file even within one process, so the reservation covers both
//!   libtest (a binary's tests run as threads) and nextest (each test is its
//!   own process): no other caller of this module can choose the port while
//!   the reservation stands.
//! - The lock files live in one fixed directory per user,
//!   `/tmp/darkmux-fixture-ports-<uid>`, not under `TMPDIR`, so processes
//!   with different `TMPDIR`s still exclude each other. They are never
//!   removed (unlinking a lock file another process may be opening is a
//!   race), so the directory is bounded at one empty file per port in
//!   [`FIXTURE_PORT_RANGE`].
//!
//! What remains is a process outside the test suite starting to listen on
//! that exact number inside the gap, which no test can prevent. The fleet
//! harness's `start_fixture` still notices that and respawns.

// Each test binary that includes this file uses a different subset.
#![allow(dead_code)]

use darkmux_types::flock::FlockGuard;
use std::process::Command;
use std::sync::Mutex;

/// Fixture ports: below every OS ephemeral range. See the module doc.
pub const FIXTURE_PORT_RANGE: std::ops::Range<u16> = 20_000..32_768;

/// How many random candidates [`pick_fixture_port`] tries before giving up.
const PICK_TRIES: usize = 64;

/// The reservations this process holds, kept until it exits so no other
/// test process (or thread) can be handed the same port.
static RESERVED: Mutex<Vec<FlockGuard>> = Mutex::new(Vec::new());

/// A port in [`FIXTURE_PORT_RANGE`] that nothing listens on now and that no
/// other caller of this module, in any process, can be handed.
pub fn pick_fixture_port() -> Result<u16, String> {
    let mut reserved = RESERVED.lock().unwrap_or_else(|e| e.into_inner());
    for _ in 0..PICK_TRIES {
        let port = candidate_port();
        if let Some(guard) = claim(port) {
            reserved.push(guard);
            return Ok(port);
        }
    }
    Err(format!(
        "no free fixture port in {FIXTURE_PORT_RANGE:?} after {PICK_TRIES} tries"
    ))
}

/// A random port in [`FIXTURE_PORT_RANGE`].
fn candidate_port() -> u16 {
    use std::hash::{BuildHasher, Hasher};
    // `RandomState` is freshly keyed per instance, which is all the
    // randomness a port choice needs, with no new dependency.
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    let span = u64::from(FIXTURE_PORT_RANGE.end - FIXTURE_PORT_RANGE.start);
    FIXTURE_PORT_RANGE.start + (h.finish() % span) as u16
}

/// Reserve `port`, then confirm nothing is listening on it. `None` when
/// another caller holds the reservation or something is bound there.
fn claim(port: u16) -> Option<FlockGuard> {
    let guard = reserve(port)?;
    std::net::TcpListener::bind(("127.0.0.1", port)).ok()?;
    Some(guard)
}

/// The cross-process reservation for `port`: an exclusive, non-blocking
/// `flock` on its lock file in [`lock_dir`]. `None` when anyone, this
/// process included, already holds it.
fn reserve(port: u16) -> Option<FlockGuard> {
    reserve_in(&lock_dir(), port)
}

/// One directory per user, at a fixed path. Not under `TMPDIR`: two test
/// processes with different `TMPDIR`s (the state-leak check gives each unit
/// its own) must still see each other's reservations.
fn lock_dir() -> std::path::PathBuf {
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    std::path::PathBuf::from(format!("/tmp/darkmux-fixture-ports-{uid}"))
}

/// [`reserve`] in `dir`. A lock file that cannot be created or locked is a
/// broken setup, never a taken port, so it panics naming the path.
fn reserve_in(dir: &std::path::Path, port: u16) -> Option<FlockGuard> {
    let path = dir.join(format!("{port}.lock"));
    darkmux_types::flock::try_lock_exclusive_owned(&path)
        .unwrap_or_else(|e| panic!("cannot use the fixture port lock {}: {e:#}", path.display()))
}

/// The pids with a TCP listener on `port`, from the OS (`lsof`, present on
/// macOS and on GitHub's ubuntu runners). Readiness uses it to confirm the
/// process answering is the one the test spawned: an answer alone can come
/// from whatever else holds the port.
pub fn listener_pids(port: u16) -> Vec<u32> {
    listener_pids_from("lsof", port)
}

fn listener_pids_from(lsof: &str, port: u16) -> Vec<u32> {
    Command::new(lsof)
        .args(["-nP", "-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .collect()
        })
        .unwrap_or_else(|e| {
            panic!("cannot run `{lsof}` ({e}); readiness needs it to tell the fixture from whatever else holds port {port}")
        })
}

/// The OS's ephemeral port range as the OS reports it (Linux's
/// `ip_local_port_range`, macOS's `net.inet.ip.portrange`), or `None` where
/// neither can be read.
pub fn os_ephemeral_range() -> Option<std::ops::RangeInclusive<u16>> {
    let text = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
        .ok()
        .or_else(macos_port_range)?;
    let mut bounds = text
        .split_whitespace()
        .filter_map(|s| s.parse::<u16>().ok());
    Some(bounds.next()?..=bounds.next()?)
}

fn macos_port_range() -> Option<String> {
    let out = Command::new("sysctl")
        .args([
            "-n",
            "net.inet.ip.portrange.first",
            "net.inet.ip.portrange.last",
        ])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture ports never come from the OS's ephemeral range, where a
    /// `bind(0)` or an outgoing connection could take one before the fixture
    /// binds it.
    #[test]
    fn fixture_ports_stay_outside_the_os_ephemeral_range() {
        let ephemeral = os_ephemeral_range().expect("reading the OS ephemeral port range");
        assert!(
            *ephemeral.end() < FIXTURE_PORT_RANGE.start || *ephemeral.start() >= FIXTURE_PORT_RANGE.end,
            "{FIXTURE_PORT_RANGE:?} overlaps the OS ephemeral range {ephemeral:?}"
        );
        for _ in 0..20 {
            let port = pick_fixture_port().expect("a free fixture port");
            assert!(
                FIXTURE_PORT_RANGE.contains(&port),
                "port {port} is outside {FIXTURE_PORT_RANGE:?}"
            );
            assert!(
                !ephemeral.contains(&port),
                "port {port} is in the OS ephemeral range {ephemeral:?}"
            );
        }
    }

    /// The forced interleaving: a port is handed out, and before its fixture
    /// binds it, another test (another process under nextest, another thread
    /// under libtest) asks for that same number. Its reservation must fail,
    /// so it moves on to another port instead of racing this one.
    #[test]
    fn a_port_handed_out_cannot_be_reserved_again_until_this_process_exits() {
        let port = pick_fixture_port().expect("a free fixture port");
        assert!(
            reserve(port).is_none(),
            "port {port} was handed out, yet another caller could reserve it before the fixture binds"
        );
    }

    /// Names the port a re-run of this binary (the child side below) must
    /// fail to reserve.
    const PARENT_PORT_VAR: &str = "DARKMUX_FIXTURE_PORT_HELD_BY_PARENT";

    /// Re-runs this test binary as a separate process that runs only
    /// [`child_side_cannot_reserve_the_parents_port`], with `tmpdir` as its
    /// `TMPDIR` when given.
    fn child_reserves(port: u16, tmpdir: Option<&std::path::Path>) -> std::process::Output {
        // The test's name is its module path without the crate's name.
        let module = module_path!().split_once("::").map_or("", |(_, rest)| rest);
        let name = format!("{module}::child_side_cannot_reserve_the_parents_port");
        let mut cmd = Command::new(std::env::current_exe().expect("this test binary"));
        cmd.args(["--exact", &name, "--nocapture", "--test-threads", "1"]).env(PARENT_PORT_VAR, port.to_string());
        if let Some(dir) = tmpdir {
            cmd.env("TMPDIR", dir);
        }
        cmd.output().expect("re-running this test binary")
    }

    fn assert_child_refused(port: u16, out: &std::process::Output) {
        let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(stdout.contains("1 passed") || !out.status.success(), "the child ran no test, so it proved nothing:\n{stdout}");
        assert!(out.status.success(), "another process reserved port {port} while this one held it:\n{stdout}\n{stderr}");
    }

    /// Another test process (nextest runs each test as its own) cannot be
    /// handed a port this process holds.
    #[test]
    fn another_process_cannot_reserve_a_port_this_one_holds() {
        let port = pick_fixture_port().expect("a free fixture port");
        assert_child_refused(port, &child_reserves(port, None));
    }

    /// The same with a different `TMPDIR`, as the state-leak check gives
    /// every unit it runs in parallel: the reservation must not live under
    /// the temp directory.
    #[test]
    fn another_process_with_its_own_tmpdir_cannot_reserve_a_port_this_one_holds() {
        let port = pick_fixture_port().expect("a free fixture port");
        let tmp = tempfile::tempdir().expect("tempdir");
        assert_child_refused(port, &child_reserves(port, Some(tmp.path())));
    }

    /// The child side of the two tests above; with no port named, a no-op.
    #[test]
    fn child_side_cannot_reserve_the_parents_port() {
        let Some(port) = std::env::var(PARENT_PORT_VAR).ok().and_then(|p| p.parse::<u16>().ok()) else {
            return;
        };
        assert!(reserve(port).is_none(), "this child reserved port {port}, which its parent holds");
    }

    /// A lock directory that cannot be used is a broken setup, not a taken
    /// port: it must fail naming the path, not read as "no free port".
    #[test]
    fn an_unusable_lock_dir_fails_naming_it() {
        let not_a_dir = tempfile::NamedTempFile::new().expect("temp file");
        let dir = not_a_dir.path().to_path_buf();
        let Err(failure) = std::panic::catch_unwind(|| reserve_in(&dir, 20_000)) else {
            panic!("an unusable lock dir read as a port someone else holds");
        };
        let said = failure.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(said.contains(&dir.display().to_string()), "the failure must name {}: {said}", dir.display());
    }

    /// Without `lsof`, readiness cannot tell the fixture from whatever else
    /// holds its port: fail at once, naming it, instead of waiting out a
    /// timeout and blaming the fixture.
    #[test]
    #[should_panic(expected = "no-such-lsof-2898")]
    fn readiness_fails_at_once_naming_lsof_when_it_cannot_run() {
        listener_pids_from("no-such-lsof-2898", 20_000);
    }
}
