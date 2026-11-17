//! The launchers of the sandboxes running on this host itself (Seatbelt,
//! Landlock), read from each backend's own state directory: every sandbox
//! there records its launcher's pid once it starts (`$d/pid`), and when it
//! started (`$d/started`). A process in one of their sandboxes
//! ([`darkmux_platform::Platform::in_sandbox_of`]) is sandboxed work,
//! whatever address it connects from. A Docker sandbox has no launcher on
//! the host; its connections arrive from the engine's forwarder
//! ([`darkmux_platform::CONTAINER_FORWARDERS`]).

use std::path::{Path, PathBuf};

/// Whether the process now at `pid` is positively not the one `recorded`
/// (a sandbox directory's `started`) names: both starts read, in one form,
/// and different. A record that cannot be compared (another form, an older
/// launcher's) or a start that cannot be read is never a mismatch.
pub(crate) type Reused = fn(u32, &str) -> bool;

/// How long reading one start may take: as long as the socket lookups it
/// sits beside (`lsof`, `netstat`: 5 s). One that does not answer is no
/// reading, so no mismatch.
const STARTED_BOUND: std::time::Duration = std::time::Duration::from_secs(5);

/// Every host sandbox's launcher pid, from Seatbelt's and Landlock's state
/// directories. `None` when a state directory exists but cannot be listed:
/// never "none".
pub fn host_sandbox_launchers() -> Option<Vec<u32>> {
    launchers_in(&[
        (crate::seatbelt::state_root(), "profile.sb", seatbelt_reused),
        (crate::landlock::state_root(), "rules", landlock_reused),
    ])
}

/// Seatbelt: a record in [`crate::seatbelt::STARTED_PREFIX`]'s form that
/// differs from the start read now in that same form.
fn seatbelt_reused(pid: u32, recorded: &str) -> bool {
    recorded.starts_with(crate::seatbelt::STARTED_PREFIX)
        && started_by(&crate::seatbelt::STARTED_READER, pid, STARTED_BOUND)
            .is_some_and(|now| now != recorded)
}

/// `pid`'s start as Seatbelt records it, read by `reader` (the command,
/// then `-p <pid>`) within `bound`: `None` when it did not answer, or named
/// no process.
fn started_by(reader: &[&str], pid: u32, bound: std::time::Duration) -> Option<String> {
    let (program, args) = reader.split_first()?;
    let mut cmd = std::process::Command::new(program);
    cmd.args(args).args(["-p", &pid.to_string()]).env_clear();
    let out = darkmux_platform::output_within(cmd, bound)?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !text.is_empty())
        .then(|| format!("{}{text}", crate::seatbelt::STARTED_PREFIX))
}

/// Landlock: a record of start ticks (its launcher's `/proc/<pid>/stat`
/// field) that differs from the ticks read now
/// ([`darkmux_platform::Platform::process_start_stamp`]).
fn landlock_reused(pid: u32, recorded: &str) -> bool {
    let now = darkmux_platform::current().process_start_stamp(pid);
    recorded
        .parse::<u64>()
        .ok()
        .zip(now)
        .is_some_and(|(then, now)| then != now)
}

/// The pids recorded under each `(root, marker, reused)`: one per
/// directory that holds the backend's `marker` file (only a directory the
/// backend wrote is a sandbox) and a `pid` that reads as one
/// ([`launcher_of`]), each once, ascending. A root that does not exist holds
/// none; one that cannot be listed is `None`.
pub(crate) fn launchers_in(roots: &[(PathBuf, &str, Reused)]) -> Option<Vec<u32>> {
    let mut pids = std::collections::BTreeSet::new();
    for (root, marker, reused) in roots {
        pids.extend(launchers_under(root, marker, *reused)?);
    }
    Some(pids.into_iter().collect())
}

/// [`launchers_in`] for one root.
fn launchers_under(root: &Path, marker: &str, reused: Reused) -> Option<Vec<u32>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(_) => return None,
    };
    let dirs = entries
        .flatten()
        .map(|e| e.path())
        .filter(|d| d.join(marker).is_file());
    Some(dirs.filter_map(|d| launcher_of(&d, reused)).collect())
}

/// The launcher pid sandbox directory `d` records, unless that pid now
/// positively runs another process ([`Reused`]: a sandbox left by a crash
/// whose pid was reused). Anything short of that keeps it: a launcher that
/// has exited (what it left in its group or session is still its
/// sandbox's), no start recorded yet, a record or a reading that cannot be
/// compared. Keeping a launcher can only make a process look like
/// sandboxed work, never the operator.
fn launcher_of(d: &Path, reused: Reused) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(d.join("pid"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let recorded = std::fs::read_to_string(d.join("started"))
        .ok()
        .map(|s| s.trim().to_string());
    let gone = recorded.is_some_and(|r| !r.is_empty() && reused(pid, &r));
    (!gone).then_some(pid)
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

    /// Every pid but 4500 (no such process now) started `utc:then`; only a
    /// record in that form compares.
    fn fake_reused(pid: u32, recorded: &str) -> bool {
        pid != 4500 && recorded.starts_with("utc:") && recorded != "utc:then"
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
            (r.to_path_buf(), "profile.sb", fake_reused as Reused),
            (r.to_path_buf(), "rules", fake_reused),
        ];
        assert_eq!(launchers_in(&roots), Some(vec![4100, 4200]));
        assert_eq!(
            launchers_in(&[(r.join("absent"), "profile.sb", fake_reused)]),
            Some(vec![])
        );
    }

    /// Only a positive, comparable mismatch drops a launcher: one started
    /// as recorded, one that has exited, one with no start recorded yet, and
    /// one whose record is in another form are kept.
    #[test]
    fn only_a_comparable_mismatch_makes_a_launcher_pid_reused() {
        let root = tempfile::TempDir::new().unwrap();
        let r = root.path();
        at(
            r,
            "same",
            &[
                ("profile.sb", ""),
                ("pid", "4100\n"),
                ("started", "utc:then\n"),
            ],
        );
        at(
            r,
            "reused",
            &[
                ("profile.sb", ""),
                ("pid", "4200\n"),
                ("started", "utc:long ago\n"),
            ],
        );
        at(
            r,
            "exited",
            &[
                ("profile.sb", ""),
                ("pid", "4500\n"),
                ("started", "utc:long ago\n"),
            ],
        );
        at(r, "starting", &[("profile.sb", ""), ("pid", "4300\n")]);
        at(
            r,
            "older-form",
            &[
                ("profile.sb", ""),
                ("pid", "4400\n"),
                ("started", "mar. 17 nov. 12:12:24 2026\n"),
            ],
        );
        assert_eq!(
            launchers_in(&[(r.to_path_buf(), "profile.sb", fake_reused)]),
            Some(vec![4100, 4300, 4400, 4500])
        );
    }

    /// Each backend's reading of this test process's start matches a record
    /// of it and tells a record of another start apart; a record in another
    /// form (Landlock's that is not ticks, Seatbelt's without its prefix,
    /// as an older launcher wrote it in its own time zone) is never a
    /// mismatch.
    #[cfg(unix)]
    #[test]
    fn each_backend_drops_only_a_start_it_can_compare() {
        let here = std::process::id();
        let now = started_by(&crate::seatbelt::STARTED_READER, here, STARTED_BOUND)
            .expect("ps reads this process's start");
        assert!(!seatbelt_reused(here, &now), "{now}");
        assert!(seatbelt_reused(here, "utc:Mon Jan  1 00:00:00 2001"));
        assert!(
            !seatbelt_reused(here, now.trim_start_matches("utc:")),
            "an unprefixed record"
        );
        if let Some(ticks) = darkmux_platform::current().process_start_stamp(here) {
            assert!(!landlock_reused(here, &ticks.to_string()));
            assert!(landlock_reused(here, &(ticks + 1).to_string()));
        }
        assert!(
            !landlock_reused(here, "Tue Nov 17 04:12:24 2026"),
            "not ticks"
        );
        assert!(
            !seatbelt_reused(u32::MAX / 2, "utc:Mon Jan  1 00:00:00 2001"),
            "no such process"
        );
    }

    /// A record made in any time zone and locale reads as the same start:
    /// the recorder and the reader both run [`crate::seatbelt::STARTED_READER`]
    /// with no environment of their own.
    #[cfg(unix)]
    #[test]
    fn a_start_recorded_under_any_time_zone_or_locale_matches() {
        let here = std::process::id().to_string();
        let reader = crate::seatbelt::STARTED_READER.join(" ");
        let launcher = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                &format!("st=$({reader} -p \"$1\") && printf 'utc:%s\\n' \"$st\""),
                "sh",
                &here,
            ])
            .env("TZ", "Pacific/Kiritimati")
            .env("LANG", "fr_FR.UTF-8")
            .env("LC_ALL", "fr_FR.UTF-8")
            .output()
            .unwrap();
        let recorded = String::from_utf8_lossy(&launcher.stdout).trim().to_string();
        assert!(recorded.starts_with("utc:"), "{recorded}");
        assert!(
            !seatbelt_reused(std::process::id(), &recorded),
            "{recorded}"
        );
    }

    /// A reading that hangs is no reading within its bound, so no mismatch.
    #[cfg(unix)]
    #[test]
    fn a_hung_start_reading_is_no_reading_within_its_bound() {
        let started = std::time::Instant::now();
        let hung = ["/bin/sh", "-c", "exec /bin/sleep 3", "sh"];
        assert_eq!(
            started_by(&hung, 1, std::time::Duration::from_millis(300)),
            None
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
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
        let answer = launchers_in(&[(locked.clone(), "profile.sb", fake_reused)]);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(answer, None);
    }
}
