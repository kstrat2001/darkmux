//! (#3112) Confining a process on this host with the kernel's own pieces:
//! user, mount, pid and (for no network) network namespaces, and Landlock.
//! Nothing to install and no daemon. darkmux-sandbox's Landlock backend is
//! built on it; that crate says what a sandbox may see, this one does it.
//!
//! [`enter`] turns the calling process into a confined one, in three
//! processes:
//! - **The caller** unshares the namespaces, maps its own user and group to
//!   themselves, and waits for the init below, exiting as it exits. It
//!   outlives the group's SIGTERM, so a stop's grace is the sandbox's.
//! - **An init** (pid 1 of the new pid namespace) builds the sandbox's root:
//!   a read-only tmpfs holding only the granted paths, each bound at its own
//!   path (read-only ones made read-only, recursively), the granted device
//!   files, and a `/proc` of the new pid namespace, then pivots into it. It
//!   reaps orphans, and when the program exits it exits with its status,
//!   which ends every process left in the namespace, `setsid` or not.
//! - **The program** sets no-new-privileges, restricts itself with Landlock,
//!   and execs.
//!
//! The namespaces carry the filesystem view and the no-network case; Landlock
//! carries what they cannot: no signal or abstract unix socket reaches past
//! the sandbox, no outside process can be traced, a pathname unix socket
//! resolves only inside a granted tree (ABI 9), TCP binds only the granted
//! ports, and the granted paths are restricted a second time.
//!
//! [`support`] says whether this host can do it. Only Linux can; elsewhere
//! [`enter`] fails as unsupported.

use std::ffi::OsString;
use std::path::PathBuf;

/// The Landlock ABI [`enter`] needs: 6 (Linux 6.12), the first that scopes
/// signals and abstract unix sockets to the sandbox. Below it a sandbox on
/// the host's network could reach the desktop's abstract sockets, so the
/// backend is refused rather than run weaker.
pub const LANDLOCK_ABI_NEEDED: u32 = 6;

/// The Landlock ABI from which a pathname unix socket outside the granted
/// trees cannot even be resolved. Below it the mount view still hides every
/// socket outside them; this is the second line.
pub const LANDLOCK_ABI_RESOLVE_UNIX: u32 = 9;

/// One host path the confined process sees, at the same path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bind {
    pub path: PathBuf,
    pub writable: bool,
}

/// What a confined process sees and may do. Nothing outside it is granted.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Confinement {
    /// An empty directory the process's root is built on. It is mounted over
    /// only inside the process's own mount namespace; the host sees it empty.
    pub root: PathBuf,
    /// In order: a later bind at a path nested in an earlier one is layered
    /// over it (a read-only `.git` over a writable tree).
    pub binds: Vec<Bind>,
    /// Symbolic links at the top of the root (`/bin` to `usr/bin`), as path
    /// and target.
    pub links: Vec<(PathBuf, PathBuf)>,
    /// Device files it may read and write (`/dev/null`).
    pub devices: Vec<PathBuf>,
    /// Whether it shares this host's network. Otherwise it has a network of
    /// its own with only a loopback: nothing of the host's, its loopback
    /// services included, is reachable.
    pub host_network: bool,
    /// TCP ports it may bind on the host's network. Landlock grants a bind
    /// by port, at any address.
    pub bind_ports: Vec<u16>,
    /// Its working directory.
    pub workdir: Option<PathBuf>,
}

/// What this host offers [`enter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConfineSupport {
    /// The kernel's Landlock ABI; `None` when Landlock is off or absent.
    pub landlock_abi: Option<u32>,
    /// Whether an unprivileged process may make user namespaces and mount
    /// in them (some distributions forbid it).
    pub user_namespaces: bool,
}

impl ConfineSupport {
    /// Why [`enter`] cannot run here, or `None` when it can.
    pub fn refusal(&self) -> Option<String> {
        let landlock = match self.landlock_abi {
            None => Some("this kernel has no Landlock (it is off, or older than Linux 5.13)".to_string()),
            Some(abi) if abi < LANDLOCK_ABI_NEEDED => Some(format!(
                "this kernel's Landlock ABI is {abi}; the sandbox needs {LANDLOCK_ABI_NEEDED} (Linux 6.12) to keep signals and abstract unix sockets inside it"
            )),
            Some(_) => None,
        };
        let namespaces = (!self.user_namespaces).then(|| "this host does not let an unprivileged process make a user namespace and mount in it".to_string());
        match (landlock, namespaces) {
            (None, None) => None,
            (Some(a), None) | (None, Some(a)) => Some(a),
            (Some(a), Some(b)) => Some(format!("{a}; and {b}")),
        }
    }
}

/// What this host offers [`enter`]. On Linux it reads the Landlock ABI and
/// tries a user namespace in a short-lived child (about a millisecond).
pub fn support() -> ConfineSupport {
    // Probed once per process: the kernel's answer does not change, and the
    // default sandbox asks it whenever the setting is resolved.
    static SUPPORT: std::sync::OnceLock<ConfineSupport> = std::sync::OnceLock::new();
    *SUPPORT.get_or_init(imp::support)
}

/// Confine this process as `c` says and exec `argv` (the program, then its
/// arguments; the program found on `PATH` when it has no slash). Returns
/// only when it could not start; once the program runs, this process exits
/// with its status (128 plus the signal that ended it). Call it from a
/// single-threaded process: it forks.
pub fn enter(c: &Confinement, argv: &[OsString]) -> std::io::Error {
    imp::enter(c, argv)
}

/// This process's arguments as the C runtime hands them to a function run
/// before `main` (see [`before_main`](crate::confine_before_main)).
///
/// # Safety
/// `argv` holds `argc` NUL-terminated strings, as the C runtime passes them.
#[cfg(unix)]
pub unsafe fn args_from(argc: i32, argv: *const *const std::ffi::c_char) -> Vec<OsString> {
    use std::os::unix::ffi::OsStrExt;
    (0..usize::try_from(argc).unwrap_or(0))
        .filter_map(|i| {
            let arg = *argv.add(i);
            (!arg.is_null()).then(|| std::ffi::OsStr::from_bytes(std::ffi::CStr::from_ptr(arg).to_bytes()).to_os_string())
        })
        .collect()
}

/// (#3112) Run `$entry` with this process's arguments before `main` (and
/// before any thread starts), where [`enter`] runs: a test binary that
/// starts Landlock sandboxes is its own helper, since a sandbox's helper is
/// the executable that planned it. `$entry` returns when the arguments do
/// not ask for the helper. Nothing is run where there is no Landlock.
#[macro_export]
macro_rules! confine_before_main {
    ($entry:path) => {
        #[cfg(target_os = "linux")]
        #[used]
        #[link_section = ".init_array"]
        static DARKMUX_CONFINE_BEFORE_MAIN: extern "C" fn(i32, *const *const std::ffi::c_char, *const *const std::ffi::c_char) = {
            extern "C" fn before_main(argc: i32, argv: *const *const std::ffi::c_char, _env: *const *const std::ffi::c_char) {
                // SAFETY: the C runtime passes `argc` strings in `argv`.
                $entry(unsafe { $crate::confine::args_from(argc, argv) });
            }
            before_main
        };
    };
}

/// An exit status as a shell reports it: the code, or 128 plus the signal.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn exit_code(exited: Option<i32>, signaled: Option<i32>) -> i32 {
    exited.or(signaled.map(|s| 128 + s)).unwrap_or(1)
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::{Confinement, ConfineSupport};

    pub fn support() -> ConfineSupport {
        ConfineSupport::default()
    }

    pub fn enter(_c: &Confinement, _argv: &[std::ffi::OsString]) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::Unsupported, "confinement needs Linux")
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{Bind, Confinement, ConfineSupport};
    use std::ffi::{CString, OsString};
    use std::io::Error;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Component, Path, PathBuf};

    // Landlock's UAPI (linux/landlock.h), not in libc.
    const CREATE_RULESET_VERSION: u32 = 1;
    const RULE_PATH_BENEATH: libc::c_int = 1;
    const RULE_NET_PORT: libc::c_int = 2;
    const FS_EXECUTE: u64 = 1 << 0;
    const FS_WRITE_FILE: u64 = 1 << 1;
    const FS_READ_FILE: u64 = 1 << 2;
    const FS_READ_DIR: u64 = 1 << 3;
    const FS_TRUNCATE: u64 = 1 << 14;
    const FS_IOCTL_DEV: u64 = 1 << 15;
    const NET_BIND_TCP: u64 = 1 << 0;
    const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
    const SCOPE_SIGNAL: u64 = 1 << 1;
    /// The rights a rule on a file (not a directory) may carry.
    const FILE_RIGHTS: u64 = FS_EXECUTE | FS_WRITE_FILE | FS_READ_FILE | FS_TRUNCATE | FS_IOCTL_DEV;
    const READ: u64 = FS_EXECUTE | FS_READ_FILE | FS_READ_DIR;

    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
        scoped: u64,
    }

    #[repr(C, packed)]
    struct PathBeneath {
        allowed_access: u64,
        parent_fd: i32,
    }

    #[repr(C)]
    struct NetPort {
        allowed_access: u64,
        port: u64,
    }

    #[repr(C)]
    struct MountAttr {
        attr_set: u64,
        attr_clr: u64,
        propagation: u64,
        userns_fd: u64,
    }

    /// Every filesystem right this ABI knows: each ABI adds one or two.
    fn fs_rights(abi: u32) -> u64 {
        let mut rights = (1 << 13) - 1;
        for (since, bit) in [(2, 13), (3, 14), (5, 15), (super::LANDLOCK_ABI_RESOLVE_UNIX, 16)] {
            if abi >= since {
                rights |= 1 << bit;
            }
        }
        rights
    }

    fn landlock_abi() -> Option<u32> {
        // SAFETY: a null attr with size 0 and the version flag only asks.
        let abi = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<u8>(), 0usize, CREATE_RULESET_VERSION) };
        (abi > 0).then_some(abi as u32)
    }

    fn cstr(path: &Path) -> Result<CString, Error> {
        CString::new(path.as_os_str().as_bytes()).map_err(|_| Error::other(format!("{} holds a NUL", path.display())))
    }

    fn check(rc: libc::c_long, what: &str) -> Result<(), Error> {
        if rc < 0 {
            let e = Error::last_os_error();
            return Err(Error::new(e.kind(), format!("{what}: {e}")));
        }
        Ok(())
    }

    fn write_file(path: &str, text: &str) -> Result<(), Error> {
        std::fs::write(path, text).map_err(|e| Error::new(e.kind(), format!("writing {path}: {e}")))
    }

    /// Make the user, mount and pid namespaces (and the network one, unless
    /// shared) and map this user and group to themselves in them.
    fn unshare(host_network: bool) -> Result<(), Error> {
        // SAFETY: plain syscalls on this process.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let mut flags = libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWPID;
        if !host_network {
            flags |= libc::CLONE_NEWNET;
        }
        check(unsafe { libc::unshare(flags) } as libc::c_long, "unshare")?;
        write_file("/proc/self/setgroups", "deny")?;
        write_file("/proc/self/uid_map", &format!("{uid} {uid} 1"))?;
        write_file("/proc/self/gid_map", &format!("{gid} {gid} 1"))
    }

    pub fn support() -> ConfineSupport {
        // A child tries what `enter` needs first: the namespaces, and a
        // mount in them. Only async-signal-safe calls run after the fork.
        // SAFETY: the child calls only unshare, open, write, mount, _exit.
        let user_namespaces = unsafe {
            let (uid, gid) = (libc::getuid(), libc::getgid());
            let uid_map = CString::new(format!("{uid} {uid} 1")).unwrap();
            let gid_map = CString::new(format!("{gid} {gid} 1")).unwrap();
            let child = libc::fork();
            if child == 0 {
                let write = |path: &[u8], text: &CString| {
                    let fd = libc::open(path.as_ptr().cast(), libc::O_WRONLY);
                    fd >= 0 && libc::write(fd, text.as_ptr().cast(), text.as_bytes().len()) >= 0
                };
                let ok = libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWPID | libc::CLONE_NEWNET) == 0
                    && write(b"/proc/self/setgroups\0", &CString::from(c"deny"))
                    && write(b"/proc/self/uid_map\0", &uid_map)
                    && write(b"/proc/self/gid_map\0", &gid_map)
                    && libc::mount(std::ptr::null(), c"/".as_ptr(), std::ptr::null(), libc::MS_REC | libc::MS_PRIVATE, std::ptr::null()) == 0;
                libc::_exit(if ok { 0 } else { 1 });
            }
            let mut status = 0;
            child > 0 && libc::waitpid(child, &mut status, 0) == child && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
        };
        ConfineSupport { landlock_abi: landlock_abi(), user_namespaces }
    }

    extern "C" fn ignore(_: libc::c_int) {}

    /// Reap children until `pid` exits, and the status to exit with as it
    /// did. Any other child reaped on the way (as pid 1 must) is dropped.
    fn wait_code(pid: libc::pid_t) -> i32 {
        let mut status = 0;
        loop {
            // SAFETY: waiting on our own children.
            let r = unsafe { libc::waitpid(-1, &mut status, 0) };
            if r == pid {
                break;
            }
            if r < 0 && Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                return 1;
            }
        }
        let exited = libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status));
        let signaled = libc::WIFSIGNALED(status).then(|| libc::WTERMSIG(status));
        super::exit_code(exited, signaled)
    }

    pub fn enter(c: &Confinement, argv: &[OsString]) -> Error {
        match run(c, argv) {
            Ok(never) => match never {},
            Err(e) => e,
        }
    }

    fn run(c: &Confinement, argv: &[OsString]) -> Result<std::convert::Infallible, Error> {
        let abi = landlock_abi().filter(|a| *a >= super::LANDLOCK_ABI_NEEDED).ok_or_else(|| Error::other("Landlock ABI 6 or later is needed"))?;
        let argv: Vec<CString> = argv.iter().map(|a| CString::new(a.as_bytes())).collect::<Result<_, _>>().map_err(|_| Error::other("an argument holds a NUL"))?;
        if argv.is_empty() {
            return Err(Error::other("no program to run"));
        }
        unshare(c.host_network)?;
        // SAFETY: this process is single-threaded (the caller's contract), so
        // the children may allocate; each exits through _exit or exec.
        unsafe {
            for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
                libc::signal(sig, ignore as *const () as libc::sighandler_t);
            }
            let init = libc::fork();
            check(init as libc::c_long, "fork")?;
            if init == 0 {
                // Ends with this process. (Its parent is outside its pid
                // namespace, so `getppid` reads 0 and cannot check this.)
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                libc::_exit(as_init(c, abi, &argv));
            }
            std::process::exit(wait_code(init))
        }
    }

    /// Pid 1 of the sandbox: build its root, start the program, reap until
    /// the program exits, and exit as it did.
    fn as_init(c: &Confinement, abi: u32, argv: &[CString]) -> i32 {
        if let Err(e) = build_root(c) {
            eprintln!("landlock: building the sandbox's root: {e}");
            return 125;
        }
        if !c.host_network {
            if let Err(e) = loopback_up() {
                eprintln!("landlock: {e}");
                return 125;
            }
        }
        // SAFETY: still single-threaded; the child execs or _exits.
        unsafe {
            let program = libc::fork();
            if program < 0 {
                return 125;
            }
            if program == 0 {
                for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
                    libc::signal(sig, libc::SIG_DFL);
                }
                libc::_exit(as_program(c, abi, argv));
            }
            wait_code(program)
        }
    }

    /// The program: its working directory, no new privileges, Landlock,
    /// then exec. Returns only the status to exit with when it cannot.
    fn as_program(c: &Confinement, abi: u32, argv: &[CString]) -> i32 {
        if let Some(dir) = &c.workdir {
            if let Err(e) = std::env::set_current_dir(dir) {
                eprintln!("landlock: working directory {}: {e}", dir.display());
                return 125;
            }
        }
        // SAFETY: a flag on this process.
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            eprintln!("landlock: no_new_privs: {}", Error::last_os_error());
            return 125;
        }
        if let Err(e) = restrict(c, abi) {
            eprintln!("landlock: {e}");
            return 125;
        }
        let mut ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        // SAFETY: a NUL-terminated argv of live CStrings.
        unsafe { libc::execvp(ptrs[0], ptrs.as_ptr()) };
        let e = Error::last_os_error();
        eprintln!("landlock: {}: {e}", argv[0].to_string_lossy());
        if e.raw_os_error() == Some(libc::ENOENT) {
            127
        } else {
            126
        }
    }

    /// `path` inside `root`.
    fn under(root: &Path, path: &Path) -> PathBuf {
        root.join(path.components().filter(|c| matches!(c, Component::Normal(_))).collect::<PathBuf>())
    }

    fn mount(source: Option<&Path>, target: &Path, fstype: Option<&str>, flags: libc::c_ulong, data: Option<&str>) -> Result<(), Error> {
        let source = source.map(cstr).transpose()?;
        let target_c = cstr(target)?;
        let fstype = fstype.map(|f| CString::new(f).unwrap());
        let data = data.map(|d| CString::new(d).unwrap());
        let ptr = |c: &Option<CString>| c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
        // SAFETY: NUL-terminated strings or null, as mount(2) takes them.
        let rc = unsafe { libc::mount(ptr(&source), target_c.as_ptr(), ptr(&fstype), flags, ptr(&data).cast()) };
        check(rc as libc::c_long, &format!("mounting on {}", target.display()))
    }

    /// Set `attrs` on the mount at `target` and, with `recursive`, every
    /// mount beneath it.
    fn set_attrs(target: &Path, attrs: u64, recursive: bool) -> Result<(), Error> {
        let target_c = cstr(target)?;
        let attr = MountAttr { attr_set: attrs, attr_clr: 0, propagation: 0, userns_fd: 0 };
        let flags = if recursive { libc::AT_RECURSIVE as libc::c_uint } else { 0 };
        // SAFETY: a live mount_attr of its declared size.
        let rc = unsafe { libc::syscall(libc::SYS_mount_setattr, libc::AT_FDCWD, target_c.as_ptr(), flags, &attr, std::mem::size_of::<MountAttr>()) };
        check(rc, &format!("setting {} read-only or nosuid", target.display()))
    }

    /// An empty directory or file at `target` to bind `source` over, of the
    /// same kind; existing ones are kept.
    fn mount_point(source: &Path, target: &Path) -> Result<(), Error> {
        let is_dir = std::fs::metadata(source).map_err(|e| Error::new(e.kind(), format!("{}: {e}", source.display())))?.is_dir();
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if is_dir {
            std::fs::create_dir_all(target)
        } else if !target.exists() {
            std::fs::File::create(target).map(drop)
        } else {
            Ok(())
        }
    }

    fn bind(source: &Path, target: &Path, attrs: u64) -> Result<(), Error> {
        mount_point(source, target)?;
        mount(Some(source), target, None, libc::MS_BIND | libc::MS_REC, None)?;
        set_attrs(target, attrs, true)
    }

    /// The sandbox's root: a tmpfs holding the granted paths, devices and a
    /// fresh `/proc`, read-only, pivoted into.
    fn build_root(c: &Confinement) -> Result<(), Error> {
        let root = &c.root;
        mount(None, Path::new("/"), None, libc::MS_REC | libc::MS_PRIVATE, None)?;
        mount(None, root, Some("tmpfs"), libc::MS_NOSUID | libc::MS_NODEV, Some("mode=0755,size=4m"))?;
        grant_paths(c)?;
        kernel_trees(root)?;
        set_attrs(root, libc::MOUNT_ATTR_RDONLY | libc::MOUNT_ATTR_NOSUID | libc::MOUNT_ATTR_NODEV, false)?;
        pivot_into(root)
    }

    /// The granted links, paths and devices, each at its own path in `root`.
    fn grant_paths(c: &Confinement) -> Result<(), Error> {
        let root = &c.root;
        for (path, target) in &c.links {
            std::os::unix::fs::symlink(target, under(root, path))?;
        }
        for Bind { path, writable } in &c.binds {
            let ro = if *writable { 0 } else { libc::MOUNT_ATTR_RDONLY };
            bind(path, &under(root, path), libc::MOUNT_ATTR_NOSUID | libc::MOUNT_ATTR_NODEV | ro)?;
        }
        for device in &c.devices {
            bind(device, &under(root, device), libc::MOUNT_ATTR_NOSUID)?;
        }
        Ok(())
    }

    /// `/dev`'s standard links, an empty `/tmp` and a fresh `/proc`.
    fn kernel_trees(root: &Path) -> Result<(), Error> {
        let dev = root.join("dev");
        std::fs::create_dir_all(&dev)?;
        for (name, target) in [("fd", "/proc/self/fd"), ("stdin", "/proc/self/fd/0"), ("stdout", "/proc/self/fd/1"), ("stderr", "/proc/self/fd/2")] {
            std::os::unix::fs::symlink(target, dev.join(name))?;
        }
        std::fs::create_dir_all(root.join("tmp"))?;
        let proc = root.join("proc");
        std::fs::create_dir_all(&proc)?;
        mount(Some(Path::new("proc")), &proc, Some("proc"), libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, None)
    }

    /// Make `root` this process's `/`, the old root detached.
    fn pivot_into(root: &Path) -> Result<(), Error> {
        std::env::set_current_dir(root)?;
        let dot = c".";
        // SAFETY: pivot_root(".", ".") stacks the old root over the new one,
        // and the detach drops it, as pivot_root(2) describes.
        unsafe {
            check(libc::syscall(libc::SYS_pivot_root, dot.as_ptr(), dot.as_ptr()), "pivot_root")?;
            check(libc::umount2(dot.as_ptr(), libc::MNT_DETACH) as libc::c_long, "detaching the old root")?;
        }
        std::env::set_current_dir("/")
    }

    /// Bring the new network namespace's loopback up, as a container's is.
    fn loopback_up() -> Result<(), Error> {
        // SAFETY: an ioctl on a socket this function owns, with a zeroed ifreq.
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
            check(fd as libc::c_long, "a socket for the loopback")?;
            let mut req: libc::ifreq = std::mem::zeroed();
            for (i, b) in b"lo".iter().enumerate() {
                req.ifr_name[i] = *b as libc::c_char;
            }
            let got = libc::ioctl(fd, libc::SIOCGIFFLAGS, &mut req);
            req.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
            let set = if got == 0 { libc::ioctl(fd, libc::SIOCSIFFLAGS, &req) } else { got };
            let e = Error::last_os_error();
            libc::close(fd);
            if set != 0 {
                return Err(Error::new(e.kind(), format!("bringing the loopback up: {e}")));
            }
        }
        Ok(())
    }

    /// Restrict this process with Landlock: every right this ABI handles is
    /// denied but on the granted paths, TCP binds (on the host's network)
    /// but on the granted ports, and signals and abstract unix sockets stay
    /// inside the sandbox.
    fn restrict(c: &Confinement, abi: u32) -> Result<(), Error> {
        let handled = fs_rights(abi);
        let attr = RulesetAttr {
            handled_access_fs: handled,
            handled_access_net: if c.host_network { NET_BIND_TCP } else { 0 },
            scoped: SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL,
        };
        // SAFETY: a live attr of its declared size.
        let fd = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &attr, std::mem::size_of::<RulesetAttr>(), 0u32) };
        check(fd, "creating the Landlock ruleset")?;
        let fd = fd as libc::c_int;
        let mut grants: Vec<(&Path, u64)> = vec![(Path::new("/"), FS_READ_DIR), (Path::new("/proc"), FS_READ_FILE | FS_READ_DIR)];
        grants.extend(c.binds.iter().map(|b| (b.path.as_path(), if b.writable { handled } else { READ })));
        grants.extend(c.devices.iter().map(|d| (d.as_path(), FS_READ_FILE | FS_WRITE_FILE | FS_IOCTL_DEV)));
        let result = (|| {
            for (path, rights) in grants {
                add_path_rule(fd, path, rights & handled)?;
            }
            if c.host_network {
                for port in &c.bind_ports {
                    let rule = NetPort { allowed_access: NET_BIND_TCP, port: u64::from(*port) };
                    // SAFETY: a live rule of the type named.
                    check(unsafe { libc::syscall(libc::SYS_landlock_add_rule, fd, RULE_NET_PORT, &rule, 0u32) }, &format!("granting port {port}"))?;
                }
            }
            // SAFETY: a ruleset fd this function made.
            check(unsafe { libc::syscall(libc::SYS_landlock_restrict_self, fd, 0u32) }, "applying the Landlock ruleset")
        })();
        // SAFETY: closing the fd made above.
        unsafe { libc::close(fd) };
        result
    }

    fn add_path_rule(ruleset: libc::c_int, path: &Path, rights: u64) -> Result<(), Error> {
        let path_c = cstr(path)?;
        // SAFETY: an O_PATH open of a NUL-terminated path.
        let fd = unsafe { libc::open(path_c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        check(fd as libc::c_long, &format!("opening {}", path.display()))?;
        let is_dir = std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false);
        let rule = PathBeneath { allowed_access: if is_dir { rights } else { rights & FILE_RIGHTS }, parent_fd: fd };
        // SAFETY: a live rule of the type named, then closing its fd.
        let rc = unsafe { libc::syscall(libc::SYS_landlock_add_rule, ruleset, RULE_PATH_BENEATH, &rule, 0u32) };
        unsafe { libc::close(fd) };
        check(rc, &format!("granting {}", path.display()))
    }

    #[cfg(test)]
    mod tests {
        /// Each ABI handles the rights it knows and none it does not: a
        /// ruleset naming an unknown right is refused by the kernel.
        #[test]
        fn the_rights_handled_follow_the_abi() {
            assert_eq!(super::fs_rights(1), (1 << 13) - 1);
            assert_eq!(super::fs_rights(6), (1 << 16) - 1);
            assert_eq!(super::fs_rights(9), (1 << 17) - 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each missing piece is named; a host with both runs.
    #[test]
    fn a_refusal_names_what_is_missing() {
        let ok = ConfineSupport { landlock_abi: Some(LANDLOCK_ABI_NEEDED), user_namespaces: true };
        assert_eq!(ok.refusal(), None);
        let old = ConfineSupport { landlock_abi: Some(4), ..ok };
        assert!(old.refusal().unwrap().contains("ABI is 4"), "{:?}", old.refusal());
        let none = ConfineSupport { landlock_abi: None, user_namespaces: false };
        let why = none.refusal().unwrap();
        assert!(why.contains("no Landlock") && why.contains("user namespace"), "{why}");
    }

    #[test]
    fn an_exit_status_reads_as_a_shell_reports_it() {
        assert_eq!(exit_code(Some(3), None), 3);
        assert_eq!(exit_code(None, Some(9)), 137);
    }
}
