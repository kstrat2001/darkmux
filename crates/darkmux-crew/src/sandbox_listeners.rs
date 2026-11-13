//! (#3559) The watch on what a sandbox on the host listens on.
//!
//! A sandbox on the host (Seatbelt, Landlock) shares this machine's network
//! on the default network, and neither kernel sandbox can hold a listener to
//! the loopback: Seatbelt's address filter knows only `*` and `localhost`,
//! and `localhost` matches every local address for a bind; Landlock filters
//! a bind by port, not address, and a `listen` on an unbound socket binds
//! every address without asking it. Code a dispatch runs legitimately
//! serves on the loopback (test servers, a dev server a browser test
//! drives), so inbound cannot simply be denied. darkmux watches instead:
//! while the sandbox runs, every [`CHECK_EVERY`] it reads the TCP listeners
//! the sandbox's processes hold
//! ([`darkmux_platform::Platform::tcp_listeners_held_by`]: its launcher, its
//! process group, and their descendants, read from each process's own open
//! files), and stops the sandbox on any listener beyond the loopback, or
//! when the listing cannot be read twice running (it cannot be vouched for).
//!
//! What this is, and is not:
//! - **It catches mistakes, not code trying to get past it.** A server that
//!   binds every address by default is stopped; a program that means to
//!   evade the watch can. A plain double-fork daemon under Seatbelt (fork,
//!   `setsid`, fork again, the middle process exits) leaves the sandbox's
//!   process group with no parent back to it: no look sees its listener,
//!   and it outlives the dispatch (measured in review).
//! - **Detection with a window, not a kernel boundary.** A listener beyond
//!   the loopback is reachable from the LAN until the next look finds it and
//!   the sandbox is stopped: up to one interval plus the stop.
//! - **TCP only.** A UDP socket is not watched: an unconnected UDP client
//!   socket (a resolver's query) is bound to every address by the kernel, so
//!   a UDP listener cannot be told from ordinary outbound traffic.
//! - **The sandbox's processes only.** Another program's listener never
//!   stops it, another sandbox's included: a Seatbelt sandbox is a session of
//!   its own, so nothing in one can move into another's process group. A
//!   process that started a session of its own and whose parents back to the
//!   sandbox have all exited (Seatbelt: `setsid` then the parent exits) is
//!   not found; on Landlock every process descends from the sandbox's init,
//!   so none is lost that way.
//! - **One listing, its holders as they are now.** A listener whose holder
//!   keeps forking a new pid and exiting the old can be missed by a look.
//!
//! A container is not watched: only its published loopback ports reach it.
//! A sandbox with no network ([`darkmux_sandbox::Network::None`]: contained
//! commands, the mods gate, a lab run's verify) cannot listen beyond its own
//! loopback and is not watched either. Client previews watch their two
//! assigned ports instead (`darkmux-serve`'s `client_preview_on_host`), with
//! the same loopback test ([`PortListener::on_loopback`]).

use darkmux_platform::{Platform, PortListener};
use darkmux_sandbox::{BackendKind, SandboxHandle};
use std::sync::mpsc;
use std::time::Duration;

/// How often a running sandbox on the host has its listeners read, and so
/// the most a listener beyond the loopback is reachable before the stop
/// begins. One look reads the process table and each member's open files
/// in-process (no command spawned): 0.52 ms of CPU on a Mac with 1,276
/// processes (a release build, 50 looks; 0.64 ms in a debug build), so a
/// look a second is about 0.05% of one core per running sandbox.
pub const CHECK_EVERY: Duration = Duration::from_secs(1);

/// One look at a sandbox's listeners: nothing beyond the loopback, a
/// listener that is (why it stops the sandbox), or no reading (why one
/// more would stop it).
#[derive(Debug, PartialEq, Eq)]
pub enum Look {
    Clear,
    Exposed(String),
    Unread(String),
}

/// One look, through `platform`, at the sandbox of `kind` that `leader`
/// leads, running `what` ("a dispatch").
pub fn look(platform: &dyn Platform, kind: BackendKind, what: &str, leader: u32) -> Look {
    match platform.tcp_listeners_held_by(leader) {
        Some(held) => exposure_reason(kind, what, &held).map_or(Look::Clear, Look::Exposed),
        None => Look::Unread(format!(
            "darkmux cannot read what it listens on, and {what} in {} must be watched: its sandbox cannot hold a \
             listener to the loopback (#3559). {}",
            darkmux_sandbox::placed_in(kind),
            darkmux_sandbox::way_back_to_docker(kind),
        )),
    }
}

/// Why a sandbox of `kind` running `what`, whose processes hold `held`,
/// must stop: each listener beyond the loopback (once, with every pid
/// holding it) and the fix. `None` when every one is on the loopback.
pub fn exposure_reason(kind: BackendKind, what: &str, held: &[(u16, PortListener)]) -> Option<String> {
    let mut named: Vec<(String, Vec<u32>)> = Vec::new();
    for (port, l) in held.iter().filter(|(_, l)| !l.on_loopback()) {
        let at = l.named_at(*port);
        match named.iter_mut().find(|(n, _)| *n == at) {
            Some((_, pids)) => pids.extend(l.pid),
            None => named.push((at, l.pid.into_iter().collect())),
        }
    }
    if named.is_empty() {
        return None;
    }
    let listed: Vec<String> = named
        .into_iter()
        .map(|(at, pids)| match pids.as_slice() {
            [] => at,
            [one] => format!("{at} (pid {one})"),
            many => format!("{at} (pids {})", many.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")),
        })
        .collect();
    Some(format!(
        "the program listened on {}, beyond this machine's loopback: a server in {what} must listen on 127.0.0.1 \
         (or localhost). {} in {} runs on this machine, whose sandbox cannot hold a listener to the loopback \
         (#3559), so darkmux stops one that listens past it",
        listed.join(", "),
        capitalized(what),
        darkmux_sandbox::placed_in(kind),
    ))
}

/// `s` with its first letter upper case, to start a sentence.
fn capitalized(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
}

/// A running watch: a thread that looks every interval until it stops the
/// sandbox or this is dropped. Dropping it ends the thread at once and
/// joins it.
pub struct ListenerWatch {
    wake: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ListenerWatch {
    /// Looks at once and then every `every`, calling `stop` (at most once,
    /// then ending) with the reason when a look finds a listener beyond the
    /// loopback, or when two looks in a row cannot read.
    pub fn start(every: Duration, mut look: impl FnMut() -> Look + Send + 'static, stop: impl FnOnce(String) + Send + 'static) -> Self {
        let (wake, asleep) = mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let mut missed = false;
            loop {
                match look() {
                    Look::Clear => missed = false,
                    Look::Exposed(why) => return stop(why),
                    Look::Unread(why) if missed => return stop(why),
                    Look::Unread(_) => missed = true,
                }
                // A send or a hang-up (the watch dropped) ends it; only the
                // interval passing looks again.
                if asleep.recv_timeout(every) != Err(mpsc::RecvTimeoutError::Timeout) {
                    return;
                }
            }
        });
        ListenerWatch { wake: Some(wake), thread: Some(thread) }
    }
}

impl Drop for ListenerWatch {
    fn drop(&mut self) {
        drop(self.wake.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// (#3559) Watches the sandbox `handle` names, on the default network,
/// whose processes `leader` leads (the process its backend's command
/// started: an on-host backend's launcher leads the sandbox's process
/// group), every [`CHECK_EVERY`] for as long as the watch lives. `what`
/// names what it runs ("a dispatch"). `None` for a container: nothing to
/// watch.
pub fn watch(handle: &SandboxHandle, leader: u32, what: &'static str, stop: impl FnOnce(String) + Send + 'static) -> Option<ListenerWatch> {
    let kind = handle.backend;
    kind.on_host().then(|| ListenerWatch::start(CHECK_EVERY, move || look(darkmux_platform::current(), kind, what, leader), stop))
}

#[cfg(test)]
#[path = "sandbox_listeners_tests.rs"]
mod tests;
