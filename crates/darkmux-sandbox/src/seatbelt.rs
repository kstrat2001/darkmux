//! (#3112 step 2, spike) The Seatbelt backend: a policy as a macOS
//! `sandbox-exec` profile around a process on the host itself. No daemon,
//! no image, no virtual machine.
//!
//! What it cannot do, against Docker:
//! - **Place a mount elsewhere.** Seatbelt allows or denies a host path
//!   where it is, so a mount is seen at its own (canonical) host path and
//!   [`SandboxBackend::place`] says so. A caller builds every in-sandbox path
//!   from `place`, never from `Mount.at`.
//! - **Hide the host's toolchain or OS.** There is no image: the process
//!   runs this host's programs, read-only, and `policy.image` is unused. It
//!   needs an `entrypoint` (a host program), since there is no image to
//!   supply one.
//! - **Limit memory or processes, or switch user.** `limits` and `user` are
//!   unused; the process runs as the caller's user.
//! - **Publish one port, or hold a bind to the loopback (#3559).** The
//!   default network may bind any port on ANY local address: `0.0.0.0` and
//!   this host's LAN address included, so a server that binds every address
//!   is reachable from the LAN. SBPL's address filter (`local ip`) takes only
//!   `*` or `localhost`, and for a bind `localhost` matches every local
//!   address (measured on macOS 26.6), so the profile can allow binding or
//!   deny it, never tell loopback from the rest. It can hold binds to PORTS:
//!   a policy naming `ports` may bind only those numbers, every other bind
//!   refused by the kernel. A caller that needs loopback-only listeners
//!   watches the address itself (client previews do).
//!   [`Network::None`] allows no network at all, the loopback included.
//! - **Reach every descendant.** A sandbox is the process group its launcher
//!   leads, killed over and over until it is empty (a process that keeps
//!   forking is caught). A descendant that leaves the group (`setsid`, or
//!   `setpgid` to a group of its own, which job control does by itself)
//!   outlives every kill, still inside the sandbox: it keeps its file and
//!   network rules (no bind but the named ports) and nothing ends it.
//!
//! Each sandbox has a directory under [`state_root`] while it exists: its
//! profile, its labels, its launcher's pid once started, its own `tmp`
//! (the only host path it may write outside its mounts, and its `HOME` and
//! `TMPDIR`), and, for one created but not started, its start script. The
//! launcher removes the directory when the sandbox exits.

use crate::{Access, BackendKind, Having, Listed, Mount, Network, Query, SandboxBackend, SandboxHandle, SandboxPolicy};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The program that applies a profile and runs a command under it.
pub const SANDBOX_EXEC: &str = darkmux_platform::SANDBOX_EXEC;

/// The shell every planned command runs: the launcher, and the lifecycle
/// scripts that read a sandbox's directory.
const SH: &str = "/bin/sh";

/// `PATH` inside a sandbox whose policy names none: Homebrew's programs,
/// then the system's (whose `python3` and `git` are `xcrun` shims).
const DEFAULT_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// Env override of [`state_root`], for tests.
pub const STATE_DIR_ENV: &str = "DARKMUX_SANDBOX_STATE_DIR";

/// Host paths every sandbox may read: the OS, its libraries and tools, and
/// Homebrew's. Never the user's home.
const SYSTEM_READ: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/System",
    "/Library",
    "/private/etc",
    "/private/var/db/timezone",
    // Which developer directory `xcrun`'s shims (`/usr/bin/python3`, `git`) use.
    "/private/var/db/xcode_select_link",
    "/private/var/select",
    "/dev",
    // Homebrew's programs and libraries, never its `var` (databases' data)
    // or the rest of its `etc` (#3553 review).
    "/opt/homebrew/bin",
    "/opt/homebrew/sbin",
    "/opt/homebrew/lib",
    "/opt/homebrew/opt",
    "/opt/homebrew/Cellar",
    "/opt/homebrew/Frameworks",
    "/opt/homebrew/include",
    "/opt/homebrew/share",
    "/opt/homebrew/etc/openssl@3",
    "/opt/homebrew/etc/ca-certificates",
    "/Applications/Xcode.app",
];

/// Paths under [`SYSTEM_READ`] that hold data, not programs: never readable.
const DENIED_READ: &[&str] = &["/usr/local/var", "/usr/local/etc"];

/// Mach services a sandbox may not look up: the keychain's and the
/// clipboard's (#3553 review). Everything else
/// a program needs (name resolution, certificate trust) stays reachable.
const DENIED_SERVICES: &[&str] =
    &["com.apple.SecurityServer", "com.apple.securityd.xpc", "com.apple.secd", "com.apple.pasteboard.1"];

/// Writes the sandbox's directory: `$1` the directory, `$2` the profile,
/// `$3` the labels. Fails (125, Docker's own "could not make it") when the
/// name is taken.
const PREPARE: &str = r#"d=$1
mkdir -m 700 "$d" 2>/dev/null || { echo "seatbelt: sandbox $d exists" >&2; exit 125; }
mkdir -m 700 "$d/tmp" && printf '%s' "$2" > "$d/profile.sb" && printf '%s' "$3" > "$d/labels" || exit 125
"#;

/// Runs the sandbox attached: `$d` set, `$1` the working directory (empty
/// for none), then the program and its arguments. Records its own pid (the
/// group's), survives the group's SIGTERM long enough to see the sandbox
/// out, and removes the directory when it exits (with [`LAUNCHED`]:
/// [`launcher`]). When its program ends, whatever the program left running
/// in its process group is killed, until none is left (#3112).
/// A process that moved to a group of its own (`setsid`, `setpgid`) is not
/// reached: unlike a container's, this sandbox does not end everything in it.
const LAUNCH: &str = r#"wd=$1; shift
echo $$ > "$d/pid" && ps -o lstart= -p $$ > "$d/started" || exit 125
trap : TERM INT HUP
[ -z "$wd" ] || cd "$wd" || { rm -rf "$d"; exit 125; }
[ -n "$1" ] || { echo "seatbelt: no program to run (a policy needs an entrypoint)" >&2; rm -rf "$d"; exit 125; }
/usr/bin/sandbox-exec -f "$d/profile.sb" "$@"
rc=$?
g=$$
"#;

/// Ends every process in group `$g` from outside it (stop, remove and kill
/// run in a group of their own): the whole group is killed at once, again
/// and again until no process is left in it, at most 1000 times. One kill
/// of a group reaches every member at that instant, so a process that keeps
/// forking a new pid (a fork-hopper) is caught where a list of pids, always
/// a step behind, is not. A process that moved to a group of its own is
/// not in it.
const DRAIN: &str = r#"n=0
while [ "$n" -lt 1000 ] && kill -0 -- "-$g" 2>/dev/null; do
  kill -KILL -- "-$g" 2>/dev/null
  n=$((n + 1))
done
"#;

/// The launcher's end. It leads the group it must empty, so it cannot kill
/// the group itself: it starts a helper in a group of its own that waits
/// for the launcher to exit and then empties the group as [`DRAIN`] does,
/// removes the directory, and exits with the program's status. Whatever the
/// program left running in its group ends a few milliseconds after the
/// launcher does.
const LAUNCHED: &str = r#"/usr/bin/perl -e 'setpgrp(0, 0); my ($l, $g) = @ARGV; select(undef, undef, undef, 0.005) while kill(0, $l); for (1 .. 1000) { last unless kill(0, -$g); kill("KILL", -$g); }' "$$" "$g" </dev/null >/dev/null 2>&1 &
rm -rf "$d"
exit $rc
"#;

/// The whole launcher script.
fn launcher() -> String {
    format!("{LAUNCH}{LAUNCHED}")
}

/// Whether `$d` is a sandbox's directory at all: only a directory holding
/// the profile this backend wrote is ever listed, stopped or deleted.
const OURS: &str = r#"[ -f "$d/profile.sb" ]"#;

/// Whether `$d`'s sandbox is running: its launcher's pid is alive and is
/// still that launcher, started when the launcher recorded (a reused pid,
/// or another sandbox's launcher, is not), leaving `$pid` set.
const ALIVE: &str = r#"pid=$(cat "$d/pid" 2>/dev/null) && kill -0 "$pid" 2>/dev/null && started=$(cat "$d/started" 2>/dev/null) && [ -n "$started" ] && [ "$(ps -o lstart= -p "$pid" 2>/dev/null)" = "$started" ]"#;

/// The directory sandboxes live under: [`STATE_DIR_ENV`], else
/// `~/.darkmux/seatbelt` (not `~/.darkmux/sandboxes`, which holds the lab's
/// workload trees).
pub fn state_root() -> PathBuf {
    match std::env::var_os(STATE_DIR_ENV) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".darkmux/seatbelt"),
    }
}

/// Sandboxes as `sandbox-exec` processes on this host.
#[derive(Debug, Clone, Copy, Default)]
pub struct SeatbeltBackend;

impl SeatbeltBackend {
    /// Whether this host can run one: `sandbox-exec` is installed.
    pub fn available() -> bool {
        Path::new(SANDBOX_EXEC).is_file()
    }

    /// The profile for `policy`, whose sandbox directory is `dir`. Denies
    /// everything, then allows: running programs, the system's read-only
    /// paths, its own `tmp`, its mounts in order (a later read-only mount
    /// over an earlier read-write one denies writes beneath it; Seatbelt's
    /// later rule wins), and its network.
    pub fn profile(policy: &SandboxPolicy, dir: &Path) -> String {
        let mut p = String::from("(version 1)\n(deny default)\n");
        p.push_str("(allow process-fork)\n(allow process-exec)\n");
        p.push_str("(allow signal (target same-sandbox))\n(allow process-info* (target same-sandbox))\n");
        p.push_str("(allow sysctl-read)\n(allow ipc-posix-shm)\n(allow ipc-posix-sem)\n(allow pseudo-tty)\n(allow file-ioctl)\n");
        p.push_str("(allow mach-lookup)\n");
        p.push_str(&format!("(deny mach-lookup {})\n", join(DENIED_SERVICES.iter().map(|s| format!("(global-name {})", quote(s))))));
        p.push_str(&format!("(allow file-read-metadata {})\n", join(Self::ancestors(policy, dir).iter().map(|a| format!("(literal {})", quote(&a.to_string_lossy()))))));
        let system = std::iter::once("(literal \"/\")".to_string()).chain(SYSTEM_READ.iter().map(|s| subpath(Path::new(s))));
        p.push_str(&format!("(allow file-read* {})\n", join(system)));
        p.push_str(&format!("(deny file-read* {})\n", join(DENIED_READ.iter().map(|s| subpath(Path::new(s))))));
        // Its own controlling terminal only: never another of the user's
        // terminals, to read keystrokes from or forge output on.
        p.push_str("(deny file-read* file-write* (regex #\"^/dev/ttys\"))\n");
        p.push_str("(allow file-write* (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/tty\") (regex #\"^/dev/fd/\"))\n");
        p.push_str(&format!("(allow file-read* file-write* {})\n", subpath(&canonical(&dir.join("tmp")))));
        for mount in &policy.mounts {
            let at = subpath(&canonical(&mount.host));
            match mount.access {
                Access::ReadWrite => p.push_str(&format!("(allow file-read* file-write* {at})\n")),
                Access::ReadOnly => p.push_str(&format!("(allow file-read* {at})\n(deny file-write* {at})\n")),
            }
        }
        // Seatbelt matches paths, not mounts: a read-only mount nested deep in
        // a read-write one could be moved out from under its rule by renaming
        // a directory above it. Each directory between them is fixed in place
        // (its contents stay writable).
        for pinned in Self::pinned_ancestors(policy) {
            p.push_str(&format!("(deny file-write* (literal {}))\n", quote(&pinned.to_string_lossy())));
        }
        if let Some(program) = &policy.entrypoint {
            if Path::new(program).is_absolute() {
                p.push_str(&format!("(allow file-read* (literal {}))\n", quote(&canonical(Path::new(program)).to_string_lossy())));
            }
        }
        match policy.network {
            // Any address. The bind rule names `localhost`, but Seatbelt
            // matches it against every local address, so `0.0.0.0` and the
            // LAN address bind too (#3559): it holds no bind to the
            // loopback. No unix socket but the resolver's, so never the
            // Docker daemon's or another service's (#3553 review).
            Network::Default => {
                p.push_str("(allow network*)\n(deny network-bind)\n");
                p.push_str(&Self::bind_rule(&policy.ports));
                p.push_str(concat!(
                    "(deny network-outbound (remote unix-socket))\n",
                    "(allow network-outbound (remote unix-socket (path-literal \"/private/var/run/mDNSResponder\")))\n",
                ));
            }
            // Nothing: a sandbox has no loopback of its own, and the host's
            // runs its databases and model servers (#3553 review).
            Network::None => {}
        }
        p
    }

    /// What the default network may bind: with `ports` named, only those
    /// port numbers (on any local address: SBPL's `localhost` matches every
    /// one, #3559), every other bind refused by the kernel; with none named,
    /// any port.
    fn bind_rule(ports: &[u16]) -> String {
        if ports.is_empty() {
            return "(allow network-bind (local ip \"localhost:*\"))\n".to_string();
        }
        format!("(allow network-bind {})\n", join(ports.iter().map(|p| format!("(local ip \"localhost:{p}\")"))))
    }

    /// The directories above every path the sandbox may reach, and the
    /// root's links into `/private`: a program resolving a path stats each
    /// directory on the way down. Nothing else outside those paths can even
    /// be seen to exist.
    fn ancestors(policy: &SandboxPolicy, dir: &Path) -> Vec<PathBuf> {
        let reachable = policy
            .mounts
            .iter()
            .map(|m| canonical(&m.host))
            .chain(std::iter::once(canonical(&dir.join("tmp"))))
            .chain(policy.entrypoint.iter().filter(|e| Path::new(e).is_absolute()).map(|e| canonical(Path::new(e))))
            .chain(SYSTEM_READ.iter().map(PathBuf::from));
        let mut all: Vec<PathBuf> = ["/tmp", "/var", "/etc"].iter().map(PathBuf::from).collect();
        for path in reachable {
            all.extend(path.ancestors().skip(1).map(Path::to_path_buf));
        }
        all.sort();
        all.dedup();
        all
    }

    /// The directories strictly between a read-only mount and the
    /// read-write mount it is nested in.
    fn pinned_ancestors(policy: &SandboxPolicy) -> Vec<PathBuf> {
        let rw: Vec<PathBuf> = policy.mounts.iter().filter(|m| m.access == Access::ReadWrite).map(|m| canonical(&m.host)).collect();
        let mut pinned = Vec::new();
        for ro in policy.mounts.iter().filter(|m| m.access == Access::ReadOnly).map(|m| canonical(&m.host)) {
            for outer in rw.iter().filter(|outer| ro.starts_with(outer) && ro != **outer) {
                pinned.extend(ro.ancestors().skip(1).take_while(|a| a != outer).map(Path::to_path_buf));
            }
        }
        pinned.sort();
        pinned.dedup();
        pinned
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

    /// The program and its arguments: the entrypoint, then the command.
    fn program(policy: &SandboxPolicy) -> Vec<String> {
        policy.entrypoint.iter().cloned().chain(policy.command.iter().cloned()).collect()
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

/// Removes `$d`'s sandbox, killing it first if it runs; leaves alone a
/// directory this backend did not write ([`OURS`]).
fn remove_script() -> String {
    format!("{OURS} || exit 0\nif {ALIVE}; then kill -KILL -- \"-$pid\" 2>/dev/null; g=$pid\n{DRAIN}fi\nrm -rf \"$d\"; exit 0")
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

/// A command that fails, naming the refused sandbox name.
fn refused(name: &str) -> Command {
    let mut cmd = Command::new(SH);
    cmd.arg("-c").arg(r#"echo "seatbelt: not a sandbox name: $1" >&2; exit 125"#).arg("sh").arg(name);
    cmd
}

/// `path` with its symlinks resolved (Seatbelt matches real paths, and
/// `/tmp` and `/var` are links); as given when it cannot be resolved. A
/// path whose own last part does not exist yet resolves its parent.
fn canonical(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if parent != path => canonical(parent).join(name),
        _ => path.to_path_buf(),
    }
}

/// A string as an SBPL string literal.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn subpath(path: &Path) -> String {
    format!("(subpath {})", quote(&path.to_string_lossy()))
}

fn join(parts: impl Iterator<Item = String>) -> String {
    parts.collect::<Vec<_>>().join(" ")
}

/// One line of the start script: each argument single-quoted for `sh`.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

impl SandboxBackend for SeatbeltBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Seatbelt
    }

    /// A mount is seen where it is on the host.
    fn place(&self, mount: &Mount) -> String {
        canonical(&mount.host).to_string_lossy().into_owned()
    }

    /// Under [`SYSTEM_READ`] and outside [`DENIED_READ`], where its links
    /// lead.
    fn host_reads(&self, path: &Path) -> bool {
        let real = canonical(path);
        SYSTEM_READ.iter().any(|s| real.starts_with(s)) && !DENIED_READ.iter().any(|s| real.starts_with(s))
    }

    fn run(&self, policy: &SandboxPolicy) -> (Command, SandboxHandle) {
        let handle = SandboxHandle { backend: BackendKind::Seatbelt, id: policy.name.clone() };
        let Some(dir) = dir_of(&policy.name) else { return (refused(&policy.name), handle) };
        let _ = std::fs::create_dir_all(state_root());
        let mut args = vec![
            dir.to_string_lossy().into_owned(),
            Self::profile(policy, &dir),
            Self::labels(policy),
            policy.workdir.clone().unwrap_or_default(),
        ];
        args.extend(Self::program(policy));
        let mut cmd = Self::script(&format!("{PREPARE}shift 3\n{}", launcher()), &args, &Self::environment(policy, &dir));
        if !policy.stdin {
            cmd.stdin(Stdio::null());
        }
        (cmd, handle)
    }

    fn create(&self, policy: &SandboxPolicy) -> (Command, SandboxHandle) {
        let handle = SandboxHandle { backend: BackendKind::Seatbelt, id: policy.name.clone() };
        let Some(dir) = dir_of(&policy.name) else { return (refused(&policy.name), handle) };
        let _ = std::fs::create_dir_all(state_root());
        let env = Self::environment(policy, &dir);
        let launch_args: Vec<String> = [dir.to_string_lossy().into_owned(), policy.workdir.clone().unwrap_or_default()]
            .into_iter()
            .chain(Self::program(policy))
            .collect();
        let start = format!(
            "exec /usr/bin/env -i {} {SH} -c {} sh {}\n",
            join(env.iter().map(|(k, v)| sh_quote(&format!("{k}={v}")))),
            sh_quote(&format!("d=$1; shift\n{}", launcher())),
            join(launch_args.iter().map(|a| sh_quote(a))),
        );
        let args = vec![dir.to_string_lossy().into_owned(), Self::profile(policy, &dir), Self::labels(policy), start];
        let cmd = Self::script(&format!("{PREPARE}printf '%s' \"$4\" > \"$d/start.sh\" || exit 125\n"), &args, &[("PATH".into(), DEFAULT_PATH.into())]);
        (cmd, handle)
    }

    fn start(&self, handle: &SandboxHandle) -> Command {
        let mut cmd = Self::on_dir(handle, r#"[ -f "$d/start.sh" ] || { echo "seatbelt: no created sandbox $d" >&2; exit 125; }
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
  g=$pid
{DRAIN}fi
rm -rf "$d"; exit 0"#
        );
        Self::on_dir(handle, &script, &[grace.as_secs().to_string()])
    }

    fn kill(&self, handle: &SandboxHandle) -> Command {
        Self::on_dir(handle, &format!("{OURS} && {ALIVE} || exit 1\nkill -KILL -- \"-$pid\" || exit 1\ng=$pid\n{DRAIN}rm -rf \"$d\""), &[])
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
        // Each sandbox: its name, whether it runs, and its labels.
        type Found = (String, bool, Vec<(String, String)>);
        let mut found: Vec<Found> = Vec::new();
        for line in listing.lines() {
            if let Some(rest) = line.strip_prefix("N\t") {
                let (name, state) = rest.split_once('\t').unwrap_or((rest, ""));
                found.push((name.to_string(), state == "running", Vec::new()));
            } else if let (Some(kv), Some(last)) = (line.strip_prefix("L\t"), found.last_mut()) {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                last.2.push((k.to_string(), v.to_string()));
            }
        }
        found
            .into_iter()
            .filter(|(_, running, _)| *running || query.stopped_too)
            .filter(|(name, _, labels)| match &query.having {
                Having::Label(key) => labels.iter().any(|(k, _)| k == key),
                Having::NamePrefix(prefix) => name.starts_with(prefix.as_str()),
                Having::Name(exact) => name == exact,
            })
            .map(|(name, _, labels)| Listed {
                handle: SandboxHandle { backend: BackendKind::Seatbelt, id: name },
                labels: query.read.iter().map(|key| labels.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())).collect(),
            })
            .collect()
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
            ports: Vec::new(),
            stdin: false,
            host_alias: None,
            user: None,
            env: vec![("A".into(), "1".into())],
            workdir: None,
            entrypoint: Some("/bin/echo".into()),
            command: vec!["hi".into()],
        }
    }

    fn mount(host: &str, access: Access) -> Mount {
        Mount { host: host.into(), at: "/workspace".into(), access }
    }

    /// A read-only mount after a read-write one denies writes beneath it,
    /// in order, and the default network's bind rule is the one written
    /// (which, #3559, Seatbelt applies to every local address: the
    /// conformance suite's `a_server_on_every_address_reaches_only_what_the_backend_allows`
    /// records what it really allows).
    #[test]
    fn the_profile_layers_mounts_in_order_and_writes_the_bind_rule() {
        let p = SeatbeltBackend::profile(
            &policy(vec![mount("/nonexistent-ws", Access::ReadWrite), mount("/nonexistent-ws/.git", Access::ReadOnly)], Network::Default),
            Path::new("/nonexistent-state/s"),
        );
        let rw = p.find("(allow file-read* file-write* (subpath \"/nonexistent-ws\"))").expect(&p);
        let ro = p.find("(deny file-write* (subpath \"/nonexistent-ws/.git\"))").expect(&p);
        assert!(rw < ro, "the read-only mount comes after the read-write one: {p}");
        assert!(p.starts_with("(version 1)\n(deny default)\n"), "{p}");
        assert!(p.contains("(deny network-bind)\n(allow network-bind (local ip \"localhost:*\"))"), "{p}");
        assert!(p.contains("(deny mach-lookup (global-name \"com.apple.SecurityServer\")"), "{p}");
        assert!(p.contains("(global-name \"com.apple.pasteboard.1\")"), "the clipboard is out of reach: {p}");
        assert!(p.contains("(deny file-read* file-write* (regex #\"^/dev/ttys\"))"), "no other terminal: {p}");
        assert!(!p.contains("(subpath \"/opt/homebrew\")"), "not all of Homebrew, its var included: {p}");
        assert!(!p.contains("/Users"), "no home is readable: {p}");
    }

    /// (#3112, #3559) A policy naming ports may bind only those numbers;
    /// one naming none, any. The address cannot be held either way.
    #[test]
    fn named_ports_are_the_only_ones_the_profile_lets_bind() {
        let mut p = policy(Vec::new(), Network::Default);
        p.ports = vec![4100, 4200];
        let named = SeatbeltBackend::profile(&p, Path::new("/s"));
        assert!(named.contains("(deny network-bind)\n(allow network-bind (local ip \"localhost:4100\") (local ip \"localhost:4200\"))\n"), "{named}");
        assert!(!named.contains("localhost:*"), "{named}");
        let any = SeatbeltBackend::profile(&policy(Vec::new(), Network::Default), Path::new("/s"));
        assert!(any.contains("(allow network-bind (local ip \"localhost:*\"))"), "{any}");
    }

    /// No network allows no network rule at all, the host's loopback
    /// included; the default network reaches no unix socket but the
    /// resolver's.
    #[test]
    fn no_network_allows_nothing_and_the_default_no_unix_socket() {
        let none = SeatbeltBackend::profile(&policy(Vec::new(), Network::None), Path::new("/s"));
        assert!(!none.contains("network"), "{none}");
        let default = SeatbeltBackend::profile(&policy(Vec::new(), Network::Default), Path::new("/s"));
        assert!(default.contains("(deny network-outbound (remote unix-socket))\n(allow network-outbound (remote unix-socket (path-literal \"/private/var/run/mDNSResponder\")))"), "{default}");
    }

    /// The directories between a read-only mount and the read-write one
    /// it is nested in are fixed in place; none above the read-write one.
    #[test]
    fn a_deep_read_only_mount_pins_the_directories_above_it() {
        let p = policy(vec![mount("/nx/ws", Access::ReadWrite), mount("/nx/ws/a/b/.git", Access::ReadOnly)], Network::None);
        assert_eq!(SeatbeltBackend::pinned_ancestors(&p), [PathBuf::from("/nx/ws/a"), PathBuf::from("/nx/ws/a/b")]);
        let shallow = policy(vec![mount("/nx/ws", Access::ReadWrite), mount("/nx/ws/.git", Access::ReadOnly)], Network::None);
        assert!(SeatbeltBackend::pinned_ancestors(&shallow).is_empty());
    }

    /// A kill finds a sandbox only by its launcher's own pid and start
    /// time: a recorded pid now held by another process (reused, or another
    /// sandbox's launcher) is not killed.
    #[test]
    fn a_kill_never_signals_a_process_that_reused_the_pid() {
        let root = tempfile::TempDir::new().unwrap();
        let d = root.path().join("darkmux-pr-1");
        std::fs::create_dir(&d).unwrap();
        std::fs::write(d.join("profile.sb"), "(version 1)").unwrap();
        let mut other = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        std::fs::write(d.join("pid"), other.id().to_string()).unwrap();
        std::fs::write(d.join("started"), "Thu Jan  1 00:00:00 1970\n").unwrap();
        let kill = Command::new(SH).arg("-c").arg(format!("d=$1\n{OURS} && {ALIVE} || exit 1\nkill -KILL -- \"$pid\"")).arg("sh").arg(&d).status().unwrap();
        let survived = other.try_wait().unwrap().is_none();
        let _ = other.kill();
        let _ = other.wait();
        assert!(!kill.success(), "a kill claimed a process it did not start");
        assert!(survived, "a kill signaled a process that reused the pid");
    }

    /// (#3112) What a sandbox reads with no mount: the OS and the system
    /// toolchain (Homebrew's programs), never a home, never Homebrew's or
    /// `/usr/local`'s data; a path is judged where its links lead.
    #[test]
    fn it_reads_the_system_and_never_a_home() {
        for yes in ["/usr/bin/env", "/bin/sh", "/opt/homebrew/bin", "/opt/homebrew/Cellar/node", "/usr/local/bin"] {
            assert!(SeatbeltBackend.host_reads(Path::new(yes)), "{yes}");
        }
        for no in ["/Users/someone/.nvm/versions/node/v24/bin", "/opt/homebrew/var", "/opt/homebrew", "/usr/local/var/db", "/usr/local/etc", "/opt/local/bin", "/"] {
            assert!(!SeatbeltBackend.host_reads(Path::new(no)), "{no}");
        }
        let home = tempfile::TempDir::new().unwrap();
        let link = home.path().join("to-usr");
        std::os::unix::fs::symlink("/usr/bin", &link).unwrap();
        assert!(SeatbeltBackend.host_reads(&link), "a link into the system reads where it leads");
        assert!(!crate::DockerBackend.host_reads(Path::new("/usr/bin")), "a container reads only its image and mounts");
    }

    /// A quote or backslash in a path cannot end the SBPL string.
    #[test]
    fn a_path_cannot_break_out_of_its_string() {
        assert_eq!(quote(r#"/a"b\c"#), r#""/a\"b\\c""#);
    }

    /// A name that could leave the state directory is refused.
    #[test]
    fn a_name_that_leaves_the_state_dir_is_refused() {
        for bad in ["../x", "a/b", "", ".x", "-x"] {
            assert!(dir_of(bad).is_none(), "{bad}");
        }
        assert!(dir_of("darkmux-dispatch-a.b_c-1").is_some());
    }

    /// A listing keeps what the query asks for: running only unless stopped
    /// ones too, by label, literal prefix or exact name, with the labels it
    /// reads in order.
    #[test]
    fn a_listing_filters_by_the_query() {
        let listing = "N\tdarkmux-a.b\trunning\nL\tk=1\nL\tj=2\nN\tdarkmux-aXb\texited\nL\tk=3\nN\tother\tcreated\n";
        let q = |having, stopped_too| Query { having, stopped_too, read: vec!["j".into(), "k".into()] };
        let ids = |q: Query| SeatbeltBackend.parse_list(&q, listing).into_iter().map(|l| l.handle.id).collect::<Vec<_>>();
        assert_eq!(ids(q(Having::Label("k".into()), false)), ["darkmux-a.b"]);
        assert_eq!(ids(q(Having::Label("k".into()), true)), ["darkmux-a.b", "darkmux-aXb"]);
        assert_eq!(ids(q(Having::NamePrefix("darkmux-a.".into()), true)), ["darkmux-a.b"]);
        assert_eq!(ids(q(Having::Name("other".into()), true)), ["other"]);
        let first = &SeatbeltBackend.parse_list(&q(Having::Label("k".into()), false), listing)[0];
        assert_eq!(first.labels, [Some("2".to_string()), Some("1".to_string())]);
    }

    /// A directory under the state root that this backend did not write
    /// (the lab's workload trees once shared the default root's name) is
    /// never listed and never deleted by a remove.
    #[test]
    fn a_directory_it_did_not_write_is_never_listed_or_removed() {
        let root = tempfile::TempDir::new().unwrap();
        let (ours, theirs) = (root.path().join("darkmux-x"), root.path().join("quick-coding"));
        std::fs::create_dir_all(&ours).unwrap();
        std::fs::write(ours.join("profile.sb"), "(version 1)").unwrap();
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

    /// A start script quotes every argument, so a value holding a quote,
    /// a space or `$` reaches the program as itself.
    #[test]
    fn sh_quoting_keeps_an_argument_whole() {
        let out = Command::new(SH).arg("-c").arg(format!("printf '%s|' {}", sh_quote("a 'b' $HOME"))).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "a 'b' $HOME|");
    }
}
