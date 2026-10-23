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

/// (#2973 review) A file written into the mounted tree that the container
/// checks for before running anything: a Docker daemon that cannot see the
/// tree (remote, or a VM without that share) mounts an empty directory,
/// where a permissive command would pass.
pub const SENTINEL: &str = ".darkmux-gate";

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
        format!("test -f /workspace/{SENTINEL} || exit {DOCKER_RUN_FAILED}\n{}", run.command),
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

/// Run `run` where `placement` says, bounded by `timeout`. In a container,
/// the sentinel is written into the tree for the run and removed after it,
/// orphaned containers are reaped first, and a container whose client was
/// killed (deadline or signal) is removed by name. `Finished` with
/// [`DOCKER_RUN_FAILED`] in a container is Docker failing to run it, never
/// the command's own exit: see [`docker_failed`].
pub fn run(placement: &Placement, run: &Run, timeout: Duration) -> Bounded {
    let in_container = matches!(placement, Placement::Container(_));
    let sentinel = run.tree.join(SENTINEL);
    if in_container {
        reap_orphaned_containers();
        if let Err(e) = std::fs::write(&sentinel, b"") {
            return Bounded::SpawnFailed(e);
        }
    }
    let (cmd, container) = command(placement, run);
    let ran = run_bounded(cmd, timeout);
    if let (Some(name), Bounded::TimedOut { .. } | Bounded::Interrupted) = (&container, &ran) {
        // Killing the `docker run` client does not stop its container.
        remove_container(name);
    }
    if in_container {
        let _ = std::fs::remove_file(&sentinel);
    }
    ran
}

/// Whether a finished run is Docker failing to run the container (a missing
/// image, a stopped daemon, the tree not mounted) rather than the command's
/// own exit.
pub fn docker_failed(placement: &Placement, ran: &Bounded) -> bool {
    matches!(placement, Placement::Container(_))
        && matches!(ran, Bounded::Finished { code: Some(DOCKER_RUN_FAILED), .. })
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
        assert_eq!(args.iter().filter(|a| *a == "-v").count(), 2, "{joined}");
        let script = args.last().unwrap();
        assert!(script.starts_with(&format!("test -f /workspace/{SENTINEL} || exit 125\n")), "{script}");
        assert!(script.ends_with("\nnpm test"), "{script}");
        assert!(!container_args("node:22", "n", &a_run(&[], None)).join(" ").contains("--user"));
    }

    /// On the host the command runs in the tree's working directory.
    #[test]
    fn a_host_run_is_sh_in_the_workdir() {
        let (cmd, name) = command(&Placement::Host, &a_run(&[], None));
        assert!(name.is_none());
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/tmp/s/tree/pkg")));
    }

    /// Exit 125 is Docker's in a container and the command's own on the host.
    #[test]
    fn exit_125_is_docker_failing_only_in_a_container() {
        let ran = Bounded::Finished { success: false, code: Some(125), stdout: vec![], stderr: vec![] };
        assert!(docker_failed(&Placement::Container("x".into()), &ran));
        assert!(!docker_failed(&Placement::Host, &ran));
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
        let ran = run(&Placement::Container("node:22-bookworm-slim".into()), &run_, Duration::from_secs(60));
        let Bounded::Finished { success, stdout, stderr, .. } = ran else { panic!("{ran:?}") };
        assert!(success, "{} {}", String::from_utf8_lossy(&stdout), String::from_utf8_lossy(&stderr));
        assert_eq!(String::from_utf8_lossy(&stdout), "hello");
        assert!(!tree.path().join(SENTINEL).exists());
    }
}
