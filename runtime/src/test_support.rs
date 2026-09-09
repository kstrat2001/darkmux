//! Test-only helpers shared across `loop_runner`'s tests, its
//! `checkpoint_regression_tests` submodule, and `compaction`'s tests.
//!
//! This module exists for one reason (#2599): make httpmock's
//! first-registered-wins shadowing self-detecting at the type level,
//! instead of relying on each test author to remember to check.
#![cfg(test)]

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
/// `assert_every_mock_was_hit` (`loop_runner::tests`) covers this on a
/// per-test, opt-in basis — a test author has to remember to call it and
/// to name every mock. This type covers the same defect CLASS
/// structurally: any mock registered through a `GuardedMockServer` is
/// checked automatically, with no separate call and no list to keep in
/// sync, including mocks a helper function registers on a test's behalf.
///
/// # Design notes
///
/// Deliberately does NOT `Deref` to the underlying `MockServer`. A
/// prototype that did left `MockServer::mock` reachable directly, which
/// lets a future test quietly opt back out of the guard (register through
/// the inner server and the wrapper never learns the mock exists). The one
/// thing tests need from the wrapped server directly — its base URL, to
/// point an `LmStudioClient` at it — is forwarded explicitly instead.
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
}

impl GuardedMockServer {
    pub fn start() -> Self {
        Self { server: MockServer::start(), registrations: std::cell::RefCell::new(Vec::new()) }
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
        self.register(false, config_fn)
    }

    /// Registers a mock that is legitimately never expected to be served:
    /// a deliberate "should not be reached" trap, one arm of a pair of
    /// mutually-exclusive predicates whose other branch this scripted run
    /// doesn't take, or an observe-only matcher whose real job happens
    /// inside the matcher closure rather than through being served.
    /// Exempted from the drop-time check; every other mock registered on
    /// this server is still checked.
    #[track_caller]
    pub fn mock_expect_zero<F>(&self, config_fn: F) -> Mock<'_>
    where
        F: FnOnce(When, Then),
    {
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
             matcher), register it with `mock_expect_zero` instead of `mock`. Registered at:\n{}",
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

    /// A mock declared via `mock_expect_zero` and genuinely never hit does
    /// not trip the guard.
    #[test]
    fn an_expect_zero_mock_left_unhit_drops_cleanly() {
        let server = GuardedMockServer::start();
        let _trap = server.mock_expect_zero(|when, then| {
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
    /// fails this test, on its own, at teardown.
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
