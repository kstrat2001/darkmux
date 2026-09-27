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
/// trusts and is opened normally. For a caller that holds a full path
/// rather than a (root, relative) pair, e.g. the trajectory tailer's
/// `<out_dir>/.darkmux-runtime/trajectory.jsonl` with `n = 2`.
pub fn open_path_tail(path: &Path, n: usize) -> Result<File, ContainedFileError> {
    let comps: Vec<Component<'_>> = path.components().collect();
    let n = n.clamp(1, comps.len().max(1));
    let split = comps.len().saturating_sub(n);
    let root: std::path::PathBuf = comps[..split].iter().collect();
    let root = if root.as_os_str().is_empty() { std::path::PathBuf::from(".") } else { root };
    let rel: std::path::PathBuf = comps[split..].iter().collect();
    open_contained(&root, &rel)
}

/// Open a single `path` whose parent directory the caller already trusts
/// (it walked there with no-follow directory entries). The final component
/// gets the same no-follow, non-blocking, regular-file-only treatment as
/// [`open_contained`].
pub fn open_regular_nofollow(path: &Path) -> Result<File, ContainedFileError> {
    open_path_tail(path, 1)
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

    #[test]
    fn open_regular_nofollow_refuses_a_symlink() {
        let (_t, root, secret) = setup();
        symlink(&secret, root.join("sub/a.txt")).unwrap();
        let err = open_regular_nofollow(&root.join("sub/a.txt")).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        fs::write(root.join("sub/b.txt"), "b").unwrap();
        assert!(open_regular_nofollow(&root.join("sub/b.txt")).is_ok());
    }
}
