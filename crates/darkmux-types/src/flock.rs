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
///
/// (#2259) Creates at mode `0o600` on POSIX (owner read/write only). This is
/// the creation point behind the hook outbox (`append_outbox_line`, and the
/// trailing-newline fixup), the audit-log append, the fleet roster and the
/// lab fixture-registry lock — none of which set a mode of their own, so
/// they landed at the process umask default (typically `0o644`,
/// world-readable) instead. Fixed here rather than at each of those call
/// sites, per the module's own "extracted from four independently
/// hand-rolled copies" rationale: one fix covers every present AND future
/// caller.
///
/// NOT every hook file routes through here, and the ones that don't set
/// their own mode: the `.quarantine` sibling opens append-only in
/// `hooks::quarantine_line`, the `.last` status sidecar goes through
/// `hooks::write_owner_only_file`, and outbox COMPACTION renames a fresh
/// temp file over the outbox (so the surviving mode is that temp's, not the
/// one set here). Each is fixed at its own writer.
///
/// `.mode()` applies ONLY at creation — an already-existing file (e.g. one
/// created by a pre-#2259 binary) keeps whatever mode it already has; this
/// function does not retroactively `chmod` it. Deliberate: silently
/// tightening permissions on a file the operator may have intentionally
/// widened (an unlikely but real case for a shared audit log) is a
/// separate, louder decision than "make new files safe by default".
///
/// The consequence, stated plainly because nothing else states it: a
/// pre-#2259 outbox already on disk at `0o644` STAYS world-readable, and
/// there is no check anywhere today that finds or reports it — `darkmux
/// doctor` has no file-mode check. Surfacing that is unbuilt work, not a
/// guarantee this comment can lean on.
pub fn lock_exclusive(path: &Path) -> Result<FlockGuard> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("opening lock file {}", path.display()))?
    };
    #[cfg(not(unix))]
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

/// (#2453) Read-side sibling of `lock_exclusive` — opens `path`
/// READ-ONLY and takes a blocking SHARED `flock(2)` (`LOCK_SH`), so any
/// number of readers proceed together but none can observe the file
/// while an exclusive writer holds it mid-rewrite.
///
/// Two deliberate differences from `lock_exclusive`, both of which exist
/// so this stays usable from a read-only introspection path (`darkmux
/// doctor`, `flow status`) without that path acquiring any authority it
/// doesn't need:
///
/// - **Never creates.** A missing file returns `Ok(None)`, not a
///   freshly-created empty one. A `doctor` run must not bring per-rule
///   state files into existence as a side effect of reporting on them.
/// - **Opens `O_RDONLY`.** `flock(2)` locks the open file DESCRIPTION and
///   imposes no access-mode requirement of its own (unlike `fcntl(2)`
///   record locks), so a shared lock on a read-only handle is legal —
///   which means this works against a file the caller has no write
///   permission for.
///
/// Callers that need the whole load-modify-save transaction still want
/// `with_locked_file`'s exclusive lock; this is only for readers.
pub fn lock_shared_existing(path: &Path) -> Result<Option<FlockGuard>> {
    let file = match OpenOptions::new().read(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow!("opening {} for shared-locked read: {}", path.display(), e));
        }
    };
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH) } != 0 {
        return Err(anyhow!(
            "flock(LOCK_SH) failed on {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(Some(FlockGuard(file)))
}

/// (#2093 merge-gate finding 3) Non-blocking sibling of `lock_exclusive` —
/// `flock(LOCK_EX | LOCK_NB)`. Returns `Ok(Some(guard))` when the lock was
/// acquired, `Ok(None)` when another holder already has it (never blocks
/// waiting), and `Err` only for a genuine I/O failure. The right shape for
/// a periodic background task (e.g. a drainer poll cycle) that should
/// SKIP this round rather than stall behind a slow holder — a blocking
/// `lock_exclusive` would make every concurrent instance queue up single
/// file, defeating the point of running more than one.
pub fn try_lock_exclusive(path: &Path) -> Result<Option<FlockGuard>> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    // (#2259) Same owner-only creation mode as `lock_exclusive` — see its
    // doc comment for the full rationale. Kept in sync deliberately (two
    // call sites, not a shared helper) since each also has its own
    // `#[cfg(unix)]`/`#[cfg(not(unix))]` split per the module doc.
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("opening lock file {}", path.display()))?
    };
    #[cfg(not(unix))]
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening lock file {}", path.display()))?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(FlockGuard(file)));
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(anyhow!("flock(LOCK_EX|LOCK_NB) failed on {}: {}", path.display(), err))
    }
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

    /// (#2093 merge-gate finding 3) `try_lock_exclusive` must never block —
    /// while another holder has the lock, it returns `Ok(None)` instead of
    /// waiting, so a drainer can skip this cycle rather than stall behind
    /// a slow (up to `POST_TIMEOUT`) holder.
    /// (#2453) `flock(2)` imposes no access-mode requirement, so a
    /// SHARED lock on an `O_RDONLY` handle is legal — the property the
    /// read-only `doctor` path depends on. This is a claim about the
    /// kernel, not about our code, so it is executed rather than assumed.
    #[test]
    fn lock_shared_existing_locks_a_read_only_handle() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("status.json");
        std::fs::write(&path, b"{}").unwrap();
        let guard = lock_shared_existing(&path).unwrap();
        assert!(guard.is_some(), "a shared lock on a read-only handle must succeed");
    }

    /// (#2453) Two readers hold the shared lock at the same time — it is
    /// genuinely `LOCK_SH`, not an exclusive lock in disguise (which
    /// would serialize every `doctor` invocation against every other).
    #[test]
    fn lock_shared_existing_admits_concurrent_readers() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("status.json");
        std::fs::write(&path, b"{}").unwrap();
        let _a = lock_shared_existing(&path).unwrap().expect("first reader");
        let b = lock_shared_existing(&path).unwrap();
        assert!(b.is_some(), "a second shared reader must not block behind the first");
    }

    /// (#2453) The whole point: while an exclusive writer holds the file
    /// mid-rewrite, a shared reader cannot get in.
    #[test]
    fn lock_shared_existing_is_excluded_by_an_exclusive_holder() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("status.json");
        std::fs::write(&path, b"{}").unwrap();
        let _held = lock_exclusive(&path).unwrap();
        // Non-blocking probe stands in for "would block" — a blocking
        // `lock_shared_existing` here would hang the test rather than
        // fail it, which is exactly the shape the review brief warns
        // against.
        let rc = unsafe {
            let f = OpenOptions::new().read(true).open(&path).unwrap();
            libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB)
        };
        assert_eq!(rc, -1, "a shared lock must be refused while an exclusive holder has the file");
    }

    /// (#2453) A missing file reports absence rather than creating one —
    /// `doctor` must not materialize per-rule state as a side effect of
    /// reporting on it.
    #[test]
    fn lock_shared_existing_never_creates_the_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("absent.json");
        let guard = lock_shared_existing(&path).unwrap();
        assert!(guard.is_none(), "a missing file must report absence");
        assert!(!path.exists(), "and must NOT have been created");
    }

    #[test]
    fn try_lock_exclusive_returns_none_when_already_held() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("drain.lock");
        let _held = lock_exclusive(&path).unwrap();

        let attempt = try_lock_exclusive(&path).unwrap();
        assert!(attempt.is_none(), "must return Ok(None), never block, while another holder has the lock");
    }

    #[test]
    fn try_lock_exclusive_succeeds_once_released() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("drain.lock");
        {
            let _held = lock_exclusive(&path).unwrap();
        } // released here

        let attempt = try_lock_exclusive(&path).unwrap();
        assert!(attempt.is_some(), "must acquire once the prior holder released");
    }
}
