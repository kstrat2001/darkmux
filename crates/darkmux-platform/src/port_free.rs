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

/// The addresses [`bind_probe`] binds, each one exactly. The probe sets
/// `SO_REUSEADDR` (as std does on a Unix listener), and under it macOS
/// refuses a bind only at the same address a socket of this user holds:
/// a listener on the wildcard leaves `127.0.0.1` bindable, and the reverse.
/// Linux refuses either way. Another user's listener is refused at any address that
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
    bind_probe_holding(port, &|_, _| {})
}

/// [`bind_probe`], calling `held` with each address, and the probe's file
/// descriptor, while its bind holds it.
pub(crate) fn bind_probe_holding(port: u16, held: &dyn Fn(SocketAddr, i32)) -> Vec<BindOutcome> {
    PROBED.iter().map(|&ip| bind_outcome(ip, &bind_only(SocketAddr::new(ip, port), held))).collect()
}

/// A TCP socket bound to `at` and closed, never listening: a listener on
/// the wildcard can raise the application firewall's prompt on a Mac.
/// `SO_REUSEADDR` is set as std sets it on a listener, so a connection of
/// this port's left in `TIME_WAIT` does not read as a holder. The socket
/// closes on every path, when its `OwnedFd` drops.
#[cfg(unix)]
fn bind_only(at: SocketAddr, held: &dyn Fn(SocketAddr, i32)) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let socket = probe_socket(if at.is_ipv4() { libc::AF_INET } else { libc::AF_INET6 })?;
    let on: libc::c_int = 1;
    let on_len = std::mem::size_of_val(&on) as libc::socklen_t;
    let raw = socket.as_raw_fd();
    checked(unsafe { libc::setsockopt(raw, libc::SOL_SOCKET, libc::SO_REUSEADDR, (&on as *const libc::c_int).cast(), on_len) })?;
    checked(bind_raw(raw, at))?;
    held(at, raw);
    Ok(())
}

/// A TCP socket of `family`, close-on-exec as std's are, so a child spawned
/// on another thread while the probe holds a port does not inherit it and
/// hold the port: set atomically at creation where the type takes
/// [`SOCKET_CLOEXEC`], and by `fcntl` straight after in every case (a
/// no-op where creation set it).
#[cfg(unix)]
fn probe_socket(family: libc::c_int) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let fd = checked(unsafe { libc::socket(family, libc::SOCK_STREAM | SOCKET_CLOEXEC, 0) })?;
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    checked(unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) })?;
    Ok(socket)
}

/// The socket type flag that makes a socket close-on-exec as it is created,
/// on a kernel whose `socket(2)` takes one; macOS's does not.
#[cfg(target_os = "linux")]
const SOCKET_CLOEXEC: libc::c_int = libc::SOCK_CLOEXEC;
#[cfg(all(unix, not(target_os = "linux")))]
const SOCKET_CLOEXEC: libc::c_int = 0;

/// No socket call of this kind where there is no libc: no answer, so busy.
#[cfg(not(unix))]
fn bind_only(_at: SocketAddr, _held: &dyn Fn(SocketAddr, i32)) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

/// A socket call's return as a result: negative is the call's error.
#[cfg(unix)]
fn checked(rc: libc::c_int) -> std::io::Result<libc::c_int> {
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

/// `bind(2)` of `fd` to `at`. The address is built from zero with its
/// family, port and address set; BSD's `sin_len` is left zero, since the
/// kernel sets it from the length passed.
#[cfg(unix)]
fn bind_raw(fd: libc::c_int, at: SocketAddr) -> libc::c_int {
    match at {
        SocketAddr::V4(v4) => {
            let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            sa.sin_family = libc::AF_INET as libc::sa_family_t;
            sa.sin_port = v4.port().to_be();
            sa.sin_addr.s_addr = u32::from(*v4.ip()).to_be();
            bind_sockaddr(fd, &sa)
        }
        SocketAddr::V6(v6) => {
            let mut sa: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            sa.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sa.sin6_port = v6.port().to_be();
            sa.sin6_addr.s6_addr = v6.ip().octets();
            bind_sockaddr(fd, &sa)
        }
    }
}

#[cfg(unix)]
fn bind_sockaddr<T>(fd: libc::c_int, sa: &T) -> libc::c_int {
    unsafe { libc::bind(fd, (sa as *const T).cast(), std::mem::size_of::<T>() as libc::socklen_t) }
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
