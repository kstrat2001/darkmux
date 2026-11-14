//! (#3112 step 2) The Landlock backend: a policy as a process on this Linux
//! host confined by the kernel's own pieces, user, mount and pid namespaces
//! (and a network namespace for [`Network::None`]) with Landlock on top
//! ([`darkmux_platform::confine`]). No daemon, no image, nothing to install.
//!
//! The sandbox's root holds only what the policy grants, each at its own
//! host path: the system's programs and libraries read-only, `/etc` without
//! its secrets, a few device files, its mounts (read-only ones are read-only
//! mounts, so renaming a directory above one does not free it), its own
//! `tmp`, and a `/proc` that shows only its own processes. Everything else,
//! the user's home included, does not exist there. `/tmp` there is empty and
//! read-only: its own `tmp` is `TMPDIR` (and `HOME`), the one place it may
//! write outside its mounts. Its first process is an init; when the program
//! exits, everything left in the sandbox ends with it, a descendant that
//! left the process group included. The program leads a session of its own,
//! with no controlling terminal, and a syscall filter refuses the kernel
//! calls Docker's default seccomp profile refuses that a sandbox never needs
//! ([`darkmux_platform::confine`] lists them).
//!
//! What it cannot do, against Docker:
//! - **Place a mount elsewhere.** A mount is seen at its own (canonical)
//!   host path and [`SandboxBackend::place`] says so, as Seatbelt does. A
//!   caller builds every in-sandbox path from `place`, never from `Mount.at`.
//! - **Hide the host's toolchain or OS.** There is no image: the process
//!   runs this host's programs, read-only, and `policy.image` is unused. It
//!   needs an `entrypoint`. A toolchain installed under the user's home
//!   (rustup's, mise's) is not visible; a mount can grant it.
//! - **Run a 32-bit program.** The syscall filter ends a process that makes
//!   a call under another architecture's numbers (arm32 on aarch64, i386 on
//!   x86_64); Docker's default profile allows them.
//! - **Limit memory or processes, or switch user.** `limits` and `user` are
//!   unused; the process runs as the caller's user.
//! - **Bind a port on the loopback only (#3559).** The default network is
//!   the host's, and Landlock grants a bind by port, never by address. A
//!   policy naming `ports` binds only those, at any address, so a published
//!   port can answer beyond the loopback if the program binds it there; and
//!   a `listen` on a socket with no bind is no bind Landlock sees, so even
//!   such a policy listens on every address, on a port the kernel picks
//!   (`a_policy_naming_ports_cannot_hold_a_listen_with_no_bind`). Landlock
//!   runs no client preview yet, so no policy naming ports reaches a user.
//!   A policy naming none (a dispatch's) binds any port, as under Seatbelt
//!   (#3567): holding it to no port would refuse a loopback server too.
//!   UDP is not filtered at all. darkmux watches a dispatch instead: the TCP
//!   listeners its processes hold are read every second and one beyond the
//!   loopback stops it (`darkmux-crew`'s `sandbox_listeners`; detection with
//!   a window, not a boundary). It may reach any address, the host's own
//!   loopback services included, but no unix socket outside its mounts and
//!   no abstract one outside the sandbox. [`Network::None`] has a loopback of
//!   its own and nothing else.
//! - **Keep a dispatch off a host service's loopback port (#3567).** As
//!   under Seatbelt, a dispatch may bind a port of its choosing on
//!   `127.0.0.1` and hold it for its whole run, and the watch never stops a
//!   loopback listener. One that takes the port of a host service that is
//!   down or restarting (a Tensorfold or Flint server darkmux starts, LM
//!   Studio, `darkmux serve`) keeps that service from starting there, and
//!   answers in its place until the dispatch ends.
//! - **Give a terminal or `/dev/shm`.** Neither is granted: started from a
//!   terminal, it has no controlling one and cannot open `/dev/tty`, so it
//!   cannot type into it (`TIOCSTI`). A terminal it inherits as stdout or
//!   stderr can still be written to, escape sequences included, as under
//!   `docker run -t`.
//!
//! It needs Landlock ABI 6 (Linux 6.12) and unprivileged user namespaces;
//! [`LandlockBackend::available`] says why not where either is missing, and
//! the backend is refused there rather than run weaker.
//!
//! Each sandbox has a directory under [`state_root`] while it exists: its
//! rules, its labels, its launcher's pid and start time once started, its
//! own `tmp` (its `HOME` and `TMPDIR`, the only path it may write outside
//! its mounts), the empty `root` its view is built on, and, for one created
//! but not started, its start script. The sandbox sees only `tmp`. The
//! launcher removes the directory when the sandbox exits.
//!
//! The confined process is started by a helper: this same executable, run
//! with [`HELPER_ARG`] first. In `darkmux` it is a row of the multi-call
//! entry (`src/multicall.rs`), routed to [`helper`] before anything else; a
//! test binary that starts sandboxes routes it with [`helper_if_asked`]
//! before its `main` ([`darkmux_platform::confine_before_main`]).

use crate::{Access, BackendKind, Listed, Mount, Network, Query, SandboxBackend, SandboxHandle, SandboxPolicy};
use darkmux_platform::confine::{Bind, Confinement};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use crate::seatbelt::{canonical, join, sh_quote};
use std::process::{Command, Stdio};

/// The first argument that makes this executable the sandbox's helper.
pub const HELPER_ARG: &str = "__darkmux-landlock";

/// The shell every planned command runs.
const SH: &str = "/bin/sh";

/// `PATH` inside a sandbox whose policy names none.
pub(crate) const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Host paths every sandbox may read where they exist: the OS's programs,
/// libraries and data, and software installed beside them. A symbolic link
/// among them (`/bin` to `usr/bin`) is kept a link. Never the user's home,
/// `/var`, `/run` or `/tmp`.
const SYSTEM_READ: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/opt"];

/// What `/etc` holds that a sandbox may not read: password hashes, sudo's
/// rules, ssh's host keys and configuration, network secrets, and this
/// machine's identifier. The rest of `/etc` is granted entry by entry.
const DENIED_ETC: &[&str] = &[
    "shadow",
    "shadow-",
    "gshadow",
    "gshadow-",
    "sudoers",
    "sudoers.d",
    "ssh",
    "NetworkManager",
    "wireguard",
    "machine-id",
    "credstore",
    "credstore.encrypted",
];

/// Where a link in `/etc` may not lead: the host's `/proc`, `/sys` and
/// `/dev` describe the host, not the sandbox.
const KERNEL_TREES: &[&str] = &["/proc", "/sys", "/dev"];

/// Device files every sandbox may read and write. Not `/dev/tty`: the
/// program has no controlling terminal.
const DEVICES: &[&str] = &["/dev/null", "/dev/zero", "/dev/full", "/dev/random", "/dev/urandom"];

/// Env override of [`state_root`], for tests (the same variable Seatbelt's
/// state directory reads).
pub use crate::seatbelt::STATE_DIR_ENV;

/// Writes the sandbox's directory: `$1` the directory, `$2` the rules, `$3`
/// the labels. Fails (125, Docker's own "could not make it") when the name
/// is taken.
const PREPARE: &str = r#"d=$1
mkdir -m 700 "$d" 2>/dev/null || { echo "landlock: sandbox $d exists" >&2; exit 125; }
mkdir -m 700 "$d/tmp" "$d/root" && printf '%s' "$2" > "$d/rules" && printf '%s' "$3" > "$d/labels" || exit 125
"#;

/// Runs the sandbox attached: `$d` set, `$1` the helper, then the program
/// and its arguments. Records its own pid (the group's) and start time,
/// survives the group's SIGTERM long enough to see the sandbox out, and
/// removes the directory when it exits.
const LAUNCH: &str = r#"helper=$1; shift
echo $$ > "$d/pid" && sed 's/.*) //' /proc/$$/stat | cut -d' ' -f20 > "$d/started" || exit 125
trap : TERM INT HUP
[ -n "$1" ] || { echo "landlock: no program to run (a policy needs an entrypoint)" >&2; rm -rf "$d"; exit 125; }
"$helper" __darkmux-landlock "$d" "$@"
rc=$?
rm -rf "$d"
exit $rc
"#;

/// Whether `$d` is a sandbox's directory at all: only a directory holding
/// the rules this backend wrote is ever listed, stopped or deleted.
const OURS: &str = r#"[ -f "$d/rules" ]"#;

/// Whether `$d`'s sandbox is running: its launcher's pid is alive and is
/// still that launcher, started when the launcher recorded (a reused pid,
/// or another sandbox's launcher, is not), leaving `$pid` set. The start
/// time is the kernel's, in clock ticks since boot.
const ALIVE: &str = r#"pid=$(cat "$d/pid" 2>/dev/null) && kill -0 "$pid" 2>/dev/null && started=$(cat "$d/started" 2>/dev/null) && [ -n "$started" ] && [ "$(sed 's/.*) //' "/proc/$pid/stat" 2>/dev/null | cut -d' ' -f20)" = "$started" ]"#;

/// The directory sandboxes live under: [`STATE_DIR_ENV`], else
/// `~/.darkmux/landlock`.
pub fn state_root() -> PathBuf {
    match std::env::var_os(STATE_DIR_ENV) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".darkmux/landlock"),
    }
}

/// Sandboxes as confined processes on this Linux host.
#[derive(Debug, Clone, Copy, Default)]
pub struct LandlockBackend;

impl LandlockBackend {
    /// Whether this host can run one, or why not. The kernel is asked once
    /// per process (two short-lived children that try the namespaces and a
    /// root built in them, about a millisecond); every later call, and
    /// [`backend`](crate::backend) calls it often, reads that answer.
    pub fn available() -> Result<(), String> {
        static REFUSAL: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        match REFUSAL.get_or_init(|| darkmux_platform::current().confine_support().refusal()) {
            None => Ok(()),
            Some(why) => Err(why.clone()),
        }
    }

    /// The rules for `policy`, whose sandbox directory is `dir`, one per
    /// line, in the order the root is built: links, then binds (the
    /// system's, `/etc`'s entries, the sandbox's `tmp`, the entrypoint, the
    /// mounts in order), then devices, the network and the working
    /// directory. `None` when a path cannot be written as one line.
    pub fn rules(policy: &SandboxPolicy, dir: &Path) -> Option<String> {
        Self::rules_with(policy, dir, Path::new("/etc"))
    }

    fn rules_with(policy: &SandboxPolicy, dir: &Path, etc: &Path) -> Option<String> {
        let mut lines = Self::system_lines()?;
        lines.extend(Self::etc_lines(etc));
        lines.push(format!("rw {}", canonical(&dir.join("tmp")).display()));
        if let Some(program) = policy.entrypoint.as_deref().filter(|e| Path::new(e).is_absolute()) {
            lines.push(format!("ro {}", canonical(Path::new(program)).display()));
        }
        for mount in &policy.mounts {
            let access = if mount.access == Access::ReadWrite { "rw" } else { "ro" };
            lines.push(format!("{access} {}", canonical(&mount.host).display()));
        }
        lines.extend(DEVICES.iter().filter(|d| Path::new(d).exists()).map(|d| format!("dev {d}")));
        match policy.network {
            Network::None => lines.push("net none".to_string()),
            Network::Default => {
                lines.push("net host".to_string());
                lines.extend(policy.ports.iter().map(|p| format!("port {p}")));
            }
        }
        if let Some(workdir) = &policy.workdir {
            lines.push(format!("workdir {workdir}"));
        }
        lines.iter().all(|l| !l.contains('\n')).then(|| lines.iter().map(|l| format!("{l}\n")).collect())
    }

    /// The system's own trees, read-only; a symlink among them (`/bin` to
    /// `usr/bin`) stays a link. `None` when a link cannot be read.
    fn system_lines() -> Option<Vec<String>> {
        let mut lines = Vec::new();
        for system in SYSTEM_READ.iter().map(Path::new) {
            match std::fs::symlink_metadata(system) {
                Ok(m) if m.file_type().is_symlink() => {
                    let target = std::fs::read_link(system).ok()?;
                    lines.push(format!("link {}\t{}", system.display(), target.display()));
                }
                Ok(_) => lines.push(format!("ro {}", system.display())),
                Err(_) => {}
            }
        }
        Some(lines)
    }

    /// `etc`'s entries, read-only and sorted, but the denied ones. A link to
    /// nowhere (a stale `/etc/resolv.conf`) grants nothing, and one into the
    /// kernel's own trees (`/etc/mtab`, the host's mount table) is not
    /// followed there.
    fn etc_lines(etc: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(etc) else { return Vec::new() };
        let mut names: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names.retain(|name| {
            let into_kernel = etc.join(name).canonicalize().is_ok_and(|real| KERNEL_TREES.iter().any(|k| real.starts_with(k)));
            !DENIED_ETC.contains(&name.as_str()) && etc.join(name).exists() && !into_kernel
        });
        names.iter().map(|name| format!("ro {}", Path::new("/etc").join(name).display())).collect()
    }

    /// `key=value`, one per line, newlines in a value dropped so one label
    /// stays one line.
    fn labels(policy: &SandboxPolicy) -> String {
        policy.labels.iter().map(|(k, v)| format!("{}={}\n", k.replace('\n', ""), v.replace('\n', ""))).collect()
    }

    /// The environment a sandbox sees: the policy's, a `PATH` when it names
    /// none, and `HOME` and `TMPDIR` in its own `tmp` when it names none.
    fn environment(policy: &SandboxPolicy, dir: &Path) -> Vec<(String, String)> {
        let mut env = policy.env.clone();
        let tmp = canonical(&dir.join("tmp")).to_string_lossy().into_owned();
        for (key, value) in [("PATH", DEFAULT_PATH.to_string()), ("HOME", tmp.clone()), ("TMPDIR", tmp)] {
            if !env.iter().any(|(k, _)| k == key) {
                env.push((key.to_string(), value));
            }
        }
        env
    }

    /// The helper and the program: this executable, the entrypoint, then
    /// the command.
    fn program(policy: &SandboxPolicy) -> Vec<String> {
        let helper = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        std::iter::once(helper).chain(policy.entrypoint.iter().cloned()).chain(policy.command.iter().cloned()).collect()
    }

    /// A shell command running `script` with `args` as `$1…`, in its own
    /// process group (so a group signal reaches the whole sandbox and not
    /// its caller), with no environment but `env`.
    fn script(script: &str, args: &[String], env: &[(String, String)]) -> Command {
        let mut cmd = Command::new(SH);
        cmd.arg("-c").arg(script).arg("sh").args(args);
        cmd.env_clear().envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        cmd
    }

    /// A lifecycle command for `handle`'s directory, or one that fails
    /// naming why when its id is no sandbox name.
    fn on_dir(handle: &SandboxHandle, script: &str, extra: &[String]) -> Command {
        match dir_of(&handle.id) {
            Some(dir) => {
                let args: Vec<String> = std::iter::once(dir.to_string_lossy().into_owned()).chain(extra.iter().cloned()).collect();
                Self::script(&format!("d=$1\n{script}"), &args, &[("PATH".into(), DEFAULT_PATH.into())])
            }
            None => refused(&handle.id),
        }
    }
}

/// The confinement `rules` describe for the sandbox whose directory is
/// `dir`, or the line that is not a rule.
fn confinement(dir: &Path, rules: &str) -> Result<Confinement, String> {
    let mut c = Confinement { root: dir.join("root"), host_network: true, ..Default::default() };
    for line in rules.lines().filter(|l| !l.is_empty()) {
        add_rule(&mut c, line)?;
    }
    Ok(c)
}

/// One rule line added to `c`, or the line when it is not a rule.
fn add_rule(c: &mut Confinement, line: &str) -> Result<(), String> {
    let (kind, rest) = line.split_once(' ').unwrap_or((line, ""));
    match kind {
        "ro" | "rw" => c.binds.push(Bind { path: PathBuf::from(rest), writable: kind == "rw" }),
        "link" => {
            let (path, target) = rest.split_once('\t').ok_or_else(|| format!("not a link: {line}"))?;
            c.links.push((PathBuf::from(path), PathBuf::from(target)));
        }
        "dev" => c.devices.push(PathBuf::from(rest)),
        "net" => c.host_network = rest == "host",
        "port" => c.bind_ports.push(rest.parse().map_err(|_| format!("not a port: {line}"))?),
        "workdir" => c.workdir = Some(PathBuf::from(rest)),
        _ => return Err(format!("not a rule: {line}")),
    }
    Ok(())
}

/// The helper, given its own arguments (the sandbox directory, the program
/// and the program's arguments): confine this process as the directory's
/// rules say and exec the program. It returns only when it cannot start,
/// with exit code 125. Run it before any thread starts.
pub fn helper(args: &[OsString]) -> std::process::ExitCode {
    match helper_plan(args) {
        Ok((c, program)) => {
            let e = darkmux_platform::confine::enter(&c, program);
            eprintln!("landlock: could not start the sandbox: {e}");
        }
        Err(why) => eprintln!("landlock: {why}"),
    }
    std::process::ExitCode::from(125)
}

/// What [`helper`] would confine and run, from its arguments: the
/// directory's rules read into a confinement, and the program with its
/// arguments; or why not, as the helper says it.
fn helper_plan(args: &[OsString]) -> Result<(darkmux_platform::confine::Confinement, &[OsString]), String> {
    let dir = args.first().map(PathBuf::from).ok_or_else(|| format!("{HELPER_ARG} needs the sandbox's directory"))?;
    let rules = dir.join("rules");
    std::fs::read_to_string(&rules)
        .map_err(|e| e.to_string())
        .and_then(|r| confinement(&dir, &r))
        .map(|c| (c, &args[1..]))
        .map_err(|why| format!("{}: {why}", rules.display()))
}

/// [`helper`], when `args` (this process's own) ask for it; it never
/// returns then. Otherwise it returns at once. For a binary without the
/// multi-call entry (a test binary, through
/// [`darkmux_platform::confine_before_main`]).
pub fn helper_if_asked(args: Vec<OsString>) {
    if args.get(1).map(|a| a.as_os_str()) != Some(HELPER_ARG.as_ref()) {
        return;
    }
    let _ = helper(&args[2..]);
    std::process::exit(125);
}

/// Removes `$d`'s sandbox, killing it first if it runs; leaves alone a
/// directory this backend did not write ([`OURS`]).
fn remove_script() -> String {
    format!("{OURS} || exit 0\nif {ALIVE}; then kill -KILL -- \"-$pid\" 2>/dev/null; fi\nrm -rf \"$d\"; exit 0")
}

/// Lists the sandboxes under `$1`, skipping any directory this backend did
/// not write ([`OURS`]).
fn list_script() -> String {
    format!(
        r#"r=$1; [ -d "$r" ] || exit 0
for d in "$r"/*; do
  {OURS} || continue
  if {ALIVE}; then s=running; elif [ -f "$d/pid" ]; then s=exited; else s=created; fi
  printf 'N\t%s\t%s\n' "${{d##*/}}" "$s"
  sed 's/^/L	/' "$d/labels" 2>/dev/null
done
exit 0"#
    )
}

/// `name`'s directory, or `None` when `name` could leave [`state_root`] (a
/// sandbox name is `[A-Za-z0-9][A-Za-z0-9_.-]*`, as Docker's are).
fn dir_of(name: &str) -> Option<PathBuf> {
    let mut chars = name.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
    let rest_ok = name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    (first_ok && rest_ok).then(|| canonical(&state_root()).join(name))
}

/// A command that fails, naming why.
fn refused_because(why: &str) -> Command {
    let mut cmd = Command::new(SH);
    cmd.arg("-c").arg(r#"echo "landlock: $1" >&2; exit 125"#).arg("sh").arg(why);
    cmd
}

/// A command that fails, naming the refused sandbox name.
fn refused(name: &str) -> Command {
    refused_because(&format!("not a sandbox name: {name}"))
}



impl SandboxBackend for LandlockBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Landlock
    }

    fn default_path(&self) -> Option<&'static str> {
        Some(DEFAULT_PATH)
    }

    /// A mount is seen where it is on the host.
    fn place(&self, mount: &Mount) -> String {
        canonical(&mount.host).to_string_lossy().into_owned()
    }

    fn run(&self, policy: &SandboxPolicy) -> (Command, SandboxHandle) {
        let handle = SandboxHandle { backend: BackendKind::Landlock, id: policy.name.clone() };
        let Some(dir) = dir_of(&policy.name) else { return (refused(&policy.name), handle) };
        let Some(rules) = Self::rules(policy, &dir) else { return (refused_because("a path holds a newline"), handle) };
        let _ = std::fs::create_dir_all(state_root());
        let mut args = vec![dir.to_string_lossy().into_owned(), rules, Self::labels(policy)];
        args.extend(Self::program(policy));
        let mut cmd = Self::script(&format!("{PREPARE}shift 3\n{LAUNCH}"), &args, &Self::environment(policy, &dir));
        if !policy.stdin {
            cmd.stdin(Stdio::null());
        }
        (cmd, handle)
    }

    fn create(&self, policy: &SandboxPolicy) -> (Command, SandboxHandle) {
        let handle = SandboxHandle { backend: BackendKind::Landlock, id: policy.name.clone() };
        let Some(dir) = dir_of(&policy.name) else { return (refused(&policy.name), handle) };
        let Some(rules) = Self::rules(policy, &dir) else { return (refused_because("a path holds a newline"), handle) };
        let _ = std::fs::create_dir_all(state_root());
        let env = Self::environment(policy, &dir);
        let launch_args: Vec<String> = std::iter::once(dir.to_string_lossy().into_owned()).chain(Self::program(policy)).collect();
        let start = format!(
            "exec /usr/bin/env -i {} {SH} -c {} sh {}\n",
            join(env.iter().map(|(k, v)| sh_quote(&format!("{k}={v}")))),
            sh_quote(&format!("d=$1; shift\n{LAUNCH}")),
            join(launch_args.iter().map(|a| sh_quote(a))),
        );
        let args = vec![dir.to_string_lossy().into_owned(), rules, Self::labels(policy), start];
        let cmd = Self::script(&format!("{PREPARE}printf '%s' \"$4\" > \"$d/start.sh\" || exit 125\n"), &args, &[("PATH".into(), DEFAULT_PATH.into())]);
        (cmd, handle)
    }

    fn start(&self, handle: &SandboxHandle) -> Command {
        let mut cmd = Self::on_dir(handle, r#"[ -f "$d/start.sh" ] || { echo "landlock: no created sandbox $d" >&2; exit 125; }
exec /bin/sh "$d/start.sh""#, &[]);
        cmd.stdin(Stdio::null());
        cmd
    }

    fn stop(&self, handle: &SandboxHandle, grace: std::time::Duration) -> Command {
        let script = format!(
            r#"{OURS} && [ -f "$d/pid" ] || exit 0
if {ALIVE}; then
  kill -TERM -- "-$pid" 2>/dev/null
  i=0; while kill -0 "$pid" 2>/dev/null && [ "$i" -lt "$(($2 * 10))" ]; do sleep 0.1; i=$((i + 1)); done
  kill -KILL -- "-$pid" 2>/dev/null
fi
rm -rf "$d"; exit 0"#
        );
        Self::on_dir(handle, &script, &[grace.as_secs().to_string()])
    }

    fn kill(&self, handle: &SandboxHandle) -> Command {
        Self::on_dir(handle, &format!("{OURS} && {ALIVE} || exit 1\nkill -KILL -- \"-$pid\" && rm -rf \"$d\""), &[])
    }

    fn remove(&self, handle: &SandboxHandle) -> Command {
        Self::on_dir(handle, &remove_script(), &[])
    }

    /// Every sandbox under [`state_root`], each as an `N<TAB>name<TAB>state`
    /// line (`running`, `exited` or `created`) and its labels as
    /// `L<TAB>key=value` lines; [`parse_list`](SandboxBackend::parse_list)
    /// filters by the query.
    fn list(&self, _query: &Query) -> Command {
        Self::script(&list_script(), &[canonical(&state_root()).to_string_lossy().into_owned()], &[("PATH".into(), DEFAULT_PATH.into())])
    }

    fn parse_list(&self, query: &Query, listing: &str) -> Vec<Listed> {
        crate::seatbelt::parse_listing(BackendKind::Landlock, query, listing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(mounts: Vec<Mount>, network: Network) -> SandboxPolicy {
        SandboxPolicy {
            name: "darkmux-dispatch-1".into(),
            image: "unused".into(),
            network,
            limits: Default::default(),
            labels: vec![("darkmux.host.pid".into(), "7".into())],
            mounts,
            ports: vec![8080],
            stdin: false,
            host_alias: None,
            user: None,
            env: vec![("A".into(), "1".into())],
            workdir: Some("/nx/ws".into()),
            entrypoint: Some("/bin/echo".into()),
            command: vec!["hi".into()],
        }
    }

    fn mount(host: &str, access: Access) -> Mount {
        Mount { host: host.into(), at: "/workspace".into(), access }
    }

    /// The rules grant the mounts in order (a read-only `.git` after the
    /// read-write tree it is layered over), the sandbox's own `tmp`, and
    /// `/etc` without its secrets; never a home.
    #[test]
    fn the_rules_layer_mounts_in_order_and_skip_etcs_secrets() {
        let etc = tempfile::TempDir::new().unwrap();
        for name in ["hosts", "shadow", "machine-id", "ssh"] {
            std::fs::write(etc.path().join(name), "x").unwrap();
        }
        std::os::unix::fs::symlink("/proc/self/mounts", etc.path().join("mtab")).unwrap();
        let p = policy(vec![mount("/nx/ws", Access::ReadWrite), mount("/nx/ws/.git", Access::ReadOnly)], Network::Default);
        let rules = LandlockBackend::rules_with(&p, Path::new("/nx/state/s"), etc.path()).unwrap();
        let rw = rules.find("rw /nx/ws\n").expect(&rules);
        let ro = rules.find("ro /nx/ws/.git\n").expect(&rules);
        assert!(rw < ro, "the read-only mount comes after the read-write one: {rules}");
        assert!(rules.contains("ro /etc/hosts\n"), "{rules}");
        for secret in ["shadow", "machine-id", "ssh", "mtab"] {
            assert!(!rules.contains(&format!("/etc/{secret}\n")), "{secret} is granted: {rules}");
        }
        assert!(rules.contains("rw /nx/state/s/tmp\n"), "{rules}");
        assert!(rules.contains("net host\nport 8080\n"), "{rules}");
        assert!(rules.ends_with("workdir /nx/ws\n"), "{rules}");
        assert!(!rules.contains("/home"), "no home is granted: {rules}");
    }

    /// No network grants no port, and a path that would split a rule in
    /// two is refused.
    #[test]
    fn no_network_grants_no_port_and_a_newline_is_refused() {
        let rules = LandlockBackend::rules(&policy(Vec::new(), Network::None), Path::new("/nx/s")).unwrap();
        assert!(rules.contains("net none\n") && !rules.contains("port "), "{rules}");
        let bad = policy(vec![mount("/nx/a\nrw /", Access::ReadOnly)], Network::None);
        assert_eq!(LandlockBackend::rules(&bad, Path::new("/nx/s")), None);
    }

    /// The helper reads back what the rules say, and refuses a line that is
    /// not one.
    #[test]
    fn the_helper_reads_the_rules_back() {
        let c = confinement(Path::new("/s"), "link /bin\tusr/bin\nro /usr\nrw /w\ndev /dev/null\nnet none\nworkdir /w\n").unwrap();
        assert_eq!(c.root, PathBuf::from("/s/root"));
        assert_eq!(c.links, [(PathBuf::from("/bin"), PathBuf::from("usr/bin"))]);
        assert_eq!(c.binds, [Bind { path: "/usr".into(), writable: false }, Bind { path: "/w".into(), writable: true }]);
        assert!(!c.host_network);
        assert_eq!(c.workdir, Some(PathBuf::from("/w")));
        assert!(confinement(Path::new("/s"), "mount /x\n").is_err());
    }

    /// A rules line naming a port or a link it cannot read is refused,
    /// naming the line; a port and the host's network read back.
    #[test]
    fn a_rule_it_cannot_read_is_refused_naming_it() {
        let c = confinement(Path::new("/s"), "net host\nport 8080\n").unwrap();
        assert!(c.host_network);
        assert_eq!(c.bind_ports, [8080]);
        assert_eq!(confinement(Path::new("/s"), "port http\n").unwrap_err(), "not a port: port http");
        assert_eq!(confinement(Path::new("/s"), "link /bin\n").unwrap_err(), "not a link: link /bin");
    }

    /// A program's environment is the policy's, a PATH, a HOME and a
    /// TMPDIR (the sandbox's own writable tmp) added only where it set none.
    #[test]
    fn the_environment_adds_path_home_and_tmpdir_only_where_unset() {
        let dir = tempfile::TempDir::new().unwrap();
        let tmp = canonical(&dir.path().join("tmp")).to_string_lossy().into_owned();
        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        let mut p = policy(Vec::new(), Network::None);
        assert_eq!(
            LandlockBackend::environment(&p, dir.path()),
            [pair("A", "1"), pair("PATH", DEFAULT_PATH), pair("HOME", &tmp), pair("TMPDIR", &tmp)]
        );
        p.env = vec![pair("PATH", "/mine"), pair("HOME", "/h")];
        assert_eq!(LandlockBackend::environment(&p, dir.path()), [pair("PATH", "/mine"), pair("HOME", "/h"), pair("TMPDIR", &tmp)]);
    }

    /// The helper exits 125 when it cannot start a sandbox, and says why: no
    /// directory, no rules in it, a line that is not a rule. A rules file it
    /// can read becomes the confinement, with the program passed on (#3575
    /// review: an exit code alone could not tell these apart).
    #[test]
    fn the_helper_refuses_what_it_cannot_start() {
        let refused = |args: &[OsString]| format!("{:?}", helper(args)) == format!("{:?}", std::process::ExitCode::from(125));
        let why = |args: &[OsString]| helper_plan(args).err().unwrap_or_default();
        let dir = tempfile::TempDir::new().unwrap();
        let at = || dir.path().as_os_str().to_owned();
        assert!(refused(&[]));
        assert!(why(&[]).contains("needs the sandbox's directory"), "{}", why(&[]));
        assert!(refused(&[at()]), "no rules");
        assert!(why(&[at()]).contains("rules"), "{}", why(&[at()]));
        std::fs::write(dir.path().join("rules"), "mount /x\n").unwrap();
        assert!(refused(&[at()]), "not a rule");
        assert!(why(&[at()]).contains("mount /x"), "{}", why(&[at()]));
        std::fs::write(dir.path().join("rules"), "net none\n").unwrap();
        assert!(refused(&[at()]), "nothing to run");
        let args = [at(), OsString::from("sh"), OsString::from("-c")];
        let (_, program) = helper_plan(&args).expect("a rules file it can read");
        assert_eq!(program, &args[1..]);
    }

    /// Arguments that do not ask for the helper return at once.
    #[test]
    fn other_arguments_do_not_start_the_helper() {
        helper_if_asked(vec!["darkmux".into(), "dispatch".into()]);
        helper_if_asked(Vec::new());
    }

    /// A kill, a stop and a remove each find a sandbox only by its
    /// launcher's own pid and start time: a recorded pid now held by another
    /// live process (reused, or another sandbox's launcher), a group leader
    /// like a launcher, is never signaled.
    #[test]
    #[serial_test::serial]
    fn kill_stop_and_remove_never_signal_a_process_that_reused_the_pid() {
        use std::os::unix::process::CommandExt;
        let root = tempfile::TempDir::new().unwrap();
        let prev = std::env::var_os(STATE_DIR_ENV);
        unsafe { std::env::set_var(STATE_DIR_ENV, root.path()) };
        let backend = LandlockBackend;
        type Plan = fn(&LandlockBackend, &SandboxHandle) -> Command;
        let lifecycle: [(&str, Plan); 3] = [
            ("kill", |b, h| b.kill(h)),
            ("stop", |b, h| b.stop(h, std::time::Duration::ZERO)),
            ("remove", |b, h| b.remove(h)),
        ];
        let mut signaled = Vec::new();
        for (verb, command) in lifecycle {
            let name = format!("darkmux-reused-{verb}");
            let d = root.path().join(&name);
            std::fs::create_dir(&d).unwrap();
            std::fs::write(d.join("rules"), "net none\n").unwrap();
            let mut other = Command::new("/bin/sleep").arg("30").process_group(0).spawn().unwrap();
            std::fs::write(d.join("pid"), other.id().to_string()).unwrap();
            std::fs::write(d.join("started"), "1\n").unwrap();
            let handle = SandboxHandle { backend: BackendKind::Landlock, id: name };
            let _ = command(&backend, &handle).status().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(100));
            if other.try_wait().unwrap().is_some() {
                signaled.push(verb);
            }
            let _ = other.kill();
            let _ = other.wait();
        }
        unsafe {
            match prev {
                Some(v) => std::env::set_var(STATE_DIR_ENV, v),
                None => std::env::remove_var(STATE_DIR_ENV),
            }
        }
        assert!(signaled.is_empty(), "signaled a process that reused the pid: {signaled:?}");
    }

    /// A process's recorded start time matches itself: the check above
    /// fails for a reused pid, not for every pid. The start time is read
    /// from `/proc`, so a host without one has nothing to check.
    #[test]
    fn the_start_time_check_knows_a_live_launcher() {
        if !Path::new("/proc/self/stat").exists() {
            return;
        }
        let root = tempfile::TempDir::new().unwrap();
        let d = root.path().join("darkmux-pr-2");
        std::fs::create_dir(&d).unwrap();
        std::fs::write(d.join("rules"), "net none\n").unwrap();
        let script = format!("d=$1\necho $$ > \"$d/pid\" && sed 's/.*) //' /proc/$$/stat | cut -d' ' -f20 > \"$d/started\"\n{ALIVE} && echo alive");
        let out = Command::new(SH).arg("-c").arg(script).arg("sh").arg(&d).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "alive");
    }

    /// A directory under the state root that this backend did not write is
    /// never listed and never deleted by a remove.
    #[test]
    fn a_directory_it_did_not_write_is_never_listed_or_removed() {
        let root = tempfile::TempDir::new().unwrap();
        let (ours, theirs) = (root.path().join("darkmux-x"), root.path().join("quick-coding"));
        std::fs::create_dir_all(&ours).unwrap();
        std::fs::write(ours.join("rules"), "net none\n").unwrap();
        std::fs::write(ours.join("labels"), "k=v\n").unwrap();
        std::fs::create_dir_all(theirs.join("src")).unwrap();
        let sh = |script: String, arg: &Path| Command::new(SH).arg("-c").arg(script).arg("sh").arg(arg).output().unwrap();
        let listed = String::from_utf8_lossy(&sh(list_script(), root.path()).stdout).into_owned();
        assert_eq!(listed, "N\tdarkmux-x\tcreated\nL\tk=v\n");
        sh(format!("d=$1\n{}", remove_script()), &theirs);
        assert!(theirs.join("src").is_dir(), "a remove deleted a directory it did not write");
        sh(format!("d=$1\n{}", remove_script()), &ours);
        assert!(!ours.exists(), "a remove left its own sandbox");
    }

    /// A name that could leave the state directory is refused.
    #[test]
    fn a_name_that_leaves_the_state_dir_is_refused() {
        for bad in ["../x", "a/b", "", ".x", "-x"] {
            assert!(dir_of(bad).is_none(), "{bad}");
        }
        assert!(dir_of("darkmux-dispatch-a.b_c-1").is_some());
    }
}
