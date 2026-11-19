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
        host_services: Vec::new(),
        refused_ports: Vec::new(),
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
    // The reason is the last line, the one a dispatch error's stderr tail
    // shows, and the shell's own "Killed: 9" notice is not printed.
    let last = stderr.lines().last().unwrap_or_default();
    assert!(last.starts_with("seatbelt: the job was stopped: its processes used "), "{stderr}");
    assert!(last.contains("over the 64 MiB sandbox memory limit (sandbox.memory)"), "{stderr}");
    assert!(last.contains("darkmux config set sandbox.memory <size>"), "{stderr}");
    assert!(!stderr.contains("Killed"), "the shell's notice is suppressed: {stderr}");
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
    assert!(String::from_utf8_lossy(&out.stderr).contains("over the 64 MiB sandbox memory limit"), "{out:?}");
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
    assert!(String::from_utf8_lossy(&out.stderr).contains("`four gigs` is not a size"), "{out:?}");
}

/// A program that dies by a signal the memory watch did not send keeps the
/// shell's own notice of it, as it did before the watch.
#[test]
#[serial_test::serial]
fn a_program_killed_by_its_own_signal_keeps_the_shells_notice() {
    if !SeatbeltBackend::available() {
        return;
    }
    let _state = StateDir::new();
    for (signal, code, notice) in [("SEGV", 139, "Segmentation fault: 11"), ("ABRT", 134, "Abort trap: 6")] {
        let limits = Limits { memory: Some("512m".into()), pids: Some(64) };
        let (mut cmd, _) = SeatbeltBackend.run(&perl("darkmux-limits-signal", limits, &format!("kill(\"{signal}\", $$);")));
        let out = cmd.output().unwrap();
        assert_eq!(out.status.code(), Some(code), "{signal}: {out:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains(notice), "{signal}: {out:?}");
    }
}

/// (#3112) What `run` and `create` decide before planning, on every OS: a
/// name that could leave the state root and a memory limit that is not one
/// are refused; otherwise the directory and the launcher's limit arguments
/// (pids, bytes, the limit as a person reads it; empty for none).
#[test]
#[serial_test::serial]
fn a_plan_refuses_a_bad_name_or_memory_and_otherwise_carries_the_limits() {
    let _state = StateDir::new();
    let mut p = perl("darkmux-plan-1", Limits { memory: Some("64m".into()), pids: Some(8) }, "1;");
    let (dir, limits) = SeatbeltBackend::planned(&p).unwrap_or_else(|_| panic!("a good policy was refused"));
    assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some("darkmux-plan-1"));
    assert_eq!(limits, ["8".to_string(), (64u64 << 20).to_string(), "64 MiB".to_string()]);
    p.limits = Limits::default();
    assert!(matches!(SeatbeltBackend::planned(&p), Ok((_, l)) if l.iter().all(String::is_empty)));
    let stderr = |cmd: Box<Command>| -> String { cmd.get_args().last().map(|a| a.to_string_lossy().into_owned()).unwrap_or_default() };
    p.limits = Limits { memory: Some("lots".into()), pids: None };
    let bad_memory = SeatbeltBackend::planned(&p).expect_err("`lots` is refused");
    assert!(stderr(bad_memory).contains("`lots` is not a size"));
    p.name = "../escape".into();
    let bad_name = SeatbeltBackend::planned(&p).expect_err("an escaping name is refused");
    assert_eq!(stderr(bad_name), "../escape");
}

/// (#3112) `create`'s command, on every OS: built, never run. A refused
/// policy gets the refusal naming why; any other gets the script that
/// writes the sandbox's directory, its profile, labels and start script.
#[test]
#[serial_test::serial]
fn a_create_command_is_the_refusal_or_the_script_that_writes_the_sandbox() {
    let _state = StateDir::new();
    let args = |cmd: &Command| cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>();
    let refused = perl("darkmux-create-1", Limits { memory: Some("0".into()), pids: None }, "1;");
    let (cmd, handle) = SeatbeltBackend::create_command(&refused);
    assert_eq!((handle.backend, handle.id.as_str()), (BackendKind::Seatbelt, "darkmux-create-1"));
    assert!(args(&cmd).last().is_some_and(|m| m.contains("`0` is below the 6m")), "{:?}", args(&cmd));
    let good = perl("darkmux-create-2", Limits { memory: Some("64m".into()), pids: Some(8) }, "print 1;");
    let (cmd, handle) = SeatbeltBackend::create_command(&good);
    assert_eq!(handle.id, "darkmux-create-2");
    let a = args(&cmd);
    assert!(a[1].contains("\"$d/start.sh\""), "writes the start script: {}", a[1]);
    assert!(a[3].ends_with("darkmux-create-2"), "the sandbox's directory: {a:?}");
    assert!(a[4].starts_with("(version 1)"), "the profile: {}", a[4]);
    assert!(a[6].contains("'8' '67108864' '64 MiB' '/usr/bin/perl'"), "the start script carries the limits: {}", a[6]);
}

/// (#3112) A created sandbox's start script runs the launcher with only
/// the sandbox's environment, then its directory, working directory, the
/// three limit arguments in order and the program, each one quoted.
#[test]
fn a_start_script_carries_the_limits_between_the_workdir_and_the_program() {
    let mut p = perl("darkmux-start-1", Limits::default(), "print 1;");
    p.workdir = Some("/w d".into());
    let script = SeatbeltBackend::start_script(&p, Path::new("/s/darkmux-start-1"), ["8".into(), "67108864".into(), "64 MiB".into()]);
    assert!(script.starts_with("exec /usr/bin/env -i "), "{script}");
    assert!(script.ends_with(" sh '/s/darkmux-start-1' '/w d' '8' '67108864' '64 MiB' '/usr/bin/perl' '-e' 'print 1;'\n"), "{script}");
    assert!(script.contains("'TMPDIR=/s/darkmux-start-1/tmp'"), "the sandbox's own tmp: {script}");
}

/// A memory limit Docker would read as no limit (`0`) is refused by both
/// backends before anything runs, Docker's without calling `docker`.
#[test]
#[serial_test::serial]
fn a_zero_memory_limit_is_refused_by_both_backends() {
    let _state = StateDir::new();
    let limits = Limits { memory: Some("0".into()), pids: None };
    let policy = perl("darkmux-limits-zero", limits, "print \"ran\\n\";");
    for (name, (mut cmd, _)) in [("seatbelt", SeatbeltBackend.run(&policy)), ("docker", crate::DockerBackend.run(&policy)), ("docker create", crate::DockerBackend.create(&policy))] {
        assert_ne!(cmd.get_program(), "docker", "{name} would run docker");
        let out = cmd.output().unwrap();
        assert_eq!(out.status.code(), Some(125), "{name}: {out:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("`0` is below the 6m"), "{name}: {out:?}");
    }
}
