//! (#2973, #906) Running model-written code: a mod's test command, a lab
//! run's verify command, a grader's test suite. Each one executes whatever
//! the model wrote into the tree it runs in, so by default it runs in a
//! container: the tree mounted at `/workspace` (plus any mount the caller
//! names) and nothing else, `--network none`, `--cap-drop=ALL`, no new
//! privileges, the coder container's memory and pid limits, `sh` as the
//! entrypoint, removed when it exits and removed by name if the deadline or
//! a signal kills its client. [`Placement::Host`] runs it on this machine
//! as the darkmux user instead, which only an operator's explicit choice
//! should ever select.

use crate::bounded_command::{run_bounded, Bounded};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Where model-written code runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// `docker run` on this image, with no network and only the named
    /// mounts.
    Container(String),
    /// `sh -c` on this machine, as the darkmux user.
    Host,
}

/// `docker run`'s exit code for "the container could not be run".
pub const DOCKER_RUN_FAILED: i32 = 125;

/// (#2973 review) A file the container checks for before running anything:
/// a Docker daemon that cannot see the host's paths (remote, or a VM
/// without that share) mounts empty directories, where a permissive command
/// would pass. (#3355 review) It lives in a directory of its own beside the
/// tree, never in it: the tree is the model's, so a file written there
/// could follow a link the model planted, and the command would see it.
pub const SENTINEL: &str = ".darkmux-gate";

/// Where the sentinel's directory is mounted, read-only.
const SENTINEL_AT: &str = "/darkmux-gate";

/// (#3355 review) The line the container prints before the command runs.
/// Its presence is what says the command ran: without it, any exit is
/// Docker's or the mount check's, and with it, any exit (125 included) is
/// the command's own. Removed from the output the caller sees.
const STARTED_LINE: &str = "darkmux-contained: started";

/// A run the container never started: Docker could not run it, or the tree
/// was not mounted. Never the command's own result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotRun(pub String);

impl std::fmt::Display for NotRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The label a container carries: the darkmux process that started it.
pub const PID_LABEL: &str = "darkmux.gate.pid";

/// One run's shape: the tree it runs in and what else the container sees.
#[derive(Debug, Clone)]
pub struct Run<'a> {
    /// The command, as `sh -c` text. In a container, paths in it must be the
    /// container's (`/workspace/...`, a mount's `at`).
    pub command: &'a str,
    /// The tree the command runs in, mounted at `/workspace`.
    pub tree: &'a Path,
    /// The working directory, relative to the tree (empty for its root).
    pub workdir: &'a str,
    /// Further host directories, each mounted read-write at its `at`.
    pub mounts: &'a [(PathBuf, String)],
    /// `--user` for the container (`None` keeps the image's user). A host
    /// run ignores it.
    pub user: Option<String>,
    /// The prefix of the container's name.
    pub name_prefix: &'a str,
}

/// `docker run` arguments for a run in a container.
pub fn container_args(image: &str, name: &str, run: &Run) -> Vec<String> {
    use crate::dispatch_internal::{DOCKER_CAP_DROP, DOCKER_MEMORY, DOCKER_PIDS_LIMIT, DOCKER_SECURITY_OPT};
    let mut args: Vec<String> = ["run", "--rm", "--init", "--name", name, "--network", "none"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    args.extend([
        // (#2973 review) The coder container's own limits: model-written
        // code here is no more trusted than the coder's.
        format!("--cap-drop={DOCKER_CAP_DROP}"),
        format!("--security-opt={DOCKER_SECURITY_OPT}"),
        format!("--pids-limit={DOCKER_PIDS_LIMIT}"),
        format!("--memory={DOCKER_MEMORY}"),
        "--label".to_string(),
        format!("{PID_LABEL}={}", std::process::id()),
        "-v".to_string(),
        format!("{}:/workspace", run.tree.display()),
    ]);
    if let Some(sentinel_dir) = sentinel_dir_for(name, run.tree) {
        args.extend(["-v".to_string(), format!("{}:{SENTINEL_AT}:ro", sentinel_dir.display())]);
    }
    for (host, at) in run.mounts {
        args.extend(["-v".to_string(), format!("{}:{at}", host.display())]);
    }
    if let Some(user) = &run.user {
        // The image's home belongs to the image's own user.
        args.extend([format!("--user={user}"), "-e".to_string(), "HOME=/tmp".to_string()]);
    }
    let workdir = match run.workdir.trim_matches('/') {
        "" => "/workspace".to_string(),
        rel => format!("/workspace/{rel}"),
    };
    args.extend([
        "-w".to_string(),
        workdir,
        // The image's own entrypoint never wraps the command (the darkmux
        // runtime image's would take `sh -c` as its arguments).
        "--entrypoint".to_string(),
        "sh".to_string(),
        image.to_string(),
        "-c".to_string(),
        format!(
            "test -f {SENTINEL_AT}/{SENTINEL} || {{ echo 'the tree was not mounted: the Docker daemon cannot see the host path' >&2; exit {DOCKER_RUN_FAILED}; }}\n\
             echo '{STARTED_LINE}'\n{}",
            run.command
        ),
    ]);
    args
}

/// The command to spawn, and the container's name when it runs in one.
pub fn command(placement: &Placement, run: &Run) -> (Command, Option<String>) {
    match placement {
        Placement::Host => {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(run.command).current_dir(run.tree.join(run.workdir));
            (cmd, None)
        }
        Placement::Container(image) => {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let name = format!("{}-{}-{n}", run.name_prefix, std::process::id());
            let mut cmd = Command::new("docker");
            cmd.args(container_args(image, &name, run));
            (cmd, Some(name))
        }
    }
}

/// The sentinel's directory for the container `name`: beside the tree, in
/// its parent, which darkmux made (the run directory, a scratch dir), never
/// inside the tree. `None` for a tree with no parent.
fn sentinel_dir_for(name: &str, tree: &Path) -> Option<PathBuf> {
    Some(tree.parent()?.join(format!("{SENTINEL}-{name}")))
}

/// Run `run` where `placement` says, bounded by `timeout`. In a container,
/// orphaned containers are reaped first, the sentinel is written beside the
/// tree for the run and removed after it, and a container whose client was
/// killed (deadline or signal) is removed by name. `Err` is a container that
/// never ran the command; `Finished` is always the command's own exit.
pub fn run(placement: &Placement, run: &Run, timeout: Duration) -> Result<Bounded, NotRun> {
    let (cmd, container) = command(placement, run);
    let Some(name) = container else {
        return Ok(run_bounded(cmd, timeout));
    };
    reap_orphaned_containers();
    let sentinel_dir = sentinel_dir_for(&name, run.tree)
        .ok_or_else(|| NotRun(format!("{} has no parent directory to hold the mount check", run.tree.display())))?;
    if let Err(e) = write_sentinel(&sentinel_dir) {
        return Err(NotRun(format!("could not write the mount check in {}: {e}", sentinel_dir.display())));
    }
    let ran = run_bounded(cmd, timeout);
    if matches!(ran, Bounded::TimedOut { .. } | Bounded::Interrupted) {
        // Killing the `docker run` client does not stop its container.
        remove_container(&name);
    }
    let _ = std::fs::remove_dir_all(&sentinel_dir);
    started(ran)
}

/// A fresh directory holding the sentinel. `create_dir` and `create_new`
/// refuse anything already there, a link included, so nothing is followed.
fn write_sentinel(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir(dir)?;
    std::fs::OpenOptions::new().write(true).create_new(true).open(dir.join(SENTINEL)).map(drop)
}

/// A container's result, read by whether it printed [`STARTED_LINE`].
fn started(ran: Bounded) -> Result<Bounded, NotRun> {
    match ran {
        Bounded::Finished { success, code, stdout, stderr } => match stdout.strip_prefix(format!("{STARTED_LINE}\n").as_bytes()) {
            Some(rest) => Ok(Bounded::Finished { success, code, stdout: rest.to_vec(), stderr }),
            None => {
                let said = String::from_utf8_lossy(&stderr).trim().to_string();
                let code = code.map_or("a signal".to_string(), |c| format!("exit {c}"));
                Err(NotRun(if said.is_empty() { format!("docker run ended ({code}) before the command started") } else { said }))
            }
        },
        other => Ok(other),
    }
}

/// (#2973 review) The containers to remove: those whose owning darkmux
/// process is gone (a crash or SIGKILL kills the `docker run` client, never
/// its container). A live owner's container is left alone.
pub fn orphaned_containers(listed: &[(String, Option<u32>)], alive: impl Fn(u32) -> bool) -> Vec<String> {
    listed.iter().filter(|(_, pid)| pid.is_some_and(|p| !alive(p))).map(|(name, _)| name.clone()).collect()
}

/// Remove containers whose owner is gone. Best effort and bounded: a Docker
/// that cannot list them is the next `docker run`'s failure to name.
pub fn reap_orphaned_containers() {
    let mut ps = Command::new("docker");
    ps.args(["ps", "-a", "--filter", &format!("label={PID_LABEL}"), "--format", &format!("{{{{.Names}}}}\t{{{{.Label \"{PID_LABEL}\"}}}}")]);
    let Bounded::Finished { success: true, stdout, .. } = run_bounded(ps, Duration::from_secs(30)) else {
        return;
    };
    let listed: Vec<(String, Option<u32>)> = String::from_utf8_lossy(&stdout)
        .lines()
        .filter_map(|l| l.split_once('\t').map(|(n, p)| (n.to_string(), p.trim().parse().ok())))
        .collect();
    for name in orphaned_containers(&listed, crate::host_sampler_lock::pid_alive) {
        remove_container(&name);
    }
}

/// Stop and remove a container whose `docker run` client was killed. Best
/// effort: a container already gone is the goal, not an error.
pub fn remove_container(name: &str) {
    let mut rm = Command::new("docker");
    rm.args(["rm", "-f", name]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    let _ = run_bounded(rm, Duration::from_secs(30));
}

/// (#906) The image a lab run's tests run in: the workload's declared image,
/// else the darkmux runtime image the dispatch itself runs (resolved and
/// version-checked the same way, pulled if absent). Refuses when Docker is
/// not reachable, naming the opt-out.
pub fn lab_test_image(workload_image: Option<&str>) -> anyhow::Result<String> {
    use anyhow::Context;
    if let Some(image) = workload_image {
        crate::dispatch_internal::validate_image_ref(image)?;
        return Ok(image.to_string());
    }
    crate::dispatch_internal::test_runtime_image().context(
        "model-written tests run in a container, so they need Docker; \
         to run them on this machine as your user instead, `darkmux config set runtime.tests_on_host true`",
    )
}

/// (#906) The user a lab run's test container runs as: the same answer the
/// dispatch container gets (the operator's `runtime.container_user`, else
/// the platform's), so files the tests write into a bind-mounted tree belong
/// to whoever owns the tree.
pub fn lab_test_user() -> anyhow::Result<Option<String>> {
    let user = crate::dispatch_internal::resolve_container_user(
        darkmux_types::config_access::container_user_override().as_deref(),
        darkmux_platform::current(),
    )?;
    Ok(user.docker_user())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_run<'a>(mounts: &'a [(PathBuf, String)], user: Option<String>) -> Run<'a> {
        Run { command: "npm test", tree: Path::new("/tmp/s/tree"), workdir: "pkg", mounts, user, name_prefix: "darkmux-test" }
    }

    /// The container sees the tree and the named mounts only: no network,
    /// no capabilities, no new privileges, the command last behind the
    /// sentinel check.
    #[test]
    fn a_container_mounts_only_the_tree_and_the_named_mounts() {
        let mounts = [(PathBuf::from("/tmp/s/report"), "/report".to_string())];
        let args = container_args("node:22", "darkmux-test-1", &a_run(&mounts, Some("501:20".into())));
        let joined = args.join(" ");
        for want in ["--network none", "--cap-drop=ALL", "--security-opt=no-new-privileges", "-v /tmp/s/tree:/workspace",
            "-v /tmp/s/report:/report", "-w /workspace/pkg", "--user=501:20 -e HOME=/tmp", "--entrypoint sh node:22 -c"] {
            assert!(joined.contains(want), "missing `{want}` in: {joined}");
        }
        // The tree, the named mount, and the mount check beside the tree, read-only.
        assert!(joined.contains(&format!("-v /tmp/s/{SENTINEL}-darkmux-test-1:{SENTINEL_AT}:ro")), "{joined}");
        assert_eq!(args.iter().filter(|a| *a == "-v").count(), 3, "{joined}");
        let script = args.last().unwrap();
        assert!(script.starts_with(&format!("test -f {SENTINEL_AT}/{SENTINEL} || {{")), "{script}");
        assert!(script.ends_with(&format!("\necho '{STARTED_LINE}'\nnpm test")), "{script}");
        assert!(!container_args("node:22", "n", &a_run(&[], None)).join(" ").contains("--user"));
    }

    /// On the host the command runs in the tree's working directory.
    #[test]
    fn a_host_run_is_sh_in_the_workdir() {
        let (cmd, name) = command(&Placement::Host, &a_run(&[], None));
        assert!(name.is_none());
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/tmp/s/tree/pkg")));
    }

    /// (#3355 review) A container that printed the start line ran the
    /// command, so its exit is the command's own, 125 included; one that did
    /// not never ran it, whatever its exit, and says why.
    #[test]
    fn only_a_started_container_reports_the_commands_exit() {
        let finished = |stdout: &str, stderr: &str| Bounded::Finished {
            success: false,
            code: Some(125),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        };
        let Ok(Bounded::Finished { code, stdout, .. }) = started(finished(&format!("{STARTED_LINE}\nout"), "")) else {
            panic!("a started run is the command's")
        };
        assert_eq!((code, stdout), (Some(125), b"out".to_vec()), "the start line is not the command's output");
        assert_eq!(started(finished("", "Unable to find image")).unwrap_err(), NotRun("Unable to find image".into()));
        let silent = started(finished("", "")).unwrap_err().0;
        assert!(silent.contains("exit 125") && silent.contains("before the command started"), "{silent}");
        assert!(started(Bounded::TimedOut { seconds: 3 }).is_ok());
    }

    /// (#3355 review) The mount check is written beside the tree, never in it,
    /// and refuses anything already at its path: a link there is not followed.
    #[test]
    fn the_mount_check_never_follows_a_link() {
        let root = tempfile::TempDir::new().unwrap();
        let tree = root.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        let dir = sentinel_dir_for("c-1", &tree).unwrap();
        assert_eq!(dir.parent(), Some(root.path()), "beside the tree, not in it");
        let target = root.path().join("precious");
        std::fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, &dir).unwrap();
        assert!(write_sentinel(&dir).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
        std::fs::remove_file(&dir).unwrap();
        write_sentinel(&dir).unwrap();
        assert!(dir.join(SENTINEL).is_file());
    }

    /// A container run whose mount check has nowhere to go never starts:
    /// a tree with no parent, or a parent that is not a directory.
    #[test]
    fn a_container_run_without_room_for_the_mount_check_never_starts() {
        let contained = Placement::Container("darkmux-no-such-image:0".into());
        let at_root = Run { tree: Path::new("/"), ..a_run(&[], None) };
        let err = run(&contained, &at_root, Duration::from_secs(5)).unwrap_err().0;
        assert!(err.contains("no parent directory"), "{err}");
        let root = tempfile::TempDir::new().unwrap();
        let file = root.path().join("not-a-dir");
        std::fs::write(&file, "").unwrap();
        let tree = file.join("tree");
        let under_a_file = Run { tree: &tree, ..a_run(&[], None) };
        let err = run(&contained, &under_a_file, Duration::from_secs(5)).unwrap_err().0;
        assert!(err.contains("could not write the mount check"), "{err}");
    }

    /// (#906) A run in a real container sees its tree and no network, and the
    /// sentinel is gone afterwards. Needs Docker and `node:22-bookworm-slim`.
    #[test]
    #[ignore = "needs Docker and node:22-bookworm-slim"]
    fn a_real_container_sees_the_tree_and_no_network() {
        let tree = tempfile::TempDir::new().unwrap();
        std::fs::write(tree.path().join("a.txt"), "hello").unwrap();
        let run_ = Run {
            command: "cat a.txt && ! getent hosts example.com",
            tree: tree.path(),
            workdir: "",
            mounts: &[],
            user: None,
            name_prefix: "darkmux-test",
        };
        let contained = Placement::Container("node:22-bookworm-slim".into());
        let ran = run(&contained, &run_, Duration::from_secs(60));
        let Ok(Bounded::Finished { success, stdout, stderr, .. }) = ran else { panic!("{ran:?}") };
        assert!(success, "{} {}", String::from_utf8_lossy(&stdout), String::from_utf8_lossy(&stderr));
        assert_eq!(String::from_utf8_lossy(&stdout), "hello");
        let left: Vec<_> = std::fs::read_dir(tree.path().parent().unwrap()).unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.starts_with(SENTINEL))
            .collect();
        assert!(left.is_empty(), "the mount check is removed: {left:?}");
        // The command sees only the model's files, and its own exit 125 is its own.
        let own = Run { command: "ls -A; exit 125", ..run_.clone() };
        let Ok(Bounded::Finished { code, stdout, .. }) = run(&contained, &own, Duration::from_secs(60)) else { panic!() };
        assert_eq!((code, String::from_utf8_lossy(&stdout).trim().to_string()), (Some(125), "a.txt".to_string()));
        // A missing image never ran the command.
        let missing = run(&Placement::Container("darkmux-no-such-image:0".into()), &run_, Duration::from_secs(60));
        assert!(missing.is_err(), "{missing:?}");
    }
}
