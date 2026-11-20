//! (#3569) The one way darkmux writes `profiles.json`: under a lock every
//! writer shares, only when the file still holds what the writer read, and
//! atomically.
//!
//! - **One lock.** `<file>.lock` beside the file, an exclusive `flock(2)`
//!   (`darkmux_types::flock`), held from the re-read to the rename, so two
//!   writers never interleave and a reader never sees half a file.
//! - **Compare, then write.** The file is re-read under the lock; when it no
//!   longer holds what the writer planned from, nothing is written
//!   ([`WriteError::Changed`]) and the writer reads it again.
//! - **Atomic.** The new text goes to a file of its own beside the target
//!   (a name no other writer uses), takes the target's permissions, and is
//!   renamed over it.
//! - **Through a link.** A `profiles.json` that is a symlink stays one: the
//!   file it names is the one written, as a plain write would do.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Why a write did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    /// The file no longer holds what the writer read; nothing was written.
    Changed,
    /// The lock, the read, the write or the rename failed.
    Failed(String),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::Changed => {
                f.write_str("profiles.json changed since it was read; nothing was written")
            }
            WriteError::Failed(why) => f.write_str(why),
        }
    }
}

/// The file `path` names: the file a symlink points to, else `path` (which
/// may not exist yet). A link whose target does not exist is refused,
/// naming both: replacing it would put a plain file where the link was.
pub fn target(path: &Path) -> Result<PathBuf, WriteError> {
    let is_link = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    match std::fs::canonicalize(path) {
        Ok(real) => Ok(real),
        Err(_) if is_link => {
            let points_to = std::fs::read_link(path)
                .map(|t| t.display().to_string())
                .unwrap_or_else(|_| "?".into());
            Err(WriteError::Failed(format!(
                "{} is a symlink to {points_to}, which does not exist; restore that file or remove the link",
                path.display()
            )))
        }
        Err(_) => Ok(path.to_path_buf()),
    }
}

/// The lock every `profiles.json` writer takes: `<file>.lock` beside the
/// file `path` names. Held until the guard drops.
pub fn lock(path: &Path) -> Result<darkmux_types::flock::FlockGuard, WriteError> {
    let target = target(path)?;
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "profiles.json".into());
    darkmux_types::flock::lock_exclusive(&target.with_file_name(format!("{name}.lock")))
        .map_err(|e| WriteError::Failed(format!("{e:#}")))
}

/// What the file `path` names holds now: `None` when there is no file.
pub fn read_now(path: &Path) -> Result<Option<String>, WriteError> {
    match std::fs::read_to_string(target(path)?) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(WriteError::Failed(format!(
            "reading {}: {e}",
            path.display()
        ))),
    }
}

/// Write `text` to `path` when it still holds `expected` (`None`: no file
/// yet), taking the lock for the whole check and write.
pub fn replace(path: &Path, expected: Option<&str>, text: &str) -> Result<(), WriteError> {
    let _held = lock(path)?;
    replace_locked(path, expected, text)
}

/// Remove the file `path` names when it still holds `expected` (what the
/// remover wrote), under the lock: a file another writer changed is kept.
pub fn remove(path: &Path, expected: &str) -> Result<(), WriteError> {
    let _held = lock(path)?;
    if read_now(path)?.as_deref() != Some(expected) {
        return Err(WriteError::Changed);
    }
    let target = target(path)?;
    std::fs::remove_file(&target)
        .map_err(|e| WriteError::Failed(format!("removing {}: {e}", target.display())))
}

/// [`replace`] for a caller that already holds [`lock`].
pub fn replace_locked(path: &Path, expected: Option<&str>, text: &str) -> Result<(), WriteError> {
    if read_now(path)?.as_deref() != expected {
        return Err(WriteError::Changed);
    }
    write_atomic(&target(path)?, text)
}

/// A name for a file beside the target that no other writer, in this
/// process or another, uses.
fn temp_beside(target: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    target.with_file_name(format!(".{name}.{}.{nanos}.{n}.tmp", std::process::id()))
}

/// `text` into a new file beside `target` (with its permissions, when it
/// exists), renamed over it. A target the operator made read-only is
/// refused, as a plain write to it is: replacing it would get round that.
fn write_atomic(target: &Path, text: &str) -> Result<(), WriteError> {
    use std::io::Write;
    let next = temp_beside(target);
    let failed = |what: &str, e: std::io::Error| {
        WriteError::Failed(format!("{what} {}: {e}", target.display()))
    };
    if target.exists() {
        std::fs::OpenOptions::new()
            .write(true)
            .open(target)
            .map_err(|e| failed("writing", e))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&next)
        .map_err(|e| failed("writing beside", e))?;
    let written = file
        .write_all(text.as_bytes())
        .and_then(|_| file.sync_all());
    let kept = std::fs::metadata(target)
        .map(|m| m.permissions())
        .and_then(|p| std::fs::set_permissions(&next, p));
    let renamed = written.and_then(|_| match kept {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => std::fs::rename(&next, target),
    });
    renamed.map_err(|e| {
        let _ = std::fs::remove_file(&next);
        failed("replacing", e)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a file that still holds what the writer read is replaced; one
    /// that changed, or appeared, is left as it is.
    #[test]
    fn a_file_is_replaced_only_while_it_holds_what_was_read() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("profiles.json");
        assert_eq!(
            replace(&file, Some("{}"), "{\"a\":1}"),
            Err(WriteError::Changed),
            "no file is not `{{}}`"
        );
        replace(&file, None, "{}").unwrap();
        assert_eq!(
            replace(&file, None, "{\"b\":1}"),
            Err(WriteError::Changed),
            "it exists now"
        );
        assert_eq!(
            replace(&file, Some("{ }"), "{\"b\":1}"),
            Err(WriteError::Changed)
        );
        replace(&file, Some("{}"), "{\"b\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{\"b\":1}");
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            left.iter()
                .all(|n| n == "profiles.json" || n == "profiles.json.lock"),
            "no temp file left: {left:?}"
        );
    }

    /// A symlinked `profiles.json` stays a link; the file it names is the
    /// one written, with its permissions kept.
    #[test]
    fn a_symlinked_registry_stays_a_link_and_its_target_is_written() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles").join("profiles.json");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "{}").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("profiles.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        replace(&link, Some("{}"), "{\"x\":1}").unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "still a link"
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "{\"x\":1}");
        assert_eq!(
            std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    /// Writers racing on one file: every write that reports success holds
    /// what it read, the file is always one whole write, and every other
    /// writer is told it changed.
    #[test]
    fn racing_writers_never_interleave_and_each_loser_is_told() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("profiles.json");
        std::fs::write(&file, "0").unwrap();
        for round in 0..200 {
            let before = std::fs::read_to_string(&file).unwrap();
            let outcomes: Vec<Result<(), WriteError>> = std::thread::scope(|s| {
                let racers: Vec<_> = (0..2)
                    .map(|i| {
                        let (file, before) = (&file, &before);
                        s.spawn(move || {
                            replace(
                                file,
                                Some(before),
                                &format!("{round}-{i}-{}", "x".repeat(4096)),
                            )
                        })
                    })
                    .collect();
                racers.into_iter().map(|r| r.join().unwrap()).collect()
            });
            let won = outcomes.iter().filter(|o| o.is_ok()).count();
            assert_eq!(
                won, 1,
                "round {round}: exactly one write lands: {outcomes:?}"
            );
            assert!(
                outcomes.contains(&Err(WriteError::Changed)),
                "round {round}"
            );
            let now = std::fs::read_to_string(&file).unwrap();
            assert!(
                now.starts_with(&format!("{round}-"))
                    && now.len() == format!("{round}-0-").len() + 4096,
                "round {round}: whole: {}",
                now.len()
            );
        }
    }

    /// A registry the operator made read-only is not replaced, and nothing
    /// is left beside it.
    #[test]
    fn a_read_only_registry_is_refused_as_a_plain_write_is() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("profiles.json");
        std::fs::write(&file, "{}").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();
        if std::fs::OpenOptions::new().write(true).open(&file).is_ok() {
            return; // running as a user who may write anything
        }
        let refused = replace(&file, Some("{}"), "{\"x\":1}").unwrap_err();
        assert!(
            matches!(&refused, WriteError::Failed(m) if m.contains("writing")),
            "{refused:?}"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{}");
        let n = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(n, 0);
    }

    /// Each write's file beside the target has a name of its own, in the
    /// target's directory: a file a crashed writer left can never block the
    /// next write (`create_new`), and no two writers share one.
    #[test]
    fn each_write_gets_a_file_of_its_own_beside_the_target() {
        let target = Path::new("/some/dir/profiles.json");
        let (a, b) = (temp_beside(target), temp_beside(target));
        assert_ne!(a, b);
        for t in [&a, &b] {
            assert_eq!(t.parent(), target.parent());
            let name = t.file_name().unwrap().to_string_lossy();
            assert!(
                name.starts_with(".profiles.json.") && name.ends_with(".tmp"),
                "{name}"
            );
        }
    }

    /// A `profiles.json` that is a link to a file that does not exist is
    /// refused naming both, never replaced by a plain file.
    #[test]
    fn a_dangling_link_is_refused_naming_it_and_its_missing_target() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("profiles.json");
        std::os::unix::fs::symlink(dir.path().join("missing").join("real.json"), &link).unwrap();
        let refused = replace(&link, None, "{}").unwrap_err();
        assert!(
            matches!(&refused, WriteError::Failed(m) if m.contains("profiles.json") && m.contains("missing/real.json")),
            "{refused:?}"
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "still a link"
        );
        assert!(!dir.path().join("missing").exists());
        assert!(remove(&link, "{}").is_err());
    }

    /// A read that fails for any reason but a missing file is an error
    /// naming the path, never "no file": a writer must not plan from it.
    #[test]
    fn a_read_that_fails_is_an_error_not_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("profiles.json");
        assert_eq!(read_now(&file), Ok(None), "no file");
        std::fs::create_dir(&file).unwrap();
        let refused = read_now(&file).unwrap_err();
        assert!(
            matches!(&refused, WriteError::Failed(m) if m.starts_with("reading ") && m.contains("profiles.json")),
            "{refused:?}"
        );
    }

    /// Reading through a dangling link is refused the way writing through
    /// one is, naming the link.
    #[test]
    fn reading_through_a_dangling_link_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("profiles.json");
        std::os::unix::fs::symlink(dir.path().join("missing.json"), &link).unwrap();
        let refused = read_now(&link).unwrap_err();
        assert!(
            matches!(&refused, WriteError::Failed(m) if m.contains("is a symlink to") && m.contains("missing.json")),
            "{refused:?}"
        );
    }

    /// A removal takes the lock and removes only the file that still holds
    /// what the remover wrote; one another writer changed is kept.
    #[test]
    fn a_removal_removes_only_what_the_remover_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("profiles.json");
        std::fs::write(&file, "theirs").unwrap();
        assert_eq!(remove(&file, "mine"), Err(WriteError::Changed));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "theirs");
        assert_eq!(remove(&file, "theirs"), Ok(()));
        assert!(!file.exists());
        assert_eq!(
            remove(&file, "theirs"),
            Err(WriteError::Changed),
            "nothing there to remove"
        );
    }
}
