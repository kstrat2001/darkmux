//! Daemon reachability probe + the every-dispatch "you won't see records
//! live" nudge.
//!
//! Lives in `darkmux-flow` because the nudge is about live flow-record
//! visibility: the `darkmux serve` daemon serves the flow stream over HTTP/
//! SSE, and a dispatch run with the daemon down still writes flow records to
//! disk but the operator can't watch them live. Both the crew dispatch path
//! and the serve daemon itself reference these, so the probe lives in the
//! foundation flow crate that both depend on (#463 cycle-break — relocated
//! here from `serve` so `crew` doesn't depend on `serve`).

/// Default address the local `darkmux serve` daemon binds to. Used by
/// the pre-dispatch reachability nudge (#104 Sprint 3) so an operator
/// running a dispatch with the daemon down sees a single-line heads-up
/// rather than discovering the silence only when they open the viewer.
pub const DEFAULT_DAEMON_ADDR: &str = "127.0.0.1:8765";

/// Probe-budget timeout for the every-dispatch reachability check.
/// Shared between the production hardcoded probe and the test helpers
/// so a future drift doesn't leave the budget assertions and the
/// actual probe disagreeing.
pub const PROBE_TIMEOUT_MS: u64 = 300;

/// Best-effort TCP probe of the local daemon. Returns `true` when a
/// connection can be opened to `DEFAULT_DAEMON_ADDR` within
/// `PROBE_TIMEOUT_MS`. Intentionally lightweight (no HTTP request) —
/// the more thorough `/health` probe lives in
/// `doctor::check_daemon_reachable` and is run on operator-explicit
/// `darkmux doctor` invocation; this helper is for the every-dispatch
/// pre-flight nudge where probe cost matters.
pub(crate) fn is_daemon_reachable() -> bool {
    let addr: std::net::SocketAddr = match DEFAULT_DAEMON_ADDR.parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    is_addr_reachable(addr, std::time::Duration::from_millis(PROBE_TIMEOUT_MS))
}

/// Pure-probe helper: TCP connect with timeout, no `/health` request.
/// Extracted so tests can verify the return-value contract against a
/// known-closed port without depending on the operator's running
/// daemon state (`is_daemon_reachable` hardcodes the address, which
/// would make a return-false assertion brittle in CI where 8765 may
/// or may not be in use).
fn is_addr_reachable(addr: std::net::SocketAddr, timeout: std::time::Duration) -> bool {
    std::net::TcpStream::connect_timeout(&addr, timeout).is_ok()
}

/// Print the one-line stderr nudge if the daemon isn't reachable.
/// Non-blocking: the dispatch always proceeds; this is purely
/// situational awareness so an operator who closed the daemon tab
/// last week doesn't lose visibility into a multi-minute dispatch
/// before realizing it.
///
/// `verb_hint` is the verb the operator just ran (e.g. "crew dispatch"
/// or "sprint review"); used in the nudge to make the message
/// context-specific.
pub fn nudge_if_daemon_unreachable(verb_hint: &str) {
    if is_daemon_reachable() {
        return;
    }
    eprintln!(
        "[!] darkmux serve isn't reachable on {} — `{}` will write flow records to disk \
         but you won't see them live. To enable live viewing, run `darkmux serve` in another tab.",
        DEFAULT_DAEMON_ADDR, verb_hint
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
        // 2x budget gives slack for slow CI without papering over a
        // regression that doubles the timeout (~600ms+ would catch).
        assert!(
            elapsed < std::time::Duration::from_millis(PROBE_TIMEOUT_MS * 2),
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
    }
}
