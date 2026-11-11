//! (#3112) What every sandbox backend must do, written once and run against
//! each backend. Each case takes the backend and an image to run; the
//! `conformance!` lines at the end instantiate the suite per backend. The
//! cases start real sandboxes, so they are opt-in like the repo's other
//! real-container tests: `cargo nextest run -p darkmux-crew
//! sandbox_conformance --run-ignored only`.
//!
//! Runs are bounded the way production bounds them ([`run_bounded`]), and
//! a sandbox whose client was cut short is removed through the backend, the
//! way a contained run removes one.

use crate::bounded_command::{run_bounded, Bounded};
use darkmux_sandbox::{Access, Limits, Mount, Network, SandboxBackend, SandboxHandle, SandboxPolicy};
use std::path::Path;
use std::time::{Duration, Instant};

/// The label every conformance sandbox carries, its value the sandbox's
/// name, so a listing finds only this suite's sandboxes.
const LABEL: &str = "darkmux.sandbox.conformance";

fn a_name() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("darkmux-conformance-{}-{n}", std::process::id())
}

/// `sh -c script` in `image`, no network, the tree read-write at
/// `/workspace`, labeled for this suite.
fn policy(image: &str, tree: &Path, script: &str) -> SandboxPolicy {
    let name = a_name();
    SandboxPolicy {
        labels: vec![(LABEL.to_string(), name.clone())],
        name,
        image: image.to_string(),
        network: Network::None,
        limits: Limits { memory: Some("256m".to_string()), pids: Some(64) },
        mounts: vec![Mount { host: tree.to_path_buf(), at: "/workspace".to_string(), access: Access::ReadWrite }],
        user: None,
        env: Vec::new(),
        workdir: Some("/workspace".to_string()),
        entrypoint: Some("sh".to_string()),
        command: vec!["-c".to_string(), script.to_string()],
    }
}

/// Removes its sandbox when dropped, so a failed assertion leaves nothing
/// running.
struct Removed<'a>(&'a dyn SandboxBackend, SandboxHandle);

impl Drop for Removed<'_> {
    fn drop(&mut self) {
        let _ = run_bounded(self.0.remove(&self.1), Duration::from_secs(30));
    }
}

/// Run `policy` to completion; its stdout on success, else a panic naming
/// what it said.
fn run_ok(backend: &dyn SandboxBackend, policy: &SandboxPolicy) -> String {
    let (cmd, handle) = backend.run(policy);
    let _removed = Removed(backend, handle);
    match run_bounded(cmd, Duration::from_secs(60)) {
        Bounded::Finished { success: true, stdout, .. } => String::from_utf8_lossy(&stdout).into_owned(),
        other => panic!("{} did not succeed: {}", policy.name, describe(&other)),
    }
}

fn describe(ran: &Bounded) -> String {
    match ran {
        Bounded::Finished { code, stdout, stderr, .. } => format!(
            "exit {code:?}, stdout {:?}, stderr {:?}",
            String::from_utf8_lossy(stdout),
            String::from_utf8_lossy(stderr)
        ),
        other => format!("{other:?}"),
    }
}

/// The values of this suite's label on the sandboxes the backend lists.
fn listed(backend: &dyn SandboxBackend) -> Vec<String> {
    let Bounded::Finished { success: true, stdout, .. } = run_bounded(backend.list_labeled(LABEL), Duration::from_secs(30)) else {
        panic!("the backend could not list its sandboxes");
    };
    backend.parse_labeled(&String::from_utf8_lossy(&stdout)).into_iter().map(|l| l.value.trim().to_string()).collect()
}

/// Wait up to 30 s for `name` to be listed or not, as `want` says.
fn until_listed(backend: &dyn SandboxBackend, name: &str, want: bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let found = listed(backend).iter().any(|v| v == name);
        if found == want || Instant::now() > deadline {
            return found == want;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The mounted tree is there, and the host path beside it, and the tree's
/// own host path, are not.
fn sees_the_mounted_tree_and_nothing_else(backend: &dyn SandboxBackend, image: &str) {
    let root = tempfile::TempDir::new().unwrap();
    let tree = root.path().join("tree");
    std::fs::create_dir(&tree).unwrap();
    std::fs::write(tree.join("a.txt"), "hello").unwrap();
    std::fs::write(root.path().join("secret.txt"), "keep out").unwrap();
    let script = format!(
        "cat /workspace/a.txt && test ! -e '{}' && test ! -e '{}'",
        root.path().join("secret.txt").display(),
        tree.join("a.txt").display()
    );
    assert_eq!(run_ok(backend, &policy(image, &tree, &script)), "hello");
}

/// A read-only mount refuses a write, alone or layered over a read-write
/// one (a checkout's `.git` over its tree), and the read-write tree beneath
/// still takes one.
fn read_only_mounts_refuse_writes(backend: &dyn SandboxBackend, image: &str) {
    let root = tempfile::TempDir::new().unwrap();
    let tree = root.path().join("tree");
    let ro = root.path().join("ro");
    std::fs::create_dir_all(tree.join(".git")).unwrap();
    std::fs::create_dir(&ro).unwrap();
    let mut p = policy(
        image,
        &tree,
        "touch /workspace/written && ! touch /workspace/.git/x 2>/dev/null && ! touch /ro/x 2>/dev/null && echo refused",
    );
    p.mounts.push(Mount { host: tree.join(".git"), at: "/workspace/.git".to_string(), access: Access::ReadOnly });
    p.mounts.push(Mount { host: ro.clone(), at: "/ro".to_string(), access: Access::ReadOnly });
    assert_eq!(run_ok(backend, &p).trim(), "refused");
    assert!(tree.join("written").is_file(), "the read-write tree took the write");
    assert!(!tree.join(".git/x").exists() && !ro.join("x").exists(), "a read-only mount took a write");
}

/// No network means no route anywhere: the routing table is empty (the
/// loopback's routes are not in it). The same probe on the default network
/// finds a route, so it can tell the two apart.
fn network_none_has_no_network(backend: &dyn SandboxBackend, image: &str) {
    let tree = tempfile::TempDir::new().unwrap();
    let probe = "tail -n +2 /proc/net/route";
    let none = run_ok(backend, &policy(image, tree.path(), probe));
    assert_eq!(none.trim(), "", "a sandbox with no network has a route");
    let default = run_ok(backend, &SandboxPolicy { network: Network::Default, ..policy(image, tree.path(), probe) });
    assert!(!default.trim().is_empty(), "the probe cannot see a route on the default network");
}

/// A run past its deadline is cut short, and removing its sandbox ends the
/// whole tree: background children included, nothing is left listed. It is
/// listed while it runs, so "not listed" is not a listing that sees nothing.
fn a_deadline_kills_the_whole_tree(backend: &dyn SandboxBackend, image: &str) {
    let tree = tempfile::TempDir::new().unwrap();
    let p = policy(image, tree.path(), "sleep 300 & sleep 300 & wait");
    let (cmd, handle) = backend.run(&p);
    let removed = Removed(backend, handle);
    let bounded = std::thread::spawn(move || run_bounded(cmd, Duration::from_secs(8)));
    assert!(until_listed(backend, &p.name, true), "{} never listed while running", p.name);
    let ran = bounded.join().unwrap();
    assert!(matches!(ran, Bounded::TimedOut { .. }), "{}", describe(&ran));
    drop(removed);
    assert!(until_listed(backend, &p.name, false), "{} outlived its deadline", p.name);
}

/// A sandbox whose client is gone (its owner killed) is still found by its
/// label, so a reaper can remove it, after which it is gone.
fn a_reaper_finds_an_orphan_by_its_label(backend: &dyn SandboxBackend, image: &str) {
    let tree = tempfile::TempDir::new().unwrap();
    let p = policy(image, tree.path(), "sleep 300");
    let (mut cmd, handle) = backend.run(&p);
    let removed = Removed(backend, handle);
    let mut client = cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
    assert!(until_listed(backend, &p.name, true), "{} never listed", p.name);
    client.kill().unwrap();
    client.wait().unwrap();
    assert!(until_listed(backend, &p.name, true), "the orphan is found by its label after its client is gone");
    drop(removed);
    assert!(until_listed(backend, &p.name, false), "{} survived its removal", p.name);
}

macro_rules! conformance {
    ($backend:ident, $make:expr, $image:literal, $needs:literal) => {
        mod $backend {
            #[test]
            #[ignore = $needs]
            fn sees_the_mounted_tree_and_nothing_else() {
                super::sees_the_mounted_tree_and_nothing_else(&$make, $image)
            }
            #[test]
            #[ignore = $needs]
            fn read_only_mounts_refuse_writes() {
                super::read_only_mounts_refuse_writes(&$make, $image)
            }
            #[test]
            #[ignore = $needs]
            fn network_none_has_no_network() {
                super::network_none_has_no_network(&$make, $image)
            }
            #[test]
            #[ignore = $needs]
            fn a_deadline_kills_the_whole_tree() {
                super::a_deadline_kills_the_whole_tree(&$make, $image)
            }
            #[test]
            #[ignore = $needs]
            fn a_reaper_finds_an_orphan_by_its_label() {
                super::a_reaper_finds_an_orphan_by_its_label(&$make, $image)
            }
        }
    };
}

conformance!(docker, darkmux_sandbox::DockerBackend, "alpine:3.22", "needs Docker and alpine:3.22");
