//! (#3432) A connection to the hub the daemon keeps across its repeated
//! reads, instead of opening one per read.
//!
//! Each [`KeptHubConnection`] holds at most one connection, to the hub it was
//! last asked about. A read takes it (or opens one, bounded, when there is
//! none), and gives it back only when the read succeeded: after any failure
//! the next read opens a new one, so a reply a read deadline left on the
//! socket is never read as the next command's answer (the same rule as the
//! SSE tail's `poll_tail`). A kept connection the hub dropped (closed, reset)
//! is opened again at once and the read retried on it, so the drop is not a
//! failed read. A hub that stopped answering is not retried: that read fails
//! within its deadline, as before.
//!
//! The daemon has one for its presence and hub-clock reads
//! ([`PRESENCE_READS`]) and the fleet window keeps one of its own, so a view
//! refresh opens no connection while the hub stays up, however many reads
//! it makes.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The connection the daemon's presence and hub-clock reads share.
pub(crate) static PRESENCE_READS: KeptHubConnection = KeptHubConnection::new();

/// How long a password rotated on the hub keeps working for a kept
/// connection: past this age it is opened again, authenticating anew, so a
/// revoked password stops the daemon's reads within it. Five minutes: a
/// reopen costs one connect (its `CLIENT SETINFO` pair and `AUTH`), so the
/// daemon's two kept connections add about 0.007 connects/s (the hub took
/// 5.85/s before #3432), while a rotation reaches the daemon in minutes,
/// well inside the time it takes to roll a password across a fleet.
pub(crate) const ROTATED_PASSWORD_LIFETIME: Duration = Duration::from_secs(5 * 60);

/// One kept connection: the hub it is to (a digest, as the URL can carry a
/// password), when it was opened, and the connection.
struct Kept {
    hub: [u8; 32],
    opened: Instant,
    conn: redis::Connection,
}

/// At most one kept connection to the hub ([module docs](self)).
#[derive(Default)]
pub(crate) struct KeptHubConnection {
    slot: Mutex<Option<Kept>>,
}

impl KeptHubConnection {
    pub(crate) const fn new() -> Self {
        Self {
            slot: Mutex::new(None),
        }
    }

    /// Run `read` on the kept connection to `url`'s hub, opening one when
    /// there is none, it is to another hub, or it is
    /// [`ROTATED_PASSWORD_LIFETIME`] old. `what` names the read in a connect
    /// error. The lock is held across the read, which is bounded. Blocking.
    pub(crate) fn read<T>(
        &self,
        url: &str,
        what: &str,
        read: impl FnMut(&mut redis::Connection) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.read_at(url, what, Instant::now(), read)
    }

    /// [`Self::read`] at `now`.
    pub(crate) fn read_at<T>(
        &self,
        url: &str,
        what: &str,
        now: Instant,
        mut read: impl FnMut(&mut redis::Connection) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let hub = *blake3::hash(url.as_bytes()).as_bytes();
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let kept = slot.take().filter(|k| still_good(k, hub, now));
        let reused = kept.is_some();
        let (mut opened, mut conn) = match kept {
            Some(k) => (k.opened, k.conn),
            None => (now, crate::bounded_redis_connection(url, what)?),
        };
        let mut result = read(&mut conn);
        if reused && result.as_ref().err().is_some_and(was_dropped) {
            (opened, conn) = (now, crate::bounded_redis_connection(url, what)?);
            result = read(&mut conn);
        }
        if result.is_ok() {
            *slot = Some(Kept { hub, opened, conn });
        }
        result
    }

    /// Close the kept connection, if any: the hub is no longer configured.
    pub(crate) fn forget(&self) {
        *self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Whether a connection is kept (tests).
    #[cfg(test)]
    pub(crate) fn is_kept(&self) -> bool {
        self.slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }
}

/// Whether `kept` may serve a read of `hub` at `now`: it is to that hub,
/// and younger than [`ROTATED_PASSWORD_LIFETIME`].
fn still_good(kept: &Kept, hub: [u8; 32], now: Instant) -> bool {
    kept.hub == hub && now.saturating_duration_since(kept.opened) < ROTATED_PASSWORD_LIFETIME
}

/// `url` as given; when it is `None` (no hub configured now) `kept` is
/// closed, so a hub turned off in config holds no connection.
pub(crate) fn url_or_forget(
    url: Option<darkmux_flow::RawRedisUrl>,
    kept: &KeptHubConnection,
) -> Option<darkmux_flow::RawRedisUrl> {
    if url.is_none() {
        kept.forget();
    }
    url
}

/// Whether a read failed because the hub (or the path to it) dropped the
/// connection: a broken pipe, or a connection reset, aborted, not connected
/// or ended mid-reply, the I/O failures redis-rs says need a new connection
/// (bar a refused connect, which is no kept connection's). A read deadline
/// is not one: the hub stopped answering in time. `RedisError` does not hand
/// out its I/O error, so this asks its own classifiers; the test pins which
/// kinds they mean.
fn was_dropped(e: &anyhow::Error) -> bool {
    e.chain()
        .find_map(|c| c.downcast_ref::<redis::RedisError>())
        .is_some_and(|r| {
            r.is_io_error() && r.is_unrecoverable_error() && !r.is_connection_refusal()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    fn io_failure(kind: std::io::ErrorKind) -> anyhow::Error {
        let e: redis::RedisResult<()> =
            Err(redis::RedisError::from(std::io::Error::new(kind, "x")));
        e.context("MGET presence keys").unwrap_err()
    }

    /// A closed, reset, aborted or unconnected connection is a drop; a read
    /// deadline (which a socket reports as `WouldBlock` or `TimedOut`), a
    /// refused connect, a refused password and a reply the hub refused are
    /// not.
    #[test]
    fn only_a_dropped_connection_is_retried() {
        use std::io::ErrorKind::*;
        for kind in [
            BrokenPipe,
            ConnectionReset,
            ConnectionAborted,
            NotConnected,
            UnexpectedEof,
        ] {
            assert!(was_dropped(&io_failure(kind)), "{kind:?}");
        }
        for kind in [WouldBlock, TimedOut, ConnectionRefused] {
            assert!(!was_dropped(&io_failure(kind)), "{kind:?}");
        }
        let refused: redis::RedisResult<()> =
            Err((redis::ErrorKind::TypeError, "wrong type").into());
        assert!(!was_dropped(&refused.context("MGET").unwrap_err()));
        let auth: redis::RedisResult<()> =
            Err((redis::ErrorKind::AuthenticationFailed, "WRONGPASS").into());
        assert!(!was_dropped(&auth.context("AUTH").unwrap_err()));
        assert!(!was_dropped(&anyhow::anyhow!("not a Redis error")));
    }

    /// A read with no hub to reach fails and keeps nothing.
    #[test]
    fn a_failed_connect_keeps_nothing() {
        let kept = KeptHubConnection::new();
        // Port 1 on loopback: nothing listens, so the connect is refused.
        let r = kept.read("redis://127.0.0.1:1", "a probe", |_| Ok(()));
        assert!(r.is_err());
        assert!(!kept.is_kept());
    }
}
