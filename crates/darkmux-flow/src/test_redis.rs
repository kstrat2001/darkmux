//! A bounded readiness wait for a `redis-server` a test spawned itself.
//!
//! Test-only (`test` or the `test-support` feature), so it never ships in a
//! release build. It lives here so `darkmux-flow`'s and `darkmux-serve`'s
//! in-process redis tests share one definition; the e2e harness keeps its
//! own (`tests/e2e/harness.rs`'s `start_fixture` and `redis_answers`), which
//! this matches.
//!
//! Those tests pick a port with `bind(:0)`, release it, and start
//! `redis-server` on it. Another process can take the port in between:
//! `redis-server` then exits, or the port is held by something that accepts
//! and never answers. A wait that polls `redis::Client::get_connection` with
//! no bound spins forever on the first and blocks forever on the second
//! (redis-rs sets no read timeout, so a deadline around it does not bound
//! it). This wait checks the child and probes over raw TCP with a timeout on
//! every step, and its error says which of the two happened.

use std::time::{Duration, Instant};

/// How long a spawned `redis-server` has to answer PING.
pub const REDIS_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Wait, bounded by `timeout`, until the `redis-server` in `child` answers
/// PING on `port`. Otherwise an error naming the cause: the child exited
/// (with its status), or it is running and nothing on the port answered as
/// redis.
pub fn wait_until_redis_answers(child: &mut std::process::Child, port: u16, timeout: Duration) -> Result<(), String> {
    let start = Instant::now();
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "redis-server exited before answering on port {port} ({status}); \
                 another process most likely took the port between bind(:0) and redis-server's own bind"
            ));
        }
        if redis_answers_ping(port) {
            return Ok(());
        }
        if start.elapsed() >= timeout {
            return Err(format!(
                "redis-server (still running) did not answer PING on port {port} within {timeout:?}; \
                 whatever holds the port is not answering as redis"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// One PING over raw TCP, bounded at every step (#2898: redis-rs bounds only
/// the TCP connect, then reads its reply with no timeout).
pub fn redis_answers_ping(port: u16) -> bool {
    use std::io::{Read, Write};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    if stream.write_all(b"PING\r\n").is_err() {
        return false;
    }
    let mut reply = [0u8; 7];
    matches!(stream.read(&mut reply), Ok(n) if reply[..n].starts_with(b"+PONG"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A server that died at startup is reported as dead, with its status,
    /// not waited on until the deadline.
    #[test]
    fn a_child_that_exits_is_reported_as_exited_with_its_status() {
        let mut child = Kill(std::process::Command::new("sh").args(["-c", "exit 3"]).spawn().unwrap());
        let err = wait_until_redis_answers(&mut child.0, free_port(), Duration::from_secs(5)).unwrap_err();
        assert!(err.contains("exited before answering") && err.contains("3"), "{err}");
    }

    /// A port held by something that accepts and never replies ends the wait
    /// at the deadline, naming that, while the child is still running.
    #[test]
    fn a_port_that_never_answers_ends_the_wait_at_the_deadline() {
        let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = holder.local_addr().unwrap().port();
        let mut child = Kill(std::process::Command::new("sleep").arg("30").spawn().unwrap());
        let start = Instant::now();
        let err = wait_until_redis_answers(&mut child.0, port, Duration::from_millis(300)).unwrap_err();
        assert!(err.contains("did not answer PING") && err.contains("still running"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(5), "bounded: {:?}", start.elapsed());
        drop(holder);
    }

    /// A port that answers PING is ready.
    #[test]
    fn a_port_that_answers_pong_is_ready() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 16];
            let _ = s.read(&mut buf);
            s.write_all(b"+PONG\r\n").unwrap();
        });
        let mut child = Kill(std::process::Command::new("sleep").arg("30").spawn().unwrap());
        wait_until_redis_answers(&mut child.0, port, Duration::from_secs(5)).unwrap();
        server.join().unwrap();
    }
}
