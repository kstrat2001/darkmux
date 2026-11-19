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
//!   deny it, never tell loopback from the rest. It can hold a sandbox to
//!   PORTS: a policy naming `ports` binds and takes in traffic only on those
//!   numbers: a bind to another port, and a `listen` with no bind, are
//!   refused, and no socket on another port accepts connections or
//!   unsolicited datagrams; replies on connections it opened (TCP, connected
//!   UDP) arrive. An unconnected UDP socket's `recvfrom` is refused, even for
//!   a reply. A caller that needs loopback-only listeners watches the
//!   address itself: a dispatch has the TCP listeners its processes hold
//!   read every second and is stopped on one beyond the loopback
//!   (`darkmux-crew`'s `sandbox_listeners`), a client preview watches its
//!   two ports. Both are detection with a window, not a boundary.
//!   [`Network::None`] allows no network at all, the loopback included.
//! - **Reach every descendant.** A sandbox is the session its launcher leads
//!   (#3559): its process group is killed over and over until it is empty (a
//!   process that keeps forking is caught), then every other process in the
//!   session is (one that moved to a group of its own, as a runtime's tool
//!   command or job control does). Nothing in it can move into another
//!   sandbox's group, or darkmux's, and it has no controlling terminal. A
//!   descendant that starts a session of its own (`setsid`) outlives every
//!   kill, still inside the sandbox: it keeps its file and network rules
//!   (binds and inbound only on the named ports) and nothing ends it:
//!   nothing marks it as the sandbox's, so nothing else is killed.
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
pub(crate) const DEFAULT_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

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
/// [`launcher`]). Its guard, a helper in a group of its own started first,
/// is given the launcher's pid (never reads it from its own parent, which is
/// launchd already when the launcher ends first), waits for the launcher to
/// be gone (at once when it already is) (its program ended, or the launcher
/// itself killed outright, which no handler sees) and then kills the
/// launcher's whole group again and again until it is empty (#3112): one
/// kill of a group reaches every member at that instant, a fork-hopper's
/// newest pid included, where a list of pids is always a step behind.
/// Then (#3559) it ends every other process in the launcher's session
/// ([`sweep!`]), so one that moved to a group of its own (`setpgid`, as a
/// runtime's tool command does) ends too. Only one that started a session of
/// its own (`setsid`) is not reached: unlike a container's, this sandbox
/// does not end everything in it.
const LAUNCH: &str = r#"wd=$1; shift
echo $$ > "$d/pid" && st=$(/usr/bin/env -i TZ=UTC0 /bin/ps -o lstart= -p $$) && [ -n "$st" ] && printf 'utc:%s\n' "$st" > "$d/started" || exit 125
{GUARD} "$$" </dev/null >/dev/null 2>&1 &
trap : TERM INT HUP
[ -z "$wd" ] || cd "$wd" || { rm -rf "$d"; exit 125; }
[ -n "$1" ] || { echo "seatbelt: no program to run (a policy needs an entrypoint)" >&2; rm -rf "$d"; exit 125; }
/usr/bin/perl -e '{CARRY}' /usr/bin/sandbox-exec -f "$d/profile.sb" "$@"
rc=$?
g=$$
"#;

/// (#3559) Perl that kills every process in session `$ARGV[0]` but itself,
/// zombies and pids 0 and 1, listing them again until none is left (at most
/// 200 times), or until a listing finds exactly the processes the last kill
/// was sent to (one it cannot kill, a setuid program's, would otherwise keep
/// it going for all 200: several seconds). Pid 0 is skipped because `ps`
/// lists it under root and `getsid(0)` is the caller's own session.
/// A Seatbelt launcher leads a session of its own, so its session is the
/// whole sandbox but what started a session of its own (`setsid`). macOS's
/// `ps` does not print a session id, so each listed pid's is asked of the
/// kernel: `getsid` is system call 310 (`<sys/syscall.h>`), which perl
/// reaches without headers; a pid gone since it was listed answers -1 and is
/// skipped. No single quote: it runs inside one.
macro_rules! sweep {
    () => {
        "my $s = $ARGV[0] + 0; my $last = \"\"; for (1 .. 200) { my @p = map { $_->[0] } grep { $_->[0] > 1 && $_->[0] != $$ && $_->[1] !~ /^Z/ && syscall(310, $_->[0] + 0) == $s } map { [split] } `/bin/ps -A -o pid=,stat=`; last unless @p; my $now = join(\",\", @p); last if $now eq $last; $last = $now; kill(\"KILL\", @p); select(undef, undef, undef, 0.01); }"
    };
}

/// Ends every process in group `$g` from outside it (stop, remove and kill
/// run in a group of their own): the whole group is killed at once, again
/// and again until no process is left in it, at most 1000 times. One kill
/// of a group reaches every member at that instant, so a process that keeps
/// forking a new pid (a fork-hopper) is caught where a list of pids, always
/// a step behind, is not. Then (#3559) every process left in the session
/// `$g` leads ([`sweep!`]): one that moved to a group of its own in it.
const DRAIN: &str = concat!(
    r#"n=0
while [ "$n" -lt 1000 ] && kill -0 -- "-$g" 2>/dev/null; do
  kill -KILL -- "-$g" 2>/dev/null
  n=$((n + 1))
done
/usr/bin/perl -e '"#,
    sweep!(),
    r#"' "$g"
"#
);

/// The launcher's end: the directory removed, the program's status its
/// own. What the program left running in the group is ended by the
/// launcher's guard ([`LAUNCH`]), once the launcher is gone.
const LAUNCHED: &str = r#"rm -rf "$d"
exit $rc
"#;

/// The whole launcher script.
fn launcher() -> String {
    format!("{}{LAUNCHED}", LAUNCH.replacen("{GUARD}", GUARD, 1).replacen("{CARRY}", CARRY, 1))
}

/// The prefix of the variables that carry a policy's environment through
/// the launcher ([`SeatbeltBackend::launcher_environment`]): each holds one
/// `key=value`. The launcher's shell, its guard and its bookkeeping run with
/// nothing else, so no policy variable reaches them under its own name
/// (`BASH_FUNC_*`, `SHELLOPTS` and `PS4`, `PERL5OPT`, `IFS` ...); only the
/// program gets them ([`CARRY`]).
pub(crate) const CARRIED: &str = "DARKMUX_SANDBOX_ENV_";

/// Perl that sets the program's environment to exactly the carried
/// variables ([`CARRIED`]), each under its own name, in the order carried
/// (so were a name carried twice, the later would win, as it does in
/// [`SeatbeltBackend::launcher_environment`], which carries each once), plus the launcher's
/// `PWD` when the policy names none, then runs its arguments as the
/// program (`sandbox-exec` and what it runs), never through a shell. It
/// starts with the launcher's own environment, so no policy variable
/// reaches perl itself. No single quote: it runs inside one.
const CARRY: &str = "my %e; for my $k (sort { substr($a, 20) <=> substr($b, 20) } grep { /^DARKMUX_SANDBOX_ENV_[0-9]+$/ } keys %ENV) { my ($n, $v) = split(/=/, $ENV{$k}, 2); $e{$n} = $v if defined $v && length $n; } $e{PWD} = $ENV{PWD} if defined $ENV{PWD} && !exists $e{PWD}; %ENV = %e; exec { $ARGV[0] } @ARGV; exit 127;";

/// The launcher's guard ([`LAUNCH`]), its launcher's pid its argument: the
/// group drained, then the session swept ([`sweep!`]).
const GUARD: &str = concat!(
    r#"/usr/bin/perl -e 'setpgrp(0, 0); my $l = $ARGV[0]; select(undef, undef, undef, 0.05) while getppid() == $l; for (1 .. 1000) { last unless kill(0, -$l); kill("KILL", -$l); } "#,
    sweep!(),
    "'"
);

/// Whether `$d` is a sandbox's directory at all: only a directory holding
/// the profile this backend wrote is ever listed, stopped or deleted.
const OURS: &str = r#"[ -f "$d/profile.sb" ]"#;

/// Whether `$d`'s sandbox is running: its launcher's pid is alive and is
/// still that launcher, started when the launcher recorded (a reused pid,
/// or another sandbox's launcher, is not), leaving `$pid` set. A start is
/// recorded and read as [`STARTED_PREFIX`] and `ps -o lstart=` run by
/// [`STARTED_READER`], never in the sandbox's own environment: `lstart`'s
/// text follows `TZ` and the locale, which a policy may set.
///
/// A record without the prefix was written by a launcher from before it,
/// in its policy's environment: it is compared the way that launcher's own
/// check compared it (`ps -o lstart=` with only `PATH` set), and when that
/// does not match while the pid lives, the sandbox is [`UNDECIDED`], never
/// gone.
const ALIVE: &str = r#"pid=$(cat "$d/pid" 2>/dev/null) && kill -0 "$pid" 2>/dev/null && started=$(cat "$d/started" 2>/dev/null) && [ -n "$started" ] && { [ "utc:$(/usr/bin/env -i TZ=UTC0 /bin/ps -o lstart= -p "$pid" 2>/dev/null)" = "$started" ] || { case $started in utc:*) false ;; *) [ "$(/bin/ps -o lstart= -p "$pid" 2>/dev/null)" = "$started" ] ;; esac; }; }"#;

/// Waits, at most 2.5 s, while `$d`'s launcher has recorded its pid but not
/// yet its start (it writes them one after the other), so a stop, kill or
/// remove landing between the two decides on both, never on the pid alone
/// ([`UNDECIDED`], which would leave the sandbox to start and run).
const SETTLE: &str = r#"i=0; while [ -f "$d/pid" ] && [ ! -s "$d/started" ] && [ "$i" -lt 50 ]; do sleep 0.05; i=$((i + 1)); done"#;

/// Whether `$d`'s sandbox, which [`ALIVE`] did not find running, may still
/// run: its pid lives and no record in the [`STARTED_PREFIX`] form says it
/// is another process (the record is from an older launcher, or not written
/// yet). Such a sandbox is never removed, stopped or listed as exited:
/// what decides it is gone is a dead pid or a comparable record that
/// differs.
const UNDECIDED: &str = r#"pid=$(cat "$d/pid" 2>/dev/null) && kill -0 "$pid" 2>/dev/null && case $(cat "$d/started" 2>/dev/null) in utc:*) false ;; *) true ;; esac"#;

/// How a launcher's start is recorded in `$d/started` and read back ([`LAUNCH`],
/// [`ALIVE`], and `host_leaders`): this prefix, then `ps -o lstart=` with no
/// environment but UTC, so its text is the same whatever `TZ`, locale or
/// system time zone the recorder and the reader have. A record without the
/// prefix was written some other way and cannot be compared.
pub(crate) const STARTED_PREFIX: &str = "utc:";

/// The command, then the pid, that reads a process's start as
/// [`STARTED_PREFIX`] records it.
pub(crate) const STARTED_READER: [&str; 6] = ["/usr/bin/env", "-i", "TZ=UTC0", "/bin/ps", "-o", "lstart="];

/// The directory sandboxes live under: [`STATE_DIR_ENV`], else
/// `~/.darkmux/seatbelt` (not `~/.darkmux/sandboxes`, which holds the lab's
/// workload trees).
pub fn state_root() -> PathBuf {
    match std::env::var_os(STATE_DIR_ENV) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".darkmux/seatbelt"),
    }
}

/// How a planned shell command is built: `script` with its arguments and
/// environment ([`SeatbeltBackend::script`] or its launcher twin).
type BuildScript = fn(&str, &[String], &[(String, String)]) -> Command;

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

    /// What the default network may bind and take in: with `ports` named,
    /// only those port numbers (on any local address: SBPL's `localhost`
    /// matches every one, #3559). Every other bind is refused, and so is
    /// inbound traffic to any other local port: a `listen` with no bind (an
    /// ephemeral port the bind rule never sees) is refused, and no socket on
    /// another port accepts connections or unsolicited datagrams. Replies on
    /// connections the sandbox opened (TCP, connected UDP) arrive; an
    /// unconnected UDP socket's `recvfrom` is refused, even for a reply.
    /// With none named, any port, as before.
    fn bind_rule(ports: &[u16]) -> String {
        if ports.is_empty() {
            return "(allow network-bind (local ip \"localhost:*\"))\n".to_string();
        }
        let named = join(ports.iter().map(|p| format!("(local ip \"localhost:{p}\")")));
        format!("(allow network-bind {named})\n(deny network-inbound)\n(allow network-inbound {named})\n")
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

    /// The environment a launcher runs in: `PATH`, and the policy's
    /// environment ([`Self::environment`]) carried under [`CARRIED`] names
    /// for the program alone ([`CARRY`]). A variable whose name is empty or
    /// holds `=` is not one a program could be given, and is dropped.
    fn launcher_environment(policy: &SandboxPolicy, dir: &Path) -> Vec<(String, String)> {
        let valid = Self::environment(policy, dir).into_iter().filter(|(k, _)| !k.is_empty() && !k.contains('='));
        std::iter::once(("PATH".to_string(), DEFAULT_PATH.to_string()))
            .chain(last_of_each(valid).into_iter().enumerate().map(|(i, (k, v))| (format!("{CARRIED}{i}"), format!("{k}={v}"))))
            .collect()
    }

    /// The environment the program `cmd` plans is given ([`CARRY`]): each
    /// variable [`Self::launcher_environment`] carried, under its own name,
    /// in the order carried. What a caller that checks a planned sandbox's
    /// environment reads, since the launcher's own holds only `PATH` and
    /// the carried variables.
    pub fn program_environment(cmd: &Command) -> Vec<(String, String)> {
        let mut carried: Vec<(usize, String, String)> = cmd
            .get_envs()
            .filter_map(|(k, v)| {
                let n = k.to_str()?.strip_prefix(CARRIED)?.parse().ok()?;
                let (key, value) = v?.to_str()?.split_once('=')?;
                Some((n, key.to_string(), value.to_string()))
            })
            .collect();
        carried.sort_by_key(|(n, ..)| *n);
        carried.into_iter().map(|(_, k, v)| (k, v)).collect()
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

    /// (#3559) [`Self::script`] for a command that becomes a sandbox's
    /// launcher: it leads a session of its own rather than only a group, so
    /// nothing in the sandbox can move into another sandbox's process group
    /// (or darkmux's: `setpgid` across sessions is refused), and it has no
    /// controlling terminal. Its group is still its own pid, which every
    /// stop, kill and its guard signal. A caller must not set a process
    /// group on it as well: the spawn would fail.
    fn launcher_script(script: &str, args: &[String], env: &[(String, String)]) -> Command {
        let mut cmd = Command::new(SH);
        cmd.arg("-c").arg(script).arg("sh").args(args);
        cmd.env_clear().envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        darkmux_platform::in_a_session_of_its_own(&mut cmd);
        cmd
    }

    /// A lifecycle command for `handle`'s directory, or one that fails
    /// naming why when its id is no sandbox name.
    fn on_dir(handle: &SandboxHandle, script: &str, extra: &[String]) -> Command {
        Self::on_dir_as(handle, script, extra, Self::script)
    }

    /// [`Self::on_dir`], its command built by `build` ([`Self::script`], or
    /// [`Self::launcher_script`] for `start`, whose command becomes the
    /// launcher).
    fn on_dir_as(handle: &SandboxHandle, script: &str, extra: &[String], build: BuildScript) -> Command {
        match dir_of(&handle.id) {
            Some(dir) => {
                let args: Vec<String> = std::iter::once(dir.to_string_lossy().into_owned()).chain(extra.iter().cloned()).collect();
                build(&format!("d=$1\n{script}"), &args, &[("PATH".into(), DEFAULT_PATH.into())])
            }
            None => refused(&handle.id),
        }
    }
}

/// `env` with each name once: its last value, in the place of its last
/// naming, as a process given the same name twice (`Command::env`, a
/// shell's `export`) keeps the later one.
fn last_of_each(env: impl IntoIterator<Item = (String, String)>) -> Vec<(String, String)> {
    let all: Vec<(String, String)> = env.into_iter().collect();
    let mut seen = std::collections::HashSet::new();
    let mut kept: Vec<(String, String)> = all.into_iter().rev().filter(|(k, _)| seen.insert(k.clone())).collect();
    kept.reverse();
    kept
}

/// Removes `$d`'s sandbox, killing it first if it runs; leaves alone a
/// directory this backend did not write ([`OURS`]), and one whose sandbox
/// may still run ([`UNDECIDED`]).
fn remove_script() -> String {
    format!("{OURS} || exit 0\n{SETTLE}\nif {ALIVE}; then kill -KILL -- \"-$pid\" 2>/dev/null; g=$pid\n{DRAIN}elif {UNDECIDED}; then exit 0\nfi\nrm -rf \"$d\"; exit 0")
}

/// Lists the sandboxes under `$1`, skipping any directory this backend did
/// not write ([`OURS`]). A sandbox that may still run but cannot be shown
/// to ([`UNDECIDED`]) is `undecided`: neither running (nothing may stop or
/// kill it) nor exited ([`undecided_sandboxes`] names it with its remedy).
fn list_script() -> String {
    format!(
        r#"r=$1; [ -d "$r" ] || exit 0
for d in "$r"/*; do
  {OURS} || continue
  if {ALIVE}; then s=running; elif {UNDECIDED}; then s=undecided; elif [ -f "$d/pid" ]; then s=exited; else s=created; fi
  printf 'N\t%s\t%s\n' "${{d##*/}}" "$s"
  sed 's/^/L	/' "$d/labels" 2>/dev/null
done
exit 0"#
    )
}

/// A sandbox that may still run but whose launcher cannot be shown to be
/// the one it recorded ([`UNDECIDED`]: a live pid beside a start record from
/// an older darkmux): darkmux neither stops nor removes it, since the pid
/// may not be its own. Its remedy is the operator's ([`Undecided::remedy`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Undecided {
    pub name: String,
    pub dir: PathBuf,
    /// The pid it recorded, when it reads.
    pub pid: Option<u32>,
}

impl Undecided {
    /// What the operator can do: look at the recorded pid, and when no
    /// sandbox of it runs, remove the record darkmux keeps for it. darkmux
    /// never kills a pid it cannot show is its own.
    pub fn remedy(&self) -> String {
        let pid = self.pid.map_or_else(|| "its recorded pid".to_string(), |p| format!("pid {p}"));
        let look = self.pid.map_or_else(String::new, |p| format!(" (`ps -o pid,lstart,command -p {p}`)"));
        format!(
            "Seatbelt sandbox `{}` may still run a program an older darkmux started: its start record cannot be compared, so darkmux cannot show {pid} is its launcher and will neither stop nor remove it. If no sandbox of it runs{look}, remove its record: rm -rf {}",
            self.name,
            sh_quote(&self.dir.to_string_lossy())
        )
    }
}

/// The names the listing ([`list_script`]) marks `undecided`.
pub(crate) fn undecided_in(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|l| l.strip_prefix("N\t")?.split_once('\t'))
        .filter(|(_, state)| *state == "undecided")
        .map(|(name, _)| name.to_string())
        .collect()
}

/// Every Seatbelt sandbox on this host that is [`Undecided`], from one
/// listing bounded at 10 s; none when the listing gives no answer.
pub fn undecided_sandboxes() -> Vec<Undecided> {
    let root = canonical(&state_root());
    let list = SeatbeltBackend::script(&list_script(), &[root.to_string_lossy().into_owned()], &[("PATH".into(), DEFAULT_PATH.into())]);
    let Some(out) = darkmux_platform::output_within(list, std::time::Duration::from_secs(10)) else { return Vec::new() };
    undecided_in(&String::from_utf8_lossy(&out.stdout))
        .into_iter()
        .map(|name| {
            let dir = root.join(&name);
            let pid = std::fs::read_to_string(dir.join("pid")).ok().and_then(|p| p.trim().parse().ok());
            Undecided { name, dir, pid }
        })
        .collect()
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
pub(crate) fn canonical(path: &Path) -> PathBuf {
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

pub(crate) fn join(parts: impl Iterator<Item = String>) -> String {
    parts.collect::<Vec<_>>().join(" ")
}

/// One line of the start script: each argument single-quoted for `sh`.
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

impl SandboxBackend for SeatbeltBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Seatbelt
    }

    fn default_path(&self) -> Option<&'static str> {
        Some(DEFAULT_PATH)
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
        let mut cmd = Self::launcher_script(&format!("{PREPARE}shift 3\n{}", launcher()), &args, &Self::launcher_environment(policy, &dir));
        if !policy.stdin {
            cmd.stdin(Stdio::null());
        }
        (cmd, handle)
    }

    fn create(&self, policy: &SandboxPolicy) -> (Command, SandboxHandle) {
        let handle = SandboxHandle { backend: BackendKind::Seatbelt, id: policy.name.clone() };
        let Some(dir) = dir_of(&policy.name) else { return (refused(&policy.name), handle) };
        let _ = std::fs::create_dir_all(state_root());
        let env = Self::launcher_environment(policy, &dir);
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
        let mut cmd = Self::on_dir_as(handle, r#"[ -f "$d/start.sh" ] || { echo "seatbelt: no created sandbox $d" >&2; exit 125; }
exec /bin/sh "$d/start.sh""#, &[], Self::launcher_script);
        cmd.stdin(Stdio::null());
        cmd
    }

    fn stop(&self, handle: &SandboxHandle, grace: std::time::Duration) -> Command {
        let script = format!(
            r#"{OURS} && [ -f "$d/pid" ] || exit 0
{SETTLE}
if {ALIVE}; then
  kill -TERM -- "-$pid" 2>/dev/null
  i=0; while kill -0 "$pid" 2>/dev/null && [ "$i" -lt "$(($2 * 10))" ]; do sleep 0.1; i=$((i + 1)); done
  kill -KILL -- "-$pid" 2>/dev/null
  g=$pid
{DRAIN}elif {UNDECIDED}; then exit 0
fi
rm -rf "$d"; exit 0"#
        );
        Self::on_dir(handle, &script, &[grace.as_secs().to_string()])
    }

    fn kill(&self, handle: &SandboxHandle) -> Command {
        Self::on_dir(handle, &format!("{OURS} || exit 1\n{SETTLE}\n{ALIVE} || exit 1\nkill -KILL -- \"-$pid\" || exit 1\ng=$pid\n{DRAIN}rm -rf \"$d\""), &[])
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
        parse_listing(BackendKind::Seatbelt, query, listing)
    }
}

/// The sandboxes in a listing of `N<TAB>name<TAB>state` and `L<TAB>key=value`
/// lines (what a host-process backend's `list` prints) that `query` asks
/// for, as `kind`'s handles. Seatbelt's and Landlock's share it.
pub(crate) fn parse_listing(kind: BackendKind, query: &Query, listing: &str) -> Vec<Listed> {
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
            handle: SandboxHandle { backend: kind, id: name },
            labels: query.read.iter().map(|key| labels.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())).collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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
        // (#3112 review) A listen with no bind gets an ephemeral port the
        // bind rule never sees: inbound is held to the named ports too.
        assert!(named.contains("(deny network-inbound)\n(allow network-inbound (local ip \"localhost:4100\") (local ip \"localhost:4200\"))\n"), "{named}");
        assert!(!named.contains("localhost:*"), "{named}");
        let any = SeatbeltBackend::profile(&policy(Vec::new(), Network::Default), Path::new("/s"));
        assert!(any.contains("(allow network-bind (local ip \"localhost:*\"))"), "{any}");
        assert!(!any.contains("network-inbound"), "no ports named, inbound as it was: {any}");
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

    /// The launcher records a start, and `ALIVE` reads one, with the same
    /// reader and prefix `host_leaders` compares against.
    #[test]
    fn a_start_is_recorded_and_read_by_the_one_reader() {
        let reader = STARTED_READER.join(" ");
        assert!(LAUNCH.contains(&format!("st=$({reader} -p $$)")) && LAUNCH.contains(&format!("'{STARTED_PREFIX}%s\\n'")), "{LAUNCH}");
        assert!(ALIVE.contains(&format!("\"{STARTED_PREFIX}$({reader} -p \"$pid\" 2>/dev/null)\"")), "{ALIVE}");
    }

    /// Runs `p` under a state root of its own until its launcher has
    /// recorded its start; the launcher, its directory and the root (kept
    /// alive by the caller), with [`STATE_DIR_ENV`] pointing at it until
    /// [`StateRoot`] drops.
    fn running(mut p: SandboxPolicy, name: &str) -> (std::process::Child, PathBuf, StateRoot) {
        let root = StateRoot::new();
        p.name = name.into();
        let (mut cmd, _) = SeatbeltBackend.run(&p);
        let child = cmd.spawn().unwrap();
        let d = canonical(root.0.path()).join(name);
        for _ in 0..300 {
            if std::fs::read_to_string(d.join("started")).is_ok_and(|s| !s.trim().is_empty()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        (child, d, root)
    }

    /// A temporary [`STATE_DIR_ENV`], restored on drop.
    struct StateRoot(tempfile::TempDir, Option<std::ffi::OsString>);

    impl StateRoot {
        fn new() -> Self {
            let root = tempfile::TempDir::new().unwrap();
            let prev = std::env::var_os(STATE_DIR_ENV);
            unsafe { std::env::set_var(STATE_DIR_ENV, root.path()) };
            StateRoot(root, prev)
        }
    }

    impl Drop for StateRoot {
        fn drop(&mut self) {
            unsafe {
                match self.1.take() {
                    Some(v) => std::env::set_var(STATE_DIR_ENV, v),
                    None => std::env::remove_var(STATE_DIR_ENV),
                }
            }
        }
    }

    /// A process's start as `ps -o lstart=` prints it under `env`, the way
    /// a launcher before the `utc:` form recorded it in its policy's
    /// environment.
    fn lstart_under(pid: u32, env: &[(&str, &str)]) -> String {
        let out = Command::new("/bin/ps").args(["-o", "lstart=", "-p", &pid.to_string()]).env_clear().envs(env.iter().copied()).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string()
    }

    /// A sandbox left running by a launcher that recorded its start the
    /// older way (no `utc:` prefix, in its policy's time zone) is never
    /// taken for gone: when its record cannot be compared, remove and stop
    /// leave its directory and its processes alone, and the listing says
    /// running. When its record does compare (written with no time zone),
    /// remove ends it as before.
    #[test]
    #[serial_test::serial]
    fn a_sandbox_with_an_older_start_record_is_never_taken_for_gone() {
        if !SeatbeltBackend::available() {
            return;
        }
        let mut p = policy(Vec::new(), Network::None);
        p.entrypoint = Some("/bin/sleep".into());
        p.command = vec!["20".into()];
        let (mut child, d, root) = running(p.clone(), "darkmux-older-record");
        let pid: u32 = std::fs::read_to_string(d.join("pid")).unwrap().trim().parse().unwrap();
        std::fs::write(d.join("started"), format!("{}\n", lstart_under(pid, &[("TZ", "Pacific/Kiritimati"), ("PATH", DEFAULT_PATH)]))).unwrap();
        let handle = SandboxHandle { backend: BackendKind::Seatbelt, id: "darkmux-older-record".into() };
        let removed = SeatbeltBackend.remove(&handle).status().unwrap();
        let stopped = SeatbeltBackend.stop(&handle, Duration::from_secs(1)).status().unwrap();
        let listing = String::from_utf8_lossy(&SeatbeltBackend.list(&Query::labeled("darkmux.host.pid")).output().unwrap().stdout).into_owned();
        let launchers = crate::host_leaders::host_sandbox_launchers();
        let undecided = undecided_sandboxes();
        let (kept, alive) = (d.is_dir(), child.try_wait().unwrap().is_none());
        let _ = std::fs::write(d.join("started"), format!("{}\n", lstart_under(pid, &[("PATH", DEFAULT_PATH)])));
        let ended = SeatbeltBackend.remove(&handle).status().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let gone = child.try_wait().unwrap().is_some();
        let _ = Command::new("/bin/kill").args(["-KILL", &format!("-{pid}")]).status();
        let _ = child.wait();
        drop(root);
        assert!(removed.success() && stopped.success());
        assert!(kept && alive, "remove or stop took a live sandbox for gone (dir kept {kept}, alive {alive})");
        assert!(listing.contains("N\tdarkmux-older-record\tundecided"), "{listing}");
        assert_eq!(undecided.iter().map(|u| (u.name.as_str(), u.pid)).collect::<Vec<_>>(), [("darkmux-older-record", Some(pid))]);
        assert!(!parse_listing(BackendKind::Seatbelt, &Query { stopped_too: false, ..Query::labeled("darkmux.host.pid") }, &listing).iter().any(|l| l.handle.id == "darkmux-older-record"), "never listed as running: nothing tries to kill it");
        assert_eq!(launchers, Some(vec![pid]));
        assert!(ended.success() && gone && !d.exists(), "a record that compares is still ended by remove");
    }

    /// A planned sandbox's launcher runs with only `PATH` and the carried
    /// variables, and [`SeatbeltBackend::program_environment`] reads back
    /// exactly the policy's environment the program is given, in order.
    #[test]
    fn the_program_environment_is_the_policys_carried_through_the_launcher() {
        let mut p = policy(Vec::new(), Network::None);
        p.env = vec![("A".into(), "1=2".into()), ("PATH".into(), "/p".into()), ("BASH_FUNC_x%%".into(), "() { :; }".into()), ("".into(), "dropped".into())];
        let (cmd, _) = SeatbeltBackend.run(&p);
        let launcher: Vec<String> = cmd.get_envs().map(|(k, _)| k.to_string_lossy().into_owned()).collect();
        assert!(launcher.iter().all(|k| k == "PATH" || k.starts_with(CARRIED)), "{launcher:?}");
        let tmp = canonical(&state_root().join("darkmux-dispatch-1/tmp")).to_string_lossy().into_owned();
        assert_eq!(
            SeatbeltBackend::program_environment(&cmd),
            [("A", "1=2"), ("PATH", "/p"), ("BASH_FUNC_x%%", "() { :; }"), ("HOME", tmp.as_str()), ("TMPDIR", tmp.as_str())]
                .map(|(k, v)| (k.to_string(), v.to_string()))
        );
    }

    /// A variable the policy names twice is carried once, its last value
    /// (a preview's recipe naming `PATH` after the toolchain's), in the
    /// place of its last naming.
    #[test]
    fn a_variable_named_twice_is_carried_once_with_its_last_value() {
        let mut p = policy(Vec::new(), Network::None);
        p.env = vec![("PATH".into(), "/toolchain".into()), ("A".into(), "1".into()), ("PATH".into(), "/recipe".into())];
        let (cmd, _) = SeatbeltBackend.run(&p);
        let tmp = canonical(&state_root().join("darkmux-dispatch-1/tmp")).to_string_lossy().into_owned();
        assert_eq!(
            SeatbeltBackend::program_environment(&cmd),
            [("A", "1"), ("PATH", "/recipe"), ("HOME", tmp.as_str()), ("TMPDIR", tmp.as_str())].map(|(k, v)| (k.to_string(), v.to_string()))
        );
    }

    /// Run after run, the program gets the last value of a variable the
    /// policy names twice, never one picked by chance.
    #[test]
    #[serial_test::serial]
    fn a_program_always_gets_the_last_value_of_a_variable_named_twice() {
        if !SeatbeltBackend::available() {
            return;
        }
        let mut seen = Vec::new();
        for n in 0..8 {
            let mut p = policy(Vec::new(), Network::None);
            p.env = vec![("PATH".into(), "/toolchain/bin:/usr/bin:/bin".into()), ("PATH".into(), "/recipe/bin:/usr/bin:/bin".into())];
            p.entrypoint = Some("/bin/sh".into());
            p.command = vec!["-c".into(), "printf %s \"$PATH\" > \"$TMPDIR/path\"; exec /bin/sleep 3".into()];
            let (mut child, d, root) = running(p, &format!("darkmux-twice-{n}"));
            let mut path = String::new();
            for _ in 0..200 {
                path = std::fs::read_to_string(d.join("tmp/path")).unwrap_or_default();
                if !path.is_empty() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = SeatbeltBackend.remove(&SandboxHandle { backend: BackendKind::Seatbelt, id: format!("darkmux-twice-{n}") }).status();
            let _ = child.wait();
            drop(root);
            seen.push(path);
        }
        assert!(seen.iter().all(|p| p == "/recipe/bin:/usr/bin:/bin"), "{seen:?}");
    }

    /// A listing's `undecided` rows are named, and nothing else; the remedy
    /// names the sandbox, the pid to look at and the one command that
    /// removes its record, never a kill.
    #[test]
    fn an_undecided_sandbox_is_named_with_a_remedy_that_kills_nothing() {
        let listing = "N\tdarkmux-a\trunning\nL\tk=v\nN\tdarkmux-b\tundecided\nN\tdarkmux-c\texited\nN\tdarkmux-d\tundecided\n";
        assert_eq!(undecided_in(listing), ["darkmux-b", "darkmux-d"]);
        let u = Undecided { name: "darkmux-b".into(), dir: PathBuf::from("/s/darkmux b"), pid: Some(4242) };
        let remedy = u.remedy();
        assert!(remedy.contains("`darkmux-b`") && remedy.contains("ps -o pid,lstart,command -p 4242"), "{remedy}");
        assert!(remedy.ends_with("rm -rf '/s/darkmux b'"), "{remedy}");
        assert!(!remedy.contains("kill"), "{remedy}");
        let unread = Undecided { pid: None, ..u };
        assert!(unread.remedy().contains("its recorded pid") && !unread.remedy().contains("ps -o"), "{}", unread.remedy());
    }

    /// The carry sets the carried variables in the order carried, by
    /// number (the tenth after the ninth), so a later one always wins.
    #[cfg(unix)]
    #[test]
    fn the_carry_sets_variables_in_the_order_carried() {
        let mut cmd = Command::new("/usr/bin/perl");
        cmd.args(["-e", CARRY, "/usr/bin/env"]).env_clear();
        for n in 0..12 {
            cmd.env(format!("{CARRIED}{n}"), format!("P={n}"));
        }
        let out = String::from_utf8_lossy(&cmd.output().unwrap().stdout).into_owned();
        assert_eq!(out.lines().collect::<Vec<_>>(), ["P=11"], "{out}");
    }

    /// A stop that lands while a launcher has recorded its pid but not yet
    /// its start waits for the start and stops it, never leaving it to run.
    #[test]
    #[serial_test::serial]
    fn a_stop_between_the_pid_and_the_start_record_still_stops_the_sandbox() {
        if !SeatbeltBackend::available() {
            return;
        }
        let root = StateRoot::new();
        let d = canonical(root.0.path()).join("darkmux-starting");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("profile.sb"), "(version 1)").unwrap();
        let mut launcher = Command::new("/bin/sleep");
        launcher.arg("30");
        std::os::unix::process::CommandExt::process_group(&mut launcher, 0);
        let mut launcher = launcher.spawn().unwrap();
        let pid = launcher.id().to_string();
        std::fs::write(d.join("pid"), format!("{pid}\n")).unwrap();
        let record = Command::new(STARTED_READER[0]).args(&STARTED_READER[1..]).args(["-p", &pid]).output().unwrap();
        let record = format!("{STARTED_PREFIX}{}\n", String::from_utf8_lossy(&record.stdout).trim_end_matches('\n'));
        let late = d.join("started");
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            std::fs::write(late, record).unwrap();
        });
        let stopped = SeatbeltBackend.stop(&SandboxHandle { backend: BackendKind::Seatbelt, id: "darkmux-starting".into() }, Duration::from_secs(1)).status().unwrap();
        writer.join().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let ended = launcher.try_wait().unwrap().is_some();
        let _ = launcher.kill();
        let _ = launcher.wait();
        assert!(stopped.success());
        assert!(ended && !d.exists(), "the stop left the starting sandbox to run (ended {ended}, dir left {})", d.exists());
    }

    /// A policy's environment reaches the program and nothing the launcher
    /// does: functions exported to the shell (`BASH_FUNC_*`) cannot forge
    /// its pid or its start record.
    #[test]
    #[serial_test::serial]
    fn a_policys_environment_cannot_forge_the_launchers_record() {
        if !SeatbeltBackend::available() {
            return;
        }
        let mut p = policy(Vec::new(), Network::None);
        p.env = vec![
            ("A".into(), "seen".into()),
            ("BASH_FUNC_echo%%".into(), "() { builtin echo 4242; }".into()),
            ("BASH_FUNC_printf%%".into(), "() { builtin printf 'utc:forged\\n'; }".into()),
            ("SHELLOPTS".into(), "xtrace".into()),
            ("PS4".into(), "$(builtin echo 4242 > \"$d/pid\")".into()),
        ];
        p.entrypoint = Some("/bin/sh".into());
        p.command = vec!["-c".into(), "/usr/bin/printenv A > \"$TMPDIR/a\"; exec /bin/sleep 3".into()];
        let (mut child, d, root) = running(p, "darkmux-env-forge");
        std::thread::sleep(Duration::from_millis(300));
        let pid = std::fs::read_to_string(d.join("pid")).unwrap_or_default();
        let started = std::fs::read_to_string(d.join("started")).unwrap_or_default();
        let seen = std::fs::read_to_string(d.join("tmp/a")).unwrap_or_default();
        let real = Command::new(STARTED_READER[0]).args(&STARTED_READER[1..]).args(["-p", &child.id().to_string()]).output().unwrap();
        let _ = child.kill();
        let _ = child.wait();
        drop(root);
        assert_eq!(pid.trim(), child.id().to_string(), "the launcher's own pid");
        assert_eq!(started.trim(), format!("{STARTED_PREFIX}{}", String::from_utf8_lossy(&real.stdout).trim()), "the launcher's own start");
        assert_eq!(seen.trim(), "seen", "the program still gets the policy's environment");
    }

    /// A running sandbox whose policy sets `TZ` and a locale is still the
    /// running sandbox its directory records: its listing says running, and
    /// its launcher is among the host's sandbox launchers. Its start is
    /// recorded and read in one fixed environment, whatever the policy's.
    #[test]
    #[serial_test::serial]
    fn a_policys_time_zone_and_locale_never_make_a_running_sandbox_look_gone() {
        if !SeatbeltBackend::available() {
            return;
        }
        let root = tempfile::TempDir::new().unwrap();
        let prev = std::env::var_os(STATE_DIR_ENV);
        unsafe { std::env::set_var(STATE_DIR_ENV, root.path()) };
        let mut p = policy(Vec::new(), Network::None);
        p.name = "darkmux-tz-test".into();
        p.env = vec![("TZ".into(), "Pacific/Kiritimati".into()), ("LANG".into(), "fr_FR.UTF-8".into()), ("LC_ALL".into(), "fr_FR.UTF-8".into())];
        p.entrypoint = Some("/bin/sleep".into());
        p.command = vec!["5".into()];
        let (mut cmd, _) = SeatbeltBackend.run(&p);
        let mut child = cmd.spawn().unwrap();
        let d = canonical(root.path()).join("darkmux-tz-test");
        for _ in 0..200 {
            if std::fs::read_to_string(d.join("started")).is_ok_and(|s| !s.trim().is_empty()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let listing = SeatbeltBackend.list(&Query::labeled("darkmux.host.pid")).output().unwrap();
        let launchers = crate::host_leaders::host_sandbox_launchers();
        let pid: Option<u32> = std::fs::read_to_string(d.join("pid")).ok().and_then(|s| s.trim().parse().ok());
        let _ = SeatbeltBackend.remove(&SandboxHandle { backend: BackendKind::Seatbelt, id: "darkmux-tz-test".into() }).status();
        let _ = child.kill();
        let _ = child.wait();
        unsafe {
            match prev {
                Some(v) => std::env::set_var(STATE_DIR_ENV, v),
                None => std::env::remove_var(STATE_DIR_ENV),
            }
        }
        let listing = String::from_utf8_lossy(&listing.stdout).into_owned();
        assert!(listing.contains("N\tdarkmux-tz-test\trunning"), "{listing}");
        assert_eq!(launchers, Some(vec![pid.expect("the launcher recorded its pid")]));
    }

    /// (#3112 review, MAJOR D) A launcher that ends at once (its working
    /// folder missing) leaves no guard behind: twenty such launches, then no
    /// process naming their sandboxes is left.
    #[test]
    #[serial_test::serial]
    fn a_launcher_that_ends_at_once_leaves_no_guard() {
        if !SeatbeltBackend::available() {
            return;
        }
        let root = tempfile::TempDir::new().unwrap();
        let prev = std::env::var_os(STATE_DIR_ENV);
        unsafe { std::env::set_var(STATE_DIR_ENV, root.path()) };
        for n in 0..20 {
            let mut p = policy(Vec::new(), Network::None);
            p.name = format!("darkmux-guard-test-{n}");
            p.workdir = Some("/nonexistent-darkmux-workdir".into());
            let (mut cmd, _) = SeatbeltBackend.run(&p);
            let out = cmd.output().unwrap();
            assert_eq!(out.status.code(), Some(125), "{}", String::from_utf8_lossy(&out.stderr));
        }
        unsafe {
            match prev {
                Some(v) => std::env::set_var(STATE_DIR_ENV, v),
                None => std::env::remove_var(STATE_DIR_ENV),
            }
        }
        // And the guard alone, under a parent that is gone before it starts:
        // it must not mistake launchd (its new parent) for its launcher.
        for _ in 0..5 {
            Command::new(SH).arg("-c").arg(format!("{GUARD} \"$$\" </dev/null >/dev/null 2>&1 & exit 0")).status().unwrap();
        }
        std::thread::sleep(Duration::from_millis(500));
        // A guard is a perl that moved itself to a group of its own; once its
        // launcher is gone, launchd (pid 1) is its parent.
        let ps = Command::new("/bin/ps").args(["-A", "-o", "pid=,ppid=,command="]).output().unwrap();
        let pids: Vec<String> = String::from_utf8_lossy(&ps.stdout)
            .lines()
            .filter(|l| l.contains("/usr/bin/perl -e setpgrp(0, 0); my $l = "))
            .filter_map(|l| {
                let mut f = l.split_whitespace();
                let (pid, ppid) = (f.next()?, f.next()?);
                (ppid == "1").then(|| pid.to_string())
            })
            .collect();
        for pid in &pids {
            let _ = Command::new("/bin/kill").args(["-KILL", pid]).status();
        }
        assert!(pids.is_empty(), "guards left behind: {pids:?}");
    }

    /// (#3559 re-review) Remove, stop and kill sweep a whole session, so
    /// each is gated on the sandbox being alive and its launcher still the
    /// pid it recorded. A stale sandbox directory whose `pid` now names an
    /// unrelated session's leader (with a child in a process group of its
    /// own), and whose start time is not that process's, is removed by each
    /// without touching that session.
    #[test]
    #[serial_test::serial]
    fn a_stale_sandbox_never_ends_an_unrelated_session() {
        use std::io::BufRead;
        if !SeatbeltBackend::available() {
            return;
        }
        struct Session {
            leader: std::process::Child,
            child: String,
        }
        impl Drop for Session {
            fn drop(&mut self) {
                // The leader's group (its `sleep`), then the child that left it.
                for pid in [format!("-{}", self.leader.id()), self.child.clone()] {
                    let _ = Command::new("/bin/kill").args(["-KILL", "--", &pid]).status();
                }
                let _ = self.leader.wait();
            }
        }
        // An unrelated session: a leader, and a child in a group of its own.
        let session = || {
            let mut leader = Command::new(SH);
            leader.args(["-c", "/usr/bin/perl -e 'setpgrp(0, 0); sleep 60' & echo $!; sleep 60"]).stdout(Stdio::piped());
            darkmux_platform::in_a_session_of_its_own(&mut leader);
            let mut leader = leader.spawn().unwrap();
            let mut line = String::new();
            std::io::BufReader::new(leader.stdout.take().unwrap()).read_line(&mut line).unwrap();
            Session { leader, child: line.trim().to_string() }
        };
        let alive = |pid: &str| Command::new("/bin/kill").args(["-0", pid]).status().is_ok_and(|s| s.success());
        let root = tempfile::TempDir::new().unwrap();
        let prev = std::env::var_os(STATE_DIR_ENV);
        unsafe { std::env::set_var(STATE_DIR_ENV, root.path()) };
        let handle = SandboxHandle { backend: BackendKind::Seatbelt, id: "darkmux-stale-b3559".into() };
        let d = canonical(root.path()).join(&handle.id);
        let stale = |lead: u32, record: &str| {
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("profile.sb"), "(version 1)").unwrap();
            std::fs::write(d.join("pid"), lead.to_string()).unwrap();
            std::fs::write(d.join("started"), format!("{record}\n")).unwrap();
        };
        let lifecycle: [(&str, &dyn Fn() -> Command); 3] = [
            ("remove", &|| SeatbeltBackend.remove(&handle)),
            ("stop", &|| SeatbeltBackend.stop(&handle, Duration::from_secs(1))),
            ("kill", &|| SeatbeltBackend.kill(&handle)),
        ];
        let (mut ended, mut kept) = (Vec::new(), Vec::new());
        // A record in today's form that is not the leader's start says the
        // pid was reused: remove and stop clear the directory. An older
        // launcher's record cannot say so, and leaves it.
        for record in ["utc:Thu Jan  1 00:00:00 1970", "Thu Jan  1 00:00:00 1970"] {
            for (what, command) in &lifecycle {
                let s = session();
                stale(s.leader.id(), record);
                let _ = command().output().unwrap();
                if !alive(&s.leader.id().to_string()) || !alive(&s.child) {
                    ended.push(*what);
                }
                if d.exists() && *what != "kill" && record.starts_with("utc:") {
                    kept.push(*what);
                }
                let _ = std::fs::remove_dir_all(&d);
            }
        }
        unsafe {
            match prev {
                Some(v) => std::env::set_var(STATE_DIR_ENV, v),
                None => std::env::remove_var(STATE_DIR_ENV),
            }
        }
        assert_eq!(ended, Vec::<&str>::new(), "an unrelated session was ended");
        assert_eq!(kept, Vec::<&str>::new(), "a reused pid's stale directory was kept");
    }

    /// A lifecycle command runs its script with `$d` the sandbox's
    /// directory under the state root and its extra arguments after it, on
    /// the backend's own PATH and nothing else of the caller's environment.
    /// A name that could leave the state root runs none of the script: it
    /// fails, naming the name. Plain `sh`, so it holds on every OS.
    #[test]
    #[serial_test::serial]
    fn a_lifecycle_command_runs_on_its_sandboxs_directory_or_refuses_the_name() {
        let root = tempfile::TempDir::new().unwrap();
        let prev = std::env::var_os(STATE_DIR_ENV);
        unsafe { std::env::set_var(STATE_DIR_ENV, root.path()) };
        let on = |id: &str, extra: &[String]| {
            let handle = SandboxHandle { backend: BackendKind::Seatbelt, id: id.into() };
            SeatbeltBackend::on_dir(&handle, r#"printf '%s|%s|%s|%s' "$d" "$2" "$PATH" "${HOME-unset}""#, extra).output().unwrap()
        };
        let ran = on("darkmux-x.1", &["7".into()]);
        let refused = on("../escape", &[]);
        unsafe {
            match prev {
                Some(v) => std::env::set_var(STATE_DIR_ENV, v),
                None => std::env::remove_var(STATE_DIR_ENV),
            }
        }
        let dir = canonical(root.path()).join("darkmux-x.1");
        assert_eq!(String::from_utf8_lossy(&ran.stdout), format!("{}|7|{DEFAULT_PATH}|unset", dir.display()));
        assert!(ran.status.success());
        assert_eq!(refused.status.code(), Some(125));
        assert!(refused.stdout.is_empty(), "the script ran for a refused name");
        assert!(String::from_utf8_lossy(&refused.stderr).contains("not a sandbox name: ../escape"), "{refused:?}");
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
