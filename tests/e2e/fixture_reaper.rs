//! (#2716) Process-lifetime plumbing for the child processes
//! `FleetHarness` spawns — the `redis-server` and the one `darkmux serve`
//! daemon per node.
//!
//! ## Why `Drop` is not the mechanism
//!
//! `FleetHarness` and `FleetNode` both kill their children in `Drop`, and
//! that has always worked for the case it covers: a test that finishes.
//! It covers none of the cases that actually leak, because **the runs that
//! leak are precisely the runs where `Drop` never executes** — a `Ctrl-C`
//! on `cargo test` (SIGINT's default disposition terminates; no unwind), a
//! `SIGKILL`ed runner, a harness process that aborts. Measured on one
//! developer machine before this module existed: 63 `redis-server`
//! processes, 60 of them on ephemeral ports with no parent, the oldest 76
//! days old, clustering into per-run sittings. Adding more cleanup to
//! `Drop` would not have reaped a single one of them.
//!
//! So this module does the two things that survive a hard kill, and the
//! `Drop` impls stay as the fast path for the ordinary case:
//!
//! ### 1. The children die with the parent (a watchdog + a process group)
//!
//! `prctl(PR_SET_PDEATHSIG)` is Linux-only, and polling for the parent's
//! pid to disappear races pid reuse. The portable Unix equivalent is a
//! pipe whose write end only this process holds: when this process dies —
//! for ANY reason, `SIGKILL` included — the kernel closes it, and a reader
//! on the other end sees EOF. [`FixtureGroup`] spawns a tiny `/bin/sh`
//! watchdog holding that read end as its stdin, makes it a process-group
//! leader, and places every fixture child in its group. On EOF the
//! watchdog signals the whole group. On an ORDERLY shutdown the harness
//! writes `stand-down` instead, and the watchdog exits without signaling.
//!
//! Using the GROUP rather than a list of pids is what makes this safe
//! against pid reuse: a process group id cannot be recycled while the
//! group still has a member, and the watchdog is a member for as long as
//! it is alive. A pid-list watchdog would signal whatever had inherited
//! the number of a child the harness had already reaped.
//!
//! ### 2. Whatever still got through is reaped at the NEXT startup
//!
//! A watchdog that is itself `SIGKILL`ed, a panic between spawn and
//! registration, a machine that lost power — [`FixtureGroup`] also writes
//! a registry file naming the owning test binary and every fixture it
//! spawned, and [`sweep_stale_registries`] runs once per test process
//! before the first fixture starts. Sweeping on STARTUP is what fixes a
//! machine that already has a backlog: it needs no cooperation from a
//! process that has already died.
//!
//! ## Why the sweep cannot kill a real server
//!
//! Identifying a harness fixture by process NAME would kill the
//! developer's own Homebrew-managed `redis-server` on 6379 — three of
//! those 63 were legitimate. Identifying it by "an ephemeral port" is no
//! better; any program may hold one. So the sweep does not pattern-match
//! at all. It kills a pid only when ALL of the following hold:
//!
//! - the pid appears in a registry file **this harness wrote**, and
//! - the owning test binary named in that file is **gone** (a live owner
//!   means a concurrently-running sibling run — the six e2e binaries run
//!   in parallel under one `cargo test` — and its file is skipped whole),
//!   and
//! - the live process's `ps` start timestamp still matches, to the
//!   second, what was recorded when it was spawned.
//!
//! A process the harness never spawned is never in a registry file, so no
//! rule can reach it. And an owner we cannot identify at all is treated as
//! "unknown", not as "dead" — see the fail-closed comment in
//! [`sweep_stale_registries_in`].
//!
//! The third condition is what defeats pid reuse, and it is worth being
//! precise about how strongly: `lstart` is ABSOLUTE WALL-CLOCK, so the
//! defense is structural, not probabilistic. A process wearing a recycled
//! pid necessarily started after the one that released it, and therefore
//! renders a strictly later timestamp than a record written in the past.
//! It is not that a collision is unlikely; under a monotonic wall clock it
//! is unreachable. The residual holes are all clock discontinuities — a DST
//! fall-back hour, an NTP step backwards, a VM snapshot restore — and each
//! would additionally need a full pid wraparound landing on the same number
//! inside that same window.
//!
//! ### Why the recorded COMMAND is not part of the match
//!
//! It was, in the first cut, and that made the sweep structurally unable
//! to reap the fixture it exists for. **`redis-server` rewrites its own
//! process title within a second of exec** — measured on redis 8.6.3:
//! `ps` reports `redis-server --port 60603 --save  --appendonly no --dir
//! … --protected-mode no` at spawn and `redis-server 127.0.0.1:60603` a
//! second later. A registry written at spawn therefore disagrees with
//! every later reading of the same living process, so a command
//! comparison rejects exactly the orphans it is pointed at. Measured on a
//! real leaked pair: the `darkmux serve` daemon matched and the redis did
//! not.
//!
//! The command is still RECORDED, because a registry a human can read
//! tells them what a pid was before they kill it. It is not compared.
//! `started` carries the whole pid-reuse argument on its own and does not
//! depend on a process leaving its own argv alone.
//! ## Every other child the harness spawns (#2716)
//!
//! Enumerated rather than assumed, because fixing one leaking child and
//! implying the rest is how the next count gets to 60. `Command::new` in
//! `tests/e2e/` and `tests/e2e_*.rs`, classified by LIFETIME — an
//! unbounded one is the leak shape, a self-terminating one is not:
//!
//! | site | child | lifetime | guarded |
//! |---|---|---|---|
//! | `harness.rs` `spawn_redis` | `redis-server` | unbounded — runs until signaled | YES |
//! | `harness.rs` `spawn_daemon` | `darkmux serve` | unbounded — runs until signaled | YES |
//! | `harness.rs` `run_cargo_build_release` | `cargo build --release` | self-terminating | no |
//! | `harness.rs` `FleetNode::cmd` callers | a `darkmux` CLI verb | self-terminating | no |
//! | `harness.rs` isolation unit test | `sleep 30` | self-terminating | no |
//! | this module | the `/bin/sh` watchdog | ends at EOF, i.e. when the harness does | n/a |
//! | this module | `/bin/sh -c 'kill …'`, `ps` | instant | n/a |
//! | `e2e_*.rs` | `redis-server --version` | instant | n/a |
//! | `e2e_fixture_hard_kill.rs` | a re-exec of the test binary | killed by its parent, 120s self-limit | n/a |
//!
//! Only the first two had the shape that produced the 60 orphans: a
//! process that never exits on its own, so an unreaped one lives until
//! the machine reboots. The unguarded rows all end by themselves — a
//! build completes, a CLI verb returns — so an orphaned one costs a
//! bounded amount of time, not a permanent resident. They are listed
//! anyway so a future reader can see they were considered rather than
//! missed. **A new child with an unbounded lifetime belongs in the group**
//! ([`FixtureGroup::place`]) and in the registry ([`FixtureGroup::register`]);
//! add a row here when you add one.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// The `/bin/sh` watchdog. Reads lines from its stdin, which is the read
/// end of a pipe whose only write end lives in the harness process.
///
/// - `stand-down` on stdin → exit quietly; the harness is tearing its own
///   fixtures down in `Drop` and will handle them.
/// - EOF → the harness is GONE (exit, panic, `SIGKILL`, `Ctrl-C`). Signal
///   the whole process group, which is every fixture child plus this
///   shell. `$$` is the shell's own pid and, because the shell was made a
///   group leader at spawn, is also the group id.
///
/// `SIGTERM` only, deliberately: `redis-server` and `darkmux serve` both
/// terminate on it, and escalating here would need a `sleep` that is
/// itself in the group being signaled, so it would be cut short by the
/// very TERM it is waiting out.
///
/// That leaves survivors to the startup sweep, and the sweep has to
/// actually be able to catch one — which is why [`reap_confirmed`] waits
/// for the process to go, escalates to `KILL`, and KEEPS the registry
/// entry when it cannot confirm. An earlier cut signalled TERM and deleted
/// the record unconditionally, which made this sentence false: a fixture
/// that ignored TERM outlived both the group signal and the sweep, and was
/// then unreachable forever.
const WATCHDOG_SCRIPT: &str = r#"
while IFS= read -r line; do
  if [ "$line" = "stand-down" ]; then
    exit 0
  fi
done
kill -TERM -$$ 2>/dev/null
exit 0
"#;

/// A process identified by more than its pid. The pid alone is a number
/// the kernel recycles; `started` + `command` pin it to one specific
/// process, so a record written minutes or days ago can be checked
/// against what is running NOW without any chance of hitting a stranger
/// that inherited the number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcId {
    pub pid: u32,
    /// `ps -o lstart=` — the process's start time.
    pub started: String,
    /// `ps -o command=` — the full argv, joined.
    pub command: String,
}

/// Ask the OS what the process at `pid` is right now. `None` when there
/// is no such RUNNING process.
///
/// `state` is in the query, and a `Z` is treated as absent, because a
/// process that has exited but whose parent has not yet `wait`ed is still
/// listed by `ps` with exit status 0 — measured: `Z    Tue Sep 15
/// 20:20:42 2026     <defunct>`. Without that, a killed child reads as
/// alive for as long as its parent holds the `Child` handle, which makes
/// any "is it gone yet" assertion built on this function pass no matter
/// what happened. That is not hypothetical: it silently passed a mutation
/// of `stand_down` that should have been caught (#2716).
pub fn identify(pid: u32) -> Option<ProcId> {
    let out = ps_query(pid).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    parse_ps_line(&line).map(|(started, command)| ProcId {
        pid,
        started,
        command,
    })
}

/// The `ps` invocation behind [`identify`], built separately so a test can
/// assert its environment rather than infer it.
///
/// `LC_ALL=C` is load-bearing, not hygiene. `lstart` renders through the
/// locale, and only C / en_US produce the 24-character `asctime(3)` shape
/// [`split_lstart`] slices at — measured: `de_DE` 25, `fr_FR` 27, `ja_JP`
/// multibyte. The field is padded to 28 so a wrong width cannot bleed into
/// the command and mis-identify a process, i.e. this is NOT a wrongful-kill
/// vector. What it breaks is quieter: the recorded command is mangled, and
/// a locale that CHANGES between the run that wrote a record and the run
/// that reads it makes every record unmatchable, disabling the sweep with
/// no signal at all. Pinning the locale also makes the 24 true by
/// construction instead of true by assumption.
fn ps_query(pid: u32) -> Command {
    let mut cmd = Command::new("ps");
    cmd.args(["-o", "state=,lstart=,command=", "-p", &pid.to_string()])
        .env("LC_ALL", "C");
    cmd
}

/// Parse one `ps -o state=,lstart=,command=` line into `(started,
/// command)`, or `None` for an empty line or a zombie. Split out from
/// [`identify`] so the shapes can be asserted without spawning anything.
fn parse_ps_line(line: &str) -> Option<(String, String)> {
    let line = line.trim_start();
    let (state, rest) = line.split_once(char::is_whitespace)?;
    if state.starts_with('Z') {
        return None; // exited; waiting to be reaped. Not running.
    }
    // `lstart` is a fixed-shape 24-character `asctime(3)` string ("Mon Sep
    // 15 19:03:37 2026", day-of-month space-padded), so the split point is
    // the 24th character, not a whitespace run — the command that follows
    // is full of spaces.
    let (started, command) = split_lstart(rest.trim_start())?;
    Some((started.to_string(), command.to_string()))
}

/// Split a `ps -o lstart=,command=` line into its two fields. `lstart`
/// renders as `asctime(3)` — `Www Mmm dd hh:mm:ss yyyy`, 24 characters,
/// day-of-month space-padded — and the command line follows after
/// whitespace.
fn split_lstart(line: &str) -> Option<(&str, &str)> {
    const LSTART_LEN: usize = 24;
    if line.len() <= LSTART_LEN || !line.is_char_boundary(LSTART_LEN) {
        return None;
    }
    let (started, rest) = line.split_at(LSTART_LEN);
    Some((started.trim(), rest.trim()))
}

impl ProcId {
    fn to_line(&self, role: &str) -> String {
        // Tab-separated because `started` contains spaces and `command`
        // contains spaces AND may contain anything else except a newline.
        format!("{role}\t{}\t{}\t{}\n", self.pid, self.started, self.command)
    }

    fn from_line(line: &str) -> Option<(String, ProcId)> {
        let mut parts = line.splitn(4, '\t');
        let role = parts.next()?.to_string();
        let pid: u32 = parts.next()?.parse().ok()?;
        let started = parts.next()?.to_string();
        let command = parts.next()?.to_string();
        Some((
            role,
            ProcId {
                pid,
                started,
                command,
            },
        ))
    }

    /// True when the process at this pid is STILL the one that was
    /// recorded. The test is the start TIME, not the command line: see
    /// the module doc's "Why the recorded COMMAND is not part of the
    /// match" — `redis-server` rewrites its own title seconds after exec,
    /// so comparing commands rejects the living fixture. A process wearing
    /// a recycled pid started after the one that released it, so its start
    /// time differs.
    pub fn still_running(&self) -> bool {
        identify(self.pid).is_some_and(|live| live.started == self.started)
    }
}

/// Parse a registry file into its `(role, ProcId)` entries. Exposed so a
/// test can assert that the fixtures a REAL harness boot spawned are
/// still matchable by the sweep's own rule some seconds later — the case
/// a synthetic `sleep` stand-in cannot exercise, because `sleep` does not
/// rewrite its process title and `redis-server` does.
pub fn registry_entries(path: &Path) -> Vec<(String, ProcId)> {
    let Ok(body) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    body.lines().filter_map(ProcId::from_line).collect()
}

/// Name this run's registry file.
///
/// The sequence number is load-bearing, not decoration. `pid` plus a
/// timestamp looks unique and is not: `SystemTime` resolves to
/// MICROseconds on macOS despite `as_nanos`, and libtest runs a binary's
/// tests on concurrent threads, so two `FixtureGroup::arm()` calls in one
/// process land on the same name whenever they fall in the same
/// microsecond. Both runs then share a registry file and either can delete
/// the other's — which is the live-run-unregistered failure in person.
///
/// Observed, not theorized: `29482-1789477487883790000.fixtures` (note the
/// trailing zeroes) was created by one test and removed by a concurrent
/// one, and the boot failed with "opening the fixture registry … No such
/// file or directory". It only failed loudly because `register` had just
/// been made loud; before that it was a silent unregistration for the whole
/// run.
fn registry_file_name(owner_pid: u32, nanos: u128) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{owner_pid}-{nanos}-{seq}.fixtures")
}

/// Where registry files live: under the workspace's `target/`, which is
/// already gitignored and is where the e2e harness keeps its other
/// cross-process state (the release-build `flock` file). Deliberately the
/// manifest's own `target/`, not `CARGO_TARGET_DIR`, so a run in a
/// `scripts/test-lane.sh` lane shares one registry with an ordinary run
/// and each sweeps the other's leftovers.
fn registry_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/.e2e-fixtures")
}

/// Reap fixtures left behind by test processes that are no longer
/// running. Runs once per test process, memoized, before the first
/// fixture of that process is spawned. Returns how many processes it
/// signaled.
pub fn sweep_stale_registries() -> usize {
    static SWEPT: OnceLock<usize> = OnceLock::new();
    *SWEPT.get_or_init(|| sweep_stale_registries_in(&registry_dir()))
}

/// The body of [`sweep_stale_registries`], against an explicit directory
/// so tests can drive it over a registry they built themselves.
pub fn sweep_stale_registries_in(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0; // no registry dir yet: nothing has ever run here.
    };
    let mut reaped = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("fixtures") {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut owner: Option<ProcId> = None;
        let mut children: Vec<ProcId> = Vec::new();
        for line in body.lines() {
            match ProcId::from_line(line) {
                Some((role, proc)) if role == "owner" => owner = Some(proc),
                Some((role, proc)) if role == "child" => children.push(proc),
                _ => {}
            }
        }
        // FAIL CLOSED on both readings of the owner line.
        //
        // A live owner is a CONCURRENT run, not an orphan — the e2e
        // binaries can run in parallel, so without this the first one to
        // boot reaps a sibling's live redis out from under it.
        //
        // And `None` — no owner line, or one that did not parse — does not
        // mean "the owner is dead", it means WE CANNOT TELL WHOSE RUN THIS
        // IS. Reading that as authorization to kill every child named in
        // the file is the one shape that turns an unlucky write into a
        // wrongful kill: `arm` writes the owner line first, so a torn or
        // truncated write (ENOSPC, a full tmpfs) leaves a first line that
        // fails to parse while the `child` lines behind it parse fine. A
        // run has one redis plus one daemon per node, so "two or more
        // children" is the normal case, not an edge one. Unknown owner =>
        // touch nothing, and leave the file rather than deleting evidence
        // we could not act on.
        if owner.as_ref().is_none_or(ProcId::still_running) {
            continue;
        }
        let mut unconfirmed: Vec<ProcId> = Vec::new();
        for child in &children {
            if !child.still_running() {
                continue;
            }
            if reap_confirmed(child) {
                reaped += 1;
            } else {
                unconfirmed.push(child.clone());
            }
        }
        // Only now is the record disposable. Removing it before confirming
        // the kill would destroy the only handle anything has on a fixture
        // that ignored the signal — and the watchdog's TERM-only design is
        // justified by "the startup sweep catches survivors", which is only
        // true if the sweep keeps its own record of one.
        rewrite_or_remove(&path, owner.as_ref(), &unconfirmed);
    }
    reaped
}

/// How long to wait for a signalled fixture to actually go away before
/// escalating, and then before giving up on it. Short: the fixtures are a
/// `redis-server` and a `darkmux serve`, both of which exit on TERM in
/// milliseconds. This only costs wall clock when something is wedged.
const SIGNAL_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Signal a fixture and CONFIRM it is gone, escalating TERM -> KILL.
/// Returns false when it is still running after both, which is the caller's
/// cue to keep the record rather than delete it.
fn reap_confirmed(child: &ProcId) -> bool {
    signal_term(child.pid);
    if wait_until_gone(child, SIGNAL_GRACE) {
        return true;
    }
    signal_kill(child.pid);
    wait_until_gone(child, SIGNAL_GRACE)
}

fn wait_until_gone(child: &ProcId, within: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + within;
    loop {
        if !child.still_running() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Dispose of a swept registry file: delete it when every fixture it named
/// is confirmed gone, otherwise rewrite it holding just the survivors so a
/// later sweep still knows they exist.
///
/// Split out from the sweep because the survivor branch cannot be reached
/// from a test — it needs a process that ignores both TERM and KILL, and
/// KILL cannot be ignored — so the behavior is asserted here directly.
fn rewrite_or_remove(path: &Path, owner: Option<&ProcId>, unconfirmed: &[ProcId]) {
    if unconfirmed.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    let mut body = String::new();
    if let Some(owner) = owner {
        body.push_str(&owner.to_line("owner"));
    }
    for child in unconfirmed {
        body.push_str(&child.to_line("child"));
    }
    let _ = write_atomic(path, &body);
}

/// Write a registry file so no reader can ever observe it half-built.
///
/// `fs::write` creates-and-truncates, which leaves a window where the file
/// exists and is EMPTY. A sibling run sweeping the shared directory in that
/// window reads no owner and no children, and — before the fail-closed gate
/// above — would have deleted it, silently unregistering a live run's
/// fixtures for the rest of its life. Temp-file-plus-rename makes the file
/// go from absent to complete in one step.
fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("fixtures.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// `kill -TERM` via the shell builtin — `kill(2)` would mean taking a
/// `libc` dependency this workspace deliberately does not have, and the
/// dep set here is kept small on purpose.
fn signal_term(pid: u32) {
    signal(pid, "TERM");
}

/// The escalation. A fixture is ours by construction here — it is named in
/// a registry file this harness wrote and its start time still matches — so
/// there is no question of whose process this is by the time we get here.
fn signal_kill(pid: u32) {
    signal(pid, "KILL");
}

fn signal(pid: u32, name: &str) {
    let _ = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("kill -{name} {pid} 2>/dev/null"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Owns the watchdog, the process group every fixture child is placed
/// in, and the registry file naming them. One per [`FleetHarness`].
///
/// [`FleetHarness`]: super::harness::FleetHarness
pub struct FixtureGroup {
    /// The `/bin/sh` watchdog, and the group leader. `None` on a platform
    /// with no process groups, where this whole struct degrades to the
    /// registry half.
    watchdog: Option<Child>,
    /// The watchdog's pid, which is also the group id every fixture child
    /// joins.
    pgid: Option<u32>,
    registry: PathBuf,
    /// Set once the watchdog has been told to stand down, so `Drop` after
    /// an explicit teardown is a no-op rather than a second wait.
    stood_down: bool,
}

impl FixtureGroup {
    /// Sweep anything a dead previous run left behind, then arm the
    /// watchdog for this run. Never fails the harness: a watchdog that
    /// could not be spawned leaves the registry half doing its job, which
    /// is strictly better than refusing to run tests — but it SAYS SO on
    /// stderr rather than proceeding with a guard silently absent.
    pub fn arm() -> Self {
        sweep_stale_registries();

        let dir = registry_dir();
        let _ = std::fs::create_dir_all(&dir);
        let owner_pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let registry = dir.join(registry_file_name(owner_pid, nanos));
        if let Some(owner) = identify(owner_pid) {
            // Atomic: see `write_atomic`. A sibling must never see this
            // file in a state where the owner line is missing.
            if let Err(e) = write_atomic(&registry, &owner.to_line("owner")) {
                eprintln!(
                    "[fixture-reaper] could not write the fixture registry at {}: {e}. \
                     This run's fixtures will not be reapable by a later run's startup \
                     sweep; the die-with-parent watchdog is still armed (#2716)",
                    registry.display()
                );
            }
        }

        let (watchdog, pgid) = Self::spawn_watchdog();
        if watchdog.is_none() {
            eprintln!(
                "[fixture-reaper] could not spawn the watchdog, so `place` is a no-op and \
                 nothing will reap this run's fixtures if it is killed. The startup-sweep \
                 half still applies on the NEXT run (#2716)"
            );
        }
        Self {
            watchdog,
            pgid,
            registry,
            stood_down: false,
        }
    }

    /// True once the watchdog has exited — which, on a live run, means the
    /// die-with-parent guard is gone and nobody said so. Checked on every
    /// [`register`], because a run that proceeds with a silently absent
    /// guard is the original defect one layer up.
    ///
    /// [`register`]: FixtureGroup::register
    pub fn watchdog_lost(&mut self) -> bool {
        match self.watchdog.as_mut() {
            // `stood_down` means we asked it to exit; that is not a loss.
            Some(_) if self.stood_down => false,
            Some(watchdog) => matches!(watchdog.try_wait(), Ok(Some(_))),
            None => true,
        }
    }

    #[cfg(unix)]
    fn spawn_watchdog() -> (Option<Child>, Option<u32>) {
        use std::os::unix::process::CommandExt;
        let spawned = Command::new("/bin/sh")
            .arg("-c")
            .arg(WATCHDOG_SCRIPT)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Group LEADER: its own pid becomes the group id, and the
            // group therefore cannot be recycled while it is alive.
            .process_group(0)
            .spawn();
        match spawned {
            Ok(child) => {
                let pgid = child.id();
                (Some(child), Some(pgid))
            }
            Err(_) => (None, None),
        }
    }

    #[cfg(not(unix))]
    fn spawn_watchdog() -> (Option<Child>, Option<u32>) {
        (None, None)
    }

    /// Place a to-be-spawned fixture in the watchdog's process group, so
    /// the watchdog's one group signal reaches it. Call this on every
    /// `Command` the harness spawns as a fixture, BEFORE `.spawn()`.
    pub fn place(&self, cmd: &mut Command) {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            if let Some(pgid) = self.pgid {
                cmd.process_group(pgid as i32);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = cmd;
        }
    }

    /// This run's registry file. Exposed so a test can assert that the
    /// fixtures the harness really spawns are really written into it —
    /// the half of the guard that survives a watchdog which was itself
    /// killed.
    pub fn registry_path(&self) -> &Path {
        &self.registry
    }

    /// Record a spawned fixture in this run's registry file, so a future
    /// run can reap it if this process dies without ever tearing it down.
    ///
    /// Returns `Err` rather than swallowing, and the callers propagate it
    /// into `FleetHarness::boot`'s error. A silently-failed registration is
    /// the guard's second half quietly absent for the whole run, with no
    /// signal — which is exactly the shape that let 60 orphans accumulate
    /// unnoticed, one level up. Failing the boot is loud, and now safe:
    /// `boot`'s error path drops this group without standing the watchdog
    /// down, so the fixture already spawned is reaped on the way out.
    pub fn register(&mut self, child: &Child) -> Result<(), String> {
        if self.watchdog_lost() {
            eprintln!(
                "[fixture-reaper] the watchdog is gone; this run's fixtures will not die \
                 with it. The registry half still applies (#2716)"
            );
        }
        let pid = child.id();
        let proc = identify(pid).ok_or_else(|| {
            format!("fixture pid {pid} was already gone when it was registered")
        })?;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.registry)
            .map_err(|e| {
                format!(
                    "opening the fixture registry {} to record pid {pid}: {e}",
                    self.registry.display()
                )
            })?;
        f.write_all(proc.to_line("child").as_bytes()).map_err(|e| {
            format!(
                "recording pid {pid} in the fixture registry {}: {e}",
                self.registry.display()
            )
        })
    }

    /// Orderly teardown: tell the watchdog NOT to signal the group (the
    /// harness's own `Drop` has already killed, or is about to kill, each
    /// child individually), wait for it to exit, and drop this run's
    /// registry file so the next run has nothing to sweep.
    pub fn stand_down(&mut self) {
        if self.stood_down {
            return;
        }
        self.stood_down = true;
        if let Some(watchdog) = self.watchdog.as_mut() {
            if let Some(stdin) = watchdog.stdin.as_mut() {
                let _ = stdin.write_all(b"stand-down\n");
                let _ = stdin.flush();
            }
            // The token above is what makes it exit QUIETLY. Dropping the
            // pipe is EOF, and EOF is precisely what makes the read loop
            // fall through to the group TERM — so if the write failed, the
            // watchdog signals the group on its way out. That is harmless
            // where this is reached (`FleetHarness::drop` has already
            // killed every child), and is the safe direction to fail in.
            drop(watchdog.stdin.take());
            let _ = watchdog.wait();
        }
        let _ = std::fs::remove_file(&self.registry);
    }
}

// DELIBERATELY NO `impl Drop for FixtureGroup` (#2716).
//
// A `Drop` here reads like tidiness and is the opposite. `stand_down` tells
// the watchdog NOT to signal the group and deletes this run's registry
// file, so running it automatically disarms BOTH halves of the guard at
// every scope exit — including the ones where the fixtures are still alive.
//
// `FleetHarness::boot` is the case that matters. It arms the group, spawns
// redis through it, and then has four fallible steps before it returns:
// `wait_for_redis`, `MockLmStudio::spawn`, `spawn_daemon`, and a
// 15-second `wait_for_daemon_health` TCP poll. On `?` from any of them the
// nodes' own `Drop` kills the daemons, but `redis: Child` drops to NOTHING
// — `std::process::Child` has no killing `Drop` — and a `Drop` here would
// then tell the watchdog to stand down and delete the record. The redis
// would survive with the die-with-parent guard told not to fire and its
// only registry entry removed: a permanent orphan, on the ordinary
// contended-machine timeout path rather than an exotic one.
//
// Without a `Drop`, the field drop closes the watchdog's stdin, the
// watchdog sees EOF, and it TERMs the group — the fixture dies. The
// registry file is left behind for a later sweep, which is the correct
// direction for a run that ended badly.
//
// The success path is unaffected: `FleetHarness::drop` calls `stand_down`
// EXPLICITLY, after killing its children (`harness.rs`), and `stood_down`
// keeps that idempotent.

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in fixture: a real child process this test owns, spawned
    /// OUTSIDE any harness group, standing for "a server the harness did
    /// not start" — the developer's own Homebrew `redis-server` on 6379,
    /// or anyone else's.
    fn spawn_stranger() -> Child {
        Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawning a stand-in stranger process")
    }

    fn write_registry(dir: &Path, name: &str, lines: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{name}.fixtures"));
        std::fs::write(&path, lines).unwrap();
        path
    }

    fn alive(pid: u32) -> bool {
        identify(pid).is_some()
    }

    /// (#2716) `ps` lists an exited-but-unreaped child with exit status
    /// 0, so an "is it still there" check that only asks `ps` whether the
    /// pid resolves answers YES for a process that is already dead. Every
    /// assertion in this module about a fixture being gone rests on this,
    /// and one of them silently passed a mutation because of it. Deleting
    /// the `Z` branch in `parse_ps_line` makes this fail.
    /// (#2716) MUST-FIX 1. `FleetHarness::boot` arms the group, spawns
    /// redis through it, and then has four fallible steps — two timed
    /// waits among them — before it returns. This reproduces that error
    /// path exactly: a fixture is spawned and registered, then BOTH the
    /// group and the `Child` handle go out of scope with no teardown,
    /// which is what `?` does.
    ///
    /// A `Drop for FixtureGroup` calling `stand_down` makes this fail in
    /// both directions at once: the fixture survives (the watchdog was
    /// told not to signal) AND the registry file is deleted (so no later
    /// sweep can find it). Restoring that impl is the mutation.
    #[cfg(unix)]
    #[test]
    fn boot_s_error_path_reaps_its_fixture_rather_than_disarming_both_guards() {
        let mut cmd = Command::new("sleep");
        cmd.arg("60").stdout(Stdio::null()).stderr(Stdio::null());

        let (pid, registry) = {
            let mut group = FixtureGroup::arm();
            group.place(&mut cmd);
            let child = cmd.spawn().expect("spawning the stand-in fixture");
            let pid = child.id();
            group.register(&child).expect("registering the fixture");
            let registry = group.registry_path().to_path_buf();
            // `std::process::Child` has no killing `Drop`, so letting it
            // fall out of scope here is byte-for-byte what `boot`'s `?`
            // does to `redis`.
            std::mem::forget(child);
            (pid, registry)
            // `group` drops here: the early return.
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while identify(pid).is_some() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let survived = identify(pid).is_some();
        if survived {
            // Never leave the probe's own orphan behind, whatever the
            // assertion decides.
            signal_kill(pid);
        }
        assert!(
            !survived,
            "the fixture outlived a harness that errored out after spawning it. `boot` has \
             four fallible steps after `spawn_redis`, two of them timeouts on a contended \
             machine, and `redis: Child` has no killing `Drop` — so if the group also \
             stands the watchdog down on the way out, nothing reaps it and the registry \
             entry that would have let a later sweep find it is deleted too (#2716)"
        );
        assert!(
            registry.exists(),
            "a run that ended badly must LEAVE its registry file behind — that record is \
             the only thing a later startup sweep can act on (#2716)"
        );
        let _ = std::fs::remove_file(&registry);
    }

    /// (#2716) MUST-FIX 2. An owner line that does not parse means the
    /// owner is UNKNOWN, not dead. `arm` writes that line first, so a torn
    /// write leaves exactly this shape: no usable owner, and `child` lines
    /// behind it that parse fine. Reading it as authorization to kill is
    /// the only path by which a process the harness did not start gets
    /// signaled. Flipping the gate back to `is_some_and` kills the
    /// stranger here.
    #[test]
    fn an_ownerless_registry_authorizes_nothing_and_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let mut stranger = spawn_stranger();
        let pid = stranger.id();
        let child = identify(pid).expect("child must be identifiable");
        // A REAL, current identity on the child line — so the only thing
        // standing between the sweep and this process is the owner gate.
        let path = write_registry(
            tmp.path(),
            "torn-owner-write",
            &format!("owner\tnot-a-pid\ttruncated\n{}", child.to_line("child")),
        );

        let reaped = sweep_stale_registries_in(tmp.path());

        assert_eq!(reaped, 0, "an unidentifiable owner must authorize no kills");
        assert!(
            alive(pid),
            "the sweep killed a process named in a registry file whose owner it could not \
             identify. `None` there means 'we cannot tell whose run this is', and the only \
             safe reading of that is to touch nothing (#2716)"
        );
        assert!(
            path.exists(),
            "a file the sweep could not act on must be left in place, not deleted — \
             removing evidence we declined to act on is how the next reader loses the \
             ability to act on it either (#2716)"
        );
        let _ = stranger.kill();
        let _ = stranger.wait();
    }

    /// (#2716) The sweep must CONFIRM a kill, not assume one. A fixture
    /// that ignores TERM is reaped by the escalation to KILL. Deleting the
    /// `signal_kill` escalation in `reap_confirmed` leaves this process
    /// alive and the count at zero.
    #[cfg(unix)]
    #[test]
    fn sweep_escalates_to_kill_when_a_fixture_ignores_term() {
        let tmp = tempfile::tempdir().unwrap();
        let mut stubborn = Command::new("/bin/sh")
            .arg("-c")
            .arg("trap '' TERM; while :; do sleep 1; done")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawning a TERM-immune fixture");
        let pid = stubborn.id();
        let child = identify(pid).expect("child must be identifiable");
        write_registry(
            tmp.path(),
            "stubborn",
            &format!(
                "owner\t0\tThu Jan  1 00:00:00 1970\t/nonexistent/owner\n{}",
                child.to_line("child")
            ),
        );

        let reaped = sweep_stale_registries_in(tmp.path());
        let gone = !alive(pid);
        // Take the readings BEFORE cleaning up, then clean up
        // unconditionally. `stubborn` ignores TERM, so a bare `wait()`
        // here blocks FOREVER whenever the escalation under test is
        // absent — which is exactly the mutation this test exists to
        // catch. Measured: it wedged a run for ten minutes and left the
        // fixture behind. A test that probes for a missing kill must not
        // depend on that kill having happened.
        if !gone {
            signal_kill(pid);
        }
        let _ = stubborn.wait();

        assert_eq!(
            reaped, 1,
            "a fixture that ignores TERM must still be reaped — the watchdog sends TERM only, \
             and its own justification for that is that the startup sweep catches survivors \
             (#2716)"
        );
        assert!(gone, "the TERM-immune fixture survived the sweep");
    }

    /// (#2716) The other half of confirmation: a fixture the sweep could
    /// NOT confirm keeps its registry entry, so a later sweep still has a
    /// handle on it. Unreachable through `sweep_stale_registries_in`
    /// itself — it would need a process that ignores KILL, which does not
    /// exist — so the disposal step is asserted directly.
    #[test]
    fn an_unconfirmed_fixture_keeps_its_record_instead_of_losing_it() {
        let tmp = tempfile::tempdir().unwrap();
        let owner = ProcId {
            pid: 4242,
            started: "Thu Jan  1 00:00:00 1970".to_string(),
            command: "/nonexistent/owner".to_string(),
        };
        let survivor = ProcId {
            pid: 4243,
            started: "Thu Jan  1 00:00:01 1970".to_string(),
            command: "redis-server --port 1".to_string(),
        };
        let path = tmp.path().join("kept.fixtures");

        rewrite_or_remove(&path, Some(&owner), std::slice::from_ref(&survivor));
        let kept = registry_entries(&path);
        assert_eq!(
            kept,
            vec![
                ("owner".to_string(), owner.clone()),
                ("child".to_string(), survivor),
            ],
            "a fixture that survived both signals must stay in the registry, with its owner \
             line intact so a later sweep still reads the file as orphaned (#2716)"
        );

        rewrite_or_remove(&path, Some(&owner), &[]);
        assert!(
            !path.exists(),
            "with every fixture confirmed gone the record is disposable and should go"
        );
    }

    /// (#2716) A registry file must never be observable half-built. A
    /// sibling run sweeping the shared directory in the window `fs::write`
    /// opens — created, truncated, not yet filled — reads no owner and no
    /// children.
    ///
    /// Asserting "the content is right afterwards" would NOT catch that:
    /// `fs::write` gets there too, just via an observable empty state. So
    /// this asserts the distinguishing fact instead — that the file is
    /// REPLACED rather than mutated in place. A handle opened before the
    /// write still sees the old bytes after a rename, because it holds the
    /// old inode; a truncate-in-place would have changed the bytes under
    /// it. Swapping `write_atomic`'s body for `fs::write` fails here.
    #[test]
    fn the_registry_is_replaced_not_truncated_in_place() {
        use std::io::Read;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("atomic.fixtures");
        let before = "owner\t1\tstale\told\n";
        std::fs::write(&path, before).unwrap();
        let mut held_open = std::fs::File::open(&path).expect("holding the pre-write inode");

        write_atomic(&path, "owner\t2\tfresh\tnew-and-rather-longer\n").expect("write");

        let mut seen_through_the_old_handle = String::new();
        held_open
            .read_to_string(&mut seen_through_the_old_handle)
            .expect("reading through the pre-write handle");
        assert_eq!(
            seen_through_the_old_handle, before,
            "the registry was mutated in place rather than replaced, so a reader that opened \
             it a moment earlier had the bytes changed underneath it — which is the same \
             window in which a sibling run sees an owner-less, child-less file (#2716)"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "owner\t2\tfresh\tnew-and-rather-longer\n",
            "a fresh reader must see the complete new content"
        );
        assert!(
            !tmp.path().join("atomic.fixtures.tmp").exists(),
            "the temp file must be renamed away, not left beside the registry"
        );
    }

    /// (#2716) Two runs in one process must never share a registry file.
    /// `pid` + `SystemTime` is not unique — macOS resolves to microseconds
    /// and libtest runs tests concurrently — and two groups on one path
    /// means either can delete the other's record while it is live.
    /// Asserted at the same timestamp on purpose, since that is exactly
    /// the case a wall-clock-only name gets wrong. Dropping the sequence
    /// number makes these equal.
    #[test]
    fn two_registry_names_at_the_same_instant_still_differ() {
        assert_ne!(
            registry_file_name(4242, 1_789_477_487_883_790_000),
            registry_file_name(4242, 1_789_477_487_883_790_000),
            "two FixtureGroups armed in the same microsecond of the same process got the same \
             registry path, so each can delete the other's live record (#2716)"
        );
    }

    /// (#2716) A failed registration must be LOUD. It opens with `append`
    /// and no `create`, so a sibling that deleted the file (or any other
    /// write failure) leaves this run's fixtures unregistered for its whole
    /// lifetime. Swallowing that in an `if let Ok` is the repo's
    /// no-silent-wrong-key rule inverted.
    #[test]
    fn register_reports_a_failure_instead_of_swallowing_it() {
        let mut group = FixtureGroup::arm();
        std::fs::remove_file(group.registry_path()).expect("removing the registry");
        let mut child = spawn_stranger();

        let err = group
            .register(&child)
            .expect_err("registering into a missing registry file must fail");
        assert!(
            err.contains("fixture registry"),
            "the error must name what could not be recorded; got {err:?}"
        );

        let _ = child.kill();
        let _ = child.wait();
        group.stand_down();
    }

    /// (#2716) A watchdog that died takes the die-with-parent half of the
    /// guard with it, and nothing noticed. Deleting the `try_wait` arm
    /// makes this report a live watchdog forever.
    #[cfg(unix)]
    #[test]
    fn a_dead_watchdog_is_detected_rather_than_assumed_alive() {
        let mut group = FixtureGroup::arm();
        assert!(
            !group.watchdog_lost(),
            "a freshly armed group has a live watchdog"
        );

        let pgid = group.pgid.expect("watchdog must have spawned");
        // A positive pid signals the watchdog ALONE, not its group.
        signal_kill(pgid);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !group.watchdog_lost() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }

        assert!(
            group.watchdog_lost(),
            "the watchdog exited and the group still reports it alive, so a run would carry \
             on with the guard silently absent (#2716)"
        );
        group.stand_down();
    }

    /// (#2716) `lstart` renders through the locale and only C / en_US give
    /// the 24-character shape `split_lstart` slices at. Dropping the
    /// `LC_ALL` pin makes the width an assumption about the developer's
    /// environment instead of a fact about the command.
    #[test]
    fn the_ps_query_pins_the_locale() {
        let cmd = ps_query(std::process::id());
        let lc_all = cmd
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new("LC_ALL"))
            .and_then(|(_, v)| v);
        assert_eq!(
            lc_all,
            Some(std::ffi::OsStr::new("C")),
            "`ps -o lstart=` is locale-formatted — de_DE renders 25 characters, fr_FR 27, \
             ja_JP multibyte. Unpinned, a locale change between the run that writes a record \
             and the run that reads it makes every record unmatchable and disables the sweep \
             with no signal (#2716)"
        );
    }

    #[test]
    fn a_zombie_reads_as_gone_not_as_running() {
        assert_eq!(
            parse_ps_line("Z    Tue Sep 15 20:20:42 2026     <defunct>"),
            None,
            "a zombie is an exited process; treating it as running makes every 'it is gone \
             now' assertion in this module vacuous (#2716)"
        );
        assert_eq!(
            parse_ps_line("S    Tue Sep 15 20:20:42 2026     sleep 20"),
            Some(("Tue Sep 15 20:20:42 2026".to_string(), "sleep 20".to_string())),
            "a running process must still parse into its start time and command"
        );
        assert_eq!(parse_ps_line(""), None, "an empty ps result is not a process");
    }

    /// (#2716) The live half of the same guard, against a real child
    /// rather than a captured `ps` line — so a change in `ps`'s output
    /// shape cannot leave the parser test passing over a format nothing
    /// emits any more.
    #[test]
    fn identify_reports_a_killed_but_unreaped_child_as_gone() {
        let mut child = spawn_stranger();
        let pid = child.id();
        assert!(identify(pid).is_some(), "the child must be visible while running");
        let _ = child.kill();
        // Deliberately NOT `child.wait()`: this test is about the window
        // where the process has exited and its parent still holds the
        // handle, which is exactly the window every teardown assertion
        // runs in.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while identify(pid).is_some() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(
            identify(pid).is_none(),
            "a killed child whose parent has not reaped it must read as gone (#2716)"
        );
        let _ = child.wait();
    }

    #[test]
    fn identify_round_trips_a_live_process_through_the_registry_line() {
        let mut child = spawn_stranger();
        let proc = identify(child.id()).expect("a just-spawned child must be identifiable");
        assert_eq!(proc.pid, child.id());
        assert!(
            proc.command.contains("sleep"),
            "command field should carry the argv, got {:?}",
            proc.command
        );
        let line = proc.to_line("child");
        let (role, parsed) = ProcId::from_line(line.trim_end()).expect("line must parse back");
        assert_eq!(role, "child");
        assert_eq!(parsed, proc, "a registry line must round-trip exactly");
        let _ = child.kill();
        let _ = child.wait();
    }

    /// (#2716) The guard that stops the sweep from killing a server it
    /// never spawned. The registry names a live pid, but the recorded
    /// START TIME is not the running process's — which is precisely what
    /// a recycled pid looks like, since whatever inherited the number
    /// started after the process that released it. Weakening
    /// `ProcId::still_running` to a bare existence check makes this kill
    /// the stranger.
    #[test]
    fn sweep_leaves_a_process_whose_recorded_start_time_does_not_match() {
        let tmp = tempfile::tempdir().unwrap();
        let mut stranger = spawn_stranger();
        let pid = stranger.id();
        // Owner line names pid 0, which never exists → the file looks
        // orphaned and IS swept; the child line names a live pid with a
        // deliberately wrong start time and command.
        write_registry(
            tmp.path(),
            "mismatched",
            &format!(
                "owner\t0\tThu Jan  1 00:00:00 1970\t/nonexistent/owner\n\
                 child\t{pid}\tThu Jan  1 00:00:00 1970\tredis-server --port 12345\n"
            ),
            // NB the recorded command here is also wrong, and that is
            // deliberately NOT what this asserts — the command is not
            // compared (see the module doc). The start time is.
        );

        sweep_stale_registries_in(tmp.path());

        assert!(
            alive(pid),
            "the sweep killed pid {pid}, whose recorded start time is not the running \
             process's. That is the pid-reuse path, and on a developer machine the process \
             wearing a recycled pid is as likely to be their own Homebrew redis-server as \
             anything else (#2716)."
        );
        let _ = stranger.kill();
        let _ = stranger.wait();
    }

    /// Return `started` with its seconds field moved by exactly one,
    /// staying inside the same minute and therefore the same calendar day.
    /// `asctime(3)` is fixed-shape — `Www Mmm dd hh:mm:ss yyyy` — so the
    /// seconds live at bytes 17..19.
    fn one_second_off(started: &str) -> String {
        let secs: u32 = started[17..19]
            .parse()
            .unwrap_or_else(|e| panic!("seconds field of {started:?}: {e}"));
        let shifted = if secs == 59 { 58 } else { secs + 1 };
        format!("{}{shifted:02}{}", &started[..17], &started[19..])
    }

    /// (#2716) Gate 3 is the entire pid-reuse defense, and it is a
    /// SECOND-resolution rule. Nothing pinned that.
    ///
    /// Every other start-time test here records `Thu Jan  1 00:00:00
    /// 1970`, which is fifty-six years off the live value. That pins "a
    /// wildly wrong timestamp is rejected" and nothing finer: weakening
    /// `ProcId::still_running` from full-string equality to a date-only
    /// comparison — two applied lines — left the whole suite at EXIT=0,
    /// 18 passed. A day-granularity rule is precisely the weakening that
    /// would matter on a machine cycling its pid space fast enough to
    /// wrap within a day, and it shipped green.
    ///
    /// So this probe is deliberately the smallest difference the rule is
    /// supposed to see: a real live process, recorded at a time one second
    /// from its actual start, on the same day, in the same minute. Shipped,
    /// that reaps nothing. Date-only, it reaps and kills.
    #[test]
    fn sweep_leaves_a_process_recorded_one_second_off_on_the_same_day() {
        let tmp = tempfile::tempdir().unwrap();
        let mut stranger = spawn_stranger();
        let pid = stranger.id();
        let live = identify(pid).expect("the stranger must be identifiable");
        let recorded = ProcId {
            started: one_second_off(&live.started),
            ..live.clone()
        };

        // Self-checks on the PROBE, so a mistake in the shift cannot leave
        // this test passing for the wrong reason. The two timestamps must
        // differ ONLY in the seconds — a date-granularity rule then cannot
        // tell them apart, and a second-granularity one must.
        assert_ne!(
            recorded.started, live.started,
            "the probe must actually differ from the live start time"
        );
        assert_eq!(
            recorded.started[..17],
            live.started[..17],
            "the probe must keep the same day, hour and minute — otherwise it is not \
             testing second resolution"
        );
        assert_eq!(
            recorded.started[19..],
            live.started[19..],
            "the probe must keep the same year"
        );

        write_registry(
            tmp.path(),
            "one-second-off",
            &format!(
                "owner\t0\tThu Jan  1 00:00:00 1970\t/nonexistent/owner\n{}",
                recorded.to_line("child")
            ),
        );

        let reaped = sweep_stale_registries_in(tmp.path());
        let survived = alive(pid);
        // Readings taken; clean up before asserting so a failure cannot
        // leave the probe's own process behind either way.
        let _ = stranger.kill();
        let _ = stranger.wait();

        assert_eq!(
            reaped, 0,
            "the sweep acted on a record whose start time is one second off the live \
             process. Gate 3 is second-resolution: it is what makes a recycled pid \
             unmatchable, since the process wearing the number started strictly later \
             than the record. A coarser comparison is a real widening of the only rule \
             standing between the sweep and a stranger (#2716)"
        );
        assert!(
            survived,
            "the sweep KILLED a live process recorded one second off, on the same day, in \
             the same minute (#2716)"
        );
    }

    /// (#2716) A process that is in NO registry file is unreachable by
    /// the sweep, whatever it is called or which port it holds. This is
    /// the property that makes "sweep on startup" safe to do at all.
    #[test]
    fn sweep_cannot_reach_a_process_no_registry_names() {
        let tmp = tempfile::tempdir().unwrap();
        let mut stranger = spawn_stranger();
        let pid = stranger.id();
        write_registry(
            tmp.path(),
            "someone-else",
            "owner\t0\tThu Jan  1 00:00:00 1970\t/nonexistent/owner\n",
        );

        sweep_stale_registries_in(tmp.path());

        assert!(
            alive(pid),
            "a process no registry file names must be unreachable by the sweep (#2716)"
        );
        let _ = stranger.kill();
        let _ = stranger.wait();
    }

    /// (#2716) The guard that stops a booting run from reaping a
    /// CONCURRENT one. The six e2e binaries run in parallel under a
    /// single `cargo test`; without the live-owner skip the first to boot
    /// kills every sibling's redis. Deleting the `still_running` check on
    /// `owner` makes this kill the live run's fixture.
    #[test]
    fn sweep_skips_a_registry_whose_owner_is_still_running() {
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = spawn_stranger();
        let pid = fixture.id();
        // THIS test process stands in for the live owner.
        let owner = identify(std::process::id()).expect("self must be identifiable");
        let child = identify(pid).expect("child must be identifiable");
        let path = write_registry(
            tmp.path(),
            "live-owner",
            &format!("{}{}", owner.to_line("owner"), child.to_line("child")),
        );

        sweep_stale_registries_in(tmp.path());

        assert!(
            alive(pid),
            "the sweep killed a fixture whose owning test binary is still running — that is a \
             concurrently-executing sibling e2e binary, not an orphan (#2716)"
        );
        assert!(
            path.exists(),
            "a live owner's registry file must be left in place for that run to remove on its \
             own way out (#2716)"
        );
        let _ = fixture.kill();
        let _ = fixture.wait();
    }

    /// (#2716) The orphan case the sweep exists for: the owner is gone
    /// and the recorded identity still matches, so the fixture is reaped.
    #[test]
    fn sweep_reaps_a_fixture_whose_owner_is_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let mut fixture = spawn_stranger();
        let pid = fixture.id();
        let child = identify(pid).expect("child must be identifiable");
        write_registry(
            tmp.path(),
            "dead-owner",
            &format!(
                "owner\t0\tThu Jan  1 00:00:00 1970\t/nonexistent/owner\n{}",
                child.to_line("child")
            ),
        );

        let reaped = sweep_stale_registries_in(tmp.path());
        assert_eq!(reaped, 1, "the orphaned fixture should have been signaled");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while alive(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = fixture.wait();
        assert!(
            !alive(pid),
            "an orphaned fixture whose owner is gone and whose identity still matches must be \
             reaped by the startup sweep (#2716)"
        );
        assert!(
            std::fs::read_dir(tmp.path()).unwrap().count() == 0,
            "a swept registry file must be removed"
        );
    }

    /// (#2716) The guard behind the die-with-parent half: every fixture
    /// must actually BE in the watchdog's process group, or the
    /// watchdog's single group signal reaches nothing. Emptying
    /// `FixtureGroup::place` makes this fail.
    #[cfg(unix)]
    #[test]
    fn place_puts_a_child_in_the_watchdog_group() {
        let mut group = FixtureGroup::arm();
        let pgid = group.pgid.expect("watchdog must have spawned");

        let mut cmd = Command::new("sleep");
        cmd.arg("30").stdout(Stdio::null()).stderr(Stdio::null());
        group.place(&mut cmd);
        let mut child = cmd.spawn().expect("spawning a placed child");

        let out = Command::new("ps")
            .args(["-o", "pgid=", "-p", &child.id().to_string()])
            .output()
            .expect("ps");
        let observed: u32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("ps should report a numeric pgid");

        assert_eq!(
            observed, pgid,
            "a fixture spawned through FixtureGroup must land in the watchdog's process group \
             — the watchdog signals the GROUP on parent death, so a child outside it is a \
             child nothing reaps (#2716)"
        );

        let _ = child.kill();
        let _ = child.wait();
        // Explicit, because `FixtureGroup` has no `Drop`: without this the
        // probe leaves its own registry file for a later run to sweep.
        group.stand_down();
    }

    /// (#2716) An orderly teardown must NOT let the watchdog signal the
    /// group: the harness's own `Drop` is handling its children, and a
    /// stray group signal at that moment is noise at best. The watchdog
    /// must exit on its own.
    #[cfg(unix)]
    #[test]
    fn stand_down_exits_the_watchdog_without_signaling_the_group() {
        let mut group = FixtureGroup::arm();
        let pgid = group.pgid.expect("watchdog must have spawned");

        let mut cmd = Command::new("sleep");
        cmd.arg("30").stdout(Stdio::null()).stderr(Stdio::null());
        group.place(&mut cmd);
        let mut child = cmd.spawn().expect("spawning a placed child");

        group.stand_down();

        assert!(
            !alive(pgid),
            "the watchdog must exit when told to stand down (#2716)"
        );
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            alive(child.id()),
            "stand-down means the harness is handling its own children; the watchdog must not \
             have signaled the group on its way out (#2716)"
        );

        let _ = child.kill();
        let _ = child.wait();
    }
}
