//! The ONE place the fleet token is attached to an outgoing request
//! (#2916 re-review MUST 3).
//!
//! Every token-bearing request to another machine (work submission,
//! `machine status`/`resources <id>`, `machine list`, the daemon's
//! peer mission-graph proxy) goes through [`peer_target`] + [`fleet_get`] /
//! [`fleet_post_json`]. A target is either this machine's own daemon (a
//! loopback address: the token is already on this machine) or a roster
//! entry VERIFIED by the identity provider ([`crate::verify_target`]): its
//! address resolves to an overlay node, and that node is the one pinned for
//! the entry. The request then dials the verified IP (the name is kept for
//! the Host header, but DNS is not asked again), so a changed DNS answer
//! or a LAN impostor never receives the token. Redirects are never followed.
//!
//! `peer_token_conformance` (this crate's tests) fails if anything else in
//! the workspace reads the fleet token to send it.
//!
//! The functions return `ureq::Error` unboxed on purpose (callers match on
//! its status), hence the `result_large_err` allowance.
#![allow(clippy::result_large_err)]

use crate::identity::IdentityProvider;
use crate::roster::MachineEntry;
use anyhow::{anyhow, Result};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// Where a token-bearing request may go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerTarget {
    pub scheme: String,
    /// The host name used in the URL and the Host header.
    pub host: String,
    pub port: u16,
    /// The verified address every connection is made to. `None` only for a
    /// loopback target (this machine), and a target without one NEVER gets
    /// the fleet token (#2916 round 3 C1): a port squatter on 127.0.0.1 (a
    /// process of another user, a sandboxed app, the far end of an ssh
    /// tunnel) must not collect it, and this machine's own daemon exempts
    /// loopback callers anyway. Reaching a peer through a loopback tunnel
    /// with the token would need an explicit opt-in; there is none.
    pub pinned_ip: Option<IpAddr>,
    /// The node id pinned by THIS lookup (first contact), for the caller to
    /// persist; `None` when the entry was already pinned or is loopback.
    pub newly_pinned: Option<String>,
    /// The node the identity provider named at `pinned_ip` and that passed
    /// the roster's pin; `None` for a target no node was verified behind (a
    /// loopback one). A reader that asks "is this machine's own node behind
    /// this target" compares it to the provider's own node.
    pub node_id: Option<String>,
}

/// Why a roster entry is not a target this machine may send to. Typed so a
/// reader can name the remedy for each: a DNS failure, a network tool that is
/// down and an address that is not a node call for different fixes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetError {
    /// The address names no host, or has a bad port.
    BadAddress { detail: String },
    /// The address did not resolve to any IP.
    DoesNotResolve { target: String, address: String },
    /// The identity provider could not answer for the address.
    IdentityUnavailable { target: String, provider: String, detail: String },
    /// The address is not a node on the provider's network.
    NotOnOverlay { target: String, address: String, provider: String },
    /// The node at the address is not the one the roster pinned.
    PinMismatch { target: String, node_name: String },
    /// This machine's own daemon address is neither loopback nor one of its
    /// own overlay addresses.
    OwnAddress { address: String },
}

/// Which way a roster address failed, without the names. The ONE place each
/// fault's remedy is worded ([`TargetFault::remedy`]): a sender's error
/// ([`TargetError`]'s text) and a reader of the fleet view (which has only the
/// typed reason) both print it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetFault {
    BadAddress,
    DoesNotResolve,
    IdentityUnavailable,
    NotOnOverlay,
    PinMismatch,
    OwnAddress,
}

impl TargetFault {
    /// What to do about this fault, for the roster entry `target`.
    pub fn remedy(self, target: &str) -> String {
        match self {
            TargetFault::BadAddress => format!(
                "Re-add {target} with a usable address: `darkmux machine add {target} --address <its tailnet DNS name>`."
            ),
            TargetFault::DoesNotResolve => {
                "Check the name (`darkmux machine list` shows the roster path) and this machine's DNS.".to_string()
            }
            TargetFault::IdentityUnavailable => "Is the network identity tool running and signed in on this \
                machine? `darkmux doctor` names its own error."
                .to_string(),
            TargetFault::NotOnOverlay => format!(
                "Point the entry at {target}'s tailnet DNS name: `darkmux machine add {target} --address <its tailnet DNS name>`."
            ),
            TargetFault::PinMismatch => format!(
                "If {target} really was replaced, re-pin it with `darkmux machine add {target} --address <its tailnet DNS name>`."
            ),
            TargetFault::OwnAddress => "Check `serve.bind`.".to_string(),
        }
    }
}

impl TargetError {
    pub fn fault(&self) -> TargetFault {
        match self {
            TargetError::BadAddress { .. } => TargetFault::BadAddress,
            TargetError::DoesNotResolve { .. } => TargetFault::DoesNotResolve,
            TargetError::IdentityUnavailable { .. } => TargetFault::IdentityUnavailable,
            TargetError::NotOnOverlay { .. } => TargetFault::NotOnOverlay,
            TargetError::PinMismatch { .. } => TargetFault::PinMismatch,
            TargetError::OwnAddress { .. } => TargetFault::OwnAddress,
        }
    }

    /// The roster entry the error is about, when it names one.
    fn target(&self) -> &str {
        match self {
            TargetError::DoesNotResolve { target, .. }
            | TargetError::IdentityUnavailable { target, .. }
            | TargetError::NotOnOverlay { target, .. }
            | TargetError::PinMismatch { target, .. } => target,
            TargetError::BadAddress { .. } | TargetError::OwnAddress { .. } => "<id>",
        }
    }
}

impl std::fmt::Display for TargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let remedy = self.fault().remedy(self.target());
        match self {
            TargetError::BadAddress { detail } => f.write_str(detail),
            TargetError::DoesNotResolve { target, address } => {
                write!(f, "the roster address for {target} (`{address}`) does not resolve; nothing was sent. {remedy}")
            }
            TargetError::IdentityUnavailable { target, provider, detail } => {
                write!(f, "cannot verify {target}'s address with {provider} ({detail}); nothing was sent. {remedy}")
            }
            TargetError::NotOnOverlay { target, address, provider } => write!(
                f,
                "the roster address for {target} (`{address}`) is not a node on the {provider} network, so nothing \
                 was sent to it (not the fleet token, not the request). {remedy}"
            ),
            TargetError::PinMismatch { target, node_name } => write!(
                f,
                "the node at {target}'s address (`{node_name}`) is not the one this roster pinned for {target}; \
                 nothing was sent. {remedy}"
            ),
            TargetError::OwnAddress { address } => write!(
                f,
                "this machine's own daemon address `{address}` is neither loopback nor one of this \
                 machine's tailnet addresses. {remedy}"
            ),
        }
    }
}

impl std::error::Error for TargetError {}

impl PeerTarget {
    /// The same verified node, dialed at its fleet listener: `port` is the
    /// listener's, and the scheme is plain `http` whatever the roster
    /// address wrote (the address names the viewer daemon, which may sit
    /// behind `tailscale serve` on https; the listener speaks plain http on
    /// the overlay address).
    pub fn at_listener(&self, port: u16) -> PeerTarget {
        PeerTarget { scheme: "http".to_string(), port, ..self.clone() }
    }

    /// The base URL (`scheme://host:port`).
    pub fn base(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        format!("{}://{host}:{}", self.scheme, self.port)
    }
}

/// Split a roster/daemon address into (scheme, host, port), with
/// `default_port` when none is written.
pub fn split_address(address: &str, default_port: u16) -> Result<(String, String, u16)> {
    let trimmed = address.trim().trim_end_matches('/');
    let (scheme, rest) = match trimmed.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        None => ("http".to_string(), trimmed),
    };
    let rest = rest.split('/').next().unwrap_or(rest);
    let host = crate::roster::address_host(rest).ok_or_else(|| anyhow!("address `{address}` names no host"))?;
    let port_str = if let Some(inner) = rest.strip_prefix('[') {
        inner.split_once(']').and_then(|(_, p)| p.strip_prefix(':')).map(str::to_string)
    } else if rest.parse::<IpAddr>().is_ok() {
        None
    } else {
        rest.rsplit_once(':').map(|(_, p)| p.to_string())
    };
    let default = if scheme == "https" { 443 } else { default_port };
    let port = match port_str {
        Some(p) => p.parse::<u16>().map_err(|_| anyhow!("address `{address}` has a bad port"))?,
        None => default,
    };
    Ok((scheme, host, port))
}

fn split_bad(address: &str, default_port: u16) -> std::result::Result<(String, String, u16), TargetError> {
    split_address(address, default_port).map_err(|e| TargetError::BadAddress { detail: format!("{e:#}") })
}

/// Where THIS machine's own daemon is: a loopback address needs no
/// verification (and gets no token); a non-loopback one (a daemon bound to
/// its tailnet address) must be one of this node's own overlay addresses as
/// the provider reports them, and is pinned to it.
pub fn local_daemon_target(
    addr: &str,
    default_port: u16,
    provider: &dyn IdentityProvider,
) -> std::result::Result<PeerTarget, TargetError> {
    let (scheme, host, port) = split_bad(addr, default_port)?;
    let ip = host.parse::<IpAddr>().ok().map(|i| i.to_canonical());
    if ip.is_some_and(|ip| ip.is_loopback()) || host == "localhost" {
        return Ok(PeerTarget { scheme, host, port, pinned_ip: None, newly_pinned: None, node_id: None });
    }
    let own = provider.local_node().map(|n| n.addresses).unwrap_or_default();
    match ip.filter(|ip| own.contains(ip)) {
        Some(ip) => Ok(PeerTarget { scheme, host, port, pinned_ip: Some(ip), newly_pinned: None, node_id: None }),
        None => Err(TargetError::OwnAddress { address: addr.to_string() }),
    }
}

/// Resolve where a token-bearing request for roster entry `name` may go.
///
/// - `local_addr: Some(addr)`: the entry is THIS machine; dial its own
///   daemon at `addr`. A loopback address needs no verification (and gets
///   no token); a non-loopback one (a daemon bound to its tailnet address)
///   must be one of THIS node's own overlay addresses as the provider
///   reports them, and is pinned to it.
/// - An entry whose address is loopback (a same-host test fleet): dialed as
///   written, since it can only reach this machine, when `loopback_ok`.
///   Work submission passes `false`: it verifies every address.
/// - Anything else must pass [`crate::verify_target`] with `provider`, and
///   is dialed at the verified IP. The port is the address's own, else
///   `default_port`; [`PeerTarget::at_listener`] re-aims the same verified
///   node at the fleet listener.
pub fn peer_target(
    name: &str,
    entry: &MachineEntry,
    local_addr: Option<&str>,
    default_port: u16,
    loopback_ok: bool,
    provider: &dyn IdentityProvider,
) -> std::result::Result<PeerTarget, TargetError> {
    if let Some(addr) = local_addr {
        return local_daemon_target(addr, default_port, provider);
    }
    let (scheme, host, port) = split_bad(&entry.address, default_port)?;
    if loopback_ok && crate::roster::address_host_is_loopback(&entry.address) {
        return Ok(PeerTarget { scheme, host, port, pinned_ip: None, newly_pinned: None, node_id: None });
    }
    let v = crate::submission::verify_target(name, entry, provider)?;
    Ok(PeerTarget {
        scheme,
        host,
        port,
        pinned_ip: Some(v.ip),
        newly_pinned: v.newly_pinned.then(|| v.node.node_id.clone()),
        node_id: Some(v.node.node_id),
    })
}

/// Persist a first-contact pin for roster entry `id`.
pub fn persist_pin(id: &str, target: &PeerTarget) -> Result<()> {
    let Some(node_id) = target.newly_pinned.clone() else { return Ok(()) };
    crate::mutate_roster(|r| {
        if let Some(e) = r.machines.get_mut(id) {
            e.node_id = Some(node_id);
        }
        Ok(())
    })
}

/// The ONE agent builder for token-bearing requests (#2916 round 3 C3):
/// redirects never followed, and a pinned target's every connection goes to
/// its verified address whatever DNS says now. The caller sets the time
/// bounds.
fn base_agent(target: &PeerTarget) -> ureq::AgentBuilder {
    let mut b = ureq::AgentBuilder::new().redirects(0);
    if let Some(ip) = target.pinned_ip {
        b = b.resolver(move |netloc: &str| -> std::io::Result<Vec<SocketAddr>> {
            let port = netloc.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok()).unwrap_or(80);
            Ok(vec![SocketAddr::new(ip, port)])
        });
    }
    b
}

/// The fleet token, attached HERE and nowhere else, and only to a target
/// pinned to a verified address (see [`PeerTarget::pinned_ip`]).
fn with_fleet_token(req: ureq::Request, target: &PeerTarget, token: Option<&str>) -> ureq::Request {
    match (token, target.pinned_ip) {
        (Some(t), Some(_)) => req.set("Authorization", &format!("Bearer {t}")),
        _ => req,
    }
}

/// GET `path` from a verified target with the fleet token (when this
/// machine has one) and `headers`. `timeout` is ONE bound on the whole
/// request (connect, sending and reading the answer), not a per-read wait.
pub fn fleet_get(
    target: &PeerTarget,
    path: &str,
    timeout: Duration,
    headers: &[(&str, &str)],
) -> std::result::Result<ureq::Response, ureq::Error> {
    let token = darkmux_flow::serve_token();
    let mut req = base_agent(target).build().get(&format!("{}{path}", target.base())).timeout(timeout);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    with_fleet_token(req, target, token.as_ref().map(|t| t.expose_for_compare())).call()
}

/// POST a JSON body to a verified target with the fleet token.
pub fn fleet_post_json(
    target: &PeerTarget,
    path: &str,
    body: &str,
    read_timeout: Duration,
) -> std::result::Result<ureq::Response, ureq::Error> {
    let token = darkmux_flow::serve_token();
    post_json_with(target, path, body, read_timeout, token.as_ref().map(|t| t.expose_for_compare()))
}

fn post_json_with(
    target: &PeerTarget,
    path: &str,
    body: &str,
    read_timeout: Duration,
    token: Option<&str>,
) -> std::result::Result<ureq::Response, ureq::Error> {
    let agent = base_agent(target)
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(read_timeout)
        .timeout_write(Duration::from_secs(30))
        .build();
    let req = agent
        .post(&format!("{}{path}", target.base()))
        .set("Content-Type", "application/json");
    with_fleet_token(req, target, token).send_string(body)
}

/// Tests only: POST with an explicit token to an unverified target.
#[cfg(any(test, feature = "test-support"))]
pub fn post_json_with_token_for_test(
    target: &PeerTarget,
    path: &str,
    body: &str,
    read_timeout: Duration,
    token: &str,
) -> std::result::Result<ureq::Response, ureq::Error> {
    post_json_with(target, path, body, read_timeout, Some(token))
}

/// Tests only: a target at `base` (`http://host:port`), not verified by
/// any provider, pinned to its host when that is an IP literal (so a test
/// talking to its own fixture listener still exercises the pinned path).
#[cfg(any(test, feature = "test-support"))]
pub fn unverified_target_for_test(url_base: &str) -> PeerTarget {
    let (scheme, host, port) = split_address(url_base, 80).unwrap();
    let pinned_ip = host.parse::<IpAddr>().ok();
    PeerTarget { scheme, host, port, pinned_ip, newly_pinned: None, node_id: None }
}

#[cfg(test)]
mod tests {
    /// A sender's error and a reader of the fleet view print the SAME remedy:
    /// the error's text carries exactly what `TargetFault::remedy` words.
    #[test]
    fn a_target_error_prints_its_faults_remedy() {
        let errors = [
            TargetError::DoesNotResolve { target: "studio".into(), address: "x".into() },
            TargetError::IdentityUnavailable { target: "studio".into(), provider: "p".into(), detail: "d".into() },
            TargetError::NotOnOverlay { target: "studio".into(), address: "x".into(), provider: "p".into() },
            TargetError::PinMismatch { target: "studio".into(), node_name: "n".into() },
        ];
        for e in errors {
            assert!(e.to_string().contains(&e.fault().remedy("studio")), "{e}");
        }
    }

    use super::*;
    use crate::identity::{test_node, StaticIdentityProvider};

    fn entry(address: &str, node: Option<&str>) -> MachineEntry {
        MachineEntry {
            id: "studio".into(),
            address: address.into(),
            description: None,
            added_unix_ms: 1,
            machine_uid: None,
            loopback_intended: false,
            node_id: node.map(str::to_string),
            extras: Default::default(),
        }
    }

    fn provider() -> StaticIdentityProvider {
        StaticIdentityProvider {
            local: test_node("nLAPTOP", "laptop", "100.64.0.7"),
            peers: vec![test_node("nSTUDIO", "studio", "100.64.0.2"), test_node("nLO", "lo", "127.0.0.1")],
            down: None,
        }
    }

    #[test]
    fn addresses_split_into_scheme_host_port() {
        assert_eq!(split_address("studio", 8765).unwrap(), ("http".into(), "studio".into(), 8765));
        assert_eq!(split_address("studio:9000", 8765).unwrap(), ("http".into(), "studio".into(), 9000));
        assert_eq!(split_address("https://studio.example/", 8765).unwrap(), ("https".into(), "studio.example".into(), 443));
        assert_eq!(split_address("fd7a::2", 8765).unwrap(), ("http".into(), "fd7a::2".into(), 8765));
        assert_eq!(split_address("[fd7a::2]:81", 8765).unwrap(), ("http".into(), "fd7a::2".into(), 81));
    }

    /// A non-loopback target must be a verified overlay node; loopback and
    /// this machine's own daemon need no verification.
    #[test]
    fn only_verified_or_loopback_targets_get_the_token() {
        let p = provider();
        let t = peer_target("studio", &entry("100.64.0.2", Some("nSTUDIO")), None, 8765, true, &p).unwrap();
        assert_eq!(t.pinned_ip, Some("100.64.0.2".parse().unwrap()));
        assert_eq!(t.node_id.as_deref(), Some("nSTUDIO"), "the verified node rides with the target");
        assert!(peer_target("studio", &entry("192.168.1.9", None), None, 8765, true, &p).is_err(), "a LAN address is refused");
        assert!(peer_target("studio", &entry("100.64.0.2", Some("nOTHER")), None, 8765, true, &p).is_err(), "a different node is refused");
        let lo = peer_target("studio", &entry("127.0.0.1:18765", None), None, 8765, true, &p).unwrap();
        assert_eq!(lo.pinned_ip, None);
        assert_eq!(lo.node_id, None, "no node stands behind an unverified loopback target");
        let me = peer_target("studio", &entry("100.64.0.2", None), Some("127.0.0.1:8765"), 8765, true, &p).unwrap();
        assert_eq!(me.base(), "http://127.0.0.1:8765");
        assert_eq!(me.pinned_ip, None, "loopback: no pin, so no token");
        // (#2916 round 3 C2) A daemon bound to this node's own tailnet
        // address is this machine, pinned to that address.
        let me_ts = peer_target("laptop", &entry("x", None), Some("100.64.0.7:8765"), 8765, true, &p).unwrap();
        assert_eq!(me_ts.pinned_ip, Some("100.64.0.7".parse().unwrap()));
        assert!(peer_target("studio", &entry("x", None), Some("100.64.0.9:8765"), 8765, true, &p).is_err());
    }

    /// A one-shot HTTP fixture that records the request it received.
    fn recording_fixture() -> (u16, std::sync::mpsc::Receiver<String>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut s, _)) = l.accept() {
                // Read the whole request (headers + Content-Length body).
                let mut got = Vec::new();
                let mut b = [0u8; 4096];
                s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                loop {
                    let n = s.read(&mut b).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(&b[..n]);
                    let text = String::from_utf8_lossy(&got).to_string();
                    if let Some(h) = text.find("\r\n\r\n") {
                        let len = text[..h]
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                            .unwrap_or(0);
                        if got.len() >= h + 4 + len {
                            break;
                        }
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&got).into_owned());
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            }
        });
        (port, rx)
    }

    /// (#2916 round 3 C1) The token rides only to a PINNED target; a
    /// loopback target (whoever squats the port) gets nothing.
    #[test]
    #[serial_test::serial]
    fn the_token_goes_only_to_a_pinned_target() {
        let prev = std::env::var("DARKMUX_SERVE_TOKEN").ok();
        unsafe { std::env::set_var("DARKMUX_SERVE_TOKEN", "tok-peer-test") };
        let run = |pinned: bool| {
            let (port, rx) = recording_fixture();
            let t = PeerTarget {
                scheme: "http".into(),
                host: "127.0.0.1".into(),
                port,
                pinned_ip: pinned.then(|| "127.0.0.1".parse().unwrap()),
                newly_pinned: None,
                node_id: None,
            };
            let _ = fleet_get(&t, "/x", Duration::from_secs(5), &[]);
            rx.recv_timeout(Duration::from_secs(5)).unwrap().to_ascii_lowercase()
        };
        let pinned = run(true);
        let loopback = run(false);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_SERVE_TOKEN", v),
                None => std::env::remove_var("DARKMUX_SERVE_TOKEN"),
            }
        }
        assert!(pinned.contains("authorization: bearer tok-peer-test"), "{pinned}");
        assert!(!loopback.contains("authorization"), "{loopback}");
    }

    /// (#2916 round 3 C3) The POST path dials the pinned address too.
    #[test]
    fn a_verified_post_is_dialed_at_its_pinned_address_not_by_dns() {
        let (port, rx) = recording_fixture();
        let t = PeerTarget {
            scheme: "http".into(),
            host: "does-not-resolve.invalid".into(),
            port,
            pinned_ip: Some("127.0.0.1".parse().unwrap()),
            newly_pinned: None,
            node_id: None,
        };
        let r = fleet_post_json(&t, "/fleet/work", "{}", Duration::from_secs(5));
        assert!(r.is_ok(), "{r:?}");
        assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap().starts_with("POST /fleet/work"));
    }

    /// (#2916 re-review C1) The request dials the VERIFIED address, never a
    /// fresh DNS answer: a host name that does not resolve at all still
    /// reaches the pinned IP.
    #[test]
    fn a_verified_target_is_dialed_at_its_pinned_address_not_by_dns() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut s, _)) = l.accept() {
                let mut b = [0u8; 1024];
                let _ = s.read(&mut b);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            }
        });
        let t = PeerTarget {
            scheme: "http".into(),
            host: "does-not-resolve.invalid".into(),
            port,
            pinned_ip: Some("127.0.0.1".parse().unwrap()),
            newly_pinned: None,
            node_id: None,
        };
        let body = fleet_get(&t, "/x", Duration::from_secs(5), &[]).unwrap().into_string().unwrap();
        assert_eq!(body, "ok");
    }
}
