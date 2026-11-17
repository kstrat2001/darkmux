//! A Seatbelt sandbox's process and memory ceilings, proven with small
//! numbers only: a process limit of 8 against 30 forks, and a 64 MiB memory
//! limit against one process holding 128 MiB. Each has its inverse (no limit,
//! or one above what the program uses), so a backend that refuses every fork
//! or stops every sandbox fails too. Skipped where `sandbox-exec` is missing.

use super::*;
use std::time::{Duration, Instant};

/// Sets [`STATE_DIR_ENV`] for the test's life, then puts it back.
struct StateDir {
    _root: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
}

impl StateDir {
    fn new() -> Self {
        let root = tempfile::TempDir::new().unwrap();
        let prev = std::env::var_os(STATE_DIR_ENV);
        unsafe { std::env::set_var(STATE_DIR_ENV, root.path()) };
        StateDir { _root: root, prev }
    }
}

impl Drop for StateDir {
    fn drop(&mut self) {
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var(STATE_DIR_ENV, v),
                None => std::env::remove_var(STATE_DIR_ENV),
            }
        }
    }
}

/// The system's perl running `script` under `limits`, no network, no mount.
fn perl(name: &str, limits: Limits, script: &str) -> SandboxPolicy {
    SandboxPolicy {
        name: name.into(),
        image: "unused".into(),
        network: Network::None,
        limits,
        labels: Vec::new(),
        mounts: Vec::new(),
        ports: Vec::new(),
        stdin: false,
        host_alias: None,
        user: None,
        env: Vec::new(),
        workdir: None,
        entrypoint: Some("/usr/bin/perl".into()),
        command: vec!["-e".into(), script.into()],
    }
}

/// Forks up to 30 children that each sleep 3 s, stopping at the first fork
/// the kernel refuses (perl's `fork` fails at once, where a shell retries
/// for seconds), and prints how many it started.
const FORK_30: &str = "my $n = 0; for (1 .. 30) { my $p = fork(); last unless defined $p; if ($p == 0) { sleep 3; exit 0 } $n++ } print \"$n\\n\"; 1 while wait() > 0;";

/// Holds 128 MiB, every page touched, for 3 s, then says it survived.
const HOLD_128_MIB: &str = "my $x = \"a\" x (128 * 1024 * 1024); sleep 3; print \"survived\\n\";";

fn started(limits: Limits) -> (usize, String) {
    let _state = StateDir::new();
    let (mut cmd, _) = SeatbeltBackend.run(&perl("darkmux-limits-fork", limits, FORK_30));
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (stdout.parse().unwrap_or(usize::MAX), String::from_utf8_lossy(&out.stderr).into_owned())
}

/// The kernel refuses a fork past the limit. RLIMIT_NPROC counts every
/// process of the user, so the limit is what the user ran at launch plus
/// `pids`: other processes starting or ending meanwhile move it a little,
/// never to 30 more.
#[test]
#[serial_test::serial]
fn a_sandbox_cannot_fork_past_its_process_limit() {
    if !SeatbeltBackend::available() {
        return;
    }
    let (n, stderr) = started(Limits { memory: None, pids: Some(8) });
    assert!(n < 30, "30 forks under a limit of 8; stderr: {stderr}");
    let (all, stderr) = started(Limits::default());
    assert_eq!(all, 30, "with no limit every fork is allowed; stderr: {stderr}");
}

fn held(memory: &str) -> (std::process::Output, Duration) {
    let _state = StateDir::new();
    let limits = Limits { memory: Some(memory.into()), pids: None };
    let (mut cmd, _) = SeatbeltBackend.run(&perl("darkmux-limits-memory", limits, HOLD_128_MIB));
    let began = Instant::now();
    let out = cmd.output().unwrap();
    (out, began.elapsed())
}

/// A sandbox whose processes hold more than its memory limit is stopped,
/// and says why; one under its limit runs to the end.
#[test]
#[serial_test::serial]
fn a_sandbox_over_its_memory_limit_is_stopped() {
    if !SeatbeltBackend::available() {
        return;
    }
    let (out, took) = held("64m");
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!stdout.contains("survived"), "128 MiB held under a 64 MiB limit; stderr: {stderr}");
    assert!(!out.status.success(), "{out:?}");
    assert!(stderr.contains("over its memory limit of 67108864 bytes"), "the reason is given: {stderr}");
    assert!(took < Duration::from_secs(3), "stopped in {took:?}, not when the program ended");
    let (ok, _) = held("512m");
    assert!(String::from_utf8_lossy(&ok.stdout).contains("survived"), "under its limit it runs: {ok:?}");
}

/// A created sandbox started later carries the same ceilings.
#[test]
#[serial_test::serial]
fn a_started_sandbox_is_held_to_its_memory_limit() {
    if !SeatbeltBackend::available() {
        return;
    }
    let _state = StateDir::new();
    let limits = Limits { memory: Some("64m".into()), pids: None };
    let policy = perl("darkmux-limits-start", limits, HOLD_128_MIB);
    let (mut create, handle) = SeatbeltBackend.create(&policy);
    assert!(create.status().unwrap().success());
    let out = SeatbeltBackend.start(&handle).output().unwrap();
    assert!(!String::from_utf8_lossy(&out.stdout).contains("survived"), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("over its memory limit"), "{out:?}");
}

/// A memory limit that is not a size is refused before anything runs.
#[test]
#[serial_test::serial]
fn a_memory_limit_that_is_not_a_size_is_refused() {
    let _state = StateDir::new();
    let limits = Limits { memory: Some("four gigs".into()), pids: None };
    let (mut cmd, _) = SeatbeltBackend.run(&perl("darkmux-limits-bad", limits, "print \"ran\\n\";"));
    let out = cmd.output().unwrap();
    assert_eq!(out.status.code(), Some(125), "{out:?}");
    assert!(out.stdout.is_empty(), "the program ran");
    assert!(String::from_utf8_lossy(&out.stderr).contains("\"four gigs\""), "{out:?}");
}

/// Docker's size syntax: a whole number, then `b`, `k`, `m` or `g` (binary
/// units, either case, an optional trailing `b`).
#[test]
fn memory_sizes_read_as_docker_reads_them() {
    for (text, bytes) in [("4g", 4u64 << 30), ("512m", 512 << 20), ("64M", 64 << 20), ("1024k", 1 << 20), ("100", 100), ("100b", 100), ("2gb", 2 << 30)] {
        assert_eq!(size_bytes(text), Some(bytes), "{text}");
    }
    for bad in ["", "g", "4x", "-1g", "1.5g", "four gigs", "4 g"] {
        assert_eq!(size_bytes(bad), None, "{bad}");
    }
}
