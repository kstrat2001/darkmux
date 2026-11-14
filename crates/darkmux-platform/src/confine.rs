//! (#3112) Confining a process on this host with the kernel's own pieces:
//! user, mount, pid and (for no network) network namespaces, and Landlock.
//! Nothing to install and no daemon. darkmux-sandbox's Landlock backend is
//! built on it; that crate says what a sandbox may see, this one does it.
//!
//! [`enter`] turns the calling process into a confined one, in three
//! processes:
//! - **The caller** unshares the namespaces, maps its own user and group to
//!   themselves, and waits for the init below, exiting as it exits. It
//!   outlives the group's SIGTERM, so a stop's grace is the sandbox's, but
//!   not its own parent: when that ends, killed outright included, so does
//!   it, and the init with it.
//! - **An init** (pid 1 of the new pid namespace) builds the sandbox's root:
//!   a read-only tmpfs holding only the granted paths, each bound at its own
//!   path (read-only ones made read-only, recursively), the granted device
//!   files, and a `/proc` of the new pid namespace, then pivots into it. It
//!   reaps orphans, passes a SIGTERM, SIGINT or SIGHUP on to the program's
//!   process group, and when the program exits it exits with its status,
//!   which ends every process left in the namespace, `setsid` or not.
//! - **The program** starts a session of its own (no controlling terminal,
//!   so a sandbox started from a terminal cannot type into it), sets
//!   no-new-privileges, restricts itself with Landlock and a syscall filter,
//!   resets every signal to its default, unblocked, and execs.
//!
//! The namespaces carry the filesystem view and the no-network case; Landlock
//! carries what they cannot: no signal or abstract unix socket reaches past
//! the sandbox, no outside process can be traced, a pathname unix socket
//! resolves only inside a granted tree (ABI 9), TCP binds only the granted
//! ports, and no path outside the granted ones is reachable. Inside the
//! granted ones it is the mounts that hold a read-only path, since Landlock's
//! rules add up: a read-only path nested in a writable one is writable to
//! Landlock and read-only as a mount.
//!
//! The syscall filter refuses what a sandbox never needs and Docker's default
//! seccomp profile refuses too: a namespace of its own (`unshare`, `setns`,
//! `clone` with a namespace flag; a new user namespace would hand it every
//! capability there, and with a network namespace the kernel's packet
//! filtering), the kernel's key store (`keyctl`, `add_key`, `request_key`),
//! `bpf`, `perf_event_open` and `userfaultfd`, each with EPERM. `clone3`
//! answers ENOSYS, so a C library falls back to `clone`. A call made under
//! another architecture's numbers ends the process: a 32-bit program (arm32
//! on aarch64, i386 on x86_64) cannot run in a sandbox, where Docker's
//! profile allows it; x86_64's x32 calls answer EPERM.
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
    /// Whether the kernel is Linux, the only one with Landlock.
    pub linux: bool,
    /// The kernel's Landlock ABI; `None` when Landlock is off or absent.
    pub landlock_abi: Option<u32>,
    /// Whether an unprivileged process may make user namespaces and mount
    /// in them (some distributions forbid it).
    pub user_namespaces: bool,
    /// Whether, in them, it can build a sandbox's root: a tmpfs, a `/proc`
    /// of its own pid namespace, `mount_setattr` and `pivot_root`. An
    /// unprivileged container (LXC, Incus) forbids the `/proc` mount.
    pub sandbox_root: bool,
    /// Whether darkmux has a syscall filter for this CPU architecture.
    pub syscall_filter: bool,
}

impl ConfineSupport {
    /// Why [`enter`] cannot run here, or `None` when it can.
    pub fn refusal(&self) -> Option<String> {
        if !self.linux {
            return Some("Landlock is part of the Linux kernel, and this host is not running Linux".to_string());
        }
        let landlock = match self.landlock_abi {
            None => Some("this kernel has no Landlock (it is off, or older than Linux 5.13)".to_string()),
            Some(abi) if abi < LANDLOCK_ABI_NEEDED => Some(format!(
                "this kernel's Landlock ABI is {abi}; the sandbox needs {LANDLOCK_ABI_NEEDED} (Linux 6.12) to keep signals and abstract unix sockets inside it"
            )),
            Some(_) => None,
        };
        let namespaces = if !self.user_namespaces {
            Some("this host does not let an unprivileged process make a user namespace and mount in it".to_string())
        } else if !self.sandbox_root {
            Some(
                "this host lets an unprivileged process make a user namespace but it cannot build a sandbox's root there \
                 (a tmpfs, a /proc of its own, mount_setattr, pivot_root; an unprivileged container, for one, forbids the /proc mount)"
                    .to_string(),
            )
        } else {
            None
        };
        let filter = (!self.syscall_filter).then(|| "darkmux has no syscall filter for this CPU architecture (it has aarch64's and x86_64's)".to_string());
        let missing: Vec<String> = [landlock, namespaces, filter].into_iter().flatten().collect();
        (!missing.is_empty()).then(|| missing.join("; and "))
    }
}

/// What this host offers [`enter`]. On Linux it reads the Landlock ABI and
/// tries what [`enter`] does in two short-lived children (the namespaces,
/// then a throwaway root built as pid 1 of the new pid namespace), about a
/// millisecond in all (median 0.9 ms on an M2).
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

    /// The namespaces a sandbox gets: user, mount and pid, and a network
    /// of its own unless it shares the host's.
    fn namespace_flags(host_network: bool) -> libc::c_int {
        let flags = libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWPID;
        if host_network {
            flags
        } else {
            flags | libc::CLONE_NEWNET
        }
    }

    /// `uid_map`'s and `gid_map`'s text mapping this user and group to
    /// themselves.
    fn self_maps(uid: libc::uid_t, gid: libc::gid_t) -> (String, String) {
        (format!("{uid} {uid} 1"), format!("{gid} {gid} 1"))
    }

    /// Make the user, mount and pid namespaces (and the network one, unless
    /// shared) and map this user and group to themselves in them.
    fn unshare(host_network: bool) -> Result<(), Error> {
        // SAFETY: plain syscalls on this process.
        let (uid_map, gid_map) = unsafe { self_maps(libc::getuid(), libc::getgid()) };
        check(unsafe { libc::unshare(namespace_flags(host_network)) } as libc::c_long, "unshare")
            .and_then(|()| write_file("/proc/self/setgroups", "deny"))
            .and_then(|()| write_file("/proc/self/uid_map", &uid_map))
            .and_then(|()| write_file("/proc/self/gid_map", &gid_map))
    }

    pub fn support() -> ConfineSupport {
        // SAFETY: getuid and getgid take nothing.
        let (uid_map, gid_map) = unsafe { self_maps(libc::getuid(), libc::getgid()) };
        // Everything the probe writes is made before it forks: after the
        // fork only async-signal-safe calls run.
        let uid_map = CString::new(uid_map).unwrap();
        let gid_map = CString::new(gid_map).unwrap();
        // SAFETY: the probe's children call only async-signal-safe functions.
        let probed = unsafe { probe(&uid_map, &gid_map) };
        ConfineSupport {
            linux: true,
            landlock_abi: landlock_abi(),
            user_namespaces: probed != PROBE_NO_NAMESPACES,
            sandbox_root: probed == 0,
            syscall_filter: AUDIT_ARCH.is_some(),
        }
    }

    /// The probe's exit when it cannot make the namespaces, and when it can
    /// but cannot build a root in them.
    const PROBE_NO_NAMESPACES: i32 = 1;
    const PROBE_NO_ROOT: i32 = 2;

    /// Fork, the child running `child` and exiting with what it returns:
    /// the child's pid, or a negative value when the fork failed. Nothing
    /// here allocates, so `child` may be a forked child's async-signal-safe
    /// code.
    unsafe fn forked(child: impl FnOnce() -> i32) -> libc::pid_t {
        let pid = libc::fork();
        if pid == 0 {
            libc::_exit(child());
        }
        pid
    }

    /// [`forked`], a failed fork an error.
    unsafe fn fork_checked(child: impl FnOnce() -> i32) -> Result<libc::pid_t, Error> {
        let pid = forked(child);
        check(pid as libc::c_long, "fork").map(|()| pid)
    }

    /// Try what [`enter`] needs in a short-lived child: the namespaces, then,
    /// as pid 1 of the new pid namespace, what `build_root` does. Returns 0
    /// when all of it worked.
    unsafe fn probe(uid_map: &std::ffi::CStr, gid_map: &std::ffi::CStr) -> i32 {
        exit_of(forked(|| probe_namespaces(uid_map, gid_map))).unwrap_or(PROBE_NO_NAMESPACES)
    }

    /// A child's exit code, when it exited.
    unsafe fn exit_of(child: libc::pid_t) -> Option<i32> {
        let mut status = 0;
        (child > 0 && libc::waitpid(child, &mut status, 0) == child && libc::WIFEXITED(status)).then(|| libc::WEXITSTATUS(status))
    }

    unsafe fn probe_namespaces(uid_map: &std::ffi::CStr, gid_map: &std::ffi::CStr) -> i32 {
        probe_status(make_namespaces(uid_map, gid_map), || exit_of(forked(|| probe_root_status())))
    }

    /// The probe's exit: [`PROBE_NO_NAMESPACES`] when the namespaces were
    /// not made, else 0 when `root` (a child building a root in them, asked
    /// only then) exited 0, else [`PROBE_NO_ROOT`].
    fn probe_status(namespaces: bool, root: impl FnOnce() -> Option<i32>) -> i32 {
        if !namespaces {
            PROBE_NO_NAMESPACES
        } else if root() == Some(0) {
            0
        } else {
            PROBE_NO_ROOT
        }
    }

    unsafe fn probe_root_status() -> i32 {
        if probe_root() {
            0
        } else {
            PROBE_NO_ROOT
        }
    }

    /// Whether every step succeeded, run in order and stopping at the first
    /// that fails (each one async-signal-safe, as the probe's are).
    fn in_order(steps: &[&dyn Fn() -> bool]) -> bool {
        steps.iter().all(|step| step())
    }

    /// What [`unshare`] does, in async-signal-safe calls: the namespaces,
    /// this user mapped to itself, and mounts private to them.
    unsafe fn make_namespaces(uid_map: &std::ffi::CStr, gid_map: &std::ffi::CStr) -> bool {
        in_order(&[
            &|| libc::unshare(namespace_flags(false)) == 0,
            &|| map_to_self(uid_map, gid_map),
            &|| libc::mount(std::ptr::null(), ROOT.as_ptr(), std::ptr::null(), libc::MS_REC | libc::MS_PRIVATE, std::ptr::null()) == 0,
        ])
    }

    const ROOT: &std::ffi::CStr = c"/";
    const SETGROUPS: &std::ffi::CStr = c"/proc/self/setgroups";
    const DENY: &std::ffi::CStr = c"deny";
    const UID_MAP: &std::ffi::CStr = c"/proc/self/uid_map";
    const GID_MAP: &std::ffi::CStr = c"/proc/self/gid_map";

    /// This user and group mapped to themselves in the new user namespace.
    unsafe fn map_to_self(uid_map: &std::ffi::CStr, gid_map: &std::ffi::CStr) -> bool {
        [(SETGROUPS, DENY), (UID_MAP, uid_map), (GID_MAP, gid_map)]
            .iter()
            .all(|(path, text)| write_text(path, text))
    }

    /// `text` written to the file at `path`, in async-signal-safe calls.
    unsafe fn write_text(path: &std::ffi::CStr, text: &std::ffi::CStr) -> bool {
        let fd = libc::open(path.as_ptr(), libc::O_WRONLY);
        fd >= 0 && libc::write(fd, text.as_ptr().cast(), text.to_bytes().len()) >= 0
    }

    /// What `build_root` needs, on a tmpfs over `/tmp` that only this mount
    /// namespace sees: a tmpfs, a `/proc` of this pid namespace,
    /// `mount_setattr` and `pivot_root`.
    unsafe fn probe_root() -> bool {
        let read_only = MountAttr { attr_set: libc::MOUNT_ATTR_RDONLY, attr_clr: 0, propagation: 0, userns_fd: 0 };
        in_order(&[
            &|| libc::mount(c"none".as_ptr(), c"/tmp".as_ptr(), c"tmpfs".as_ptr(), libc::MS_NOSUID | libc::MS_NODEV, c"mode=0755,size=64k".as_ptr().cast()) == 0,
            &|| libc::mkdir(c"/tmp/proc".as_ptr(), 0o755) == 0,
            &|| libc::mount(c"proc".as_ptr(), c"/tmp/proc".as_ptr(), c"proc".as_ptr(), libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, std::ptr::null()) == 0,
            &|| libc::syscall(libc::SYS_mount_setattr, libc::AT_FDCWD, c"/tmp".as_ptr(), 0, &read_only, std::mem::size_of::<MountAttr>()) == 0,
            &|| libc::chdir(c"/tmp".as_ptr()) == 0,
            &|| libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".".as_ptr()) == 0,
        ])
    }

    extern "C" fn ignore(_: libc::c_int) {}

    /// Reap children until `pid` exits, and the status to exit with as it
    /// did. Any other child reaped on the way (as pid 1 must) is dropped.
    fn wait_code(pid: libc::pid_t) -> i32 {
        reaped_code(pid, |status| {
            // SAFETY: waiting on our own children.
            let r = unsafe { libc::waitpid(-1, status, 0) };
            (r >= 0).then_some(r).ok_or_else(|| Error::last_os_error().raw_os_error())
        })
    }

    /// [`wait_code`] with its reaping given: `reap` fills a wait status and
    /// returns the child it reaped, or the errno it failed with. A failure
    /// other than EINTR ends the wait with status 1.
    fn reaped_code(pid: libc::pid_t, mut reap: impl FnMut(&mut i32) -> Result<libc::pid_t, Option<i32>>) -> i32 {
        let mut status = 0;
        loop {
            match reap(&mut status) {
                Ok(r) if r == pid => break,
                Err(errno) if errno != Some(libc::EINTR) => return 1,
                _ => {}
            }
        }
        let exited = libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status));
        let signaled = libc::WIFSIGNALED(status).then(|| libc::WTERMSIG(status));
        super::exit_code(exited, signaled)
    }

    pub fn enter(c: &Confinement, argv: &[OsString]) -> Error {
        let Err(e) = run(c, argv);
        e
    }

    fn run(c: &Confinement, argv: &[OsString]) -> Result<std::convert::Infallible, Error> {
        let (abi, argv) = ready(landlock_abi(), argv)?;
        ends_with_its_parent().and_then(|()| unshare(c.host_network)).and_then(|()| become_init(c, abi, &argv))
    }

    /// The Landlock ABI to confine with and the program's argv as C strings,
    /// or why it cannot start: an ABI older than needed, an argument that
    /// holds a NUL, or no program at all.
    fn ready(abi: Option<u32>, argv: &[OsString]) -> Result<(u32, Vec<CString>), Error> {
        let abi = abi.filter(|a| *a >= super::LANDLOCK_ABI_NEEDED).ok_or_else(|| Error::other("Landlock ABI 6 or later is needed"))?;
        let argv: Vec<CString> = argv.iter().map(|a| CString::new(a.as_bytes())).collect::<Result<_, _>>().map_err(|_| Error::other("an argument holds a NUL"))?;
        if argv.is_empty() {
            return Err(Error::other("no program to run"));
        }
        Ok((abi, argv))
    }

    /// Start the sandbox's init, pid 1 of the new pid namespace, and exit as
    /// it does. The stop signals are ignored here: the init passes them on.
    fn become_init(c: &Confinement, abi: u32, argv: &[CString]) -> Result<std::convert::Infallible, Error> {
        // SAFETY: this process is single-threaded (the caller's contract), so
        // the child may allocate; it exits through _exit.
        unsafe {
            set_handlers(&STOP_SIGNALS, ignore as *const () as libc::sighandler_t);
            fork_checked(|| {
                // Ends with this process. (Its parent is outside its pid
                // namespace, so `getppid` reads 0 and cannot check this.)
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                as_init(c, abi, argv)
            })
        }
        .map(|init| std::process::exit(wait_code(init)))
    }

    /// `handler` for each of `signals`.
    unsafe fn set_handlers(signals: &[libc::c_int], handler: libc::sighandler_t) {
        for sig in signals {
            libc::signal(*sig, handler);
        }
    }

    /// Ends this process when its parent (the launcher that records the
    /// sandbox) ends, killed outright included, so a sandbox whose launcher
    /// is gone, and which `remove` can no longer find by its pid, does not
    /// run on. A parent already gone before the request ends it now.
    fn ends_with_its_parent() -> Result<(), Error> {
        // SAFETY: getppid and prctl take no pointers.
        let parent = unsafe { libc::getppid() };
        check(unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } as libc::c_long, "prctl").map(|()| exit_if_orphaned(parent))
    }

    /// Exit 137 now when this process's parent is no longer `parent`.
    fn exit_if_orphaned(parent: libc::pid_t) {
        // SAFETY: getppid takes nothing.
        if unsafe { libc::getppid() } != parent {
            std::process::exit(137);
        }
    }

    /// What a step of starting the sandbox that failed prints, and the
    /// status the sandbox exits with for it.
    fn failed(e: Error) -> i32 {
        eprintln!("landlock: {e}");
        125
    }

    /// Pid 1 of the sandbox: build its root, start the program, reap until
    /// the program exits, and exit as it did.
    fn as_init(c: &Confinement, abi: u32, argv: &[CString]) -> i32 {
        build_root(c)
            .map_err(|e| Error::new(e.kind(), format!("building the sandbox's root: {e}")))
            .and_then(|()| own_loopback_up(c))
            // SAFETY: still single-threaded; the child execs or _exits.
            .and_then(|()| unsafe { start_program(c, abi, argv) })
            .unwrap_or_else(failed)
    }

    /// A network of the sandbox's own has its loopback brought up; the
    /// host's is left as it is.
    fn own_loopback_up(c: &Confinement) -> Result<(), Error> {
        if c.host_network {
            Ok(())
        } else {
            loopback_up()
        }
    }

    /// Fork the program and wait for it, as the init. The stop signals are
    /// blocked across the fork, so one that arrives before the program's pid
    /// is known is passed on once it is.
    unsafe fn start_program(c: &Confinement, abi: u32, argv: &[CString]) -> Result<i32, Error> {
        let before = block(&STOP_SIGNALS);
        fork_checked(|| as_program(c, abi, argv)).map(|program| {
            PROGRAM.store(program, std::sync::atomic::Ordering::SeqCst);
            set_handlers(&STOP_SIGNALS, pass_on as *const () as libc::sighandler_t);
            libc::sigprocmask(libc::SIG_SETMASK, &before, std::ptr::null_mut());
            wait_code(program)
        })
    }

    /// Block `signals`, returning the mask before.
    unsafe fn block(signals: &[libc::c_int]) -> libc::sigset_t {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for sig in signals {
            libc::sigaddset(&mut set, *sig);
        }
        let mut before: libc::sigset_t = std::mem::zeroed();
        libc::sigprocmask(libc::SIG_BLOCK, &set, &mut before);
        before
    }

    /// The signals a stop sends the sandbox's process group, which the init
    /// passes on to the program's own.
    const STOP_SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

    /// The program's pid, for [`pass_on`].
    static PROGRAM: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

    /// The init's stop-signal handler: the program leads a session of its
    /// own, so a stop's signal to the sandbox's group reaches it only
    /// through here; to the program alone while its session is not made yet.
    extern "C" fn pass_on(sig: libc::c_int) {
        let program = PROGRAM.load(std::sync::atomic::Ordering::SeqCst);
        if program > 0 {
            signal_group_or_process(program, sig);
        }
    }

    /// `sig` to the process group `pid` leads, or to `pid` alone when it
    /// leads none (yet). Async-signal-safe.
    fn signal_group_or_process(pid: libc::pid_t, sig: libc::c_int) {
        // SAFETY: kill is async-signal-safe.
        if unsafe { libc::kill(-pid, sig) } != 0 {
            unsafe { libc::kill(pid, sig) };
        }
    }

    /// The program: a session of its own, its working directory, no new
    /// privileges, Landlock, the syscall filter, every signal at its default
    /// and unblocked, then exec. Returns only the status to exit with when
    /// it cannot.
    fn as_program(c: &Confinement, abi: u32, argv: &[CString]) -> i32 {
        // SAFETY: setsid takes nothing.
        check(unsafe { libc::setsid() } as libc::c_long, "a session of its own")
            .and_then(|()| into_workdir(c))
            // SAFETY: a flag on this process.
            .and_then(|()| check(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } as libc::c_long, "no_new_privs"))
            .and_then(|()| restrict(c, abi))
            .and_then(|()| filter_syscalls())
            .map(|()| {
                signals_at_default();
                exec(argv)
            })
            .unwrap_or_else(failed)
    }

    /// Into the sandbox's working directory, when it has one.
    fn into_workdir(c: &Confinement) -> Result<(), Error> {
        c.workdir.as_ref().map_or(Ok(()), |dir| {
            std::env::set_current_dir(dir).map_err(|e| Error::new(e.kind(), format!("working directory {}: {e}", dir.display())))
        })
    }

    /// Exec `argv`; returns only the status to exit with when it cannot.
    fn exec(argv: &[CString]) -> i32 {
        let mut ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        // SAFETY: a NUL-terminated argv of live CStrings.
        unsafe { libc::execvp(ptrs[0], ptrs.as_ptr()) };
        let e = Error::last_os_error();
        eprintln!("landlock: {}: {e}", argv[0].to_string_lossy());
        exec_failed(e.raw_os_error())
    }

    /// The status for an exec that failed, as a shell's: 127 when the
    /// program was not found, else 126.
    fn exec_failed(errno: Option<i32>) -> i32 {
        if errno == Some(libc::ENOENT) {
            127
        } else {
            126
        }
    }

    /// Every signal at its default and none blocked, as a program expects
    /// to start: an ignored one survives exec, and this process's runtime
    /// ignores SIGPIPE (`cmd | head` would print "Broken pipe" and fail).
    fn signals_at_default() {
        // SAFETY: dispositions and the mask of this process, which execs next.
        unsafe {
            for sig in 1..libc::SIGRTMAX() + 1 {
                libc::signal(sig, libc::SIG_DFL);
            }
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
        }
    }

    // seccomp's UAPI (linux/seccomp.h, linux/filter.h, linux/audit.h).
    const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
    const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    /// BPF_LD | BPF_W | BPF_ABS (the first two are 0).
    const BPF_LD_W_ABS: u16 = 0x20;
    const BPF_JEQ_K: u16 = 0x05 | 0x10;
    const BPF_JGE_K: u16 = 0x05 | 0x30;
    const BPF_JSET_K: u16 = 0x05 | 0x40;
    const BPF_RET_K: u16 = 0x06;
    /// `seccomp_data`'s offsets: the call's number, its architecture, and
    /// the low half of its first argument (both architectures are
    /// little-endian).
    const DATA_NR: u32 = 0;
    const DATA_ARCH: u32 = 4;
    const DATA_ARG0_LOW: u32 = 16;
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: Option<u32> = Some(0xc000_00b7);
    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: Option<u32> = Some(0xc000_003e);
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    const AUDIT_ARCH: Option<u32> = None;

    /// The calls refused with EPERM (the module doc says why).
    const REFUSED: [libc::c_long; 8] = [
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
    ];

    /// The `clone` flags that make a namespace.
    const NAMESPACE_FLAGS: libc::c_int = libc::CLONE_NEWNS
        | libc::CLONE_NEWCGROUP
        | libc::CLONE_NEWUTS
        | libc::CLONE_NEWIPC
        | libc::CLONE_NEWUSER
        | libc::CLONE_NEWPID
        | libc::CLONE_NEWNET;

    /// The filter: another architecture's calls end the process (no call
    /// reaches the kernel under a number this filter does not mean), the
    /// refused calls answer EPERM, `clone3` ENOSYS, `clone` with a namespace
    /// flag EPERM; everything else is allowed.
    fn syscall_filter(arch: u32) -> Vec<libc::sock_filter> {
        let stmt = |code, k| libc::sock_filter { code, jt: 0, jf: 0, k };
        let jump = |code, k, jt, jf| libc::sock_filter { code, jt, jf, k };
        let errno = |e: libc::c_int| stmt(BPF_RET_K, SECCOMP_RET_ERRNO | e as u32);
        let mut f = vec![stmt(BPF_LD_W_ABS, DATA_ARCH), jump(BPF_JEQ_K, arch, 1, 0), stmt(BPF_RET_K, SECCOMP_RET_KILL_PROCESS), stmt(BPF_LD_W_ABS, DATA_NR)];
        // x86_64's x32 calls are the same calls under other numbers.
        if cfg!(target_arch = "x86_64") {
            f.extend([jump(BPF_JGE_K, 0x4000_0000, 0, 1), errno(libc::EPERM)]);
        }
        for nr in REFUSED {
            f.extend([jump(BPF_JEQ_K, nr as u32, 0, 1), errno(libc::EPERM)]);
        }
        f.extend([jump(BPF_JEQ_K, libc::SYS_clone3 as u32, 0, 1), errno(libc::ENOSYS)]);
        f.extend([
            jump(BPF_JEQ_K, libc::SYS_clone as u32, 0, 3),
            stmt(BPF_LD_W_ABS, DATA_ARG0_LOW),
            jump(BPF_JSET_K, NAMESPACE_FLAGS as u32, 0, 1),
            errno(libc::EPERM),
        ]);
        f.push(stmt(BPF_RET_K, SECCOMP_RET_ALLOW));
        f
    }

    /// Install [`syscall_filter`] on this process and what it starts. Needs
    /// no-new-privileges, set first.
    fn filter_syscalls() -> Result<(), Error> {
        let arch = AUDIT_ARCH.ok_or_else(|| Error::other("no syscall filter for this CPU architecture"))?;
        let filter = syscall_filter(arch);
        let program = libc::sock_fprog { len: filter.len() as libc::c_ushort, filter: filter.as_ptr().cast_mut() };
        // SAFETY: a live filter program of the length given.
        check(unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program) } as libc::c_long, "installing the syscall filter")
    }

    /// `path` inside `root`.
    fn under(root: &Path, path: &Path) -> PathBuf {
        root.join(path.components().filter(|c| matches!(c, Component::Normal(_))).collect::<PathBuf>())
    }

    /// mount(2)'s strings, each NUL-terminated, or absent.
    struct MountArgs {
        source: Option<CString>,
        target: CString,
        fstype: Option<CString>,
        data: Option<CString>,
    }

    fn mount_args(source: Option<&Path>, target: &Path, fstype: Option<&str>, data: Option<&str>) -> Result<MountArgs, Error> {
        Ok(MountArgs {
            source: source.map(cstr).transpose()?,
            target: cstr(target)?,
            fstype: fstype.map(|f| CString::new(f).unwrap()),
            data: data.map(|d| CString::new(d).unwrap()),
        })
    }

    fn mount(source: Option<&Path>, target: &Path, fstype: Option<&str>, flags: libc::c_ulong, data: Option<&str>) -> Result<(), Error> {
        let ptr = |c: &Option<CString>| c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
        mount_args(source, target, fstype, data).and_then(|a| {
            // SAFETY: NUL-terminated strings or null, as mount(2) takes them.
            let rc = unsafe { libc::mount(ptr(&a.source), a.target.as_ptr(), ptr(&a.fstype), flags, ptr(&a.data).cast()) };
            check(rc as libc::c_long, &format!("mounting on {}", target.display()))
        })
    }

    /// `mount_setattr`'s flags: every mount beneath too, with `recursive`.
    fn attr_flags(recursive: bool) -> libc::c_uint {
        if recursive {
            libc::AT_RECURSIVE as libc::c_uint
        } else {
            0
        }
    }

    /// Set `attrs` on the mount at `target` and, with `recursive`, every
    /// mount beneath it.
    fn set_attrs(target: &Path, attrs: u64, recursive: bool) -> Result<(), Error> {
        let attr = MountAttr { attr_set: attrs, attr_clr: 0, propagation: 0, userns_fd: 0 };
        cstr(target).and_then(|target_c| {
            // SAFETY: a live mount_attr of its declared size.
            let rc = unsafe { libc::syscall(libc::SYS_mount_setattr, libc::AT_FDCWD, target_c.as_ptr(), attr_flags(recursive), &attr, std::mem::size_of::<MountAttr>()) };
            check(rc, &format!("setting {} read-only or nosuid", target.display()))
        })
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
        mount_point(source, target)
            .and_then(|()| mount(Some(source), target, None, libc::MS_BIND | libc::MS_REC, None))
            .and_then(|()| set_attrs(target, attrs, true))
    }

    /// The sandbox's root: a tmpfs holding the granted paths, devices and a
    /// fresh `/proc`, read-only, pivoted into.
    fn build_root(c: &Confinement) -> Result<(), Error> {
        let root = &c.root;
        mount(None, Path::new("/"), None, libc::MS_REC | libc::MS_PRIVATE, None)
            .and_then(|()| mount(None, root, Some("tmpfs"), libc::MS_NOSUID | libc::MS_NODEV, Some("mode=0755,size=4m")))
            .and_then(|()| grant_paths(c))
            .and_then(|()| kernel_trees(root))
            .and_then(|()| set_attrs(root, libc::MOUNT_ATTR_RDONLY | libc::MOUNT_ATTR_NOSUID | libc::MOUNT_ATTR_NODEV, false))
            .and_then(|()| pivot_into(root))
    }

    /// Each granted path and device, where it is bound in the root, and the
    /// mount attributes it gets there: the paths in order (read-only unless
    /// writable), then the devices.
    fn root_binds(c: &Confinement) -> Vec<(&Path, PathBuf, u64)> {
        let paths = c.binds.iter().map(|Bind { path, writable }| {
            let ro = if *writable { 0 } else { libc::MOUNT_ATTR_RDONLY };
            (path.as_path(), under(&c.root, path), libc::MOUNT_ATTR_NOSUID | libc::MOUNT_ATTR_NODEV | ro)
        });
        let devices = c.devices.iter().map(|device| (device.as_path(), under(&c.root, device), libc::MOUNT_ATTR_NOSUID));
        paths.chain(devices).collect()
    }

    /// The granted links, paths and devices, each at its own path in the root.
    fn grant_paths(c: &Confinement) -> Result<(), Error> {
        c.links
            .iter()
            .try_for_each(|(path, target)| std::os::unix::fs::symlink(target, under(&c.root, path)))
            .and_then(|()| root_binds(c).into_iter().try_for_each(|(source, target, attrs)| bind(source, &target, attrs)))
    }

    /// `/dev`'s standard links, as name and target.
    const DEV_LINKS: [(&str, &str); 4] = [("fd", "/proc/self/fd"), ("stdin", "/proc/self/fd/0"), ("stdout", "/proc/self/fd/1"), ("stderr", "/proc/self/fd/2")];

    /// `/dev`'s standard links, an empty `/tmp` and a fresh `/proc`.
    fn kernel_trees(root: &Path) -> Result<(), Error> {
        let (dev, proc) = (root.join("dev"), root.join("proc"));
        std::fs::create_dir_all(&dev)
            .and_then(|()| DEV_LINKS.iter().try_for_each(|(name, target)| std::os::unix::fs::symlink(target, dev.join(name))))
            .and_then(|()| std::fs::create_dir_all(root.join("tmp")))
            .and_then(|()| std::fs::create_dir_all(&proc))
            .and_then(|()| mount(Some(Path::new("proc")), &proc, Some("proc"), libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, None))
    }

    /// Make `root` this process's `/`, the old root detached.
    fn pivot_into(root: &Path) -> Result<(), Error> {
        let dot = c".";
        // SAFETY: pivot_root(".", ".") stacks the old root over the new one,
        // and the detach drops it, as pivot_root(2) describes.
        std::env::set_current_dir(root)
            .and_then(|()| check(unsafe { libc::syscall(libc::SYS_pivot_root, dot.as_ptr(), dot.as_ptr()) }, "pivot_root"))
            .and_then(|()| check(unsafe { libc::umount2(dot.as_ptr(), libc::MNT_DETACH) } as libc::c_long, "detaching the old root"))
            .and_then(|()| std::env::set_current_dir("/"))
    }

    /// An `ifreq` naming the interface `name`.
    fn ifreq_named(name: &[u8]) -> libc::ifreq {
        // SAFETY: an all-zero ifreq is a valid one.
        let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
        for (slot, b) in req.ifr_name.iter_mut().zip(name) {
            *slot = *b as libc::c_char;
        }
        req
    }

    /// Bring the new network namespace's loopback up, as a container's is.
    fn loopback_up() -> Result<(), Error> {
        // SAFETY: a socket this function owns, closed below.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        check(fd as libc::c_long, "a socket for the loopback")?;
        let result = interface_up(fd, ifreq_named(b"lo"));
        // SAFETY: closing the socket made above.
        unsafe { libc::close(fd) };
        result
    }

    /// The UP flag set on the interface `req` names, through the socket `fd`.
    fn interface_up(fd: libc::c_int, mut req: libc::ifreq) -> Result<(), Error> {
        // SAFETY: ioctls on a live socket with a live ifreq, its flags the
        // union member SIOCGIFFLAGS filled.
        check(unsafe { libc::ioctl(fd, libc::SIOCGIFFLAGS, &mut req) } as libc::c_long, "bringing the loopback up").and_then(|()| {
            unsafe { req.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short };
            check(unsafe { libc::ioctl(fd, libc::SIOCSIFFLAGS, &req) } as libc::c_long, "bringing the loopback up")
        })
    }

    /// What [`restrict`] asks of Landlock: the rights it handles, and each
    /// path's and port's grant.
    struct Ruleset {
        attr: RulesetAttr,
        paths: Vec<(PathBuf, u64)>,
        ports: Vec<u16>,
    }

    /// Every right this ABI handles is denied but on the granted paths (the
    /// root's listing and `/proc` read, a writable bind's every right, a
    /// read-only one's read and execute, a device's read and write), TCP
    /// binds (on the host's network) but on the granted ports, and signals
    /// and abstract unix sockets stay inside the sandbox.
    fn ruleset(c: &Confinement, abi: u32) -> Ruleset {
        let handled = fs_rights(abi);
        let paths = [(PathBuf::from("/"), FS_READ_DIR), (PathBuf::from("/proc"), FS_READ_FILE | FS_READ_DIR)]
            .into_iter()
            .chain(c.binds.iter().map(|b| (b.path.clone(), if b.writable { handled } else { READ })))
            .chain(c.devices.iter().map(|d| (d.clone(), FS_READ_FILE | FS_WRITE_FILE | FS_IOCTL_DEV)))
            .map(|(path, rights)| (path, rights & handled))
            .collect();
        let (net, ports) = if c.host_network { (NET_BIND_TCP, c.bind_ports.clone()) } else { (0, Vec::new()) };
        Ruleset {
            attr: RulesetAttr { handled_access_fs: handled, handled_access_net: net, scoped: SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL },
            paths,
            ports,
        }
    }

    /// Restrict this process with Landlock as [`ruleset`] says.
    fn restrict(c: &Confinement, abi: u32) -> Result<(), Error> {
        let rules = ruleset(c, abi);
        // SAFETY: a live attr of its declared size.
        let fd = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &rules.attr, std::mem::size_of::<RulesetAttr>(), 0u32) };
        check(fd, "creating the Landlock ruleset")?;
        let result = apply(fd as libc::c_int, &rules);
        // SAFETY: closing the fd made above.
        unsafe { libc::close(fd as libc::c_int) };
        result
    }

    /// `rules`' grants added to the ruleset `fd`, then this process
    /// restricted by it.
    fn apply(fd: libc::c_int, rules: &Ruleset) -> Result<(), Error> {
        rules
            .paths
            .iter()
            .try_for_each(|(path, rights)| add_path_rule(fd, path, *rights))
            .and_then(|()| rules.ports.iter().try_for_each(|port| add_port_rule(fd, *port)))
            // SAFETY: a ruleset fd the caller made.
            .and_then(|()| check(unsafe { libc::syscall(libc::SYS_landlock_restrict_self, fd, 0u32) }, "applying the Landlock ruleset"))
    }

    fn add_port_rule(ruleset: libc::c_int, port: u16) -> Result<(), Error> {
        let rule = NetPort { allowed_access: NET_BIND_TCP, port: u64::from(port) };
        // SAFETY: a live rule of the type named.
        check(unsafe { libc::syscall(libc::SYS_landlock_add_rule, ruleset, RULE_NET_PORT, &rule, 0u32) }, &format!("granting port {port}"))
    }

    fn add_path_rule(ruleset: libc::c_int, path: &Path, rights: u64) -> Result<(), Error> {
        let fd = open_path(path)?;
        let rule = PathBeneath { allowed_access: rule_rights(path, rights), parent_fd: fd };
        // SAFETY: a live rule of the type named, then closing its fd.
        let rc = unsafe { libc::syscall(libc::SYS_landlock_add_rule, ruleset, RULE_PATH_BENEATH, &rule, 0u32) };
        unsafe { libc::close(fd) };
        check(rc, &format!("granting {}", path.display()))
    }

    /// `path` opened `O_PATH`, for a rule beneath it.
    fn open_path(path: &Path) -> Result<libc::c_int, Error> {
        let path_c = cstr(path)?;
        // SAFETY: an O_PATH open of a NUL-terminated path.
        let fd = unsafe { libc::open(path_c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        check(fd as libc::c_long, &format!("opening {}", path.display())).map(|()| fd)
    }

    /// The rights a rule beneath `path` carries: all of `rights` on a
    /// directory, only those a file can have on anything else.
    fn rule_rights(path: &Path, rights: u64) -> u64 {
        if std::fs::metadata(path).is_ok_and(|m| m.is_dir()) {
            rights
        } else {
            rights & FILE_RIGHTS
        }
    }

    #[cfg(test)]
    mod tests {
        /// A host that lets a process make a user namespace but not mount a
        /// `/proc` in it (an unprivileged container, whose `/proc` has a
        /// locked mount over part of it) cannot confine one: every sandbox
        /// would fail building its root. util-linux `unshare` makes such a
        /// host here; the probe runs in it as this test, asked by
        /// `CONFINE_PROBE_CHILD`.
        #[test]
        fn a_host_that_refuses_a_proc_mount_cannot_confine() {
            if std::env::var_os("CONFINE_PROBE_CHILD").is_some() {
                println!("refusal: {:?}", super::support().refusal());
                return;
            }
            if super::support().refusal().is_some() || !std::path::Path::new("/usr/bin/unshare").exists() {
                return;
            }
            let me = std::env::current_exe().unwrap();
            let out = std::process::Command::new("/usr/bin/unshare")
                .args(["-Urm", "/bin/sh", "-c", r#"mount -t tmpfs none /proc/sys && exec /usr/bin/unshare -Urm "$0" --exact "$1" --nocapture"#])
                .arg(&me)
                .arg("confine::imp::tests::a_host_that_refuses_a_proc_mount_cannot_confine")
                .env("CONFINE_PROBE_CHILD", "1")
                .output()
                .unwrap();
            let said = String::from_utf8_lossy(&out.stdout);
            assert!(said.contains("refusal: Some(") && said.contains("cannot build a sandbox's root"), "{said}{}", String::from_utf8_lossy(&out.stderr));
        }

        /// Each ABI handles the rights it knows and none it does not: a
        /// ruleset naming an unknown right is refused by the kernel.
        #[test]
        fn the_rights_handled_follow_the_abi() {
            assert_eq!(super::fs_rights(1), (1 << 13) - 1);
            assert_eq!(super::fs_rights(6), (1 << 16) - 1);
            assert_eq!(super::fs_rights(9), (1 << 17) - 1);
        }

        use super::*;
        use std::path::{Path, PathBuf};

        /// A sandbox has a network of its own unless it shares the host's;
        /// the user, mount and pid namespaces it always has.
        #[test]
        fn the_namespaces_follow_the_network() {
            let always = libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWPID;
            assert_eq!(namespace_flags(true), always);
            assert_eq!(namespace_flags(false), always | libc::CLONE_NEWNET);
        }

        #[test]
        fn the_maps_map_this_user_and_group_to_themselves() {
            assert_eq!(self_maps(1000, 100), ("1000 1000 1".to_string(), "100 100 1".to_string()));
        }

        /// The probe's exit: no namespaces (the root never tried), a root
        /// that built, and one that did not or whose child never exited.
        #[test]
        fn the_probe_says_what_failed_first() {
            assert_eq!(probe_status(false, || panic!("no root is tried without namespaces")), PROBE_NO_NAMESPACES);
            assert_eq!(probe_status(true, || Some(0)), 0);
            assert_eq!(probe_status(true, || Some(3)), PROBE_NO_ROOT);
            assert_eq!(probe_status(true, || None), PROBE_NO_ROOT);
        }

        /// Reaping waits through other children and EINTR for the program,
        /// then exits as it did: its code, or 128 plus its signal; any other
        /// failure to reap is status 1.
        #[test]
        fn the_init_exits_as_its_program_did() {
            let reaping = |steps: Vec<(Result<libc::pid_t, Option<i32>>, i32)>| {
                let mut steps = steps.into_iter();
                reaped_code(7, move |status| {
                    let (r, s) = steps.next().expect("reaped past the program");
                    *status = s;
                    r
                })
            };
            assert_eq!(reaping(vec![(Ok(8), 0), (Err(Some(libc::EINTR)), 0), (Ok(7), 3 << 8)]), 3);
            assert_eq!(reaping(vec![(Ok(7), libc::SIGTERM)]), 128 + libc::SIGTERM);
            assert_eq!(reaping(vec![(Err(Some(libc::ECHILD)), 0)]), 1);
            assert_eq!(reaping(vec![(Err(None), 0)]), 1);
        }

        /// Nothing starts without the ABI it needs, an argv of C strings
        /// and a program; what was wrong is said.
        #[test]
        fn a_sandbox_starts_only_with_its_abi_and_a_program() {
            let os = |args: &[&str]| args.iter().map(OsString::from).collect::<Vec<_>>();
            let refused = |abi, args: &[&str]| ready(abi, &os(args)).unwrap_err().to_string();
            assert!(refused(None, &["true"]).contains("ABI 6 or later"));
            assert!(refused(Some(super::super::LANDLOCK_ABI_NEEDED - 1), &["true"]).contains("ABI 6 or later"));
            // (#3575 review) The floor itself is enough.
            assert!(ready(Some(super::super::LANDLOCK_ABI_NEEDED), &os(&["true"])).is_ok());
            assert!(refused(Some(9), &[]).contains("no program to run"));
            assert!(refused(Some(9), &["tr\0ue"]).contains("holds a NUL"));
            let (abi, argv) = ready(Some(9), &os(&["sh", "-c", "true"])).unwrap();
            assert_eq!(abi, 9);
            assert_eq!(argv, vec![c"sh".to_owned(), c"-c".to_owned(), c"true".to_owned()]);
        }

        /// `enter` with nothing to run returns why, here, before it forks or
        /// makes a namespace: this test goes on running.
        #[test]
        fn entering_with_no_program_returns_why_without_starting_anything() {
            let e = enter(&Confinement::default(), &[]).to_string();
            assert!(e.contains("no program to run") || e.contains("ABI 6 or later"), "{e}");
        }

        #[test]
        fn an_exec_that_fails_exits_as_a_shell_would() {
            assert_eq!(exec_failed(Some(libc::ENOENT)), 127);
            assert_eq!(exec_failed(Some(libc::EACCES)), 126);
            assert_eq!(exec_failed(None), 126);
        }

        /// The filter's shape: another architecture's call ends the
        /// process; each refused call, `clone3` and a namespace-making
        /// `clone` answer an errno; anything else is allowed.
        #[test]
        fn the_syscall_filter_refuses_what_it_says() {
            let arch = 0xc000_00b7;
            let f = syscall_filter(arch);
            let at = |i: usize| (f[i].code, f[i].k);
            // (#3575 review) Where each test jumps is what the program does.
            let jumps = |i: usize| (f[i].jt, f[i].jf);
            assert_eq!(at(0), (BPF_LD_W_ABS, DATA_ARCH));
            assert_eq!(at(1), (BPF_JEQ_K, arch));
            assert_eq!(jumps(1), (1, 0), "another arch falls through to the kill");
            assert_eq!(at(2), (BPF_RET_K, SECCOMP_RET_KILL_PROCESS));
            assert_eq!(at(3), (BPF_LD_W_ABS, DATA_NR));
            assert_eq!(*f.last().map(|i| (i.code, i.k)).as_ref().unwrap(), (BPF_RET_K, SECCOMP_RET_ALLOW));
            let answers = |nr: libc::c_long| {
                let i = f.iter().position(|i| i.code == BPF_JEQ_K && i.k == nr as u32).unwrap_or_else(|| panic!("call {nr} not matched"));
                assert_eq!(jumps(i), (0, 1), "call {nr}: matched runs its answer, else skips it");
                f[i + 1].k
            };
            for nr in REFUSED {
                assert_eq!(answers(nr), SECCOMP_RET_ERRNO | libc::EPERM as u32, "call {nr}");
            }
            assert_eq!(answers(libc::SYS_clone3), SECCOMP_RET_ERRNO | libc::ENOSYS as u32);
            let clone = f.iter().position(|i| i.code == BPF_JEQ_K && i.k == libc::SYS_clone as u32).unwrap();
            assert_eq!(jumps(clone), (0, 3), "clone checks its flags, else skips them");
            assert_eq!(at(clone + 1), (BPF_LD_W_ABS, DATA_ARG0_LOW));
            assert_eq!(jumps(clone + 2), (0, 1), "namespace flags are refused, else allowed");
            assert_eq!(at(clone + 2), (BPF_JSET_K, NAMESPACE_FLAGS as u32));
            assert_eq!(at(clone + 3), (BPF_RET_K, SECCOMP_RET_ERRNO | libc::EPERM as u32));
        }

        /// mount(2)'s strings: a path holding a NUL is refused, absent ones
        /// stay absent.
        #[test]
        fn the_mount_strings_are_what_was_asked() {
            assert!(mount_args(None, Path::new("/a\0b"), None, None).is_err());
            assert!(mount_args(Some(Path::new("/a\0b")), Path::new("/t"), None, None).is_err());
            let a = mount_args(None, Path::new("/t"), None, None).unwrap();
            assert_eq!((a.source, a.target, a.fstype, a.data), (None, c"/t".to_owned(), None, None));
            let a = mount_args(Some(Path::new("proc")), Path::new("/t"), Some("proc"), Some("mode=0755")).unwrap();
            assert_eq!((a.source, a.fstype, a.data), (Some(c"proc".to_owned()), Some(c"proc".to_owned()), Some(c"mode=0755".to_owned())));
        }

        #[test]
        fn attributes_reach_the_mounts_beneath_only_when_recursive() {
            assert_eq!(attr_flags(true), libc::AT_RECURSIVE as libc::c_uint);
            assert_eq!(attr_flags(false), 0);
        }

        /// A bind's mount point is of its source's kind, its parents made;
        /// an existing file is kept; a missing source is named.
        #[test]
        fn a_mount_point_is_made_like_its_source() {
            let t = tempfile::TempDir::new().unwrap();
            let (dir, file) = (t.path().join("dir"), t.path().join("file"));
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(&file, "source").unwrap();
            let root = t.path().join("root");
            mount_point(&dir, &root.join("a/dir")).unwrap();
            assert!(root.join("a/dir").is_dir());
            mount_point(&file, &root.join("b/file")).unwrap();
            assert_eq!(std::fs::read_to_string(root.join("b/file")).unwrap(), "");
            std::fs::write(root.join("b/file"), "kept").unwrap();
            mount_point(&file, &root.join("b/file")).unwrap();
            assert_eq!(std::fs::read_to_string(root.join("b/file")).unwrap(), "kept");
            let missing = t.path().join("missing");
            assert!(mount_point(&missing, &root.join("c")).unwrap_err().to_string().contains(&missing.display().to_string()));
            assert!(!root.join("c").exists());
        }

        /// Each granted path is bound at its own path in the root, in order,
        /// read-only unless writable, then each device; none setuid or a
        /// device node but the devices.
        #[test]
        fn the_root_binds_each_grant_at_its_own_path() {
            let c = Confinement {
                root: PathBuf::from("/r"),
                binds: vec![Bind { path: "/w".into(), writable: true }, Bind { path: "/o/p".into(), writable: false }],
                devices: vec!["/dev/null".into()],
                ..Default::default()
            };
            let (nosuid, nodev, ro) = (libc::MOUNT_ATTR_NOSUID, libc::MOUNT_ATTR_NODEV, libc::MOUNT_ATTR_RDONLY);
            assert_eq!(
                root_binds(&c),
                vec![
                    (Path::new("/w"), PathBuf::from("/r/w"), nosuid | nodev),
                    (Path::new("/o/p"), PathBuf::from("/r/o/p"), nosuid | nodev | ro),
                    (Path::new("/dev/null"), PathBuf::from("/r/dev/null"), nosuid),
                ]
            );
        }

        #[test]
        fn an_ifreq_names_its_interface() {
            let req = ifreq_named(b"lo");
            assert_eq!(&req.ifr_name[..3], &[b'l' as libc::c_char, b'o' as libc::c_char, 0]);
            assert!(req.ifr_name[3..].iter().all(|b| *b == 0));
        }

        /// What Landlock is asked: every right the ABI knows handled; the
        /// root listed, `/proc` read, a writable bind's every right, a
        /// read-only one's read and execute, a device's read, write and
        /// ioctl, each cut to the ABI; TCP binds held to the granted ports on
        /// the host's network, and nothing of the network handled on a
        /// network of its own; signals and abstract sockets scoped.
        #[test]
        fn the_ruleset_grants_what_the_confinement_says() {
            let mut c = Confinement {
                binds: vec![Bind { path: "/w".into(), writable: true }, Bind { path: "/o".into(), writable: false }],
                devices: vec!["/dev/null".into()],
                host_network: true,
                bind_ports: vec![8080],
                ..Default::default()
            };
            let all = fs_rights(9);
            let r = ruleset(&c, 9);
            assert_eq!((r.attr.handled_access_fs, r.attr.handled_access_net, r.attr.scoped), (all, NET_BIND_TCP, SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL));
            assert_eq!(
                r.paths,
                vec![
                    (PathBuf::from("/"), FS_READ_DIR),
                    (PathBuf::from("/proc"), FS_READ_FILE | FS_READ_DIR),
                    (PathBuf::from("/w"), all),
                    (PathBuf::from("/o"), READ),
                    (PathBuf::from("/dev/null"), FS_READ_FILE | FS_WRITE_FILE | FS_IOCTL_DEV),
                ]
            );
            assert_eq!(r.ports, vec![8080]);
            // ABI 1 knows no ioctl right: it is not asked for.
            assert_eq!(ruleset(&c, 1).paths[4].1, FS_READ_FILE | FS_WRITE_FILE);
            c.host_network = false;
            let r = ruleset(&c, 9);
            assert_eq!((r.attr.handled_access_net, r.ports), (0, Vec::new()));
        }

        /// A rule beneath a directory carries every right asked; beneath
        /// anything else, only a file's.
        #[test]
        fn a_rule_beneath_a_file_carries_only_a_files_rights() {
            let t = tempfile::TempDir::new().unwrap();
            let file = t.path().join("f");
            std::fs::write(&file, "").unwrap();
            let all = fs_rights(9);
            assert_eq!(rule_rights(t.path(), all), all);
            assert_eq!(rule_rights(&file, all), all & FILE_RIGHTS);
            assert_eq!(rule_rights(&t.path().join("missing"), all), all & FILE_RIGHTS);
            assert_ne!(all & FILE_RIGHTS, all, "a directory's rights reach past a file's");
        }

        /// Whether `pid` ends (signaled, reaped) within five seconds; it is
        /// killed and reaped either way.
        fn ends_within(pid: libc::pid_t) -> Option<i32> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut status = 0;
            while std::time::Instant::now() < deadline {
                // SAFETY: waiting on this test's own child.
                if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == pid {
                    return libc::WIFSIGNALED(status).then(|| libc::WTERMSIG(status));
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            // SAFETY: ending and reaping this test's own child.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, &mut status, 0);
            }
            None
        }

        /// A stop's signal reaches a program that leads no group yet (its
        /// session not made), and the whole group of one that does.
        #[test]
        fn a_stop_reaches_the_program_and_its_group() {
            // SAFETY: the children call only async-signal-safe functions.
            unsafe {
                let alone = forked(|| {
                    libc::pause();
                    0
                });
                std::thread::sleep(std::time::Duration::from_millis(50));
                signal_group_or_process(alone, libc::SIGTERM);
                assert_eq!(ends_within(alone), Some(libc::SIGTERM), "a program leading no group");

                let mut pipe = [0; 2];
                assert_eq!(libc::pipe(pipe.as_mut_ptr()), 0);
                let leader = forked(|| {
                    libc::close(pipe[0]);
                    libc::setsid();
                    // Its child holds the pipe too: it reads EOF only once
                    // both are gone.
                    forked(|| {
                        libc::pause();
                        0
                    });
                    libc::pause();
                    0
                });
                libc::close(pipe[1]);
                std::thread::sleep(std::time::Duration::from_millis(100));
                signal_group_or_process(leader, libc::SIGTERM);
                let mut poll = libc::pollfd { fd: pipe[0], events: libc::POLLIN, revents: 0 };
                let mut byte = 0u8;
                let ended = libc::poll(&mut poll, 1, 5000) == 1 && libc::read(pipe[0], (&mut byte as *mut u8).cast(), 1) == 0;
                assert_eq!(ends_within(leader), Some(libc::SIGTERM), "the group's leader");
                libc::close(pipe[0]);
                assert!(ended, "the group's other process outlived the stop");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each missing piece is named; a host with all of them runs.
    #[test]
    fn a_refusal_names_what_is_missing() {
        let ok = ConfineSupport { linux: true, landlock_abi: Some(LANDLOCK_ABI_NEEDED), user_namespaces: true, sandbox_root: true, syscall_filter: true };
        assert_eq!(ok.refusal(), None);
        let old = ConfineSupport { landlock_abi: Some(4), ..ok };
        assert!(old.refusal().unwrap().contains("ABI is 4"), "{:?}", old.refusal());
        let none = ConfineSupport { landlock_abi: None, user_namespaces: false, sandbox_root: false, ..ok };
        let why = none.refusal().unwrap();
        assert!(why.contains("no Landlock") && why.contains("user namespace") && !why.contains("root"), "{why}");
        let contained = ConfineSupport { sandbox_root: false, ..ok };
        let why = contained.refusal().unwrap();
        assert!(why.contains("cannot build a sandbox's root") && why.contains("/proc"), "{why}");
        let unfiltered = ConfineSupport { syscall_filter: false, ..ok };
        assert!(unfiltered.refusal().unwrap().contains("no syscall filter for this CPU"), "{:?}", unfiltered.refusal());
    }

    /// Off Linux the answer is that Landlock is Linux's, not that the
    /// kernel is old.
    #[test]
    fn off_linux_the_refusal_says_landlock_needs_linux() {
        let why = ConfineSupport::default().refusal().unwrap();
        assert_eq!(why, "Landlock is part of the Linux kernel, and this host is not running Linux");
    }

    #[test]
    fn an_exit_status_reads_as_a_shell_reports_it() {
        assert_eq!(exit_code(Some(3), None), 3);
        assert_eq!(exit_code(None, Some(9)), 137);
    }
}
