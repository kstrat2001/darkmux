//! (#3200) The daemon's reaper for dispatch containers whose darkmux host
//! died without its handlers running (SIGKILL, an OOM kill). The rule and
//! the docker calls live in `darkmux_crew::orphan_reaper`; this module is
//! the cadence and the log.
//!
//! Same dedicated-`std::thread` shape and stop flag as `host_sampler`. One
//! bounded `docker ps` per tick; a `docker kill` only for an orphan. It
//! writes no flow record: each reaped container gets one log line naming it
//! and its dead host pid, so an operator who sees a container vanish can
//! find why. The dispatch it belonged to keeps the terminal state the read
//! side already derives for a host that died mid-run (#3125's lost beat);
//! the reaper writes no dispatch terminal of its own.

use darkmux_crew::orphan_reaper::{host_liveness, reap_tick, HostLiveness, ReapTick};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// How often the reaper looks. An orphan holds memory and a pids budget,
/// not a model, so a minute is prompt enough, and the cost is one
/// `docker ps` a minute.
pub(crate) const REAP_INTERVAL: Duration = Duration::from_secs(60);

/// How often the thread checks its stop flag between ticks.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// One tick's log lines. `docker_warned` carries "already said docker is
/// unavailable" between ticks, so a machine without docker logs that once.
pub(crate) fn tick_lines(
    this_machine: Option<&str>,
    liveness: impl Fn(u32) -> HostLiveness,
    docker_warned: &mut bool,
) -> Vec<String> {
    match reap_tick(this_machine, liveness) {
        ReapTick::DockerUnavailable if *docker_warned => Vec::new(),
        ReapTick::DockerUnavailable => {
            *docker_warned = true;
            vec![
                "darkmux serve: dispatch-container reaper idle: `docker ps` did not answer"
                    .to_string(),
            ]
        }
        ReapTick::Listed { reaped } => {
            *docker_warned = false;
            reaped
                .into_iter()
                .map(|(orphan, killed)| {
                    let verb = if killed { "killed" } else { "could not kill" };
                    format!(
                        "darkmux serve: {verb} orphaned dispatch container {} (its darkmux host, pid {}, is gone)",
                        orphan.name, orphan.host_pid
                    )
                })
                .collect()
        }
    }
}

/// Start the reaper thread: a tick at once, then every `interval` until
/// `stop_flag` is set.
pub(crate) fn spawn(interval: Duration, stop_flag: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let this_machine = darkmux_hardware::machine_uid();
        let mut docker_warned = false;
        loop {
            for line in tick_lines(this_machine, host_liveness, &mut docker_warned) {
                eprintln!("{line}");
            }
            let mut slept = Duration::ZERO;
            while slept < interval {
                if stop_flag.load(Ordering::SeqCst) {
                    return;
                }
                let nap = STOP_POLL_INTERVAL.min(interval - slept);
                std::thread::sleep(nap);
                slept += nap;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    fn with_path<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let prev = std::env::var("PATH").ok();
        // SAFETY: every caller is `#[serial]`.
        unsafe {
            std::env::set_var(
                "PATH",
                format!("{}:{}", dir.display(), prev.clone().unwrap_or_default()),
            )
        };
        let out = f();
        unsafe {
            match prev {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }
        out
    }

    /// The serve tick against a fake docker listing one orphan and one live
    /// host's container: one line, naming the orphan and its dead pid; the
    /// fake saw exactly one `ps` and one `kill`.
    #[test]
    #[serial]
    fn a_serve_tick_kills_the_orphan_and_names_it_and_its_host() {
        let dir = TempDir::new().unwrap();
        let record = dir.path().join("calls.txt");
        let docker = dir.path().join("docker");
        std::fs::write(
            &docker,
            format!(
                "#!/bin/sh\necho \"$@\" >> {}\nif [ \"$1\" = ps ]; then printf 'darkmux-dispatch-dead\\t200\\t1\\tM\\ndarkmux-dispatch-live\\t100\\t1\\tM\\n'; fi\n",
                record.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let liveness = |pid| {
            if pid == 100 {
                HostLiveness::Running { start: Some(1) }
            } else {
                HostLiveness::Gone
            }
        };
        let mut warned = false;
        let lines = with_path(dir.path(), || tick_lines(Some("M"), liveness, &mut warned));
        assert_eq!(
            lines,
            vec!["darkmux serve: killed orphaned dispatch container darkmux-dispatch-dead (its darkmux host, pid 200, is gone)"]
        );
        let calls = std::fs::read_to_string(&record).unwrap();
        assert_eq!(
            calls.lines().filter(|l| l.starts_with("ps ")).count(),
            1,
            "{calls}"
        );
        assert_eq!(
            calls
                .lines()
                .filter(|l| l.starts_with("kill "))
                .collect::<Vec<_>>(),
            vec!["kill darkmux-dispatch-dead"]
        );
    }

    /// No docker: one line on the first tick, silence after.
    #[test]
    #[serial]
    fn without_docker_the_reaper_says_so_once() {
        let empty = TempDir::new().unwrap();
        let prev = std::env::var("PATH").ok();
        // SAFETY: `#[serial]`.
        unsafe { std::env::set_var("PATH", empty.path()) };
        let mut warned = false;
        let first = tick_lines(Some("M"), |_| HostLiveness::Gone, &mut warned);
        let second = tick_lines(Some("M"), |_| HostLiveness::Gone, &mut warned);
        unsafe {
            match prev {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }
        assert_eq!(first.len(), 1, "{first:?}");
        assert!(second.is_empty(), "{second:?}");
    }
}
