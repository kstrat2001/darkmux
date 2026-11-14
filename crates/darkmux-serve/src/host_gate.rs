//! The viewer daemon answers only a `Host` that names this machine.
//!
//! A page on an attacker's domain can have its name re-resolved to
//! 127.0.0.1 (DNS rebinding): the browser then sends that page's requests to
//! this daemon as same-origin, carrying the attacker's name in `Host`. With
//! read auth off (the default), such a request is served like a tailnet
//! peer's, so the page could read flow records and panels. The fix every
//! local server uses: refuse a `Host` this machine does not answer to.
//!
//! A `Host` is accepted when it is an IP literal (any address: the tailnet,
//! the LAN, loopback), `localhost`, one of this machine's own names (its OS
//! host name, with and without `.local`, and its overlay node's short and
//! full DNS names), or the host of a roster address. A request with no
//! `Host` passes: a browser always sends one, so the attack cannot omit it.
//! Everything else is refused with `421 Misdirected Request`, before any
//! route or the read-auth gate runs.
//!
//! The names are read once and kept. A name not in the kept set re-reads
//! them, at most once per [`REFRESH_AFTER`], so a rename or a roster edit is
//! picked up without letting a stream of foreign names spawn a provider
//! call each. While the last read had no answer from the overlay network
//! (still starting after a boot), the bound is [`RETRY_INCOMPLETE_AFTER`],
//! so the tailnet name is not refused for a whole interval.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// How long the kept names stand before a `Host` they lack re-reads them.
const REFRESH_AFTER: Duration = Duration::from_secs(30);

/// How soon a miss re-reads the names when the last read had no answer from
/// the overlay network (it is still starting after a boot).
const RETRY_INCOMPLETE_AFTER: Duration = Duration::from_secs(2);

/// Where the names come from, and whether the read was complete (the
/// overlay network answered). Blocking: the production source asks the
/// overlay network's tool and reads the roster.
type NameSource = dyn Fn() -> (HashSet<String>, bool) + Send + Sync;

/// The name a test router's `Host` uses for this machine's tailnet name.
#[cfg(test)]
pub(crate) const TEST_MACHINE_NAME: &str = "hub.tailnet-test.example";

/// This machine's names, read lazily and kept.
pub(crate) struct HostNames {
    source: Box<NameSource>,
    /// The kept names and when they were read; `None` until first needed.
    known: Mutex<Option<Kept>>,
    /// Held across a re-read, so concurrent misses read once.
    reading: tokio::sync::Mutex<()>,
}

impl HostNames {
    /// The daemon's: this machine's OS host name, its overlay node and the
    /// roster's hosts.
    pub(crate) fn production() -> Self {
        Self::from_source(Box::new(|| {
            let node = darkmux_fleet::configured_provider_or_unavailable()
                .local_node()
                .ok();
            let roster: Vec<String> = darkmux_fleet::load_roster()
                .map(|r| r.machines.into_values().map(|m| m.address).collect())
                .unwrap_or_default();
            let complete = node.is_some();
            (
                names_of(os_hostname().as_deref(), node.as_ref(), &roster),
                complete,
            )
        }))
    }

    /// A fixed set, for tests.
    #[cfg(test)]
    pub(crate) fn fixed(names: &[&str]) -> Self {
        let set: HashSet<String> = names.iter().map(|n| normalize(n)).collect();
        Self::from_source(Box::new(move || (set.clone(), true)))
    }

    fn from_source(source: Box<NameSource>) -> Self {
        Self {
            source,
            known: Mutex::new(None),
            reading: tokio::sync::Mutex::new(()),
        }
    }

    /// The kept names; with `current`, only while they are too young to
    /// re-read.
    fn kept(&self, current: bool) -> Option<Arc<HashSet<String>>> {
        let guard = self.known.lock().unwrap_or_else(|p| p.into_inner());
        let kept = guard.as_ref()?;
        let interval = if kept.complete {
            REFRESH_AFTER
        } else {
            RETRY_INCOMPLETE_AFTER
        };
        (!current || kept.at.elapsed() < interval).then(|| kept.names.clone())
    }

    /// Whether `name` (already normalized) is one of this machine's.
    async fn answers_to(self: &Arc<Self>, name: &str) -> bool {
        if self.kept(false).is_some_and(|n| n.contains(name)) {
            return true;
        }
        let _reading = self.reading.lock().await;
        // A concurrent miss may have read them while this one waited.
        if let Some(names) = self.kept(true) {
            return names.contains(name);
        }
        let me = self.clone();
        let Ok((names, complete)) = tokio::task::spawn_blocking(move || (me.source)()).await else {
            return false;
        };
        let names = Arc::new(names);
        *self.known.lock().unwrap_or_else(|p| p.into_inner()) = Some(Kept {
            at: Instant::now(),
            names: names.clone(),
            complete,
        });
        names.contains(name)
    }
}

/// One read of the names.
struct Kept {
    at: Instant,
    names: Arc<HashSet<String>>,
    /// The overlay network answered this read. While it has not, a miss
    /// re-reads after [`RETRY_INCOMPLETE_AFTER`] instead of [`REFRESH_AFTER`].
    complete: bool,
}

/// A host name as compared: lowercase, no trailing dot.
fn normalize(name: &str) -> String {
    name.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// The names `os_host`, `node` and the roster `addresses` give this machine:
/// the OS host name, its first label and that label with `.local`; the
/// node's short and full DNS names; each roster address's host. IP literals
/// are left out (every one is accepted anyway).
pub(crate) fn names_of(
    os_host: Option<&str>,
    node: Option<&darkmux_fleet::NodeIdentity>,
    addresses: &[String],
) -> HashSet<String> {
    let mut out = HashSet::new();
    if let Some(host) = os_host.map(normalize).filter(|h| !h.is_empty()) {
        let label = host.split('.').next().unwrap_or(&host).to_string();
        out.insert(format!("{label}.local"));
        out.insert(label);
        out.insert(host);
    }
    if let Some(node) = node {
        out.insert(normalize(&node.name));
        out.extend(node.dns_name.as_deref().map(normalize));
    }
    out.extend(
        addresses
            .iter()
            .filter_map(|a| darkmux_fleet::address_host(a))
            .map(|h| normalize(&h)),
    );
    out.retain(|n| !n.is_empty() && n.parse::<std::net::IpAddr>().is_err());
    out
}

/// The kernel's host name.
#[cfg(unix)]
fn os_hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for `len` bytes; gethostname writes at most
    // that many and the result is read only up to its first NUL.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    std::str::from_utf8(&buf[..end])
        .ok()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

#[cfg(not(unix))]
fn os_hostname() -> Option<String> {
    None
}

/// What a request's `Host` says, as far as the gate cares.
#[derive(Debug, PartialEq, Eq)]
enum HostClaim {
    /// No `Host` at all: not something a browser sends.
    Absent,
    /// An IP literal or `localhost`.
    Address,
    /// A name, normalized, to look up.
    Name(String),
    /// More than one `Host`, or one that does not parse.
    Malformed,
}

fn claim_of(headers: &axum::http::HeaderMap) -> HostClaim {
    let mut values = headers.get_all(axum::http::header::HOST).iter();
    let Some(value) = values.next() else {
        return HostClaim::Absent;
    };
    if values.next().is_some() {
        return HostClaim::Malformed;
    }
    value
        .to_str()
        .map_or(HostClaim::Malformed, |raw| claim_of_value(raw.trim()))
}

/// One `Host` value read: `[v6]` or `[v6]:port`, else `name` or `name:port`.
fn claim_of_value(raw: &str) -> HostClaim {
    if let Some(rest) = raw.strip_prefix('[') {
        return if bracketed_v6(rest) {
            HostClaim::Address
        } else {
            HostClaim::Malformed
        };
    }
    let (name, port) = match raw.rsplit_once(':') {
        Some((n, p)) => (n, Some(p)),
        None => (raw, None),
    };
    if port.is_some_and(|p| p.parse::<u16>().is_err()) || name.is_empty() {
        return HostClaim::Malformed;
    }
    let name = normalize(name);
    if name == "localhost" || name.parse::<std::net::IpAddr>().is_ok() {
        return HostClaim::Address;
    }
    HostClaim::Name(name)
}

/// Whether `rest` (after the `[`) is `v6]` or `v6]:port`.
fn bracketed_v6(rest: &str) -> bool {
    rest.split_once(']').is_some_and(|(ip, port)| {
        ip.parse::<std::net::Ipv6Addr>().is_ok()
            && (port.is_empty()
                || port
                    .strip_prefix(':')
                    .is_some_and(|p| p.parse::<u16>().is_ok()))
    })
}

/// The gate itself: every request but one whose `Host` names this machine
/// is refused.
pub(crate) async fn host_gate_mw(
    State(names): State<Arc<HostNames>>,
    req: Request,
    next: Next,
) -> Response {
    let allowed = match claim_of(req.headers()) {
        HostClaim::Absent | HostClaim::Address => true,
        HostClaim::Name(name) => names.answers_to(&name).await,
        HostClaim::Malformed => false,
    };
    if allowed {
        next.run(req).await
    } else {
        refused()
    }
}

fn refused() -> Response {
    (
        StatusCode::MISDIRECTED_REQUEST,
        "darkmux serve answers only a Host that names this machine: localhost, an IP address, \
this machine's host or tailnet name, or a fleet roster address. Open the viewer at one of those.\n",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(hosts: &[&str]) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        for v in hosts {
            h.append(axum::http::header::HOST, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn a_host_is_read_as_an_address_a_name_or_malformed() {
        for addr in [
            "localhost",
            "LOCALHOST:8765",
            "127.0.0.1",
            "127.0.0.1:8765",
            "[::1]",
            "[::1]:8765",
            "100.64.0.7:8090",
            "192.168.1.20",
        ] {
            assert_eq!(claim_of(&headers(&[addr])), HostClaim::Address, "{addr}");
        }
        assert_eq!(
            claim_of(&headers(&["Studio.Tailnet.ts.net.:8090"])),
            HostClaim::Name("studio.tailnet.ts.net".into())
        );
        assert_eq!(
            claim_of(&headers(&["evil.example"])),
            HostClaim::Name("evil.example".into())
        );
        assert_eq!(claim_of(&headers(&[])), HostClaim::Absent);
        for bad in ["evil.example:http", ":8765", "[::1", "[nope]:1"] {
            assert_eq!(claim_of(&headers(&[bad])), HostClaim::Malformed, "{bad}");
        }
        assert_eq!(
            claim_of(&headers(&["localhost", "evil.example"])),
            HostClaim::Malformed,
            "two Hosts"
        );
    }

    #[test]
    fn this_machines_names_come_from_its_host_name_its_node_and_the_roster() {
        let node = darkmux_fleet::test_node("nLAPTOP", "laptop", "100.64.0.1");
        let node = darkmux_fleet::NodeIdentity {
            dns_name: Some("laptop.tailnet-example.ts.net".into()),
            ..node
        };
        let roster = vec![
            "studio.tailnet-example.ts.net:8765".to_string(),
            "http://Mini:9000/".to_string(),
            "100.64.0.9:8765".to_string(),
        ];
        let names = names_of(Some("Kains-MacBook-Pro.local"), Some(&node), &roster);
        for want in [
            "kains-macbook-pro.local",
            "kains-macbook-pro",
            "laptop",
            "laptop.tailnet-example.ts.net",
            "studio.tailnet-example.ts.net",
            "mini",
        ] {
            assert!(names.contains(want), "{want} in {names:?}");
        }
        assert!(
            !names.iter().any(|n| n.parse::<std::net::IpAddr>().is_ok()),
            "no IP literal is kept: {names:?}"
        );
        assert!(names_of(None, None, &[]).is_empty());
    }

    #[tokio::test]
    async fn a_name_missing_from_the_kept_set_re_reads_it_at_most_once_per_interval() {
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r = reads.clone();
        let names = Arc::new(HostNames::from_source(Box::new(move || {
            r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (["laptop".to_string()].into_iter().collect(), true)
        })));
        assert!(names.answers_to("laptop").await);
        assert!(!names.answers_to("evil.example").await);
        assert!(!names.answers_to("evil2.example").await);
        assert!(names.answers_to("laptop").await);
        assert_eq!(
            reads.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "foreign names inside the interval read nothing"
        );
    }

    /// After a boot the overlay network may not answer the first read, and a
    /// phone on the tailnet name would then be refused for a whole interval.
    /// While the last read had no overlay answer, a miss re-reads after a
    /// short bound; once a read is complete the normal interval applies.
    #[tokio::test]
    async fn an_incomplete_read_is_retried_soon_until_the_overlay_answers() {
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r = reads.clone();
        let names = Arc::new(HostNames::from_source(Box::new(move || {
            let n = r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                (HashSet::new(), false)
            } else {
                (["laptop".to_string()].into_iter().collect(), true)
            }
        })));
        let count = || reads.load(std::sync::atomic::Ordering::SeqCst);
        let age = |by: Duration| {
            let mut g = names.known.lock().unwrap();
            let kept = g.as_mut().unwrap();
            kept.at = Instant::now().checked_sub(by).unwrap();
        };
        assert!(
            !names.answers_to("laptop").await,
            "the overlay did not answer the first read"
        );
        assert!(!names.answers_to("laptop").await);
        assert_eq!(count(), 1, "the retry is bounded");
        age(RETRY_INCOMPLETE_AFTER + Duration::from_millis(100));
        assert!(
            names.answers_to("laptop").await,
            "a short wait later the miss re-reads"
        );
        assert_eq!(count(), 2);
        age(RETRY_INCOMPLETE_AFTER + Duration::from_millis(100));
        assert!(!names.answers_to("evil.example").await);
        assert_eq!(count(), 2, "a complete read keeps the normal interval");
    }
}
