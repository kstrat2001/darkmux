//! (#3112) Previews in a sandbox on the host ([`BackendKind::on_host`]:
//! Seatbelt now, Landlock later): no image, so the recipe runs this
//! machine's own toolchain, granted read-only
//! ([`darkmux_sandbox::toolchain`]); no published port, so the dev server
//! binds host ports itself. The kernel holds it to its two assigned ports
//! (the policy names them: every other bind is refused), but not to the
//! loopback (#3559), so darkmux reads what listens on those two ports from
//! the kernel's socket table ([`exposed_in`]), and after a stop removes
//! whatever still holds them ([`OnHost::sweep`]).

use super::{ContainerSpec, PreviewContainers, Recipe};
use darkmux_platform::PortListener;
use darkmux_sandbox::toolchain::Toolchain;
use darkmux_sandbox::{BackendKind, SandboxHandle};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// The folders darkmux manages programs in, searched before the host's
/// PATH. None yet: #3561 puts a darkmux-verified Node.js under
/// `~/.darkmux/toolchains/<tool>/<v>/` and names its `bin` here; it is then
/// found, granted (its version folder) and put first on the PATH by the
/// same [`darkmux_sandbox::toolchain::resolve`] a host-installed one goes
/// through. A dispatch on the host can call the same two functions.
fn managed_toolchain_bins() -> Vec<PathBuf> {
    Vec::new()
}

/// (#3112) The host toolchain `recipe` runs with under `kind`, found on
/// `path_var` (darkmux's managed folders first): see
/// [`darkmux_sandbox::toolchain`] for what is granted and refused.
pub(crate) fn resolve_toolchain(
    recipe: &Recipe,
    kind: BackendKind,
    workdir: &Path,
    path_var: &str,
    home: Option<&Path>,
) -> Result<Toolchain, String> {
    use darkmux_sandbox::toolchain::{resolve, search_path, Ask};
    let backend = darkmux_sandbox::backend_of(kind);
    let path = search_path(&managed_toolchain_bins(), path_var);
    let commands = std::iter::once(recipe.command.as_str()).chain(recipe.install.as_ref().map(|i| i.command.as_str())).collect();
    resolve(&Ask {
        commands,
        workdir: Some(workdir),
        path_var: &path,
        home,
        reads: &|p| backend.host_reads(p),
        placed: format!("a preview in {}", darkmux_sandbox::placed_in(kind)),
        runs_with: "`darkmux client serve`",
    })
}

/// Whether `l` is on the loopback: `127.x`, `::1`, `::ffff:127.x`, or an
/// address scoped to the loopback interface (`fe80::1%lo0`).
fn on_loopback(l: &PortListener) -> bool {
    let scoped = l.scope.as_deref().is_some_and(|s| s == "lo0" || s == "lo");
    scoped
        || match l.addr {
            std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
            v4 => v4.is_loopback(),
        }
}

/// The listeners bound beyond the loopback: every address (`0.0.0.0`,
/// `::`) or any other address not on it.
pub(crate) fn beyond_loopback(listeners: &[PortListener]) -> Vec<PortListener> {
    listeners.iter().filter(|l| !on_loopback(l)).cloned().collect()
}

/// Why a preview on the host was stopped: each listener beyond the
/// loopback on its two ports, what darkmux gave the dev server, the fix.
pub(crate) fn exposure_reason(kind: BackendKind, exposed: &[(u16, PortListener)], port: u16, hmr_port: u16) -> String {
    let named: Vec<String> = exposed
        .iter()
        .map(|(on, l)| {
            let at = std::net::SocketAddr::new(l.addr, *on);
            let role = if *on == port { "the app port" } else if *on == hmr_port { "the HMR port" } else { "a port" };
            match l.pid {
                Some(pid) => format!("{at} ({role}, pid {pid})"),
                None => format!("{at} ({role})"),
            }
        })
        .collect();
    format!(
        "stopped the preview: it listened beyond this Mac's loopback, on {}. A preview in {} runs on this \
         machine, whose sandbox can hold it to its two ports but not to the loopback (#3559), so darkmux stops one \
         that reaches past it. Bind the dev server to 127.0.0.1 (the recipe's HOST `{{host}}` or `0.0.0.0` is \
         given as 127.0.0.1 here; a `--host 0.0.0.0` in its command is not), and a Vite HMR port too \
         (`server.hmr.host`)",
        named.join(", "),
        darkmux_sandbox::placed_in(kind),
    )
}

/// Why a dev server on the host never answered on `port`, from what
/// listens on it (`None`: unknown).
pub(crate) fn not_ready_reason(kind: BackendKind, port: u16, listeners: Option<&[PortListener]>) -> String {
    let sandbox = darkmux_sandbox::placed_in(kind);
    let v6_loopback = std::net::IpAddr::from(std::net::Ipv6Addr::LOCALHOST);
    match listeners {
        None => format!("the dev server didn't answer on port {port} within 3 minutes; in {sandbox} it must listen on 127.0.0.1:{port}"),
        Some([]) => format!(
            "nothing listens on port {port} after 3 minutes: in {sandbox} the dev server runs on this machine and must \
             listen on 127.0.0.1 at $PORT (the recipe's `{{port}}`), the only port it may bind besides its HMR port"
        ),
        Some(all) if all.iter().all(|l| l.addr == v6_loopback) => format!(
            "the dev server listens on [::1]:{port} (IPv6 loopback) only; the preview is reached at 127.0.0.1: bind it \
             to 127.0.0.1 (the recipe's HOST `{{host}}`)"
        ),
        Some(_) => format!("the dev server listens on 127.0.0.1:{port} but never answered with a 2xx or 3xx within 3 minutes"),
    }
}

/// The listeners beyond the loopback on `ports`, as `platform` reads them
/// from the kernel's socket table (not by process, which one that keeps
/// forking can race). Each port's read is asked once more when it does not
/// read; a second miss is an error naming why (the watch then stops the
/// preview: it cannot vouch for it), never "nothing listens".
pub(crate) fn exposed_in(platform: &dyn darkmux_platform::Platform, kind: BackendKind, ports: [u16; 2]) -> Result<Vec<(u16, PortListener)>, String> {
    let mut exposed = Vec::new();
    for port in ports {
        let Some(all) = platform.tcp_port_listeners(port).or_else(|| platform.tcp_port_listeners(port)) else {
            return Err(format!(
                "stopped the preview: darkmux cannot read what listens on its port {port} here, and a preview in {} \
                 must be watched (its sandbox cannot hold a bind to the loopback, #3559). To run previews in a \
                 container instead, `darkmux config set {} {}`",
                darkmux_sandbox::placed_in(kind),
                darkmux_sandbox::SANDBOX_KEY,
                darkmux_sandbox::setting_value(BackendKind::Docker),
            ));
        };
        exposed.extend(beyond_loopback(&all).into_iter().map(|l| (port, l)));
    }
    Ok(exposed)
}

/// What a running preview's sandbox holds: its two ports, and its
/// launcher's start stamp once started.
#[derive(Debug, Clone, Copy)]
struct Held {
    ports: [u16; 2],
    started: Option<u64>,
}

/// The production [`PreviewContainers`] for a sandbox on the host: the
/// backend's own commands for the preview's policy, this machine's
/// toolchain, the listener watch and the post-stop sweep through
/// `darkmux-platform`.
pub struct OnHost {
    kind: BackendKind,
    held: Mutex<HashMap<String, Held>>,
}

/// How long one lifecycle command of a sandbox on the host may take.
const CALL_BOUND: Duration = Duration::from_secs(30);

/// How many looks the post-stop sweep takes, 50 ms apart.
const SWEEP_LOOKS: u32 = 40;

impl OnHost {
    pub fn new(kind: BackendKind) -> Self {
        OnHost { kind, held: Mutex::new(HashMap::new()) }
    }

    fn handle(&self, name: &str) -> SandboxHandle {
        SandboxHandle { backend: self.kind, id: name.to_string() }
    }

    /// (#3112 review) After a stop, whatever still listens on the sandbox's
    /// two ports and started no earlier than its launcher is killed, until
    /// both are free: a process that left the sandbox's process group
    /// (`setsid`, `setpgid`) outlives the group's kill, and the kernel lets
    /// it bind only these two ports. Returns what still listens. A process
    /// that left the group and holds no port is not found: there is no mark
    /// on it a sweep could read reliably.
    pub(crate) fn sweep(&self, name: &str) -> Vec<(u16, PortListener)> {
        let held = self.held.lock().unwrap().remove(name);
        let Some(Held { ports, started: Some(started) }) = held else {
            return Vec::new();
        };
        sweep_ports(darkmux_platform::current(), ports, started, |pid| {
            // SAFETY: a plain signal to a pid.
            unsafe { libc::kill(pid as i32, libc::SIGKILL) == 0 }
        })
    }
}

/// [`OnHost::sweep`] against `platform`, killing with `kill`: each look,
/// every owner of a listener on `ports` (the kernel's last user of the
/// socket, and every process the platform finds holding it) whose start
/// stamp is no earlier than `started` (a process older than the sandbox is
/// never one of its own). Up to [`SWEEP_LOOKS`] looks, 50 ms apart; returns
/// what still listens after the last.
pub(crate) fn sweep_ports(
    platform: &dyn darkmux_platform::Platform,
    ports: [u16; 2],
    started: u64,
    kill: impl Fn(u32) -> bool,
) -> Vec<(u16, PortListener)> {
    let mut left = Vec::new();
    for look in 0..SWEEP_LOOKS {
        left = ports
            .iter()
            .flat_map(|&p| platform.tcp_port_listeners(p).unwrap_or_default().into_iter().map(move |l| (p, l)))
            .collect::<Vec<_>>();
        if left.is_empty() {
            break;
        }
        let mut owners: Vec<u32> = left.iter().filter_map(|(_, l)| l.pid).collect();
        for &port in &ports {
            owners.extend(platform.tcp_listeners(port).unwrap_or_default());
        }
        owners.sort_unstable();
        owners.dedup();
        for pid in owners.into_iter().filter(|&pid| platform.process_start_stamp(pid).is_some_and(|s| s >= started)) {
            kill(pid);
        }
        if look + 1 < SWEEP_LOOKS {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    left
}

impl PreviewContainers for OnHost {
    fn kind(&self) -> BackendKind {
        self.kind
    }

    /// No image: the sandbox's own name stands in for one, and the
    /// toolchain's identity is what the install fingerprint counts.
    fn image_and_user(&self) -> Result<(String, Option<String>), String> {
        Ok((darkmux_sandbox::setting_value(self.kind).to_string(), None))
    }

    fn toolchain(&self, recipe: &Recipe, workdir: &Path) -> Result<Toolchain, String> {
        let home = std::env::var_os("HOME").map(PathBuf::from).map(|h| h.canonicalize().unwrap_or(h));
        resolve_toolchain(recipe, self.kind, workdir, &std::env::var("PATH").unwrap_or_default(), home.as_deref())
    }

    fn host_labels(&self) -> Vec<String> {
        darkmux_crew::orphan_reaper::HostIdentity::this_process()
            .map(|h| darkmux_crew::orphan_reaper::host_labels(&h))
            .unwrap_or_default()
    }

    fn create(&self, spec: &ContainerSpec) -> Result<(), String> {
        use darkmux_crew::bounded_command::{run_bounded, Bounded};
        let (cmd, _) = darkmux_sandbox::backend_of(self.kind).create(&super::preview_policy(spec));
        match run_bounded(cmd, CALL_BOUND) {
            Bounded::Finished { success: true, .. } => {
                self.held.lock().unwrap().insert(spec.name.clone(), Held { ports: spec.ports, started: None });
                Ok(())
            }
            Bounded::Finished { stderr, .. } => Err(format!("making the preview's sandbox: {}", String::from_utf8_lossy(&stderr).trim())),
            other => Err(format!("making the preview's sandbox did not finish: {other:?}")),
        }
    }

    fn start(&self, name: &str, log: &Path) -> Result<std::process::Child, String> {
        let out = std::fs::OpenOptions::new().append(true).create(true).open(log).map_err(|e| format!("opening {}: {e}", log.display()))?;
        let err = out.try_clone().map_err(|e| format!("opening {}: {e}", log.display()))?;
        let handle = self.handle(name);
        let mut cmd = darkmux_sandbox::for_handle(&handle).start(&handle);
        cmd.stdin(std::process::Stdio::null()).stdout(out).stderr(err);
        // Its own process group, which the backend's launcher leads: a
        // Ctrl-C at `client serve` reaches darkmux, which removes every
        // preview itself.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let child = cmd.spawn().map_err(|e| format!("starting the preview's sandbox: {e}"))?;
        let stamp = darkmux_platform::current().process_start_stamp(child.id());
        if let Some(held) = self.held.lock().unwrap().get_mut(name) {
            held.started = stamp;
        }
        Ok(child)
    }

    fn listeners(&self, _name: &str) -> Option<String> {
        None
    }

    fn not_ready_reason(&self, _name: &str, port: u16) -> String {
        not_ready_reason(self.kind, port, darkmux_platform::current().tcp_port_listeners(port).as_deref())
    }

    fn exposed(&self, ports: [u16; 2]) -> Result<Vec<(u16, PortListener)>, String> {
        exposed_in(darkmux_platform::current(), self.kind, ports)
    }

    fn remove(&self, name: &str) {
        use darkmux_crew::bounded_command::run_bounded;
        let quiet = |mut c: std::process::Command| {
            c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
            c
        };
        let handle = self.handle(name);
        let backend = darkmux_sandbox::for_handle(&handle);
        let _ = run_bounded(quiet(backend.stop(&handle, Duration::from_secs(5))), CALL_BOUND);
        let _ = run_bounded(quiet(backend.remove(&handle)), CALL_BOUND);
        for (port, l) in self.sweep(name) {
            eprintln!(
                "darkmux client serve: preview sandbox {name}: {} still listens on its port {port} after it was stopped \
                 and swept",
                std::net::SocketAddr::new(l.addr, port)
            );
        }
    }

    fn reap_orphans(&self) -> Vec<String> {
        super::reap_preview_orphans()
    }
}
