//! (#3560, part 2 of 4) Requests that reach `client serve` through its
//! tailnet entry ([`crate::client_remote`] publishes it).
//!
//! `tailscale serve` terminates TLS and proxies each request to
//! `127.0.0.1:<port>`, so it arrives from loopback, and in Tailscale
//! 1.102.4 (`ipn/ipnlocal/serve.go`) it carries:
//!
//! - `Host`: the `Host` the device sent, passed through as it came
//!   (`r.Out.Host = r.In.Host`). Tailscale picks the handler by the TLS SNI,
//!   not by `Host`, so this is the device's choice and proves nothing; it is
//!   required to name the entry only as one more sign of the shape;
//! - `X-Forwarded-Host` (the same), `X-Forwarded-Proto: https` (set when the
//!   request came over TLS, which an `--https` entry always does) and
//!   `X-Forwarded-For` (the device's tailnet address, one address);
//!   `addProxyForwardedHeaders`. Go's `ReverseProxy` drops any of these the
//!   device sent before Tailscale sets them;
//! - for a device logged in as a user (not a tagged one, and not through
//!   Funnel, which this rule refuses before identity is read),
//!   `Tailscale-User-Login` (`""` for a login that is not valid UTF-8), `Tailscale-User-Name`,
//!   `Tailscale-User-Profile-Pic` and `Tailscale-Headers-Info`, each deleted
//!   first so a device cannot squat on them (`addTailscaleIdentityHeaders`).
//!   A Funnel request gets `Tailscale-Funnel-Request` and no user. No header
//!   names the device itself (its node), only its user.
//!
//! **The device comes from `whois`.** `X-Forwarded-For` is the address of
//! the connection Tailscale accepted, the same one it names the user from,
//! so `tailscale whois` of it (read-only; [`device_at`]) names the device:
//! its stable node id, which a pairing records and every later request
//! through the entry must match ([`device_refusal`]). Behind a subnet router
//! that rewrites source addresses, that address is the router's, so every
//! device behind it is the router; the user check still applies. A `whois`
//! that fails or names nothing refuses the request (`remote_device_unknown`):
//! never guessed. It runs only for a request this process's entry forwarded,
//! after the token and the user check out, and an answer that named a
//! device is kept briefly per address (`fleet_listener::IdentityCache`).
//!
//! **Any process on this machine can send all of that to loopback.** So the
//! headers decide only which checks apply, never what is allowed: a request
//! through the entry needs a paired device's bearer token, the same as one
//! from this machine, and then also the tailnet user and device recorded for
//! that token ([`identity_checked`], [`device_refusal`]). A local process that
//! forges the headers gains nothing it would not have without them. A
//! pairing through the entry takes no token, so it is never approved on its
//! own: it records the user and device and waits for the operator.

use axum::http::{HeaderMap, StatusCode};
use darkmux_fleet::overlay::ServeEntry;
use darkmux_fleet::NodeIdentity;
use darkmux_types::client_code::ClientCode;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};

/// (`clients.json` 1.3) The tailnet identity a device paired with, as
/// Tailscale names it on a request through the entry: on a paired client,
/// and on the pairing request it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TailnetIdentity {
    /// The `Tailscale-User-Login` value exactly as Tailscale sent it at
    /// pairing (the user's login name; RFC 2047 Q-encoded when it is not
    /// ASCII), compared byte for byte.
    pub login: String,
    /// The device the pairing came from, as Tailscale's `whois` named the
    /// forwarded address then.
    pub device: TailnetDevice,
}

/// A tailnet device, as `tailscale whois` names the one at an address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TailnetDevice {
    /// Tailscale's stable node id: what a request is checked against. Never
    /// shown by darkmux's own surfaces.
    pub id: String,
    /// The device's tailnet name at pairing (first label of its MagicDNS
    /// name), for the operator; a rename does not change [`Self::id`].
    pub name: String,
    /// The host name the device's OS reported at pairing, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_name: Option<String>,
}

/// Which tailnet device is at an address (`tailscale whois`): `Ok(None)`
/// when none is, `Err` when Tailscale could not answer.
pub type DeviceLookup = std::sync::Arc<dyn Fn(IpAddr) -> Result<Option<NodeIdentity>, String> + Send + Sync>;

/// How a request reached the listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Arrival {
    /// From an app on this machine ([`crate::is_local_at`]).
    Local,
    /// Forwarded by this process's tailnet entry ([`forwarded_by`]), with
    /// the tailnet user Tailscale named (`None`: it named none, or more
    /// than one) and the device address it forwarded for
    /// (`X-Forwarded-For`).
    Tailnet { login: Option<String>, from: IpAddr },
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
    match (entry, forwarded_for(headers)) {
        (Some(entry), Some(from)) if loopback && forwarded_by(headers, entry) => Arrival::Tailnet {
            login: one(headers, LOGIN)
                .filter(|l| !l.is_empty())
                .map(str::to_string),
            from,
        },
        _ => Arrival::Foreign,
    }
}

/// The one `X-Forwarded-For` address, when there is exactly one.
fn forwarded_for(headers: &HeaderMap) -> Option<IpAddr> {
    one(headers, "x-forwarded-for").and_then(|v| v.parse::<IpAddr>().ok())
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
        && forwarded_for(headers).is_some_and(is_tailnet_address)
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

/// The identity a request through the entry must then match, once its
/// token named a paired client; refused when that client recorded no
/// tailnet identity (`recorded` `None`), Tailscale named no user (`login`
/// `None`), or another user than the recorded one. `url` is the entry's,
/// for the remedy.
pub(crate) fn identity_checked<'a>(
    recorded: Option<&'a TailnetIdentity>,
    login: Option<&str>,
    url: &str,
) -> Result<&'a TailnetIdentity, (ClientCode, String)> {
    let Some(recorded) = recorded else {
        return Err((
            ClientCode::NotPairedRemotely,
            format!(
                "this device's token was paired on this Mac, with no tailnet identity, so it is answered only there: \
                 pair the device over the tailnet (through {url}) to use it from another device"
            ),
        ));
    };
    let Some(login) = login else {
        return Err((
            ClientCode::RemoteIdentityMissing,
            "Tailscale named no tailnet user for this request (a device logged in with a tag has none, and a login \
             that is not valid UTF-8 is sent empty), so it cannot be checked against the user this device paired as: \
             log the device in to Tailscale as that user, not with a tag"
                .to_string(),
        ));
    };
    if login != recorded.login {
        return Err(identity_mismatch(login));
    }
    Ok(recorded)
}

/// The refusal of a request from tailnet user `login`, another than the
/// one its device paired as.
fn identity_mismatch(login: &str) -> (ClientCode, String) {
    (
        ClientCode::RemoteIdentityMismatch,
        format!(
            "this request comes from tailnet user {login}, not the one this device paired as: log the device in to \
             Tailscale as that user, or pair it again over the tailnet as this one"
        ),
    )
}

/// The user a pairing through the entry records: the one Tailscale named,
/// or the refusal when it named none.
pub(crate) fn pairing_login(login: Option<&str>) -> Result<String, (ClientCode, String)> {
    login.map(str::to_string).ok_or_else(|| {
        (
            ClientCode::RemoteIdentityMissing,
            "Tailscale named no tailnet user for this request (a device logged in with a tag has none, and a login \
             that is not valid UTF-8 is sent empty), so there is no identity to pair it as: log the device in to \
             Tailscale as a user, not with a tag, then pair again"
                .to_string(),
        )
    })
}

/// The device `whois` named at a forwarded address, or (fail closed) the
/// refusal when it named none or could not answer.
pub(crate) fn device_named(found: Result<Option<NodeIdentity>, String>) -> Result<TailnetDevice, (ClientCode, String)> {
    match found {
        Ok(Some(node)) => Ok(TailnetDevice { id: node.node_id, name: node.name, host_name: node.host_name }),
        // Why `whois` failed names this Mac's tool and state, so it is not
        // sent to the device.
        Ok(None) | Err(_) => Err((
            ClientCode::RemoteDeviceUnknown,
            "Tailscale on this Mac could not say which tailnet device this request came from, so it cannot be \
             checked or recorded: run `tailscale status` on the Mac, then try again"
                .to_string(),
        )),
    }
}

/// Why a request whose token paired from `recorded` is refused, given what
/// `whois` found at its forwarded address; `None` when it is that device.
pub(crate) fn device_refusal(recorded: &TailnetDevice, found: Result<Option<NodeIdentity>, String>) -> Option<(ClientCode, String)> {
    match device_named(found) {
        Err(refused) => Some(refused),
        Ok(device) if device.id == recorded.id => None,
        Ok(device) => Some(device_mismatch(&device)),
    }
}

/// The refusal of a request from `device`, another than the one its token,
/// or its pairing, came from.
fn device_mismatch(device: &TailnetDevice) -> (ClientCode, String) {
    (
        ClientCode::RemoteDeviceMismatch,
        format!(
            "this request comes from tailnet device {}, not the one this device paired from: use the device that \
             paired, or pair this one over the tailnet",
            device.name
        ),
    )
}

/// Why a poll is refused, when it does not come the way its pairing did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PollRefusal {
    /// As if the request did not exist: a poll from this Mac for a pairing
    /// asked for through the entry.
    NotFound,
    Coded(ClientCode, String),
}

/// Whether a poll from `polling` (`None`: this Mac) may collect a pairing
/// asked for from `asked` (`None`: this Mac).
pub(crate) fn poll_refusal(asked: Option<&TailnetIdentity>, polling: Option<&TailnetIdentity>) -> Option<PollRefusal> {
    match (asked, polling) {
        (None, None) => None,
        (Some(_), None) => Some(PollRefusal::NotFound),
        (None, Some(_)) => Some(PollRefusal::Coded(
            ClientCode::NotPairedRemotely,
            "this pairing was asked for by an app on this Mac, so only that app collects it there".to_string(),
        )),
        (Some(asked), Some(polling)) => same_identity(asked, polling),
    }
}

/// [`poll_refusal`] through the entry: the user, then the device, must be
/// the ones that asked.
fn same_identity(asked: &TailnetIdentity, polling: &TailnetIdentity) -> Option<PollRefusal> {
    if polling.login != asked.login {
        let (code, why) = identity_mismatch(&polling.login);
        return Some(PollRefusal::Coded(code, why));
    }
    (polling.device.id != asked.device.id).then(|| {
        let (code, why) = device_mismatch(&polling.device);
        PollRefusal::Coded(code, why)
    })
}

/// A remote identity as the operator is shown it: Tailscale's user and
/// device, each display-safe.
pub fn described(identity: &TailnetIdentity) -> String {
    let safe = darkmux_platform::display_safe;
    let device = &identity.device;
    let host = device.host_name.as_deref().map(|h| format!(" ({})", safe(h))).unwrap_or_default();
    format!("tailnet user {}, device {}{host}", safe(&identity.login), safe(&device.name))
}

/// The device at `ip`, from a recent answer in `cache` or from `lookup`
/// (off the async workers). Only an answer that named a device is kept.
pub(crate) async fn device_at(
    lookup: &DeviceLookup,
    cache: &crate::fleet_listener::IdentityCache,
    ip: IpAddr,
) -> Result<Option<NodeIdentity>, String> {
    let now = std::time::Instant::now();
    if let Some(node) = cache.get(ip, now) {
        return Ok(Some(node));
    }
    let ask = lookup.clone();
    let answer = tokio::task::spawn_blocking(move || ask(ip)).await.unwrap_or_else(|e| Err(e.to_string()));
    if let Ok(Some(node)) = &answer {
        cache.put(ip, node.clone(), now);
    }
    answer
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

#[cfg(test)]
#[path = "client_tailnet_tests.rs"]
mod tests;
