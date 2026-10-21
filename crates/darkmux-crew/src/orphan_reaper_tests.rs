use super::*;
use serial_test::serial;
use tempfile::TempDir;

const MACHINE: &str = "MACHINE-A";

fn labeled(name: &str, pid: u32, start: u64, machine: &str) -> ListedContainer {
    ListedContainer {
        name: name.to_string(),
        host_pid: Some(pid),
        host_start: Some(start),
        host_machine: Some(machine.to_string()),
    }
}

/// Host pid 100 started at 1000 and is alive; pid 200 is gone; pid 300 is
/// a different process (started at 9999) on a reused pid; pid 400 is alive
/// but its start time cannot be read.
fn liveness(pid: u32) -> HostLiveness {
    match pid {
        100 => HostLiveness::Running { start: Some(1000) },
        300 => HostLiveness::Running { start: Some(9999) },
        400 => HostLiveness::Running { start: None },
        _ => HostLiveness::Gone,
    }
}

#[test]
fn the_reap_rule_table() {
    let unlabeled = ListedContainer {
        name: "darkmux-dispatch-old".to_string(),
        host_pid: None,
        host_start: None,
        host_machine: None,
    };
    let half_labeled = ListedContainer {
        host_start: None,
        ..labeled("darkmux-dispatch-half", 200, 1000, MACHINE)
    };
    let cases: Vec<(&str, ListedContainer, bool)> = vec![
        (
            "alive host -> keep",
            labeled("darkmux-dispatch-alive", 100, 1000, MACHINE),
            false,
        ),
        (
            "dead pid -> reap",
            labeled("darkmux-dispatch-dead", 200, 1000, MACHINE),
            true,
        ),
        (
            "pid reused, start differs -> reap",
            labeled("darkmux-dispatch-reused", 300, 1000, MACHINE),
            true,
        ),
        (
            "start unreadable -> keep",
            labeled("darkmux-dispatch-unreadable", 400, 1000, MACHINE),
            false,
        ),
        ("unlabeled (older darkmux) -> keep", unlabeled, false),
        ("partly labeled -> keep", half_labeled, false),
        (
            "non-darkmux name, dead host -> keep",
            labeled("someone-else", 200, 1000, MACHINE),
            false,
        ),
        (
            "gate container, dead host -> keep",
            labeled("darkmux-gate-200-0", 200, 1000, MACHINE),
            false,
        ),
        (
            "another machine's host -> keep",
            labeled("darkmux-dispatch-peer", 200, 1000, "MACHINE-B"),
            false,
        ),
    ];
    for (why, container, reap) in cases {
        let got =
            orphaned_dispatch_containers(std::slice::from_ref(&container), Some(MACHINE), liveness);
        assert_eq!(!got.is_empty(), reap, "{why}: {got:?}");
        if reap {
            assert_eq!(
                got,
                vec![Orphan {
                    name: container.name.clone(),
                    host_pid: container.host_pid.unwrap()
                }],
                "{why}"
            );
        }
    }
}

#[test]
fn an_unknown_machine_identity_reaps_nothing() {
    let dead = labeled("darkmux-dispatch-dead", 200, 1000, MACHINE);
    assert!(orphaned_dispatch_containers(&[dead], None, liveness).is_empty());
}

#[test]
fn parse_reads_labels_and_treats_empty_as_absent() {
    let out = "darkmux-dispatch-a\t200\t1000\tMACHINE-A\ndarkmux-dispatch-b\t\t\t\n\n";
    let got = parse_docker_ps(out);
    assert_eq!(
        got,
        vec![
            labeled("darkmux-dispatch-a", 200, 1000, MACHINE),
            ListedContainer {
                name: "darkmux-dispatch-b".into(),
                host_pid: None,
                host_start: None,
                host_machine: None
            },
        ]
    );
}

#[test]
fn host_label_args_are_three_labels() {
    let host = HostIdentity {
        pid: 7,
        start: 70,
        machine: "M".into(),
    };
    assert_eq!(
        host_label_args(&host),
        vec![
            "--label",
            "darkmux.dispatch.host-pid=7",
            "--label",
            "darkmux.dispatch.host-start=70",
            "--label",
            "darkmux.dispatch.host-machine=M",
        ]
    );
}

#[test]
fn this_process_is_alive_by_its_own_reading() {
    let pid = std::process::id();
    let HostLiveness::Running { start } = host_liveness(pid) else {
        panic!("own pid reads as gone")
    };
    assert_eq!(start, darkmux_platform::current().process_start_stamp(pid));
}

/// A fake `docker` on `PATH` that lists `ps_output` for `ps` and records
/// every invocation. Returns the record path and the previous `PATH`.
fn install_fake_docker(dir: &TempDir, ps_output: &str) -> (std::path::PathBuf, Option<String>) {
    let record = dir.path().join("invoked-with.txt");
    let listing = dir.path().join("ps.txt");
    std::fs::write(&listing, ps_output).unwrap();
    let docker = dir.path().join("docker");
    std::fs::write(
        &docker,
        format!(
            "#!/bin/sh\necho \"$@\" >> {}\nif [ \"$1\" = ps ]; then cat {}; fi\n",
            record.display(),
            listing.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Warm the first exec (macOS's first-exec cost for a fresh executable).
    std::process::Command::new(&docker)
        .arg("warm-up")
        .status()
        .unwrap();
    std::fs::remove_file(&record).unwrap();
    let prev = std::env::var("PATH").ok();
    // SAFETY: every caller is `#[serial]`.
    unsafe {
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                prev.clone().unwrap_or_default()
            ),
        )
    };
    (record, prev)
}

fn restore_path(prev: Option<String>) {
    // SAFETY: every caller is `#[serial]`.
    unsafe {
        match prev {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
    }
}

/// One tick against a fake docker: it lists dispatch containers only, kills
/// exactly the orphan, and leaves the live host's and the unlabeled one.
#[test]
#[serial]
fn a_tick_lists_once_and_kills_only_the_orphan() {
    let dir = TempDir::new().unwrap();
    let ps = "darkmux-dispatch-alive\t100\t1000\tMACHINE-A\n\
              darkmux-dispatch-dead\t200\t1000\tMACHINE-A\n\
              darkmux-dispatch-old\t\t\t\n";
    let (record, prev) = install_fake_docker(&dir, ps);
    let tick = reap_tick(Some(MACHINE), liveness);
    restore_path(prev);
    let ReapTick::Listed { reaped } = tick else {
        panic!("docker answered: {tick:?}")
    };
    assert_eq!(
        reaped,
        vec![(
            Orphan {
                name: "darkmux-dispatch-dead".into(),
                host_pid: 200
            },
            true
        )]
    );
    let calls = std::fs::read_to_string(&record).unwrap();
    let calls: Vec<&str> = calls.lines().collect();
    assert_eq!(calls.len(), 2, "one ps, one kill: {calls:?}");
    assert!(
        calls[0].starts_with("ps --filter name=^darkmux-dispatch- --format"),
        "{calls:?}"
    );
    assert_eq!(calls[1], "kill darkmux-dispatch-dead");
}

/// No docker on `PATH`: the tick says so and does nothing else.
#[test]
#[serial]
fn a_tick_without_docker_does_nothing() {
    let empty = TempDir::new().unwrap();
    let prev = std::env::var("PATH").ok();
    // SAFETY: `#[serial]`.
    unsafe { std::env::set_var("PATH", empty.path()) };
    let tick = reap_tick(Some(MACHINE), liveness);
    restore_path(prev);
    assert!(matches!(tick, ReapTick::DockerUnavailable), "{tick:?}");
}
