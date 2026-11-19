//! Whether a TCP port is free for a server darkmux starts. Binding it is the
//! authority: a listing of listeners leaves out what this user cannot see
//! (another user's or root's, under `lsof` on macOS or `/proc/<pid>/fd` on
//! Linux), and a port handed out on that word may already be served by
//! someone else. The listeners this user can see still count.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// What binding one address on a port came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindOutcome {
    /// darkmux bound it, and let it go.
    Bound,
    /// A socket holds it, whoever's.
    InUse,
    /// This host has no such address (an IPv6 one on a host without IPv6).
    Absent,
    /// The bind failed some other way, so it says nothing.
    Failed,
}

/// The addresses [`bind_probe`] binds, each one exactly. std sets
/// `SO_REUSEADDR` on a Unix listener, and under it macOS refuses a bind only
/// at the same address a socket of this user holds: a listener on the
/// wildcard leaves `127.0.0.1` bindable, and the reverse. Linux refuses
/// either way. Another user's listener is refused at any address that
/// overlaps it, but only in its own family: an IPv6-only listener leaves
/// every IPv4 address bindable, so both families are probed.
pub const PROBED: [IpAddr; 4] = [
    IpAddr::V4(Ipv4Addr::LOCALHOST),
    IpAddr::V4(Ipv4Addr::UNSPECIFIED),
    IpAddr::V6(Ipv6Addr::LOCALHOST),
    IpAddr::V6(Ipv6Addr::UNSPECIFIED),
];

/// What a bind of `addr` returned: in use when the host says so; any other
/// failure is an absent address for IPv6 (a host without it) and no answer
/// for IPv4, which every host has.
pub fn bind_outcome(addr: IpAddr, result: &std::io::Result<()>) -> BindOutcome {
    match result {
        Ok(()) => BindOutcome::Bound,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => BindOutcome::InUse,
        Err(_) if addr.is_ipv6() => BindOutcome::Absent,
        Err(_) => BindOutcome::Failed,
    }
}

/// Binds each of [`PROBED`] on `port` in turn, releasing each before the
/// next.
pub fn bind_probe(port: u16) -> Vec<BindOutcome> {
    PROBED
        .iter()
        .map(|&ip| bind_outcome(ip, &std::net::TcpListener::bind(SocketAddr::new(ip, port)).map(drop)))
        .collect()
}

/// The decision: free only when darkmux bound the port at some address,
/// nothing refused or failed a bind, and no listener this user can see holds
/// it (`visible`, [`crate::Platform::tcp_listeners`]: `None` when the host
/// cannot list them, which leaves the binds to decide).
pub fn port_free_from(binds: &[BindOutcome], visible: Option<&[u32]>) -> bool {
    let bound = binds.contains(&BindOutcome::Bound);
    let unrefused = binds.iter().all(|b| matches!(b, BindOutcome::Bound | BindOutcome::Absent));
    bound && unrefused && visible.is_none_or(|pids| pids.is_empty())
}

/// Whether a server darkmux starts can listen on `port` on this machine:
/// [`port_free_from`] over [`bind_probe`] and this host's visible listeners.
pub fn tcp_port_free(port: u16) -> bool {
    port_free_from(&bind_probe(port), crate::current().tcp_listeners(port).as_deref())
}
