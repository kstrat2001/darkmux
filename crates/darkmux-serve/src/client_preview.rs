//! Live previews for paired apps: darkmux runs a workspace's dev server from
//! the repo's own `.darkmux/preview.json` and tells the app its URL.
//!
//! Draft recipe shape (unknown keys are ignored while the format settles):
//!
//! ```json
//! {
//!   "command": "npm run dev",
//!   "env": {"PORT": "{port}", "HOST": "127.0.0.1", "VITE_HMR_PORT": "{hmrPort}"},
//!   "ready": {"http": "/"},
//!   "install": {"command": "npm ci", "unless_exists": "node_modules"},
//!   "env_from_source": [".env"]
//! }
//! ```
//!
//! Each preview gets its own free app port and HMR port, substituted into
//! `env` (`{port}`, `{hmrPort}`), so two previews never collide. `install`
//! runs first when its `unless_exists` path is missing. `env_from_source`
//! copies untracked files (like `.env`) from the checkout a worktree was made
//! from, never overwriting. The dev server runs in its own process group with
//! a watchdog: it stops when darkmux stops, when it is stopped, or when it
//! exits on its own.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Deserialize, Debug, Clone)]
pub struct Recipe {
    pub command: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub ready: Option<Ready>,
    #[serde(default)]
    pub install: Option<Install>,
    #[serde(default)]
    pub env_from_source: Vec<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Ready {
    #[serde(default)]
    pub http: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Install {
    pub command: String,
    #[serde(default)]
    pub unless_exists: Option<String>,
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

/// The checkout a worktree was made from (its main working tree), if `dir`
/// is a linked worktree.
pub fn source_of(dir: &Path) -> Option<PathBuf> {
    let common = crate::client_workspace::common_dir(dir)?;
    let own = dir.join(".git").canonicalize().ok();
    if own.as_deref() == Some(common.as_path()) {
        return None;
    }
    common.parent().map(Path::to_path_buf)
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

/// The workspace's `.darkmux/preview.json`, parsed and checked. The caller
/// matches on "no .darkmux/preview.json", so that wording is load-bearing.
fn load_recipe(workdir: &Path) -> Result<Recipe, String> {
    let recipe_path = workdir.join(".darkmux/preview.json");
    if std::fs::symlink_metadata(&recipe_path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err("the preview recipe is a symlink; refusing".into());
    }
    let text = std::fs::read_to_string(&recipe_path)
        .map_err(|_| "this workspace has no .darkmux/preview.json".to_string())?;
    let recipe: Recipe = serde_json::from_str(&text).map_err(|e| format!("preview.json: {e}"))?;
    check_recipe_paths(&recipe)?;
    Ok(recipe)
}

/// Every path a recipe names stays inside the work copy.
fn check_recipe_paths(recipe: &Recipe) -> Result<(), String> {
    for f in &recipe.env_from_source {
        if !plain_relative(f) {
            return Err(format!(
                "env_from_source: {f:?} must be a plain relative path"
            ));
        }
    }
    if let Some(u) = recipe
        .install
        .as_ref()
        .and_then(|i| i.unless_exists.as_ref())
    {
        if !plain_relative(u) {
            return Err(format!(
                "install.unless_exists: {u:?} must be a plain relative path"
            ));
        }
    }
    Ok(())
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

    /// Starts the workspace's preview (or returns the running one). The
    /// boolean says whether it was already running.
    pub fn start(&self, name: &str, workdir: &Path) -> Result<(serde_json::Value, bool), String> {
        let mut entries = self.entries.lock().unwrap();
        if let Some(e) = entries.get(name) {
            let e = e.lock().unwrap();
            if e.status.live() {
                return Ok((Self::snapshot(name, &e), true));
            }
        }
        let recipe = load_recipe(workdir)?;
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        let (port, hmr_port) = (free_port()?, free_port()?);
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

        let workdir = workdir.to_path_buf();
        let watch = self.watch_pid;
        std::thread::spawn(move || run(entry, recipe, workdir, log, port, hmr_port, watch));
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

fn run(
    entry: Arc<Mutex<Entry>>,
    recipe: Recipe,
    workdir: PathBuf,
    log: PathBuf,
    port: u16,
    hmr_port: u16,
    watch: u32,
) {
    let ended = prepare(&entry, &recipe, &workdir, &log)
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
    log: &Path,
) -> Result<(), Halt> {
    copy_env_from_source(recipe, workdir)?;
    if let Some(install) = &recipe.install {
        install_deps(entry, install, workdir, log)?;
    }
    Ok(())
}

/// Untracked files the dev server needs, from the checkout this worktree
/// came from; never overwritten.
fn copy_env_from_source(recipe: &Recipe, workdir: &Path) -> Result<(), String> {
    let Some(source) = source_of(workdir) else {
        return Ok(());
    };
    for f in &recipe.env_from_source {
        let (from, to) = (source.join(f), workdir.join(f));
        if from.is_file() && std::fs::symlink_metadata(&to).is_err() {
            if let Err(e) = std::fs::copy(&from, &to) {
                return Err(format!("copying {f} from {}: {e}", source.display()));
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
            "command": "touch server-ran",
            "install": {"command": "exit 4", "unless_exists": "deps"}
        });
        std::fs::write(wt.join(".darkmux/preview.json"), recipe.to_string()).unwrap();
        let previews = Previews::new(dir.path().join("p"), std::process::id());
        previews.start("w", &wt).unwrap();
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

    #[test]
    fn recipes_ignore_keys_they_dont_know() {
        let r: Recipe = serde_json::from_str(
            r#"{"schema":"draft","command":"npm run dev","port":3333,"hmr":{"framework":"vite"},"env":{"PORT":"{port}"}}"#,
        )
        .unwrap();
        assert_eq!(r.command, "npm run dev");
        assert_eq!(r.env["PORT"], "{port}");
    }
}
