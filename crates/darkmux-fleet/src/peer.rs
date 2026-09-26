//! The ONE place the fleet token is attached to an outgoing request
//! (#2916 re-review MUST 3).
//!
//! Every token-bearing request to another machine (work submission,
//! `machine status`/`resources <id>`, `machine list --deep`, the daemon's
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
    /// The verified address every connection is made to; `None` only for a
    /// loopback target (this machine).
    pub pinned_ip: Option<IpAddr>,
    /// The node id pinned by THIS lookup (first contact), for the caller to
    /// persist; `None` when the entry was already pinned or is loopback.
    pub newly_pinned: Option<String>,
}

impl PeerTarget {
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

/// Resolve where a token-bearing request for roster entry `name` may go.
///
/// - `local_addr: Some(addr)`: the entry is THIS machine; dial its own
///   daemon at `addr` (loopback), no verification needed.
/// - An entry whose address is loopback (a same-host test fleet): dialed as
///   written, since it can only reach this machine, when `loopback_ok`.
///   Work submission passes `false`: it verifies every address.
/// - Anything else must pass [`crate::verify_target`] with `provider`, and
///   is dialed at the verified IP. `port` overrides the address's own port
///   (work submission uses the fleet listener's port).
pub fn peer_target(
    name: &str,
    entry: &MachineEntry,
    local_addr: Option<&str>,
    port: Option<u16>,
    default_port: u16,
    loopback_ok: bool,
    provider: &dyn IdentityProvider,
) -> Result<PeerTarget> {
    if let Some(addr) = local_addr {
        let (scheme, host, p) = split_address(addr, default_port)?;
        let ip = host.parse::<IpAddr>().ok();
        if !ip.is_some_and(|ip| ip.is_loopback()) && host != "localhost" {
            return Err(anyhow!("this machine's own daemon address `{addr}` is not loopback"));
        }
        return Ok(PeerTarget { scheme, host, port: port.unwrap_or(p), pinned_ip: None, newly_pinned: None });
    }
    let (scheme, host, p) = split_address(&entry.address, default_port)?;
    if loopback_ok && crate::roster::address_host_is_loopback(&entry.address) {
        return Ok(PeerTarget { scheme, host, port: port.unwrap_or(p), pinned_ip: None, newly_pinned: None });
    }
    let v = crate::submission::verify_target(name, entry, provider)?;
    Ok(PeerTarget {
        scheme,
        host,
        port: port.unwrap_or(p),
        pinned_ip: Some(v.ip),
        newly_pinned: v.newly_pinned.then(|| v.node.node_id.clone()),
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

fn agent_for(target: &PeerTarget, timeout: Duration) -> ureq::Agent {
    let mut b = ureq::AgentBuilder::new().timeout(timeout).redirects(0);
    if let Some(ip) = target.pinned_ip {
        // Dial the VERIFIED address, whatever DNS says now.
        b = b.resolver(move |netloc: &str| -> std::io::Result<Vec<SocketAddr>> {
            let port = netloc.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok()).unwrap_or(80);
            Ok(vec![SocketAddr::new(ip, port)])
        });
    }
    b.build()
}

/// The fleet token, attached HERE and nowhere else.
fn with_fleet_token(req: ureq::Request, token: Option<&str>) -> ureq::Request {
    match token {
        Some(t) => req.set("Authorization", &format!("Bearer {t}")),
        None => req,
    }
}

/// GET `path` from a verified target with the fleet token (when this
/// machine has one) and `headers`.
pub fn fleet_get(
    target: &PeerTarget,
    path: &str,
    timeout: Duration,
    headers: &[(&str, &str)],
) -> std::result::Result<ureq::Response, ureq::Error> {
    let token = darkmux_flow::serve_token();
    let mut req = agent_for(target, timeout).get(&format!("{}{path}", target.base()));
    for (k, v) in headers {
        req = req.set(k, v);
    }
    with_fleet_token(req, token.as_ref().map(|t| t.expose_for_compare())).call()
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
    let agent = {
        let mut b = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(read_timeout)
            .timeout_write(Duration::from_secs(30))
            .redirects(0);
        if let Some(ip) = target.pinned_ip {
            b = b.resolver(move |netloc: &str| -> std::io::Result<Vec<SocketAddr>> {
                let port = netloc.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok()).unwrap_or(80);
                Ok(vec![SocketAddr::new(ip, port)])
            });
        }
        b.build()
    };
    let req = agent
        .post(&format!("{}{path}", target.base()))
        .set("Content-Type", "application/json");
    with_fleet_token(req, token).send_string(body)
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

/// Tests only: a target at `base` (`http://host:port`), unverified.
#[cfg(any(test, feature = "test-support"))]
pub fn unverified_target_for_test(url_base: &str) -> PeerTarget {
    let (scheme, host, port) = split_address(url_base, 80).unwrap();
    PeerTarget { scheme, host, port, pinned_ip: None, newly_pinned: None }
}

#[cfg(test)]
mod tests {
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
        let t = peer_target("studio", &entry("100.64.0.2", Some("nSTUDIO")), None, None, 8765, true, &p).unwrap();
        assert_eq!(t.pinned_ip, Some("100.64.0.2".parse().unwrap()));
        assert!(peer_target("studio", &entry("192.168.1.9", None), None, None, 8765, true, &p).is_err(), "a LAN address is refused");
        assert!(peer_target("studio", &entry("100.64.0.2", Some("nOTHER")), None, None, 8765, true, &p).is_err(), "a different node is refused");
        let lo = peer_target("studio", &entry("127.0.0.1:18765", None), None, None, 8765, true, &p).unwrap();
        assert_eq!(lo.pinned_ip, None);
        let me = peer_target("studio", &entry("100.64.0.2", None), Some("127.0.0.1:8765"), None, 8765, true, &p).unwrap();
        assert_eq!(me.base(), "http://127.0.0.1:8765");
        assert!(peer_target("studio", &entry("x", None), Some("100.64.0.9:8765"), None, 8765, true, &p).is_err());
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
        };
        let body = fleet_get(&t, "/x", Duration::from_secs(5), &[]).unwrap().into_string().unwrap();
        assert_eq!(body, "ok");
    }
}
