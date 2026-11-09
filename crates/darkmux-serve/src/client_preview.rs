//! Live previews for paired apps: darkmux runs a workspace's dev server from
//! a `.darkmux/preview.json` recipe and tells the app its URL.
//!
//! **Off by default.** Even with the recipe read only from the user's own
//! checkout, a preview runs the WORKSPACE's code: its `package.json`
//! scripts, its dev server and whatever they load, which a model may have
//! written. That code runs on this machine as the user, outside the
//! dispatch sandbox. So a paired app can start a preview only when this
//! machine says so (`darkmux config set clients.preview.enabled true`). A
//! preview in its own sandboxed container is planned as separate work.
//! Known gap: a file a running preview writes in the workspace while a run
//! is working there (a build output, a cache the repo does not ignore) is
//! indistinguishable from the run's own change and can be committed as the
//! run's.
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
//! `schema_version` is required):
//!
//! ```json
//! {
//!   "schema_version": "1.0",
//!   "command": "npm run dev",
//!   "env": {"PORT": "{port}", "HOST": "127.0.0.1", "VITE_HMR_PORT": "{hmrPort}"},
//!   "ready": {"http": "/"},
//!   "install": {"command": "npm ci", "unless_exists": "node_modules"},
//!   "env_from_source": [".env"]
//! }
//! ```
//!
//! Each preview gets its own free app port and HMR port, substituted into
//! `env` (`{port}`, `{hmrPort}`), so two previews never collide. The command
//! runs in the workspace. `install` runs first when its `unless_exists` path
//! is missing there. `env_from_source` copies untracked files (like `.env`)
//! from that same checkout into the workspace, never overwriting. The dev
//! server runs in its own process group with a watchdog: it stops when
//! darkmux stops, when it is stopped, or when it exits on its own. It and
//! its install step get a stripped environment ([`PREVIEW_ENV_KEPT`] plus
//! the recipe's `env` and the ports), so nothing else `client serve` was
//! started with (an exported API key) reaches the workspace's code.

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
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(RecipeRefusal::NotFound(format!(
            "no {RECIPE_PATH} in {}",
            checkout.display()
        ))),
        Err(e) => Err(RecipeRefusal::Unreadable(format!("reading {}: {e}", p.display()))),
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
            darkmux_types::data_version::newer_refusal("preview recipe", &v, PREVIEW_RECIPE_SCHEMA_VERSION)
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
    let unless = recipe.install.as_ref().and_then(|i| i.unless_exists.as_ref());
    let named = recipe
        .env_from_source
        .iter()
        .map(|f| ("env_from_source", f))
        .chain(unless.map(|u| ("install.unless_exists", u)));
    for (key, p) in named {
        if !plain_relative(p) {
            return Err(RecipeRefusal::Invalid(format!("{key}: {p:?} must be a plain relative path")));
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
        return Err(RecipeRefusal::Invalid(format!("{RECIPE_PATH} is not a JSON object")));
    }
    check_version(&doc)?;
    let issues = darkmux_types::user_files::key_issues::<Recipe>(&doc, &darkmux_types::user_files::no_retired);
    if !issues.is_empty() {
        let lines: Vec<String> = issues.iter().map(ToString::to_string).collect();
        return Err(RecipeRefusal::Invalid(format!("{RECIPE_PATH}: {}", lines.join("; "))));
    }
    let recipe: Recipe =
        serde_json::from_value(doc).map_err(|e| RecipeRefusal::Invalid(format!("{RECIPE_PATH}: {e}")))?;
    check_recipe_paths(&recipe)?;
    Ok(recipe)
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
    pgid: Option<i32>,
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
    /// The process whose exit stops every preview (darkmux itself).
    watch_pid: u32,
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

#[cfg(unix)]
fn kill_group(pgid: i32, sig: i32) {
    unsafe {
        libc::kill(-pgid, sig);
    }
}

#[cfg(not(unix))]
fn kill_group(_pgid: i32, _sig: i32) {}

fn group_alive(pgid: i32) -> bool {
    #[cfg(unix)]
    unsafe {
        libc::kill(-pgid, 0) == 0
    }
    #[cfg(not(unix))]
    {
        let _ = pgid;
        false
    }
}

impl Previews {
    pub fn new(dir: PathBuf, watch_pid: u32) -> Self {
        Self {
            dir,
            watch_pid,
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
        Self::new(dir, std::process::id())
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
    /// from: the recipe is read there, never in `workdir`, where the dev
    /// server runs. The boolean says whether it was already running.
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
            pgid: None,
            log: log.clone(),
            last_asked: (self.clock)(),
        }));
        let snap = Self::snapshot(name, &entry.lock().unwrap());
        entries.insert(name.to_string(), entry.clone());
        drop(entries);

        let at = RunIn {
            workdir: workdir.to_path_buf(),
            checkout: checkout.to_path_buf(),
            log,
            port,
            hmr_port,
            watch: self.watch_pid,
        };
        std::thread::spawn(move || run(entry, recipe, at));
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

    /// Stops it: TERM to its process group, KILL after five seconds.
    pub fn stop(&self, name: &str) -> bool {
        let entry = self.entries.lock().unwrap().get(name).cloned();
        let Some(entry) = entry else { return false };
        let pgid = {
            let mut e = entry.lock().unwrap();
            e.status = Status::Stopped;
            e.pgid.take()
        };
        if let Some(pgid) = pgid {
            end_group(pgid);
        }
        true
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
        let mut ended = Vec::new();
        for (name, entry) in self.entries.lock().unwrap().iter() {
            let mut e = entry.lock().unwrap();
            if e.status.live() && now.saturating_sub(e.last_asked) >= idle_after {
                e.status = Status::Stopped;
                ended.push((name.clone(), e.pgid.take()));
            }
        }
        for (_, pgid) in &ended {
            if let Some(pgid) = pgid {
                end_group(*pgid);
            }
        }
        let mut names: Vec<String> = ended.into_iter().map(|(n, _)| n).collect();
        names.sort();
        names
    }
}

/// TERM to a process group, KILL after five seconds.
fn end_group(pgid: i32) {
    #[cfg(unix)]
    kill_group(pgid, libc::SIGTERM);
    let deadline = Instant::now() + Duration::from_secs(5);
    while group_alive(pgid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    #[cfg(unix)]
    if group_alive(pgid) {
        kill_group(pgid, libc::SIGKILL);
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

/// The only variables of `client serve`'s own environment a preview's
/// processes get: what a dev server needs to find its tools and a home.
/// Everything else (an API key exported where `client serve` started, a
/// token) stays out of code the workspace holds, which a run may have
/// written. The recipe's own `env` and the port variables are added on top.
pub const PREVIEW_ENV_KEPT: &[&str] = &["PATH", "HOME", "USER", "LANG", "TMPDIR", "TERM"];

fn sh(command: &str, workdir: &Path, log: &Path) -> std::process::Command {
    let out = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(log);
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.env_clear();
    for key in PREVIEW_ENV_KEPT {
        if let Some(v) = std::env::var_os(key) {
            cmd.env(key, v);
        }
    }
    cmd.arg("-c")
        .arg(command)
        .current_dir(workdir)
        .stdin(std::process::Stdio::null());
    if let Ok(f) = out {
        if let Ok(f2) = f.try_clone() {
            cmd.stdout(f).stderr(f2);
        }
    }
    cmd
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

/// Where one preview runs: its work copy, the user's own checkout its recipe
/// and env files come from, its log, its ports, and the process whose exit
/// ends it.
struct RunIn {
    workdir: PathBuf,
    checkout: PathBuf,
    log: PathBuf,
    port: u16,
    hmr_port: u16,
    watch: u32,
}

fn run(entry: Arc<Mutex<Entry>>, recipe: Recipe, at: RunIn) {
    let RunIn {
        workdir,
        checkout,
        log,
        port,
        hmr_port,
        watch,
    } = at;
    let ended = prepare(&entry, &recipe, &workdir, &checkout, &log)
        .and_then(|()| serve(&entry, &recipe, &workdir, &log, port, hmr_port, watch));
    // A stop is already recorded; `set` never overwrites it with a failure.
    if let Err(Halt::Failed(why)) = ended {
        set(&entry, Status::Failed(why));
    }
}

/// Everything before the dev server: env files, then the install step.
fn prepare(
    entry: &Arc<Mutex<Entry>>,
    recipe: &Recipe,
    workdir: &Path,
    checkout: &Path,
    log: &Path,
) -> Result<(), Halt> {
    copy_env_from_source(recipe, workdir, checkout)?;
    if let Some(install) = &recipe.install {
        install_deps(entry, install, workdir, log)?;
    }
    Ok(())
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
    let parent = workdir.join(rel).parent().map(Path::to_path_buf).unwrap_or_default();
    let inside = match (parent.canonicalize(), workdir.canonicalize()) {
        (Ok(p), Ok(w)) => p.starts_with(w),
        // A folder that isn't there yet: the copy fails on its own.
        (Err(_), _) => true,
        (Ok(_), Err(_)) => false,
    };
    if inside {
        Ok(())
    } else {
        Err(format!("env_from_source: {rel} would land outside the workspace; refusing"))
    }
}

/// Runs the recipe's install command unless its `unless_exists` path is
/// already there.
fn install_deps(
    entry: &Arc<Mutex<Entry>>,
    install: &Install,
    workdir: &Path,
    log: &Path,
) -> Result<(), Halt> {
    let needed = install
        .unless_exists
        .as_ref()
        .map(|p| std::fs::symlink_metadata(workdir.join(p)).is_err())
        .unwrap_or(true);
    if !needed {
        return Ok(());
    }
    advance(entry, Status::Installing)?;
    match sh(&install.command, workdir, log).status() {
        Ok(s) if s.success() => {}
        Ok(s) => return Err(format!("install ({}) exited with {s}", install.command).into()),
        Err(e) => return Err(format!("install: {e}").into()),
    }
    advance(entry, Status::Starting)
}

/// Starts the dev server, waits for it to answer, then waits for it to end.
/// It only ever returns a halt: a dev server is never expected to exit, so
/// an exit after ready is a failure unless it was stopped.
fn serve(
    entry: &Arc<Mutex<Entry>>,
    recipe: &Recipe,
    workdir: &Path,
    log: &Path,
    port: u16,
    hmr_port: u16,
    watch: u32,
) -> Result<(), Halt> {
    let mut child = spawn_server(recipe, workdir, log, port, hmr_port, watch)?;
    let pgid = adopt(entry, &mut child)?;
    wait_ready(entry, &mut child, pgid, port, &ready_path(recipe))?;
    // Ready: wait for it to end, and say so unless it was stopped.
    let status = child.wait();
    Err(Halt::Failed(format!(
        "the dev server exited ({})",
        status.map(|s| s.to_string()).unwrap_or_default()
    )))
}

/// The dev server in its own process group, under a watchdog: the group
/// ends when the server exits, or when darkmux (`watch`) is gone.
fn spawn_server(
    recipe: &Recipe,
    workdir: &Path,
    log: &Path,
    port: u16,
    hmr_port: u16,
    watch: u32,
) -> Result<std::process::Child, String> {
    let wrapper = format!(
        "trap 'kill 0' EXIT; ( {} ) & server=$!; while kill -0 {watch} 2>/dev/null && kill -0 $server 2>/dev/null; do sleep 1; done; if kill -0 $server 2>/dev/null; then exit 0; fi; wait $server",
        recipe.command
    );
    let mut cmd = sh(&wrapper, workdir, log);
    for (k, v) in &recipe.env {
        cmd.env(k, substitute(v, port, hmr_port));
    }
    cmd.env("DARKMUX_PREVIEW_PORT", port.to_string())
        .env("DARKMUX_PREVIEW_HMR_PORT", hmr_port.to_string());
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
        .map_err(|e| format!("starting {}: {e}", recipe.command))
}

/// Records the server's process group so `stop` can reach it; if the
/// preview was stopped while it spawned, ends it instead.
fn adopt(entry: &Arc<Mutex<Entry>>, child: &mut std::process::Child) -> Result<i32, Halt> {
    let pgid = child.id() as i32;
    let mut e = entry.lock().unwrap();
    if e.status == Status::Stopped {
        drop(e);
        kill_group(pgid, 15);
        let _ = child.wait();
        return Err(Halt::Stopped);
    }
    e.pgid = Some(pgid);
    Ok(pgid)
}

/// The path the readiness probe asks for: the recipe's `ready.http`, else `/`.
fn ready_path(recipe: &Recipe) -> String {
    recipe
        .ready
        .as_ref()
        .and_then(|r| r.http.clone())
        .unwrap_or_else(|| "/".to_string())
}

/// Polls until the server answers on `path`; halts if it exits, is
/// stopped, or stays silent for 3 minutes.
fn wait_ready(
    entry: &Arc<Mutex<Entry>>,
    child: &mut std::process::Child,
    pgid: i32,
    port: u16,
    path: &str,
) -> Result<(), Halt> {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("the dev server exited ({status})").into());
        }
        if entry.lock().unwrap().status == Status::Stopped {
            let _ = child.wait();
            return Err(Halt::Stopped);
        }
        if http_ok(port, path) {
            set(entry, Status::Ready);
            return Ok(());
        }
        if Instant::now() > deadline {
            kill_group(pgid, 15);
            return Err(Halt::Failed(
                "the dev server didn't answer within 3 minutes".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(300));
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

    /// (review) The dev server and its install step get a stripped
    /// environment: the few variables a dev server needs, the recipe's own
    /// `env` and the ports, and nothing else `client serve` was started
    /// with, so an API key exported there never reaches code a run wrote.
    #[test]
    #[serial_test::serial]
    fn a_preview_gets_only_a_minimal_environment() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().join("w");
        std::fs::create_dir_all(&wt).unwrap();
        let recipe = serde_json::json!({
            "schema_version": "1.0",
            "command": "env > server-env.txt; exit 3",
            "env": {"FROM_RECIPE": "yes {port}"},
            "install": {"command": "env > install-env.txt", "unless_exists": "deps"}
        });
        std::fs::create_dir_all(wt.join(".darkmux")).unwrap();
        std::fs::write(wt.join(".darkmux/preview.json"), recipe.to_string()).unwrap();
        unsafe { std::env::set_var("SOME_PROVIDER_API_KEY", "must-not-leak") };
        let previews = Previews::new(dir.path().join("p"), std::process::id());
        let (state, _) = previews.start("w", &wt, &wt).unwrap();
        let v = until_failed(&previews, "w");
        unsafe { std::env::remove_var("SOME_PROVIDER_API_KEY") };
        assert_eq!(v["status"], "failed", "{v}");
        for file in ["server-env.txt", "install-env.txt"] {
            let env = std::fs::read_to_string(wt.join(file)).unwrap();
            assert!(!env.contains("must-not-leak") && !env.contains("SOME_PROVIDER_API_KEY"), "{file}: {env}");
            assert!(env.lines().any(|l| l.starts_with("PATH=")), "{file} keeps PATH: {env}");
            assert!(env.lines().any(|l| l.starts_with("HOME=")), "{file} keeps HOME: {env}");
        }
        let server = std::fs::read_to_string(wt.join("server-env.txt")).unwrap();
        assert!(server.contains(&format!("FROM_RECIPE=yes {}", state.port)), "{server}");
        assert!(server.contains(&format!("DARKMUX_PREVIEW_PORT={}", state.port)), "{server}");
    }

    /// (review) `env_from_source` never writes through a link a run
    /// planted: a workspace whose `config` is a symlink out of it gets no
    /// `config/.env`, there or anywhere, and the preview says why.
    #[cfg(unix)]
    #[test]
    fn env_from_source_never_writes_through_a_planted_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let (wt, checkout, outside) = (dir.path().join("w"), dir.path().join("mine"), dir.path().join("outside"));
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
        let previews = Previews::new(dir.path().join("p"), std::process::id());
        previews.start("w", &wt, &checkout).unwrap();
        let v = until_failed(&previews, "w");
        assert_eq!(v["status"], "failed", "{v}");
        assert!(v["error"].as_str().unwrap().contains("symlink"), "{v}");
        assert!(!outside.join(".env").exists(), "wrote through the planted link");
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
        let previews = Previews::new(dir.path().join("p"), std::process::id());
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
    /// on a frozen clock; returns the previews and the server's process
    /// group once it has one.
    fn a_live_preview(dir: &Path, clock: Clock) -> (Previews, i32) {
        let wt = dir.join("w");
        std::fs::create_dir_all(wt.join(".darkmux")).unwrap();
        let recipe = serde_json::json!({"schema_version": "1.0", "command": "sleep 30"});
        std::fs::write(wt.join(".darkmux/preview.json"), recipe.to_string()).unwrap();
        let previews = Previews::new(dir.join("p"), std::process::id()).with_clock(clock);
        previews.start("w", &wt, &wt).unwrap();
        for _ in 0..200 {
            let pgid = previews.entries.lock().unwrap()["w"].lock().unwrap().pgid;
            if let Some(pgid) = pgid {
                return (previews, pgid);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the preview's server never started");
    }

    /// (#3459) A preview nobody has asked about for `idle_minutes` is
    /// stopped, and its process group with it; asking (a status) restarts
    /// the count.
    #[cfg(unix)]
    #[test]
    fn an_idle_preview_is_stopped_once_nobody_asked_for_idle_minutes() {
        let dir = tempfile::tempdir().unwrap();
        let (t, clock) = frozen_clock();
        let (previews, pgid) = a_live_preview(dir.path(), clock);
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
        assert!(!group_alive(pgid), "its process group is gone");
        assert!(previews.stop_idle(15).is_empty(), "stopped once only");
    }

    /// (#3459) `0` means never, however long nobody asks; asking again
    /// for a running preview (a second start) counts as asking.
    #[cfg(unix)]
    #[test]
    fn zero_idle_minutes_never_stops_and_a_repeated_start_is_an_ask() {
        let dir = tempfile::tempdir().unwrap();
        let (t, clock) = frozen_clock();
        let (previews, _pgid) = a_live_preview(dir.path(), clock);
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
            (serde_json::json!({"schema_version": "1.0", "command": "x", "comand": "y"}), "`comand`"),
            (serde_json::json!({"schema_version": "1.0", "command": "x", "port": 3333}), "`port`"),
            (serde_json::json!({"schema_version": "1.0", "command": "x", "ready": {"htp": "/"}}), "`ready.htp`"),
            (serde_json::json!({"schema_version": "1.0", "command": "x", "install": {"command": "i", "unless": "n"}}), "`install.unless`"),
        ] {
            let e = parsed(doc).unwrap_err();
            assert!(matches!(e, RecipeRefusal::Invalid(_)), "{e:?}");
            assert!(e.message().contains(&format!("unknown key {key}")), "{e:?}");
        }
        let e = parsed(serde_json::json!({"schema_version": "1.0", "command": "x", "comand": "y"})).unwrap_err();
        assert!(e.message().contains("did you mean `command`?"), "{e:?}");
    }

    /// Contract 5: `schema_version` is required and must be one this darkmux
    /// reads; a newer one says to upgrade.
    #[test]
    fn a_recipe_needs_a_schema_version_this_darkmux_reads() {
        let missing = parsed(serde_json::json!({"command": "x"})).unwrap_err();
        assert!(missing.message().contains("`schema_version`") && missing.message().contains("missing"), "{missing:?}");
        let unknown = parsed(serde_json::json!({"schema_version": "0.9", "command": "x"})).unwrap_err();
        assert!(unknown.message().contains("`schema_version` \"0.9\""), "{unknown:?}");
        let newer = parsed(serde_json::json!({"schema_version": "2.0", "command": "x"})).unwrap_err();
        assert!(newer.message().contains("newer darkmux"), "{newer:?}");
        let typed = parsed(serde_json::json!({"schema_version": 1, "command": "x"})).unwrap_err();
        assert!(typed.message().contains("`schema_version`"), "{typed:?}");
    }

    #[test]
    fn a_recipe_is_a_json_object_with_paths_inside_the_work_copy() {
        assert!(matches!(parse_recipe("[1]"), Err(RecipeRefusal::Invalid(_))));
        assert!(matches!(parse_recipe("{nope"), Err(RecipeRefusal::Invalid(_))));
        let e = parsed(serde_json::json!({"schema_version": "1.0", "command": "x", "env_from_source": ["../.env"]})).unwrap_err();
        assert!(e.message().contains("env_from_source"), "{e:?}");
        let e = parsed(serde_json::json!({"schema_version": "1.0", "command": "x", "install": {"command": "i", "unless_exists": "/etc"}})).unwrap_err();
        assert!(e.message().contains("install.unless_exists"), "{e:?}");
    }
}
