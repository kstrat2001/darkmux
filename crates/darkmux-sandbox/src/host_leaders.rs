//! The launchers of the sandboxes running on this host itself (Seatbelt,
//! Landlock), read from each backend's own state directory: every sandbox
//! there records its launcher's pid once it starts (`$d/pid`). A process in
//! one of their sandboxes ([`darkmux_platform::Platform::in_sandbox_of`]) is
//! sandboxed work, whatever address it connects from. A Docker sandbox has
//! no launcher on the host; its connections arrive from the engine's
//! forwarder ([`darkmux_platform::CONTAINER_FORWARDERS`]).

use std::path::{Path, PathBuf};

/// Every running-or-started host sandbox's launcher pid, from Seatbelt's
/// and Landlock's state directories. `None` when a state directory exists
/// but cannot be listed: never "none".
pub fn host_sandbox_launchers() -> Option<Vec<u32>> {
    launchers_in(&[
        (crate::seatbelt::state_root(), "profile.sb"),
        (crate::landlock::state_root(), "rules"),
    ])
}

/// The pids recorded under each `(root, marker)`: one per directory that
/// holds the backend's `marker` file (only a directory the backend wrote is
/// a sandbox) and a `pid` that reads as one, each once, ascending. A root
/// that does not exist holds none; one that cannot be listed is `None`.
pub(crate) fn launchers_in(roots: &[(PathBuf, &str)]) -> Option<Vec<u32>> {
    let mut pids = std::collections::BTreeSet::new();
    for (root, marker) in roots {
        pids.extend(launchers_under(root, marker)?);
    }
    Some(pids.into_iter().collect())
}

/// [`launchers_in`] for one root.
fn launchers_under(root: &Path, marker: &str) -> Option<Vec<u32>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(_) => return None,
    };
    let dirs = entries
        .flatten()
        .map(|e| e.path())
        .filter(|d| d.join(marker).is_file());
    Some(
        dirs.filter_map(|d| {
            std::fs::read_to_string(d.join("pid"))
                .ok()?
                .trim()
                .parse()
                .ok()
        })
        .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each backend's sandboxes are found by its own marker; a directory
    /// without one, a sandbox not started yet (no pid), and a pid that does
    /// not read are not launchers. A missing root holds none.
    #[test]
    fn a_launcher_is_a_pid_a_backend_recorded_beside_its_marker() {
        let root = tempfile::TempDir::new().unwrap();
        let at = |name: &str, files: &[(&str, &str)]| {
            let d = root.path().join(name);
            std::fs::create_dir_all(&d).unwrap();
            for (f, body) in files {
                std::fs::write(d.join(f), body).unwrap();
            }
        };
        at(
            "seatbelt-run",
            &[("profile.sb", "(version 1)"), ("pid", "4100\n")],
        );
        at("landlock-run", &[("rules", "{}"), ("pid", "4200\n")]);
        at("not-ours", &[("pid", "4300\n")]);
        at("created", &[("profile.sb", "(version 1)")]);
        at(
            "garbled",
            &[("profile.sb", "(version 1)"), ("pid", "forty\n")],
        );
        let roots = [
            (root.path().to_path_buf(), "profile.sb"),
            (root.path().to_path_buf(), "rules"),
        ];
        assert_eq!(launchers_in(&roots), Some(vec![4100, 4200]));
        assert_eq!(
            launchers_in(&[(root.path().join("absent"), "profile.sb")]),
            Some(vec![])
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
        let answer = launchers_in(&[(locked.clone(), "profile.sb")]);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(answer, None);
    }
}
