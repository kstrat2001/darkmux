//! Workspace integrity snapshot + diff.
//!
//! Part of epic #417 (automate-the-human-in-the-loop). This is the
//! *"let me check what the agent actually changed"* recovery — what a
//! human watching a dispatch would do is `git status` after the agent
//! reports done. The automated equivalent: hash the workspace before
//! the dispatch starts and again after it finishes; emit the delta
//! (added / modified / removed / bytes_changed) in qa-reply.json. (#421)
//!
//! Pure observability — no behavioral change. Pairs with #420's claim-
//! verify disagreement detection: the delta tells you *what* changed;
//! the claim-mismatch tells you whether the agent's narrative matches
//! the delta.
//!
//! ## Walk shape
//!
//! - Skip directories that are never source-code or operator-edited:
//!   `.git`, `node_modules`, `target`, `dist`, `build`, `__pycache__`,
//!   `.darkmux-runtime` (per-dispatch runtime droppings; not workspace
//!   content the operator cares about).
//! - Cap per-file size at 50 MB — anything larger is skipped (build
//!   artifacts, binaries, lockfiles in some projects). The cap exists
//!   so a sandbox containing a huge data file doesn't make the
//!   snapshot take seconds.
//! - Cap total file count at 50000 — workspaces beyond this are
//!   probably misconfigured (mounted a parent dir by mistake);
//!   walk bails with `WalkOverflow` so the operator sees the signal
//!   rather than waiting forever.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Hard cap on per-file size — files larger than this are skipped
/// (their existence is still recorded but contents not hashed; the
/// snapshot uses size-only equality for these). 50 MB.
const MAX_FILE_SIZE_BYTES: u64 = 50 * 1024 * 1024;

/// Hard cap on file-count — beyond this, the snapshot bails with
/// `WalkError::Overflow` to surface the misconfiguration.
const MAX_FILE_COUNT: usize = 50_000;

/// Directory names always skipped during the walk. Path-segment match
/// (not glob; we don't want to lint each entry against globs for cost
/// reasons). Add new entries here when new "never workspace content"
/// directories surface.
const SKIPPED_DIR_NAMES: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    "__pycache__",
    ".darkmux-runtime",
    ".venv",
    ".mypy_cache",
    ".pytest_cache",
    ".next",
    // Empirical addition (2026-05-27): the first real-world post-#421
    // dispatch (Beat 47 follow-up validation) surfaced 159 entries
    // in `coverage/` (Jest/Vitest/c8/nyc HTML output from the verify
    // command). These are build artifacts, not operator-meaningful
    // changes — workspace_delta operators want signal about what the
    // AGENT changed, not what the post-dispatch verify command
    // generated as a side effect.
    "coverage",
];

/// Per-file snapshot entry. `hash` is `None` when the file exceeded
/// the per-file size cap (we still track its existence + size, but
/// don't hash it for cost reasons).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FileSnapshot {
    pub size: u64,
    pub hash: Option<[u8; 32]>,
}

/// Snapshot of a workspace at a moment in time. Keyed on relative
/// path (relative to the snapshot root) so two snapshots taken with
/// the same root produce comparable keys.
#[derive(Debug, Clone, Default)]
pub(crate) struct WorkspaceSnapshot {
    pub files: BTreeMap<PathBuf, FileSnapshot>,
}

/// Differential between two snapshots taken at the same root.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct WorkspaceDelta {
    pub added: Vec<PathBuf>,
    pub modified: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
    /// Sum of absolute file-size changes (bytes_after - bytes_before
    /// for modified, full size for added/removed). Approximate; useful
    /// for at-a-glance "did anything substantial change" signal.
    pub total_bytes_changed: u64,
}

#[derive(Debug)]
pub(crate) enum WalkError {
    Overflow { count_at_overflow: usize },
    Io(std::io::Error),
}

impl std::fmt::Display for WalkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalkError::Overflow { count_at_overflow } => write!(
                f,
                "workspace walk exceeded {MAX_FILE_COUNT} file limit (saw {count_at_overflow}); \
                 sandbox likely contains a misconfigured mount"
            ),
            WalkError::Io(e) => write!(f, "io error walking workspace: {e}"),
        }
    }
}

impl std::error::Error for WalkError {}

impl From<std::io::Error> for WalkError {
    fn from(e: std::io::Error) -> Self {
        WalkError::Io(e)
    }
}

/// Walk the workspace and produce a snapshot. Skipped dirs are not
/// descended into. Files over the size cap have their size recorded
/// but `hash` set to `None`.
pub(crate) fn compute_snapshot(root: &Path) -> Result<WorkspaceSnapshot, WalkError> {
    let mut snapshot = WorkspaceSnapshot::default();
    let mut count = 0usize;
    walk_dir(root, root, &mut snapshot, &mut count)?;
    Ok(snapshot)
}

fn walk_dir(
    root: &Path,
    dir: &Path,
    snapshot: &mut WorkspaceSnapshot,
    count: &mut usize,
) -> Result<(), WalkError> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(WalkError::Io(e)),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if SKIPPED_DIR_NAMES.contains(&name) {
                continue;
            }
            walk_dir(root, &path, snapshot, count)?;
        } else if file_type.is_file() {
            *count += 1;
            if *count > MAX_FILE_COUNT {
                return Err(WalkError::Overflow {
                    count_at_overflow: *count,
                });
            }
            let metadata = entry.metadata()?;
            let size = metadata.len();
            let hash = if size > MAX_FILE_SIZE_BYTES {
                None
            } else {
                Some(hash_file(&path)?)
            };
            let rel = path
                .strip_prefix(root)
                .expect("walk descended from root, so strip_prefix is total")
                .to_path_buf();
            snapshot
                .files
                .insert(rel, FileSnapshot { size, hash });
        }
        // Symlinks and other entries silently skipped — they're not
        // workspace content for delta-tracking purposes.
        // (Edge case: a path that was a regular file in `before` and
        // is a directory in `after` reads as `removed` in the delta,
        // because the directory has no entry in `snapshot.files` —
        // only `is_file()` paths are inserted. Same shape in reverse.
        // The behavior is invariant-bearing but easy to miss; named
        // here so future readers don't re-derive it.)
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<[u8; 32], WalkError> {
    let mut hasher = blake3::Hasher::new();
    let mut file = fs::File::open(path)?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

/// Diff two snapshots taken at the same root. Order-stable (sorted
/// via the underlying BTreeMap) so two runs on the same inputs
/// produce byte-identical output.
pub(crate) fn diff_snapshots(before: &WorkspaceSnapshot, after: &WorkspaceSnapshot) -> WorkspaceDelta {
    let mut delta = WorkspaceDelta::default();
    // Iterate the union of paths.
    for (path, after_entry) in &after.files {
        match before.files.get(path) {
            None => {
                delta.added.push(path.clone());
                delta.total_bytes_changed = delta
                    .total_bytes_changed
                    .saturating_add(after_entry.size);
            }
            Some(before_entry) => {
                if file_changed(before_entry, after_entry) {
                    delta.modified.push(path.clone());
                    let diff = after_entry.size.abs_diff(before_entry.size);
                    delta.total_bytes_changed =
                        delta.total_bytes_changed.saturating_add(diff);
                }
            }
        }
    }
    for path in before.files.keys() {
        if !after.files.contains_key(path) {
            delta.removed.push(path.clone());
            let size = before.files[path].size;
            delta.total_bytes_changed = delta.total_bytes_changed.saturating_add(size);
        }
    }
    delta
}

fn file_changed(before: &FileSnapshot, after: &FileSnapshot) -> bool {
    if before.size != after.size {
        return true;
    }
    // Same size — content may still differ. Compare hashes when both
    // are present. When either is None (file too big to hash), size-
    // equality is the best we can do; report as unchanged.
    match (&before.hash, &after.hash) {
        (Some(b), Some(a)) => b != a,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(root: &Path, rel: &str, content: &[u8]) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, content).unwrap();
    }

    // ─── compute_snapshot ────────────────────────────────────────────────

    #[test]
    fn snapshot_of_empty_dir_is_empty() {
        let tmp = TempDir::new().unwrap();
        let s = compute_snapshot(tmp.path()).unwrap();
        assert!(s.files.is_empty());
    }

    #[test]
    fn snapshot_of_nonexistent_dir_is_empty() {
        // A missing workspace path is harmless — empty snapshot, not error.
        let tmp = TempDir::new().unwrap();
        let bogus = tmp.path().join("does-not-exist");
        let s = compute_snapshot(&bogus).unwrap();
        assert!(s.files.is_empty());
    }

    #[test]
    fn snapshot_records_files_with_size_and_hash() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.txt", b"hello");
        write(tmp.path(), "sub/b.txt", b"world");

        let s = compute_snapshot(tmp.path()).unwrap();
        assert_eq!(s.files.len(), 2);
        let a = s.files.get(Path::new("a.txt")).unwrap();
        assert_eq!(a.size, 5);
        assert!(a.hash.is_some());
        let b = s.files.get(Path::new("sub/b.txt")).unwrap();
        assert_eq!(b.size, 5);
        assert!(b.hash.is_some());
    }

    #[test]
    fn snapshot_skips_known_dirs() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/main.rs", b"fn main(){}");
        write(tmp.path(), "node_modules/lib.js", b"x");
        write(tmp.path(), "target/debug/foo", b"x");
        write(tmp.path(), ".git/HEAD", b"x");
        write(tmp.path(), ".darkmux-runtime/trajectory.jsonl", b"x");

        let s = compute_snapshot(tmp.path()).unwrap();
        let keys: Vec<_> = s.files.keys().collect();
        assert_eq!(keys, vec![&PathBuf::from("src/main.rs")]);
    }

    #[test]
    fn snapshot_skips_coverage_dir() {
        // Empirical addition surfaced by the Beat 47 follow-up
        // dispatch — verify-generated Jest/Vitest HTML coverage
        // output was producing 159 false "modified" entries.
        // Regression guard: a future change that drops `coverage`
        // from the skip list will fail this test.
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/main.rs", b"fn main(){}");
        write(tmp.path(), "coverage/index.html", b"<html/>");
        write(tmp.path(), "coverage/src/foo.ts.html", b"<html/>");
        write(tmp.path(), "coverage/lcov-report/index.html", b"<html/>");

        let s = compute_snapshot(tmp.path()).unwrap();
        let keys: Vec<_> = s.files.keys().collect();
        assert_eq!(keys, vec![&PathBuf::from("src/main.rs")]);
    }

    #[test]
    fn snapshot_handles_oversized_file_without_hashing() {
        // Create a file slightly over the cap. We hash-pathway it
        // without hashing — size is still recorded.
        let tmp = TempDir::new().unwrap();
        // Use sparse-write to avoid actually writing 50 MB in the test.
        let p = tmp.path().join("big.bin");
        let f = fs::File::create(&p).unwrap();
        f.set_len(MAX_FILE_SIZE_BYTES + 1).unwrap();
        drop(f);

        let s = compute_snapshot(tmp.path()).unwrap();
        let entry = s.files.get(Path::new("big.bin")).unwrap();
        assert_eq!(entry.size, MAX_FILE_SIZE_BYTES + 1);
        assert!(entry.hash.is_none(), "oversize file should be size-only");
    }

    // ─── diff_snapshots ──────────────────────────────────────────────────

    #[test]
    fn diff_no_changes_produces_empty_delta() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.txt", b"hello");
        let s1 = compute_snapshot(tmp.path()).unwrap();
        let s2 = compute_snapshot(tmp.path()).unwrap();
        let delta = diff_snapshots(&s1, &s2);
        assert!(delta.added.is_empty());
        assert!(delta.modified.is_empty());
        assert!(delta.removed.is_empty());
        assert_eq!(delta.total_bytes_changed, 0);
    }

    #[test]
    fn diff_detects_added_file() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.txt", b"hello");
        let before = compute_snapshot(tmp.path()).unwrap();
        write(tmp.path(), "b.txt", b"newcontent");
        let after = compute_snapshot(tmp.path()).unwrap();
        let delta = diff_snapshots(&before, &after);
        assert_eq!(delta.added, vec![PathBuf::from("b.txt")]);
        assert!(delta.modified.is_empty());
        assert!(delta.removed.is_empty());
        assert_eq!(delta.total_bytes_changed, 10);
    }

    #[test]
    fn diff_detects_removed_file() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.txt", b"hello");
        write(tmp.path(), "b.txt", b"world");
        let before = compute_snapshot(tmp.path()).unwrap();
        fs::remove_file(tmp.path().join("b.txt")).unwrap();
        let after = compute_snapshot(tmp.path()).unwrap();
        let delta = diff_snapshots(&before, &after);
        assert_eq!(delta.removed, vec![PathBuf::from("b.txt")]);
        assert!(delta.added.is_empty());
        assert!(delta.modified.is_empty());
        assert_eq!(delta.total_bytes_changed, 5);
    }

    #[test]
    fn diff_detects_modified_file_via_size_change() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.txt", b"hello");
        let before = compute_snapshot(tmp.path()).unwrap();
        write(tmp.path(), "a.txt", b"hello world");
        let after = compute_snapshot(tmp.path()).unwrap();
        let delta = diff_snapshots(&before, &after);
        assert_eq!(delta.modified, vec![PathBuf::from("a.txt")]);
        assert_eq!(delta.total_bytes_changed, 6); // 11 - 5
    }

    #[test]
    fn diff_detects_modified_file_via_hash_change_with_same_size() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.txt", b"hello");
        let before = compute_snapshot(tmp.path()).unwrap();
        // Same size, different content — hash-based detection catches it.
        write(tmp.path(), "a.txt", b"world");
        let after = compute_snapshot(tmp.path()).unwrap();
        let delta = diff_snapshots(&before, &after);
        assert_eq!(delta.modified, vec![PathBuf::from("a.txt")]);
        // Size diff is 0 in bytes-changed accounting; modified set
        // captures the change qualitatively.
        assert_eq!(delta.total_bytes_changed, 0);
    }

    #[test]
    fn diff_handles_mixed_add_modify_remove() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "kept.txt", b"unchanged");
        write(tmp.path(), "removed.txt", b"bye");
        write(tmp.path(), "modified.txt", b"old");
        let before = compute_snapshot(tmp.path()).unwrap();

        fs::remove_file(tmp.path().join("removed.txt")).unwrap();
        write(tmp.path(), "modified.txt", b"new-content");
        write(tmp.path(), "added.txt", b"fresh");
        let after = compute_snapshot(tmp.path()).unwrap();

        let delta = diff_snapshots(&before, &after);
        assert_eq!(delta.added, vec![PathBuf::from("added.txt")]);
        assert_eq!(delta.removed, vec![PathBuf::from("removed.txt")]);
        assert_eq!(delta.modified, vec![PathBuf::from("modified.txt")]);
        // added: 5, removed: 3, modified: |11-3| = 8 → total 16
        assert_eq!(delta.total_bytes_changed, 16);
    }

    #[test]
    fn diff_is_deterministic_across_runs_on_same_inputs() {
        // BTreeMap iteration is sorted, so delta vec ordering should
        // be stable run-to-run on the same inputs.
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "z.txt", b"z");
        write(tmp.path(), "a.txt", b"a");
        write(tmp.path(), "m.txt", b"m");
        let before = compute_snapshot(tmp.path()).unwrap();

        write(tmp.path(), "new-z.txt", b"new z");
        write(tmp.path(), "new-a.txt", b"new a");
        let after = compute_snapshot(tmp.path()).unwrap();

        let d1 = diff_snapshots(&before, &after);
        let d2 = diff_snapshots(&before, &after);
        assert_eq!(d1, d2);
        // Order is sorted (BTreeMap iteration):
        assert_eq!(d1.added, vec![PathBuf::from("new-a.txt"), PathBuf::from("new-z.txt")]);
    }

    #[test]
    fn diff_oversize_files_with_same_size_treated_as_unchanged() {
        // When both before and after have the same size for an
        // oversize file (hash=None on both sides), we don't have
        // evidence of change. Report as unchanged. This is the
        // pragmatic cap; operator-relevant edits land on
        // source-sized files anyway.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("big.bin");
        let f = fs::File::create(&p).unwrap();
        f.set_len(MAX_FILE_SIZE_BYTES + 100).unwrap();
        drop(f);
        let s1 = compute_snapshot(tmp.path()).unwrap();
        let s2 = compute_snapshot(tmp.path()).unwrap();
        let delta = diff_snapshots(&s1, &s2);
        assert!(delta.modified.is_empty());
    }
}
