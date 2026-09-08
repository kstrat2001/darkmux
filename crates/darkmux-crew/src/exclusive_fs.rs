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
//! - **`write_file_refusing_symlinks_0600`, the PARENT (unix) —
//!   structurally closed as of #2478.** The prior `symlink_metadata`
//!   check-then-use gave the parent no real protection: `rename(2)`
//!   resolves the destination's *directory* components normally, and
//!   `create_dir_all` accepts a symlink-to-directory already sitting at
//!   the parent as "already exists" — verified, a rename into
//!   `<symlinked-parent>/STOP` follows the link, and the widest window
//!   was the ordinary first-write-to-a-fresh-workspace path (`Err(NotFound)
//!   => create_dir_all(parent)`). The fix pins the parent as a directory
//!   fd — `open(parent, O_DIRECTORY|O_NOFOLLOW)` — and every subsequent
//!   MUTATION (`openat` for the temp file, `renameat` onto the final
//!   name, `fchmod` for the `0700` lock) is relative to that fd, never to
//!   the path again. (The leaf LOUDNESS check is the one deliberate
//!   exception and is still an `lstat` on the path — it writes nothing,
//!   so a parent swapped under it can only cost a spurious refusal or a
//!   missed warning, never an unsafe write. See that check's own note.)
//!   `O_NOFOLLOW` fails with `ELOOP` on a symlink
//!   regardless of *when* the symlink was planted — before the call,
//!   between the call and `create_dir_all`, or between `create_dir_all`
//!   and the open — so there is no window left for an attacker to win.
//!   `libc` is already a `cfg(unix)` dependency of this crate (added by
//!   #2310 for `setpgid`), so this costs no new dependency surface. Kept
//!   as `std`-only (the narrowed, not closed, `symlink_metadata`
//!   check-then-use) on non-unix — see that function's own doc.
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

/// (#2478) Test-only synchronization seam for proving the parent-directory
/// TOCTOU race deterministically, rather than relying on real thread
/// scheduling to win a microsecond-scale window. A test registers a
/// closure in `PARENT_RACE_HOOK` (below) that plants a symlink exactly
/// where the documented race window opens; the production code calls this
/// function at that exact point, so the test exercises the SAME branch a
/// real attacker would, with no flakiness. Outside `cfg(test)` this
/// compiles to an empty `#[inline(always)]` function — it can never fire,
/// allocate, or touch a thread-local in a real binary.
///
/// BOTH arms are gated on `unix`, not just on `test`: the only call site
/// (`open_parent_dir_fd`) is itself `cfg(unix)`, so on a non-unix build
/// this function — and, under `cfg(test)`, its whole backing thread-local
/// — would be dead code, which is a `-D warnings` failure on a platform
/// this module still deliberately supports (see the `cfg(not(unix))`
/// arm of `write_file_refusing_symlinks_0600`).
#[cfg(all(test, unix))]
fn parent_race_test_hook() {
    tests::PARENT_RACE_HOOK.with(|cell| {
        if let Some(hook) = cell.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(all(unix, not(test)))]
#[inline(always)]
fn parent_race_test_hook() {}

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
/// planted at `path` — the LEAF — in the gap between step 2's check and
/// this write: `rename`/`renameat` replace WHATEVER is currently at the
/// destination without ever dereferencing it. On unix, step 1's PARENT is
/// now equally immune (#2478): the parent is pinned as a directory fd
/// (`open_parent_dir_fd`, below) via `open(parent, O_DIRECTORY|O_NOFOLLOW)`,
/// and every remaining operation that MUTATES anything — creating the temp
/// file, renaming it onto `path`, `fchmod`ing a freshly-created parent to
/// `0700` — goes through `openat`/`renameat`/`fchmod` relative to that fd,
/// never through the path again. (The leaf loudness `lstat` below is the
/// one remaining path-based call; it is read-only and cannot admit a
/// write, so it is not part of this guarantee.)
/// A symlink planted at ANY point — before the call, between the
/// call and `create_dir_all`, or between `create_dir_all` and the pinning
/// open — cannot retarget an fd that already points at the real directory,
/// and `O_NOFOLLOW` refuses to hand out an fd for a symlink in the first
/// place. See this module's own "how strong each guard actually is" note
/// for the executed verdict. Non-unix keeps the narrower `std`-only
/// check-then-use shape (see `open_parent_dir_fd`'s `cfg(not(unix))` twin).
///
/// A uniquely-named temp file is created in the SAME directory (the same
/// `O_CREAT|O_EXCL` pattern `dispatch_internal.rs`'s `remote_chat_attempt`
/// already uses for its own secret-bearing curl config — safe here for the
/// same reason: the name is unique per call, so nothing could already be
/// planted at it), then renamed onto `path`. This is the same
/// tmp-file-then-rename convention `thermal_governor.rs`'s own
/// `write_pace_file` already uses for its pace file, for the same
/// atomicity reason (a reader must never observe a half-written file).
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
    #[cfg(unix)]
    let file_name = path
        .file_name()
        .ok_or_else(|| format!("{} has no file name", path.display()))?;

    #[cfg(unix)]
    let parent_fd = open_parent_dir_fd(path, parent)?;

    #[cfg(not(unix))]
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

    // (#2478) Deliberately still PATH-based, not `fstatat` on `parent_fd`.
    // This is the LOUDNESS check (see the module doc): it writes nothing,
    // and the write below goes through the pinned fd regardless of what it
    // reports. A parent swapped between the pin above and this `lstat` can
    // therefore only cost a spurious refusal or a missed warning — never an
    // unsafe write — and an attacker able to do that could equally plant a
    // symlink at the real leaf and get the same refusal, so it hands them
    // no capability they did not already have. Left path-based rather than
    // spending a sixth `unsafe` wrapper on a check that cannot affect
    // safety either way.
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

    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::io::FromRawFd;

        let dir_fd = parent_fd.as_raw();
        let tmp_cname = CString::new(tmp_name.as_bytes())
            .map_err(|_| "temp file name contains a NUL byte".to_string())?;
        let file_cname = CString::new(file_name.as_bytes())
            .map_err(|_| format!("{} file name contains a NUL byte", path.display()))?;

        let raw_fd = openat_create_excl(dir_fd, &tmp_cname, 0o600).map_err(|e| {
            format!(
                "creating temp file {} in {}: {e}",
                tmp_name,
                parent.display()
            )
        })?;
        // `File::from_raw_fd` takes ownership: its `Drop` closes `raw_fd`
        // on every exit from this block, including the `write_all`
        // failure below — no manual close bookkeeping needed here.
        //
        // SAFETY: `raw_fd` was just returned by a successful `openat(2)`
        // in `openat_create_excl` and is not used, closed, or wrapped
        // anywhere else — this is the single point that takes ownership
        // of it.
        let mut f = unsafe { std::fs::File::from_raw_fd(raw_fd) };
        {
            use std::io::Write;
            if let Err(e) = f.write_all(contents) {
                drop(f);
                unlinkat_best_effort(dir_fd, &tmp_cname);
                return Err(format!(
                    "writing temp file {} in {}: {e}",
                    tmp_name,
                    parent.display()
                ));
            }
        }
        drop(f);

        if let Err(e) = renameat_same_dir(dir_fd, &tmp_cname, &file_cname) {
            unlinkat_best_effort(dir_fd, &tmp_cname);
            return Err(format!("renaming temp file onto {}: {e}", path.display()));
        }
        // `parent_fd` (an `OwnedDirFd`) is a local of the ENCLOSING
        // function, not of this block — it drops (closing the pinned
        // directory fd) when `write_file_refusing_symlinks_0600` returns,
        // on this success path exactly as on every early `?`/`return Err`
        // above.
        Ok(())
    }

    #[cfg(not(unix))]
    {
        let tmp_path = parent.join(tmp_name);
        std::fs::write(&tmp_path, contents)
            .map_err(|e| format!("writing temp file {}: {e}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp_path);
            format!("renaming temp file onto {}: {e}", path.display())
        })
    }
}

/// (#2478, unix only) Pin `parent` as a directory fd, refusing rather than
/// following if its final path component is a symlink or not a directory
/// at all — this is the function that actually closes the parent race
/// documented in the module doc, by never trusting a path-based check
/// again once it holds the fd. `path` is only used for the error text.
///
/// Shape: try to open `parent` directly first (the common re-stamp case:
/// it already exists as a real directory, and nothing here changes its
/// mode — this function only locks a directory IT creates to `0700`,
/// never one it merely reused). If that reports the parent absent, create
/// it (`create_dir_all`, same as before #2478 — nothing else in this
/// crate creates intermediate ancestors, so this branch carries no new
/// risk of its own) and open it again. (Strictly: the `0700` lock follows
/// the ABSENT verdict, not proof of authorship — if a third party creates
/// the parent between that verdict and the `create_dir_all`, the `Ok` is
/// "already exists" and this tightens a directory it did not create. That
/// is darkmux's own workspace path and tightening is the safe direction,
/// so it is named rather than guarded against.) Only the FINAL component
/// is locked; intermediate ancestors `create_dir_all` had to create keep
/// its default `0755`. The second open is NOT a formality:
/// `create_dir_all`'s `Ok` is never trusted as a safety signal on its own
/// — it happily accepts a symlink-to-directory as "already exists" (see
/// the module doc) — so this function re-opens with `O_NOFOLLOW`
/// regardless of what `create_dir_all` reported, and an attacker's symlink
/// planted at ANY point up to and including that second open still gets
/// refused there.
#[cfg(unix)]
fn open_parent_dir_fd(path: &Path, parent: &Path) -> Result<OwnedDirFd, String> {
    match open_dir_nofollow(parent) {
        Ok(fd) => return Ok(fd),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(describe_open_dir_refusal(path, parent, &e)),
    }

    // (#2478 RED-proof seam) the documented TOCTOU window: the parent has
    // just been confirmed absent, and an attacker who plants a symlink
    // here wins the race against `create_dir_all` below — narrowed to
    // nothing by the `open_dir_nofollow` retry that follows it. A no-op
    // outside `cfg(test)` — see `parent_race_test_hook`'s own doc.
    parent_race_test_hook();

    fs::create_dir_all(parent)
        .map_err(|e| format!("creating parent directory {}: {e}", parent.display()))?;

    let fd = match open_dir_nofollow(parent) {
        Ok(fd) => fd,
        Err(e) => return Err(describe_open_dir_refusal(path, parent, &e)),
    };

    // We just created this directory (or a symlink swap was already
    // refused above) — lock it to `0700` via the fd we hold, matching
    // `create_dir_exclusive_0700`'s convention for darkmux-owned
    // directories under `~/.darkmux/`. `fchmod` on the fd rather than
    // `set_permissions` on the path so this, too, cannot be raced: the fd
    // is already pinned to the right inode regardless of what the path
    // resolves to by the time this call happens.
    fchmod_fd(fd.as_raw(), 0o700)
        .map_err(|e| format!("setting permissions on {}: {e}", parent.display()))?;

    Ok(fd)
}

/// (#2478, unix only) RAII guard for a directory fd opened by
/// [`open_dir_nofollow`]. `Drop` closes it on every exit path out of
/// [`write_file_refusing_symlinks_0600`] — the early `?`/`return Err`s
/// above included — because `Drop` runs regardless of how the scope ends.
/// A leaked fd here is a slow resource leak in code that runs from the
/// thermal breaker, i.e. exactly when the machine is already in trouble.
#[cfg(unix)]
struct OwnedDirFd(std::os::unix::io::RawFd);

#[cfg(unix)]
impl OwnedDirFd {
    fn as_raw(&self) -> std::os::unix::io::RawFd {
        self.0
    }
}

#[cfg(unix)]
impl Drop for OwnedDirFd {
    fn drop(&mut self) {
        // SAFETY: `self.0` was returned by a successful `open(2)` in
        // `open_dir_nofollow`, and `OwnedDirFd` is its only owner (never
        // wrapped in a `File`, never cloned) — so this is the single
        // point that closes it, exactly once, on every exit path.
        unsafe {
            libc::close(self.0);
        }
    }
}

/// (#2478, unix only) Turn a refused `open_dir_nofollow(parent)` into a
/// human-readable reason. The SECURITY decision is already final by the
/// time this runs — the caller has already decided to refuse because the
/// open itself failed — so the one `symlink_metadata` call in here is for
/// MESSAGE ACCURACY only, exactly like the leaf's own loudness check
/// elsewhere in this file: reordering or removing it could never turn a
/// refusal into an acceptance.
///
/// Needed because `O_DIRECTORY|O_NOFOLLOW` does not report a stable,
/// platform-independent errno for "the final component is a symlink" —
/// probed on this platform (macOS): a symlink-to-directory AND a plain
/// regular file both surface as `ENOTDIR`, not `ELOOP` (`ELOOP` only
/// appears with `O_NOFOLLOW` alone, without `O_DIRECTORY`). Linux is
/// documented to report `ELOOP` for the symlink case, so both are checked.
#[cfg(unix)]
fn describe_open_dir_refusal(path: &Path, parent: &Path, e: &std::io::Error) -> String {
    let is_symlink = e.raw_os_error() == Some(libc::ELOOP)
        || (e.raw_os_error() == Some(libc::ENOTDIR)
            && fs::symlink_metadata(parent)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false));
    if is_symlink {
        format!(
            "refusing to write {} — its parent {} is a symlink, not a real directory",
            path.display(),
            parent.display()
        )
    } else if e.raw_os_error() == Some(libc::ENOTDIR) {
        format!(
            "refusing to write {} — its parent {} exists but is not a directory",
            path.display(),
            parent.display()
        )
    } else {
        format!("opening parent directory {}: {e}", parent.display())
    }
}

/// (#2478, unix only) Open `path` as a directory, refusing to follow it if
/// its FINAL component is a symlink (`O_NOFOLLOW`) or is not a directory
/// (`O_DIRECTORY`). This is the primitive `open_parent_dir_fd` builds the
/// actual guard from: unlike `symlink_metadata` (a check, divorced from
/// whatever uses the path next), the fd this returns is pinned to whatever
/// inode was open AT THIS INSTANT — nothing that happens to the path
/// afterward can retarget it, because every subsequent operation in
/// [`write_file_refusing_symlinks_0600`] uses the fd, never the path.
#[cfg(unix)]
fn open_dir_nofollow(path: &Path) -> std::io::Result<OwnedDirFd> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains a NUL byte")
    })?;
    // SAFETY: `c_path` is a valid NUL-terminated C string for the
    // lifetime of this call (it isn't dropped until after `open`
    // returns). The returned `c_int` is checked for `< 0` before being
    // trusted as a real fd, and ownership of a successful fd passes to
    // the returned `OwnedDirFd`, whose `Drop` is the only thing that ever
    // closes it.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(OwnedDirFd(fd))
}

/// (#2478, unix only) `fchmod(2)` on an already-open fd rather than
/// `set_permissions` on a path — the fd is already pinned to the right
/// inode, so there is no path component left for a symlink swap to
/// exploit here either.
///
/// SAFETY: `fd` must be a currently-open, valid file descriptor. Every
/// caller passes `OwnedDirFd::as_raw()`, so it is.
#[cfg(unix)]
fn fchmod_fd(fd: std::os::unix::io::RawFd, mode: u32) -> std::io::Result<()> {
    let ret = unsafe { libc::fchmod(fd, mode as libc::mode_t) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// (#2478, unix only) `openat(2)` with `O_CREAT|O_EXCL`, relative to
/// `dir_fd` — fails rather than following or truncating anything already
/// at `name` (including a symlink), the same "must not already exist"
/// property the leaf write already relied on via `create_new`, now scoped
/// to the pinned directory instead of a path.
///
/// SAFETY: `dir_fd` must be a valid, currently-open directory fd (callers
/// pass `OwnedDirFd::as_raw()`); `name` must be a bare filename with no
/// `/`, so resolution stays confined to that directory. The returned
/// `c_int` is checked for `< 0` before being trusted as a real fd.
#[cfg(unix)]
fn openat_create_excl(
    dir_fd: std::os::unix::io::RawFd,
    name: &std::ffi::CStr,
    mode: u32,
) -> std::io::Result<std::os::unix::io::RawFd> {
    let fd = unsafe {
        libc::openat(
            dir_fd,
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
            // `openat` is variadic when `O_CREAT` is set, and variadic
            // args promote to `c_uint` regardless of `mode_t`'s per-
            // platform width (`u16` on macOS/BSD, `u32` on Linux) — pass
            // `c_uint` explicitly rather than `mode_t` so this compiles
            // identically on both.
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(fd)
}

/// (#2478, unix only) `renameat(2)` with the SAME `dir_fd` for both the
/// old and new name — the temp file and the final destination live in the
/// same directory by construction. Like `rename(2)`, this replaces
/// whatever is currently at `new` as a directory entry without ever
/// dereferencing it, so the LEAF guarantee (a symlink planted at the
/// destination is replaced, never written through) is unchanged.
///
/// SAFETY: `dir_fd` must be a valid, currently-open directory fd; `old`
/// and `new` must be bare filenames resolved within it.
#[cfg(unix)]
fn renameat_same_dir(
    dir_fd: std::os::unix::io::RawFd,
    old: &std::ffi::CStr,
    new: &std::ffi::CStr,
) -> std::io::Result<()> {
    let ret = unsafe { libc::renameat(dir_fd, old.as_ptr(), dir_fd, new.as_ptr()) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// (#2478, unix only) Best-effort `unlinkat(2)` cleanup of a half-written
/// temp file after a failure — its result is deliberately discarded,
/// matching the existing non-unix path's `let _ = std::fs::remove_file(..)`
/// cleanup. A temp file this fails to remove is litter, not a security
/// issue: it was created with `O_EXCL` under a name unique to this call.
///
/// SAFETY: `dir_fd` must be a valid, currently-open directory fd; `name`
/// must be a bare filename resolved within it.
#[cfg(unix)]
fn unlinkat_best_effort(dir_fd: std::os::unix::io::RawFd, name: &std::ffi::CStr) {
    unsafe {
        libc::unlinkat(dir_fd, name.as_ptr(), 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // (#2478) Backing storage for `parent_race_test_hook` — `pub(super)`
    // because the hook function that reads it lives in the OUTER module
    // (it has to: it's called from production code), and a child module's
    // private items are not visible to its parent by default.
    #[cfg(unix)]
    thread_local! {
        pub(super) static PARENT_RACE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            std::cell::RefCell::new(None);
    }

    /// (#2478) Register a closure to run exactly where the parent-race
    /// hook fires in production code, then reset it after `body` runs so
    /// no residue leaks into an unrelated test on the same thread (tests
    /// share a thread pool; `#[test]` functions are not guaranteed a
    /// fresh thread each).
    #[cfg(unix)]
    fn with_parent_race_hook(hook: impl FnOnce() + 'static, body: impl FnOnce()) {
        // The clear runs from a `Drop` guard, not as a trailing statement:
        // `body` is expected to panic in exactly the case this test exists
        // to catch (the VULNERABLE assertion), and a hook that had not yet
        // fired would otherwise survive the unwind and fire inside an
        // UNRELATED later test on the same thread — planting a symlink at a
        // path whose tempdir is already gone, i.e. a panic raised from
        // inside production code, attributed to the wrong test.
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                PARENT_RACE_HOOK.with(|cell| *cell.borrow_mut() = None);
            }
        }
        let _clear = Clear;
        PARENT_RACE_HOOK.with(|cell| *cell.borrow_mut() = Some(Box::new(hook)));
        body();
    }

    /// (#2478) THE red-proof: plant a symlink at the PARENT in the exact
    /// window the module doc names — after `symlink_metadata` has already
    /// reported the parent absent, before this function acts on that
    /// verdict — and show the write lands in the attacker's directory.
    /// Before the fix: RED, because `create_dir_all` accepts the
    /// symlink-to-directory as "already exists" and the write proceeds
    /// inside it. After the fix: GREEN, because the `O_NOFOLLOW` open
    /// that replaces the trust-`create_dir_all`'s-`Ok` step refuses
    /// regardless of when the symlink appeared.
    #[test]
    #[cfg(unix)]
    fn write_file_refusing_symlinks_closes_the_parent_race_window() {
        let dir = tempfile::tempdir().unwrap();
        let attacker_dir = dir.path().join("attacker-owned");
        fs::create_dir_all(&attacker_dir).unwrap();
        // `parent` does not exist yet at the moment of the call — the
        // `Err(NotFound)` branch is the one the widest, most common
        // window opens in (first write to a fresh workspace).
        let parent = dir.path().join("crawl-workspace");
        let victim = parent.join("STOP");

        {
            with_parent_race_hook(
                {
                    let parent = parent.clone();
                    let attacker_dir = attacker_dir.clone();
                    move || {
                        std::os::unix::fs::symlink(&attacker_dir, &parent).unwrap();
                    }
                },
                || {
                    let result =
                        write_file_refusing_symlinks_0600(&victim, b"thermal-critical\n");
                    // Pre-fix: `Ok(())`, and the bytes are sitting in the
                    // attacker's directory, not under `crawl-workspace`.
                    // Post-fix: `Err`, and nothing was written anywhere.
                    if result.is_ok() {
                        panic!(
                            "VULNERABLE: write_file_refusing_symlinks_0600 succeeded through a \
                             parent-directory symlink planted in the documented race window; \
                             landed in attacker dir: {}",
                            attacker_dir.join("STOP").exists()
                        );
                    }
                },
            );

            assert!(
                !attacker_dir.join("STOP").exists(),
                "the write must not land in the attacker's directory"
            );
            assert!(
                !parent.join("STOP").exists() || fs::symlink_metadata(&parent)
                    .map(|m| !m.file_type().is_symlink())
                    .unwrap_or(true),
                "the parent must not have been left as (or resolved through) the planted symlink"
            );
        }
    }

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

    /// (#2478) A parent sitting there as a plain REGULAR FILE is refused
    /// with the "not a directory" wording, not the symlink wording. This
    /// pins the non-symlink half of `describe_open_dir_refusal`'s
    /// `ENOTDIR` split — which matters because on macOS `ENOTDIR` is the
    /// errno for BOTH a symlink-to-directory and a plain file (probed;
    /// `ELOOP` only appears without `O_DIRECTORY`), so the two are told
    /// apart by a follow-up `lstat` rather than by the errno. With that
    /// split unpinned, collapsing it would silently start telling
    /// operators to go looking for a symlink that is not there.
    #[test]
    #[cfg(unix)]
    fn write_file_refusing_symlinks_refuses_a_regular_file_parent() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("crawl-root");
        fs::write(&parent, b"i am a file, not a directory\n").unwrap();

        let err = write_file_refusing_symlinks_0600(&parent.join("STOP"), b"thermal-critical\n")
            .unwrap_err();

        assert!(
            err.contains("exists but is not a directory"),
            "a regular-file parent must be reported as such: {err}"
        );
        assert!(
            !err.contains("is a symlink"),
            "a regular file must NOT be reported as a symlink: {err}"
        );
        assert_eq!(
            fs::read_to_string(&parent).unwrap(),
            "i am a file, not a directory\n",
            "the refusal must not have written through or truncated the file"
        );
    }

    /// (#2478) A DANGLING symlink at the parent — one pointing at a target
    /// that does not exist — is refused, and is NOT classified as
    /// `NotFound`.
    ///
    /// The classification is what this pins, not the wording for its own
    /// sake. `open_parent_dir_fd` treats `NotFound` as "absent, go ahead
    /// and `create_dir_all`", and a dangling symlink is the one shape that
    /// looks absent to a careless check while being an existing directory
    /// ENTRY. If `O_DIRECTORY|O_NOFOLLOW` on one ever reported `ENOENT`
    /// instead of `ENOTDIR`/`ELOOP`, this call would fall into that branch.
    ///
    /// Stated precisely, because the tempting version of this claim is
    /// wrong: darkmux would NOT then be made to create the attacker's
    /// target. Probed — `create_dir_all` on a dangling symlink returns
    /// `EEXIST` and creates nothing, because the final `mkdir` sees the
    /// symlink's own directory entry. (Its `Ok` on a symlink-to-EXISTING-
    /// directory, which is the premise this whole fix rests on, was probed
    /// separately and does hold.) What misclassification actually costs is
    /// the REFUSAL REASON: the operator-facing message degrades from
    /// "its parent ... is a symlink, not a real directory" — which tells
    /// them to go look at a planted link — to "creating parent directory
    /// ...: File exists", which tells them nothing. #2456 made this
    /// module's refusals loud on purpose; a refusal that no longer names
    /// the symlink is a regression in the thing that was fixed.
    ///
    /// Probed on macOS: `ENOTDIR`. Documented on Linux: `ELOOP`. Nothing
    /// else in this suite would notice if that changed.
    #[test]
    #[cfg(unix)]
    fn write_file_refusing_symlinks_refuses_a_dangling_symlinked_parent() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("crawl-root");
        let attacker_target = dir.path().join("attacker-named-target");
        std::os::unix::fs::symlink(&attacker_target, &parent).unwrap();

        let err = write_file_refusing_symlinks_0600(&parent.join("STOP"), b"thermal-critical\n")
            .unwrap_err();

        assert!(
            err.contains("is a symlink"),
            "a dangling symlinked parent must still be REFUSED AS A SYMLINK — a generic \
             \"File exists\" here means the NotFound classification has widened and the \
             refusal has stopped naming the planted link: {err}"
        );
        assert!(
            !attacker_target.exists(),
            "nothing may be created at the symlink's target"
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

    /// (#2478) The parent this function creates is locked to `0700`,
    /// matching `create_dir_exclusive_0600`'s convention — not left at
    /// `create_dir_all`'s default `0755`.
    #[test]
    #[cfg(unix)]
    fn write_file_refusing_symlinks_locks_a_freshly_created_parent_to_0700() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("crawl-root");
        let path = parent.join("STOP");

        write_file_refusing_symlinks_0600(&path, b"thermal-critical\n").unwrap();

        let mode = fs::metadata(&parent).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "freshly-created parent must be locked to 0700, not create_dir_all's default 0755"
        );
    }

    /// (#2478) The `0700` lock only applies to a parent this function
    /// itself creates. A pre-existing parent (the legitimate re-stamp
    /// case, or simply an operator-managed directory) is reused as-is —
    /// this function never silently tightens permissions on a directory
    /// it didn't create.
    #[test]
    #[cfg(unix)]
    fn write_file_refusing_symlinks_does_not_rechmod_an_existing_parent() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("crawl-root");
        fs::create_dir_all(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o750)).unwrap();
        let path = parent.join("STOP");

        write_file_refusing_symlinks_0600(&path, b"thermal-critical\n").unwrap();

        let mode = fs::metadata(&parent).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o750,
            "an already-existing parent's mode must be left exactly as it was"
        );
    }
}
