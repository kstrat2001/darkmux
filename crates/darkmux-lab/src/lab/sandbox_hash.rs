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
    for rel_path in &files {
        // Length-prefixed path to avoid collision between
        // (foo, "bar") and (foob, "ar").
        let path_str = rel_path.to_string_lossy();
        let path_bytes = path_str.as_bytes();
        hasher.update(&(path_bytes.len() as u64).to_le_bytes());
        hasher.update(path_bytes);

        let abs = sandbox_dir.join(rel_path);
        let content = std::fs::read(&abs)
            .with_context(|| format!("reading {}", abs.display()))?;
        hasher.update(&(content.len() as u64).to_le_bytes());
        hasher.update(&content);
    }

    Ok(format!("blake3:{}", hasher.finalize().to_hex()))
}

/// Recursively collect file paths relative to `root`, skipping any
/// entry whose name (file or dir) matches an excluded entry.
fn collect_files(
    dir: &Path,
    root: &Path,
    excludes: &[&str],
    out: &mut Vec<PathBuf>,
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
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files(&path, root, excludes, out)?;
        } else if file_type.is_file() {
            // Store relative path so the hash is stable across moves.
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_path_buf())
                .unwrap_or(path);
            out.push(rel);
        }
        // Symlinks: skip silently for now — sandboxes shouldn't rely on
        // them, and following could cause loops. Phase 2 can add a
        // manifest opt-in if needed.
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
