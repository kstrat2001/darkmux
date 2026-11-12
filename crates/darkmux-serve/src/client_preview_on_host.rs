//! (#3112) Previews in a sandbox on the host ([`BackendKind::on_host`]:
//! Seatbelt now, Landlock later): no image, so the recipe runs this
//! machine's own toolchain, granted read-only ([`resolve_toolchain`]); no
//! published port, so the dev server binds host ports itself, and since
//! Seatbelt cannot hold that bind to the loopback (#3559) darkmux watches
//! what the preview's process group listens on ([`beyond_loopback`]).

use super::{ContainerSpec, PreviewContainers, Recipe};
use darkmux_platform::GroupListener;
use darkmux_sandbox::{BackendKind, SandboxHandle};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What a preview on the host runs with in place of an image.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Toolchain {
    /// `PATH` inside the sandbox; `None` keeps the image's (Docker).
    pub path: Option<String>,
    /// The install folders granted read-only, beyond what the sandbox
    /// already reads.
    pub read_only: Vec<PathBuf>,
    /// What the install fingerprint counts in place of an image id: each
    /// program and where it really is, so a new node reinstalls.
    pub identity: String,
}

/// Shell words that are not programs on PATH.
const SHELL_WORDS: &[&str] = &[
    "cd", "export", "set", "unset", ".", "source", "exit", "wait", "trap", "read", "eval", "shift", "return", "local",
    ":", "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac", "in", "{", "}", "!",
];

/// Words that run the next word as the program.
const PREFIX_WORDS: &[&str] = &["exec", "env", "command", "nohup"];

/// How many interpreters deep a script's `#!/usr/bin/env` is followed.
const SHEBANG_DEPTH: usize = 3;

/// The programs a shell command runs: the first word of each simple
/// command (after `;`, `&&`, `||`, `|`, `&`, a newline or a parenthesis),
/// past variable assignments, redirections and `exec`/`env` (and its
/// flags), that is not a shell word and names no path (a workspace script)
/// or variable. `env` (or `nohup`) with nothing after it is the program.
pub(crate) fn programs_of(command: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let flat = command.replace(">&", "> ").replace("<&", "< ");
    for segment in flat.split([';', '&', '|', '\n', '(', ')']) {
        let Some(word) = program_of(segment) else { continue };
        if !SHELL_WORDS.contains(&word.as_str()) && !word.contains('/') && !word.contains('$') && !out.contains(&word) {
            out.push(word);
        }
    }
    out
}

/// The program one simple command runs, when it runs one.
fn program_of(segment: &str) -> Option<String> {
    let redirect = |w: &str| w.trim_start_matches(|c: char| c.is_ascii_digit()).starts_with(['<', '>']);
    let mut words = segment.split_whitespace();
    let mut prefix = None;
    while let Some(w) = words.next() {
        if redirect(w) {
            // `> file` names its target in the next word; `>file` in this one.
            if w.trim_start_matches(|c: char| c.is_ascii_digit()).trim_start_matches(['<', '>']).is_empty() {
                words.next();
            }
            continue;
        }
        if (w.contains('=') && !w.starts_with(['=', '-'])) || (prefix.is_some() && w.starts_with('-')) {
            continue;
        }
        if PREFIX_WORDS.contains(&w) {
            prefix = prefix.or(Some(w));
            continue;
        }
        return Some(w.trim_matches(|c| c == '"' || c == '\'').to_string()).filter(|w| !w.is_empty());
    }
    prefix.filter(|p| ["env", "nohup"].contains(p)).map(str::to_string)
}

/// The absolute directories of a `PATH` value, in order, once each.
fn path_dirs(path_var: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for d in path_var.split(':').filter(|d| d.starts_with('/')).map(PathBuf::from) {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    dirs
}

/// The first executable file named `name` in `dirs`.
fn find_program(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(name)).find(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file() && executable(&m)))
}

#[cfg(unix)]
fn executable(m: &std::fs::Metadata) -> bool {
    std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_m: &std::fs::Metadata) -> bool {
    true
}

/// The folder a program's install lives in, from the folder it is in: a
/// `bin`, `sbin` or `shims` folder's parent (nvm's version folder, a
/// toolchain's root), unless that parent is the home or the home's shared
/// `.local`, when only the folder itself.
fn install_prefix(dir: &Path, home: Option<&Path>) -> PathBuf {
    let shared = |p: &Path| home.is_some_and(|h| p == h || p == h.join(".local"));
    match (dir.file_name().and_then(|n| n.to_str()), dir.parent()) {
        (Some("bin" | "sbin" | "shims"), Some(parent)) if !shared(parent) => parent.to_path_buf(),
        _ => dir.to_path_buf(),
    }
}

/// A script's interpreter: `#!/usr/bin/env [-S] name` gives `name` to find
/// on PATH, `#!/abs/path` gives that path.
enum Interpreter {
    OnPath(String),
    At(PathBuf),
}

fn interpreter_of(script: &Path) -> Option<Interpreter> {
    use std::io::Read;
    let mut head = [0u8; 256];
    let n = std::fs::File::open(script).and_then(|mut f| f.read(&mut head)).ok()?;
    let text = String::from_utf8_lossy(&head[..n]);
    let line = text.strip_prefix("#!")?.lines().next()?.trim();
    let mut words = line.split_whitespace();
    let first = words.next()?;
    if first.ends_with("/env") {
        return words.find(|w| !w.starts_with('-')).map(|w| Interpreter::OnPath(w.to_string()));
    }
    Some(Interpreter::At(PathBuf::from(first)))
}

/// Why a program did not resolve: not on PATH (its name), or a grant
/// refused (the whole message).
enum Unresolved {
    Missing(String),
    Refused(String),
}

/// What one resolution found: grants and the identity's lines.
struct Found {
    read_only: Vec<PathBuf>,
    identity: Vec<String>,
}

impl Found {
    /// Grants the install folder of `at` (a program, or what it links to)
    /// unless the sandbox already reads `at` or an earlier grant covers it;
    /// refuses one that would be the home or a folder above it, or `/`.
    fn grant(&mut self, program: &str, at: &Path, home: Option<&Path>, reads: &dyn Fn(&Path) -> bool) -> Result<(), Unresolved> {
        let dir = at.parent().unwrap_or(Path::new("/"));
        let prefix = install_prefix(dir, home);
        if reads(at) || self.read_only.iter().any(|g| prefix.starts_with(g)) {
            return Ok(());
        }
        if prefix.parent().is_none() || home.is_some_and(|h| h.starts_with(&prefix)) {
            return Err(Unresolved::Refused(format!(
                "`{program}` is at {}: its install folder would be {}, your home or a folder above it, which a \
                 preview may not read. Install `{program}` in a folder of its own (nvm, volta, Homebrew), or put \
                 that folder on PATH before {}",
                at.display(),
                prefix.display(),
                dir.display()
            )));
        }
        self.read_only.retain(|g| !g.starts_with(&prefix));
        self.read_only.push(prefix);
        Ok(())
    }
}

/// Resolves `program` on `dirs` into `found`, and the interpreter its
/// script names, `depth` levels down.
fn resolve_one(program: &str, dirs: &[PathBuf], home: Option<&Path>, reads: &dyn Fn(&Path) -> bool, found: &mut Found, depth: usize) -> Result<(), Unresolved> {
    let Some(at) = find_program(program, dirs) else {
        return Err(Unresolved::Missing(program.to_string()));
    };
    let real = at.canonicalize().unwrap_or_else(|_| at.clone());
    let line = format!("{program}={}", real.display());
    if found.identity.contains(&line) {
        return Ok(());
    }
    found.identity.push(line);
    found.grant(program, &at, home, reads)?;
    if real != at {
        found.grant(program, &real, home, reads)?;
    }
    match (interpreter_of(&real), depth) {
        (Some(Interpreter::OnPath(name)), d) if d < SHEBANG_DEPTH => resolve_one(&name, dirs, home, reads, found, d + 1),
        (Some(Interpreter::At(path)), _) => found.grant(program, &path.canonicalize().unwrap_or(path), home, reads),
        _ => Ok(()),
    }
}

/// The folders darkmux manages programs in for previews, searched before
/// the host's PATH. None yet: #3561 puts a darkmux-verified Node.js under
/// `~/.darkmux/toolchains/` and names its `bin` here, and it is then found,
/// granted read-only and put first on the preview's PATH by the same
/// [`resolve_toolchain`] that grants a host-installed one (one seam).
fn managed_toolchain_bins() -> Vec<PathBuf> {
    Vec::new()
}

/// The PATH a preview's programs are looked up on: darkmux's managed
/// toolchain folders first, then `host_path` (`client serve`'s own).
pub(crate) fn search_path(managed: &[PathBuf], host_path: &str) -> String {
    managed.iter().map(|d| d.display().to_string()).chain(std::iter::once(host_path.to_string())).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(":")
}

/// (#3112) The host toolchain a recipe runs under `kind`: each program its
/// `command` and `install.command` run (and the interpreter a script asks
/// `env` for: npm's node), found on `path_var` (the PATH `client serve`
/// runs with) as a shell would find it. Granted read-only: the install
/// folder of each one the sandbox does not already read (`reads`), never
/// the home (`home`) or a folder above it. The sandbox's PATH is
/// `path_var`'s folders in order, those it can read. A program not on PATH
/// is refused, naming it, where darkmux looked, and the fix.
pub(crate) fn resolve_toolchain(
    recipe: &Recipe,
    kind: BackendKind,
    path_var: &str,
    home: Option<&Path>,
    reads: &dyn Fn(&Path) -> bool,
) -> Result<Toolchain, String> {
    let dirs = path_dirs(path_var);
    let mut found = Found { read_only: Vec::new(), identity: Vec::new() };
    let commands = std::iter::once(recipe.command.as_str()).chain(recipe.install.as_ref().map(|i| i.command.as_str()));
    for program in commands.flat_map(programs_of) {
        resolve_one(&program, &dirs, home, reads, &mut found, 0).map_err(|why| match why {
            Unresolved::Missing(missing) => missing_program(&missing, &program, kind, &dirs),
            Unresolved::Refused(message) => message,
        })?;
    }
    let inside: Vec<String> = dirs
        .iter()
        .filter(|d| reads(d) || found.read_only.iter().any(|g| d.starts_with(g)))
        .map(|d| d.display().to_string())
        .collect();
    found.identity.sort();
    Ok(Toolchain { path: Some(inside.join(":")), read_only: found.read_only, identity: found.identity.join(";") })
}

/// Why `why` (which `program` runs, or is) could not be found.
fn missing_program(why: &str, program: &str, kind: BackendKind, dirs: &[PathBuf]) -> String {
    let looked: Vec<String> = dirs.iter().map(|d| d.display().to_string()).collect();
    let needed = if why == program { String::new() } else { format!(" (`{program}` runs it)") };
    format!(
        "the recipe runs `{why}`{needed}, which is not on the PATH `darkmux client serve` runs with (looked in: {}). \
         A preview in {} runs this machine's own toolchain: install `{why}` (Node.js through nvm or Homebrew, say), \
         or start `darkmux client serve` from a shell where `{why}` is on PATH",
        if looked.is_empty() { "nothing".to_string() } else { looked.join(", ") },
        darkmux_sandbox::placed_in(kind),
    )
}

/// The listeners bound beyond the loopback: every address (`0.0.0.0`,
/// `::`) or any other non-loopback one. `127.x`, `::1` and `::ffff:127.x`
/// are loopback.
pub(crate) fn beyond_loopback(listeners: &[GroupListener]) -> Vec<GroupListener> {
    let loopback = |ip: &std::net::IpAddr| match ip {
        std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
        v4 => v4.is_loopback(),
    };
    listeners.iter().filter(|l| !loopback(&l.addr)).cloned().collect()
}

/// Why a preview on the host was stopped: each listener beyond the
/// loopback, what darkmux gave the dev server, and the fix.
pub(crate) fn exposure_reason(kind: BackendKind, exposed: &[GroupListener], port: u16, hmr_port: u16) -> String {
    let named: Vec<String> = exposed
        .iter()
        .map(|l| {
            let at = std::net::SocketAddr::new(l.addr, l.port);
            let role = match l.port {
                p if p == port => "the app port, ",
                p if p == hmr_port => "the HMR port, ",
                _ => "",
            };
            format!("{at} ({role}pid {})", l.pid)
        })
        .collect();
    format!(
        "stopped the preview: it listened beyond this Mac's loopback, on {}. A preview in {} runs on this \
         machine, whose sandbox cannot hold a bind to the loopback (#3559), so darkmux stops one that reaches \
         past it. Bind the dev server to 127.0.0.1 (the recipe's HOST `{{host}}` or `0.0.0.0` is given as \
         127.0.0.1 here; a `--host 0.0.0.0` in its command is not), and a Vite HMR port too (`server.hmr.host`)",
        named.join(", "),
        darkmux_sandbox::placed_in(kind),
    )
}

/// Why a dev server on the host never answered on `port`, from what its
/// process group listens on (`None`: unknown).
pub(crate) fn not_ready_reason(kind: BackendKind, port: u16, listeners: Option<&[GroupListener]>) -> String {
    let on_port: Vec<&GroupListener> = listeners.unwrap_or_default().iter().filter(|l| l.port == port).collect();
    let sandbox = darkmux_sandbox::placed_in(kind);
    match listeners {
        None => format!("the dev server didn't answer on port {port} within 3 minutes; in {sandbox} it must listen on 127.0.0.1:{port}"),
        Some(_) if on_port.is_empty() => format!(
            "nothing in the preview listens on port {port} after 3 minutes: in {sandbox} the dev server runs on this \
             machine and must listen on 127.0.0.1 at $PORT (the recipe's `{{port}}`)"
        ),
        Some(_) if on_port.iter().all(|l| l.addr == std::net::IpAddr::from(std::net::Ipv6Addr::LOCALHOST)) => format!(
            "the dev server listens on [::1]:{port} (IPv6 loopback) only; the preview is reached at 127.0.0.1: bind it \
             to 127.0.0.1 (the recipe's HOST `{{host}}`)"
        ),
        Some(_) => format!("the dev server listens on 127.0.0.1:{port} but never answered with a 2xx or 3xx within 3 minutes"),
    }
}

/// The production [`PreviewContainers`] for a sandbox on the host: the
/// backend's own commands for the preview's policy, this machine's
/// toolchain, and the listener watch through `darkmux-platform`.
pub struct OnHost {
    pub kind: BackendKind,
}

/// How long one lifecycle command of a sandbox on the host may take.
const CALL_BOUND: Duration = Duration::from_secs(30);

impl OnHost {
    fn handle(&self, name: &str) -> SandboxHandle {
        SandboxHandle { backend: self.kind, id: name.to_string() }
    }
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

    fn toolchain(&self, recipe: &Recipe) -> Result<Toolchain, String> {
        let backend = darkmux_sandbox::backend_of(self.kind);
        let path = search_path(&managed_toolchain_bins(), &std::env::var("PATH").unwrap_or_default());
        let home = std::env::var_os("HOME").map(PathBuf::from).map(|h| h.canonicalize().unwrap_or(h));
        resolve_toolchain(recipe, self.kind, &path, home.as_deref(), &|p| backend.host_reads(p))
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
            Bounded::Finished { success: true, .. } => Ok(()),
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
        // Its own process group, which the backend's launcher leads: the
        // group the listener watch reads, and a Ctrl-C at `client serve`
        // reaches darkmux, which removes every preview itself.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        cmd.spawn().map_err(|e| format!("starting the preview's sandbox: {e}"))
    }

    fn listeners(&self, _name: &str) -> Option<String> {
        None
    }

    fn not_ready_reason(&self, _name: &str, port: u16, group: u32) -> String {
        not_ready_reason(self.kind, port, darkmux_platform::current().tcp_group_listeners(group).as_deref())
    }

    fn exposed(&self, group: u32) -> Result<Vec<GroupListener>, String> {
        exposed_in(darkmux_platform::current(), self.kind, group)
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
    }

    fn reap_orphans(&self) -> Vec<String> {
        super::reap_preview_orphans()
    }
}

/// The listeners beyond the loopback in process group `group`, as
/// `platform` reads them; an error naming why when it cannot read them (the
/// watch then stops the preview: it cannot vouch for it).
pub(crate) fn exposed_in(platform: &dyn darkmux_platform::Platform, kind: BackendKind, group: u32) -> Result<Vec<GroupListener>, String> {
    // One unreadable read is asked again before the preview is stopped.
    match platform.tcp_group_listeners(group).or_else(|| platform.tcp_group_listeners(group)) {
        Some(all) => Ok(beyond_loopback(&all)),
        None => Err(format!(
            "stopped the preview: darkmux cannot read what it listens on here, and a preview in {} must be watched \
             (its sandbox cannot hold a bind to the loopback, #3559). To run previews in a container instead, \
             `darkmux config set {} {}`",
            darkmux_sandbox::placed_in(kind),
            darkmux_sandbox::SANDBOX_KEY,
            darkmux_sandbox::setting_value(BackendKind::Docker),
        )),
    }
}
