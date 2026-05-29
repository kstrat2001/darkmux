//! Content hash of a sandbox directory (#488).
//!
//! Phase 1 of the lab-reproducibility cluster (#487). Each per-run
//! sandbox gets a `final_hash` recorded in its run manifest. Phase 2
//! adds `baseline_hash` (the source fixture's hash at clone time) so
//! `dm lab compare` can mechanically verify two runs were controlled
//! experiments against the same fixture state.
//!
//! ## What's in the hash (Phase 1, simple defaults)
//!
//! - Every regular file under the sandbox, EXCLUDING:
//!   - `.darkmux-runtime/` (the runtime's own bookkeeping — not the model's
//!     contribution to the sandbox state)
//!   - `node_modules/`, `target/`, `__pycache__/`, `.git/` (derived /
//!     irrelevant; including them makes hash unstable across machines)
//!
//! Phase 2 layers fixture-manifest-driven include/exclude rules on top.
//!
//! ## Determinism
//!
//! Files are walked in sorted order by relative path. Hash inputs are
//! `(rel_path, file_content)` pairs — `(rel_path)` is hashed first as
//! a length-prefixed string, then the file's bytes. Same content +
//! same layout → same hash regardless of mtimes, walk order, or
//! filesystem inode order.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Default exclusion set for Phase 1. These are the "definitely
/// shouldn't be in the hash" entries for any reasonable sandbox.
/// Phase 2 lets fixture manifests declare additional excludes.
const DEFAULT_EXCLUDES: &[&str] = &[
    ".darkmux-runtime",
    "node_modules",
    "target",
    "__pycache__",
    ".git",
    ".coverage",
];

/// Compute a content hash of `sandbox_dir`, applying default
/// exclusions. Output is the BLAKE3 hash prefixed with `blake3:`.
///
/// Returns an error if `sandbox_dir` doesn't exist or can't be walked.
pub(crate) fn hash_sandbox_dir(sandbox_dir: &Path) -> Result<String> {
    hash_sandbox_dir_with_excludes(sandbox_dir, DEFAULT_EXCLUDES)
}

/// Same as `hash_sandbox_dir` but with a caller-supplied exclusion
/// set. Used by Phase 2 (fixture manifest) and tests.
pub(crate) fn hash_sandbox_dir_with_excludes(sandbox_dir: &Path, excludes: &[&str]) -> Result<String> {
    if !sandbox_dir.exists() {
        return Err(anyhow::anyhow!(
            "sandbox dir does not exist: {}",
            sandbox_dir.display()
        ));
    }
    if !sandbox_dir.is_dir() {
        return Err(anyhow::anyhow!(
            "sandbox path is not a directory: {}",
            sandbox_dir.display()
        ));
    }

    let mut files = Vec::new();
    collect_files(sandbox_dir, sandbox_dir, excludes, &mut files)?;
    files.sort();

    let mut hasher = blake3::Hasher::new();
    for (rel_path, kind) in &files {
        // Length-prefixed path to avoid collision between
        // (foo, "bar") and (foob, "ar"). (#494) Path bytes come from the
        // raw OsStr on Unix, not `to_string_lossy`, so a non-UTF8
        // filename hashes deterministically instead of collapsing to
        // U+FFFD (which two machines could disagree on).
        let path_bytes = os_path_bytes(rel_path);
        hasher.update(&(path_bytes.len() as u64).to_le_bytes());
        hasher.update(&path_bytes);

        let abs = sandbox_dir.join(rel_path);
        match kind {
            EntryKind::File => {
                // Unchanged from Phase 1: (content_len, content). A
                // file-only sandbox with UTF-8 paths hashes byte-for-byte
                // identically to before this change.
                let content = std::fs::read(&abs)
                    .with_context(|| format!("reading {}", abs.display()))?;
                hasher.update(&(content.len() as u64).to_le_bytes());
                hasher.update(&content);
            }
            EntryKind::Symlink => {
                // (#494) Symlinks were silently skipped, so two sandboxes
                // differing only in a symlink hashed identically. Hash the
                // link TARGET (length-prefixed) under a distinguishing
                // marker; never follow it, so there's no loop risk.
                let target = std::fs::read_link(&abs)
                    .with_context(|| format!("read_link {}", abs.display()))?;
                hasher.update(SYMLINK_MARKER);
                let target_bytes = os_path_bytes(&target);
                hasher.update(&(target_bytes.len() as u64).to_le_bytes());
                hasher.update(&target_bytes);
            }
        }
    }

    Ok(format!("blake3:{}", hasher.finalize().to_hex()))
}

/// Marker mixed into the hash for a symlink entry so its (target) block
/// can't be confused with a regular file's content block. (#494)
const SYMLINK_MARKER: &[u8] = b"\0darkmux-symlink\0";

/// Raw path bytes for hashing. On Unix this is the exact `OsStr` byte
/// content (no lossy UTF-8 substitution); elsewhere it falls back to a
/// lossy UTF-8 view (Windows paths are UTF-16 and rarely non-representable
/// in practice, and the lab runs on POSIX). (#494)
#[cfg(unix)]
fn os_path_bytes(p: &Path) -> std::borrow::Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;
    std::borrow::Cow::Borrowed(p.as_os_str().as_bytes())
}
#[cfg(not(unix))]
fn os_path_bytes(p: &Path) -> std::borrow::Cow<'_, [u8]> {
    std::borrow::Cow::Owned(p.to_string_lossy().into_owned().into_bytes())
}

/// What kind of filesystem entry contributes to the hash. (#494)
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum EntryKind {
    File,
    Symlink,
}

/// Recursively collect file paths relative to `root`, skipping any
/// entry whose name (file or dir) matches an excluded entry.
fn collect_files(
    dir: &Path,
    root: &Path,
    excludes: &[&str],
    out: &mut Vec<(PathBuf, EntryKind)>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("read_dir {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if excludes.iter().any(|e| name_str.as_ref() == *e) {
            continue;
        }
        // `file_type()` from read_dir does NOT follow symlinks, so a
        // symlink reports `is_symlink()` (not is_dir/is_file) — we never
        // traverse INTO it, avoiding loops.
        let file_type = entry.file_type()?;
        let rel = || {
            path.strip_prefix(root)
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|_| path.clone())
        };
        if file_type.is_dir() {
            collect_files(&path, root, excludes, out)?;
        } else if file_type.is_file() {
            out.push((rel(), EntryKind::File));
        } else if file_type.is_symlink() {
            // (#494) Record the symlink itself; the hash uses its target,
            // not the (unfollowed) referent's contents.
            out.push((rel(), EntryKind::Symlink));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn same_content_same_hash() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        for dir in [&a, &b] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("file.txt"), "hello world").unwrap();
            std::fs::write(dir.join("other.md"), "# header").unwrap();
        }
        assert_eq!(
            hash_sandbox_dir(&a).unwrap(),
            hash_sandbox_dir(&b).unwrap()
        );
    }

    #[test]
    fn different_content_different_hash() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("file.txt"), "hello world").unwrap();
        std::fs::write(b.join("file.txt"), "different content").unwrap();
        assert_ne!(
            hash_sandbox_dir(&a).unwrap(),
            hash_sandbox_dir(&b).unwrap()
        );
    }

    #[test]
    fn nested_dirs_hashed_consistently() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        for dir in [&a, &b] {
            std::fs::create_dir_all(dir.join("sub/deep")).unwrap();
            std::fs::write(dir.join("sub/deep/file.txt"), "deep").unwrap();
            std::fs::write(dir.join("top.txt"), "top").unwrap();
        }
        assert_eq!(
            hash_sandbox_dir(&a).unwrap(),
            hash_sandbox_dir(&b).unwrap()
        );
    }

    #[test]
    fn excluded_dirs_dont_affect_hash() {
        // Two sandboxes with identical real content but DIFFERENT
        // content inside excluded dirs — hashes must match.
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        for dir in [&a, &b] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("real.txt"), "real content").unwrap();
            std::fs::create_dir_all(dir.join("node_modules/sub")).unwrap();
        }
        std::fs::write(a.join("node_modules/sub/a-only.txt"), "a-only").unwrap();
        std::fs::write(b.join("node_modules/sub/b-only.txt"), "b-only").unwrap();
        std::fs::create_dir_all(a.join(".darkmux-runtime")).unwrap();
        std::fs::write(
            a.join(".darkmux-runtime/trajectory.jsonl"),
            "different in a",
        )
        .unwrap();
        assert_eq!(
            hash_sandbox_dir(&a).unwrap(),
            hash_sandbox_dir(&b).unwrap(),
            "excluded dirs must not affect hash"
        );
    }

    #[test]
    fn output_format_prefixed() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "x").unwrap();
        let h = hash_sandbox_dir(tmp.path()).unwrap();
        assert!(h.starts_with("blake3:"), "hash should be prefixed: {h}");
        // BLAKE3 hex is 64 chars; with prefix total = 71.
        assert_eq!(h.len(), 7 + 64, "unexpected hash length: {h}");
    }

    #[test]
    fn rejects_missing_dir() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope");
        let err = hash_sandbox_dir(&missing).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "got: {err}");
    }

    #[test]
    fn rejects_file_path() {
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("file.txt");
        std::fs::write(&f, "x").unwrap();
        let err = hash_sandbox_dir(&f).unwrap_err();
        assert!(err.to_string().contains("not a directory"), "got: {err}");
    }

    #[test]
    fn path_length_prefixing_prevents_collision() {
        // Two sandboxes that, without length-prefixing, could produce
        // the same hash because their byte streams concatenate to the
        // same thing.
        //
        // Sandbox A: file "ab" containing "cd"
        // Sandbox B: file "abc" containing "d"
        // Without length-prefix: stream = "ab" + "cd" vs "abc" + "d" → same!
        // With length-prefix: streams diverge.
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("ab"), "cd").unwrap();
        std::fs::write(b.join("abc"), "d").unwrap();
        assert_ne!(
            hash_sandbox_dir(&a).unwrap(),
            hash_sandbox_dir(&b).unwrap()
        );
    }

    // ─── (#494) symlinks + non-UTF8 path determinism (Unix) ──────────

    #[cfg(unix)]
    #[test]
    fn symlink_presence_affects_hash() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        for dir in [&a, &b] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("real.txt"), "content").unwrap();
        }
        // `b` additionally has a symlink. Pre-#494 these hashed equal
        // (symlink silently skipped); now the symlink contributes.
        symlink("real.txt", b.join("link.txt")).unwrap();
        assert_ne!(
            hash_sandbox_dir(&a).unwrap(),
            hash_sandbox_dir(&b).unwrap(),
            "a sandbox with a symlink must hash differently from one without"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_target_change_affects_hash() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        for dir in [&a, &b] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("x.txt"), "content").unwrap();
            std::fs::write(dir.join("y.txt"), "content").unwrap();
        }
        // Same content everywhere; only the symlink TARGET differs.
        symlink("x.txt", a.join("link")).unwrap();
        symlink("y.txt", b.join("link")).unwrap();
        assert_ne!(
            hash_sandbox_dir(&a).unwrap(),
            hash_sandbox_dir(&b).unwrap(),
            "retargeting a symlink must change the hash"
        );
    }

    #[cfg(unix)]
    #[test]
    fn identical_symlinks_hash_equal() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        for dir in [&a, &b] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("t.txt"), "c").unwrap();
            symlink("t.txt", dir.join("link")).unwrap();
        }
        assert_eq!(
            hash_sandbox_dir(&a).unwrap(),
            hash_sandbox_dir(&b).unwrap(),
            "identical content + identical symlinks must hash equal"
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_filename_hashes_deterministically_and_distinctly() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let tmp = TempDir::new().unwrap();
        let s = tmp.path().join("s");
        std::fs::create_dir_all(&s).unwrap();
        // 0xFF 0xFE is not valid UTF-8 — `to_string_lossy` would map both
        // this and a different invalid sequence to U+FFFD, colliding.
        let name1 = OsStr::from_bytes(b"bad\xff\xfe.txt");
        std::fs::write(s.join(name1), "data").unwrap();
        // Stable across repeated hashing (no panic, no lossy nondeterminism).
        let h1 = hash_sandbox_dir(&s).unwrap();
        assert_eq!(h1, hash_sandbox_dir(&s).unwrap());

        // A different non-UTF8 name with the same content must NOT collide
        // (it would under to_string_lossy → both U+FFFD).
        let s2 = tmp.path().join("s2");
        std::fs::create_dir_all(&s2).unwrap();
        let name2 = OsStr::from_bytes(b"bad\xfe\xff.txt");
        std::fs::write(s2.join(name2), "data").unwrap();
        assert_ne!(
            h1,
            hash_sandbox_dir(&s2).unwrap(),
            "distinct non-UTF8 filenames must hash distinctly (no lossy collision)"
        );
    }

    #[test]
    fn deterministic_across_walks() {
        let tmp = TempDir::new().unwrap();
        let s = tmp.path().join("s");
        std::fs::create_dir_all(s.join("z")).unwrap();
        std::fs::create_dir_all(s.join("a")).unwrap();
        std::fs::write(s.join("z/file.txt"), "z").unwrap();
        std::fs::write(s.join("a/file.txt"), "a").unwrap();
        // Compute many times — same result every time (sorts internally).
        let first = hash_sandbox_dir(&s).unwrap();
        for _ in 0..5 {
            assert_eq!(hash_sandbox_dir(&s).unwrap(), first);
        }
    }
}
