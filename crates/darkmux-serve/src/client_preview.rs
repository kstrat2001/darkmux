//! Live previews for paired apps: darkmux runs a workspace's dev server from
//! a `.darkmux/preview.json` recipe and tells the app its URL.
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
//! darkmux stops, when it is stopped, or when it exits on its own.

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
}

/// All previews this darkmux runs, by workspace name.
pub struct Previews {
    dir: PathBuf,
    /// The process whose exit stops every preview (darkmux itself).
    watch_pid: u32,
    entries: Mutex<HashMap<String, Arc<Mutex<Entry>>>>,
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
        }
    }

    pub fn production() -> Self {
        let dir = darkmux_types::workdir::worktrees_base_dir()
            .parent()
            .map(|p| p.join("previews"))
            .unwrap_or_else(|| PathBuf::from("previews"));
        Self::new(dir, std::process::id())
    }

    fn snapshot(name: &str, e: &Entry) -> serde_json::Value {
        let mut v = serde_json::json!({
            "workspace": name,
            "status": e.status.word(),
            "url": format!("http://127.0.0.1:{}/", e.port),
            "port": e.port,
            "hmr_port": e.hmr_port,
        });
        if let Status::Failed(why) = &e.status {
            v["error"] = why.clone().into();
            v["log_tail"] = tail(&e.log, 20).into();
        }
        v
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
    ) -> Result<(serde_json::Value, bool), RecipeRefusal> {
        let mut entries = self.entries.lock().unwrap();
        if let Some(e) = entries.get(name) {
            let e = e.lock().unwrap();
            if e.status.live() {
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

    pub fn status(&self, name: &str) -> Option<serde_json::Value> {
        let entries = self.entries.lock().unwrap();
        let e = entries.get(name)?.lock().unwrap();
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
        true
    }
}

fn set(entry: &Arc<Mutex<Entry>>, status: Status) -> bool {
    let mut e = entry.lock().unwrap();
    if e.status == Status::Stopped {
        return false;
    }
    e.status = status;
    true
}

fn sh(command: &str, workdir: &Path, log: &Path) -> std::process::Command {
    let out = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(log);
    let mut cmd = std::process::Command::new("/bin/sh");
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
            if let Err(e) = std::fs::copy(&from, &to) {
                return Err(format!("copying {f} from {}: {e}", checkout.display()));
            }
        }
    }
    Ok(())
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
            v = previews.status("w").unwrap();
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
