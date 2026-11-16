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
/// `flock` on its lock file under the temp directory. `None` when anyone,
/// this process included, already holds it.
fn reserve(port: u16) -> Option<FlockGuard> {
    let path = std::env::temp_dir()
        .join("darkmux-fixture-ports")
        .join(format!("{port}.lock"));
    darkmux_types::flock::try_lock_exclusive_owned(&path)
        .ok()
        .flatten()
}

/// The pids with a TCP listener on `port`, from the OS (`lsof`, present on
/// macOS and on GitHub's ubuntu runners). Readiness uses it to confirm the
/// process answering is the one the test spawned: an answer alone can come
/// from whatever else holds the port.
pub fn listener_pids(port: u16) -> Vec<u32> {
    Command::new("lsof")
        .args(["-nP", "-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default()
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
            !ephemeral.contains(&FIXTURE_PORT_RANGE.start)
                && !ephemeral.contains(&(FIXTURE_PORT_RANGE.end - 1)),
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
}
