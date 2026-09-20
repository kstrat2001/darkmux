//! Daemon reachability probe + the every-dispatch "you won't see records
//! live" nudge.
//!
//! Lives in `darkmux-flow` because the nudge is about live flow-record
//! visibility: the `darkmux serve` daemon serves the flow stream over HTTP/
//! SSE, and a dispatch run with the daemon down still writes flow records to
//! disk but the operator can't watch them live. Both the dispatch path
//! and the serve daemon itself reference these, so the probe lives in the
//! foundation flow crate that both depend on (#463 cycle-break — relocated
//! here from `serve` so `crew` doesn't depend on `serve`).

/// The BUILT-IN default address the local `darkmux serve` daemon binds to
/// — the bottom tier only, never the answer on its own.
///
/// (#2765) **Call [`daemon_addr`], not this.** This constant used to BE the
/// probe's address, and that is precisely the defect the issue reports: the
/// daemon's port lived only in its launch command, so a machine whose
/// daemon was started on a different port kept every client probing 8765.
/// The nudge below then printed "darkmux serve isn't reachable on
/// 127.0.0.1:8765" at a healthy daemon answering elsewhere, and the
/// operator lost the live view with nothing anywhere reporting an error.
/// Kept public because it names the built-in tier (`config_access`'s
/// `SERVE_PORT_DEFAULT`/`SERVE_BIND_DEFAULT` compose to exactly this) and
/// because tests assert the default is still what operators read in docs.
pub const DEFAULT_DAEMON_ADDR: &str = "127.0.0.1:8765";

/// (#907) The BUILT-IN default daemon port as a typed value — single source
/// for the `8765` literal so callers don't re-derive it by string-splitting
/// `DEFAULT_DAEMON_ADDR` (which is wrong for IPv6 / port-less addresses).
///
/// (#2765) For "what port is the daemon on THIS machine", call
/// [`daemon_port`] — this is the bottom tier of that resolution, not the
/// resolution.
pub const DEFAULT_DAEMON_PORT: u16 = 8765;

/// (#2765) The resolved `host:port` a client on this machine should probe to
/// reach the local daemon — `env(DARKMUX_SERVE_BIND/_PORT) > config.serve.*
/// > the built-in defaults above`, with a wildcard bind probed on loopback.
///
/// Thin by design: the resolution itself lives in ONE place
/// (`darkmux_types::config_access::serve_client_addr`), the same place
/// `darkmux serve` itself reads its listen address from. Re-exported here
/// so the client-side probe callers that already depend on this module do
/// not each grow their own config read.
pub fn daemon_addr() -> String {
    darkmux_types::config_access::serve_client_addr()
}

/// (#2765) The resolved daemon port — `env(DARKMUX_SERVE_PORT) >
/// config.serve.port > 8765`. For callers that need the port alone (a
/// portless peer address getting a default appended, a viewer URL).
pub fn daemon_port() -> u16 {
    darkmux_types::config_access::serve_port()
}

/// Probe-budget timeout for the every-dispatch reachability check.
/// Shared between the production hardcoded probe and the test helpers
/// so a future drift doesn't leave the budget assertions and the
/// actual probe disagreeing.
pub const PROBE_TIMEOUT_MS: u64 = 300;

/// Best-effort TCP probe of the local daemon. Returns `true` when a
/// connection can be opened to [`daemon_addr`] within `PROBE_TIMEOUT_MS`.
/// Intentionally lightweight (no HTTP request) — the more thorough
/// `/health` probe lives in `doctor::check_daemon_reachable` and is run on
/// operator-explicit `darkmux doctor` invocation; this helper is for the
/// every-dispatch pre-flight nudge where probe cost matters.
///
/// (#2765) Probes the RESOLVED address, not the built-in literal. The
/// address it probed and the address the nudge prints come from the same
/// call ([`nudge_if_daemon_unreachable`] resolves once and passes it in), so
/// the message can never name a port other than the one that was tried.
pub(crate) fn is_daemon_reachable_at(addr: &str) -> bool {
    let addr: std::net::SocketAddr = match addr.parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    is_addr_reachable(addr, std::time::Duration::from_millis(PROBE_TIMEOUT_MS))
}

/// Pure-probe helper: TCP connect with timeout, no `/health` request.
/// Extracted so tests can verify the return-value contract against a
/// known-closed port without depending on the operator's running
/// daemon state (`is_daemon_reachable_at` takes whatever address resolved,
/// which would make a return-false assertion brittle in CI where that port
/// may or may not be in use).
fn is_addr_reachable(addr: std::net::SocketAddr, timeout: std::time::Duration) -> bool {
    std::net::TcpStream::connect_timeout(&addr, timeout).is_ok()
}

/// Print the one-line stderr nudge if the daemon isn't reachable.
/// Non-blocking: the dispatch always proceeds; this is purely
/// situational awareness so an operator who closed the daemon tab
/// last week doesn't lose visibility into a multi-minute dispatch
/// before realizing it.
///
/// `verb_hint` is the verb the operator just ran (e.g. "dispatch"
/// or "phase review"); used in the nudge to make the message
/// context-specific.
pub fn nudge_if_daemon_unreachable(verb_hint: &str) {
    // (#2765) Resolve ONCE and use the same string for the probe and the
    // message. The bug this closes was the two disagreeing: the operator
    // read "isn't reachable on 127.0.0.1:8765" while their daemon was
    // healthy on the port their config named, and the live view went dark
    // with no error anywhere.
    let addr = daemon_addr();
    if is_daemon_reachable_at(&addr) {
        return;
    }
    eprintln!(
        "[!] darkmux serve isn't reachable on {}. `{}` will write flow records to disk \
         but you won't see them live. To enable live viewing, start the daemon: \
         `brew services start darkmux` (or `darkmux serve` from source).",
        addr, verb_hint
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Listening port reports reachable. Bound on an ephemeral
    /// loopback port so the assertion is deterministic.
    ///
    /// Split into a separate test (formerly one combined assertion with
    /// a drop-and-reprobe second leg, #188) because the drop+reprobe
    /// pattern raced macOS TIME_WAIT semantics: the kernel briefly
    /// kept the just-released port in a state where `connect_timeout`
    /// could still report reachable. Disjoint resources for each
    /// assertion eliminates the race.
    #[test]
    fn is_addr_reachable_returns_true_for_listening_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
        let open_addr = listener.local_addr().expect("local_addr");
        assert!(is_addr_reachable(open_addr, std::time::Duration::from_millis(PROBE_TIMEOUT_MS)));
        // Listener drops at end of scope — no second probe, no race.
    }

    /// Closed port reports unreachable. Uses port 1 (tcpmux, reserved
    /// in IANA's well-known range; not bound by any process on a normal
    /// system). The connect attempt gets ECONNREFUSED essentially
    /// instantly, well under PROBE_TIMEOUT_MS.
    ///
    /// Picked deliberately over: (a) drop-and-reprobe an ephemeral —
    /// races TIME_WAIT (the #188 flake); (b) an arbitrary high port —
    /// non-zero collision probability with whatever happens to be
    /// running on the test machine.
    #[test]
    fn is_addr_reachable_returns_false_for_closed_port() {
        let closed: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert!(!is_addr_reachable(closed, std::time::Duration::from_millis(PROBE_TIMEOUT_MS)));
    }

    /// Lock the probe budget so a future timeout-doubling slip doesn't
    /// silently make the every-dispatch nudge a noticeable pre-flight tax.
    #[test]
    fn is_addr_reachable_respects_probe_timeout_budget() {
        // Probe a known-unroutable address (TEST-NET-1, RFC 5737) so
        // the timeout path is exercised, not the connect-refused path.
        let dead: std::net::SocketAddr = "192.0.2.1:1".parse().unwrap();
        let timeout = std::time::Duration::from_millis(PROBE_TIMEOUT_MS);
        let start = std::time::Instant::now();
        let result = is_addr_reachable(dead, timeout);
        let elapsed = start.elapsed();

        assert!(!result, "unroutable address must report unreachable");
        // 4x, not 2x. The 2x bound flaked on main (2026-09-20): the
        // coverage job measured 803ms against a 600ms bound and went red,
        // while the same job passed on a PR minutes earlier. This body runs
        // inside a `cargo llvm-cov` instrumented binary on a shared runner,
        // so hundreds of ms of scheduling + instrumentation overhead land
        // between `Instant::now()` and the syscall returning -- noise that
        // has nothing to do with the budget being measured.
        //
        // Widening does not give up the regression this guards, because
        // that regression is not a doubled constant (that is a deliberate
        // edit, visible in review). It is the timeout NOT BEING APPLIED --
        // `connect` falling back to the OS default, which on macOS is ~75
        // SECONDS. A 1.2s bound still catches that by ~60x. Tightening this
        // back to 2x to "catch a doubling" trades a real, enormous signal
        // for an intermittently-red pipeline.
        assert!(
            elapsed < std::time::Duration::from_millis(PROBE_TIMEOUT_MS * 4),
            "probe should respect ~{}ms budget, took {:?}",
            PROBE_TIMEOUT_MS,
            elapsed
        );
    }

    #[test]
    fn default_daemon_addr_is_127_0_0_1_8765() {
        // Lock the address — anything else surprises operators reading
        // the nudge stderr line for the first time.
        assert_eq!(DEFAULT_DAEMON_ADDR, "127.0.0.1:8765");
        let parsed: std::net::SocketAddr = DEFAULT_DAEMON_ADDR.parse().expect("must parse");
        assert_eq!(parsed.port(), 8765);
        assert!(parsed.ip().is_loopback());
        // (#907) the typed port const must stay in sync with the addr literal.
        assert_eq!(parsed.port(), DEFAULT_DAEMON_PORT);
        // (#2765) …and the built-in tier `config_access` resolves through
        // must compose to exactly this. Two constants for one built-in
        // default is how the server and the client drifted apart in the
        // first place; this pins them together.
        assert_eq!(
            format!(
                "{}:{}",
                darkmux_types::config_access::SERVE_BIND_DEFAULT,
                darkmux_types::config_access::SERVE_PORT_DEFAULT
            ),
            DEFAULT_DAEMON_ADDR
        );
    }

    /// (#2765) The probe address is RESOLVED, not the literal. This is the
    /// defect the issue reports: a daemon started on a configured port kept
    /// every client probing 8765, and the nudge told the operator their
    /// healthy daemon was unreachable while the live view silently went
    /// dark.
    #[serial_test::serial]
    #[test]
    fn the_probe_follows_the_configured_port_not_the_built_in_literal() {
        let k = "DARKMUX_SERVE_PORT";
        let prev = std::env::var(k).ok();
        unsafe { std::env::remove_var(k) };
        assert_eq!(daemon_addr(), DEFAULT_DAEMON_ADDR, "unset resolves to the built-in");

        unsafe { std::env::set_var(k, "8799") };
        assert_eq!(daemon_addr(), "127.0.0.1:8799", "the configured port wins");
        assert_eq!(daemon_port(), 8799);
        assert_ne!(
            daemon_addr(),
            DEFAULT_DAEMON_ADDR,
            "if this ever equals the literal again, every client is back to \
             probing a port the daemon may not be on"
        );

        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    /// (#2765) The probe honors the resolved address for real — a listener
    /// on a non-default port is found because the resolution pointed there,
    /// which a hardcoded 8765 could not have done.
    #[serial_test::serial]
    #[test]
    fn a_listener_on_a_non_default_port_is_reachable_once_the_config_names_it() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
        let port = listener.local_addr().expect("local_addr").port();
        let k = "DARKMUX_SERVE_PORT";
        let prev = std::env::var(k).ok();

        unsafe { std::env::set_var(k, port.to_string()) };
        assert!(
            is_daemon_reachable_at(&daemon_addr()),
            "the resolved address must be the one actually probed"
        );

        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}
