//! The Docker backend: a policy as a `docker run` command line. This file
//! is the one place outside the ratchet's list that may spell the `docker`
//! program (`docker-calls.allow`).

use crate::{Access, BackendKind, Having, Listed, Mount, Network, Query, SandboxBackend, SandboxHandle, SandboxPolicy};
use std::process::Command;

const PROGRAM: &str = "docker";

/// `--cap-drop`: every Linux capability. A sandbox's code needs none.
pub const CAP_DROP: &str = "ALL";

/// `--security-opt`: no setuid, setgid or file-capability escalation, even
/// where a capability were granted.
pub const SECURITY_OPT: &str = "no-new-privileges";

/// Sandboxes as containers, through the `docker` CLI on `PATH`.
#[derive(Debug, Clone, Copy, Default)]
pub struct DockerBackend;

impl DockerBackend {
    /// `docker run`'s arguments for `policy` (the program itself excluded),
    /// in a fixed order: the sandbox's identity and invariants, its network
    /// and limits, its labels, its mounts in the policy's order, its user
    /// and environment, its working directory, then the image and command.
    pub fn run_args(policy: &SandboxPolicy) -> Vec<String> {
        Self::args("run", policy)
    }

    /// `run_args` under another verb (`create` takes the same line).
    fn args(verb: &str, policy: &SandboxPolicy) -> Vec<String> {
        let mut args = strings(&[verb, "--rm", "--init", "--name", &policy.name]);
        if policy.network == Network::None {
            args.extend(strings(&["--network", "none"]));
        }
        args.extend([format!("--cap-drop={CAP_DROP}"), format!("--security-opt={SECURITY_OPT}")]);
        args.extend(policy.limits.pids.map(|n| format!("--pids-limit={n}")));
        args.extend(policy.limits.memory.as_ref().map(|m| format!("--memory={m}")));
        for (key, value) in &policy.labels {
            args.extend(["--label".to_string(), format!("{key}={value}")]);
        }
        args.extend(policy.mounts.iter().flat_map(volume));
        for port in &policy.ports {
            args.extend(["-p".to_string(), format!("127.0.0.1:{port}:{port}")]);
        }
        args.extend(policy.user.as_ref().map(|u| format!("--user={u}")));
        for (key, value) in &policy.env {
            // Always `KEY=value`: a bare `-e KEY` would copy the host's value.
            args.extend(["-e".to_string(), format!("{key}={value}")]);
        }
        if let Some(workdir) = &policy.workdir {
            args.extend(["-w".to_string(), workdir.clone()]);
        }
        if let Some(entrypoint) = &policy.entrypoint {
            args.extend(["--entrypoint".to_string(), entrypoint.clone()]);
        }
        args.push(policy.image.clone());
        args.extend(policy.command.iter().cloned());
        args
    }

    /// `docker ps`'s arguments for `query`: `-a` for stopped ones too, one
    /// filter, and a format printing the name and each label read,
    /// tab-separated.
    pub fn list_args(query: &Query) -> Vec<String> {
        let mut args = strings(&["ps"]);
        if query.stopped_too {
            args.push("-a".to_string());
        }
        let filter = match &query.having {
            Having::Label(key) => format!("label={key}"),
            Having::NamePrefix(prefix) => format!("name=^{prefix}"),
        };
        let labels: String = query.read.iter().map(|key| format!("\t{{{{.Label \"{key}\"}}}}")).collect();
        args.extend(["--filter".to_string(), filter, "--format".to_string(), format!("{{{{.Names}}}}{labels}")]);
        args
    }

    /// `docker exec`: `argv` in the running container. Docker's alone (a
    /// backend without containers has nothing to exec into), so not part of
    /// [`SandboxBackend`].
    pub fn exec(handle: &SandboxHandle, argv: &[&str]) -> Command {
        let mut cmd = Command::new(PROGRAM);
        cmd.args(["exec", &handle.id]).args(argv);
        cmd
    }

    fn handle(id: &str) -> SandboxHandle {
        SandboxHandle { backend: BackendKind::Docker, id: id.to_string() }
    }
}

/// A listed label's value, `None` when it is empty or Docker's `<no value>`.
fn present(field: Option<&str>) -> Option<String> {
    field.map(str::trim).filter(|v| !v.is_empty() && *v != "<no value>").map(str::to_string)
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

/// `-v host:at`, with `:ro` for a read-only mount.
fn volume(mount: &Mount) -> [String; 2] {
    let ro = match mount.access {
        Access::ReadOnly => ":ro",
        Access::ReadWrite => "",
    };
    ["-v".to_string(), format!("{}:{}{ro}", mount.host.display(), mount.at)]
}

impl SandboxBackend for DockerBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Docker
    }

    fn run(&self, policy: &SandboxPolicy) -> (Command, SandboxHandle) {
        let mut cmd = Command::new(PROGRAM);
        cmd.args(Self::run_args(policy));
        (cmd, Self::handle(&policy.name))
    }

    fn create(&self, policy: &SandboxPolicy) -> (Command, SandboxHandle) {
        let mut cmd = Command::new(PROGRAM);
        cmd.args(Self::args("create", policy));
        (cmd, Self::handle(&policy.name))
    }

    fn start(&self, handle: &SandboxHandle) -> Command {
        let mut cmd = Command::new(PROGRAM);
        cmd.args(["start", "-a", &handle.id]);
        cmd
    }

    fn stop(&self, handle: &SandboxHandle, grace: std::time::Duration) -> Command {
        let mut cmd = Command::new(PROGRAM);
        cmd.args(["stop", "-t", &grace.as_secs().to_string(), &handle.id]);
        cmd
    }

    fn remove(&self, handle: &SandboxHandle) -> Command {
        let mut cmd = Command::new(PROGRAM);
        cmd.args(["rm", "-f", &handle.id]);
        cmd
    }

    fn list(&self, query: &Query) -> Command {
        let mut cmd = Command::new(PROGRAM);
        cmd.args(Self::list_args(query));
        cmd
    }

    /// One `name<TAB>value…` line per container. An empty value, or
    /// Docker's `<no value>`, is a label the container does not carry.
    fn parse_list(&self, query: &Query, listing: &str) -> Vec<Listed> {
        listing
            .lines()
            .filter_map(|line| {
                let mut fields = line.split('\t');
                let name = fields.next()?.trim();
                let labels = (0..query.read.len()).map(|_| present(fields.next())).collect();
                (!name.is_empty()).then(|| Listed { handle: Self::handle(name), labels })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Limits;
    use std::path::PathBuf;

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    fn mount(host: &str, at: &str, access: Access) -> Mount {
        Mount { host: PathBuf::from(host), at: at.to_string(), access }
    }

    fn bare() -> SandboxPolicy {
        SandboxPolicy {
            name: "s-1".into(),
            image: "img:1".into(),
            network: Network::Default,
            limits: Limits::default(),
            labels: Vec::new(),
            mounts: Vec::new(),
            ports: Vec::new(),
            user: None,
            env: Vec::new(),
            workdir: None,
            entrypoint: None,
            command: Vec::new(),
        }
    }

    /// A bare policy still gets every invariant: removed on exit, an init,
    /// no capabilities, no new privileges. The default network adds no flag.
    #[test]
    fn a_bare_policy_carries_only_the_invariants() {
        assert_eq!(
            DockerBackend::run_args(&bare()),
            strings(&["run", "--rm", "--init", "--name", "s-1", "--cap-drop=ALL", "--security-opt=no-new-privileges", "img:1"])
        );
    }

    /// Every field, in the fixed order, mounts and environment in the
    /// policy's own order.
    #[test]
    fn a_full_policy_renders_every_field_in_order() {
        let policy = SandboxPolicy {
            network: Network::None,
            limits: Limits { memory: Some("4g".into()), pids: Some(512) },
            labels: vec![("a.b".into(), "1".into()), ("c".into(), "2".into())],
            mounts: vec![
                mount("/h/tree", "/workspace", Access::ReadWrite),
                mount("/h/tree/.git", "/workspace/.git", Access::ReadOnly),
            ],
            ports: vec![4100, 4200],
            user: Some("501:20".into()),
            env: vec![("HOME".into(), "/tmp".into()), ("K".into(), "v w".into())],
            workdir: Some("/workspace".into()),
            entrypoint: Some("sh".into()),
            command: strings(&["-c", "echo hi"]),
            ..bare()
        };
        assert_eq!(
            DockerBackend::run_args(&policy),
            strings(&["run", "--rm", "--init", "--name", "s-1", "--network", "none", "--cap-drop=ALL",
                "--security-opt=no-new-privileges", "--pids-limit=512", "--memory=4g", "--label", "a.b=1", "--label", "c=2",
                "-v", "/h/tree:/workspace", "-v", "/h/tree/.git:/workspace/.git:ro", "-p", "127.0.0.1:4100:4100",
                "-p", "127.0.0.1:4200:4200", "--user=501:20", "-e", "HOME=/tmp",
                "-e", "K=v w", "-w", "/workspace", "--entrypoint", "sh", "img:1", "-c", "echo hi"])
        );
    }

    /// `run` spawns `docker` with `run_args`, named by the policy; the
    /// handle is Docker's and the container's name.
    #[test]
    fn run_spawns_docker_and_names_the_handle_by_the_policy() {
        let (cmd, handle) = DockerBackend.run(&bare());
        assert_eq!(cmd.get_program(), "docker");
        assert_eq!(args_of(&cmd), DockerBackend::run_args(&bare()));
        assert_eq!(handle, SandboxHandle { backend: BackendKind::Docker, id: "s-1".into() });
    }

    /// `create` is `run`'s line under `docker create`, so a created sandbox
    /// has every invariant; `start` attaches to it by name; `stop` gives it
    /// the grace in whole seconds before the kill.
    #[test]
    fn create_start_and_stop_are_docker_create_start_and_stop() {
        let policy = SandboxPolicy { ports: vec![4100], ..bare() };
        let (cmd, handle) = DockerBackend.create(&policy);
        let mut want = DockerBackend::run_args(&policy);
        want[0] = "create".into();
        assert_eq!((cmd.get_program().to_str(), args_of(&cmd)), (Some("docker"), want));
        assert_eq!(handle, DockerBackend::handle("s-1"));
        let cmd = DockerBackend.start(&handle);
        assert_eq!((cmd.get_program().to_str(), args_of(&cmd)), (Some("docker"), strings(&["start", "-a", "s-1"])));
        let cmd = DockerBackend.stop(&handle, std::time::Duration::from_millis(5900));
        assert_eq!((cmd.get_program().to_str(), args_of(&cmd)), (Some("docker"), strings(&["stop", "-t", "5", "s-1"])));
        let cmd = DockerBackend::exec(&handle, &["cat", "/proc/net/tcp"]);
        assert_eq!((cmd.get_program().to_str(), args_of(&cmd)), (Some("docker"), strings(&["exec", "s-1", "cat", "/proc/net/tcp"])));
    }

    /// Removal is by name and forced. A listing by label shows stopped
    /// containers too and filters on the key; one by name prefix shows
    /// running ones, anchored at the name's start; each prints the name
    /// and every label read, tab-separated.
    #[test]
    fn remove_and_list_are_docker_rm_and_ps() {
        let cmd = DockerBackend.remove(&DockerBackend::handle("s-1"));
        assert_eq!((cmd.get_program().to_str(), args_of(&cmd)), (Some("docker"), strings(&["rm", "-f", "s-1"])));
        let cmd = DockerBackend.list(&Query::labeled("darkmux.gate.pid"));
        assert_eq!(
            (cmd.get_program().to_str(), args_of(&cmd)),
            (
                Some("docker"),
                strings(&["ps", "-a", "--filter", "label=darkmux.gate.pid", "--format", "{{.Names}}\t{{.Label \"darkmux.gate.pid\"}}"])
            )
        );
        let by_name = Query { having: Having::NamePrefix("p-".into()), stopped_too: false, read: strings(&["a", "b"]) };
        assert_eq!(
            DockerBackend::list_args(&by_name),
            strings(&["ps", "--filter", "name=^p-", "--format", "{{.Names}}\t{{.Label \"a\"}}\t{{.Label \"b\"}}"])
        );
    }

    /// Each line is one container, with one value per label read, trimmed;
    /// an empty field, `<no value>` or a missing one is `None`; a blank
    /// line or an empty name is not a container.
    #[test]
    fn a_listing_parses_to_handles_and_values() {
        let query = Query { having: Having::NamePrefix("x".into()), stopped_too: false, read: strings(&["a", "b"]) };
        let listed = DockerBackend.parse_list(&query, "a\t111\t x \nno-tab\nb\t \t<no value>\n\n\t9\t9\n");
        let got: Vec<(String, Vec<Option<String>>)> = listed.into_iter().map(|l| (l.handle.id, l.labels)).collect();
        let some = |s: &str| Some(s.to_string());
        assert_eq!(
            got,
            vec![("a".into(), vec![some("111"), some("x")]), ("no-tab".into(), vec![None, None]), ("b".into(), vec![None, None])]
        );
    }
}
