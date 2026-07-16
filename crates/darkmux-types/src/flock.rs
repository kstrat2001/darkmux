//! Shared cross-process `flock(2)` RAII guard.
//!
//! Extracted from four independently hand-rolled, byte-identical copies:
//! `darkmux-lab`'s fixture-registry lock (`lab/registry.rs`),
//! `darkmux-flow`'s audit-log append lock (`integrity.rs`),
//! `darkmux-fleet`'s roster lock (`roster.rs`), and the e2e test harness's
//! release-build lock (`tests/e2e/harness.rs`). darkmux-types is already a
//! dependency leaf of all four, so this is the natural home.
//!
//! POSIX-only (`flock(2)` has no Windows equivalent) — every caller keeps
//! its own `#[cfg(unix)]` / `#[cfg(not(unix))]` split; the fallback branch
//! (plain load-modify-save, no cross-process serialization) is caller-
//! specific and stays at each call site rather than living here.

use anyhow::{anyhow, Context, Result};
use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// RAII guard releasing an exclusive `flock(2)` lock on drop. Owns the
/// locked `File` directly (rather than a bare `RawFd` alongside a
/// separately-owned `File`, the shape all four original copies used) so
/// there is exactly one thing to keep alive, and exactly one thing whose
/// drop matters — no risk of the file closing (and so releasing the lock
/// via the kernel's close-releases-all-locks rule) before the guard's own
/// `Drop` runs.
pub struct FlockGuard(File);

impl FlockGuard {
    /// Access the underlying locked file — for callers (e.g. an
    /// append-only audit log) that read/write the SAME file they locked,
    /// rather than using a sidecar `.lock` file purely for mutual
    /// exclusion.
    pub fn file(&mut self) -> &mut File {
        &mut self.0
    }
}

impl Drop for FlockGuard {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Open (creating if absent) `path` and acquire a blocking exclusive
/// `flock(2)` on it, returning a guard that releases the lock when
/// dropped. Creates the parent directory if it doesn't exist yet.
pub fn lock_exclusive(path: &Path) -> Result<FlockGuard> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening lock file {}", path.display()))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(anyhow!(
            "flock(LOCK_EX) failed on {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(FlockGuard(file))
}

/// Run `f` while holding an exclusive lock on `path`, then return `f`'s
/// result. Lock acquired → closure runs (with access to the locked file
/// handle) → lock released when the guard drops at the end of this
/// function — i.e. strictly after `f` returns.
///
/// This is the right shape for a transaction that's fully self-contained
/// within one closure (load → mutate → save, or read → append). A caller
/// whose lock-release timing must be visibly pinned to a specific later
/// statement (see `darkmux-fleet::roster::mutate_roster`'s explicit
/// early drop, timed to release only after its atomic rename) keeps its
/// own open-coded `lock_exclusive` call instead — the guard type is
/// still shared, just not routed through this convenience wrapper.
pub fn with_locked_file<F, T>(path: &Path, f: F) -> Result<T>
where
    F: FnOnce(&mut File) -> Result<T>,
{
    let mut guard = lock_exclusive(path)?;
    f(guard.file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};
    use tempfile::TempDir;

    #[test]
    fn lock_exclusive_creates_parent_dir_and_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nested/deep/registry.json.lock");
        let guard = lock_exclusive(&path);
        assert!(guard.is_ok());
        assert!(path.exists());
    }

    #[test]
    fn with_locked_file_runs_closure_and_releases_after() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("audit.jsonl");
        let result = with_locked_file(&path, |file| {
            file.write_all(b"hello\n")?;
            Ok(42)
        });
        assert_eq!(result.unwrap(), 42);

        // Lock is released — a second acquisition on the same path
        // succeeds immediately rather than blocking.
        let mut guard2 = lock_exclusive(&path).expect("second lock should succeed");
        let mut contents = String::new();
        guard2.file().seek(SeekFrom::Start(0)).unwrap();
        guard2.file().read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "hello\n");
    }

    #[test]
    fn with_locked_file_propagates_closure_error() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("lock");
        let result: Result<()> = with_locked_file(&path, |_file| Err(anyhow!("boom")));
        assert!(result.is_err());
    }
}
