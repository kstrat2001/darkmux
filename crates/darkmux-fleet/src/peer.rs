//! The ONE place the fleet token is attached to an outgoing request
//! (#2916 re-review MUST 3).
//!
//! Every token-bearing request to another machine (work submission,
//! `machine status`/`resources <id>`, `machine list`, the daemon's
//! peer mission-graph proxy) goes through [`peer_target`] + [`fleet_get`] /
//! [`fleet_post_json`], which take only a [`SettledTarget`]: one whose first-contact
//! pin is already saved in the roster. A target is either this machine's own daemon (a
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
use anyhow::{anyhow, Context, Result};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// Where a token-bearing request may go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerTarget {
    pub(crate) scheme: String,
    /// The host name used in the URL and the Host header.
    pub(crate) host: String,
    pub(crate) port: u16,
    /// The verified address every connection is made to. `None` only for a
    /// loopback target (this machine), and a target without one NEVER gets
    /// the fleet token (#2916 round 3 C1): a port squatter on 127.0.0.1 (a
    /// process of another user, a sandboxed app, the far end of an ssh
    /// tunnel) must not collect it, and this machine's own daemon exempts
    /// loopback callers anyway. Reaching a peer through a loopback tunnel
    /// with the token would need an explicit opt-in; there is none.
    pub(crate) pinned_ip: Option<IpAddr>,
    /// The node id pinned by THIS lookup (first contact), for the caller to
    /// persist; `None` when the entry was already pinned or is loopback.
    pub(crate) newly_pinned: Option<String>,
    /// The node the identity provider named at `pinned_ip` and that passed
    /// the roster's pin; `None` for a target no node was verified behind (a
    /// loopback one). A reader that asks "is this machine's own node behind
    /// this target" compares it to the provider's own node.
    pub(crate) node_id: Option<String>,
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
    /// The peer verified, but its node could not be written into the roster.
    PinNotSaved,
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
            TargetFault::PinNotSaved => format!(
                "The roster file could not be written, so {target}'s node was not pinned and the fleet token was \
                 not sent. Check that the roster file and its directory are writable (`darkmux machine list` \
                 shows the roster path)."
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

/// A target the fleet token may go to: this machine's own daemon, a
/// loopback entry (which never gets the token), or a verified node whose pin
/// is already persisted in the roster. It exists only after
/// [`pin_on_first_contact`](crate::pin_on_first_contact) succeeded or
/// [`PeerTarget::already_settled`] found no roster entry behind it, so
/// [`fleet_get`] and [`fleet_post_json`] cannot be handed a target whose
/// first-contact pin is unsaved. The field is private and no constructor
/// takes a bare [`PeerTarget`] with a pending pin.
///
/// The positive twin: the same call with a settled target compiles, so a
/// rename of `fleet_get`, `PeerTarget` or `SettledTarget` breaks THIS test
/// loudly instead of quietly making the failing ones below vacuous.
///
/// ```
/// use darkmux_fleet::{fleet_get, PeerTarget, SettledTarget};
/// fn send(settled: &SettledTarget) {
///     let _ = fleet_get(settled, "/x", std::time::Duration::from_secs(1), &[]);
/// }
/// fn settle(t: PeerTarget) -> Result<SettledTarget, PeerTarget> {
///     t.already_settled()
/// }
/// ```
///
/// A merely-verified target cannot be sent to (type mismatch):
///
/// ```compile_fail,E0308
/// use darkmux_fleet::{fleet_get, PeerTarget};
/// fn unsafe_send(verified_but_unpinned: &PeerTarget) {
///     let _ = fleet_get(verified_but_unpinned, "/x", std::time::Duration::from_secs(1), &[]);
/// }
/// ```
///
/// Nor can one be built from outside this crate (private constructor):
///
/// ```compile_fail
/// use darkmux_fleet::{PeerTarget, SettledTarget};
/// fn forge(t: PeerTarget) -> SettledTarget {
///     SettledTarget::new(t)
/// }
/// ```
///
/// Nor with the tuple constructor (private field):
///
/// ```compile_fail
/// use darkmux_fleet::{PeerTarget, SettledTarget};
/// fn forge(t: PeerTarget) -> SettledTarget {
///     SettledTarget(t)
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettledTarget(PeerTarget);

impl SettledTarget {
    /// Only the pin settlers build one ([`crate::pin_on_first_contact`] and the
    /// sender's own read-only variant).
    pub(crate) fn new(target: PeerTarget) -> Self {
        SettledTarget(target)
    }

    /// The base URL (`scheme://host:port`).
    pub fn base(&self) -> String {
        self.0.base()
    }

    /// The node the identity provider named behind this target, when one was
    /// verified (see [`PeerTarget::node_id`]).
    pub fn node_id(&self) -> Option<&str> {
        self.0.node_id.as_deref()
    }

    /// Tests only: name the node behind a fixture target.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_node_id_for_test(mut self, node: &str) -> Self {
        self.0.node_id = Some(node.to_string());
        self
    }
}

impl PeerTarget {
    /// The node id behind this target; see the field's own doc.
    pub fn node_id(&self) -> Option<&str> {
        self.node_id.as_deref()
    }

    /// The verified address every connection is made to; see the field's doc.
    pub fn pinned_ip(&self) -> Option<IpAddr> {
        self.pinned_ip
    }

    /// This target as one the token may go to, for a target no roster entry
    /// stands behind: a loopback one or this machine's own daemon. `Err` hands
    /// the target back when a node was verified for a roster entry (pinned or
    /// not): [`crate::pin_on_first_contact`] settles it against the SAVED
    /// entry, so a stale snapshot cannot vouch for it.
    pub fn already_settled(self) -> std::result::Result<SettledTarget, PeerTarget> {
        if self.newly_pinned.is_some() || self.node_id.is_some() {
            return Err(self);
        }
        Ok(SettledTarget(self))
    }

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

/// What the saved roster must still say for `target`'s verified node to be the
/// one `snapshot` (the entry the caller verified against) means: the entry
/// exists under its own id, still has the address that was verified, and pins
/// no other node. With `require_pinned` the pin must also be present and equal
/// (a target verified against an already-pinned entry).
fn check_saved_entry(
    saved: Option<&MachineEntry>,
    snapshot: &MachineEntry,
    node: &str,
    require_pinned: bool,
) -> Result<()> {
    let changed = |why: String| {
        anyhow!("the roster entry for {} changed while contacting it ({why}); nothing was sent, retry", snapshot.id)
    };
    let Some(saved) = saved else {
        return Err(changed(format!(
            "no entry is keyed `{}`: it was removed, or its key differs from its id",
            snapshot.id
        )));
    };
    if saved.address != snapshot.address {
        return Err(changed("its address was edited".to_string()));
    }
    match saved.node_id.as_deref().filter(|p| !p.is_empty()) {
        Some(pinned) if pinned != node => Err(changed("another node was pinned".to_string())),
        None if require_pinned => Err(changed("its pin was cleared".to_string())),
        _ => Ok(()),
    }
}

/// Persist a first-contact pin for the roster entry `snapshot` was read from,
/// compare-and-set under the roster lock: the saved entry must still be the
/// one that was verified (see [`check_saved_entry`]). Returns the node id the
/// saved entry now pins.
pub(crate) fn persist_pin(snapshot: &MachineEntry, target: &PeerTarget) -> Result<String> {
    let Some(node_id) = target.newly_pinned.clone() else { return confirm_pin(snapshot, target) };
    crate::mutate_roster(|r| {
        let saved = r.machines.get_mut(&snapshot.id);
        check_saved_entry(saved.as_deref(), snapshot, &node_id, false)?;
        if let Some(e) = saved {
            e.node_id = Some(node_id.clone());
        }
        Ok(node_id)
    })
}

/// (#3028) The entry as it stands once a card's `uid` and `name` are taken
/// in, or `None` when the card is not confirmed to be the entry's machine or
/// adds nothing. Confirmed means: an entry that holds a uid needs the card to
/// state the same one (so a rename is learned, and a different machine is
/// not); an entry with no uid needs the card's name to be the entry's id or
/// its learned name (a card under another name teaches nothing, so a
/// mis-pointed first contact is not adopted). A uid or name that is not well
/// formed on the wire is ignored, and a name another entry (`others`, the
/// other entries' ids and learned names) already carries is not taken as this entry's
/// current name, so a peer's self-chosen name cannot make another entry's
/// address ambiguous.
fn taught_by_card(e: &MachineEntry, uid: Option<&str>, name: Option<&str>, others: &[&str]) -> Option<MachineEntry> {
    let uid = uid.filter(|u| crate::job::validate_machine_uid(u).is_ok());
    let name = name.filter(|n| darkmux_types::profile_address::machine_name_problem(n).is_none());
    let confirmed = match e.machine_uid.as_deref() {
        Some(known) => uid.is_some_and(|u| known.eq_ignore_ascii_case(u)),
        None => name.is_some_and(|n| {
            crate::job::same_machine(n, &e.id) || e.current_name.as_deref().is_some_and(|c| crate::job::same_machine(n, c))
        }),
    };
    if !confirmed {
        return None;
    }
    let mut next = e.clone();
    if e.machine_uid.is_none() {
        next.machine_uid = uid.map(str::to_string);
    }
    if let Some(n) = name.filter(|n| !others.iter().any(|o| crate::job::same_machine(o, n))) {
        next.current_name = Some(n.to_string());
    }
    (next != *e).then_some(next)
}

/// (#3028) Write what the peer's own card said about itself (its hardware
/// uid and the `machine_id` it goes by now) onto the roster entry `snapshot`
/// was read from, and return the entry as it then stands. Written only when
/// `target` carries a verified node, the same node the pin names: the card
/// is the pinned node's, and only when the card is confirmed to be the
/// entry's machine ([`taught_by_card`]). Like the pin,
/// compare-and-set under the roster lock ([`check_saved_entry`]); an entry
/// the card adds nothing to is not rewritten. The entry's id, the key the
/// operator wrote, is never touched.
pub fn learn_identity(
    snapshot: &MachineEntry,
    target: &SettledTarget,
    uid: Option<&str>,
    name: Option<&str>,
) -> Result<MachineEntry> {
    let Some(node) = target.node_id() else { return Ok(snapshot.clone()) };
    if taught_by_card(snapshot, uid, name, &[]).is_none() {
        return Ok(snapshot.clone());
    }
    crate::mutate_roster(|r| {
        // Every name another entry answers to: its key and its learned name.
        let others: Vec<String> = r
            .machines
            .iter()
            .filter(|(k, _)| **k != snapshot.id)
            .flat_map(|(k, e)| [Some(k.clone()), e.current_name.clone()])
            .flatten()
            .collect();
        let others: Vec<&str> = others.iter().map(String::as_str).collect();
        let saved = r.machines.get_mut(&snapshot.id);
        check_saved_entry(saved.as_deref(), snapshot, node, true)?;
        let saved = saved.ok_or_else(|| anyhow!("no entry is keyed `{}`", snapshot.id))?;
        if let Some(next) = taught_by_card(saved, uid, name, &others) {
            *saved = next;
        }
        Ok(saved.clone())
    })
}

/// Re-read the saved roster and confirm `target`'s verified node is still the
/// one `snapshot`'s entry pins (no write). Returns the pinned node id.
pub(crate) fn confirm_pin(snapshot: &MachineEntry, target: &PeerTarget) -> Result<String> {
    let node = target.node_id.clone().ok_or_else(|| anyhow!("no verified node to confirm for {}", snapshot.id))?;
    let roster = crate::load_roster().context("re-reading the fleet roster")?;
    check_saved_entry(roster.machines.get(&snapshot.id), snapshot, &node, true)?;
    Ok(node)
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

/// The fleet token, attached HERE and nowhere else, and only to a settled
/// target (see [`SettledTarget`]) pinned to a verified address (see
/// [`PeerTarget::pinned_ip`]).
fn with_fleet_token(req: ureq::Request, target: &SettledTarget, token: Option<&str>) -> ureq::Request {
    match (token, target.0.pinned_ip) {
        (Some(t), Some(_)) => req.set("Authorization", &format!("Bearer {t}")),
        _ => req,
    }
}

/// GET `path` from a verified target with the fleet token (when this
/// machine has one) and `headers`. `timeout` is ONE bound on the whole
/// request (connect, sending and reading the answer), not a per-read wait.
pub fn fleet_get(
    target: &SettledTarget,
    path: &str,
    timeout: Duration,
    headers: &[(&str, &str)],
) -> std::result::Result<ureq::Response, ureq::Error> {
    let token = darkmux_flow::serve_token();
    let mut req = base_agent(&target.0).build().get(&format!("{}{path}", target.base())).timeout(timeout);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    with_fleet_token(req, target, token.as_ref().map(|t| t.expose_for_compare())).call()
}

/// POST a JSON body to a verified target with the fleet token.
pub fn fleet_post_json(
    target: &SettledTarget,
    path: &str,
    body: &str,
    read_timeout: Duration,
) -> std::result::Result<ureq::Response, ureq::Error> {
    let token = darkmux_flow::serve_token();
    post_json_with(target, path, body, read_timeout, token.as_ref().map(|t| t.expose_for_compare()))
}

fn post_json_with(
    target: &SettledTarget,
    path: &str,
    body: &str,
    read_timeout: Duration,
    token: Option<&str>,
) -> std::result::Result<ureq::Response, ureq::Error> {
    let agent = base_agent(&target.0)
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
    target: &SettledTarget,
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
pub fn unverified_target_for_test(url_base: &str) -> SettledTarget {
    let (scheme, host, port) = split_address(url_base, 80).unwrap();
    let pinned_ip = host.parse::<IpAddr>().ok();
    SettledTarget(PeerTarget { scheme, host, port, pinned_ip, newly_pinned: None, node_id: None })
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
            current_name: None,
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

    /// The promise: a target whose first-contact pin is unsaved cannot become
    /// a token-bearing one. `already_settled` refuses it; a pinned entry, a
    /// loopback one and this machine's own daemon pass.
    #[test]
    fn a_first_contact_target_is_not_settled_until_its_pin_is_saved() {
        let p = provider();
        let first = peer_target("studio", &entry("100.64.0.2", None), None, 8765, true, &p).unwrap();
        assert_eq!(first.newly_pinned.as_deref(), Some("nSTUDIO"));
        let back = first.already_settled().expect_err("a pending pin is not settled");
        assert_eq!(back.newly_pinned.as_deref(), Some("nSTUDIO"), "the target comes back for pin_on_first_contact");
        let pinned = peer_target("studio", &entry("100.64.0.2", Some("nSTUDIO")), None, 8765, true, &p).unwrap();
        assert!(pinned.already_settled().is_err(), "a verified peer is settled against the saved entry, not a snapshot");
        let lo = peer_target("studio", &entry("127.0.0.1:18765", None), None, 8765, true, &p).unwrap();
        assert!(lo.already_settled().is_ok());
        let own = peer_target("laptop", &entry("x", None), Some("100.64.0.7:8765"), 8765, true, &p).unwrap();
        assert!(own.already_settled().is_ok());
    }

    /// Run `f` against a roster file at `path`, restoring the environment.
    fn with_roster_file<T>(path: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let prev = std::env::var("DARKMUX_FLEET_FILE").ok();
        unsafe { std::env::set_var("DARKMUX_FLEET_FILE", path) };
        let out = f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLEET_FILE", v),
                None => std::env::remove_var("DARKMUX_FLEET_FILE"),
            }
        }
        out
    }

    /// Pinning settles a first-contact target only when the roster write
    /// succeeded; an unwritable roster yields no settled target at all.
    #[test]
    #[serial_test::serial]
    fn pin_on_first_contact_settles_only_when_the_roster_is_written() {
        let p = provider();
        let first = || peer_target("studio", &entry("100.64.0.2", None), None, 8765, true, &p).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("fleet.json");
        let pinned = with_roster_file(&file, || {
            crate::mutate_roster(|r| {
                r.machines.insert("studio".into(), entry("100.64.0.2", None));
                Ok(())
            })
            .unwrap();
            crate::pin_on_first_contact(first(), &entry("100.64.0.2", None), &p).expect("settled after the write");
            crate::load_roster().unwrap().machines["studio"].node_id.clone()
        });
        assert_eq!(pinned.as_deref(), Some("nSTUDIO"));
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "a file, not a directory").unwrap();
        let refused = with_roster_file(&blocker.join("fleet.json"), || crate::pin_on_first_contact(first(), &entry("100.64.0.2", None), &p));
        assert!(refused.is_err(), "an unwritable roster never yields a settled target");
    }

    /// The roster as saved when `pin_on_first_contact` runs, against the
    /// snapshot `entry("100.64.0.2", None)` a caller verified: returns the
    /// outcome and the node the saved `key` entry pins afterward.
    fn pin_against_saved(
        saved: Option<(&str, MachineEntry)>,
        snapshot: MachineEntry,
        target_of: impl FnOnce(&StaticIdentityProvider) -> PeerTarget,
    ) -> (Result<SettledTarget>, Option<String>) {
        let p = provider();
        let dir = tempfile::tempdir().unwrap();
        with_roster_file(&dir.path().join("fleet.json"), || {
            crate::mutate_roster(|r| {
                if let Some((key, e)) = saved.clone() {
                    r.machines.insert(key.to_string(), e);
                }
                Ok(())
            })
            .unwrap();
            let out = crate::pin_on_first_contact(target_of(&p), &snapshot, &p);
            let pinned = crate::load_roster().unwrap().machines.values().find_map(|e| e.node_id.clone());
            (out, pinned)
        })
    }

    fn first_contact(p: &StaticIdentityProvider) -> PeerTarget {
        peer_target("studio", &entry("100.64.0.2", None), None, 8765, true, p).unwrap()
    }

    /// The pin write is compare-and-set: an entry that is not there, or is not
    /// the one that was verified, yields no settled target and writes nothing.
    #[test]
    #[serial_test::serial]
    fn a_pin_is_saved_only_against_the_entry_that_was_verified() {
        let snap = entry("100.64.0.2", None);
        let (ok, pinned) = pin_against_saved(Some(("studio", snap.clone())), snap.clone(), first_contact);
        assert!(ok.is_ok());
        assert_eq!(pinned.as_deref(), Some("nSTUDIO"));

        let (removed, _) = pin_against_saved(None, snap.clone(), first_contact);
        let msg = format!("{:#}", removed.unwrap_err());
        assert!(msg.contains("removed, or its key differs"), "{msg}");

        let (keyed_apart, pinned) = pin_against_saved(Some(("Studio", snap.clone())), snap.clone(), first_contact);
        assert!(keyed_apart.is_err(), "a key that differs from the id has no entry to pin");
        assert_eq!(pinned, None, "nothing was written");

        let (moved, pinned) = pin_against_saved(Some(("studio", entry("100.64.0.9", None))), snap.clone(), first_contact);
        assert!(format!("{:#}", moved.unwrap_err()).contains("address was edited"));
        assert_eq!(pinned, None, "the verified node never lands on a different address");

        let (raced, pinned) = pin_against_saved(Some(("studio", entry("100.64.0.2", Some("nOTHER")))), snap.clone(), first_contact);
        assert!(format!("{:#}", raced.unwrap_err()).contains("another node was pinned"));
        assert_eq!(pinned.as_deref(), Some("nOTHER"), "a concurrent pin is not overwritten");

        let (same, _) = pin_against_saved(Some(("studio", entry("100.64.0.2", Some("nSTUDIO")))), snap, first_contact);
        assert!(same.is_ok(), "the same node pinned concurrently is fine");
    }

    /// A peer verified against an already-pinned entry is re-checked against
    /// the saved roster: a pin cleared or changed since the snapshot refuses.
    #[test]
    #[serial_test::serial]
    fn an_already_pinned_peer_is_reconfirmed_against_the_saved_entry() {
        let pinned_snap = entry("100.64.0.2", Some("nSTUDIO"));
        let target = |p: &StaticIdentityProvider| peer_target("studio", &pinned_snap, None, 8765, true, p).unwrap();
        let (ok, _) = pin_against_saved(Some(("studio", pinned_snap.clone())), pinned_snap.clone(), target);
        assert!(ok.is_ok());
        let (cleared, _) = pin_against_saved(Some(("studio", entry("100.64.0.2", None))), pinned_snap.clone(), target);
        assert!(format!("{:#}", cleared.unwrap_err()).contains("pin was cleared"));
        let (readdressed, _) = pin_against_saved(Some(("studio", entry("100.64.0.9", Some("nSTUDIO")))), pinned_snap.clone(), target);
        assert!(readdressed.is_err());
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
            let t = t.already_settled().unwrap();
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
        let t = t.already_settled().unwrap();
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
        let t = t.already_settled().unwrap();
        let body = fleet_get(&t, "/x", Duration::from_secs(5), &[]).unwrap().into_string().unwrap();
        assert_eq!(body, "ok");
    }

    /// `learn_identity` against a roster that holds `saved` under `studio`,
    /// for a target pinned to `nSTUDIO`: the entry it returns and the entry
    /// as saved afterward.
    fn learn_against_saved(
        saved: MachineEntry,
        snapshot: MachineEntry,
        uid: Option<&str>,
        name: Option<&str>,
    ) -> (Result<MachineEntry>, MachineEntry) {
        let p = provider();
        let dir = tempfile::tempdir().unwrap();
        with_roster_file(&dir.path().join("fleet.json"), || {
            crate::mutate_roster(|r| {
                r.machines.insert("studio".into(), saved);
                Ok(())
            })
            .unwrap();
            let target = SettledTarget::new(first_contact(&p));
            let out = learn_identity(&snapshot, &target, uid, name);
            (out, crate::load_roster().unwrap().machines["studio"].clone())
        })
    }

    fn pinned_entry() -> MachineEntry {
        entry("100.64.0.2", Some("nSTUDIO"))
    }

    /// (#3035) A top-level roster field this binary does not know (a newer
    /// binary's, or a hand-added note) survives every path that rewrites the
    /// roster: `machine add`, a first-contact pin, an identity learned from a
    /// card, and `machine remove`.
    #[test]
    #[serial_test::serial]
    fn an_unknown_top_level_roster_field_survives_every_rewrite_path() {
        let p = provider();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("fleet.json");
        std::fs::write(
            &file,
            r#"{"version":"2","from_the_future":{"x":1},"machines":{"studio":{"id":"studio","address":"100.64.0.2","added_unix_ms":1}}}"#,
        )
        .unwrap();
        let future = |what: &str| {
            let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
            assert_eq!(raw["from_the_future"]["x"], 1, "lost across {what}: {raw}");
        };
        with_roster_file(&file, || {
            crate::mutate_roster(|r| crate::add_machine(r, "mini", "100.64.0.3", None, None)).unwrap();
            future("machine add");
            let snapshot = entry("100.64.0.2", None);
            persist_pin(&snapshot, &first_contact(&p)).unwrap();
            future("a first-contact pin");
            let target = SettledTarget::new(first_contact(&p));
            learn_identity(&pinned_entry(), &target, Some("UID-S"), Some("studio")).unwrap();
            future("an identity learned from a card");
            crate::mutate_roster(|r| Ok(crate::remove_machine(r, "mini"))).unwrap();
            future("machine remove");
        });
    }

    /// (#3028) The pinned peer's card teaches the entry its uid and its
    /// current name; the key the operator wrote is not touched.
    #[test]
    #[serial_test::serial]
    fn a_pinned_peers_card_under_the_entrys_name_teaches_its_uid() {
        let (out, saved) = learn_against_saved(pinned_entry(), pinned_entry(), Some("UID-S"), Some("Studio"));
        let out = out.unwrap();
        for e in [&out, &saved] {
            assert_eq!(e.machine_uid.as_deref(), Some("UID-S"));
            assert_eq!(e.current_name.as_deref(), Some("Studio"));
            assert_eq!(e.id, "studio");
        }
    }

    /// (#3028) A card stating a DIFFERENT uid than the one the entry holds is
    /// never written over it, and its name is not taken either: the entry
    /// keeps what it knew and the view's mismatch handling says so.
    #[test]
    #[serial_test::serial]
    fn a_different_uid_is_never_written_over_a_known_one() {
        let known = MachineEntry { machine_uid: Some("UID-S".into()), current_name: Some("studio".into()), ..pinned_entry() };
        let (out, saved) = learn_against_saved(known.clone(), known.clone(), Some("UID-OTHER"), Some("impostor"));
        assert_eq!(out.unwrap(), known, "the entry as it stands is returned");
        assert_eq!(saved, known, "nothing was written");
    }

    /// (#3028) The same uid under a new name updates the name alone, and the
    /// uid compares case-insensitively.
    #[test]
    #[serial_test::serial]
    fn the_same_uid_under_a_new_name_updates_the_name() {
        let known = MachineEntry { machine_uid: Some("uid-s".into()), current_name: Some("studio".into()), ..pinned_entry() };
        let (out, saved) = learn_against_saved(known.clone(), known, Some("UID-S"), Some("studio-2"));
        out.unwrap();
        assert_eq!((saved.machine_uid.as_deref(), saved.current_name.as_deref()), (Some("uid-s"), Some("studio-2")));
    }

    /// (#3028) Compare-and-set, like the pin: an entry edited since the card
    /// was fetched is not written, and a target with no verified node (a
    /// loopback entry) teaches nothing.
    #[test]
    #[serial_test::serial]
    fn learning_is_saved_only_against_the_entry_that_was_verified() {
        let moved = MachineEntry { address: "100.64.0.9".into(), ..pinned_entry() };
        let (out, saved) = learn_against_saved(moved.clone(), pinned_entry(), Some("UID-S"), Some("studio"));
        assert!(format!("{:#}", out.unwrap_err()).contains("address was edited"));
        assert_eq!(saved, moved, "nothing was written");

        let p = provider();
        let dir = tempfile::tempdir().unwrap();
        let unverified = with_roster_file(&dir.path().join("fleet.json"), || {
            crate::mutate_roster(|r| {
                r.machines.insert("studio".into(), pinned_entry());
                Ok(())
            })
            .unwrap();
            let lo = peer_target("lo", &entry("127.0.0.1", None), None, 8765, true, &p).unwrap().already_settled().unwrap();
            assert_eq!(lo.node_id(), None);
            learn_identity(&pinned_entry(), &lo, Some("UID-S"), Some("studio")).unwrap()
        });
        assert_eq!((unverified.machine_uid, unverified.current_name), (None, None));
    }

    /// (#3028) A first contact (no uid known) whose card is under another
    /// name is not adopted: the entry may be pointed at the wrong machine, and
    /// nothing is written.
    #[test]
    #[serial_test::serial]
    fn a_first_contact_card_under_another_name_teaches_nothing() {
        let (out, saved) = learn_against_saved(pinned_entry(), pinned_entry(), Some("UID-DB"), Some("darkbook"));
        assert_eq!(out.unwrap(), pinned_entry());
        assert_eq!(saved, pinned_entry(), "nothing was written");
    }

    /// (#3028) A name that is not a valid machine name confirms nothing.
    #[test]
    #[serial_test::serial]
    fn a_card_name_that_is_not_a_machine_name_teaches_nothing() {
        let (out, saved) = learn_against_saved(pinned_entry(), pinned_entry(), Some("UID-S"), Some("not a name!"));
        out.unwrap();
        assert_eq!(saved, pinned_entry());
    }

    /// (#3028) A known uid needs the card to state it: a card with no uid
    /// cannot rename an entry.
    #[test]
    #[serial_test::serial]
    fn a_card_with_no_uid_cannot_rename_an_entry_that_holds_one() {
        let known = MachineEntry { machine_uid: Some("UID-S".into()), ..pinned_entry() };
        let (_, saved) = learn_against_saved(known.clone(), known.clone(), None, Some("studio-now"));
        assert_eq!(saved, known);
    }

    /// (#3028) A name another entry already carries is not taken as this
    /// entry's current name (it would make that entry's address ambiguous);
    /// the rest of the card is still learned.
    #[test]
    #[serial_test::serial]
    fn a_name_another_entry_carries_is_not_learned() {
        let known = MachineEntry { machine_uid: Some("UID-S".into()), ..pinned_entry() };
        let p = provider();
        let dir = tempfile::tempdir().unwrap();
        let saved = with_roster_file(&dir.path().join("fleet.json"), || {
            crate::mutate_roster(|r| {
                r.machines.insert("studio".into(), known.clone());
                r.machines.insert("Mini".into(), MachineEntry { id: "Mini".into(), ..entry("100.64.0.3", None) });
                Ok(())
            })
            .unwrap();
            let target = SettledTarget::new(first_contact(&p));
            let out = learn_identity(&known, &target, Some("UID-S"), Some("mini")).unwrap();
            (out, crate::load_roster().unwrap().machines["studio"].clone())
        });
        assert_eq!(saved.0.current_name, None);
        assert_eq!(saved.1.current_name, None, "nothing was written");
    }

    /// (5.0) A name another entry already holds as its LEARNED name is not
    /// taken either; a genuinely new name still is.
    #[test]
    #[serial_test::serial]
    fn a_name_another_entry_has_learned_is_not_learned_but_a_new_name_is() {
        let known = MachineEntry { machine_uid: Some("UID-S".into()), ..pinned_entry() };
        let p = provider();
        let dir = tempfile::tempdir().unwrap();
        let learn = |name: &str| {
            with_roster_file(&dir.path().join("fleet.json"), || {
                crate::mutate_roster(|r| {
                    r.machines.insert("studio".into(), known.clone());
                    r.machines.insert(
                        "m2".into(),
                        MachineEntry { id: "m2".into(), current_name: Some("mini".into()), ..entry("100.64.0.3", None) },
                    );
                    Ok(())
                })
                .unwrap();
                let target = SettledTarget::new(first_contact(&p));
                learn_identity(&known, &target, Some("UID-S"), Some(name)).unwrap().current_name
            })
        };
        assert_eq!(learn("mini"), None, "`mini` is m2's learned name");
        assert_eq!(learn("MINI"), None, "a case variant of m2's learned name is refused too");
        assert_eq!(learn("studio-now").as_deref(), Some("studio-now"), "a new name is learned");
    }
}
