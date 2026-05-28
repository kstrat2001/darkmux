//! Copy-on-write directory clone for per-run sandbox isolation (#488).
//!
//! Phase 1 of the lab-reproducibility cluster (#487). Each `darkmux lab
//! run` invocation needs its own isolated sandbox dir — the source
//! fixture must NOT be touched, and the per-run sandbox must be fast +
//! cheap to create.
//!
//! ## Strategy
//!
//! 1. **macOS / APFS**: `cp -c -R` uses `clonefile(2)` under the hood —
//!    near-instant, near-zero disk delta, COW on write.
//! 2. **Linux with reflink-capable FS (btrfs/xfs/zfs)**: `cp
//!    --reflink=auto -r` does the same.
//! 3. **Fallback (any platform)**: `cp -R` (deep copy). Slow for large
//!    fixtures but always correct.
//!
//! We shell out to `cp` rather than pulling in the `reflink-copy` crate
//! per the "don't add dependencies casually" doctrine. `cp` is on every
//! POSIX system the runtime targets.
//!
//! ## What this is NOT
//!
//! - Not a sync utility — destination must not exist.
//! - Not a Windows implementation — darkmux is POSIX-only.

use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::process::Command;

/// COW-clone `src` to `dst`. `dst` must not exist; parent dir of `dst`
/// must exist (the caller's responsibility — typically `run_dir`).
///
/// On macOS uses `cp -c -R` (clonefile). On Linux tries `cp
/// --reflink=auto -r` first; falls back to `cp -R` if the reflink
/// option isn't available on that `cp`. Returns the path to `dst`
/// on success.
pub fn cow_clone_dir(src: &Path, dst: &Path) -> Result<()> {
    if !src.exists() {
        return Err(anyhow!("source dir does not exist: {}", src.display()));
    }
    if dst.exists() {
        return Err(anyhow!(
            "destination already exists: {} (clone refuses to overwrite)",
            dst.display()
        ));
    }
    if let Some(parent) = dst.parent() {
        if !parent.exists() {
            return Err(anyhow!(
                "destination parent does not exist: {} (caller must create)",
                parent.display()
            ));
        }
    }

    let attempts = cow_attempts();
    let mut last_err: Option<String> = None;
    for attempt in &attempts {
        let mut cmd = Command::new("cp");
        for arg in attempt.iter() {
            cmd.arg(arg);
        }
        cmd.arg(src);
        cmd.arg(dst);
        match cmd.output() {
            Ok(out) if out.status.success() => return Ok(()),
            Ok(out) => {
                last_err = Some(format!(
                    "cp {:?} returned status {}: {}",
                    attempt,
                    out.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
                // Clean up partial dst before retrying with the next attempt.
                let _ = std::fs::remove_dir_all(dst);
            }
            Err(e) => {
                last_err = Some(format!("cp {:?} failed to spawn: {}", attempt, e));
            }
        }
    }
    Err(anyhow!(
        "all cp attempts failed for {} → {}: {}",
        src.display(),
        dst.display(),
        last_err.unwrap_or_else(|| "no attempts ran".into())
    ))
    .context("cow_clone_dir")
}

/// Per-platform argv attempts in preference order. First success wins;
/// remaining attempts are skipped.
fn cow_attempts() -> Vec<Vec<&'static str>> {
    if cfg!(target_os = "macos") {
        // `cp -c -R` is APFS clonefile on macOS. Fallback to `cp -R`
        // covers tmpfs / non-APFS filesystems used in CI.
        vec![vec!["-c", "-R"], vec!["-R"]]
    } else {
        // Linux: reflink=auto for btrfs/xfs/zfs; fallback to deep copy.
        // BSD's `cp` predates --reflink; falls through to `-R`.
        vec![vec!["--reflink=auto", "-r"], vec!["-r"]]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn clones_simple_dir() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), "alpha").unwrap();
        std::fs::write(src.join("b.txt"), "beta").unwrap();
        let dst = tmp.path().join("dst");

        cow_clone_dir(&src, &dst).unwrap();

        assert!(dst.exists());
        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "alpha");
        assert_eq!(std::fs::read_to_string(dst.join("b.txt")).unwrap(), "beta");
    }

    #[test]
    fn clones_nested_dir_structure() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("sub/deep")).unwrap();
        std::fs::write(src.join("sub/deep/file.txt"), "deep content").unwrap();
        std::fs::write(src.join("top.txt"), "top content").unwrap();
        let dst = tmp.path().join("dst");

        cow_clone_dir(&src, &dst).unwrap();

        assert_eq!(
            std::fs::read_to_string(dst.join("sub/deep/file.txt")).unwrap(),
            "deep content"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("top.txt")).unwrap(),
            "top content"
        );
    }

    #[test]
    fn rejects_missing_source() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("does-not-exist");
        let dst = tmp.path().join("dst");
        let err = cow_clone_dir(&src, &dst).unwrap_err();
        assert!(
            err.to_string().contains("source dir does not exist"),
            "expected missing-source error, got: {}",
            err
        );
    }

    #[test]
    fn rejects_existing_destination() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        let err = cow_clone_dir(&src, &dst).unwrap_err();
        assert!(
            err.to_string().contains("destination already exists"),
            "expected existing-dest error, got: {}",
            err
        );
    }

    #[test]
    fn rejects_missing_destination_parent() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let dst = tmp.path().join("missing-parent").join("dst");
        let err = cow_clone_dir(&src, &dst).unwrap_err();
        assert!(
            err.to_string().contains("destination parent does not exist"),
            "expected missing-parent error, got: {}",
            err
        );
    }

    #[test]
    fn cloned_destination_is_independent_of_source() {
        // The load-bearing invariant: editing the clone must not touch
        // the source. This is what per-run isolation depends on.
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("baseline.txt"), "original").unwrap();
        let dst = tmp.path().join("dst");

        cow_clone_dir(&src, &dst).unwrap();

        // Mutate the clone.
        std::fs::write(dst.join("baseline.txt"), "modified").unwrap();
        std::fs::write(dst.join("new-file.txt"), "added in clone").unwrap();

        // Source must be untouched.
        assert_eq!(
            std::fs::read_to_string(src.join("baseline.txt")).unwrap(),
            "original",
            "source file was mutated by clone edit — COW invariant broken"
        );
        assert!(
            !src.join("new-file.txt").exists(),
            "source got the clone's new file — COW invariant broken"
        );
    }
}
