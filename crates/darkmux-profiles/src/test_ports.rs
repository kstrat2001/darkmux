//! (#3488) Ports for this crate's stand-in servers, each one held by one
//! test at a time on this machine.
//!
//! `bind("127.0.0.1:0")` hands out a port from the system's ephemeral range,
//! which every process on the machine draws from, and which the next
//! `bind(0)` in any process may be handed again the moment the first
//! listener closes. Two tests (two nextest processes) given one port both
//! start a stand-in serving `darkmux:m/a` on it, and the second's start
//! fails `port_in_use` on the first's answer. These ports come instead from
//! a range below every system's ephemeral one, and each is held with an
//! `flock` on a file named for it in the temp folder, so no other test asks
//! for it while it is held.

use std::path::{Path, PathBuf};

/// Where each held port's lock file lives: shared by every test process.
fn lock_dir() -> PathBuf {
    std::env::temp_dir().join("darkmux-test-ports")
}

/// Below macOS's ephemeral range (49152-65535) and Linux's (32768-60999),
/// so no port the system hands out for a `bind(0)` or an outgoing
/// connection is ever one of these.
const RANGE: std::ops::Range<u16> = 20_000..30_000;

/// The locks this process holds, kept until it exits: under nextest that
/// is the one test's run.
static HELD: std::sync::Mutex<Vec<darkmux_types::flock::FlockGuard>> =
    std::sync::Mutex::new(Vec::new());

/// A port in [`RANGE`] nothing listens on, held for the rest of this
/// process: no other test on this machine is handed it meanwhile.
pub(crate) fn free_port() -> u16 {
    let dir = lock_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let (port, held) = reserve(&dir, candidates(std::process::id()), bindable)
        .expect("a free test port in 20000-29999");
    HELD.lock().unwrap_or_else(|e| e.into_inner()).push(held);
    port
}

/// Every port of [`RANGE`] once, starting at a place `seed` picks, so
/// processes started together mostly ask for different ports first.
fn candidates(seed: u32) -> impl Iterator<Item = u16> {
    let len = u32::from(RANGE.end - RANGE.start);
    (0..len).map(move |i| RANGE.start + ((seed.wrapping_mul(7919).wrapping_add(i)) % len) as u16)
}

/// Whether a listener can be made on loopback `port` now.
fn bindable(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// The first of `ports` this process now holds the lock of (in `dir`) and
/// that `bindable` says is free.
fn reserve(
    dir: &Path,
    ports: impl Iterator<Item = u16>,
    bindable: impl Fn(u16) -> bool,
) -> Option<(u16, darkmux_types::flock::FlockGuard)> {
    for port in ports {
        let lock = dir.join(format!("{port}.lock"));
        let Ok(Some(held)) = darkmux_types::flock::try_lock_exclusive_owned(&lock) else {
            continue;
        };
        if bindable(port) {
            return Some((port, held));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A port one reservation holds is never handed to another while held,
    /// even when the other asks for it first; once let go it can be.
    #[test]
    fn a_held_port_is_never_handed_out_again() {
        let dir = tempfile::tempdir().unwrap();
        let (first, held) = reserve(dir.path(), [20_001, 20_002].into_iter(), |_| true).unwrap();
        let (second, _) = reserve(dir.path(), [20_001, 20_002].into_iter(), |_| true).unwrap();
        assert_eq!(
            (first, second),
            (20_001, 20_002),
            "a held port went to a second test"
        );
        drop(held);
        let (again, _) = reserve(dir.path(), [20_001].into_iter(), |_| true).unwrap();
        assert_eq!(again, 20_001);
    }

    /// A port something already listens on is passed over.
    #[test]
    fn a_port_in_use_is_passed_over() {
        let dir = tempfile::tempdir().unwrap();
        let (port, _) = reserve(dir.path(), [20_001, 20_002].into_iter(), |p| p != 20_001).unwrap();
        assert_eq!(port, 20_002);
        assert!(reserve(dir.path(), [20_001].into_iter(), |_| false).is_none());
    }

    /// Every candidate is in the range, each once, whatever the seed.
    #[test]
    fn the_candidates_cover_the_range_once() {
        for seed in [0, 1, 4_000_000_000] {
            let mut all: Vec<u16> = candidates(seed).collect();
            all.sort_unstable();
            assert_eq!(all, RANGE.collect::<Vec<_>>());
        }
    }

    /// The range is below the ephemeral ports this machine hands out.
    #[test]
    fn the_range_is_below_the_ephemeral_ports() {
        let ephemeral = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(
            ephemeral >= RANGE.end,
            "bind(0) handed out {ephemeral}, inside the test range"
        );
    }
}
