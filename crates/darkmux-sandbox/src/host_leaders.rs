//! The launchers of the sandboxes running on this host itself (Seatbelt,
//! Landlock), read from each backend's own state directory: every sandbox
//! there records its launcher's pid once it starts (`$d/pid`), and when it
//! started (`$d/started`). A process in one of their sandboxes
//! ([`darkmux_platform::Platform::in_sandbox_of`]) is sandboxed work,
//! whatever address it connects from. A Docker sandbox has no launcher on
//! the host; its connections arrive from the engine's forwarder
//! ([`darkmux_platform::CONTAINER_FORWARDERS`]).

use std::path::{Path, PathBuf};

/// When process `pid` started, in the form its backend records in
/// `$d/started`; `None` when no process has that pid (or it cannot be read).
pub(crate) type StartedNow = fn(u32) -> Option<String>;

/// Every host sandbox's launcher pid, from Seatbelt's and Landlock's state
/// directories. `None` when a state directory exists but cannot be listed:
/// never "none".
pub fn host_sandbox_launchers() -> Option<Vec<u32>> {
    launchers_in(&[
        (
            crate::seatbelt::state_root(),
            "profile.sb",
            seatbelt_started,
        ),
        (crate::landlock::state_root(), "rules", landlock_started),
    ])
}

/// Seatbelt's record of a start: `ps -o lstart=`, read exactly as its
/// `ALIVE` check reads it (`ps` on [`crate::seatbelt::DEFAULT_PATH`], no
/// other environment), so the two agree on whether a launcher is the one
/// recorded. `lstart` is local time in the C locale only in that
/// environment: a `TZ` or locale would change the text.
fn seatbelt_started(pid: u32) -> Option<String> {
    let mut cmd = std::process::Command::new("ps");
    cmd.args(["-o", "lstart=", "-p", &pid.to_string()])
        .env_clear()
        .env("PATH", crate::seatbelt::DEFAULT_PATH);
    let out = cmd.output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}

/// Landlock's record of a start: the start time in clock ticks from
/// `/proc/<pid>/stat`, as its launcher writes it and its `ALIVE` check
/// reads it ([`darkmux_platform::Platform::process_start_stamp`]).
fn landlock_started(pid: u32) -> Option<String> {
    darkmux_platform::current()
        .process_start_stamp(pid)
        .map(|t| t.to_string())
}

/// The pids recorded under each `(root, marker, started_now)`: one per
/// directory that holds the backend's `marker` file (only a directory the
/// backend wrote is a sandbox) and a `pid` that reads as one
/// ([`launcher_of`]), each once, ascending. A root that does not exist holds
/// none; one that cannot be listed is `None`.
pub(crate) fn launchers_in(roots: &[(PathBuf, &str, StartedNow)]) -> Option<Vec<u32>> {
    let mut pids = std::collections::BTreeSet::new();
    for (root, marker, started_now) in roots {
        pids.extend(launchers_under(root, marker, *started_now)?);
    }
    Some(pids.into_iter().collect())
}

/// [`launchers_in`] for one root.
fn launchers_under(root: &Path, marker: &str, started_now: StartedNow) -> Option<Vec<u32>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(_) => return None,
    };
    let dirs = entries
        .flatten()
        .map(|e| e.path())
        .filter(|d| d.join(marker).is_file());
    Some(dirs.filter_map(|d| launcher_of(&d, started_now)).collect())
}

/// The launcher pid sandbox directory `d` records, unless that pid now
/// runs another process: one that started at another time than `d`
/// recorded (a sandbox left by a crash whose pid was reused), as the
/// backend's own `ALIVE` check reads it. A launcher that has exited is kept
/// (what it left in its group or session is still its sandbox's), as is one
/// whose start cannot be compared (none recorded yet, or none read): either
/// way it can only make a process look like sandboxed work, never the
/// operator.
fn launcher_of(d: &Path, started_now: StartedNow) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(d.join("pid"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let recorded = std::fs::read_to_string(d.join("started"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let reused = match (recorded, started_now(pid)) {
        (Some(then), Some(now)) => then != now,
        _ => false,
    };
    (!reused).then_some(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sandbox directory under `root` holding `files`.
    fn at(root: &Path, name: &str, files: &[(&str, &str)]) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        for (f, body) in files {
            std::fs::write(d.join(f), body).unwrap();
        }
    }

    /// Every pid started `then`, but 4500, which no process has now.
    fn started_then(pid: u32) -> Option<String> {
        (pid != 4500).then(|| "then".to_string())
    }

    /// Each backend's sandboxes are found by its own marker; a directory
    /// without one, a sandbox not started yet (no pid), and a pid that does
    /// not read are not launchers. A missing root holds none.
    #[test]
    fn a_launcher_is_a_pid_a_backend_recorded_beside_its_marker() {
        let root = tempfile::TempDir::new().unwrap();
        let r = root.path();
        at(
            r,
            "seatbelt-run",
            &[("profile.sb", "(version 1)"), ("pid", "4100\n")],
        );
        at(r, "landlock-run", &[("rules", "{}"), ("pid", "4200\n")]);
        at(r, "not-ours", &[("pid", "4300\n")]);
        at(r, "created", &[("profile.sb", "(version 1)")]);
        at(
            r,
            "garbled",
            &[("profile.sb", "(version 1)"), ("pid", "forty\n")],
        );
        let roots = [
            (r.to_path_buf(), "profile.sb", started_then as StartedNow),
            (r.to_path_buf(), "rules", started_then),
        ];
        assert_eq!(launchers_in(&roots), Some(vec![4100, 4200]));
        assert_eq!(
            launchers_in(&[(r.join("absent"), "profile.sb", started_then)]),
            Some(vec![])
        );
    }

    /// A recorded pid that now runs a process started at another time is a
    /// reused pid, not the launcher; one started when recorded, one that has
    /// exited, and one with no start recorded yet are kept.
    #[test]
    fn a_reused_launcher_pid_is_not_a_launcher() {
        let root = tempfile::TempDir::new().unwrap();
        let r = root.path();
        at(
            r,
            "same",
            &[("profile.sb", ""), ("pid", "4100\n"), ("started", "then\n")],
        );
        at(
            r,
            "reused",
            &[
                ("profile.sb", ""),
                ("pid", "4200\n"),
                ("started", "long ago\n"),
            ],
        );
        at(
            r,
            "exited",
            &[
                ("profile.sb", ""),
                ("pid", "4500\n"),
                ("started", "long ago\n"),
            ],
        );
        at(r, "starting", &[("profile.sb", ""), ("pid", "4300\n")]);
        assert_eq!(
            launchers_in(&[(r.to_path_buf(), "profile.sb", started_then)]),
            Some(vec![4100, 4300, 4500])
        );
    }

    /// Each backend's reading of a start is the form its launcher records:
    /// this test process's own start reads the same twice, and a sandbox
    /// directory naming this process with another start is a reused pid.
    #[cfg(unix)]
    #[test]
    fn each_backend_reads_a_start_as_its_launcher_records_it() {
        let here = std::process::id();
        assert!(
            seatbelt_started(here).is_some(),
            "ps reads this process's start"
        );
        for read in [seatbelt_started as StartedNow, landlock_started] {
            // A platform that reads no start stamps has no Landlock either.
            let Some(now) = read(here) else { continue };
            assert_eq!(read(here), Some(now.clone()));
            let root = tempfile::TempDir::new().unwrap();
            at(
                root.path(),
                "live",
                &[
                    ("profile.sb", ""),
                    ("pid", &here.to_string()),
                    ("started", &format!("{now}\n")),
                ],
            );
            at(
                root.path(),
                "stale",
                &[
                    ("rules", ""),
                    ("pid", &here.to_string()),
                    ("started", "Mon Jan  1 00:00:00 2001\n"),
                ],
            );
            assert_eq!(
                launchers_in(&[(root.path().to_path_buf(), "profile.sb", read)]),
                Some(vec![here]),
                "started as recorded"
            );
            assert_eq!(
                launchers_in(&[(root.path().to_path_buf(), "rules", read)]),
                Some(vec![]),
                "a reused pid"
            );
        }
        assert_eq!(seatbelt_started(u32::MAX / 2), None, "no such process");
    }

    /// Seatbelt's reading matches what a launcher records whatever this
    /// process's own `TZ`: the launcher's `ps` runs with no `TZ`, and so
    /// does this one.
    #[cfg(unix)]
    #[test]
    fn seatbelt_reads_a_start_in_the_launchers_environment() {
        let here = std::process::id().to_string();
        let launcher = std::process::Command::new("/bin/sh")
            .args(["-c", "ps -o lstart= -p \"$1\"", "sh", &here])
            .env_clear()
            .env("PATH", crate::seatbelt::DEFAULT_PATH)
            .output()
            .unwrap();
        let recorded = String::from_utf8_lossy(&launcher.stdout).trim().to_string();
        let prev = std::env::var_os("TZ");
        unsafe { std::env::set_var("TZ", "Pacific/Kiritimati") };
        let read = seatbelt_started(std::process::id());
        match prev {
            Some(v) => unsafe { std::env::set_var("TZ", v) },
            None => unsafe { std::env::remove_var("TZ") },
        }
        assert_eq!(read, Some(recorded));
    }

    /// A state directory that exists but cannot be listed is no answer.
    #[cfg(unix)]
    #[test]
    fn an_unlistable_state_directory_is_no_answer() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::TempDir::new().unwrap();
        let locked = root.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let answer = launchers_in(&[(locked.clone(), "profile.sb", started_then)]);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(answer, None);
    }
}
