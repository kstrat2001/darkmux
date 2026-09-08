//! (#2158 / #2456) Symlink-refusing primitives for creating
//! darkmux-owned filesystem state at a path this crate does not fully
//! control the provenance of — a caller-named out-dir, a crawl `STOP`
//! file's parent, anything sitting under a directory an attacker with
//! local write access could have pre-planted a symlink or a leftover
//! entry into.
//!
//! Two shapes live here because they answer two different questions:
//!
//! - **"Must not already exist"** (`create_dir_exclusive_0700`,
//!   `create_dir_exclusive_unique_0700`) — a fresh directory darkmux is
//!   about to own outright (a dispatch's out-dir). Anything already at the
//!   path — file, dir, or symlink — is a hazard, so the guard is one
//!   atomic `mkdir(2)`: it either creates the directory or fails, with no
//!   separate exists-check-then-create window for a symlink to win a race
//!   in.
//! - **"May already exist, legitimately"** (`write_file_refusing_symlinks_0600`)
//!   — a file darkmux may need to OVERWRITE on a later, unrelated call
//!   (the thermal breaker's crawl `STOP` file gets re-stamped by a later
//!   mission's own trip — see `thermal_governor::stop_file_body`'s own
//!   doc for why that overwrite is load-bearing, not a bug). `create_new`
//!   alone would wrongly refuse that legitimate second write, so this
//!   shape instead refuses only when a symlink is ALREADY sitting at the
//!   path or its parent, and otherwise replaces the content atomically.
//!
//! **How strong each guard actually is — stated so neither is mistaken
//! for more than it is** (the #2158 review found a check in this family
//! that read as a defense and was a postcondition assertion; the same
//! honesty is owed here). Verdicts below were EXECUTED against
//! `rename(2)`/`symlink_metadata(2)` on this platform, not read off POSIX:
//!
//! - **`create_dir_exclusive_0700` — structurally closed.** One atomic
//!   `mkdir(2)`; there is no window.
//! - **`write_file_refusing_symlinks_0600`, the LEAF (`path` itself) —
//!   structurally closed.** `rename(2)` replaces the destination DIRECTORY
//!   ENTRY without dereferencing it, so a symlink planted at `path` — even
//!   one that wins the race against the check — is replaced, never written
//!   through. Verified: a symlink planted at the destination is left
//!   pointing at a byte-identical target and the destination is a regular
//!   file afterwards. The leaf `symlink_metadata` check is therefore about
//!   LOUDNESS (an operator must hear that a symlink was sitting on a
//!   darkmux state file), not about correctness.
//! - **`write_file_refusing_symlinks_0600`, the PARENT — NARROWED, not
//!   closed.** `rename(2)` resolves the destination's *directory*
//!   components normally, so it gives the parent no protection at all:
//!   verified, a rename into `<symlinked-parent>/STOP` follows the link.
//!   The `symlink_metadata` check therefore does the whole job here, and
//!   it is a check-then-use: it defeats a symlink PRE-PLANTED before the
//!   call (which is #2456's actual reported hazard — plant and wait for a
//!   breaker to trip), and it does NOT defeat an attacker who swaps the
//!   parent between the check and the write. Closing that residue
//!   structurally means pinning the parent as a directory fd
//!   (`open(parent, O_DIRECTORY|O_NOFOLLOW)`, then `openat`/`renameat`
//!   relative to it) — `libc` is already a `cfg(unix)` dependency of this
//!   crate, so it costs no new dependency surface. Not done here: it
//!   trades a `std`-only implementation for `unsafe` FFI in a
//!   security-bearing path, which is a design call, not a review edit.
//! - **Anything ABOVE the parent — NOT covered.** `symlink_metadata` is
//!   only `lstat(2)`: it declines to follow the FINAL component and
//!   dereferences every ancestor silently. Verified: with `<root>/crawl`
//!   planted as a symlink, `symlink_metadata("<root>/crawl/<name>")`
//!   reports a plain directory. Reaching that needs write access to
//!   `<root>` — a strictly stronger position than the "write access to
//!   `<root>/crawl/`" these guards are scoped to — so it is out of the
//!   stated threat model, but it is a real limit on the claim and is
//!   named rather than rounded off.
//!
//! Originally two independent hand-rolled checks in two files
//! (`dispatch_internal.rs`'s `create_dir_exclusive_0700` from #2158, and
//! `thermal_governor.rs`'s `write_stop_file`, left an documented-but-unfixed
//! gap by #2158's own review — filed as #2456). Consolidated here so both
//! consumers share one convention rather than two: a second security-bearing
//! filesystem check invented independently is exactly the outcome #2456's
//! own writeup warned against ("worth fixing with one convention rather
//! than two").

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// (#2158) Create `path` in a way immune to a pre-planted symlink OR a
/// leftover directory sitting at the same name: `create_dir` (never
/// `create_dir_all`) is ONE atomic `mkdir(2)` syscall with no
/// separate exists-check-then-create window, so anything already at
/// `path` — a directory, a file, or a symlink, dangling or not — makes the
/// call fail with `AlreadyExists` rather than being silently reused or
/// followed. This is the strong form: it closes the TOCTOU race
/// structurally, not just narrows it by making the name harder to guess
/// (a random suffix reduces the odds an attacker wins the race; it does
/// not remove the race).
///
/// The `symlink_metadata` call below is a POSTCONDITION ASSERTION, not a
/// defense — stated plainly so it is not mistaken for one. Once
/// `create_dir` has returned `Ok`, `mkdir(2)` has created a real directory
/// at `path`, so the assertion cannot fire: reaching it would require
/// another process to `rmdir` our directory and plant a symlink in the
/// window between the two calls, which the sticky bit on `/tmp` already
/// forbids to anything but the owner. It is kept because it is free and
/// documents the invariant this function promises its callers, and it is
/// deliberately NOT load-bearing: a weaker `mkdir` would not be rescued by
/// a later `stat` either (that is the same TOCTOU window, just moved).
///
/// Locks the result to `0o700` on unix — callers mount this at a
/// container path or write secrets/trajectories into it; no other local
/// user should be able to read or traverse it.
///
/// Used directly by `dispatch_internal.rs`'s `resolve_host_out`'s
/// CALLER-NAMED branch, where an existing path is a caller-contract
/// violation and must stay a hard refusal (the crawl's per-unit collision
/// check depends on exactly that). The AUTO-NAMED sites go through
/// `create_dir_exclusive_unique_0700` below instead.
pub(crate) fn create_dir_exclusive_0700(path: &Path) -> Result<()> {
    if !try_create_dir_exclusive_0700(path)? {
        bail!(
            "darkmux: refusing to create {} — something already exists at that path (a \
             leftover from a prior run, or a planted symlink); never reused or followed",
            path.display()
        );
    }
    Ok(())
}

/// (#2158) The fallible-but-not-fatal core of `create_dir_exclusive_0700`.
/// `Ok(true)` — we created `path`. `Ok(false)` — something was already
/// there, so NOTHING was created, reused or followed; the caller decides
/// whether that is fatal (a caller-named dir) or merely means "pick another
/// name" (an auto-generated one). `Err` is a real I/O failure.
///
/// Separated out precisely so the auto-named callers can tell
/// `AlreadyExists` apart from a genuine error without string-matching an
/// `anyhow` message.
fn try_create_dir_exclusive_0700(path: &Path) -> Result<bool> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => {
            return Err(e).with_context(|| format!("creating directory: {}", path.display()));
        }
    }
    let meta = fs::symlink_metadata(path)
        .with_context(|| format!("stat'ing freshly-created directory: {}", path.display()))?;
    if meta.file_type().is_symlink() {
        bail!(
            "darkmux: refusing to use {} — it resolved as a symlink immediately after creation; \
             not proceeding",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting permissions on directory: {}", path.display()))?;
    }
    Ok(true)
}

/// How many names `create_dir_exclusive_unique_0700` will try before giving
/// up. Generous: every attempt past the first means a real collision, and
/// an attacker who pre-plants all of them earns a LOUD refusal, never a
/// redirect.
pub(crate) const EXCLUSIVE_DIR_ATTEMPTS: u32 = 16;

/// (#2158) The AUTO-NAMED counterpart to `create_dir_exclusive_0700`, for
/// the sites whose directory name darkmux derives itself rather than
/// receiving from a caller: `dispatch_internal.rs`'s `resolve_host_out`
/// `None` branch and the dispatch's auto-workspace tempdir.
///
/// Both derive their name from `<role_id>-<unix_micros>`, which is NOT a
/// uniqueness guarantee — it is a wall clock. Two same-role dispatches that
/// reach `SystemTime::now()` in the same microsecond derive the same name,
/// and because a dispatch's out-dir is deliberately never cleaned, a clock
/// that steps backwards can re-derive the name of a dir still sitting in
/// `temp_dir()`. Making a bare `AlreadyExists` fatal at these two sites
/// would turn the #2158 hardening into an availability cliff: a single
/// leftover directory would refuse that name FOREVER, and concurrent
/// sibling `dispatch.internal` steps (which carry step-derived session ids,
/// so the duplicate-container-name check does NOT cover them) would take
/// each other down.
///
/// So: keep the exclusive create — every attempt is still one atomic
/// `mkdir(2)` that never reuses or follows what is already there — and on a
/// collision try the NEXT name rather than reusing the occupied one. The
/// security property is identical (darkmux only ever writes into a
/// directory it just created itself); only the availability cliff is gone.
/// Returns the path actually created, which the caller must use in place of
/// `base`.
pub(crate) fn create_dir_exclusive_unique_0700(base: &Path) -> Result<PathBuf> {
    let stem = base
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("cannot derive a unique directory name from {}", base.display()))?
        .to_os_string();
    for attempt in 0..EXCLUSIVE_DIR_ATTEMPTS {
        let candidate = if attempt == 0 {
            base.to_path_buf()
        } else {
            let mut name = stem.clone();
            name.push(format!("-{attempt}"));
            base.with_file_name(name)
        };
        if try_create_dir_exclusive_0700(&candidate)? {
            return Ok(candidate);
        }
    }
    bail!(
        "darkmux: refusing to create a dispatch directory — {} and its next {} candidate names \
         are all already taken (stale leftovers under the temp dir, or something planting them); \
         nothing was reused or followed",
        base.display(),
        EXCLUSIVE_DIR_ATTEMPTS - 1
    );
}

/// (#2456) Write `contents` to `path`, refusing rather than following if
/// EITHER `path` or its parent directory is already a symlink.
///
/// This is deliberately NOT `create_dir_exclusive_0700`'s "must not
/// already exist" shape: `thermal_governor.rs`'s `write_stop_file` — the
/// one production caller today — legitimately OVERWRITES a `STOP` file a
/// previous mission's breaker already wrote (see `stop_file_body`'s own
/// doc: nothing ever deletes this file, and a later mission re-stamps it
/// under its own name so it isn't wrongly held by an older run's stop). A
/// bare `create_new` at the final path would refuse that legitimate
/// second write exactly as hard as it refuses an attacker's symlink, which
/// would turn this hardening into an availability regression the first
/// time a workspace saw two thermal trips.
///
/// So the check is split across the two things that actually matter:
///
/// 1. **The parent.** `symlink_metadata` on `path`'s parent — refuse if it
///    is a symlink (the exact #2456 hazard: `<root>/crawl/<name>` planted
///    as a symlink silently redirects everything "under" it) or exists as
///    anything other than a real directory. Create it via `create_dir_all`
///    when genuinely absent — nothing plantable exists yet at a path with
///    no parent directory, so this branch carries no new risk.
/// 2. **The target itself.** `symlink_metadata` on `path` — refuse if it
///    is ALREADY a symlink, rather than silently clobbering it. A refusal
///    here is deliberately loud (this function never silently replaces a
///    planted symlink) even though the write below would be safe against
///    it regardless (see the next paragraph) — an operator finding a
///    symlink planted at a state file darkmux owns is exactly the kind of
///    thing a security-bearing write must surface, not paper over.
///
/// The write itself is atomic and, independently, ALSO immune to a symlink
/// planted at `path` — the LEAF, and only the leaf — in the gap between
/// step 2's check and this write. It gives step 1's PARENT check no such
/// backstop: `rename(2)` declines to dereference only its final component
/// and resolves the destination's directory components normally, so a
/// parent swapped after step 1 is followed. See this module's own
/// "how strong each guard actually is" note for the executed verdicts. A
/// uniquely-named temp file is created in the SAME directory with the
/// `OpenOptions::new().write(true).create_new(true).mode(0o600)` shape
/// (the same O_CREAT|O_EXCL pattern `dispatch_internal.rs`'s
/// `remote_chat_attempt` already uses for its own secret-bearing curl
/// config — safe here for the same reason: the name is unique per call, so
/// nothing could already be planted at it), then `rename`d onto `path`.
/// POSIX `rename(2)` atomically replaces WHATEVER is currently at the
/// destination — a regular file or a symlink — without ever dereferencing
/// it, so even a symlink that won a race against step 2's check gets
/// overwritten as a directory entry, never followed to write through it.
/// This is the same tmp-file-then-rename convention
/// `thermal_governor.rs`'s own `write_pace_file` already uses for its
/// pace file, for the same atomicity reason (a reader must never observe
/// a half-written file).
///
/// Returns `Err` with a human-readable reason on any refusal or I/O
/// failure. Never panics — the one production caller
/// (`thermal_governor::write_stop_file`) is the thermal breaker's LAST
/// ACTION under duress and must not panic or block; it surfaces this
/// `Err` as an operator-facing warning rather than unwrapping it.
pub(crate) fn write_file_refusing_symlinks_0600(path: &Path, contents: &[u8]) -> Result<(), String> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => return Err(format!("{} has no parent directory", path.display())),
    };

    match fs::symlink_metadata(parent) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(format!(
                "refusing to write {} — its parent {} is a symlink, not a real directory",
                path.display(),
                parent.display()
            ));
        }
        Ok(meta) if !meta.is_dir() => {
            return Err(format!(
                "refusing to write {} — its parent {} exists but is not a directory",
                path.display(),
                parent.display()
            ));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(parent)
                .map_err(|e| format!("creating parent directory {}: {e}", parent.display()))?;
        }
        Err(e) => {
            return Err(format!("stat'ing parent directory {}: {e}", parent.display()));
        }
    }

    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(format!(
                "refusing to write {} — it already exists as a symlink, not a regular file",
                path.display()
            ));
        }
    }

    let tmp_name = format!(
        ".{}.tmp.{}.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("write"),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let tmp_path = parent.join(tmp_name);

    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp_path)
            .map_err(|e| format!("creating temp file {}: {e}", tmp_path.display()))?;
        // Clean up on failure too — a half-written temp left behind in
        // `<root>/crawl/<name>/` outlives the process and is exactly the
        // kind of litter the next operator has to explain to themselves.
        f.write_all(contents).map_err(|e| {
            let _ = std::fs::remove_file(&tmp_path);
            format!("writing temp file {}: {e}", tmp_path.display())
        })?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&tmp_path, contents)
            .map_err(|e| format!("writing temp file {}: {e}", tmp_path.display()))?;
    }

    std::fs::rename(&tmp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        format!("renaming temp file onto {}: {e}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_file_refusing_symlinks_refuses_a_symlinked_parent() {
        let dir = tempfile::tempdir().unwrap();
        let real_target = dir.path().join("real-target");
        fs::create_dir_all(&real_target).unwrap();
        let linked_parent = dir.path().join("linked-parent");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_target, &linked_parent).unwrap();

        let victim = linked_parent.join("STOP");
        let err = write_file_refusing_symlinks_0600(&victim, b"thermal-critical\n").unwrap_err();
        assert!(err.contains("symlink"), "unexpected error: {err}");
        assert!(
            !real_target.join("STOP").exists(),
            "must not follow the symlinked parent onto the real target"
        );
    }

    #[test]
    fn write_file_refusing_symlinks_refuses_a_symlinked_target() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("crawl-root");
        fs::create_dir_all(&parent).unwrap();
        let attacker_target = dir.path().join("attacker-owned-file");
        std::fs::write(&attacker_target, b"pre-existing\n").unwrap();
        let victim = parent.join("STOP");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&attacker_target, &victim).unwrap();

        let err = write_file_refusing_symlinks_0600(&victim, b"thermal-critical\n").unwrap_err();
        assert!(err.contains("symlink"), "unexpected error: {err}");
        assert_eq!(
            std::fs::read_to_string(&attacker_target).unwrap(),
            "pre-existing\n",
            "must never write through a symlink planted at the target itself"
        );
    }

    /// (#2456) The LEAF guarantee — "even a symlink that won the race
    /// against the check gets REPLACED, never written through" — rests
    /// entirely on a platform property of `rename(2)`. Pin the property
    /// itself rather than trusting the citation: if a future platform (or
    /// a future `std::fs::rename` shim) ever dereferenced a symlinked
    /// destination, the guard above would silently become the only thing
    /// standing between a planted link and the breaker's write, and
    /// nothing else in this suite would notice.
    #[test]
    fn rename_replaces_a_symlinked_destination_rather_than_following_it() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, b"ORIGINAL\n").unwrap();
        let dest = dir.path().join("STOP");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&victim, &dest).unwrap();
        let tmp = dir.path().join(".STOP.tmp");
        fs::write(&tmp, b"replacement\n").unwrap();

        fs::rename(&tmp, &dest).unwrap();

        assert_eq!(
            fs::read_to_string(&victim).unwrap(),
            "ORIGINAL\n",
            "rename must not write through the symlinked destination"
        );
        let meta = fs::symlink_metadata(&dest).unwrap();
        assert!(!meta.file_type().is_symlink(), "the symlink entry must be replaced");
        assert!(meta.is_file());
        assert_eq!(fs::read_to_string(&dest).unwrap(), "replacement\n");
    }

    #[test]
    fn write_file_refusing_symlinks_writes_and_overwrites_the_ordinary_case() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("crawl-root");
        let path = parent.join("STOP");

        write_file_refusing_symlinks_0600(&path, b"thermal-critical mission=m-1\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "thermal-critical mission=m-1\n");

        // A later, legitimate re-stamp by a different mission overwrites
        // cleanly — this is the load-bearing case `create_new` alone would
        // wrongly refuse.
        write_file_refusing_symlinks_0600(&path, b"thermal-critical mission=m-2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "thermal-critical mission=m-2\n");
    }
}
