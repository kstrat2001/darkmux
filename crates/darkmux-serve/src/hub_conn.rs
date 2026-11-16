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

/// The connection the daemon's presence and hub-clock reads share.
pub(crate) static PRESENCE_READS: KeptHubConnection = KeptHubConnection::new();

/// One kept connection: the hub it is to (a digest, as the URL can carry a
/// password) and the connection.
#[derive(Default)]
pub(crate) struct KeptHubConnection {
    slot: Mutex<Option<([u8; 32], redis::Connection)>>,
}

impl KeptHubConnection {
    pub(crate) const fn new() -> Self {
        Self {
            slot: Mutex::new(None),
        }
    }

    /// Run `read` on the kept connection to `url`'s hub, opening one when
    /// there is none (or it is to another hub). `what` names the read in a
    /// connect error. The lock is held across the read, which is bounded.
    /// Blocking.
    pub(crate) fn read<T>(
        &self,
        url: &str,
        what: &str,
        mut read: impl FnMut(&mut redis::Connection) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let hub = *blake3::hash(url.as_bytes()).as_bytes();
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let kept = slot.take().filter(|(h, _)| *h == hub).map(|(_, conn)| conn);
        let reused = kept.is_some();
        let mut conn = match kept {
            Some(conn) => conn,
            None => crate::bounded_redis_connection(url, what)?,
        };
        let mut result = read(&mut conn);
        if reused && result.as_ref().err().is_some_and(was_dropped) {
            conn = crate::bounded_redis_connection(url, what)?;
            result = read(&mut conn);
        }
        if result.is_ok() {
            *slot = Some((hub, conn));
        }
        result
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

/// Whether a read failed because the hub dropped the connection (closed or
/// reset it), not because it stopped answering in time.
fn was_dropped(e: &anyhow::Error) -> bool {
    e.chain()
        .find_map(|c| c.downcast_ref::<redis::RedisError>())
        .is_some_and(redis::RedisError::is_connection_dropped)
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

    /// A closed or reset connection is a drop; a read deadline (which a
    /// socket reports as `WouldBlock` or `TimedOut`) and a reply the hub
    /// refused are not.
    #[test]
    fn only_a_dropped_connection_is_retried() {
        use std::io::ErrorKind::*;
        for kind in [BrokenPipe, ConnectionReset, UnexpectedEof] {
            assert!(was_dropped(&io_failure(kind)), "{kind:?}");
        }
        for kind in [WouldBlock, TimedOut] {
            assert!(!was_dropped(&io_failure(kind)), "{kind:?}");
        }
        let refused: redis::RedisResult<()> =
            Err((redis::ErrorKind::TypeError, "wrong type").into());
        assert!(!was_dropped(&refused.context("MGET").unwrap_err()));
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
