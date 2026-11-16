//! (#2898) Ports for the processes a root integration test spawns, and the
//! readiness helpers that go with them.
//!
//! Binding port 0, reading the number, dropping the listener and handing the
//! number to a child leaves a gap: until the child binds, the port is free,
//! and it is an EPHEMERAL port, the kind the OS gives to every other
//! `bind(0)` and uses as the source port of every outgoing connection. Under
//! a parallel run something else takes it in that gap, the child exits with
//! "Address already in use", and a readiness check that only connects can
//! even mistake the thief for the child.
//!
//! The ports come from the one allocator every darkmux test process shares,
//! `darkmux_types::test_ports` (its module doc has the scheme). This file
//! adds what only the root tests need: who holds a listener ([`listener_pids`])
//! and the OS's ephemeral range ([`os_ephemeral_range`]) for asserting that a
//! handed-out port lies outside it.

// Each test binary that includes this file uses a different subset.
#![allow(dead_code)]

pub use darkmux_types::test_ports::{pick_fixture_port, FIXTURE_PORT_RANGE};

use std::process::Command;

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

    /// The fixture range and the ephemeral range THIS OS reports share no
    /// port, so no `bind(0)` or outgoing connection is ever given one.
    #[test]
    fn the_fixture_range_does_not_overlap_the_os_ephemeral_range() {
        let ephemeral = os_ephemeral_range().expect("reading the OS ephemeral port range");
        assert!(
            *ephemeral.end() < FIXTURE_PORT_RANGE.start
                || *ephemeral.start() >= FIXTURE_PORT_RANGE.end,
            "{FIXTURE_PORT_RANGE:?} overlaps the OS ephemeral range {ephemeral:?}"
        );
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
