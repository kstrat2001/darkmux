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
use std::io::{self, Read, Write};
use std::path::{Component, Path};

/// Default read cap for runtime bookkeeping files (trajectory, metrics,
/// findings). Generous: a long agentic run's trajectory is tens of MB. The
/// cap exists so a model cannot make the host buffer an unbounded file,
/// not to police ordinary sizes.
pub const DEFAULT_MAX_BYTES: u64 = 1024 * 1024 * 1024;

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

/// Copy `root/rel` to `dst` (created or truncated), bounded by `max_bytes`.
/// `dst` is host-owned and is written normally. Nothing is written when the
/// source is refused or missing. Returns the number of bytes copied.
pub fn copy_contained(
    root: &Path,
    rel: &Path,
    dst: &Path,
    max_bytes: u64,
) -> Result<u64, ContainedFileError> {
    let bytes = read_contained(root, rel, max_bytes)?;
    let mut out = File::create(dst).map_err(ContainedFileError::Io)?;
    out.write_all(&bytes).map_err(ContainedFileError::Io)?;
    Ok(bytes.len() as u64)
}

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
/// copy root) whose target is `target` resolves, lexically, to a path
/// inside the root. Absolute targets never do.
fn relative_link_stays_inside(link_dir: &Path, target: &Path) -> bool {
    let mut depth: usize = link_dir
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count();
    for c in target.components() {
        match c {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            }
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
/// directory fd down the recursion: each directory is listed through its
/// own fd (`fdopendir`), each entry's type comes from `fstatat(...,
/// AT_SYMLINK_NOFOLLOW)` relative to that fd, a subdirectory is opened with
/// `openat(O_DIRECTORY|O_NOFOLLOW)` and a file with
/// `openat(O_NOFOLLOW|O_NONBLOCK)` then checked `S_ISREG` on the open fd.
/// So a directory renamed away and replaced with a link after it was
/// opened is still read through the fd we hold (its original contents),
/// and one replaced before it was opened fails the no-follow open and is
/// skipped.
///
/// A symlink is recreated verbatim when its target is RELATIVE and stays
/// inside the tree (real checkouts commit these: `node_modules/.bin/*`,
/// docs links); an absolute or escaping one is skipped and reported. The
/// recreated link points at the COPY, whose entries were all vetted, so a
/// chain through a skipped link dangles rather than escapes.
///
/// Regular files keep their permission bits. On macOS the bytes are cloned
/// from the verified fd (`fclonefileat`, copy-on-write on APFS), falling
/// back to a plain copy across volumes; elsewhere `std::io::copy`, which
/// uses `copy_file_range` on Linux.
///
/// `dst` is the caller's fresh, host-owned directory (created if absent)
/// and is written by path.
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
    std::fs::create_dir_all(dst)?;
    tree::walk(&root_fd, src, Path::new(""), dst, &mut report)?;
    Ok(report)
}

/// Non-Unix hosts are unsupported (see this crate's Cargo.toml); refuse
/// rather than fall back to a following copy.
#[cfg(not(unix))]
pub fn copy_tree_nofollow(_src: &Path, _dst: &Path) -> io::Result<TreeCopyReport> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "copy_tree_nofollow needs a Unix host"))
}

#[cfg(unix)]
mod tree {
    use super::{relative_link_stays_inside, TreeCopyReport};
    use std::ffi::{CStr, CString, OsStr, OsString};
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    fn cstr(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
    }

    pub(super) fn openat(dir: &OwnedFd, name: &OsStr, flags: libc::c_int) -> io::Result<OwnedFd> {
        let c = cstr(name)?;
        // SAFETY: live fd, valid C string.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: freshly returned, unowned fd.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Entry names of the directory `dir` refers to, read through a dup of
    /// that fd (never by path). `.` and `..` excluded.
    fn list(dir: &OwnedFd) -> io::Result<Vec<OsString>> {
        // SAFETY: dup of a live fd; fdopendir takes ownership of the dup.
        let dupfd = unsafe { libc::dup(dir.as_raw_fd()) };
        if dupfd < 0 {
            return Err(io::Error::last_os_error());
        }
        let dp = unsafe { libc::fdopendir(dupfd) };
        if dp.is_null() {
            let e = io::Error::last_os_error();
            unsafe { libc::close(dupfd) };
            return Err(e);
        }
        // The dup shares the file offset with `dir`; start from the top.
        unsafe { libc::rewinddir(dp) };
        let mut out = Vec::new();
        loop {
            // SAFETY: `dp` is a valid DIR*; the returned entry is valid until
            // the next readdir on it, and we copy the name out immediately.
            let ent = unsafe { libc::readdir(dp) };
            if ent.is_null() {
                break;
            }
            let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." {
                out.push(OsString::from_vec(name.to_vec()));
            }
        }
        unsafe { libc::closedir(dp) };
        Ok(out)
    }

    fn lstat_at(dir: &OwnedFd, name: &OsStr) -> io::Result<libc::stat> {
        let c = cstr(name)?;
        // SAFETY: zeroed stat is a valid out-param; fd and string are live.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::fstatat(dir.as_raw_fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st)
    }

    fn readlink_at(dir: &OwnedFd, name: &OsStr) -> io::Result<PathBuf> {
        let c = cstr(name)?;
        let mut buf = vec![0u8; libc::PATH_MAX as usize + 1];
        // SAFETY: buffer is writable for its full length.
        let n = unsafe {
            libc::readlinkat(dir.as_raw_fd(), c.as_ptr(), buf.as_mut_ptr().cast(), buf.len())
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        buf.truncate(n as usize);
        Ok(PathBuf::from(OsString::from_vec(buf)))
    }

    /// Copy the verified regular file `from` to `dst_dir/name`.
    fn copy_file(from: File, dst_dir: &Path, name: &OsStr, mode: u32) -> io::Result<()> {
        let dst = dst_dir.join(name);
        #[cfg(target_os = "macos")]
        {
            let dst_dir_fd: OwnedFd = File::open(dst_dir)?.into();
            let c = cstr(name)?;
            // `CLONE_NOFOLLOW` from <sys/clonefile.h>; the libc crate does
            // not export it. Never follow at the destination either.
            const CLONE_NOFOLLOW: u32 = 0x0001;
            // SAFETY: both fds live, valid C string.
            let rc = unsafe {
                libc::fclonefileat(from.as_raw_fd(), dst_dir_fd.as_raw_fd(), c.as_ptr(), CLONE_NOFOLLOW)
            };
            if rc == 0 {
                #[cfg(test)]
                super::CLONED_FILES.with(|n| n.set(n.get() + 1));
                std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(mode & 0o7777))?;
                return Ok(());
            }
            // Not clonable (another volume, not APFS): plain copy below.
        }
        let mut from = from;
        let mut to = File::create(&dst)?;
        io::copy(&mut from, &mut to)?;
        to.set_permissions(std::fs::Permissions::from_mode(mode & 0o7777))?;
        Ok(())
    }

    pub(super) fn walk(
        dir: &OwnedFd,
        src_dir: &Path,
        rel: &Path,
        dst_dir: &Path,
        report: &mut TreeCopyReport,
    ) -> io::Result<()> {
        let names = list(dir)?;
        #[cfg(test)]
        super::run_after_list_hook(src_dir);
        for name in names {
            let rel_entry = rel.join(&name);
            let skip = |report: &mut TreeCopyReport, why: String| report.skipped.push((rel_entry.clone(), why));
            let st = match lstat_at(dir, &name) {
                Ok(st) => st,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            #[cfg(test)]
            super::run_before_open_hook(&src_dir.join(&name));
            match st.st_mode & libc::S_IFMT {
                libc::S_IFDIR => {
                    let sub = match openat(dir, &name, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW) {
                        Ok(fd) => fd,
                        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) => {
                            skip(report, "changed from a directory to a symlink during the copy".into());
                            continue;
                        }
                        Err(e) => return Err(e),
                    };
                    let dst_sub = dst_dir.join(&name);
                    std::fs::create_dir_all(&dst_sub)?;
                    walk(&sub, &src_dir.join(&name), &rel_entry, &dst_sub, report)?;
                }
                libc::S_IFREG => {
                    let fd = match openat(dir, &name, libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK) {
                        Ok(fd) => fd,
                        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::EMLINK)) => {
                            skip(report, "changed from a file to a symlink during the copy".into());
                            continue;
                        }
                        Err(e) => return Err(e),
                    };
                    let file = File::from(fd);
                    let meta = file.metadata()?;
                    if !meta.file_type().is_file() {
                        skip(report, "changed to a non-regular file during the copy".into());
                        continue;
                    }
                    copy_file(file, dst_dir, &name, meta.permissions().mode())?;
                    report.files += 1;
                }
                libc::S_IFLNK => {
                    let target = readlink_at(dir, &name)?;
                    if target.is_relative() && relative_link_stays_inside(rel, &target) {
                        std::os::unix::fs::symlink(&target, dst_dir.join(&name))?;
                        report.links_recreated.push(rel_entry);
                    } else {
                        skip(
                            report,
                            format!("symlink to `{}` points outside the tree (not recreated)", target.display()),
                        );
                    }
                }
                _ => skip(report, "not a regular file, directory or symlink".into()),
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
