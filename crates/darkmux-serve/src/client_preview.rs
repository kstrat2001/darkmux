//! Live previews for paired apps: darkmux runs a workspace's dev server from
//! a `.darkmux/preview.json` recipe, in a container, and tells the app its
//! URL.
//!
//! **In a container (#3456).** A preview runs the WORKSPACE's code: its
//! `package.json` scripts, its dev server and whatever they load, which a
//! model may have written. So each running preview gets one long-lived
//! container of its own, separate from dispatch containers, from the darkmux
//! runtime image resolved the way a dispatch resolves it (it has node and
//! python). The workspace is its only mount (read-write, at
//! [`WORKSPACE_AT`]), so a recipe cannot read a host path outside it. Only
//! the preview's app port and HMR port are published, each on 127.0.0.1 at
//! the same number inside; reaching a preview from a phone stays `tailscale
//! serve`, never Funnel. Its environment is exactly the recipe's `env`, the
//! port variables and [`POLLING_ENV`], so nothing `client serve` was started
//! with (an exported API key) reaches the workspace's code. It has the
//! dispatch container's hardening and the default network (an install step
//! fetches packages). It stays up across the dispatches that edit the
//! workspace, so edits hot-reload into it; a stop, the idle check (#3459)
//! and `client serve`'s shutdown remove it, and one a `client serve` that
//! died left behind is removed when the next one starts (it carries its
//! host's labels, like a dispatch container). No preview process runs on the
//! host.
//!
//! The workspace holds the persistent state (code, `node_modules`, caches);
//! the container is disposable. `node_modules` installed in the container
//! holds Linux binaries, which is fine: the workspace is darkmux's worktree,
//! not the user's checkout. A dev server must listen on `0.0.0.0` inside the
//! container (the published port is still loopback-only on the host), and
//! file watching crosses the bind mount by polling: Chokidar and Watchpack
//! read [`POLLING_ENV`]; a Vite project sets `server.watch.usePolling: true`
//! in its own config.
//!
//! Previews are still **off by default** (`darkmux config set
//! clients.preview.enabled true`). Known gap: a file a running preview
//! writes in the workspace while a run is working there (a build output, a
//! cache the repo does not ignore) is indistinguishable from the run's own
//! change and can be committed as the run's.
//!
//! **Trust.** The recipe is a command darkmux runs, so it is read ONLY from
//! the user's own checkout: the `source_path` checkout the workspace's
//! worktree was made from, as darkmux recorded it when it made the workspace
//! (`client_workspace::own_checkout_of`), never from the workspace itself. A
//! file a Coder run wrote in the workspace is never read. A workspace cloned
//! from a URL has no checkout of the user's, so it gets no preview; neither
//! does one whose checkout has no recipe (a copy in the workspace does not
//! count). Each refusal says why and what to do: add
//! `.darkmux/preview.json` to your own checkout of the repo.
//!
//! Recipe shape (contract 7 and 5: an unknown key is refused naming it, and
//! `schema_version` is required; each `env` key is a variable name):
//!
//! ```json
//! {
//!   "schema_version": "1.0",
//!   "command": "npm run dev",
//!   "env": {"PORT": "{port}", "HOST": "0.0.0.0", "VITE_HMR_PORT": "{hmrPort}"},
//!   "ready": {"http": "/"},
//!   "install": {"command": "npm ci", "unless_exists": "node_modules"},
//!   "env_from_source": [".env"]
//! }
//! ```
//!
//! Each preview gets its own free app port and HMR port, substituted into
//! `env` (`{port}`, `{hmrPort}`), so two previews never collide. `install`
//! runs first, in the container, when its `unless_exists` path is missing in
//! the workspace. `env_from_source` copies untracked files (like `.env`) from
//! that same checkout into the workspace, never overwriting and never
//! through a symlink.

use crate::client_wire::PreviewState;
use darkmux_types::data_version::PREVIEW_RECIPE_SCHEMA_VERSION;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Where a repo keeps its recipe, relative to the checkout's top.
pub const RECIPE_PATH: &str = ".darkmux/preview.json";
/// The largest `preview.json` darkmux reads.
pub const MAX_RECIPE_BYTES: u64 = 16 * 1024;

/// A `.darkmux/preview.json`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, schemars::JsonSchema)]
pub struct Recipe {
    /// The recipe format's version ([`PREVIEW_RECIPE_SCHEMA_VERSION`]).
    pub schema_version: String,
    pub command: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<Ready>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<Install>,
    #[serde(default)]
    pub env_from_source: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, schemars::JsonSchema)]
pub struct Ready {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, schemars::JsonSchema)]
pub struct Install {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unless_exists: Option<String>,
}

/// Why a recipe was not used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipeRefusal {
    /// There is none to use (no checkout of the user's, or no file in it).
    NotFound(String),
    /// A symlink where the recipe or its folder should be.
    Symlink(String),
    TooLarge(String),
    /// Not a recipe this darkmux runs: not JSON, an unknown key, a missing
    /// or unknown `schema_version`, a path that escapes the work copy.
    Invalid(String),
    /// The disk could not be read.
    Unreadable(String),
}

impl RecipeRefusal {
    pub fn message(&self) -> &str {
        match self {
            RecipeRefusal::NotFound(m)
            | RecipeRefusal::Symlink(m)
            | RecipeRefusal::TooLarge(m)
            | RecipeRefusal::Invalid(m)
            | RecipeRefusal::Unreadable(m) => m,
        }
    }
}

/// The recipe file's text in `checkout`: a symlinked `.darkmux` or
/// `preview.json` is refused, and the file is opened without following a
/// link.
pub fn read_recipe_text(checkout: &Path) -> Result<String, RecipeRefusal> {
    let file = checkout.join(RECIPE_PATH);
    for p in [&checkout.join(".darkmux"), &file] {
        exists_unlinked(p, checkout)?;
    }
    let text = read_capped(&file)?;
    if text.len() as u64 > MAX_RECIPE_BYTES {
        return Err(RecipeRefusal::TooLarge(format!(
            "{RECIPE_PATH} is larger than {MAX_RECIPE_BYTES} bytes"
        )));
    }
    Ok(text)
}

/// `p` exists and is not a symlink.
fn exists_unlinked(p: &Path, checkout: &Path) -> Result<(), RecipeRefusal> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_symlink() => Err(RecipeRefusal::Symlink(format!(
            "{} is a symlink; refusing",
            p.display()
        ))),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(RecipeRefusal::NotFound(
            format!("no {RECIPE_PATH} in {}", checkout.display()),
        )),
        Err(e) => Err(RecipeRefusal::Unreadable(format!(
            "reading {}: {e}",
            p.display()
        ))),
    }
}

/// A regular file's text, opened without following a link, one byte past
/// the cap at most (so an oversized file is told apart).
fn read_capped(file: &Path) -> Result<String, RecipeRefusal> {
    use std::io::Read;
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let mut text = String::new();
    opts.open(file)
        .and_then(|f| {
            if !f.metadata()?.is_file() {
                return Err(std::io::Error::other("not a file"));
            }
            f.take(MAX_RECIPE_BYTES + 1).read_to_string(&mut text)
        })
        .map_err(|e| RecipeRefusal::Unreadable(format!("reading {}: {e}", file.display())))?;
    Ok(text)
}

/// The recipe's `schema_version` must be one this darkmux reads.
fn check_version(doc: &serde_json::Value) -> Result<(), RecipeRefusal> {
    if let Some(v) = darkmux_types::data_version::newer(doc, PREVIEW_RECIPE_SCHEMA_VERSION) {
        return Err(RecipeRefusal::Invalid(format!(
            "{RECIPE_PATH}: {}",
            darkmux_types::data_version::newer_refusal(
                "preview recipe",
                &v,
                PREVIEW_RECIPE_SCHEMA_VERSION
            )
        )));
    }
    match doc.get("schema_version").and_then(serde_json::Value::as_str) {
        Some(PREVIEW_RECIPE_SCHEMA_VERSION) => Ok(()),
        other => Err(RecipeRefusal::Invalid(format!(
            "{RECIPE_PATH}: `schema_version` {} is not one this darkmux reads (write \"{PREVIEW_RECIPE_SCHEMA_VERSION}\")",
            other.map_or_else(|| "is missing; it".to_string(), |v| format!("\"{v}\""))
        ))),
    }
}

/// Paths the recipe names inside the work copy: plain relative ones only.
fn check_recipe_paths(recipe: &Recipe) -> Result<(), RecipeRefusal> {
    let unless = recipe
        .install
        .as_ref()
        .and_then(|i| i.unless_exists.as_ref());
    let named = recipe
        .env_from_source
        .iter()
        .map(|f| ("env_from_source", f))
        .chain(unless.map(|u| ("install.unless_exists", u)));
    for (key, p) in named {
        if !plain_relative(p) {
            return Err(RecipeRefusal::Invalid(format!(
                "{key}: {p:?} must be a plain relative path"
            )));
        }
    }
    Ok(())
}

/// A recipe's text, checked: a JSON object whose every key the format
/// knows (an unknown or misspelled one is refused naming it and the closest
/// valid key), a `schema_version` this darkmux reads, and paths that stay
/// inside the work copy.
pub fn parse_recipe(text: &str) -> Result<Recipe, RecipeRefusal> {
    let doc: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| RecipeRefusal::Invalid(format!("{RECIPE_PATH} is not JSON: {e}")))?;
    if !doc.is_object() {
        return Err(RecipeRefusal::Invalid(format!(
            "{RECIPE_PATH} is not a JSON object"
        )));
    }
    check_version(&doc)?;
    let issues = darkmux_types::user_files::key_issues::<Recipe>(
        &doc,
        &darkmux_types::user_files::no_retired,
    );
    if !issues.is_empty() {
        let lines: Vec<String> = issues.iter().map(ToString::to_string).collect();
        return Err(RecipeRefusal::Invalid(format!(
            "{RECIPE_PATH}: {}",
            lines.join("; ")
        )));
    }
    let recipe: Recipe = serde_json::from_value(doc)
        .map_err(|e| RecipeRefusal::Invalid(format!("{RECIPE_PATH}: {e}")))?;
    check_recipe_paths(&recipe)?;
    check_env_names(&recipe)?;
    Ok(recipe)
}

/// (#3456) Each `env` key is a plain variable name (`[A-Za-z_][A-Za-z0-9_]*`):
/// it reaches the preview container as `-e KEY=value`.
fn check_env_names(recipe: &Recipe) -> Result<(), RecipeRefusal> {
    let is_name = |k: &str| {
        k.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    let mut keys: Vec<&String> = recipe.env.keys().collect();
    keys.sort();
    match keys.into_iter().find(|k| !is_name(k)) {
        Some(bad) => Err(RecipeRefusal::Invalid(format!(
            "{RECIPE_PATH}: env: {bad:?} is not a variable name (letters, digits and `_`, not starting with a digit)"
        ))),
        None => Ok(()),
    }
}

/// The recipe in the user's own `checkout`, read and checked.
pub fn load_recipe(checkout: &Path) -> Result<Recipe, RecipeRefusal> {
    parse_recipe(&read_recipe_text(checkout)?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Installing,
    Starting,
    Ready,
    Failed(String),
    Stopped,
}

impl Status {
    fn word(&self) -> &'static str {
        match self {
            Status::Installing => "installing",
            Status::Starting => "starting",
            Status::Ready => "ready",
            Status::Failed(_) => "failed",
            Status::Stopped => "stopped",
        }
    }
    fn live(&self) -> bool {
        matches!(self, Status::Installing | Status::Starting | Status::Ready)
    }
}

#[derive(Debug)]
struct Entry {
    port: u16,
    hmr_port: u16,
    status: Status,
    /// The preview's container, once its `docker run` has started; taken
    /// by whoever stops it.
    container: Option<String>,
    log: PathBuf,
    /// When an app last asked about it (a start or a status), in the
    /// [`Previews`] clock's seconds.
    last_asked: u64,
}

/// Seconds on a wall clock: Unix seconds in production. A wall clock, not
/// `Instant`, so time a Mac spends asleep counts as idle.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// All previews this darkmux runs, by workspace name.
pub struct Previews {
    dir: PathBuf,
    containers: Arc<dyn PreviewContainers>,
    entries: Mutex<HashMap<String, Arc<Mutex<Entry>>>>,
    clock: Clock,
}

/// A relative path inside the work copy: no absolute paths, no `..`.
pub fn plain_relative(p: &str) -> bool {
    let path = Path::new(p);
    !p.is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// A free TCP port on 127.0.0.1, by asking the OS.
fn free_port() -> Result<u16, String> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| format!("finding a free port: {e}"))
}

fn substitute(value: &str, port: u16, hmr: u16) -> String {
    value
        .replace("{port}", &port.to_string())
        .replace("{hmrPort}", &hmr.to_string())
}

/// One HTTP GET; ready on any 2xx or 3xx.
fn http_ok(port: u16, path: &str) -> bool {
    use std::io::{Read, Write};
    let Ok(mut s) = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    ) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let req = format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut head = [0u8; 16];
    let n = s.read(&mut head).unwrap_or(0);
    let line = String::from_utf8_lossy(&head[..n]);
    line.starts_with("HTTP/") && matches!(line.as_bytes().get(9), Some(b'2') | Some(b'3'))
}

fn tail(log: &Path, lines: usize) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

// ─── The preview container ──────────────────────────────────────────────

/// Every preview container's name starts with this (contract 4: darkmux
/// removes only containers it names this way).
pub use darkmux_crew::orphan_reaper::PREVIEW_CONTAINER_PREFIX;

/// Where the workspace is mounted in a preview container.
pub const WORKSPACE_AT: &str = "/workspace";

/// The label naming the workspace a preview container serves.
pub const WORKSPACE_LABEL: &str = "darkmux.preview.workspace";

/// What a preview container gets in its environment before the recipe's
/// own `env` (which may override them): file watchers poll, because change
/// events do not cross a Docker Desktop bind mount reliably. Chokidar (most
/// node dev servers) and Watchpack (webpack, Next.js) read these; Vite
/// needs `server.watch.usePolling` in its own config.
pub const POLLING_ENV: &[(&str, &str)] = &[
    ("CHOKIDAR_USEPOLLING", "true"),
    ("WATCHPACK_POLLING", "true"),
];

/// The first line a preview container prints: without it, its exit is
/// Docker's (no image, a bad mount), not the recipe's.
const STARTED_LINE: &str = "darkmux-preview: container started";
/// Printed after the install step, before the dev server starts.
const SERVING_LINE: &str = "darkmux-preview: starting the dev server";
/// Printed when the install step fails, followed by its exit code.
const INSTALL_FAILED_LINE: &str = "darkmux-preview: install exited with";

/// One preview container: everything `docker run` is told. Nothing of
/// `client serve`'s own environment is in it: `env` is the whole of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    /// The workspace, mounted read-write at [`WORKSPACE_AT`]; the only host
    /// path the container sees.
    pub workspace: PathBuf,
    /// The app port and the HMR port, each published on 127.0.0.1 only, at
    /// the same number inside.
    pub ports: [u16; 2],
    /// `--user`; `None` keeps the image's user.
    pub user: Option<String>,
    /// `key=value` labels: the host identity the orphan reaper reads, and
    /// the workspace.
    pub labels: Vec<String>,
    /// The container's environment, in order (a later key wins).
    pub env: Vec<(String, String)>,
    /// The `sh -c` script: the install step when it is due, then the dev
    /// server.
    pub script: String,
}

/// `docker run` arguments for a preview container: the dispatch
/// container's hardening (no capabilities, no new privileges, its pid and
/// memory limits, `--init`), removed when it exits, the workspace the only
/// mount, only the preview's two ports published and only on loopback, and
/// an environment of exactly `spec.env`. It keeps the default network: an
/// install step fetches packages.
pub fn docker_run_args(spec: &ContainerSpec) -> Vec<String> {
    use darkmux_crew::dispatch_internal::{
        DOCKER_CAP_DROP, DOCKER_MEMORY, DOCKER_PIDS_LIMIT, DOCKER_SECURITY_OPT,
    };
    let mut args: Vec<String> = ["run", "--rm", "--init", "--name", &spec.name]
        .iter()
        .map(|s| s.to_string())
        .collect();
    args.extend([
        format!("--cap-drop={DOCKER_CAP_DROP}"),
        format!("--security-opt={DOCKER_SECURITY_OPT}"),
        format!("--pids-limit={DOCKER_PIDS_LIMIT}"),
        format!("--memory={DOCKER_MEMORY}"),
    ]);
    for label in &spec.labels {
        args.extend(["--label".to_string(), label.clone()]);
    }
    args.extend([
        "-v".to_string(),
        format!("{}:{WORKSPACE_AT}", spec.workspace.display()),
        "-w".to_string(),
        WORKSPACE_AT.to_string(),
    ]);
    for port in spec.ports {
        args.extend(["-p".to_string(), format!("127.0.0.1:{port}:{port}")]);
    }
    if let Some(user) = &spec.user {
        // The image's home belongs to the image's own user.
        args.extend([
            format!("--user={user}"),
            "-e".to_string(),
            "HOME=/tmp".to_string(),
        ]);
    }
    for (k, v) in &spec.env {
        // Always `KEY=value`: a bare `-e KEY` would copy the host's value.
        args.extend(["-e".to_string(), format!("{k}={v}")]);
    }
    args.extend([
        // The runtime image's own entrypoint never wraps the script.
        "--entrypoint".to_string(),
        "sh".to_string(),
        spec.image.clone(),
        "-c".to_string(),
        spec.script.clone(),
    ]);
    args
}

/// The container's script: a start line, the install step when it is due
/// (in a subshell, so its own `exit` ends only it), then the dev server.
fn container_script(recipe: &Recipe, install_due: bool) -> String {
    let mut script = format!("echo '{STARTED_LINE}'\n");
    if let (true, Some(install)) = (install_due, &recipe.install) {
        script.push_str(&format!(
            "(\n{}\n) || {{ s=$?; echo \"{INSTALL_FAILED_LINE} $s\"; exit $s; }}\n",
            install.command
        ));
    }
    script.push_str(&format!("echo '{SERVING_LINE}'\n{}\n", recipe.command));
    script
}

/// The container's environment: polling first, the recipe's `env` with
/// its ports filled in, then the port variables, which the recipe cannot
/// override.
fn container_env(recipe: &Recipe, port: u16, hmr_port: u16) -> Vec<(String, String)> {
    let mut recipe_env: Vec<(&String, &String)> = recipe.env.iter().collect();
    recipe_env.sort();
    POLLING_ENV
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .chain(
            recipe_env
                .into_iter()
                .map(|(k, v)| (k.clone(), substitute(v, port, hmr_port))),
        )
        .chain([
            ("DARKMUX_PREVIEW_PORT".to_string(), port.to_string()),
            ("DARKMUX_PREVIEW_HMR_PORT".to_string(), hmr_port.to_string()),
        ])
        .collect()
}

/// What runs preview containers: Docker in production. A seam so the
/// preview lifecycle is testable without Docker.
pub trait PreviewContainers: Send + Sync {
    /// The image and `--user` a preview container runs with, resolved as a
    /// dispatch resolves them. May pull the version-pinned runtime image.
    fn image_and_user(&self) -> Result<(String, Option<String>), String>;
    /// The labels naming the darkmux process that starts a container.
    fn host_labels(&self) -> Vec<String>;
    /// Starts the container, attached: its output goes to `log`, and the
    /// child ends when the container does.
    fn run(&self, spec: &ContainerSpec, log: &Path) -> Result<std::process::Child, String>;
    /// Stops and removes the container: TERM, KILL after five seconds.
    /// Best effort; a container already gone is the goal.
    fn remove(&self, name: &str);
    /// Removes preview containers whose `client serve` died without
    /// removing them.
    fn reap_orphans(&self) -> Vec<String>;
}

/// The production [`PreviewContainers`]: the `docker` CLI, the darkmux
/// runtime image.
pub struct Docker {
    /// An image to use instead of resolving the runtime image (the opt-in
    /// Docker test names one already present, so it never pulls).
    pub image: Option<String>,
}

/// How long one `docker stop` or `docker rm` may take.
const DOCKER_CALL_BOUND: Duration = Duration::from_secs(30);

impl PreviewContainers for Docker {
    fn image_and_user(&self) -> Result<(String, Option<String>), String> {
        let image = match &self.image {
            Some(image) => image.clone(),
            None => darkmux_crew::dispatch_internal::resolved_runtime_image()
                .map_err(|e| format!("{e:#}"))?,
        };
        let user = darkmux_crew::dispatch_internal::runtime_container_user()
            .map_err(|e| format!("{e:#}"))?;
        Ok((image, user))
    }

    fn host_labels(&self) -> Vec<String> {
        darkmux_crew::orphan_reaper::HostIdentity::this_process()
            .map(|h| darkmux_crew::orphan_reaper::host_labels(&h))
            .unwrap_or_default()
    }

    fn run(&self, spec: &ContainerSpec, log: &Path) -> Result<std::process::Child, String> {
        let out = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(log)
            .map_err(|e| format!("opening {}: {e}", log.display()))?;
        let err = out
            .try_clone()
            .map_err(|e| format!("opening {}: {e}", log.display()))?;
        let mut cmd = std::process::Command::new("docker");
        cmd.args(docker_run_args(spec))
            .stdin(std::process::Stdio::null())
            .stdout(out)
            .stderr(err);
        // Its own process group: a Ctrl-C at `client serve` reaches darkmux,
        // which removes every preview container itself.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        cmd.spawn().map_err(|e| format!("starting docker: {e}"))
    }

    fn remove(&self, name: &str) {
        use darkmux_crew::bounded_command::run_bounded;
        let quiet = |args: &[&str]| {
            let mut c = std::process::Command::new("docker");
            c.args(args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            c
        };
        let _ = run_bounded(quiet(&["stop", "-t", "5", name]), DOCKER_CALL_BOUND);
        let _ = run_bounded(quiet(&["rm", "-f", name]), DOCKER_CALL_BOUND);
    }

    fn reap_orphans(&self) -> Vec<String> {
        use darkmux_crew::orphan_reaper::{host_liveness, reap_tick_named, LocalHost, ReapTick};
        match reap_tick_named(
            PREVIEW_CONTAINER_PREFIX,
            LocalHost::this_host().as_ref(),
            host_liveness,
        ) {
            ReapTick::Listed { reaped } => reaped.into_iter().map(|(o, _)| o.name).collect(),
            ReapTick::DockerUnavailable => Vec::new(),
        }
    }
}

impl Previews {
    pub fn new(dir: PathBuf, containers: Arc<dyn PreviewContainers>) -> Self {
        Self {
            dir,
            containers,
            entries: Mutex::new(HashMap::new()),
            clock: Arc::new(unix_seconds),
        }
    }

    /// The same previews on another clock (a test's frozen one).
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    pub fn production() -> Self {
        let dir = darkmux_types::workdir::worktrees_base_dir()
            .parent()
            .map(|p| p.join("previews"))
            .unwrap_or_else(|| PathBuf::from("previews"));
        Self::new(dir, Arc::new(Docker { image: None }))
    }

    fn snapshot(name: &str, e: &Entry) -> PreviewState {
        let failed = match &e.status {
            Status::Failed(why) => Some(why.clone()),
            _ => None,
        };
        PreviewState {
            workspace: name.to_string(),
            status: e.status.word().to_string(),
            url: format!("http://127.0.0.1:{}/", e.port),
            port: e.port,
            hmr_port: e.hmr_port,
            log_tail: failed.as_ref().map(|_| tail(&e.log, 20)),
            error: failed,
        }
    }

    /// Starts the workspace's preview (or returns the running one) from the
    /// recipe in `checkout`, the user's own checkout the workspace was made
    /// from: the recipe is read there, never in `workdir`, which is what
    /// the preview's container mounts. The boolean says whether it was
    /// already running.
    pub fn start(
        &self,
        name: &str,
        workdir: &Path,
        checkout: &Path,
    ) -> Result<(PreviewState, bool), RecipeRefusal> {
        let mut entries = self.entries.lock().unwrap();
        if let Some(e) = entries.get(name) {
            let mut e = e.lock().unwrap();
            if e.status.live() {
                e.last_asked = (self.clock)();
                return Ok((Self::snapshot(name, &e), true));
            }
        }
        let recipe = load_recipe(checkout)?;
        let unreadable = |e: String| RecipeRefusal::Unreadable(e);
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| unreadable(format!("{}: {e}", self.dir.display())))?;
        let (port, hmr_port) = (
            free_port().map_err(unreadable)?,
            free_port().map_err(unreadable)?,
        );
        let log = self.dir.join(format!("{name}.log"));
        let _ = std::fs::write(&log, "");
        let entry = Arc::new(Mutex::new(Entry {
            port,
            hmr_port,
            status: Status::Starting,
            container: None,
            log: log.clone(),
            last_asked: (self.clock)(),
        }));
        let snap = Self::snapshot(name, &entry.lock().unwrap());
        entries.insert(name.to_string(), entry.clone());
        drop(entries);

        let at = RunIn {
            name: name.to_string(),
            workdir: workdir.to_path_buf(),
            checkout: checkout.to_path_buf(),
            log,
            port,
            hmr_port,
        };
        let containers = self.containers.clone();
        std::thread::spawn(move || run(entry, recipe, at, &*containers));
        Ok((snap, false))
    }

    /// The preview's state. An app asking counts: it restarts the idle
    /// count ([`Previews::stop_idle`]).
    pub fn status(&self, name: &str) -> Option<PreviewState> {
        let entries = self.entries.lock().unwrap();
        let mut e = entries.get(name)?.lock().unwrap();
        e.last_asked = (self.clock)();
        Some(Self::snapshot(name, &e))
    }

    /// Stops it: its container gets TERM, then KILL after five seconds, and
    /// is removed.
    pub fn stop(&self, name: &str) -> bool {
        let entry = self.entries.lock().unwrap().get(name).cloned();
        let Some(entry) = entry else { return false };
        let container = {
            let mut e = entry.lock().unwrap();
            e.status = Status::Stopped;
            e.container.take()
        };
        if let Some(container) = container {
            self.containers.remove(&container);
        }
        true
    }

    /// Stops every preview, live or not, and removes its container: what
    /// `client serve` does as it shuts down. Returns the workspaces whose
    /// container it removed.
    pub fn stop_all(&self) -> Vec<String> {
        self.stop_where(|_| true)
    }

    /// (#3459) Stops every live preview nobody has asked about (a start or
    /// a status) for `idle_minutes`, the way [`Previews::stop`] does; `0`
    /// never stops one. An app that quit or crashed never stops its own
    /// preview, so without this a dev server ran until darkmux did. Each
    /// is judged and marked stopped under its own lock, so a status that
    /// lands meanwhile either counts or finds it stopped. Returns the
    /// workspaces stopped.
    pub fn stop_idle(&self, idle_minutes: u64) -> Vec<String> {
        if idle_minutes == 0 {
            return Vec::new();
        }
        let now = (self.clock)();
        let idle_after = idle_minutes.saturating_mul(60);
        self.stop_where(|e| e.status.live() && now.saturating_sub(e.last_asked) >= idle_after)
    }

    /// Marks every preview `pick` chooses stopped, under its own lock, then
    /// removes their containers. Returns their workspaces, sorted.
    fn stop_where(&self, pick: impl Fn(&Entry) -> bool) -> Vec<String> {
        let mut ended = Vec::new();
        for (name, entry) in self.entries.lock().unwrap().iter() {
            let mut e = entry.lock().unwrap();
            if pick(&e) {
                e.status = Status::Stopped;
                ended.push((name.clone(), e.container.take()));
            }
        }
        for container in ended.iter().filter_map(|(_, c)| c.as_ref()) {
            self.containers.remove(container);
        }
        let mut names: Vec<String> = ended.into_iter().map(|(n, _)| n).collect();
        names.sort();
        names
    }

    /// Removes preview containers a `client serve` that died left behind
    /// (their labeled host process is gone). Returns their names.
    pub fn reap_orphans(&self) -> Vec<String> {
        self.containers.reap_orphans()
    }
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn set(entry: &Arc<Mutex<Entry>>, status: Status) -> bool {
    let mut e = entry.lock().unwrap();
    if e.status == Status::Stopped {
        return false;
    }
    e.status = status;
    true
}

/// Why a preview's run ended before its dev server did: it was stopped (say
/// nothing), or it failed (say why).
enum Halt {
    Stopped,
    Failed(String),
}

impl From<String> for Halt {
    fn from(why: String) -> Self {
        Halt::Failed(why)
    }
}

/// Moves the preview to `status`, or halts if it was stopped meanwhile.
fn advance(entry: &Arc<Mutex<Entry>>, status: Status) -> Result<(), Halt> {
    if set(entry, status) {
        Ok(())
    } else {
        Err(Halt::Stopped)
    }
}

/// Where one preview runs: its workspace, the user's own checkout its
/// recipe and env files come from, its log and its ports.
struct RunIn {
    name: String,
    workdir: PathBuf,
    checkout: PathBuf,
    log: PathBuf,
    port: u16,
    hmr_port: u16,
}

fn run(entry: Arc<Mutex<Entry>>, recipe: Recipe, at: RunIn, containers: &dyn PreviewContainers) {
    let ended = prepare(&entry, &recipe, &at)
        .and_then(|install_due| serve(&entry, &recipe, &at, install_due, containers));
    // A stop is already recorded; `set` never overwrites it with a failure.
    if let Err(Halt::Failed(why)) = ended {
        set(&entry, Status::Failed(why));
    }
}

/// Everything before the container, on the host: env files into the
/// workspace, and whether the install step is due (its `unless_exists`
/// path is missing). Returns that.
fn prepare(entry: &Arc<Mutex<Entry>>, recipe: &Recipe, at: &RunIn) -> Result<bool, Halt> {
    copy_env_from_source(recipe, &at.workdir, &at.checkout)?;
    let Some(install) = &recipe.install else {
        return Ok(false);
    };
    let due = install
        .unless_exists
        .as_ref()
        .map(|p| std::fs::symlink_metadata(at.workdir.join(p)).is_err())
        .unwrap_or(true);
    if due {
        advance(entry, Status::Installing)?;
    }
    Ok(due)
}

/// Untracked files the dev server needs, from the user's own checkout (the
/// one the recipe came from); never overwritten.
fn copy_env_from_source(recipe: &Recipe, workdir: &Path, checkout: &Path) -> Result<(), String> {
    for f in &recipe.env_from_source {
        let (from, to) = (checkout.join(f), workdir.join(f));
        if from.is_file() && std::fs::symlink_metadata(&to).is_err() {
            check_unlinked_inside(workdir, f)?;
            if let Err(e) = std::fs::copy(&from, &to) {
                return Err(format!("copying {f} from {}: {e}", checkout.display()));
            }
        }
    }
    Ok(())
}

/// Copying to `rel` inside `workdir` writes inside it: no component of the
/// path (a folder a run may have replaced with a link) is a symlink, and the
/// parent, resolved, is still inside the work copy.
fn check_unlinked_inside(workdir: &Path, rel: &str) -> Result<(), String> {
    let mut at = workdir.to_path_buf();
    let parts: Vec<_> = Path::new(rel).components().collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        at.push(part);
        if std::fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!(
                "env_from_source: {} in the workspace is a symlink; refusing to copy {rel} through it",
                at.display()
            ));
        }
    }
    let parent = workdir
        .join(rel)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let inside = match (parent.canonicalize(), workdir.canonicalize()) {
        (Ok(p), Ok(w)) => p.starts_with(w),
        // A folder that isn't there yet: the copy fails on its own.
        (Err(_), _) => true,
        (Ok(_), Err(_)) => false,
    };
    if inside {
        Ok(())
    } else {
        Err(format!(
            "env_from_source: {rel} would land outside the workspace; refusing"
        ))
    }
}

/// The preview's container spec.
fn spec_for(
    recipe: &Recipe,
    at: &RunIn,
    install_due: bool,
    (image, user): (String, Option<String>),
    host_labels: Vec<String>,
) -> ContainerSpec {
    let mut labels = host_labels;
    labels.push(format!("{WORKSPACE_LABEL}={}", at.name));
    ContainerSpec {
        // The app port makes it unique: a restarted preview never collides
        // with its predecessor while that one is still being removed.
        name: format!("{PREVIEW_CONTAINER_PREFIX}{}-{}", at.name, at.port),
        image,
        workspace: at.workdir.clone(),
        ports: [at.port, at.hmr_port],
        user,
        labels,
        env: container_env(recipe, at.port, at.hmr_port),
        script: container_script(recipe, install_due),
    }
}

/// Starts the container, waits for the dev server to answer, then waits
/// for it to end. It only ever returns a halt: a dev server is never
/// expected to exit, so an exit after ready is a failure unless it was
/// stopped.
fn serve(
    entry: &Arc<Mutex<Entry>>,
    recipe: &Recipe,
    at: &RunIn,
    install_due: bool,
    containers: &dyn PreviewContainers,
) -> Result<(), Halt> {
    let resolved = containers
        .image_and_user()
        .map_err(|e| format!("a preview runs in a container: {e}"))?;
    let spec = spec_for(recipe, at, install_due, resolved, containers.host_labels());
    let mut child = containers.run(&spec, &at.log)?;
    adopt(entry, &mut child, &spec.name, containers)?;
    wait_ready(entry, &mut child, &spec.name, containers, at, recipe)?;
    // Ready: wait for it to end, and say why unless it was stopped.
    let status = child.wait();
    Err(Halt::Failed(exit_reason(recipe, &at.log, status)))
}

/// Why the container ended, from what it printed: Docker never started it,
/// the install step failed, or the dev server exited.
fn exit_reason(
    recipe: &Recipe,
    log: &Path,
    status: std::io::Result<std::process::ExitStatus>,
) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let status = status.map(|s| s.to_string()).unwrap_or_default();
    if !text.lines().any(|l| l == STARTED_LINE) {
        let said = text
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim();
        return format!("the preview container did not start ({status}): {said}");
    }
    let install_code = text
        .lines()
        .find_map(|l| l.strip_prefix(INSTALL_FAILED_LINE))
        .map(str::trim);
    match (install_code, &recipe.install) {
        (Some(code), Some(install)) => {
            format!(
                "install ({}) exited with exit status: {code}",
                install.command
            )
        }
        _ => format!("the dev server exited ({status})"),
    }
}

/// Records the container so `stop` can reach it; if the preview was
/// stopped while it started, removes it instead.
fn adopt(
    entry: &Arc<Mutex<Entry>>,
    child: &mut std::process::Child,
    name: &str,
    containers: &dyn PreviewContainers,
) -> Result<(), Halt> {
    let mut e = entry.lock().unwrap();
    if e.status == Status::Stopped {
        drop(e);
        containers.remove(name);
        let _ = child.wait();
        return Err(Halt::Stopped);
    }
    e.container = Some(name.to_string());
    Ok(())
}

/// The path the readiness probe asks for: the recipe's `ready.http`, else `/`.
fn ready_path(recipe: &Recipe) -> String {
    recipe
        .ready
        .as_ref()
        .and_then(|r| r.http.clone())
        .unwrap_or_else(|| "/".to_string())
}

/// How long a started dev server may take to answer.
const READY_WITHIN: Duration = Duration::from_secs(180);

/// Polls until the dev server answers on its published port; halts if the
/// container exits, the preview is stopped, or the server stays silent for
/// [`READY_WITHIN`] after the install step. The install step itself is not
/// bounded (a first `npm ci` can be slow).
fn wait_ready(
    entry: &Arc<Mutex<Entry>>,
    child: &mut std::process::Child,
    name: &str,
    containers: &dyn PreviewContainers,
    at: &RunIn,
    recipe: &Recipe,
) -> Result<(), Halt> {
    let path = ready_path(recipe);
    let mut serving_since = None;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(Halt::Failed(exit_reason(recipe, &at.log, Ok(status))));
        }
        if entry.lock().unwrap().status == Status::Stopped {
            let _ = child.wait();
            return Err(Halt::Stopped);
        }
        if serving_since.is_none() && log_has(&at.log, SERVING_LINE) {
            serving_since = Some(Instant::now());
            advance(entry, Status::Starting)?;
        }
        if serving_since.is_some() && http_ok(at.port, &path) {
            set(entry, Status::Ready);
            return Ok(());
        }
        if serving_since.is_some_and(|t| t.elapsed() > READY_WITHIN) {
            containers.remove(name);
            return Err(Halt::Failed(format!(
                "the dev server didn't answer on port {} within 3 minutes; in its container it \
                 must listen on 0.0.0.0 (the port is published on 127.0.0.1 only)",
                at.port
            )));
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Whether `log` holds the line `line`.
fn log_has(log: &Path, line: &str) -> bool {
    std::fs::read_to_string(log).is_ok_and(|t| t.lines().any(|l| l == line))
}

/// TEST ONLY: a [`PreviewContainers`] that plays a container's part
/// without Docker, so the preview lifecycle (ports, readiness, the install
/// step, stop, the HTTP routes) is testable on any machine. It runs the
/// spec's script with `sh` in the workspace, with exactly the spec's
/// environment plus `PATH` (an image has its own), in its own process
/// group under a watchdog that ends it with the test process. Production
/// never runs a preview on the host: [`Docker`] is the only production
/// implementation. The shape a real container gets is asserted on
/// [`docker_run_args`] directly.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct HostEmulator {
    /// Every spec it was asked to run.
    pub specs: Mutex<Vec<ContainerSpec>>,
    /// Every container it was asked to remove.
    pub removed: Mutex<Vec<String>>,
    /// Set: resolving the image fails with this.
    pub image_error: Mutex<Option<String>>,
    groups: Mutex<HashMap<String, i32>>,
}

#[cfg(test)]
impl PreviewContainers for HostEmulator {
    fn image_and_user(&self) -> Result<(String, Option<String>), String> {
        match self.image_error.lock().unwrap().clone() {
            Some(e) => Err(e),
            None => Ok(("emulated-runtime-image".to_string(), None)),
        }
    }

    fn host_labels(&self) -> Vec<String> {
        vec![format!("darkmux.dispatch.host-pid={}", std::process::id())]
    }

    fn run(&self, spec: &ContainerSpec, log: &Path) -> Result<std::process::Child, String> {
        use std::os::unix::process::CommandExt;
        self.specs.lock().unwrap().push(spec.clone());
        let out = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(log)
            .map_err(|e| e.to_string())?;
        let err = out.try_clone().map_err(|e| e.to_string())?;
        let watchdog = format!(
            "trap 'kill 0' EXIT; ( {} ) & c=$!; while kill -0 {} 2>/dev/null && kill -0 $c 2>/dev/null; do sleep 1; done; \
             if kill -0 $c 2>/dev/null; then exit 0; fi; wait $c",
            spec.script,
            std::process::id()
        );
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.env_clear();
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
        cmd.envs(spec.env.iter().map(|(k, v)| (k, v)))
            .arg("-c")
            .arg(watchdog)
            .current_dir(&spec.workspace)
            .stdin(std::process::Stdio::null())
            .stdout(out)
            .stderr(err)
            .process_group(0);
        let child = cmd.spawn().map_err(|e| e.to_string())?;
        self.groups
            .lock()
            .unwrap()
            .insert(spec.name.clone(), child.id() as i32);
        Ok(child)
    }

    fn remove(&self, name: &str) {
        self.removed.lock().unwrap().push(name.to_string());
        let Some(pgid) = self.groups.lock().unwrap().remove(name) else {
            return;
        };
        let alive = || unsafe { libc::kill(-pgid, 0) == 0 };
        unsafe { libc::kill(-pgid, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        if alive() {
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
        }
    }

    fn reap_orphans(&self) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
impl HostEmulator {
    /// Whether the emulated container `name` still has a live process.
    pub fn running(&self, name: &str) -> bool {
        self.groups
            .lock()
            .unwrap()
            .get(name)
            .is_some_and(|g| unsafe { libc::kill(-g, 0) == 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_relative_paths() {
        for good in [".env", "node_modules", "config/.env.local"] {
            assert!(plain_relative(good), "{good}");
        }
        for bad in ["", "/etc/passwd", "../.env", "a/../../b", "./x"] {
            assert!(!plain_relative(bad), "{bad:?}");
        }
    }

    #[test]
    fn ports_are_substituted() {
        assert_eq!(substitute("{port}:{hmrPort}", 4100, 4200), "4100:4200");
        assert_eq!(substitute("127.0.0.1", 1, 2), "127.0.0.1");
    }

    /// Waits for the preview to fail and returns its state.
    fn until_failed(previews: &Previews, name: &str) -> serde_json::Value {
        let mut v = serde_json::Value::Null;
        for _ in 0..200 {
            v = serde_json::to_value(previews.status(name).unwrap()).unwrap();
            if v["status"] == "failed" {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        v
    }

    /// Previews on a [`HostEmulator`]; returns both.
    fn emulated(dir: &Path) -> (Previews, Arc<HostEmulator>) {
        let emu = Arc::new(HostEmulator::default());
        (Previews::new(dir.join("p"), emu.clone()), emu)
    }

    fn a_recipe(v: serde_json::Value) -> Recipe {
        parse_recipe(&v.to_string()).unwrap()
    }

    /// A spec as a start builds it, for a workspace at `/tmp/ws/site`.
    fn a_spec(recipe: &Recipe, user: Option<String>) -> ContainerSpec {
        let at = RunIn {
            name: "site".into(),
            workdir: PathBuf::from("/tmp/ws/site"),
            checkout: PathBuf::from("/tmp/mine"),
            log: PathBuf::from("/tmp/p/site.log"),
            port: 4100,
            hmr_port: 4200,
        };
        let labels = vec!["darkmux.dispatch.host-pid=7".to_string()];
        spec_for(recipe, &at, true, ("sha256:runtime".into(), user), labels)
    }

    /// The values `docker run` is given for `flag` (`-v`, `-p`, `-e`, ...).
    fn values_of(args: &[String], flag: &str) -> Vec<String> {
        args.windows(2)
            .filter(|w| w[0] == flag)
            .map(|w| w[1].clone())
            .collect()
    }

    /// (#3456) A preview container sees the workspace and nothing else of
    /// the host, publishes only its two ports and only on 127.0.0.1, runs
    /// with the dispatch container's hardening, and carries the labels the
    /// orphan reaper reads.
    #[test]
    fn a_preview_container_mounts_only_the_workspace_and_publishes_only_its_ports_on_loopback() {
        let recipe =
            a_recipe(serde_json::json!({"schema_version": "1.0", "command": "npm run dev"}));
        let args = docker_run_args(&a_spec(&recipe, None));
        assert_eq!(
            args[..5],
            [
                "run",
                "--rm",
                "--init",
                "--name",
                "darkmux-preview-site-4100"
            ]
        );
        assert_eq!(
            values_of(&args, "-v"),
            ["/tmp/ws/site:/workspace"],
            "the only mount"
        );
        assert!(
            !args
                .iter()
                .any(|a| a.starts_with("--mount") || a.starts_with("--volume")),
            "{args:?}"
        );
        assert_eq!(
            values_of(&args, "-p"),
            ["127.0.0.1:4100:4100", "127.0.0.1:4200:4200"]
        );
        assert!(
            !args.iter().any(|a| a == "-P" || a.starts_with("--publish")),
            "{args:?}"
        );
        for flag in [
            "--network",
            "--privileged",
            "--pid",
            "--ipc",
            "--cap-add",
            "--device",
        ] {
            let named = |a: &String| a == flag || a.starts_with(&format!("{flag}="));
            assert!(!args.iter().any(named), "{flag} in {args:?}");
        }
        for want in [
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--pids-limit=512",
            "--memory=4g",
        ] {
            assert!(args.contains(&want.to_string()), "{want} missing: {args:?}");
        }
        assert_eq!(
            values_of(&args, "--label"),
            [
                "darkmux.dispatch.host-pid=7",
                "darkmux.preview.workspace=site"
            ]
        );
        assert_eq!(values_of(&args, "-w"), ["/workspace"]);
        let image_at = args.iter().position(|a| a == "sha256:runtime").unwrap();
        assert_eq!(args[image_at - 2..image_at], ["--entrypoint", "sh"]);
        assert_eq!(args[image_at + 1], "-c");
        assert_eq!(args.len(), image_at + 3, "the script is the last argument");
    }

    /// (#3456) No host environment reaches the container: its environment
    /// is polling for file watchers, the recipe's `env` with its ports
    /// filled in, and the port variables, each as `KEY=value` (a bare `-e
    /// KEY` would copy the host's value). A key exported where `client
    /// serve` runs appears nowhere in the arguments.
    #[test]
    #[serial_test::serial]
    fn a_preview_container_gets_no_host_environment() {
        unsafe { std::env::set_var("SOME_PROVIDER_API_KEY", "must-not-leak") };
        let recipe = a_recipe(serde_json::json!({
            "schema_version": "1.0", "command": "npm run dev",
            "env": {"PORT": "{port}", "VITE_HMR_PORT": "{hmrPort}", "CHOKIDAR_USEPOLLING": "false"}
        }));
        let args = docker_run_args(&a_spec(&recipe, None));
        unsafe { std::env::remove_var("SOME_PROVIDER_API_KEY") };
        let joined = args.join(" ");
        assert!(
            !joined.contains("must-not-leak") && !joined.contains("SOME_PROVIDER_API_KEY"),
            "{joined}"
        );
        assert!(
            !args.iter().any(|a| a.starts_with("--env-file")),
            "{joined}"
        );
        assert_eq!(
            values_of(&args, "-e"),
            [
                "CHOKIDAR_USEPOLLING=true",
                "WATCHPACK_POLLING=true",
                "CHOKIDAR_USEPOLLING=false",
                "PORT=4100",
                "VITE_HMR_PORT=4200",
                "DARKMUX_PREVIEW_PORT=4100",
                "DARKMUX_PREVIEW_HMR_PORT=4200",
            ],
            "polling first (the recipe may turn it off), the ports last"
        );
        // With a container user, its home is one it owns.
        let args = docker_run_args(&a_spec(&recipe, Some("501:20".into())));
        assert!(
            args.join(" ").contains("--user=501:20 -e HOME=/tmp"),
            "{args:?}"
        );
    }

    /// (#3456) The container runs the install step only when it is due, in
    /// a subshell so its own `exit` ends only it, and an install failure
    /// stops before the dev server, saying its exit code.
    #[test]
    fn the_container_script_installs_only_when_due() {
        let recipe = a_recipe(serde_json::json!({
            "schema_version": "1.0", "command": "echo serving",
            "install": {"command": "echo installing; exit 4", "unless_exists": "node_modules"}
        }));
        let not_due = container_script(&recipe, false);
        assert!(!not_due.contains("installing"), "{not_due}");
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(container_script(&recipe, true))
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert_eq!(out.status.code(), Some(4), "{text}");
        assert!(text.starts_with(STARTED_LINE), "{text}");
        assert!(
            text.contains(&format!("{INSTALL_FAILED_LINE} 4")) && !text.contains("serving"),
            "{text}"
        );
    }

    /// (#3456) Why a container ended, from what it printed.
    #[test]
    fn an_ended_container_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("l.log");
        let recipe = a_recipe(serde_json::json!({
            "schema_version": "1.0", "command": "x", "install": {"command": "npm ci"}
        }));
        let why = |text: &str| {
            std::fs::write(&log, text).unwrap();
            exit_reason(&recipe, &log, Err(std::io::Error::other("gone")))
        };
        let never = why("docker: Error response from daemon: No such image\n");
        assert!(
            never.contains("did not start") && never.contains("No such image"),
            "{never}"
        );
        assert_eq!(
            why(&format!(
                "{STARTED_LINE}\nnpm ERR!\n{INSTALL_FAILED_LINE} 1\n"
            )),
            "install (npm ci) exited with exit status: 1"
        );
        assert!(why(&format!("{STARTED_LINE}\n{SERVING_LINE}\nboom\n"))
            .starts_with("the dev server exited"));
    }

    /// (#3456) A machine that cannot run the preview's container (no
    /// Docker, no image) fails the preview saying so, and runs nothing.
    #[test]
    fn a_preview_that_cannot_get_its_container_fails_saying_so() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().join("w");
        std::fs::create_dir_all(wt.join(".darkmux")).unwrap();
        let recipe = serde_json::json!({"schema_version": "1.0", "command": "touch ran"});
        std::fs::write(wt.join(".darkmux/preview.json"), recipe.to_string()).unwrap();
        let (previews, emu) = emulated(dir.path());
        *emu.image_error.lock().unwrap() = Some("darkmux's runtime requires Docker".into());
        previews.start("w", &wt, &wt).unwrap();
        let v = until_failed(&previews, "w");
        assert_eq!(v["status"], "failed", "{v}");
        assert!(
            v["error"]
                .as_str()
                .unwrap()
                .contains("a preview runs in a container: darkmux's runtime requires Docker"),
            "{v}"
        );
        assert!(emu.specs.lock().unwrap().is_empty() && !wt.join("ran").exists());
    }

    /// (#3456) Stopping a preview, and `client serve` shutting down, remove
    /// its container; the shutdown removes every one.
    #[test]
    fn stop_and_shutdown_remove_the_container() {
        let dir = tempfile::tempdir().unwrap();
        let (_t, clock) = frozen_clock();
        let (previews, emu, name) = a_live_preview(dir.path(), clock);
        assert!(previews.stop("w"));
        assert_eq!(*emu.removed.lock().unwrap(), std::slice::from_ref(&name));
        assert!(!emu.running(&name));
        let (previews, emu, name) = a_live_preview(&dir.path().join("2"), frozen_clock().1);
        assert_eq!(previews.stop_all(), ["w"]);
        assert_eq!(*emu.removed.lock().unwrap(), std::slice::from_ref(&name));
        assert!(!emu.running(&name));
    }

    /// (review) `env_from_source` never writes through a link a run
    /// planted: a workspace whose `config` is a symlink out of it gets no
    /// `config/.env`, there or anywhere, and the preview says why.
    #[cfg(unix)]
    #[test]
    fn env_from_source_never_writes_through_a_planted_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let (wt, checkout, outside) = (
            dir.path().join("w"),
            dir.path().join("mine"),
            dir.path().join("outside"),
        );
        for d in [&wt, &checkout.join("config"), &outside] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(checkout.join("config/.env"), "SECRET=1\n").unwrap();
        std::os::unix::fs::symlink(&outside, wt.join("config")).unwrap();
        let recipe = serde_json::json!({
            "schema_version": "1.0", "command": "touch server-ran; exit 3", "env_from_source": ["config/.env"]
        });
        std::fs::create_dir_all(checkout.join(".darkmux")).unwrap();
        std::fs::write(checkout.join(".darkmux/preview.json"), recipe.to_string()).unwrap();
        let (previews, _emu) = emulated(dir.path());
        previews.start("w", &wt, &checkout).unwrap();
        let v = until_failed(&previews, "w");
        assert_eq!(v["status"], "failed", "{v}");
        assert!(v["error"].as_str().unwrap().contains("symlink"), "{v}");
        assert!(
            !outside.join(".env").exists(),
            "wrote through the planted link"
        );
        assert!(!wt.join("server-ran").exists(), "the server never ran");
    }

    #[test]
    fn a_failed_install_says_so_and_never_starts_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().join("w");
        std::fs::create_dir_all(wt.join(".darkmux")).unwrap();
        let recipe = serde_json::json!({
            "schema_version": "1.0",
            "command": "touch server-ran",
            "install": {"command": "exit 4", "unless_exists": "deps"}
        });
        std::fs::write(wt.join(".darkmux/preview.json"), recipe.to_string()).unwrap();
        let (previews, _emu) = emulated(dir.path());
        previews.start("w", &wt, &wt).unwrap();
        let mut v = serde_json::Value::Null;
        for _ in 0..200 {
            v = serde_json::to_value(previews.status("w").unwrap()).unwrap();
            if v["status"] == "failed" {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(v["status"], "failed", "{v}");
        assert!(
            v["error"]
                .as_str()
                .unwrap()
                .starts_with("install (exit 4) exited with"),
            "{v}"
        );
        assert!(!wt.join("server-ran").exists(), "the server never ran");
    }

    /// A frozen clock a test moves by hand, in seconds.
    fn frozen_clock() -> (Arc<std::sync::atomic::AtomicU64>, Clock) {
        let t = Arc::new(std::sync::atomic::AtomicU64::new(1_000_000));
        let read = t.clone();
        let clock: Clock = Arc::new(move || read.load(std::sync::atomic::Ordering::SeqCst));
        (t, clock)
    }

    fn advance_minutes(t: &std::sync::atomic::AtomicU64, minutes: u64) {
        t.fetch_add(minutes * 60, std::sync::atomic::Ordering::SeqCst);
    }

    /// A preview whose server never answers (so it stays `starting`, live)
    /// on a frozen clock and a [`HostEmulator`]; returns the previews, the
    /// emulator and the container's name once it runs.
    fn a_live_preview(dir: &Path, clock: Clock) -> (Previews, Arc<HostEmulator>, String) {
        let wt = dir.join("w");
        std::fs::create_dir_all(wt.join(".darkmux")).unwrap();
        let recipe = serde_json::json!({"schema_version": "1.0", "command": "sleep 30"});
        std::fs::write(wt.join(".darkmux/preview.json"), recipe.to_string()).unwrap();
        let (previews, emu) = emulated(dir);
        let previews = previews.with_clock(clock);
        previews.start("w", &wt, &wt).unwrap();
        for _ in 0..200 {
            let container = previews.entries.lock().unwrap()["w"]
                .lock()
                .unwrap()
                .container
                .clone();
            if let Some(name) = container {
                return (previews, emu, name);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the preview's container never started");
    }

    /// (#3459) A preview nobody has asked about for `idle_minutes` is
    /// stopped, and its container removed; asking (a status) restarts
    /// the count.
    #[cfg(unix)]
    #[test]
    fn an_idle_preview_is_stopped_once_nobody_asked_for_idle_minutes() {
        let dir = tempfile::tempdir().unwrap();
        let (t, clock) = frozen_clock();
        let (previews, emu, container) = a_live_preview(dir.path(), clock);
        advance_minutes(&t, 14);
        assert!(previews.stop_idle(15).is_empty(), "14 minutes: not idle");
        let asked = previews.status("w").unwrap();
        assert_eq!(asked.status, "starting", "a status is an ask");
        advance_minutes(&t, 1);
        assert!(
            previews.stop_idle(15).is_empty(),
            "15 minutes since the start, but the status restarted the count"
        );
        advance_minutes(&t, 13);
        assert!(previews.stop_idle(15).is_empty(), "14 since the status");
        advance_minutes(&t, 1);
        assert_eq!(previews.stop_idle(15), vec!["w".to_string()], "15 since");
        let entry = previews.entries.lock().unwrap()["w"].clone();
        assert_eq!(entry.lock().unwrap().status, Status::Stopped);
        assert_eq!(
            *emu.removed.lock().unwrap(),
            std::slice::from_ref(&container),
            "its container is removed"
        );
        assert!(!emu.running(&container));
        assert!(previews.stop_idle(15).is_empty(), "stopped once only");
    }

    /// (#3459) `0` means never, however long nobody asks; asking again
    /// for a running preview (a second start) counts as asking.
    #[cfg(unix)]
    #[test]
    fn zero_idle_minutes_never_stops_and_a_repeated_start_is_an_ask() {
        let dir = tempfile::tempdir().unwrap();
        let (t, clock) = frozen_clock();
        let (previews, _emu, _container) = a_live_preview(dir.path(), clock);
        advance_minutes(&t, 60 * 24 * 365);
        assert!(previews.stop_idle(0).is_empty(), "0 is never");
        let wt = dir.path().join("w");
        let (_, running) = previews.start("w", &wt, &wt).unwrap();
        assert!(running, "the same preview, still running");
        advance_minutes(&t, 10);
        assert!(previews.stop_idle(15).is_empty(), "the 2nd start asked");
        advance_minutes(&t, 5);
        assert_eq!(previews.stop_idle(15), vec!["w".to_string()]);
    }

    /// (#3456, opt-in) A real preview in the darkmux runtime image: it
    /// serves the workspace on its published port, its environment holds
    /// nothing `client serve` was started with, it cannot read a host path
    /// beside the workspace, and a stop removes the container. Needs Docker
    /// and this darkmux's version-pinned runtime image already present; it
    /// never pulls. Run it with
    /// `cargo nextest run -p darkmux-serve --run-ignored only a_preview_runs_in_the_runtime_image`.
    #[test]
    #[ignore]
    #[serial_test::serial]
    fn a_preview_runs_in_the_runtime_image_and_reaches_nothing_else_of_the_host() {
        let image = darkmux_crew::runtime_image::pinned_runtime_image(env!("CARGO_PKG_VERSION"));
        let inspect = std::process::Command::new("docker")
            .args(["image", "inspect", &image])
            .output();
        assert!(
            inspect.is_ok_and(|o| o.status.success()),
            "needs {image} present locally; this test never pulls it"
        );
        let dir = tempfile::tempdir().unwrap();
        let (wt, outside) = (dir.path().join("ws"), dir.path().join("outside"));
        std::fs::create_dir_all(wt.join(".darkmux")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(wt.join("index.html"), "hello from the container").unwrap();
        std::fs::write(outside.join("secret.txt"), "host-secret").unwrap();
        let command = format!(
            "env > env.txt; cat '{}' > leak.txt 2>&1; exec python3 -m http.server \"$PORT\" --bind 0.0.0.0",
            outside.join("secret.txt").display()
        );
        let recipe = serde_json::json!({"schema_version": "1.0", "command": command, "env": {"PORT": "{port}"}});
        std::fs::write(wt.join(".darkmux/preview.json"), recipe.to_string()).unwrap();
        unsafe { std::env::set_var("DARKMUX_TEST_HOST_SECRET", "must-not-leak") };
        let previews = Previews::new(
            dir.path().join("p"),
            Arc::new(Docker { image: Some(image) }),
        );
        let (state, _) = previews.start("ws", &wt, &wt).unwrap();
        unsafe { std::env::remove_var("DARKMUX_TEST_HOST_SECRET") };
        let mut v = serde_json::Value::Null;
        for _ in 0..600 {
            v = serde_json::to_value(previews.status("ws").unwrap()).unwrap();
            if v["status"] == "ready" || v["status"] == "failed" {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let container = previews.entries.lock().unwrap()["ws"]
            .lock()
            .unwrap()
            .container
            .clone();
        assert_eq!(
            v["status"],
            "ready",
            "{v} {}",
            tail(&dir.path().join("p/ws.log"), 20)
        );
        let body = {
            use std::io::{Read, Write};
            let mut s = std::net::TcpStream::connect(("127.0.0.1", state.port)).unwrap();
            s.write_all(b"GET /index.html HTTP/1.0\r\n\r\n").unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            out
        };
        assert!(body.contains("hello from the container"), "{body}");
        let env = std::fs::read_to_string(wt.join("env.txt")).unwrap();
        assert!(
            !env.contains("must-not-leak") && !env.contains("DARKMUX_TEST_HOST_SECRET"),
            "{env}"
        );
        assert!(
            env.contains("CHOKIDAR_USEPOLLING=true")
                && env.contains(&format!("PORT={}", state.port)),
            "{env}"
        );
        let leak = std::fs::read_to_string(wt.join("leak.txt")).unwrap();
        assert!(
            !leak.contains("host-secret"),
            "read a host path outside the workspace: {leak}"
        );
        let name = container.expect("the container was adopted");
        assert!(previews.stop("ws"));
        let listed = std::process::Command::new("docker")
            .args([
                "ps",
                "-a",
                "--filter",
                &format!("name=^{name}$"),
                "--format",
                "{{.Names}}",
            ])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&listed.stdout).trim(),
            "",
            "the container is removed"
        );
        assert!(!http_ok(state.port, "/"), "nothing answers any more");
    }

    fn parsed(v: serde_json::Value) -> Result<Recipe, RecipeRefusal> {
        parse_recipe(&v.to_string())
    }

    /// Contract 7: a key the recipe format does not know is refused, named
    /// with the closest valid key, at the top and nested.
    #[test]
    fn a_recipe_refuses_keys_it_does_not_know() {
        let ok = serde_json::json!({"schema_version": "1.0", "command": "npm run dev", "env": {"PORT": "{port}"}});
        assert_eq!(parsed(ok.clone()).unwrap().command, "npm run dev");
        for (doc, key) in [
            (
                serde_json::json!({"schema_version": "1.0", "command": "x", "comand": "y"}),
                "`comand`",
            ),
            (
                serde_json::json!({"schema_version": "1.0", "command": "x", "port": 3333}),
                "`port`",
            ),
            (
                serde_json::json!({"schema_version": "1.0", "command": "x", "ready": {"htp": "/"}}),
                "`ready.htp`",
            ),
            (
                serde_json::json!({"schema_version": "1.0", "command": "x", "install": {"command": "i", "unless": "n"}}),
                "`install.unless`",
            ),
        ] {
            let e = parsed(doc).unwrap_err();
            assert!(matches!(e, RecipeRefusal::Invalid(_)), "{e:?}");
            assert!(e.message().contains(&format!("unknown key {key}")), "{e:?}");
        }
        let e = parsed(serde_json::json!({"schema_version": "1.0", "command": "x", "comand": "y"}))
            .unwrap_err();
        assert!(e.message().contains("did you mean `command`?"), "{e:?}");
    }

    /// Contract 5: `schema_version` is required and must be one this darkmux
    /// reads; a newer one says to upgrade.
    #[test]
    fn a_recipe_needs_a_schema_version_this_darkmux_reads() {
        let missing = parsed(serde_json::json!({"command": "x"})).unwrap_err();
        assert!(
            missing.message().contains("`schema_version`") && missing.message().contains("missing"),
            "{missing:?}"
        );
        let unknown =
            parsed(serde_json::json!({"schema_version": "0.9", "command": "x"})).unwrap_err();
        assert!(
            unknown.message().contains("`schema_version` \"0.9\""),
            "{unknown:?}"
        );
        let newer =
            parsed(serde_json::json!({"schema_version": "2.0", "command": "x"})).unwrap_err();
        assert!(newer.message().contains("newer darkmux"), "{newer:?}");
        let typed = parsed(serde_json::json!({"schema_version": 1, "command": "x"})).unwrap_err();
        assert!(typed.message().contains("`schema_version`"), "{typed:?}");
    }

    /// (#3456) Each `env` key reaches the container as `-e KEY=value`, so
    /// it must be a plain variable name: one with `=` would set another
    /// variable, and an empty one is no variable.
    #[test]
    fn a_recipe_env_key_is_a_variable_name() {
        for bad in ["", "A=B", "1A", "A B", "A-B"] {
            let e = parsed(
                serde_json::json!({"schema_version": "1.0", "command": "x", "env": {bad: "v"}}),
            )
            .unwrap_err();
            assert!(matches!(e, RecipeRefusal::Invalid(_)), "{bad:?}: {e:?}");
            assert!(
                e.message().contains("env:") && e.message().contains("variable name"),
                "{bad:?}: {e:?}"
            );
        }
        for good in ["PORT", "_X", "VITE_HMR_PORT", "a1"] {
            assert!(
                parsed(
                    serde_json::json!({"schema_version": "1.0", "command": "x", "env": {good: "v"}})
                )
                .is_ok(),
                "{good}"
            );
        }
    }

    #[test]
    fn a_recipe_is_a_json_object_with_paths_inside_the_work_copy() {
        assert!(matches!(
            parse_recipe("[1]"),
            Err(RecipeRefusal::Invalid(_))
        ));
        assert!(matches!(
            parse_recipe("{nope"),
            Err(RecipeRefusal::Invalid(_))
        ));
        let e = parsed(serde_json::json!({"schema_version": "1.0", "command": "x", "env_from_source": ["../.env"]})).unwrap_err();
        assert!(e.message().contains("env_from_source"), "{e:?}");
        let e = parsed(serde_json::json!({"schema_version": "1.0", "command": "x", "install": {"command": "i", "unless_exists": "/etc"}})).unwrap_err();
        assert!(e.message().contains("install.unless_exists"), "{e:?}");
    }
}
