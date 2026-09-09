//! Test-only helpers shared across `loop_runner`'s tests, its
//! `checkpoint_regression_tests` submodule, and `compaction`'s tests.
//!
//! This module exists for one reason (#2599): make httpmock's
//! first-registered-wins shadowing self-detecting at the type level,
//! instead of relying on each test author to remember to check.
#![cfg(test)]
// (#2599) The one place in the crate allowed to name `httpmock::MockServer`
// directly — `runtime/clippy.toml`'s `disallowed-types` entry forbids it
// everywhere else, so a future test can't quietly opt back out of the
// guard by constructing a bare server. See `GuardedMockServer`'s own doc
// for why that matters.
#![allow(clippy::disallowed_types)]

use httpmock::{Mock, MockServer, Then, When};

/// One `.mock(...)` call recorded by [`GuardedMockServer`].
struct Registration {
    id: usize,
    location: &'static std::panic::Location<'static>,
    expect_zero: bool,
}

/// Wraps an `httpmock::MockServer` and asserts, at drop, that every mock
/// registered through it was actually served at least once — unless the
/// call site explicitly declared the zero legitimate via
/// [`GuardedMockServer::mock_expect_zero`].
///
/// # Why this exists
///
/// httpmock serves the FIRST-REGISTERED mock whose predicate matches, not
/// the most specific one (#2541). A later mock whose predicate is fully
/// covered by an earlier one is silently unreachable — the loop never
/// advances past the point that earlier mock keeps answering, or (worse,
/// #2599) a mock whose whole job is a side-channel invariant check inside
/// its matcher closure never gets its matcher evaluated at all. Neither
/// failure mode announces itself: the test still runs to completion and
/// asserts on whatever state it produced, which can be internally
/// consistent and still be checking the wrong thing.
///
/// This type covers that defect CLASS structurally: any mock registered
/// through a `GuardedMockServer` is checked automatically, with no separate
/// call and no list to keep in sync, including mocks a helper function
/// registers on a test's behalf (the older `assert_every_mock_was_hit`
/// per-test opt-in helper this superseded is gone — see #2599's PR
/// history for its retirement, once `GuardedMockServer` covered its one
/// remaining caller for free).
///
/// # The exemption path is order-enforced, not just counted
///
/// `mock_expect_zero` (below) does NOT, on its own, prove a mock's matcher
/// was ever evaluated — a hits-based check structurally cannot distinguish
/// "never served because the predicate is legitimately false" from "never
/// served because an earlier mock shadowed it and its matcher was never
/// consulted at all." That second shape is exactly what happened to
/// `checkpoint_regression_tests::
/// a_salvage_after_a_checkpoint_never_leaves_two_assistant_messages_adjacent`'s
/// detector for as long as it was registered after the two mocks whose
/// predicates already covered every request (fixed by moving it first).
///
/// A true evaluation probe — instrument the matcher itself and assert it
/// was CALLED, not just that it never won — was considered and rejected as
/// unbuildable in a form stronger than what ordering already gives: httpmock
/// stores mocks in a `BTreeMap<usize, _>` keyed by a monotonically
/// increasing registration id and matches by iterating it in ascending
/// order (`mocks.values().find(...)`, `httpmock` 0.7's
/// `server/web/handlers.rs`) — strict first-registered-wins with NO API to
/// retroactively give a new registration a lower id than one that already
/// exists. A probe minted at `mock_expect_zero`-call time can therefore
/// only ever outrank mocks that do not yet exist; the only way to
/// guarantee it (or the exempted mock itself) is ever reachable is to
/// register it before anything that could shadow it — which is the SAME
/// mechanism as the ordering rule below, just paid for with a parallel
/// bookkeeping structure instead of a single `Cell<bool>`. So this type
/// enforces the ordering directly: `mock_expect_zero` panics IMMEDIATELY,
/// at registration, naming both call sites, if any real `.mock(...)` was
/// already registered on this server — the shadow shape becomes
/// unrepresentable rather than merely detected after the fact. Every
/// `mock_expect_zero` call also requires a written reason (a non-empty
/// `&'static str`), so the exemption documents itself at the call site
/// rather than living only in a doc comment nobody re-reads.
///
/// # Design notes
///
/// Deliberately does NOT `Deref` to the underlying `MockServer`. A
/// prototype that did left `MockServer::mock` reachable directly, which
/// lets a future test quietly opt back out of the guard (register through
/// the inner server and the wrapper never learns the mock exists). The one
/// thing tests need from the wrapped server directly — its base URL, to
/// point an `LmStudioClient` at it — is forwarded explicitly instead.
/// `runtime/clippy.toml`'s `disallowed-types` entry backs this up
/// structurally: a bare `httpmock::MockServer` fails to compile anywhere
/// outside this module.
///
/// The per-registration bookkeeping stores only a `usize` id and a
/// `&'static Location`, never an `httpmock::Mock<'_>` itself: a `Mock`
/// borrows from the `MockServer` it came from, and holding one inside this
/// struct alongside the `MockServer` it borrows from would be a
/// self-referential struct. httpmock sidesteps that for us on purpose —
/// `Mock::id` is a public field and `Mock::new(id, &MockServer)` is a
/// public constructor specifically so a caller can remember an id and
/// reconstruct a short-lived `Mock` handle later without holding the
/// original binding alive (see the upstream rationale linked from
/// `Mock`'s own doc comment). `Drop` below does exactly that: it never
/// touches the `Mock` values callers were handed back, only the ids it
/// kept.
pub struct GuardedMockServer {
    server: MockServer,
    registrations: std::cell::RefCell<Vec<Registration>>,
    /// Set the first time `.mock(...)` (a REAL, non-exempt registration)
    /// is called. `mock_expect_zero` refuses to register once this is
    /// true — see the module doc's "order-enforced" section.
    real_mock_registered: std::cell::Cell<bool>,
}

impl GuardedMockServer {
    pub fn start() -> Self {
        Self {
            server: MockServer::start(),
            registrations: std::cell::RefCell::new(Vec::new()),
            real_mock_registered: std::cell::Cell::new(false),
        }
    }

    /// The base URL of the wrapped server — enough to point an
    /// `LmStudioClient` (or any other HTTP client) at it. The only
    /// accessor forwarded; see the "does NOT `Deref`" note above.
    pub fn base_url(&self) -> String {
        self.server.base_url()
    }

    /// Registers a mock the run is expected to be served at least once.
    /// Panics at drop, naming this call site, if it never is.
    #[track_caller]
    pub fn mock<F>(&self, config_fn: F) -> Mock<'_>
    where
        F: FnOnce(When, Then),
    {
        self.real_mock_registered.set(true);
        self.register(false, config_fn)
    }

    /// Registers a mock that is legitimately never expected to be served:
    /// a deliberate "should not be reached" trap, one arm of a pair of
    /// mutually-exclusive predicates whose other branch this scripted run
    /// doesn't take, or an observe-only matcher whose real job happens
    /// inside the matcher closure rather than through being served.
    /// Exempted from the drop-time hits check; every other mock registered
    /// on this server is still checked.
    ///
    /// `reason` must be a non-empty, human-readable explanation of why
    /// zero hits is the correct outcome here — it is checked at
    /// registration, not just decorative.
    ///
    /// This does NOT, on its own, prove the mock's matcher was ever
    /// EVALUATED — and "it already self-asserts via `Mock::assert_hits(0)`"
    /// is not a safe reason to skip this. `Mock::assert_hits(0)` only
    /// proves the mock was never SERVED; it proves nothing about whether
    /// its matcher was ever even CONSULTED. A mock that does its real work
    /// inside the matcher closure (a side-channel counter, an invariant
    /// check) rather than through being served can be silently shadowed by
    /// an earlier mock whose predicate happens to cover every request —
    /// the matcher never runs, the counter never moves, and
    /// `assert_hits(0)` stays trivially true throughout regardless of
    /// whether the invariant it's meant to check actually holds. That is
    /// what the ordering rule below closes — not by detecting the shadow
    /// after the fact, but by making it unrepresentable.
    ///
    /// Panics IMMEDIATELY (not at drop) if any real `.mock(...)` was
    /// already registered on this server: httpmock's matching is strict
    /// first-registered-wins (see the module doc), so an expect-zero mock
    /// registered after a real one can be silently shadowed by it — which
    /// is the exact defect this type exists to prevent. Move every
    /// `mock_expect_zero` call ahead of every `mock` call on the same
    /// server.
    #[track_caller]
    pub fn mock_expect_zero<F>(&self, reason: &'static str, config_fn: F) -> Mock<'_>
    where
        F: FnOnce(When, Then),
    {
        assert!(
            !reason.trim().is_empty(),
            "mock_expect_zero(...) at {} requires a non-empty reason explaining why zero \
             hits is legitimate here",
            std::panic::Location::caller(),
        );
        assert!(
            !self.real_mock_registered.get(),
            "mock_expect_zero(...) at {} was registered AFTER a real .mock(...) already \
             exists on this server (#2599). httpmock serves the first-registered match — \
             an expect-zero mock registered after a real one can be silently shadowed by \
             it, and its matcher may never even be evaluated, which is exactly the defect \
             class this type exists to prevent. Move this mock_expect_zero(...) call to \
             before every .mock(...) call on this server.",
            std::panic::Location::caller(),
        );
        self.register(true, config_fn)
    }

    #[track_caller]
    fn register<F>(&self, expect_zero: bool, config_fn: F) -> Mock<'_>
    where
        F: FnOnce(When, Then),
    {
        let location = std::panic::Location::caller();
        let mock = self.server.mock(config_fn);
        self.registrations.borrow_mut().push(Registration { id: mock.id, location, expect_zero });
        mock
    }
}

impl Drop for GuardedMockServer {
    fn drop(&mut self) {
        // A test failing on its own assertion is already unwinding by the
        // time its GuardedMockServer values drop. Piling a second panic
        // on top would abort the process instead of reporting the test's
        // real failure — and the real failure is the more useful message
        // anyway, so stand down rather than compete with it.
        //
        // Correct behavior, with a consequence worth knowing (#2599
        // review): when a test fails for its OWN reason, this guard's
        // shadowing signal is suppressed right along with it — it only
        // reappears once that real failure is fixed. A shadowing bug this
        // guard would otherwise catch can therefore hide behind an
        // unrelated red test for as long as that test stays red. Don't
        // read "green after the real fix, with no guard complaint" as
        // proof there was never any shadowing to find — it's proof only
        // that THIS run's requests didn't trip it, same as any other pass.
        if std::thread::panicking() {
            return;
        }
        let unhit: Vec<&'static std::panic::Location<'static>> = self
            .registrations
            .borrow()
            .iter()
            .filter(|reg| !reg.expect_zero)
            .filter(|reg| Mock::new(reg.id, &self.server).hits() == 0)
            .map(|reg| reg.location)
            .collect();
        assert!(
            unhit.is_empty(),
            "{} mock(s) registered on this GuardedMockServer were never hit (#2599: a \
             broader mock registered earlier may be shadowing them — httpmock serves the \
             first-registered match, not the most specific one, so a later mock whose \
             predicate is fully covered by an earlier one is silently unreachable). If this \
             mock is a deliberate exception (a trap, an untaken branch, an observe-only \
             matcher), register it with `mock_expect_zero` instead of `mock` — BEFORE any \
             real `.mock(...)` call on this server. Registered at:\n{}",
            unhit.len(),
            unhit.iter().map(|l| format!("  - {l}")).collect::<Vec<_>>().join("\n"),
        );
    }
}

mod self_tests {
    use super::*;

    /// The straightforward case: every registered mock gets hit at least
    /// once, so drop is silent.
    #[test]
    fn a_server_where_every_mock_is_hit_drops_cleanly() {
        let server = GuardedMockServer::start();
        let _m = server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/ok");
            then.status(200);
        });
        let response = raw_http_get(&format!("{}/ok", server.base_url()));
        assert_eq!(response, 200);
        // Falls out of scope here; Drop must not panic.
    }

    /// A mock declared via `mock_expect_zero` — registered BEFORE the real
    /// mock below, as the ordering rule requires — and genuinely never hit
    /// does not trip the guard.
    #[test]
    fn an_expect_zero_mock_left_unhit_drops_cleanly() {
        let server = GuardedMockServer::start();
        let _trap = server.mock_expect_zero("deliberate trap; this self-test never sends a request that should reach it", |when, then| {
            when.method(httpmock::Method::GET).path("/should-not-be-reached");
            then.status(500);
        });
        let _live = server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/ok");
            then.status(200);
        });
        let response = raw_http_get(&format!("{}/ok", server.base_url()));
        assert_eq!(response, 200);
    }

    /// The case this whole module exists to catch: a broader mock
    /// registered first shadows a narrower one registered after it, and
    /// the test body has NO assertion at all that would otherwise catch
    /// the shadowed mock going unhit — the guard has to be the thing that
    /// fails this test, on its own, at teardown. Both mocks here are real
    /// (`.mock`, not `.mock_expect_zero`) — this is the BASE hits-check,
    /// independent of the exemption path below.
    #[test]
    #[should_panic(expected = "1 mock(s) registered on this GuardedMockServer were never hit")]
    fn a_shadowed_mock_with_no_explicit_check_fails_at_teardown() {
        let server = GuardedMockServer::start();
        // Registered FIRST with a predicate that matches every GET to
        // /ok — httpmock serves the first-registered match, so this mock
        // answers every request regardless of what `_shadowed` below
        // asks for.
        let _catch_all = server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/ok");
            then.status(200);
        });
        // Registered SECOND with a strictly narrower (more specific)
        // predicate. httpmock does not prefer the more specific match —
        // it never even gets a chance to be evaluated for a request
        // `_catch_all` already claimed.
        let _shadowed = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/ok")
                .header("x-only-if-not-shadowed", "true");
            then.status(200);
        });
        // No assertion of ANY kind in this test's body — not on `_shadowed`,
        // not even on the response the request actually got back. If this
        // test passes, it's not because anything here checked the
        // invariant; it's because nothing did. The guard alone has to be
        // what fails it, at drop.
        let _ = raw_http_get(&format!("{}/ok", server.base_url()));
    }

    /// (#2599 review) The exact regression the exemption's ordering rule
    /// exists to make unrepresentable: a broad real mock registered FIRST,
    /// then an attempt to declare a narrower mock expect-zero AFTER it.
    /// Under the old (pre-review) design this shape compiled, ran, and
    /// passed — silently, because a hits-based check cannot tell "the
    /// predicate is legitimately false" from "the predicate was shadowed
    /// and never evaluated." This is rows 3 and 4 of the review's table
    /// (detector registered last, with either the shipped or a mutated
    /// predicate): both now fail identically, at registration, before the
    /// predicate's own truth value can matter at all.
    #[test]
    #[should_panic(expected = "was registered AFTER a real .mock(...) already exists")]
    fn an_expect_zero_registered_after_a_real_mock_panics_at_registration() {
        let server = GuardedMockServer::start();
        // Registered first, matches every GET to /ok — the shape of
        // `_m1`/`_m2` in `a_salvage_after_a_checkpoint_never_leaves_two_
        // assistant_messages_adjacent` before the #2599 fix moved the
        // detector ahead of them.
        let _catch_all = server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/ok");
            then.status(200);
        });
        // This must never get far enough to matter — the assert inside
        // `mock_expect_zero` fires before httpmock even sees the
        // registration attempt. Whether this predicate is the "shipped"
        // one (always false) or a "mutated to always fire" one is
        // irrelevant to the outcome, which is the point: the old
        // hits()==0 check could only catch the mutated case (row 4) and
        // only by accident of what the test body happened to assert;
        // this fails BOTH rows the same way, for a reason that doesn't
        // depend on the predicate at all.
        let _would_be_shadowed = server.mock_expect_zero(
            "self-test: this call is expected to panic before it registers anything",
            |when, then| {
                when.method(httpmock::Method::GET)
                    .path("/ok")
                    .header("x-only-if-not-shadowed", "true");
                then.status(200);
            },
        );
    }

    /// `mock_expect_zero` requires a real, non-empty reason — not just a
    /// present-but-blank string — so the exemption can't be rubber-stamped
    /// with an empty literal to satisfy the type signature alone.
    #[test]
    #[should_panic(expected = "requires a non-empty reason")]
    fn an_empty_reason_is_rejected() {
        let server = GuardedMockServer::start();
        let _ = server.mock_expect_zero("   ", |when, then| {
            when.method(httpmock::Method::GET).path("/never");
            then.status(500);
        });
    }

    /// (#2599) An intermediate helper that itself calls `server.mock(...)`
    /// on a caller's behalf must be `#[track_caller]`, or every unhit-mock
    /// panic it causes reports THIS FUNCTION's own internal line —
    /// identical for every caller — instead of whichever test actually
    /// called it. This mirrors `register_three_turn_tool_then_stop_script`
    /// in `loop_runner.rs`, which has 3 real callers and needed exactly
    /// this fix.
    #[track_caller]
    fn register_an_unhit_mock_via_helper(server: &GuardedMockServer) {
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/never-called-via-helper");
            then.status(200);
        });
    }

    /// Two servers, two calls to the SAME `#[track_caller]` helper above,
    /// from two DIFFERENT lines in this test's own body. Each server's
    /// Drop-time panic must name ITS OWN call site here — not the
    /// helper's internal `server.mock(...)` line (identical for both
    /// calls) and not the OTHER call's line — proving `#[track_caller]`
    /// really does propagate through an intermediate function rather than
    /// stopping at it.
    ///
    /// `#[serial_test::serial]`: this test installs a process-global panic
    /// hook (the only way to reliably read a panic's fully-formatted
    /// "at <file>:<line>:<col>: <message>" text back out — `assert!`'s
    /// panic payload type is an internal `core::panicking` formatting
    /// type, not `String`, so downcasting the caught payload silently
    /// fails). A global hook installed while other tests panic
    /// concurrently could race; serializing against every other
    /// `#[serial_test::serial]` test in the crate avoids that.
    #[test]
    #[serial_test::serial]
    fn a_helper_marked_track_caller_reports_the_calling_tests_own_line() {
        let server_a = GuardedMockServer::start();
        // The call and the `line!()` capture MUST sit on the exact same
        // physical line — `line!()` reports its OWN line, and it is only
        // useful here because that line is also the call site
        // `#[track_caller]` will report.
        let call_a_line = { register_an_unhit_mock_via_helper(&server_a); line!() };
        let msg_a = capture_panic_message(std::panic::AssertUnwindSafe(|| drop(server_a)));

        let server_b = GuardedMockServer::start();
        let call_b_line = { register_an_unhit_mock_via_helper(&server_b); line!() };
        let msg_b = capture_panic_message(std::panic::AssertUnwindSafe(|| drop(server_b)));

        assert_ne!(
            call_a_line, call_b_line,
            "the two calls must be on different lines for this test to prove anything"
        );
        let file = std::path::Path::new(file!())
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("test_support.rs");
        let site_a = format!("{file}:{call_a_line}:");
        let site_b = format!("{file}:{call_b_line}:");
        assert!(
            msg_a.contains(&site_a),
            "server A's panic must name ITS OWN call site ({site_a}), not the helper's \
             internal line — got: {msg_a}"
        );
        assert!(
            !msg_a.contains(&site_b),
            "server A's panic must NOT name server B's call site ({site_b}) — got: {msg_a}"
        );
        assert!(
            msg_b.contains(&site_b),
            "server B's panic must name ITS OWN call site ({site_b}), not the helper's \
             internal line — got: {msg_b}"
        );
        assert!(
            !msg_b.contains(&site_a),
            "server B's panic must NOT name server A's call site ({site_a}) — got: {msg_b}"
        );
    }

    /// Runs `f`, expecting it to panic, and returns the panic's full
    /// formatted text (`"thread '...' panicked at <file>:<line>:<col>:\n
    /// <message>"`) via a temporary panic hook — more reliable than
    /// downcasting the caught payload, whose concrete type
    /// (`core::panicking`'s internal formatting wrapper for `assert!`'s
    /// multi-argument messages) isn't `String` or `&str` and so can't be
    /// recovered by `downcast_ref` at all.
    fn capture_panic_message(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
        use std::sync::{Arc, Mutex};
        let captured = Arc::new(Mutex::new(String::new()));
        let captured_in_hook = Arc::clone(&captured);
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            *captured_in_hook.lock().unwrap() = info.to_string();
        }));
        let result = std::panic::catch_unwind(f);
        std::panic::set_hook(previous_hook);
        assert!(result.is_err(), "expected a panic, but the closure returned normally");
        let msg = captured.lock().unwrap().clone();
        assert!(!msg.is_empty(), "the panic hook never fired — nothing was captured");
        msg
    }

    /// A minimal HTTP GET without pulling in the crate's own async HTTP
    /// client machinery — these self-tests only need to prove the guard's
    /// own bookkeeping, not exercise `LmStudioClient`.
    fn raw_http_get(url: &str) -> u16 {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        let after_scheme = url.trim_start_matches("http://");
        let (host_port, path) = after_scheme.split_once('/').unwrap_or((after_scheme, ""));
        let mut stream = TcpStream::connect(host_port).expect("connect to mock server");
        let request =
            format!("GET /{path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).expect("write request");
        let mut resp = String::new();
        stream.read_to_string(&mut resp).expect("read response");
        // Status line looks like "HTTP/1.1 200 OK\r\n...".
        resp.split_whitespace()
            .nth(1)
            .and_then(|code| code.parse::<u16>().ok())
            .expect("parse status code")
    }
}
