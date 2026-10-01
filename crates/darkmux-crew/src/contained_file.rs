//! (#2869) Reading files out of a directory a container can write.
//!
//! A dispatch's out-dir (mounted at `/darkmux-out`) and its workspace
//! (mounted at `/workspace`, read-write unless the dispatch asks for `:ro`)
//! are writable by the model's own tools. Whatever the host later reads
//! from them was placed there by the model, including symlinks. A plain
//! `fs::copy` / `fs::read_to_string` follows a symlink ON THE HOST, so a
//! model that plants `metrics.json -> ~/.ssh/id_ed25519` gets the host to
//! read that file into the run directory. A FIFO or a device node in the
//! same place hangs the read instead.
//!
//! Every host-side read of such a directory goes through [`open_contained`]
//! instead. It walks the relative path one component at a time from a
//! directory handle on the root, opening each component with `O_NOFOLLOW`
//! (`openat`), so:
//!
//! - a symlink at ANY component (the file itself, or a directory above it
//!   swapped for a link) is refused, not followed;
//! - a relative path with `..`, a root, or a prefix is refused before any
//!   open, so the path cannot name anything outside the root;
//! - the file is opened `O_NONBLOCK`, so a FIFO cannot hang the open, and
//!   then checked with `fstat` on the OPEN descriptor, so a FIFO, directory,
//!   socket or device is refused without a check-then-open race;
//! - reads are bounded by a caller-supplied byte cap.
//!
//! A hard link cannot reach a host file from inside the container: a link
//! needs its target visible on the same filesystem, and the container sees
//! only the two mounts. The root fix for the whole class is running the
//! container as a different uid than the host reader (named at
//! `dispatch_internal`'s mount code); this module is the host-side guard
//! until then, and stays useful after it.
//!
//! Absence is not a refusal: [`ContainedFileError::NotFound`] is what a
//! missing file returns, and callers treat it as they treated a missing
//! file before. A [`ContainedFileError::Refused`] is always something the
//! caller must report (warn, and record where the run has a record).
//!
//! The write-side sibling is `exclusive_fs` (creating darkmux-owned state
//! under a directory someone else could have planted a link in); both pin
//! a directory fd and `openat` relative to it with `O_NOFOLLOW`.

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Component, Path};

/// Default read cap for runtime bookkeeping files (trajectory, metrics,
/// findings). Generous: a long agentic run's trajectory is tens of MB. The
/// cap exists so a model cannot make the host buffer an unbounded file,
/// not to police ordinary sizes.
pub const DEFAULT_MAX_BYTES: u64 = 1024 * 1024 * 1024;

/// (#2869) Read cap for small, fixed-shape bookkeeping files the host
/// parses whole (`metrics.json`, the resume origin file). A real one is a
/// few hundred bytes.
pub const SMALL_FILE_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Why a contained read did not produce a file.
#[derive(Debug)]
pub enum ContainedFileError {
    /// The file (or a directory above it) does not exist. Not a refusal.
    NotFound,
    /// The path was refused on safety grounds. The string names why, and
    /// always contains one of: `symlink`, `not a regular file`,
    /// `escapes the directory`, `not a directory`, `exceeds`.
    Refused(String),
    /// Any other I/O failure.
    Io(io::Error),
}

impl ContainedFileError {
    /// `true` for [`ContainedFileError::Refused`].
    pub fn is_refused(&self) -> bool {
        matches!(self, ContainedFileError::Refused(_))
    }
}

impl fmt::Display for ContainedFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContainedFileError::NotFound => write!(f, "not found"),
            ContainedFileError::Refused(why) => write!(f, "refused: {why}"),
            ContainedFileError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ContainedFileError {}

impl From<io::Error> for ContainedFileError {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::NotFound {
            ContainedFileError::NotFound
        } else {
            ContainedFileError::Io(e)
        }
    }
}

/// Split `rel` into its components, refusing anything that is not a plain
/// name (`..`, `.`, a root, a Windows prefix) and an empty path.
fn plain_components(rel: &Path) -> Result<Vec<&std::ffi::OsStr>, ContainedFileError> {
    let mut out = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(n) => out.push(n),
            _ => {
                return Err(ContainedFileError::Refused(format!(
                    "`{}` escapes the directory (only plain relative names are read)",
                    rel.display()
                )))
            }
        }
    }
    if out.is_empty() {
        return Err(ContainedFileError::Refused(
            "empty path escapes the directory".to_string(),
        ));
    }
    Ok(out)
}

#[cfg(unix)]
mod imp {
    use super::ContainedFileError;
    use std::ffi::{CString, OsStr};
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    fn cstr(name: &OsStr) -> Result<CString, ContainedFileError> {
        CString::new(name.as_bytes()).map_err(|_| {
            ContainedFileError::Refused("a path component contains a NUL byte and escapes the directory".into())
        })
    }

    fn openat(dir: &OwnedFd, name: &OsStr, flags: libc::c_int) -> io::Result<OwnedFd> {
        let c = cstr(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        // SAFETY: `dir` is a live descriptor we own, `c` is a valid
        // NUL-terminated string that outlives the call.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just returned by a successful `openat`, and
        // nothing else owns it.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Map an `openat` failure on component `name` to the module's error.
    /// `ELOOP` is what `O_NOFOLLOW` returns for a symlink on both Linux and
    /// macOS (and `EMLINK` on some BSDs).
    /// macOS reports `ENOTDIR` rather than `ELOOP` for a symlink opened with
    /// `O_DIRECTORY | O_NOFOLLOW`, so a directory-step `ENOTDIR` is looked at
    /// with `fstatat(AT_SYMLINK_NOFOLLOW)` to name the real reason.
    fn is_symlink_at(dir: &OwnedFd, name: &OsStr) -> bool {
        let Ok(c) = cstr(name) else { return false };
        // SAFETY: zeroed `stat` is a valid out-parameter; `dir` and `c` are
        // live for the call.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::fstatat(dir.as_raw_fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW)
        };
        rc == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFLNK
    }

    fn classify(e: io::Error, dir: &OwnedFd, name: &OsStr, want_dir: bool) -> ContainedFileError {
        let symlink = || {
            ContainedFileError::Refused(format!(
                "`{}` is a symlink (not followed)",
                Path::new(name).display()
            ))
        };
        match e.raw_os_error() {
            Some(libc::ELOOP) | Some(libc::EMLINK) => symlink(),
            Some(libc::ENOTDIR) if want_dir && is_symlink_at(dir, name) => symlink(),
            Some(libc::ENOTDIR) if want_dir => ContainedFileError::Refused(format!(
                "`{}` is not a directory",
                Path::new(name).display()
            )),
            Some(libc::ENOENT) => ContainedFileError::NotFound,
            _ => ContainedFileError::Io(e),
        }
    }

    pub(super) fn open(root: &Path, parts: &[&OsStr]) -> Result<File, ContainedFileError> {
        // The root itself is host-chosen (a temp dir darkmux created, or a
        // run's sandbox path), so it is opened normally.
        let root_file = File::open(root)?;
        let mut dir: OwnedFd = root_file.into();
        let (last, dirs) = parts.split_last().expect("plain_components refuses empty paths");
        for name in dirs {
            let next = openat(&dir, name, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .map_err(|e| classify(e, &dir, name, true))?;
            dir = next;
        }
        // O_NONBLOCK: opening a FIFO for reading otherwise blocks until a
        // writer appears. Regular files ignore the flag.
        let fd = openat(&dir, last, libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .map_err(|e| classify(e, &dir, last, false))?;
        let file = File::from(fd);
        // fstat on the OPEN descriptor: what we check is what we read.
        let meta = file.metadata()?;
        if !meta.file_type().is_file() {
            return Err(ContainedFileError::Refused(format!(
                "`{}` is not a regular file",
                Path::new(last).display()
            )));
        }
        Ok(file)
    }
}

#[cfg(not(unix))]
mod imp {
    use super::ContainedFileError;
    use std::ffi::OsStr;
    use std::fs::File;
    use std::path::Path;

    // darkmux does not support non-Unix hosts (see this crate's Cargo.toml);
    // this is a best-effort equivalent so the crate still type-checks. It
    // has the check-then-open race the Unix path avoids.
    pub(super) fn open(root: &Path, parts: &[&OsStr]) -> Result<File, ContainedFileError> {
        let mut p = root.to_path_buf();
        for name in parts {
            p.push(name);
            let m = std::fs::symlink_metadata(&p)?;
            if m.file_type().is_symlink() {
                return Err(ContainedFileError::Refused(format!(
                    "`{}` is a symlink (not followed)",
                    Path::new(name).display()
                )));
            }
        }
        let file = File::open(&p)?;
        if !file.metadata()?.is_file() {
            return Err(ContainedFileError::Refused("not a regular file".into()));
        }
        Ok(file)
    }
}

/// Open `root/rel` for reading, refusing symlinks at every component,
/// anything that is not a regular file, and any `rel` that is not a plain
/// relative path. See the module doc.
pub fn open_contained(root: &Path, rel: &Path) -> Result<File, ContainedFileError> {
    let parts = plain_components(rel)?;
    imp::open(root, &parts)
}

/// Open `path`, applying the no-follow walk of [`open_contained`] to its
/// last `n` components; everything above them is the root the caller
/// trusts and is opened normally, BY PATH. So the prefix above the last
/// `n` components must be host-owned: with `n = 1` a parent the container
/// can write would be followed. For a caller that holds a full path rather
/// than a (root, relative) pair, e.g. the trajectory tailer's
/// `<out_dir>/.darkmux-runtime/trajectory.jsonl` with `n = 2`, where
/// `<out_dir>` is the host-created temp dir.
pub fn open_path_tail(path: &Path, n: usize) -> Result<File, ContainedFileError> {
    let comps: Vec<Component<'_>> = path.components().collect();
    let n = n.clamp(1, comps.len().max(1));
    let split = comps.len().saturating_sub(n);
    let root: std::path::PathBuf = comps[..split].iter().collect();
    let root = if root.as_os_str().is_empty() { std::path::PathBuf::from(".") } else { root };
    let rel: std::path::PathBuf = comps[split..].iter().collect();
    open_contained(&root, &rel)
}

/// Read all of `file`, refusing if it is larger than `max_bytes` (checked
/// against both the size at open and the bytes actually read, since a
/// running container can still be appending).
fn read_bounded(mut file: File, max_bytes: u64, label: &Path) -> Result<Vec<u8>, ContainedFileError> {
    let too_big = || {
        ContainedFileError::Refused(format!(
            "`{}` exceeds the {max_bytes}-byte read cap",
            label.display()
        ))
    };
    if file.metadata()?.len() > max_bytes {
        return Err(too_big());
    }
    let mut buf = Vec::new();
    (&mut file).take(max_bytes + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > max_bytes {
        return Err(too_big());
    }
    Ok(buf)
}

/// [`open_contained`], then read the whole file (bounded by `max_bytes`).
pub fn read_contained(root: &Path, rel: &Path, max_bytes: u64) -> Result<Vec<u8>, ContainedFileError> {
    read_bounded(open_contained(root, rel)?, max_bytes, rel)
}

/// [`read_contained`] as UTF-8 text. Invalid UTF-8 is an `Io` error
/// (`InvalidData`), matching `fs::read_to_string`.
pub fn read_contained_to_string(
    root: &Path,
    rel: &Path,
    max_bytes: u64,
) -> Result<String, ContainedFileError> {
    let bytes = read_contained(root, rel, max_bytes)?;
    String::from_utf8(bytes)
        .map_err(|e| ContainedFileError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))
}

/// Copy `root/rel` to a NEW file `dst`, bounded by `max_bytes`. `dst` is
/// created exclusively (`O_CREAT|O_EXCL`): an existing file there is
/// refused and left untouched, never truncated. Streams rather than
/// buffering the whole file. When the source is refused, missing, or turns
/// out larger than the cap mid-copy, nothing this call created is left at
/// `dst`. Returns the number of bytes copied.
pub fn copy_contained(
    root: &Path,
    rel: &Path,
    dst: &Path,
    max_bytes: u64,
) -> Result<u64, ContainedFileError> {
    let mut file = open_contained(root, rel)?;
    let too_big = || {
        ContainedFileError::Refused(format!("`{}` exceeds the {max_bytes}-byte read cap", rel.display()))
    };
    if file.metadata()?.len() > max_bytes {
        return Err(too_big());
    }
    let mut out = File::options()
        .write(true)
        .create_new(true)
        .open(dst)
        .map_err(ContainedFileError::Io)?;
    let copied = io::copy(&mut (&mut file).take(max_bytes + 1), &mut out);
    let fail = |e: ContainedFileError| {
        drop(std::fs::remove_file(dst));
        Err(e)
    };
    match copied {
        Ok(n) if n > max_bytes => fail(too_big()),
        Ok(n) => Ok(n),
        Err(e) => fail(ContainedFileError::Io(e)),
    }
}

/// (#2869) Deepest directory nesting [`copy_tree_nofollow`] walks. The
/// walk holds one fd per level; this keeps it well under a 256-fd process
/// limit (the CLI does not raise its limit the way `darkmux serve` does).
pub const MAX_TREE_DEPTH: usize = 128;

/// What [`copy_tree_nofollow`] did beyond copying regular files.
#[derive(Debug, Default)]
pub struct TreeCopyReport {
    /// Regular files copied.
    pub files: usize,
    /// Relative, in-tree symlinks recreated (paths relative to the source
    /// root).
    pub links_recreated: Vec<std::path::PathBuf>,
    /// Entries not copied, with why: absolute or escaping symlinks, FIFOs,
    /// sockets, devices, and entries that changed type between listing and
    /// open.
    pub skipped: Vec<(std::path::PathBuf, String)>,
}

/// Whether a symlink at `link_dir/<name>` (with `link_dir` relative to the
/// copy root) whose target is `target` is safe to recreate: it resolves
/// inside the root no matter what the other recreated links are.
///
/// (#2869 F1) The target must be a LEADING run of `..` (and `.`), then
/// plain names only. A `..` after a name is refused, because the kernel
/// resolves that name first and it may itself be a recreated link:
/// `sub/up -> ..` is fine alone, but `sub/up/sub/up/../x` climbs one level
/// per `up/..` pair while cancelling lexically. With `..` only leading, the
/// climb is taken from the link's own (real, never-symlinked) directory
/// and bounded by its depth, and every later name descends; by induction
/// over the other recreated links, which obey the same rule, the result
/// stays inside the root. Absolute targets are refused.
fn relative_link_stays_inside(link_dir: &Path, target: &Path) -> bool {
    let depth: usize = link_dir
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count();
    let mut ups = 0usize;
    let mut seen_name = false;
    for c in target.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if seen_name {
                    return false;
                }
                ups += 1;
                if ups > depth {
                    return false;
                }
            }
            Component::Normal(_) => seen_name = true,
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// (#2869) Copy the tree at `src` into `dst` without ever following a
/// symlink and without ever re-opening a directory by path.
///
/// `src` itself is opened `O_DIRECTORY|O_NOFOLLOW` (its parents are the
/// caller's, trusted, and opened by path). From there the walk carries a
/// directory fd down the recursion: each directory is listed through a
/// close-on-exec dup of its own fd (`fdopendir`), each entry's type comes
/// from `fstatat(..., AT_SYMLINK_NOFOLLOW)` relative to that fd, a
/// subdirectory is opened with `openat(O_DIRECTORY|O_NOFOLLOW)` and a file
/// with `openat(O_NOFOLLOW|O_NONBLOCK)` then checked `S_ISREG` on the open
/// fd. So a directory renamed away and replaced with a link after it was
/// opened is still read through the fd we hold (its original contents),
/// and one replaced before it was opened fails the no-follow open and is
/// skipped, reported as whatever it became.
///
/// The destination side is written the same way: through a directory fd
/// per level (`mkdirat`, `openat(O_CREAT|O_EXCL|O_NOFOLLOW)`, `symlinkat`,
/// `fclonefileat`), so nothing already sitting in `dst` (a planted link, or
/// a case-folded name collision on a case-insensitive volume) is ever
/// followed or overwritten; such a collision fails the copy.
///
/// A symlink is recreated verbatim only under
/// [`relative_link_stays_inside`]'s rule (real checkouts commit these:
/// `node_modules/.bin/*`, `lib-alias -> lib`); any other link is skipped
/// and reported.
///
/// The walk holds one fd per level and refuses a tree deeper than
/// [`MAX_TREE_DEPTH`]. Regular files keep their permission bits. On macOS
/// the bytes are cloned from the verified fd (`fclonefileat`, copy-on-write
/// on APFS), falling back to a plain copy across volumes; elsewhere
/// `std::io::copy`, which uses `copy_file_range` on Linux.
#[cfg(unix)]
pub fn copy_tree_nofollow(src: &Path, dst: &Path) -> io::Result<TreeCopyReport> {
    let mut report = TreeCopyReport::default();
    let parent = src.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = src
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source has no file name"))?;
    let parent_fd: std::os::fd::OwnedFd = File::open(parent)?.into();
    let root_fd = tree::openat(&parent_fd, name, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .map_err(|e| match e.raw_os_error() {
            Some(libc::ELOOP) | Some(libc::ENOTDIR) => io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is a symlink or not a directory; refusing to copy through it", src.display()),
            ),
            _ => e,
        })?;
    // `dst` is the caller's fresh, host-owned scratch directory; it is
    // created by path, then held as an fd like the source.
    std::fs::create_dir_all(dst)?;
    let dst_fd: std::os::fd::OwnedFd = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dst)?
        .into();
    tree::walk(&root_fd, src, Path::new(""), &dst_fd, 0, &mut report)?;
    Ok(report)
}

/// Non-Unix hosts are unsupported (see this crate's Cargo.toml); refuse
/// rather than fall back to a following copy.
#[cfg(not(unix))]
pub fn copy_tree_nofollow(_src: &Path, _dst: &Path) -> io::Result<TreeCopyReport> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "copy_tree_nofollow needs a Unix host"))
}

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

#[cfg(unix)]
mod tree {
    use super::{relative_link_stays_inside, TreeCopyReport, MAX_TREE_DEPTH};
    use std::ffi::{CStr, CString, OsStr, OsString};
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    fn cstr(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
    }

    fn check(rc: libc::c_int) -> io::Result<()> {
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn openat(dir: &OwnedFd, name: &OsStr, flags: libc::c_int) -> io::Result<OwnedFd> {
        openat_mode(dir, name, flags, 0)
    }

    fn openat_mode(dir: &OwnedFd, name: &OsStr, flags: libc::c_int, mode: libc::c_uint) -> io::Result<OwnedFd> {
        let c = cstr(name)?;
        // SAFETY: live fd, valid C string.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags | libc::O_CLOEXEC, mode) };
        check(fd)?;
        // SAFETY: freshly returned, unowned fd.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// (#2869 C2) A close-on-exec duplicate, so a concurrent fork+exec
    /// (another step's shell command) never inherits it.
    pub(super) fn dup_cloexec(fd: &OwnedFd) -> io::Result<OwnedFd> {
        // SAFETY: duplicating a live fd.
        let d = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        check(d)?;
        // SAFETY: freshly returned, unowned fd.
        Ok(unsafe { OwnedFd::from_raw_fd(d) })
    }

    #[cfg(target_os = "macos")]
    fn errno_ptr() -> *mut libc::c_int {
        // SAFETY: always valid for the calling thread.
        unsafe { libc::__error() }
    }

    #[cfg(not(target_os = "macos"))]
    fn errno_ptr() -> *mut libc::c_int {
        // SAFETY: always valid for the calling thread.
        unsafe { libc::__errno_location() }
    }

    /// Entry names of the directory `dir` refers to, read through a dup of
    /// that fd (never by path). `.` and `..` excluded. A `readdir` error
    /// (errno set on a NULL return) is an error, not a short listing.
    fn list(dir: &OwnedFd) -> io::Result<Vec<OsString>> {
        let dupfd = dup_cloexec(dir)?.into_raw_fd();
        // SAFETY: fdopendir takes ownership of the dup on success.
        let dp = unsafe { libc::fdopendir(dupfd) };
        if dp.is_null() {
            let e = io::Error::last_os_error();
            unsafe { libc::close(dupfd) };
            return Err(e);
        }
        // The dup shares the file offset with `dir`; start from the top.
        unsafe { libc::rewinddir(dp) };
        let mut out = Vec::new();
        let result = loop {
            // SAFETY: resetting this thread's errno so a NULL from readdir
            // can be told apart as end-of-directory vs error.
            unsafe { *errno_ptr() = 0 };
            // SAFETY: `dp` is a valid DIR*; the entry is valid until the
            // next readdir on it, and the name is copied out immediately.
            let ent = unsafe { libc::readdir(dp) };
            if ent.is_null() {
                let errno = unsafe { *errno_ptr() };
                break if errno == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(errno)) };
            }
            let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." {
                out.push(OsString::from_vec(name.to_vec()));
            }
        };
        unsafe { libc::closedir(dp) };
        result.map(|()| out)
    }

    fn lstat_at(dir: &OwnedFd, name: &OsStr) -> io::Result<libc::stat> {
        let c = cstr(name)?;
        // SAFETY: zeroed stat is a valid out-param; fd and string are live.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        check(unsafe { libc::fstatat(dir.as_raw_fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) })?;
        Ok(st)
    }

    fn type_name(mode: libc::mode_t) -> &'static str {
        match mode & libc::S_IFMT {
            libc::S_IFDIR => "directory",
            libc::S_IFREG => "regular file",
            libc::S_IFLNK => "symlink",
            libc::S_IFIFO => "fifo",
            libc::S_IFSOCK => "socket",
            libc::S_IFCHR => "character device",
            libc::S_IFBLK => "block device",
            _ => "unknown file type",
        }
    }

    /// What `name` is NOW (for the message when it changed under us).
    fn now_is(dir: &OwnedFd, name: &OsStr) -> &'static str {
        lstat_at(dir, name).map(|st| type_name(st.st_mode)).unwrap_or("gone")
    }

    fn readlink_at(dir: &OwnedFd, name: &OsStr) -> io::Result<PathBuf> {
        let c = cstr(name)?;
        let mut buf = vec![0u8; libc::PATH_MAX as usize + 1];
        // SAFETY: buffer is writable for its full length.
        let n = unsafe { libc::readlinkat(dir.as_raw_fd(), c.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        buf.truncate(n as usize);
        Ok(PathBuf::from(OsString::from_vec(buf)))
    }

    /// Copy the verified regular file `from` to `dst_dir/name`, created
    /// exclusively relative to the destination directory's fd.
    fn copy_file(from: File, dst_dir: &OwnedFd, name: &OsStr, mode: u32) -> io::Result<()> {
        let mode = mode & 0o7777;
        #[cfg(all(test, target_os = "macos"))]
        let force_byte_copy = super::FORCE_BYTE_COPY.with(|f| f.get());
        #[cfg(all(not(test), target_os = "macos"))]
        let force_byte_copy = false;
        #[cfg(target_os = "macos")]
        if !force_byte_copy {
            let c = cstr(name)?;
            // `CLONE_NOFOLLOW` from <sys/clonefile.h>; the libc crate does
            // not export it. fclonefileat never overwrites (EEXIST).
            const CLONE_NOFOLLOW: u32 = 0x0001;
            // SAFETY: both fds live, valid C string.
            let rc = unsafe { libc::fclonefileat(from.as_raw_fd(), dst_dir.as_raw_fd(), c.as_ptr(), CLONE_NOFOLLOW) };
            if rc == 0 {
                #[cfg(test)]
                super::CLONED_FILES.with(|n| n.set(n.get() + 1));
                // SAFETY: live fd, valid C string; does not follow a link.
                check(unsafe {
                    libc::fchmodat(dst_dir.as_raw_fd(), c.as_ptr(), mode as libc::mode_t, libc::AT_SYMLINK_NOFOLLOW)
                })?;
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EEXIST) {
                return Err(e);
            }
            // Not clonable (another volume, not APFS): plain copy below.
        }
        let to = openat_mode(
            dst_dir,
            name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            0o600,
        )?;
        let mut to = File::from(to);
        let mut from = from;
        io::copy(&mut from, &mut to)?;
        to.set_permissions(std::fs::Permissions::from_mode(mode))?;
        Ok(())
    }

    fn mkdir_at(dst_dir: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
        let c = cstr(name)?;
        // SAFETY: live fd, valid C string. EEXIST (anything already there,
        // including a link) is an error: nothing pre-existing is reused.
        check(unsafe { libc::mkdirat(dst_dir.as_raw_fd(), c.as_ptr(), 0o777) })?;
        openat(dst_dir, name, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW)
    }

    fn symlink_at(target: &Path, dst_dir: &OwnedFd, name: &OsStr) -> io::Result<()> {
        let t = cstr(target.as_os_str())?;
        let c = cstr(name)?;
        // SAFETY: live fd, valid C strings.
        check(unsafe { libc::symlinkat(t.as_ptr(), dst_dir.as_raw_fd(), c.as_ptr()) })
    }

    pub(super) fn walk(
        dir: &OwnedFd,
        src_dir: &Path,
        rel: &Path,
        dst_dir: &OwnedFd,
        depth: usize,
        report: &mut TreeCopyReport,
    ) -> io::Result<()> {
        if depth > MAX_TREE_DEPTH {
            return Err(io::Error::other(format!(
                "{} is nested deeper than {MAX_TREE_DEPTH} directories; refusing to copy it",
                src_dir.display()
            )));
        }
        let names = list(dir)?;
        #[cfg(test)]
        super::run_after_list_hook(src_dir);
        for name in names {
            let rel_entry = rel.join(&name);
            let st = match lstat_at(dir, &name) {
                Ok(st) => st,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            #[cfg(test)]
            super::run_before_open_hook(&src_dir.join(&name));
            let changed = |report: &mut TreeCopyReport, was: &str| {
                let why = format!("changed from a {was} to a {} during the copy", now_is(dir, &name));
                report.skipped.push((rel_entry.clone(), why));
            };
            match st.st_mode & libc::S_IFMT {
                libc::S_IFDIR => {
                    let sub = match openat(dir, &name, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW) {
                        Ok(fd) => fd,
                        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) => {
                            changed(report, "directory");
                            continue;
                        }
                        Err(e) => return Err(e),
                    };
                    let dst_sub = mkdir_at(dst_dir, &name)?;
                    walk(&sub, &src_dir.join(&name), &rel_entry, &dst_sub, depth + 1, report)?;
                }
                libc::S_IFREG => {
                    let fd = match openat(dir, &name, libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK) {
                        Ok(fd) => fd,
                        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::EMLINK)) => {
                            changed(report, "regular file");
                            continue;
                        }
                        Err(e) => return Err(e),
                    };
                    let file = File::from(fd);
                    let meta = file.metadata()?;
                    if !meta.file_type().is_file() {
                        changed(report, "regular file");
                        continue;
                    }
                    copy_file(file, dst_dir, &name, meta.permissions().mode())?;
                    report.files += 1;
                }
                libc::S_IFLNK => {
                    let target = readlink_at(dir, &name)?;
                    if target.is_relative() && relative_link_stays_inside(rel, &target) {
                        symlink_at(&target, dst_dir, &name)?;
                        report.links_recreated.push(rel_entry);
                    } else {
                        report.skipped.push((
                            rel_entry,
                            format!(
                                "symlink to `{}` could resolve outside the tree (not recreated)",
                                target.display()
                            ),
                        ));
                    }
                }
                other => report.skipped.push((rel_entry, format!("{} (not copied)", type_name(other)))),
            }
        }
        Ok(())
    }
}

// (#2869) Test-only seam: called by a tree walk right after it has listed
// a directory and before it acts on any entry, with that directory's
// source path. Lets a test swap an entry (or the directory itself) for a
// symlink between the listing and the open, which is the race a walk that
// re-opens parents by path loses.
#[cfg(test)]
pub(crate) type AfterListHook = Box<dyn FnMut(&Path)>;

#[cfg(test)]
thread_local! {
    static AFTER_LIST_HOOK: std::cell::RefCell<Option<AfterListHook>> =
        const { std::cell::RefCell::new(None) };
}

// Test-only: skip the macOS clone so the byte-copy path is exercised.
#[cfg(test)]
thread_local! {
    pub(crate) static FORCE_BYTE_COPY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

// Test-only: files `copy_tree_nofollow` cloned (macOS `fclonefileat`)
// rather than byte-copied, on this thread.
#[cfg(test)]
thread_local! {
    pub(crate) static CLONED_FILES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Install the test-only after-listing hook for this thread.
#[cfg(test)]
pub(crate) fn set_after_list_hook(f: Option<AfterListHook>) {
    AFTER_LIST_HOOK.with(|h| *h.borrow_mut() = f);
}

// Test-only seam: called by the tree walk after it has read an entry's
// type (`fstatat`) and before it opens that entry, with the entry's source
// path. Lets a test swap the entry between the type check and the open.
#[cfg(test)]
thread_local! {
    static BEFORE_OPEN_HOOK: std::cell::RefCell<Option<AfterListHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_before_open_hook(f: Option<AfterListHook>) {
    BEFORE_OPEN_HOOK.with(|h| *h.borrow_mut() = f);
}

#[cfg(test)]
pub(crate) fn run_before_open_hook(entry: &Path) {
    BEFORE_OPEN_HOOK.with(|h| {
        if let Some(f) = h.borrow_mut().as_mut() {
            f(entry);
        }
    });
}

#[cfg(test)]
pub(crate) fn run_after_list_hook(dir: &Path) {
    AFTER_LIST_HOOK.with(|h| {
        if let Some(f) = h.borrow_mut().as_mut() {
            f(dir);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn setup() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("out");
        fs::create_dir_all(root.join("sub")).unwrap();
        let secret = tmp.path().join("secret.txt");
        fs::write(&secret, "SECRET").unwrap();
        (tmp, root, secret)
    }

    #[test]
    fn reads_a_regular_file_under_a_subdir() {
        let (_t, root, _s) = setup();
        fs::write(root.join("sub/a.txt"), "hello").unwrap();
        let got = read_contained_to_string(&root, Path::new("sub/a.txt"), 100).unwrap();
        assert_eq!(got, "hello");
    }

    #[test]
    fn refuses_a_final_component_symlink() {
        let (_t, root, secret) = setup();
        symlink(&secret, root.join("sub/a.txt")).unwrap();
        let err = read_contained(&root, Path::new("sub/a.txt"), 100).unwrap_err();
        assert!(err.is_refused() && err.to_string().contains("symlink"), "{err}");
    }

    #[test]
    fn refuses_an_intermediate_directory_symlink() {
        let (t, root, _s) = setup();
        let host_dir = t.path().join("host");
        fs::create_dir_all(&host_dir).unwrap();
        fs::write(host_dir.join("a.txt"), "HOST").unwrap();
        fs::remove_dir_all(root.join("sub")).unwrap();
        symlink(&host_dir, root.join("sub")).unwrap();
        let err = read_contained(&root, Path::new("sub/a.txt"), 100).unwrap_err();
        assert!(err.is_refused() && err.to_string().contains("symlink"), "{err}");
    }

    #[test]
    fn refuses_parent_dir_and_absolute_paths() {
        let (_t, root, _s) = setup();
        for rel in ["../secret.txt", "sub/../../secret.txt", "/etc/hosts", ""] {
            let err = read_contained(&root, Path::new(rel), 100).unwrap_err();
            assert!(
                err.is_refused() && err.to_string().contains("escapes the directory"),
                "{rel}: {err}"
            );
        }
    }

    #[test]
    fn refuses_a_fifo_and_a_directory() {
        let (_t, root, _s) = setup();
        let fifo = root.join("sub/pipe");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let err = read_contained(&root, Path::new("sub/pipe"), 100).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
        let err = read_contained(&root, Path::new("sub"), 100).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
    }

    #[test]
    fn missing_is_not_found_not_refused() {
        let (_t, root, _s) = setup();
        assert!(matches!(
            read_contained(&root, Path::new("sub/nope"), 100),
            Err(ContainedFileError::NotFound)
        ));
        assert!(matches!(
            read_contained(&root, Path::new("nodir/nope"), 100),
            Err(ContainedFileError::NotFound)
        ));
    }

    #[test]
    fn refuses_a_file_over_the_cap() {
        let (_t, root, _s) = setup();
        fs::write(root.join("sub/big"), vec![b'x'; 11]).unwrap();
        let err = read_contained(&root, Path::new("sub/big"), 10).unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");
        assert_eq!(read_contained(&root, Path::new("sub/big"), 11).unwrap().len(), 11);
    }

    #[test]
    fn copy_writes_nothing_for_a_refused_source() {
        let (t, root, secret) = setup();
        symlink(&secret, root.join("sub/a.txt")).unwrap();
        let dst = t.path().join("dst.txt");
        assert!(copy_contained(&root, Path::new("sub/a.txt"), &dst, 100).is_err());
        assert!(!dst.exists());
    }

    /// On APFS the tree copy clones file bytes (copy-on-write) from the
    /// verified fd instead of streaming them, keeping the gate's copy cheap.
    #[cfg(target_os = "macos")]
    #[test]
    fn copy_tree_nofollow_clones_regular_files_on_macos() {
        let (t, root, _s) = setup();
        fs::write(root.join("sub/a.txt"), "hello").unwrap();
        let dst = t.path().join("dst");
        CLONED_FILES.with(|n| n.set(0));
        let report = copy_tree_nofollow(&root, &dst).unwrap();
        assert_eq!(report.files, 1);
        assert_eq!(fs::read_to_string(dst.join("sub/a.txt")).unwrap(), "hello");
        assert_eq!(CLONED_FILES.with(|n| n.get()), 1, "the file was not cloned");
    }

    #[test]
    fn copy_tree_nofollow_refuses_a_symlinked_root() {
        let (t, root, _s) = setup();
        let link = t.path().join("link");
        symlink(&root, &link).unwrap();
        let err = copy_tree_nofollow(&link, &t.path().join("dst")).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
    }

    #[test]
    fn relative_link_stays_inside_is_lexical() {
        assert!(relative_link_stays_inside(Path::new("bin"), Path::new("../lib/x")));
        assert!(relative_link_stays_inside(Path::new(""), Path::new("lib")));
        assert!(!relative_link_stays_inside(Path::new("bin"), Path::new("../../x")));
        assert!(!relative_link_stays_inside(Path::new(""), Path::new("..")));
        assert!(!relative_link_stays_inside(Path::new("a"), Path::new("/etc/hosts")));
        assert!(relative_link_stays_inside(Path::new("a/b"), Path::new("./../../c")));
        // (#2869 F1) `..` after a Normal component is refused: the kernel
        // resolves the Normal first, and it may itself be a recreated link.
        assert!(!relative_link_stays_inside(Path::new(""), Path::new("x/..")));
        assert!(!relative_link_stays_inside(Path::new(""), Path::new("sub/up/../s")));
        // Refused even where the link is deep enough that counting the `..`
        // against its depth alone would admit it.
        assert!(!relative_link_stays_inside(Path::new("x"), Path::new("sub/up/../s")));
        assert!(relative_link_stays_inside(Path::new("node_modules/.bin"), Path::new("../pkg/bin.js")));
    }

    /// (#2869 F1, the review's probe) `sub/up -> ..` is a legitimate in-tree
    /// link, but `esc -> sub/up/sub/up/../HOST-SECRET` climbs through it:
    /// lexically `up/..` cancels, physically `sub/up` is the root and `..`
    /// from it is the directory ABOVE the copy. Recreating `esc` would let
    /// `test_command` read a host file through the scratch checkout.
    #[test]
    fn copy_tree_nofollow_does_not_recreate_a_link_that_climbs_through_another_link() {
        let (t, root, _s) = setup();
        symlink("..", root.join("sub/up")).unwrap();
        symlink("sub/up/sub/up/../HOST-SECRET", root.join("esc")).unwrap();
        fs::write(t.path().join("HOST-SECRET"), "HOST").unwrap();
        let dst = t.path().join("dst");

        let report = copy_tree_nofollow(&root, &dst).unwrap();

        assert!(
            fs::read_to_string(dst.join("esc")).is_err(),
            "dst/esc resolves to a host file outside the copy"
        );
        assert!(report.skipped.iter().any(|(p, _)| p == Path::new("esc")), "{:?}", report.skipped);
        assert_eq!(fs::read_link(dst.join("sub/up")).unwrap(), Path::new(".."), "the plain in-tree link stays");
    }

    /// The same climb from a link one level down, where the target's single
    /// `..` is within the link's own depth: `x/sub/up -> ../..` is the root,
    /// so `x/esc -> sub/up/../HOST-SECRET` is the directory above the copy.
    #[test]
    fn copy_tree_nofollow_does_not_recreate_a_deeper_link_that_climbs_through_another_link() {
        let (t, root, _s) = setup();
        fs::create_dir_all(root.join("x/sub")).unwrap();
        symlink("../..", root.join("x/sub/up")).unwrap();
        symlink("sub/up/../HOST-SECRET", root.join("x/esc")).unwrap();
        fs::write(t.path().join("HOST-SECRET"), "HOST").unwrap();
        let dst = t.path().join("dst");

        copy_tree_nofollow(&root, &dst).unwrap();

        assert!(fs::read_to_string(dst.join("x/esc")).is_err(), "dst/x/esc resolves outside the copy");
        assert_eq!(fs::read_link(dst.join("x/sub/up")).unwrap(), Path::new("../.."));
    }

    /// (#2869 C3) One fd is held per directory level; a tree deeper than
    /// the cap is refused with a clear error instead of hitting EMFILE.
    #[test]
    fn copy_tree_nofollow_refuses_a_tree_deeper_than_the_cap() {
        let (t, root, _s) = setup();
        let mut deep = root.clone();
        for _ in 0..(MAX_TREE_DEPTH + 2) {
            deep.push("d");
        }
        fs::create_dir_all(&deep).unwrap();
        let err = copy_tree_nofollow(&root, &t.path().join("dst")).unwrap_err();
        assert!(err.to_string().contains("deeper than"), "{err}");
    }

    /// (#2869 C2) The fd `fdopendir` consumes is a close-on-exec dup, so a
    /// concurrent fork+exec (a gate's `test_command`) never inherits it.
    #[test]
    fn dup_cloexec_sets_close_on_exec() {
        let f = File::open(".").unwrap();
        let fd: std::os::fd::OwnedFd = f.into();
        let d = tree::dup_cloexec(&fd).unwrap();
        use std::os::fd::AsRawFd;
        // SAFETY: querying flags of a live fd.
        let flags = unsafe { libc::fcntl(d.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0, "flags {flags}");
    }

    /// (#2869 C4) Destination writes never follow a link already sitting in
    /// the destination: a planted `dst/f.txt -> <host file>` is not written
    /// through, and a planted `dst/sub -> <host dir>` is not written into.
    #[test]
    fn copy_tree_nofollow_does_not_write_through_links_in_the_destination() {
        let (t, root, _s) = setup();
        fs::write(root.join("f.txt"), "FROM-SRC").unwrap();
        fs::write(root.join("sub/g.txt"), "FROM-SRC").unwrap();
        let host_file = t.path().join("host-file");
        fs::write(&host_file, "ORIGINAL").unwrap();
        let host_dir = t.path().join("host-dir");
        fs::create_dir_all(&host_dir).unwrap();

        let dst = t.path().join("dst");
        fs::create_dir_all(&dst).unwrap();
        symlink(&host_file, dst.join("f.txt")).unwrap();
        let _ = copy_tree_nofollow(&root, &dst);
        assert_eq!(fs::read_to_string(&host_file).unwrap(), "ORIGINAL", "wrote through a dst file link");
        // Again on the byte-copy path (other volumes; every non-macOS host).
        fs::remove_file(dst.join("f.txt")).ok();
        fs::remove_dir_all(dst.join("sub")).ok();
        symlink(&host_file, dst.join("f.txt")).unwrap();
        FORCE_BYTE_COPY.with(|f| f.set(true));
        let _ = copy_tree_nofollow(&root, &dst);
        FORCE_BYTE_COPY.with(|f| f.set(false));
        assert_eq!(fs::read_to_string(&host_file).unwrap(), "ORIGINAL", "byte copy wrote through a dst link");

        let dst2 = t.path().join("dst2");
        fs::create_dir_all(&dst2).unwrap();
        symlink(&host_dir, dst2.join("sub")).unwrap();
        let _ = copy_tree_nofollow(&root, &dst2);
        assert!(!host_dir.join("g.txt").exists(), "wrote into a dst directory link");
    }

    /// (#2869 C6) An entry swapped between the type check and the open is
    /// reported as what it actually became, not always "symlink".
    #[test]
    fn a_swapped_entry_is_reported_as_its_actual_type() {
        let (t, root, _s) = setup();
        fs::create_dir_all(root.join("d")).unwrap();
        let dst = t.path().join("dst");
        let report = copy_with_swap_before_open(&root, "d", |p| {
            fs::remove_dir(p).unwrap();
            let c = std::ffi::CString::new(p.to_str().unwrap()).unwrap();
            // SAFETY: valid NUL-terminated path.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        }, &dst);
        let why = &report.skipped.iter().find(|(p, _)| p == Path::new("d")).expect("skipped").1;
        assert!(why.contains("fifo") && !why.contains("symlink"), "{why}");
    }

    /// (#2869 C5) An over-cap source leaves nothing at the destination.
    #[test]
    fn copy_contained_leaves_nothing_behind_for_an_over_cap_file() {
        let (t, root, _s) = setup();
        fs::write(root.join("sub/big"), vec![b'x'; 64]).unwrap();
        let dst = t.path().join("big-copy");
        let err = copy_contained(&root, Path::new("sub/big"), &dst, 10).unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");
        assert!(!dst.exists(), "a partial copy was left behind");
        assert_eq!(copy_contained(&root, Path::new("sub/big"), &dst, 64).unwrap(), 64);
        assert_eq!(fs::read(&dst).unwrap().len(), 64);
    }

    /// (#2869 review 3) `dst` is created exclusively: an existing file is
    /// refused, never truncated, so an over-cap source cannot delete it.
    #[test]
    fn copy_contained_refuses_an_existing_destination_and_leaves_it_intact() {
        let (t, root, _s) = setup();
        fs::write(root.join("sub/big"), vec![b'x'; 64]).unwrap();
        fs::write(root.join("sub/small"), "new").unwrap();
        let dst = t.path().join("existing");
        fs::write(&dst, "KEEP").unwrap();

        assert!(copy_contained(&root, Path::new("sub/small"), &dst, 64).is_err(), "overwrote an existing dst");
        assert_eq!(fs::read_to_string(&dst).unwrap(), "KEEP");
        assert!(copy_contained(&root, Path::new("sub/big"), &dst, 10).is_err());
        assert_eq!(fs::read_to_string(&dst).unwrap(), "KEEP", "an over-cap copy removed the existing dst");
    }

    /// Swap `src/<name>` for `with` between the walk's type check and its
    /// open, run the copy, and return the report.
    fn copy_with_swap_before_open(
        root: &Path,
        name: &'static str,
        swap: impl Fn(&Path) + 'static,
        dst: &Path,
    ) -> TreeCopyReport {
        let target = root.join(name);
        set_before_open_hook(Some(Box::new(move |p: &Path| {
            if p == target.as_path() {
                swap(p);
            }
        })));
        let r = copy_tree_nofollow(root, dst);
        set_before_open_hook(None);
        r.unwrap()
    }

    #[test]
    fn copy_tree_nofollow_refuses_a_dir_swapped_for_a_link_between_stat_and_open() {
        let (t, root, _s) = setup();
        fs::create_dir_all(root.join("d")).unwrap();
        let host = t.path().join("host");
        fs::create_dir_all(&host).unwrap();
        fs::write(host.join("f.txt"), "HOST").unwrap();
        let h = host.clone();
        let dst = t.path().join("dst");
        let report = copy_with_swap_before_open(&root, "d", move |p| {
            fs::remove_dir(p).unwrap();
            symlink(&h, p).unwrap();
        }, &dst);
        assert!(!dst.join("d/f.txt").exists(), "followed a directory swapped for a link");
        assert!(report.skipped.iter().any(|(p, _)| p == Path::new("d")), "{:?}", report.skipped);
    }

    #[test]
    fn copy_tree_nofollow_refuses_a_file_swapped_for_a_link_between_stat_and_open() {
        let (t, root, secret) = setup();
        fs::write(root.join("f.txt"), "REAL").unwrap();
        let dst = t.path().join("dst");
        let report = copy_with_swap_before_open(&root, "f.txt", move |p| {
            fs::remove_file(p).unwrap();
            symlink(&secret, p).unwrap();
        }, &dst);
        assert!(!dst.join("f.txt").exists(), "followed a file swapped for a link");
        assert_eq!(report.skipped.len(), 1, "{:?}", report.skipped);
    }

    #[test]
    fn copy_tree_nofollow_refuses_a_file_swapped_for_a_fifo_between_stat_and_open() {
        let (t, root, _s) = setup();
        fs::write(root.join("f.txt"), "REAL").unwrap();
        let dst = t.path().join("dst");
        let report = copy_with_swap_before_open(&root, "f.txt", |p| {
            fs::remove_file(p).unwrap();
            let c = std::ffi::CString::new(p.to_str().unwrap()).unwrap();
            // SAFETY: valid NUL-terminated path.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        }, &dst);
        assert!(!dst.join("f.txt").exists(), "a FIFO was copied as a file");
        assert_eq!(report.skipped.len(), 1, "{:?}", report.skipped);
    }
}
