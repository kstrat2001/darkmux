//! (#3488, #2898) Ports for the servers a test starts: one allocator for
//! every darkmux test process, whatever crate it belongs to.
//!
//! Binding port 0, reading the number and dropping the listener leaves a
//! gap until the server binds it, and the number is an EPHEMERAL port: the
//! kind the OS gives every other `bind(0)` on the machine and uses as the
//! source port of every outgoing connection. Two test processes can be
//! handed the same one, and a start then sees the other test's server
//! answer on its port.
//!
//! [`pick_fixture_port`] closes that gap for every test that uses it:
//!
//! - The port comes from [`FIXTURE_PORT_RANGE`], below every OS ephemeral
//!   range (Linux assigns 32768-60999, macOS 49152-65535), so no `bind(0)`
//!   and no outgoing connection is ever given it.
//! - It is reserved with an exclusive `flock` on `<port>.lock` in
//!   [`lock_dir`], held until this process exits. `flock` conflicts between
//!   two opens of one file even within one process, so the reservation
//!   covers libtest (a binary's tests run as threads) and nextest (each test
//!   is its own process).
//! - The lock directory is one fixed path per user, not under `TMPDIR`, so
//!   processes with different `TMPDIR`s (the state-leak check gives each
//!   unit its own) still exclude each other. Lock files are never removed
//!   (unlinking a lock file another process may be opening is a race), so
//!   the directory holds at most one empty file per port in the range.
//!
//! A process outside the test suite that starts listening on that exact
//! number inside the gap is the one case no test can prevent.

use crate::flock::FlockGuard;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Fixture ports: below every OS ephemeral range. See the module doc.
pub const FIXTURE_PORT_RANGE: std::ops::Range<u16> = 20_000..32_768;

/// How many random candidates [`pick_fixture_port`] tries before giving up.
const PICK_TRIES: usize = 64;

/// The reservations this process holds, kept until it exits so no other
/// test process (or thread) is handed the same port.
static RESERVED: Mutex<Vec<FlockGuard>> = Mutex::new(Vec::new());

/// A port in [`FIXTURE_PORT_RANGE`] that nothing listens on now and that no
/// other caller of this module, in any process, can be handed.
pub fn pick_fixture_port() -> Result<u16, String> {
    let mut reserved = RESERVED.lock().unwrap_or_else(|e| e.into_inner());
    for _ in 0..PICK_TRIES {
        let port = candidate_port();
        if let Some(guard) = claim(&lock_dir(), port, bindable) {
            reserved.push(guard);
            return Ok(port);
        }
    }
    Err(format!(
        "no free fixture port in {FIXTURE_PORT_RANGE:?} after {PICK_TRIES} tries"
    ))
}

/// [`pick_fixture_port`], for a test that has no use for the error.
///
/// # Panics
///
/// When no port in the range is free.
pub fn fixture_port() -> u16 {
    pick_fixture_port().unwrap_or_else(|e| panic!("{e}"))
}

/// The one lock directory for every darkmux test process of this user. Not
/// under `TMPDIR`: two test processes with different `TMPDIR`s must still
/// see each other's reservations.
pub fn lock_dir() -> PathBuf {
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    PathBuf::from(format!("/tmp/darkmux-fixture-ports-{uid}"))
}

/// The cross-process reservation for `port`: an exclusive, non-blocking
/// `flock` on its lock file in [`lock_dir`]. `None` when anyone, this
/// process included, already holds it.
pub fn reserve(port: u16) -> Option<FlockGuard> {
    reserve_in(&lock_dir(), port)
}

/// [`reserve`] in `dir`. A lock file that cannot be created or locked is a
/// broken setup, never a taken port, so it panics naming the path.
pub fn reserve_in(dir: &Path, port: u16) -> Option<FlockGuard> {
    let path = dir.join(format!("{port}.lock"));
    crate::flock::try_lock_exclusive_owned(&path)
        .unwrap_or_else(|e| panic!("cannot use the fixture port lock {}: {e:#}", path.display()))
}

/// A random port in [`FIXTURE_PORT_RANGE`].
fn candidate_port() -> u16 {
    use std::hash::{BuildHasher, Hasher};
    // `RandomState` is freshly keyed per instance: all the randomness a
    // port choice needs, with no new dependency.
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    let span = u64::from(FIXTURE_PORT_RANGE.end - FIXTURE_PORT_RANGE.start);
    FIXTURE_PORT_RANGE.start + (h.finish() % span) as u16
}

/// `port` reserved in `dir`, when `bindable` then says nothing listens
/// there. `None` when another caller holds it or something is bound there.
fn claim(dir: &Path, port: u16, bindable: impl Fn(u16) -> bool) -> Option<FlockGuard> {
    let guard = reserve_in(dir, port)?;
    bindable(port).then_some(guard)
}

/// Whether a listener can be made on loopback `port` now.
fn bindable(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// A port the OS hands out for `bind(0)` is never a fixture port.
    #[test]
    fn the_range_is_below_the_ephemeral_ports() {
        let ephemeral = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(
            !FIXTURE_PORT_RANGE.contains(&ephemeral),
            "bind(0) handed out {ephemeral}, inside the fixture range"
        );
    }

    /// A port one reservation holds is not handed to another while held;
    /// once let go it can be. A port something listens on is passed over.
    #[test]
    fn a_held_or_listened_on_port_is_passed_over() {
        let dir = tempfile::tempdir().unwrap();
        let held = claim(dir.path(), 20_001, |_| true).expect("a free port is claimed");
        assert!(
            claim(dir.path(), 20_001, |_| true).is_none(),
            "a held port went to a second caller"
        );
        drop(held);
        assert!(
            claim(dir.path(), 20_001, |_| true).is_some(),
            "a port let go is free again"
        );
        assert!(
            claim(dir.path(), 20_002, |_| false).is_none(),
            "a port something listens on is passed over"
        );
    }

    /// A port handed out stays reserved for this process: another caller
    /// asking for that same number before the server binds it is refused.
    #[test]
    fn a_port_handed_out_cannot_be_reserved_again_until_this_process_exits() {
        let port = fixture_port();
        assert!(FIXTURE_PORT_RANGE.contains(&port));
        assert!(
            reserve(port).is_none(),
            "port {port} was handed out, yet could be reserved again"
        );
    }

    /// Names the port a re-run of this binary (the child side below) must
    /// fail to reserve.
    const PARENT_PORT_VAR: &str = "DARKMUX_FIXTURE_PORT_HELD_BY_PARENT";

    /// Another process, here with a `TMPDIR` of its own as the state-leak
    /// check gives each unit, cannot reserve a port this process holds.
    #[test]
    fn another_process_with_its_own_tmpdir_cannot_reserve_a_port_this_one_holds() {
        let port = fixture_port();
        let tmp = tempfile::tempdir().unwrap();
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "test_ports::tests::child_side_cannot_reserve_the_parents_port",
                "--test-threads",
                "1",
            ])
            .env(PARENT_PORT_VAR, port.to_string())
            .env("TMPDIR", tmp.path())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("1 passed"),
            "the child ran no test, so it proved nothing:\n{stdout}"
        );
        assert!(
            out.status.success(),
            "another process reserved port {port} while this one held it:\n{stdout}"
        );
    }

    /// The child side of the test above; with no port named, a no-op.
    #[test]
    fn child_side_cannot_reserve_the_parents_port() {
        let Some(port) = std::env::var(PARENT_PORT_VAR)
            .ok()
            .and_then(|p| p.parse::<u16>().ok())
        else {
            return;
        };
        assert!(
            reserve(port).is_none(),
            "this child reserved port {port}, which its parent holds"
        );
    }

    /// A lock directory that cannot be used is a broken setup, not a taken
    /// port: it fails naming the path.
    #[test]
    fn an_unusable_lock_dir_fails_naming_it() {
        let not_a_dir = tempfile::NamedTempFile::new().unwrap();
        let dir = not_a_dir.path().to_path_buf();
        let Err(failure) = std::panic::catch_unwind(|| reserve_in(&dir, 20_000)) else {
            panic!("an unusable lock dir read as a port someone else holds");
        };
        let said = failure
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default();
        assert!(
            said.contains(&dir.display().to_string()),
            "the failure must name {}: {said}",
            dir.display()
        );
    }
}
