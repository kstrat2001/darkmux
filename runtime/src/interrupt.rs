//! SIGTERM/SIGINT handling (#3014): an execution a signal ends still closes
//! its trajectory with a terminal `interrupted` record.
//!
//! The runtime is PID 1 in its container, and a process running as PID 1
//! ignores SIGTERM unless it installs a handler, so without this `docker
//! stop` waits out its grace period and kills the container with no record
//! of why. SIGKILL cannot be caught and writes nothing; the host closes that
//! gap (see the lab provider's trajectory preservation).
//!
//! The handler only does what is async-signal-safe: it writes the signal
//! number to a pipe. A watcher thread reads the pipe, writes the terminal
//! record through [`InterruptWriter`], and ends the process with the
//! conventional 128 + signal exit code.

use std::sync::atomic::{AtomicI32, Ordering};

use crate::trajectory::InterruptWriter;

/// Write end of the self-pipe, or -1 before [`install`].
static PIPE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(sig: libc::c_int) {
    let fd = PIPE_WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = sig as u8;
        // SAFETY: `write(2)` is async-signal-safe; `byte` outlives the call.
        unsafe {
            libc::write(fd, (&byte as *const u8).cast(), 1);
        }
    }
}

/// The signals that end an execution deliberately.
const HANDLED: [libc::c_int; 2] = [libc::SIGTERM, libc::SIGINT];

/// Install the handlers and the watcher. `on_done` receives the exit code
/// (128 + signal) after the terminal record is written; production passes
/// `std::process::exit`. Returns `false` when the pipe could not be made, in
/// which case the default dispositions stay in place.
pub fn install(mut writer: InterruptWriter, on_done: fn(i32)) -> bool {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a valid two-element array for pipe(2) to fill.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return false;
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    PIPE_WRITE_FD.store(write_fd, Ordering::SeqCst);
    let spawned = std::thread::Builder::new().name("interrupt-watcher".into()).spawn(move || {
        let mut byte = 0u8;
        // SAFETY: read(2) into one owned byte from a pipe this thread owns.
        let n = unsafe { libc::read(read_fd, (&mut byte as *mut u8).cast(), 1) };
        if n == 1 {
            writer.write_interrupted();
            on_done(128 + i32::from(byte));
        }
    });
    if spawned.is_err() {
        PIPE_WRITE_FD.store(-1, Ordering::SeqCst);
        return false;
    }
    for sig in HANDLED {
        // SAFETY: `on_signal` is async-signal-safe (one write(2)).
        unsafe {
            libc::signal(sig, on_signal as *const () as usize);
        }
    }
    true
}

/// Production wiring: install the handlers for `traj`, exiting the process
/// with 128 + signal once the terminal record is written.
pub fn install_for(traj: &crate::trajectory::Trajectory) {
    if let Some(writer) = traj.interrupt_writer() {
        install(writer, |code| std::process::exit(code));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trajectory::Trajectory;
    use std::sync::mpsc;
    use std::time::Duration;

    static DONE: std::sync::Mutex<Option<mpsc::Sender<i32>>> = std::sync::Mutex::new(None);

    fn report(code: i32) {
        if let Some(tx) = DONE.lock().unwrap().as_ref() {
            let _ = tx.send(code);
        }
    }

    fn restore_defaults() {
        for sig in HANDLED {
            // SAFETY: restoring the default disposition.
            unsafe {
                libc::signal(sig, libc::SIG_DFL);
            }
        }
        PIPE_WRITE_FD.store(-1, Ordering::SeqCst);
    }

    fn records(dir: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(darkmux_trajectory::trajectory_path(dir))
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// (#3014) Promise: an execution a SIGTERM ends has its events on disk
    /// and a terminal `dispatch.complete` saying it was interrupted.
    #[test]
    #[serial_test::serial]
    fn a_sigterm_mid_run_writes_a_terminal_interrupted_record() {
        let tmp = tempfile::tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        traj.append_dispatch_start("m", 1, 1, &["read"]);
        let (tx, rx) = mpsc::channel();
        *DONE.lock().unwrap() = Some(tx);
        assert!(install(traj.interrupt_writer().expect("an open recorder"), report));

        // SAFETY: signalling this process, whose handler is installed.
        unsafe { libc::raise(libc::SIGTERM) };
        let code = rx.recv_timeout(Duration::from_secs(5));
        restore_defaults();

        assert_eq!(code, Ok(128 + libc::SIGTERM), "the watcher must run and report the exit code");
        let all = records(tmp.path());
        assert_eq!(all.first().unwrap()["type"], "dispatch.start", "events before the signal are kept");
        let last = all.last().unwrap();
        assert_eq!(last["type"], "dispatch.complete");
        assert_eq!(last["result"], "interrupted");
    }

    /// A run that already closed its trajectory does not get a second
    /// terminal record from a late signal.
    #[test]
    #[serial_test::serial]
    fn a_signal_after_a_normal_finish_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        traj.append_dispatch_start("m", 1, 1, &["read"]);
        let mut writer = traj.interrupt_writer().unwrap();
        traj.append_dispatch_complete("stop", 5, None);
        assert!(!writer.write_interrupted(), "the terminal record already exists");
        let completes = records(tmp.path()).iter().filter(|r| r["type"] == "dispatch.complete").count();
        assert_eq!(completes, 1);
    }

    /// The other order: the interrupt wins the race, so the run's own exit
    /// writes nothing more.
    #[test]
    fn the_normal_exit_after_an_interrupt_writes_no_second_terminal_record() {
        let tmp = tempfile::tempdir().unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let mut writer = traj.interrupt_writer().unwrap();
        assert!(writer.write_interrupted());
        traj.append_dispatch_complete("error", 5, None);
        let all = records(tmp.path());
        assert_eq!(all.iter().filter(|r| r["type"] == "dispatch.complete").count(), 1);
        assert_eq!(all.last().unwrap()["result"], "interrupted");
    }
}
