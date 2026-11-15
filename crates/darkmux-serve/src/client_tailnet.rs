//! (#3560, part 2 of 4) Requests that reach `client serve` through its
//! tailnet entry ([`crate::client_remote`] publishes it).
//!
//! `tailscale serve` terminates TLS and proxies each request to
//! `127.0.0.1:<port>`, so it arrives from loopback, and in Tailscale
//! 1.102.4 (`ipn/ipnlocal/serve.go`) it carries:
//!
//! - `Host`: the `Host` the device sent (`r.Out.Host = r.In.Host`), which is
//!   the entry's MagicDNS name and port (no port for 443);
//! - `X-Forwarded-Host` (the same), `X-Forwarded-Proto: https` (set when the
//!   request came over TLS, which an `--https` entry always does) and
//!   `X-Forwarded-For` (the device's tailnet address, one address);
//!   `addProxyForwardedHeaders`. Go's `ReverseProxy` drops any of these the
//!   device sent before Tailscale sets them;
//! - for a device logged in as a user (not a tagged one, and not through
//!   Funnel), `Tailscale-User-Login`, `Tailscale-User-Name`,
//!   `Tailscale-User-Profile-Pic` and `Tailscale-Headers-Info`, each deleted
//!   first so a device cannot squat on them (`addTailscaleIdentityHeaders`).
//!   A Funnel request gets `Tailscale-Funnel-Request` and no user. No header
//!   names the device itself (its node), only its user.
//!
//! **Any process on this machine can send all of that to loopback.** So the
//! headers decide only which checks apply, never what is allowed: a request
//! through the entry needs a paired device's bearer token, the same as one
//! from this machine, and then also the tailnet user recorded for that
//! token ([`identity_refusal`]). A local process that forges the headers
//! gains nothing it would not have without them.

use axum::http::{HeaderMap, StatusCode};
use darkmux_fleet::overlay::ServeEntry;
use darkmux_types::client_code::ClientCode;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};

/// (`clients.json` 1.3) The tailnet identity a device paired with, as
/// Tailscale names it on a request through the entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TailnetIdentity {
    /// The `Tailscale-User-Login` value exactly as Tailscale sent it at
    /// pairing (the user's login name; RFC 2047 Q-encoded when it is not
    /// ASCII), compared byte for byte.
    pub login: String,
}

/// How a request reached the listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Arrival {
    /// From an app on this machine ([`crate::is_local_at`]).
    Local,
    /// Forwarded by this process's tailnet entry ([`forwarded_by`]), with
    /// the tailnet user Tailscale named (`None`: it named none, or more
    /// than one).
    Tailnet { login: Option<String> },
    /// Neither.
    Foreign,
}

/// The identity header Tailscale sets for a user's device.
const LOGIN: &str = "tailscale-user-login";

/// Headers `tailscale serve` never sets on a request through an `--https`
/// entry darkmux published: another proxy's (`Forwarded`, `Via`, ...), and
/// Funnel's marker (darkmux's entry has no Funnel). One of them means the
/// request came some other way.
const NOT_TAILSCALE: &[&str] = &[
    "forwarded",
    "via",
    "x-real-ip",
    "x-forwarded-port",
    "x-forwarded-server",
    "cf-connecting-ip",
    "true-client-ip",
    "tailscale-funnel-request",
];

/// How `headers` from `peer` reached a listener bound at `bound`, serving
/// paired devices through `entry` (`None`: no entry, so nothing arrives
/// through one).
pub(crate) fn arrival(
    peer: Option<SocketAddr>,
    headers: &HeaderMap,
    bound: Option<SocketAddr>,
    entry: Option<&ServeEntry>,
) -> Arrival {
    if crate::is_local_at(peer, headers, bound) {
        return Arrival::Local;
    }
    let loopback = peer.is_some_and(|p| p.ip().to_canonical().is_loopback());
    match entry {
        Some(entry) if loopback && forwarded_by(headers, entry) => Arrival::Tailnet {
            login: one(headers, LOGIN)
                .filter(|l| !l.is_empty())
                .map(str::to_string),
        },
        _ => Arrival::Foreign,
    }
}

/// Whether `headers` are what `tailscale serve` sets forwarding to
/// `entry`: a `Host` and an `X-Forwarded-Host` that each name it,
/// `X-Forwarded-Proto: https`, one `X-Forwarded-For` that is a tailnet
/// address, and no header Tailscale does not set ([`NOT_TAILSCALE`]). A
/// header that must be there is there exactly once.
pub(crate) fn forwarded_by(headers: &HeaderMap, entry: &ServeEntry) -> bool {
    let names_it = |h: &str| one(headers, h).is_some_and(|v| names_entry(v, entry));
    names_it(axum::http::header::HOST.as_str())
        && names_it("x-forwarded-host")
        && one(headers, "x-forwarded-proto") == Some("https")
        && one(headers, "x-forwarded-for")
            .and_then(|v| v.parse::<IpAddr>().ok())
            .is_some_and(is_tailnet_address)
        && !NOT_TAILSCALE.iter().any(|h| headers.contains_key(*h))
}

/// The value of `name` when `headers` carry it exactly once, as text.
fn one<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    match (values.next(), values.next()) {
        (Some(v), None) => v.to_str().ok(),
        _ => None,
    }
}

/// Whether a `Host` value names `entry`: its name and port, in any case,
/// with the port left out only for 443 (HTTPS's default, which a device
/// does not send).
pub(crate) fn names_entry(value: &str, entry: &ServeEntry) -> bool {
    let full = format!("{}:{}", entry.host, entry.port);
    value.eq_ignore_ascii_case(&full)
        || (entry.port == 443 && value.eq_ignore_ascii_case(&entry.host))
}

/// Whether `ip` is in a tailnet's address ranges: `100.64.0.0/10` (IPv4)
/// or `fd7a:115c:a1e0::/48` (IPv6), as Tailscale assigns them.
pub(crate) fn is_tailnet_address(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let [a, b, _, _] = v4.octets();
            a == 100 && (b & 0xc0) == 64
        }
        IpAddr::V6(v6) => v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// Why a request through the entry is refused once its token named a
/// paired client: that client recorded no tailnet identity (`recorded`
/// `None`), Tailscale named no user (`login` `None`), or another user than
/// the recorded one; `None` when it is the recorded one. `url` is the
/// entry's, for the remedy.
pub(crate) fn identity_refusal(
    recorded: Option<&TailnetIdentity>,
    login: Option<&str>,
    url: &str,
) -> Option<(ClientCode, String)> {
    let Some(recorded) = recorded else {
        return Some((
            ClientCode::NotPairedRemotely,
            format!(
                "this device's token was paired on this Mac, with no tailnet identity, so it is answered only there: \
                 pair the device over the tailnet (through {url}) to use it from another device"
            ),
        ));
    };
    let Some(login) = login else {
        return Some((
            ClientCode::RemoteIdentityMissing,
            "Tailscale named no tailnet user for this request (a device logged in with a tag, or a request through \
             Funnel, has none), so it cannot be checked against the user this device paired as: log the device in to \
             Tailscale as that user"
                .to_string(),
        ));
    };
    (login != recorded.login).then(|| {
        (
            ClientCode::RemoteIdentityMismatch,
            format!(
                "this request comes from tailnet user {login}, not the one this device paired as: log the device in to \
                 Tailscale as that user, or pair it again over the tailnet as this one"
            ),
        )
    })
}

/// The refusal of a request that is not from this machine and not
/// forwarded by `entry` either: `not_local` with no entry (as before
/// #3560), `not_forwarded` naming the entry's URL with one.
pub(crate) fn not_from_here(entry: Option<&ServeEntry>) -> (StatusCode, ClientCode, String) {
    match entry {
        None => (
            StatusCode::FORBIDDEN,
            ClientCode::NotLocal,
            "darkmux client serve answers only apps on this machine (a loopback connection naming this listener, \
             through no proxy)"
                .to_string(),
        ),
        Some(entry) => (
            StatusCode::FORBIDDEN,
            ClientCode::NotForwarded,
            format!(
                "darkmux client serve answers apps on this machine, and paired devices through its tailnet entry {} \
                 only; this request came through neither (another proxy, or a Host other than {}:{}): use {}",
                entry.url(),
                entry.host,
                entry.port,
                entry.url()
            ),
        ),
    }
}

/// The refusal of a pairing route through the entry: pairing over the
/// tailnet is #3560's next part.
pub(crate) fn pairing_refusal() -> (StatusCode, ClientCode, String) {
    (
        StatusCode::FORBIDDEN,
        ClientCode::RemotePairingUnavailable,
        "pairing a device over the tailnet is not in this darkmux yet: until it is, only an app on this Mac can pair, \
         and a device paired there is answered only there"
            .to_string(),
    )
}

#[cfg(test)]
#[path = "client_tailnet_tests.rs"]
mod tests;
