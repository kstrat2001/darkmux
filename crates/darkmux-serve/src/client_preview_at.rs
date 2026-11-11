//! Files inside a folder a run can write, reached one name at a time from a
//! descriptor of that folder: every step is `openat` with `O_NOFOLLOW`, so a
//! symlink anywhere on the way is refused at the moment it is crossed, not
//! at an earlier check a writer could race (swap a folder for a link between
//! the check and the act). What is deleted or created is therefore always
//! inside the folder the walk started from.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path};

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what.to_string())
}

/// The last OS error, with a symlink met on the way said plainly: `ELOOP`
/// from `O_NOFOLLOW`, or `ENOTDIR` where a folder was asked for (macOS
/// answers a link that way with `O_DIRECTORY`; so does a plain file).
fn os_error() -> io::Error {
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ELOOP) => io::Error::other("a symlink is on the way"),
        Some(libc::ENOTDIR) => io::Error::other("a symlink or a file is on the way where a folder should be"),
        _ => e,
    }
}

fn cstr(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| invalid("a NUL in a path"))
}

/// `openat(dir, name)` with `O_NOFOLLOW | O_CLOEXEC` added.
fn open_at(dir: RawFd, name: &OsStr, flags: libc::c_int, mode: libc::c_uint) -> io::Result<OwnedFd> {
    let c = cstr(name)?;
    // SAFETY: a valid NUL-terminated name; the fd is checked before use.
    let fd = unsafe { libc::openat(dir, c.as_ptr(), flags | libc::O_NOFOLLOW | libc::O_CLOEXEC, mode) };
    if fd < 0 {
        return Err(os_error());
    }
    // SAFETY: `openat` just returned this fd, owned by nobody else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `name` in `dir` as it is, never followed: `None` when absent.
fn lstat_at(dir: RawFd, name: &OsStr) -> io::Result<Option<libc::stat>> {
    let c = cstr(name)?;
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: valid name and an out pointer to a whole `stat`.
    let rc = unsafe { libc::fstatat(dir, c.as_ptr(), st.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
    if rc == 0 {
        // SAFETY: `fstatat` filled it.
        return Ok(Some(unsafe { st.assume_init() }));
    }
    let e = io::Error::last_os_error();
    match e.kind() {
        io::ErrorKind::NotFound => Ok(None),
        _ => Err(e),
    }
}

fn is_dir(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFDIR
}

fn unlink_at(dir: RawFd, name: &OsStr, flags: libc::c_int) -> io::Result<()> {
    let c = cstr(name)?;
    // SAFETY: a valid NUL-terminated name.
    match unsafe { libc::unlinkat(dir, c.as_ptr(), flags) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// The names in the folder `dir` (not `.` or `..`).
fn names_in(dir: &OwnedFd) -> io::Result<Vec<OsString>> {
    // SAFETY: duplicating an fd we own; the copy is handed to `fdopendir`,
    // which owns it from then on and closes it in `closedir`.
    let dup = unsafe { libc::dup(dir.as_raw_fd()) };
    if dup < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `dup` is a fresh fd of a directory.
    let stream = unsafe { libc::fdopendir(dup) };
    if stream.is_null() {
        let e = io::Error::last_os_error();
        // SAFETY: `fdopendir` failed, so `dup` is still ours to close.
        unsafe { libc::close(dup) };
        return Err(e);
    }
    let mut names = Vec::new();
    loop {
        // SAFETY: `stream` is open; an entry is valid until the next call.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        // SAFETY: `d_name` is NUL-terminated within the entry.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.push(OsStr::from_bytes(name).to_os_string());
        }
    }
    // SAFETY: closes the stream and the fd it owns.
    unsafe { libc::closedir(stream) };
    Ok(names)
}

/// The deepest tree [`remove`] deletes, in folders. The walk holds one
/// descriptor per level, and `client serve` runs under launchd's 256-file
/// limit; no dependency tree comes near this, so a deeper one (a run built
/// it) is refused with a reason rather than run out of descriptors.
pub(crate) const MAX_DELETE_DEPTH: usize = 128;

/// One open folder in [`remove_tree_at`]'s walk: its descriptor, its name
/// in the folder below it on the stack, and the entries left to visit.
struct Frame {
    fd: OwnedFd,
    name: OsString,
    pending: Vec<OsString>,
}

impl Frame {
    fn open(dir: RawFd, name: &OsStr) -> io::Result<Frame> {
        let fd = open_at(dir, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        let pending = names_in(&fd)?;
        Ok(Frame { fd, name: name.to_os_string(), pending })
    }
}

/// Removes `name` in `dir`: a folder with everything in it, each entry
/// reached from its own folder's descriptor; a file or a symlink unlinked
/// (a link is never followed). Iterative, with the open folders on an
/// explicit stack: a run can build a tree deeper than any thread's stack.
fn remove_tree_at(dir: RawFd, name: &OsStr) -> io::Result<()> {
    let Some(first) = remove_or_open(dir, name)? else {
        return Ok(());
    };
    let mut stack = vec![first];
    while let Some(top) = stack.last_mut() {
        match top.pending.pop() {
            Some(entry) => {
                if let Some(sub) = remove_or_open(top.fd.as_raw_fd(), &entry)? {
                    push_within_depth(&mut stack, sub)?;
                }
            }
            None => {
                let done = stack.pop().expect("a frame");
                let parent = stack.last().map_or(dir, |f| f.fd.as_raw_fd());
                unlink_at(parent, &done.name, libc::AT_REMOVEDIR)?;
            }
        }
    }
    Ok(())
}

/// Descends into `sub`, unless that would make the walk deeper than
/// [`MAX_DELETE_DEPTH`].
fn push_within_depth(stack: &mut Vec<Frame>, sub: Frame) -> io::Result<()> {
    if stack.len() >= MAX_DELETE_DEPTH {
        return Err(io::Error::other(format!(
            "the tree is deeper than {MAX_DELETE_DEPTH} folders, more than darkmux deletes; delete it yourself"
        )));
    }
    stack.push(sub);
    Ok(())
}

/// `entry` in `at`: a folder is opened for the walk to descend into;
/// anything else is unlinked now (absent is done).
fn remove_or_open(at: RawFd, entry: &OsStr) -> io::Result<Option<Frame>> {
    match lstat_at(at, entry)? {
        Some(st) if is_dir(&st) => Frame::open(at, entry).map(Some),
        Some(_) => unlink_at(at, entry, 0).map(|_| None),
        None => Ok(None),
    }
}

/// The folder holding `rel` under `root`, opened one folder at a time with
/// `O_NOFOLLOW | O_DIRECTORY`, and `rel`'s last name. `rel` must be plain
/// names only.
fn parent_of(root: &Path, rel: &str) -> io::Result<(OwnedFd, OsString)> {
    let parts = Path::new(rel)
        .components()
        .map(|c| match c {
            Component::Normal(n) => Ok(n),
            _ => Err(invalid("not a plain relative path")),
        })
        .collect::<io::Result<Vec<&OsStr>>>()?;
    let (last, folders) = parts.split_last().ok_or_else(|| invalid("an empty path"))?;
    let root = cstr(root.as_os_str())?;
    // SAFETY: a valid NUL-terminated path; the fd is checked before use.
    let fd = unsafe { libc::open(root.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `open` just returned this fd.
    let mut at = unsafe { OwnedFd::from_raw_fd(fd) };
    for folder in folders {
        at = open_at(at.as_raw_fd(), folder, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    }
    Ok((at, last.to_os_string()))
}

/// Deletes `rel` inside `root`; absent is done.
pub(crate) fn remove(root: &Path, rel: &str) -> io::Result<()> {
    let (dir, name) = parent_of(root, rel)?;
    remove_tree_at(dir.as_raw_fd(), &name)
}

/// Creates the new file `rel` inside `root` with permission bits `mode`;
/// refused when anything is already there.
pub(crate) fn create_new(root: &Path, rel: &str, mode: u32) -> io::Result<File> {
    let (dir, name) = parent_of(root, rel)?;
    let fd = open_at(dir.as_raw_fd(), &name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, mode as libc::c_uint)?;
    Ok(File::from(fd))
}

/// Writes `bytes` as the new file `rel` inside `root` with permission bits
/// `mode`, making each missing folder on the way (`0o755`). No link is
/// followed at any name; refused when anything is already at `rel`.
pub(crate) fn write_new_making_folders(
    root: &Path,
    rel: &str,
    bytes: &[u8],
    mode: u32,
) -> io::Result<()> {
    use io::Write;
    let parts = Path::new(rel)
        .components()
        .map(|c| match c {
            Component::Normal(n) => Ok(n),
            _ => Err(invalid("not a plain relative path")),
        })
        .collect::<io::Result<Vec<&OsStr>>>()?;
    let (last, folders) = parts.split_last().ok_or_else(|| invalid("an empty path"))?;
    let root = cstr(root.as_os_str())?;
    // SAFETY: a valid NUL-terminated path; the fd is checked before use.
    let fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `open` just returned this fd.
    let mut at = unsafe { OwnedFd::from_raw_fd(fd) };
    for folder in folders {
        let c = cstr(folder)?;
        // SAFETY: a valid NUL-terminated name and an fd we own.
        if unsafe { libc::mkdirat(at.as_raw_fd(), c.as_ptr(), 0o755) } != 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::AlreadyExists {
                return Err(e);
            }
        }
        at = open_at(
            at.as_raw_fd(),
            folder,
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        )?;
    }
    let mut out = File::from(open_at(
        at.as_raw_fd(),
        last,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        mode,
    )?);
    out.write_all(bytes)?;
    out.set_permissions(std::fs::Permissions::from_mode(mode))?;
    out.sync_all()
}

/// Opens the regular file `rel` inside `root` for reading, with its
/// permission bits. Opened `O_NONBLOCK`, so a FIFO (or a device) there
/// never waits for a writer before it is refused; blocking reads again once
/// it is known to be a regular file.
pub(crate) fn open_regular(root: &Path, rel: &str) -> io::Result<(File, u32)> {
    let (dir, name) = parent_of(root, rel)?;
    let file = regular_at(&dir, &name)?;
    let mode = file.metadata()?.permissions().mode() & 0o777;
    Ok((file, mode))
}

/// Replaces the regular file `rel` inside `root` with `new`, only while it
/// still holds `expected`. The file is opened once, with no link followed
/// at any name on the way, and read through that descriptor; `new` goes to
/// a fresh file beside it, renamed over it in the folder the walk opened,
/// so a link put there meanwhile is replaced, never written through. Its
/// permission bits are kept.
pub(crate) fn replace_regular(root: &Path, rel: &str, expected: &[u8], new: &[u8]) -> io::Result<()> {
    use io::Read;
    let (dir, name) = parent_of(root, rel)?;
    let file = regular_at(&dir, &name)?;
    let mode = file.metadata()?.mode() & 0o777;
    let mut held = Vec::with_capacity(expected.len());
    file.take(expected.len() as u64 + 1).read_to_end(&mut held)?;
    if held != expected {
        return Err(io::Error::other("it changed while darkmux read it"));
    }
    let mut temp = OsString::from(".");
    temp.push(&name);
    temp.push(".darkmux-edit");
    let renamed = write_new_at(&dir, &temp, new, mode).and_then(|()| rename_in(&dir, &temp, &name));
    if renamed.is_err() {
        let _ = unlink_at(dir.as_raw_fd(), &temp, 0);
    }
    renamed
}

/// Writes `bytes` to a new file `name` in `dir` with permission bits
/// `mode`, synced; one left there by an edit that never finished is
/// unlinked first, never followed.
fn write_new_at(dir: &OwnedFd, name: &OsStr, bytes: &[u8], mode: u32) -> io::Result<()> {
    use io::Write;
    if let Err(e) = unlink_at(dir.as_raw_fd(), name, 0) {
        if e.kind() != io::ErrorKind::NotFound {
            return Err(e);
        }
    }
    let mut out = File::from(open_at(dir.as_raw_fd(), name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, mode)?);
    out.write_all(bytes)?;
    // The umask may have narrowed the bits `O_CREAT` set.
    out.set_permissions(std::fs::Permissions::from_mode(mode))?;
    out.sync_all()
}

/// The regular file `name` in `dir`, opened read-only without following a
/// link and without waiting on a FIFO.
fn regular_at(dir: &OwnedFd, name: &OsStr) -> io::Result<File> {
    let file = File::from(open_at(dir.as_raw_fd(), name, libc::O_RDONLY | libc::O_NONBLOCK, 0)?);
    if !file.metadata()?.is_file() {
        return Err(invalid("not a regular file"));
    }
    // SAFETY: an fd we own; reads its flags and clears one.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

/// `renameat(dir, from, dir, to)`: names in one folder, neither followed.
fn rename_in(dir: &OwnedFd, from: &OsStr, to: &OsStr) -> io::Result<()> {
    let (from, to) = (cstr(from)?, cstr(to)?);
    // SAFETY: valid NUL-terminated names and an fd we own.
    match unsafe { libc::renameat(dir.as_raw_fd(), from.as_ptr(), dir.as_raw_fd(), to.as_ptr()) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `root` with `a/b/c` (a file) and `outside/a/b/c`, and a link to
    /// `outside`'s copy of the folder at depth `at` (0: `a`, 1: `a/b`).
    fn tree_with_link_at(at: usize) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let (root, outside) = (dir.path().join("root"), dir.path().join("outside"));
        for base in [&root, &outside] {
            std::fs::create_dir_all(base.join("a/b")).unwrap();
            std::fs::write(base.join("a/b/c"), "x").unwrap();
        }
        let linked = ["a", "a/b"][at];
        std::fs::remove_dir_all(root.join(linked)).unwrap();
        std::os::unix::fs::symlink(outside.join(linked), root.join(linked)).unwrap();
        (dir, root, outside)
    }

    #[test]
    fn a_link_at_any_folder_on_the_way_is_refused_and_nothing_outside_changes() {
        for at in 0..2 {
            let (_dir, root, outside) = tree_with_link_at(at);
            let err = remove(&root, "a/b/c").unwrap_err();
            assert!(err.to_string().contains("symlink"), "{at}: {err}");
            let err = create_new(&root, "a/b/new", 0o600).unwrap_err();
            assert!(err.to_string().contains("symlink"), "{at}: {err}");
            assert!(open_regular(&root, "a/b/c").is_err(), "{at}: read through the link");
            assert!(replace_regular(&root, "a/b/c", b"x", b"y").is_err(), "{at}: replaced through the link");
            assert_eq!(std::fs::read(outside.join("a/b/c")).unwrap(), b"x", "{at}: written outside");
            assert!(outside.join("a/b/c").exists(), "{at}: deleted outside");
            assert!(!outside.join("a/b/new").exists(), "{at}: created outside");
        }
    }

    #[test]
    fn a_replace_writes_only_the_file_it_read_and_keeps_its_bits() {
        let dir = tempfile::tempdir().unwrap();
        let (root, outside) = (dir.path().join("root"), dir.path().join("outside"));
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::write(root.join("a/run.sh"), "old").unwrap();
        std::fs::set_permissions(root.join("a/run.sh"), std::fs::Permissions::from_mode(0o766)).unwrap();
        // A leftover temp file from an edit that never finished is replaced.
        std::fs::write(root.join("a/.run.sh.darkmux-edit"), "stale").unwrap();
        replace_regular(&root, "a/run.sh", b"old", b"new").unwrap();
        assert_eq!(std::fs::read(root.join("a/run.sh")).unwrap(), b"new");
        assert_eq!(std::fs::metadata(root.join("a/run.sh")).unwrap().permissions().mode() & 0o777, 0o766);
        assert_eq!(std::fs::read_dir(root.join("a")).unwrap().count(), 1, "no temp file is left");
        // Not what was read: nothing is written.
        let err = replace_regular(&root, "a/run.sh", b"old", b"other").unwrap_err();
        assert!(err.to_string().contains("changed"), "{err}");
        assert_eq!(std::fs::read(root.join("a/run.sh")).unwrap(), b"new");
        assert_eq!(std::fs::read_dir(root.join("a")).unwrap().count(), 1);
        // A link at the last name is never written through.
        std::fs::write(&outside, "new").unwrap();
        std::fs::remove_file(root.join("a/run.sh")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("a/run.sh")).unwrap();
        let err = replace_regular(&root, "a/run.sh", b"new", b"written").unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert_eq!(std::fs::read(&outside).unwrap(), b"new");
    }

    #[test]
    fn a_link_at_the_last_name_is_unlinked_never_followed_or_read() {
        let dir = tempfile::tempdir().unwrap();
        let (root, outside) = (dir.path().join("root"), dir.path().join("outside"));
        std::fs::create_dir_all(outside.join("deps")).unwrap();
        std::fs::write(outside.join("deps/keep"), "x").unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(outside.join("deps"), root.join("deps")).unwrap();
        assert!(open_regular(&root, "deps").is_err(), "opened through the link");
        assert!(create_new(&root, "deps", 0o600).is_err(), "created through the link");
        remove(&root, "deps").unwrap();
        assert!(std::fs::symlink_metadata(root.join("deps")).is_err(), "the link is gone");
        assert!(outside.join("deps/keep").exists(), "followed the link");
    }

    /// Folders nested `depth` deep under `root/deep`, built one level at a
    /// time from descriptors (the whole path is far past `PATH_MAX`).
    fn nest(root: &Path, depth: usize) {
        std::fs::create_dir_all(root).unwrap();
        let mut at = File::open(root).unwrap();
        let name = std::ffi::CString::new("deep").unwrap();
        for _ in 0..depth {
            // SAFETY: valid fd and name.
            assert_eq!(unsafe { libc::mkdirat(at.as_raw_fd(), name.as_ptr(), 0o700) }, 0);
            // SAFETY: valid fd and name; the result is checked.
            let fd = unsafe { libc::openat(at.as_raw_fd(), name.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
            assert!(fd >= 0);
            // SAFETY: a fresh fd.
            at = unsafe { File::from_raw_fd(fd) };
        }
    }

    /// The open descriptors in this process, counted from `/dev/fd`.
    fn open_fds() -> usize {
        std::fs::read_dir("/dev/fd").unwrap().count()
    }

    /// Removes `root/deep`, `depth` folders deep, in a temporary folder
    /// that is cleaned up when the result is dropped (a tree left behind is
    /// one a recursive walker elsewhere, like the leak check's census, has
    /// to descend).
    fn remove_nested(depth: usize) -> (tempfile::TempDir, io::Result<()>) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        nest(&root, depth);
        let removed = remove(&root, "deep");
        (dir, removed)
    }

    /// A tree as deep as darkmux deletes is removed.
    ///
    /// Serial with the refusal test below: this walk holds a descriptor per
    /// level, which that test's process-wide count would read as its own.
    #[test]
    #[serial_test::serial(deep_tree_descriptors)]
    fn a_tree_as_deep_as_the_walk_takes_is_removed() {
        let (dir, removed) = remove_nested(MAX_DELETE_DEPTH);
        removed.unwrap();
        assert!(std::fs::symlink_metadata(dir.path().join("root/deep")).is_err(), "the tree is gone");
    }

    /// A run can build a tree far deeper than [`MAX_DELETE_DEPTH`] (a
    /// recursive walk's stack ran out near 2,000 levels on 2 MiB; a
    /// descriptor per level runs past a 256-file limit sooner). One deeper
    /// is refused with a reason, without holding a descriptor per level.
    ///
    /// `/dev/fd` counts the whole process, and under `cargo test` other
    /// tests run on other threads of it, opening files of their own (the
    /// release-mode gate failed here on a +2 bound). A walk that kept its
    /// frames would leave [`MAX_DELETE_DEPTH`] open, so half that is the
    /// bound: far above what a neighbor holds, far below a leak.
    #[test]
    #[serial_test::serial(deep_tree_descriptors)]
    fn a_tree_deeper_than_the_walk_takes_is_refused() {
        let before = open_fds();
        let (_dir, removed) = remove_nested(MAX_DELETE_DEPTH + 72);
        let err = removed.unwrap_err();
        assert!(err.to_string().contains("deeper than"), "{err}");
        let after = open_fds();
        assert!(after < before + MAX_DELETE_DEPTH / 2, "descriptors left open: {before} before, {after} after");
    }

    /// A path that is not plain names, a root that is not there, a NUL in a
    /// name and a missing folder on the way are each refused, touching
    /// nothing.
    #[test]
    fn paths_the_walk_cannot_take_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep"), "x").unwrap();
        for bad in ["", "../keep", "a/../keep", "/keep", "./keep"] {
            let err = remove(dir.path(), bad).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{bad:?}: {err}");
        }
        assert_eq!(remove(&dir.path().join("missing"), "keep").unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(remove(dir.path(), "ke\0ep").unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert!(create_new(dir.path(), "missing/new", 0o600).is_err(), "a missing folder on the way");
        assert!(dir.path().join("keep").exists());
    }

    /// A folder whose entries cannot be unlinked (no write permission)
    /// fails the delete with the OS's reason, rather than reporting it done.
    #[test]
    fn a_folder_that_cannot_be_emptied_fails_the_delete() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("deps/locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("f"), "x").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
        let out = remove(dir.path(), "deps");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(out.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert!(locked.join("f").exists());
    }

    /// A FIFO where the file should be is refused at once, never opened in a
    /// way that waits for a writer.
    #[test]
    fn a_fifo_at_the_source_is_refused_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let fifo = std::ffi::CString::new(root.join(".env").as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(open_regular(&root, ".env").map(|_| ()));
        });
        let out = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("opening the FIFO blocked");
        assert!(out.is_err(), "a FIFO is not a regular file");
    }

    #[test]
    fn a_real_tree_is_removed_with_its_links_unlinked_and_a_file_is_copied() {
        let dir = tempfile::tempdir().unwrap();
        let (root, outside) = (dir.path().join("root"), dir.path().join("outside"));
        std::fs::create_dir_all(root.join("deps/x/y")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep"), "x").unwrap();
        std::fs::write(root.join("deps/x/y/f"), "f").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("deps/x/out")).unwrap();
        remove(&root, "deps").unwrap();
        assert!(!root.join("deps").exists());
        assert!(outside.join("keep").exists(), "followed a link inside the tree");
        remove(&root, "deps").unwrap();
        std::fs::write(root.join("src"), "body").unwrap();
        let (mut from, mode) = open_regular(&root, "src").unwrap();
        let mut to = create_new(&root, "dst", mode).unwrap();
        std::io::copy(&mut from, &mut to).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("dst")).unwrap(), "body");
        assert!(create_new(&root, "dst", 0o600).is_err(), "never overwrites");
    }
}
