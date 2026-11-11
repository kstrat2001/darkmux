//! The Docker backend: a policy as a `docker run` command line. This file
//! is the one place outside the ratchet's list that may spell the `docker`
//! program (`docker-calls.allow`).

use crate::{Access, BackendKind, Labeled, Mount, Network, SandboxBackend, SandboxHandle, SandboxPolicy};
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
        let mut args = strings(&["run", "--rm", "--init", "--name", &policy.name]);
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

    fn handle(id: &str) -> SandboxHandle {
        SandboxHandle { backend: BackendKind::Docker, id: id.to_string() }
    }
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

    fn remove(&self, handle: &SandboxHandle) -> Command {
        let mut cmd = Command::new(PROGRAM);
        cmd.args(["rm", "-f", &handle.id]);
        cmd
    }

    fn list_labeled(&self, key: &str) -> Command {
        let mut cmd = Command::new(PROGRAM);
        cmd.args(["ps", "-a", "--filter", &format!("label={key}"), "--format", &format!("{{{{.Names}}}}\t{{{{.Label \"{key}\"}}}}")]);
        cmd
    }

    /// One `name<TAB>value` line per container; a line with no tab is not
    /// one.
    fn parse_labeled(&self, listing: &str) -> Vec<Labeled> {
        listing
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(name, value)| Labeled { handle: Self::handle(name), value: value.to_string() })
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
                "-v", "/h/tree:/workspace", "-v", "/h/tree/.git:/workspace/.git:ro", "--user=501:20", "-e", "HOME=/tmp",
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

    /// Removal is by name and forced; a listing filters on the label key and
    /// prints each container's name and that label's value.
    #[test]
    fn remove_and_list_are_docker_rm_and_ps() {
        let cmd = DockerBackend.remove(&DockerBackend::handle("s-1"));
        assert_eq!((cmd.get_program().to_str(), args_of(&cmd)), (Some("docker"), strings(&["rm", "-f", "s-1"])));
        let cmd = DockerBackend.list_labeled("darkmux.gate.pid");
        assert_eq!(
            args_of(&cmd),
            strings(&["ps", "-a", "--filter", "label=darkmux.gate.pid", "--format", "{{.Names}}\t{{.Label \"darkmux.gate.pid\"}}"])
        );
    }

    /// Each tabbed line is one container and its value, kept as listed; a
    /// line with no tab is skipped.
    #[test]
    fn a_listing_parses_to_handles_and_values() {
        let listed = DockerBackend.parse_labeled("a\t111\nno-tab\nb\t \nc\t222 \n");
        let got: Vec<(String, String)> = listed.into_iter().map(|l| (l.handle.id, l.value)).collect();
        assert_eq!(got, vec![("a".into(), "111".into()), ("b".into(), " ".into()), ("c".into(), "222 ".into())]);
    }
}
