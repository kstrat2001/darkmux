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
//! workspace, so edits hot-reload into it. It is created first and started
//! after (`docker create`, then `docker start -a`), so a stop at any moment
//! finds it by name. A stop, the idle check (#3459), `client serve`'s
//! shutdown and any end of the preview (a failure, the docker CLI exiting)
//! remove it. A `client serve` killed without its handlers cannot, so each
//! container watches a heartbeat file `client serve` touches every 30 s
//! (mounted read-only at [`HEARTBEAT_AT`]) and stops itself once the file
//! has not changed for 12 looks 10 s apart. It counts looks rather than
//! comparing the file with the clock, which Docker Desktop resyncs after the
//! Mac sleeps; sleep counts nothing there (the idle timeout is the one
//! policy that counts it). The watchdog runs in the same container as the
//! workspace's code, which can kill it; such a preview is left to what
//! covered it before the heartbeat: one left behind is removed when the next
//! `client serve` starts (it carries its host's labels, like a dispatch
//! container). No preview process runs on the host.
//!
//! **What a preview container can reach.** No host file outside the
//! workspace and no host environment. It does have the network, and on
//! Docker Desktop `host.docker.internal` reaches this Mac's loopback
//! services, arriving from 127.0.0.1: `client serve`'s own unauthenticated
//! pairing routes (`POST /client/pair`, `GET /client/pair/:id`; a pairing
//! still needs your approval), LM Studio, the darkmux daemon, and any local
//! database or cache. Dispatch containers have the same reach (they need it
//! to call the model), and `--add-host` does not remove it. That is the
//! remaining reason previews stay off by default.
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
//! A project darkmux made from a template (#3460) has no checkout of the
//! user's either, but its first commit is darkmux's own (`start_fresh`).
//! darkmux records that commit's SHA beside the workspace (`.<name>.template`)
//! and reads the recipe as it was there, with host git from the repository
//! it holds ([`RecipeSource::TemplateCommit`]): a regular file, not a link,
//! within the size cap, parsed as strictly. Every object on its path (the
//! commit, its root tree, the `.darkmux` tree, the recipe's blob) is read by
//! id and refused unless it hashes back to that id, and replace refs are off
//! in every host git call, so a forged object or a planted
//! `refs/replace/` in that repository changes nothing. `start_fresh` adds
//! the recipe to the first commit even when an ignore rule would leave it
//! out. No later commit (a run's) and
//! no edit in the work tree changes it. Such a recipe may not list
//! `env_from_source` files, since there is no checkout to copy them from.
//! The recipe is darkmux's own shape: an app's template keeps its own keys
//! in a file of its own (the Mux Bot templates move theirs out of
//! `.darkmux/preview.json`).
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
//! runs first, in the container. (#3484) After it succeeds darkmux records a
//! fingerprint of its inputs beside the workspace (`.<name>.install` in the
//! worktrees base, like the `.source` record, where no run can write): the
//! runtime image id, the install command, `package.json` and each lockfile
//! present (`package-lock.json`, `npm-shrinkwrap.json`, `pnpm-lock.yaml`,
//! `yarn.lock`, `bun.lock`, `bun.lockb`), each read without following a link
//! (a symlinked lockfile counts as its link text). The install is skipped only
//! when its `unless_exists` path exists AND the record matches, so a
//! dependency a run adds, a new image, or a broken earlier install is
//! installed again; a failed install records nothing. A start with
//! `{"clean": true}` stops the preview, forgets the record and deletes the
//! `unless_exists` path inside the workspace (a symlink there is unlinked,
//! never followed; nothing outside the work copy, never your checkout), then
//! starts it with a fresh install. `env_from_source` copies untracked files (like `.env`) from
//! that same checkout into the workspace, never overwriting and never
//! through a symlink at either end.

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
                "{key}: {p:?} must be a plain relative path (no `..`, and no trailing `/` or `.`)"
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

/// Where a workspace's recipe is trusted from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipeSource {
    /// The user's own checkout the workspace was made from (#3451): the
    /// recipe, and each `env_from_source` file, are read there.
    Checkout(PathBuf),
    /// (#3460) A project darkmux made from a template: the recipe as it was
    /// in the first commit, the one darkmux made in `start_fresh`, read with
    /// host git from the repository darkmux holds, never from the work tree.
    /// A later commit (a run's) never changes what is trusted.
    TemplateCommit { git_dir: PathBuf, commit: String },
}

impl RecipeSource {
    /// The recipe, read and checked as strictly as a checkout's. A template
    /// project has no checkout of the user's to copy `env_from_source` files
    /// from, so a template recipe that lists any is refused, naming the key.
    pub fn load(&self) -> Result<Recipe, RecipeRefusal> {
        match self {
            RecipeSource::Checkout(checkout) => load_recipe(checkout),
            RecipeSource::TemplateCommit { git_dir, commit } => {
                let recipe = parse_recipe(&read_recipe_at_commit(git_dir, commit)?)?;
                if recipe.env_from_source.is_empty() {
                    return Ok(recipe);
                }
                Err(RecipeRefusal::Invalid(format!(
                    "{RECIPE_PATH}: `env_from_source` names files to copy from a checkout of yours, and a \
                     project made from a template has none; remove `env_from_source` from the template's recipe"
                )))
            }
        }
    }

    /// The user's own checkout, when the recipe comes from one.
    pub fn checkout(&self) -> Option<&Path> {
        match self {
            RecipeSource::Checkout(c) => Some(c),
            RecipeSource::TemplateCommit { .. } => None,
        }
    }
}

/// (#3460) The recipe's text as it is in `commit` of the repository at
/// `git_dir` (`git show <commit>:.darkmux/preview.json`, through host git
/// with no work tree). It must be a regular file (not a symlink, nor under
/// a symlinked `.darkmux`) within [`MAX_RECIPE_BYTES`].
pub fn read_recipe_at_commit(git_dir: &Path, commit: &str) -> Result<String, RecipeRefusal> {
    if !crate::client_workspace::is_commit_sha(commit) {
        return Err(RecipeRefusal::Invalid(format!(
            "{commit:?} is not a full commit SHA; refusing the template's recipe"
        )));
    }
    let git = darkmux_types::host_git::HostGit::held_git_dir(git_dir);
    // (#3460 review) Every object on the recipe's path is checked against
    // its id (git does not check a loose object's hash when it reads it), so
    // what `ls-tree` reports below comes from objects that are what their
    // ids say. Replace refs are off in every `HostGit` call.
    let commit_body = verified_object(&git, "commit", commit)?;
    let root = String::from_utf8_lossy(&commit_body)
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("tree "))
        .map(str::to_string)
        .ok_or_else(|| RecipeRefusal::Invalid(format!("commit {commit} names no tree")))?;
    verified_object(&git, "tree", &root)?;
    let listed = git_stdout(
        &git,
        &["ls-tree", "-l", "-t", commit, "--", ".darkmux", RECIPE_PATH],
    )?;
    let (folder, blob) = check_recipe_entries(&listed, commit)?;
    verified_object(&git, "tree", &folder)?;
    // The size was checked on the tree entry, before reading the blob.
    String::from_utf8(verified_object(&git, "blob", &blob)?)
        .map_err(|_| RecipeRefusal::Invalid(format!("{RECIPE_PATH} at {commit} is not UTF-8")))
}

/// `git <args>`'s stdout, through host git; a failure is unreadable.
fn git_stdout(
    git: &darkmux_types::host_git::HostGit,
    args: &[&str],
) -> Result<String, RecipeRefusal> {
    let out = git
        .output(args)
        .map_err(|e| RecipeRefusal::Unreadable(format!("reading the template's recipe: {e}")))?;
    if !out.status.success() {
        return Err(RecipeRefusal::Unreadable(format!(
            "reading the template's recipe: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// The object `oid` of type `kind`, read with `cat-file` and refused unless
/// its content hashes back to `oid` (`hash-object --stdin`, the repository's
/// own hash, no filters).
fn verified_object(
    git: &darkmux_types::host_git::HostGit,
    kind: &str,
    oid: &str,
) -> Result<Vec<u8>, RecipeRefusal> {
    use std::io::Write;
    let unreadable = |e: String| RecipeRefusal::Unreadable(format!("reading {kind} {oid}: {e}"));
    let read = git
        .output(&["cat-file", kind, oid])
        .map_err(|e| unreadable(e.to_string()))?;
    if !read.status.success() {
        return Err(unreadable(
            String::from_utf8_lossy(&read.stderr).trim().to_string(),
        ));
    }
    let mut hash = git
        .command(&["hash-object", "-t", kind, "--no-filters", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| unreadable(e.to_string()))?;
    if let Some(mut stdin) = hash.stdin.take() {
        stdin
            .write_all(&read.stdout)
            .map_err(|e| unreadable(e.to_string()))?;
    }
    let hashed = hash
        .wait_with_output()
        .map_err(|e| unreadable(e.to_string()))?;
    if String::from_utf8_lossy(&hashed.stdout).trim() != oid {
        return Err(RecipeRefusal::Invalid(format!(
            "{kind} {oid} in the template's repository does not match its content; refusing its recipe"
        )));
    }
    Ok(read.stdout)
}

/// `git ls-tree -l` lines for `.darkmux` and the recipe in `commit`: no
/// symlink on the way, `.darkmux` a folder, and the recipe a regular file
/// within the size cap. Returns the folder's and the recipe's object ids.
fn check_recipe_entries(listed: &str, commit: &str) -> Result<(String, String), RecipeRefusal> {
    let entries: Vec<LsTreeEntry> = listed.lines().filter_map(ls_tree_entry).collect();
    if let Some(e) = entries.iter().find(|e| e.mode == "120000") {
        return Err(RecipeRefusal::Symlink(format!(
            "{} is a symlink in the template's first commit {commit}; refusing",
            e.path
        )));
    }
    let Some(recipe) = entries.iter().find(|e| e.path == RECIPE_PATH) else {
        return Err(RecipeRefusal::NotFound(format!(
            "no {RECIPE_PATH} in the template's first commit {commit}"
        )));
    };
    check_recipe_blob(recipe.mode, recipe.size, commit)?;
    let folder = entries
        .iter()
        .find(|e| e.path == ".darkmux" && e.mode == "040000")
        .ok_or_else(|| RecipeRefusal::Invalid(format!(".darkmux is not a folder in {commit}")))?;
    Ok((folder.oid.to_string(), recipe.oid.to_string()))
}

/// One `git ls-tree -l` line.
struct LsTreeEntry<'a> {
    mode: &'a str,
    oid: &'a str,
    size: &'a str,
    path: &'a str,
}

fn ls_tree_entry(line: &str) -> Option<LsTreeEntry<'_>> {
    let (meta, path) = line.split_once('\t')?;
    let fields: Vec<&str> = meta.split_whitespace().collect();
    Some(LsTreeEntry {
        mode: fields.first()?,
        oid: fields.get(2)?,
        size: fields.get(3).copied().unwrap_or("-"),
        path,
    })
}

/// The recipe's tree entry: a regular file (`100644` or `100755`) of at
/// most [`MAX_RECIPE_BYTES`].
fn check_recipe_blob(mode: &str, size: &str, commit: &str) -> Result<(), RecipeRefusal> {
    if mode != "100644" && mode != "100755" {
        return Err(RecipeRefusal::Invalid(format!(
            "{RECIPE_PATH} in the template's first commit {commit} is not a regular file"
        )));
    }
    match size.parse::<u64>() {
        Ok(n) if n <= MAX_RECIPE_BYTES => Ok(()),
        _ => Err(RecipeRefusal::TooLarge(format!(
            "{RECIPE_PATH} is larger than {MAX_RECIPE_BYTES} bytes"
        ))),
    }
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
    /// The preview's container, once `docker create` made it; taken by
    /// whoever removes it.
    container: Option<String>,
    /// Its heartbeat file, which `client serve` touches while it runs.
    heartbeat: Option<PathBuf>,
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
    /// Picks a free port (a test pins one, to make two runs share it).
    ports: Arc<dyn Fn() -> Result<u16, String> + Send + Sync>,
}

/// A relative path inside the work copy, spelled plainly: no absolute
/// path, and every `/`-separated part a real name (no `..`, no `.`, no
/// empty part, so no trailing `/`). A trailing `/` or `/.` would make the
/// filesystem follow a symlink at the last name, which a delete or a copy
/// must never do.
pub fn plain_relative(p: &str) -> bool {
    !p.is_empty()
        && !Path::new(p).is_absolute()
        && p.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
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
/// Printed when the container stops itself because darkmux stopped beating.
const STALE_LINE: &str =
    "darkmux-preview: darkmux stopped answering (its heartbeat is stale); stopping";

/// (#3456 review F2) Where a preview container sees its heartbeat folder,
/// read-only. `client serve` touches the file in it every [`BEAT_EVERY`];
/// the container stops itself once the file's mtime has not changed for
/// [`HEARTBEAT_MISSED_LOOKS`] looks in a row, so a `client serve` killed
/// without its handlers (SIGKILL, a crash) does not leave a dev server
/// running until the next one starts. It counts looks, never compares the
/// mtime with the clock: after the Mac sleeps, Docker Desktop resyncs the
/// VM's clock, which would make a live preview's heartbeat look old. Sleep
/// counts nothing here; the idle timeout (#3459) is the one policy that
/// counts it.
pub const HEARTBEAT_AT: &str = "/darkmux-heartbeat";
/// The heartbeat file's name, in its folder.
pub const HEARTBEAT_FILE: &str = "alive";
/// How often `client serve` touches each live preview's heartbeat.
pub const BEAT_EVERY: Duration = Duration::from_secs(30);
/// How many looks in a row may find the heartbeat unchanged before the
/// container stops itself: 12 looks 10 s apart, two minutes of running time.
pub const HEARTBEAT_MISSED_LOOKS: u32 = 12;
/// How often the container looks at its heartbeat, in seconds.
const HEARTBEAT_CHECK_SECS: &str = "10";
/// How long a stale container's script gets after TERM before KILL, in
/// seconds: a dev server that ignores TERM still ends.
const STALE_KILL_GRACE_SECS: &str = "5";

/// What the container's watchdog reads, and when it gives up.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Watch {
    /// The heartbeat file, as the container sees it.
    file: String,
    /// Looks in a row that may find it unchanged.
    missed: u32,
    /// `sleep`'s argument between looks.
    every: String,
    /// `sleep`'s argument between the TERM and the KILL.
    grace: String,
}

impl Watch {
    fn production() -> Self {
        Watch {
            file: format!("{HEARTBEAT_AT}/{HEARTBEAT_FILE}"),
            missed: HEARTBEAT_MISSED_LOOKS,
            every: HEARTBEAT_CHECK_SECS.to_string(),
            grace: STALE_KILL_GRACE_SECS.to_string(),
        }
    }
}

/// One preview container: everything `docker run` is told. Nothing of
/// `client serve`'s own environment is in it: `env` is the whole of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    /// The workspace, mounted read-write at [`WORKSPACE_AT`]: the only host
    /// path the container can write.
    pub workspace: PathBuf,
    /// The preview's heartbeat folder, mounted read-only at
    /// [`HEARTBEAT_AT`]; it holds nothing but the heartbeat file.
    pub heartbeat: PathBuf,
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

/// `docker create` arguments for a preview container: the dispatch
/// container's hardening (no capabilities, no new privileges, its pid and
/// memory limits, `--init`), removed when it exits, the workspace its only
/// writable mount (and its heartbeat folder, read-only), only the
/// preview's two ports published and only on loopback, and an environment
/// of exactly `spec.env`. It keeps the default network: an install step
/// fetches packages. Created first and started after (`docker start -a`),
/// so a stop at any moment finds the container by name.
pub fn docker_create_args(spec: &ContainerSpec) -> Vec<String> {
    use darkmux_crew::dispatch_internal::{
        DOCKER_CAP_DROP, DOCKER_MEMORY, DOCKER_PIDS_LIMIT, DOCKER_SECURITY_OPT,
    };
    let mut args: Vec<String> = ["create", "--rm", "--init", "--name", &spec.name]
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
        "-v".to_string(),
        format!("{}:{HEARTBEAT_AT}:ro", spec.heartbeat.display()),
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

/// The container's script: a start line, a watchdog in the background
/// that ends the script (and with it the container: `--init`'s PID 1 exits
/// when it does) once the heartbeat is stale, then, in a subshell, the
/// install step when it is due (its own `exit` ends only it) and the dev
/// server.
fn container_script(recipe: &Recipe, install_due: bool, watch: &Watch) -> String {
    let mut script = format!("echo '{STARTED_LINE}'\n{}", watchdog(watch));
    script.push_str("(\n");
    if let (true, Some(install)) = (install_due, &recipe.install) {
        script.push_str(&format!(
            "(\n{}\n) || {{ s=$?; echo \"{INSTALL_FAILED_LINE} $s\"; exit $s; }}\n",
            install.command
        ));
    }
    script.push_str(&format!("echo '{SERVING_LINE}'\n{}\n)\n", recipe.command));
    script
}

/// The background loop that stops the container when darkmux stops
/// beating: every `watch.every` it reads the heartbeat's mtime (GNU `stat`,
/// else BSD's; a missing file reads as `gone`), counts the looks in a row
/// that found it unchanged, and once that reaches `watch.missed` it says so
/// and ends the script (`$$`, the script's own pid): TERM, then KILL after
/// `watch.grace`, so the script ends even when `$$` does not on TERM (a
/// shell that ran the dev server in its own place, one started with TERM
/// ignored). Workspace code runs in
/// the same container and can kill this loop; then the preview is left to
/// the reaper at the next `client serve` start, as before the heartbeat.
fn watchdog(watch: &Watch) -> String {
    format!(
        "( last=''; same=0; while kill -0 $$ 2>/dev/null && sleep {every}; do \
         seen=$(stat -c %Y '{file}' 2>/dev/null || stat -f %m '{file}' 2>/dev/null || echo gone); \
         if [ \"$seen\" = \"$last\" ]; then same=$((same + 1)); else same=0; last=$seen; fi; \
         if [ $same -ge {missed} ]; then echo '{STALE_LINE}'; kill -TERM $$; \
         sleep {grace}; kill -KILL $$ 2>/dev/null; exit 0; fi; \
         done ) &\n",
        every = watch.every,
        file = watch.file,
        missed = watch.missed,
        grace = watch.grace,
    )
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
    /// Creates the container (not started). It returns once the container
    /// exists, so a stop from then on finds it by name.
    fn create(&self, spec: &ContainerSpec) -> Result<(), String>;
    /// Starts the created container, attached: its output goes to `log`,
    /// and the child ends when the container does.
    fn start(&self, name: &str, log: &Path) -> Result<std::process::Child, String>;
    /// The container's `/proc/net/tcp` and `/proc/net/tcp6`, to tell why a
    /// dev server never answered; `None` when they cannot be read.
    fn listeners(&self, name: &str) -> Option<String>;
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

/// How long one `docker stop`, `docker rm` or `docker exec` may take.
const DOCKER_CALL_BOUND: Duration = Duration::from_secs(30);
/// How long `docker create` may take (the image is already resolved).
const DOCKER_CREATE_BOUND: Duration = Duration::from_secs(120);

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

    fn create(&self, spec: &ContainerSpec) -> Result<(), String> {
        use darkmux_crew::bounded_command::{run_bounded, Bounded};
        let mut cmd = std::process::Command::new("docker");
        cmd.args(docker_create_args(spec));
        match run_bounded(cmd, DOCKER_CREATE_BOUND) {
            Bounded::Finished { success: true, .. } => Ok(()),
            Bounded::Finished { stderr, .. } => Err(format!(
                "docker create: {}",
                String::from_utf8_lossy(&stderr).trim()
            )),
            other => Err(format!("docker create did not finish: {other:?}")),
        }
    }

    fn listeners(&self, name: &str) -> Option<String> {
        use darkmux_crew::bounded_command::{run_bounded, Bounded};
        let mut cmd = std::process::Command::new("docker");
        cmd.args(["exec", name, "cat", "/proc/net/tcp", "/proc/net/tcp6"]);
        match run_bounded(cmd, DOCKER_CALL_BOUND) {
            Bounded::Finished { stdout, .. } if !stdout.is_empty() => {
                Some(String::from_utf8_lossy(&stdout).to_string())
            }
            _ => None,
        }
    }

    fn start(&self, name: &str, log: &Path) -> Result<std::process::Child, String> {
        let out = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(log)
            .map_err(|e| format!("opening {}: {e}", log.display()))?;
        let err = out
            .try_clone()
            .map_err(|e| format!("opening {}: {e}", log.display()))?;
        let mut cmd = std::process::Command::new("docker");
        cmd.args(["start", "-a", name])
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
            ports: Arc::new(free_port),
        }
    }

    /// The same previews picking ports with `ports` (a test's fixed ones).
    #[cfg(test)]
    pub(crate) fn with_ports(
        mut self,
        ports: Arc<dyn Fn() -> Result<u16, String> + Send + Sync>,
    ) -> Self {
        self.ports = ports;
        self
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
    /// recipe `source` names: the user's own checkout the workspace was made
    /// from, or a template project's first commit. Never from `workdir`,
    /// which is what the preview's container mounts. The boolean says
    /// whether it was already running.
    pub fn start(
        &self,
        name: &str,
        workdir: &Path,
        source: &RecipeSource,
    ) -> Result<(PreviewState, bool), RecipeRefusal> {
        let mut entries = self.entries.lock().unwrap();
        if let Some(e) = entries.get(name) {
            let mut e = e.lock().unwrap();
            if e.status.live() {
                e.last_asked = (self.clock)();
                return Ok((Self::snapshot(name, &e), true));
            }
        }
        let recipe = source.load()?;
        let unreadable = |e: String| RecipeRefusal::Unreadable(e);
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| unreadable(format!("{}: {e}", self.dir.display())))?;
        let (port, hmr_port) = (
            (self.ports)().map_err(unreadable)?,
            (self.ports)().map_err(unreadable)?,
        );
        let log = self.dir.join(format!("{name}.log"));
        let _ = std::fs::write(&log, "");
        let entry = Arc::new(Mutex::new(Entry {
            port,
            hmr_port,
            status: Status::Starting,
            container: None,
            heartbeat: None,
            log: log.clone(),
            last_asked: (self.clock)(),
        }));
        let snap = Self::snapshot(name, &entry.lock().unwrap());
        entries.insert(name.to_string(), entry.clone());
        drop(entries);

        let at = RunIn {
            name: name.to_string(),
            run: run_id(),
            workdir: workdir.to_path_buf(),
            checkout: source.checkout().map(Path::to_path_buf),
            log,
            port,
            hmr_port,
        };
        let containers = self.containers.clone();
        std::thread::spawn(move || run(entry, recipe, at, &*containers));
        Ok((snap, false))
    }

    /// (#3484) Clears the way for a fresh install: stops the preview (its
    /// container removed), forgets its install record, and deletes the
    /// recipe's `install.unless_exists` path inside the workspace. A recipe
    /// with neither only has its record cleared. The workspace is
    /// run-written, so the path is deleted only after it resolves inside the
    /// work copy (no symlinked folder on the way), and a symlink there is
    /// unlinked, never followed. Never the user's own checkout.
    pub fn clean(
        &self,
        name: &str,
        workdir: &Path,
        source: &RecipeSource,
    ) -> Result<(), RecipeRefusal> {
        let recipe = source.load()?;
        if source.checkout().is_some_and(|c| same_dir(c, workdir)) {
            return Err(RecipeRefusal::Invalid(format!(
                "{} is your own checkout, not darkmux's workspace; refusing to clean it",
                workdir.display()
            )));
        }
        self.stop(name);
        if let Some(base) = workdir.parent() {
            crate::client_workspace::forget_install(base, name);
        }
        let Some(rel) = recipe
            .install
            .as_ref()
            .and_then(|i| i.unless_exists.as_deref())
        else {
            return Ok(());
        };
        remove_inside(workdir, rel).map_err(RecipeRefusal::Unreadable)
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

    /// (#3456 review F2) Touches the heartbeat of every live preview. `client
    /// serve` calls it every [`BEAT_EVERY`]; a container whose heartbeat is
    /// heartbeat stops changing for [`HEARTBEAT_MISSED_LOOKS`] looks stops itself.
    pub fn beat(&self) {
        let files: Vec<PathBuf> = self
            .entries
            .lock()
            .unwrap()
            .values()
            .filter_map(|e| {
                let e = e.lock().unwrap();
                e.status.live().then(|| e.heartbeat.clone()).flatten()
            })
            .collect();
        for file in files {
            touch(&file);
        }
    }

    /// Removes preview containers a `client serve` that died left behind
    /// (their labeled host process is gone). Returns their names.
    pub fn reap_orphans(&self) -> Vec<String> {
        self.containers.reap_orphans()
    }
}

/// Sets `file`'s mtime to now, in place (the container sees the same
/// inode through its bind mount). Best effort.
fn touch(file: &Path) {
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(file) {
        let _ = f.set_modified(std::time::SystemTime::now());
    }
}

/// (#3456 review F2) Beats every `interval` until `stop` is set: the
/// thread `client serve` runs so its preview containers know it is alive.
pub fn spawn_heartbeat(
    previews: Arc<Previews>,
    interval: Duration,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    use std::sync::atomic::Ordering;
    std::thread::spawn(move || {
        let poll = interval.min(Duration::from_millis(200));
        while !stop.load(Ordering::SeqCst) {
            previews.beat();
            let mut slept = Duration::ZERO;
            while slept < interval && !stop.load(Ordering::SeqCst) {
                std::thread::sleep(poll);
                slept += poll;
            }
        }
    })
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
/// recipe and env files come from (none for a template project), its log
/// and its ports.
struct RunIn {
    name: String,
    /// (#3485) This run's own id: its container and heartbeat are keyed by
    /// it, so two runs of one workspace never share either, even on one port.
    run: String,
    workdir: PathBuf,
    checkout: Option<PathBuf>,
    log: PathBuf,
    port: u16,
    hmr_port: u16,
}

fn run(entry: Arc<Mutex<Entry>>, recipe: Recipe, at: RunIn, containers: &dyn PreviewContainers) {
    let ended = prepare(&recipe, &at).and_then(|()| serve(&entry, &recipe, &at, containers));
    // A stop is already recorded; `set` never overwrites it with a failure.
    if let Err(Halt::Failed(why)) = ended {
        set(&entry, Status::Failed(why));
    }
    // (#3456 review) However it ended (the docker CLI can exit while its
    // container runs on), a container nobody has removed yet is removed now.
    let (left, heartbeat) = {
        let mut e = entry.lock().unwrap();
        (e.container.take(), e.heartbeat.take())
    };
    if let Some(container) = left {
        containers.remove(&container);
    }
    if let Some(dir) = heartbeat.as_deref().and_then(Path::parent) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Everything before the container, on the host: env files into the
/// workspace.
fn prepare(recipe: &Recipe, at: &RunIn) -> Result<(), Halt> {
    if let Some(checkout) = &at.checkout {
        copy_env_from_source(recipe, &at.workdir, checkout)?;
    }
    Ok(())
}

/// (#3484) The lockfiles whose contents are an install's inputs, in the
/// workspace's root, beside `package.json`, which always counts (a run can
/// add a dependency there before any lockfile changes).
const LOCKFILES: [&str; 6] = [
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
];
/// The most of one input file the fingerprint reads.
const MAX_INPUT_BYTES: u64 = 256 * 1024 * 1024;

/// (#3484) The fingerprint of an install: the runtime image id, the install
/// command and its inputs (`package.json` and each lockfile present). The
/// workspace is run-written, so an input is read without following a link:
/// a symlink is hashed as its link text, never as what it points at.
pub(crate) fn install_fingerprint(workdir: &Path, command: &str, image: &str) -> String {
    let mut hash = blake3::Hasher::new();
    hash_field(&mut hash, "image", image.as_bytes());
    hash_field(&mut hash, "command", command.as_bytes());
    let present = LOCKFILES
        .into_iter()
        .filter(|f| std::fs::symlink_metadata(workdir.join(f)).is_ok());
    for file in std::iter::once("package.json").chain(present) {
        hash_field(&mut hash, file, &input_bytes(&workdir.join(file)));
    }
    hash.finalize().to_hex().to_string()
}

fn hash_field(hash: &mut blake3::Hasher, name: &str, bytes: &[u8]) {
    hash.update(name.as_bytes());
    hash.update(&[0]);
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

/// One input's bytes as the fingerprint sees them, tagged by kind.
fn input_bytes(path: &Path) -> Vec<u8> {
    use std::io::Read;
    let tagged = |tag: &[u8], rest: &[u8]| [tag, rest].concat();
    match std::fs::symlink_metadata(path) {
        Err(_) => b"absent".to_vec(),
        Ok(m) if m.file_type().is_symlink() => {
            let target = std::fs::read_link(path).unwrap_or_default();
            tagged(b"link:", target.as_os_str().as_encoded_bytes())
        }
        Ok(m) if !m.is_file() => b"not-a-file".to_vec(),
        Ok(_) => {
            let mut opts = std::fs::OpenOptions::new();
            opts.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.custom_flags(libc::O_NOFOLLOW);
            }
            let mut bytes = Vec::new();
            match opts
                .open(path)
                .and_then(|f| f.take(MAX_INPUT_BYTES).read_to_end(&mut bytes))
            {
                Ok(_) => tagged(b"file:", &bytes),
                Err(_) => b"unreadable".to_vec(),
            }
        }
    }
}

/// (#3484) Whether the install is due, and the fingerprint to record once
/// it succeeds. Without `unless_exists` it always runs. With it, the
/// install is skipped only when that path exists AND the recorded
/// fingerprint of the last successful install matches this one, so a
/// changed lockfile, a changed image or a broken first install is redone.
fn install_plan(recipe: &Recipe, at: &RunIn, image: &str) -> (bool, Option<String>) {
    let Some(install) = &recipe.install else {
        return (false, None);
    };
    let fingerprint = install_fingerprint(&at.workdir, &install.command, image);
    let due = match &install.unless_exists {
        None => true,
        Some(p) => {
            let missing = std::fs::symlink_metadata(at.workdir.join(p)).is_err();
            missing || recorded_install(at).as_deref() != Some(fingerprint.as_str())
        }
    };
    (due, Some(fingerprint))
}

/// The workspace's recorded install fingerprint, beside it in the base.
fn recorded_install(at: &RunIn) -> Option<String> {
    crate::client_workspace::install_fingerprint_of(at.workdir.parent()?, &at.name)
}

/// Records a successful install's fingerprint beside the workspace.
fn record_installed(at: &RunIn, fingerprint: &str) {
    let Some(base) = at.workdir.parent() else {
        return;
    };
    if let Err(e) = crate::client_workspace::record_install(base, &at.name, fingerprint) {
        eprintln!("darkmux client serve: preview {}: {e}", at.name);
    }
}

/// Whether `a` and `b` are the same directory.
fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// (#3484) Deletes `rel` inside `workdir`: only once it resolves inside the
/// work copy ([`check_unlinked_inside`]: no symlinked folder on the way),
/// and a symlink at `rel` is unlinked, never followed. Absent is done.
fn remove_inside(workdir: &Path, rel: &str) -> Result<(), String> {
    if !plain_relative(rel) {
        return Err(format!(
            "{rel:?} is not a plain relative path; refusing to delete it"
        ));
    }
    check_unlinked_inside(workdir, rel, "install.unless_exists", "delete")?;
    let path = workdir.join(rel);
    let removed = match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => Err(e),
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(&path),
        // A file, or a symlink (removed as the link itself).
        Ok(_) => std::fs::remove_file(&path),
    };
    removed.map_err(|e| format!("deleting {}: {e}", path.display()))
}

/// Untracked files the dev server needs, from the user's own checkout (the
/// one the recipe came from); never overwritten. The checkout is the user's,
/// but a run that once had it mounted could have left a link there (a
/// `.env` pointing at a key), so a source reached through any symlink is
/// refused, and the file is opened without following one.
fn copy_env_from_source(recipe: &Recipe, workdir: &Path, checkout: &Path) -> Result<(), String> {
    for f in &recipe.env_from_source {
        let (from, to) = (checkout.join(f), workdir.join(f));
        if std::fs::symlink_metadata(&to).is_ok() {
            continue;
        }
        if let Some(link) = first_link(checkout, f, true) {
            return Err(format!(
                "env_from_source: {} in your checkout is a symlink; refusing to copy {f} through it \
                 (copy the file it points at into place instead)",
                link.display()
            ));
        }
        if std::fs::symlink_metadata(&from).is_ok_and(|m| m.is_file()) {
            check_unlinked_inside(workdir, f, "env_from_source", "copy")?;
            copy_unlinked(&from, &to).map_err(|e| format!("copying {f} from {}: {e}", checkout.display()))?;
        }
    }
    Ok(())
}

/// Copies the regular file `from` to a new file `to`, with its permission
/// bits: never through a symlink at either end (`from` opened with
/// `O_NOFOLLOW`, `to` created exclusively).
fn copy_unlinked(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut read = std::fs::OpenOptions::new();
    read.read(true);
    let mut write = std::fs::OpenOptions::new();
    write.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        read.custom_flags(libc::O_NOFOLLOW);
        write.mode(std::fs::symlink_metadata(from)?.permissions().mode() & 0o777);
    }
    std::io::copy(&mut read.open(from)?, &mut write.open(to)?).map(|_| ())
}

/// The first component of `rel` under `root` that is a symlink: every
/// folder on the way, and the last name too when `with_last`.
fn first_link(root: &Path, rel: &str, with_last: bool) -> Option<PathBuf> {
    let parts: Vec<_> = Path::new(rel).components().collect();
    let upto = if with_last { parts.len() } else { parts.len().saturating_sub(1) };
    let mut at = root.to_path_buf();
    for part in &parts[..upto] {
        at.push(part);
        if std::fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink()) {
            return Some(at);
        }
    }
    None
}

/// Writing (`verb`: copying or deleting) `rel` inside `workdir`, named by the
/// recipe's `key`, stays inside it: no folder on the way (one a run may have
/// replaced with a link) is a symlink, and the parent, resolved, is still
/// inside the work copy.
fn check_unlinked_inside(workdir: &Path, rel: &str, key: &str, verb: &str) -> Result<(), String> {
    if let Some(at) = first_link(workdir, rel, false) {
        return Err(format!(
            "{key}: {} in the workspace is a symlink; refusing to {verb} {rel} through it",
            at.display()
        ));
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
        Err(format!("{key}: {rel} is outside the workspace; refusing to {verb} it"))
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
    let name = container_name(at);
    ContainerSpec {
        heartbeat: heartbeat_dir(at, &name),
        name,
        image,
        workspace: at.workdir.clone(),
        ports: [at.port, at.hmr_port],
        user,
        labels,
        env: container_env(recipe, at.port, at.hmr_port),
        script: container_script(recipe, install_due, &Watch::production()),
    }
}

/// The preview's container name. The app port makes it unique: a restarted
/// preview never collides with its predecessor while that one is still
/// being removed.
fn container_name(at: &RunIn) -> String {
    format!(
        "{PREVIEW_CONTAINER_PREFIX}{}-{}-{}",
        at.name, at.port, at.run
    )
}

/// (#3485) A preview run's id: unique in this process (a counter) and
/// across restarts of it (the clock).
fn run_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{nanos:x}{n:x}")
}

/// The preview's heartbeat folder: beside its log, outside the workspace.
fn heartbeat_dir(at: &RunIn, name: &str) -> PathBuf {
    let previews = at.log.parent().map(Path::to_path_buf).unwrap_or_default();
    previews.join("heartbeats").join(name)
}

/// A fresh heartbeat folder and file for the container, recorded on the
/// entry so [`Previews::beat`] touches it.
fn fresh_heartbeat(entry: &Arc<Mutex<Entry>>, dir: &Path) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let file = dir.join(HEARTBEAT_FILE);
    std::fs::write(&file, "").map_err(|e| format!("{}: {e}", file.display()))?;
    entry.lock().unwrap().heartbeat = Some(file);
    Ok(())
}

/// Starts the container, waits for the dev server to answer, then waits
/// for it to end. It only ever returns a halt: a dev server is never
/// expected to exit, so an exit after ready is a failure unless it was
/// stopped.
fn serve(
    entry: &Arc<Mutex<Entry>>,
    recipe: &Recipe,
    at: &RunIn,
    containers: &dyn PreviewContainers,
) -> Result<(), Halt> {
    let resolved = containers
        .image_and_user()
        .map_err(|e| format!("a preview runs in a container: {e}"))?;
    let (install_due, fingerprint) = install_plan(recipe, at, &resolved.0);
    if install_due {
        advance(entry, Status::Installing)?;
    }
    // Recorded once the dev server line shows the install succeeded.
    let installed = fingerprint.filter(|_| install_due);
    let spec = spec_for(recipe, at, install_due, resolved, containers.host_labels());
    fresh_heartbeat(entry, &spec.heartbeat)?;
    containers.create(&spec)?;
    adopt(entry, &spec.name, containers)?;
    let mut child = containers.start(&spec.name, &at.log)?;
    wait_ready(
        entry,
        &mut child,
        (&spec.name, containers),
        at,
        (recipe, installed.as_deref()),
    )?;
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
    if text.lines().any(|l| l == STALE_LINE) {
        return "the preview's container stopped itself: darkmux stopped answering".to_string();
    }
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

/// Records the created container so a stop can reach it; if the preview
/// was stopped while it was being created, removes it instead (#3456
/// review F1: the container now exists, so the removal finds it).
fn adopt(
    entry: &Arc<Mutex<Entry>>,
    name: &str,
    containers: &dyn PreviewContainers,
) -> Result<(), Halt> {
    let mut e = entry.lock().unwrap();
    if e.status == Status::Stopped {
        drop(e);
        containers.remove(name);
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
    (name, containers): (&str, &dyn PreviewContainers),
    at: &RunIn,
    (recipe, installed): (&Recipe, Option<&str>),
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
            now_serving(entry, at, installed)?;
        }
        if serving_since.is_some() && http_ok(at.port, &path) {
            set(entry, Status::Ready);
            return Ok(());
        }
        if serving_since.is_some_and(|t| t.elapsed() > READY_WITHIN) {
            let listening = containers.listeners(name);
            return Err(Halt::Failed(not_ready_reason(
                at.port,
                listening.as_deref(),
            )));
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// The dev server line appeared: the install, when one ran, succeeded
/// (#3484: record it), and the preview is starting.
fn now_serving(entry: &Arc<Mutex<Entry>>, at: &RunIn, installed: Option<&str>) -> Result<(), Halt> {
    if let Some(fingerprint) = installed {
        record_installed(at, fingerprint);
    }
    advance(entry, Status::Starting)
}

/// Why a dev server never answered on its published port. (#3456 review F5)
/// When the container's `/proc/net/tcp*` shows it listening on the port on
/// loopback only, that is why: say so, and what to change.
fn not_ready_reason(port: u16, listeners: Option<&str>) -> String {
    if listeners.is_some_and(|l| listens_on_loopback_only(l, port)) {
        return format!(
            "the dev server listens on 127.0.0.1 inside its container; set it to bind 0.0.0.0 \
             (the port is published on this Mac's loopback only); port {port}"
        );
    }
    format!(
        "the dev server didn't answer on port {port} within 3 minutes; in its container it must \
         listen on 0.0.0.0 (the port is published on 127.0.0.1 only)"
    )
}

/// Whether `/proc/net/tcp` / `tcp6` text shows a listener on `port` bound
/// to a loopback address (127.x, `::1`, or `::ffff:127.x`) and none on every
/// address (`0.0.0.0` or `::`), which the published port could reach.
fn listens_on_loopback_only(proc_net: &str, port: u16) -> bool {
    let want = format!("{port:04X}");
    let mut loopback = false;
    for line in proc_net.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (Some(local), Some(state)) = (fields.get(1), fields.get(3)) else {
            continue;
        };
        let Some((addr, p)) = local.rsplit_once(':') else {
            continue;
        };
        if *state != "0A" || p != want {
            continue;
        }
        if addr.chars().all(|c| c == '0') {
            return false;
        }
        loopback |= is_loopback_hex(addr);
    }
    loopback
}

/// A `/proc/net/tcp*` address (hex, in the kernel's byte order) that is
/// loopback: IPv4 127.x, IPv6 `::1`, or IPv4-mapped `::ffff:127.x`.
fn is_loopback_hex(addr: &str) -> bool {
    match addr.len() {
        8 => addr.ends_with("7F"),
        32 => {
            addr == "00000000000000000000000001000000"
                || (addr.starts_with("0000000000000000FFFF0000") && addr.ends_with("7F"))
        }
        _ => false,
    }
}

/// Whether `log` holds the line `line`.
fn log_has(log: &Path, line: &str) -> bool {
    std::fs::read_to_string(log).is_ok_and(|t| t.lines().any(|l| l == line))
}

/// TEST ONLY: a [`PreviewContainers`] that plays a container's part
/// without Docker, so the preview lifecycle (ports, readiness, the install
/// step, stop, the HTTP routes) is testable on any machine. `create`
/// records the spec; `start` runs its script with `sh` in the workspace,
/// with exactly the spec's environment plus `PATH` (an image has its own)
/// and the heartbeat folder at its host path (as the mount would show it),
/// in its own process group under a watchdog that ends it with the test
/// process. Production never runs a preview on the host: [`Docker`] is the
/// only production implementation, and the shape a real container gets is
/// asserted on [`docker_create_args`] directly.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct HostEmulator {
    /// Every spec it was asked to create.
    pub specs: Mutex<Vec<ContainerSpec>>,
    /// Every container it was asked to remove.
    pub removed: Mutex<Vec<String>>,
    /// Set: resolving the image fails with this.
    pub image_error: Mutex<Option<String>>,
    /// Set: the image id resolving answers (default `emulated-runtime-image`).
    pub image_id: Mutex<Option<String>>,
    /// How long `create` takes (a stop can land meanwhile).
    pub create_delay: Mutex<Duration>,
    /// How long `start` waits before starting (a stop can land between
    /// create and start).
    pub start_delay: Mutex<Duration>,
    /// Set: `start`'s child (the docker CLI) exits at once while the
    /// container runs on.
    pub cli_exits_early: Mutex<bool>,
    /// What `listeners` answers.
    pub proc_net: Mutex<Option<String>>,
    created: Mutex<HashMap<String, ContainerSpec>>,
    groups: Mutex<HashMap<String, i32>>,
}

#[cfg(test)]
impl PreviewContainers for HostEmulator {
    fn image_and_user(&self) -> Result<(String, Option<String>), String> {
        match self.image_error.lock().unwrap().clone() {
            Some(e) => Err(e),
            None => {
                let image = self.image_id.lock().unwrap().clone();
                Ok((
                    image.unwrap_or_else(|| "emulated-runtime-image".to_string()),
                    None,
                ))
            }
        }
    }

    fn host_labels(&self) -> Vec<String> {
        vec![format!("darkmux.dispatch.host-pid={}", std::process::id())]
    }

    fn create(&self, spec: &ContainerSpec) -> Result<(), String> {
        std::thread::sleep(*self.create_delay.lock().unwrap());
        self.specs.lock().unwrap().push(spec.clone());
        self.created
            .lock()
            .unwrap()
            .insert(spec.name.clone(), spec.clone());
        Ok(())
    }

    fn start(&self, name: &str, log: &Path) -> Result<std::process::Child, String> {
        use std::os::unix::process::CommandExt;
        std::thread::sleep(*self.start_delay.lock().unwrap());
        let Some(spec) = self.created.lock().unwrap().get(name).cloned() else {
            return Err(format!(
                "Error response from daemon: No such container: {name}"
            ));
        };
        let out = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(log)
            .map_err(|e| e.to_string())?;
        let err = out.try_clone().map_err(|e| e.to_string())?;
        let script = spec
            .script
            .replace(HEARTBEAT_AT, &spec.heartbeat.display().to_string());
        let watchdog = format!(
            "trap 'kill 0' EXIT; ( {script} ) & c=$!; while kill -0 {} 2>/dev/null && kill -0 $c 2>/dev/null; do sleep 1; done; \
             if kill -0 $c 2>/dev/null; then exit 0; fi; wait $c",
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
        let container = cmd.spawn().map_err(|e| e.to_string())?;
        self.groups
            .lock()
            .unwrap()
            .insert(name.to_string(), container.id() as i32);
        if *self.cli_exits_early.lock().unwrap() {
            // The container's process is reaped by the group kill in `remove`.
            std::mem::forget(container);
            return std::process::Command::new("true")
                .spawn()
                .map_err(|e| e.to_string());
        }
        Ok(container)
    }

    fn listeners(&self, _name: &str) -> Option<String> {
        self.proc_net.lock().unwrap().clone()
    }

    fn remove(&self, name: &str) {
        self.removed.lock().unwrap().push(name.to_string());
        self.created.lock().unwrap().remove(name);
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

    /// Whether the container `name` exists (created, not yet removed).
    pub fn exists(&self, name: &str) -> bool {
        self.created.lock().unwrap().contains_key(name)
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
        for bad in ["", "/etc/passwd", "../.env", "a/../../b", "./x", "node_modules/", "node_modules/.", "a//b", "a/./b", "."] {
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
            run: "r1".into(),
            workdir: PathBuf::from("/tmp/ws/site"),
            checkout: Some(PathBuf::from("/tmp/mine")),
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
        let args = docker_create_args(&a_spec(&recipe, None));
        assert_eq!(
            args[..5],
            [
                "create",
                "--rm",
                "--init",
                "--name",
                "darkmux-preview-site-4100-r1"
            ]
        );
        assert_eq!(
            values_of(&args, "-v"),
            [
                "/tmp/ws/site:/workspace",
                "/tmp/p/heartbeats/darkmux-preview-site-4100-r1:/darkmux-heartbeat:ro"
            ],
            "the workspace, and the heartbeat folder read-only"
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
        let args = docker_create_args(&a_spec(&recipe, None));
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
        let args = docker_create_args(&a_spec(&recipe, Some("501:20".into())));
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
        let beat = tempfile::NamedTempFile::new().unwrap();
        let watch = a_watch(beat.path(), 600);
        let not_due = container_script(&recipe, false, &watch);
        assert!(!not_due.contains("installing"), "{not_due}");
        // Waited on, not `output()`: the watchdog in the background keeps
        // the pipe open until it sees the script gone (in a container, the
        // script's exit ends the container).
        let log = tempfile::NamedTempFile::new().unwrap();
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(container_script(&recipe, true, &watch))
            .stdout(log.reopen().unwrap())
            .status()
            .unwrap();
        let text = std::fs::read_to_string(log.path()).unwrap();
        assert_eq!(status.code(), Some(4), "{text}");
        assert!(text.starts_with(STARTED_LINE), "{text}");
        assert!(
            text.contains(&format!("{INSTALL_FAILED_LINE} 4")) && !text.contains("serving"),
            "{text}"
        );
    }

    /// A watchdog on a host file, looking every 0.1 s.
    fn a_watch(file: &Path, missed: u32) -> Watch {
        Watch {
            file: file.display().to_string(),
            missed,
            every: "0.1".to_string(),
            grace: "0.5".to_string(),
        }
    }

    /// Sets `file`'s mtime `secs` seconds into the past.
    fn age(file: &Path, secs: u64) {
        let f = std::fs::OpenOptions::new().write(true).open(file).unwrap();
        f.set_modified(std::time::SystemTime::now() - Duration::from_secs(secs))
            .unwrap();
    }

    /// Sets `file`'s mtime to `secs` seconds after a fixed point a day ago.
    fn beat_at(file: &Path, secs: u64) {
        let day_ago = std::time::SystemTime::now() - Duration::from_secs(86_400);
        let f = std::fs::OpenOptions::new().write(true).open(file).unwrap();
        f.set_modified(day_ago + Duration::from_secs(secs)).unwrap();
    }

    /// (#3456 review F2, the old `a_preview_ends_when_darkmux_does`) A
    /// preview container stops itself once darkmux stops beating: its script
    /// runs while the heartbeat keeps changing, and ends, saying why, once it
    /// has not changed for `missed` looks. (#3456 review B) It never compares
    /// the heartbeat with the clock: these beats are a day behind it, as
    /// they would look after the VM's clock jumped on wake, and still count.
    /// Driven directly, looking every 0.1 s.
    #[cfg(unix)]
    #[test]
    fn a_preview_ends_when_darkmux_does() {
        use std::os::unix::process::CommandExt;
        let beat = tempfile::NamedTempFile::new().unwrap();
        beat_at(beat.path(), 0);
        let recipe = a_recipe(serde_json::json!({"schema_version": "1.0", "command": "sleep 30"}));
        let out = tempfile::NamedTempFile::new().unwrap();
        let mut sh = std::process::Command::new("sh")
            .arg("-c")
            .arg(container_script(&recipe, false, &a_watch(beat.path(), 5)))
            .stdout(out.reopen().unwrap())
            .process_group(0)
            .spawn()
            .unwrap();
        let group = sh.id() as i32;
        // A beat every 0.3 s, the looks every 0.1 s: two or so unchanged looks
        // between beats, under the 5 allowed only because a change resets
        // the count.
        for i in 1..=12 {
            beat_at(beat.path(), i);
            std::thread::sleep(Duration::from_millis(300));
        }
        let alive = sh.try_wait().unwrap().is_none();
        let mut ended = None;
        if alive {
            for _ in 0..40 {
                ended = sh.try_wait().unwrap();
                if ended.is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        unsafe { libc::kill(-group, libc::SIGKILL) };
        assert!(
            alive,
            "a changing heartbeat keeps it running, however old its mtime"
        );
        assert!(
            ended.is_some(),
            "an unchanging heartbeat ends the container's script"
        );
        let said = std::fs::read_to_string(out.path()).unwrap();
        assert!(said.contains(STALE_LINE), "{said}");
    }

    /// A stale container's script ends even when it ignores TERM: the
    /// watchdog follows the TERM with KILL. Driven directly: `sh` started
    /// with TERM ignored, which a shell cannot undo.
    #[cfg(unix)]
    #[test]
    fn a_stale_preview_that_ignores_term_is_killed() {
        use std::os::unix::process::CommandExt;
        let beat = tempfile::NamedTempFile::new().unwrap();
        beat_at(beat.path(), 0);
        let recipe = a_recipe(serde_json::json!({"schema_version": "1.0", "command": "sleep 30"}));
        let out = tempfile::NamedTempFile::new().unwrap();
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c")
            .arg(container_script(&recipe, false, &a_watch(beat.path(), 3)))
            .stdout(out.reopen().unwrap())
            .process_group(0);
        // SAFETY: only async-signal-safe `signal` between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
                Ok(())
            })
        };
        let mut sh = cmd.spawn().unwrap();
        let group = sh.id() as i32;
        let mut ended = None;
        for _ in 0..60 {
            ended = sh.try_wait().unwrap();
            if ended.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        unsafe { libc::kill(-group, libc::SIGKILL) };
        let said = std::fs::read_to_string(out.path()).unwrap();
        assert!(said.contains(STALE_LINE), "the watchdog saw it stale: {said}");
        assert!(ended.is_some(), "a script that ignores TERM is still ended");
    }

    /// (#3456 review F2) `beat` touches every live preview's heartbeat, the
    /// file the container sees through its read-only mount.
    #[test]
    fn beat_touches_each_live_previews_heartbeat() {
        let dir = tempfile::tempdir().unwrap();
        let (previews, emu, name) = a_live_preview(dir.path(), frozen_clock().1);
        let spec = emu.specs.lock().unwrap()[0].clone();
        assert_eq!(spec.name, name);
        let file = spec.heartbeat.join(HEARTBEAT_FILE);
        age(&file, 600);
        previews.beat();
        let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
        assert!(
            modified.elapsed().unwrap() < Duration::from_secs(5),
            "touched"
        );
        previews.stop("w");
    }

    /// (#3456 review F2) `client serve`'s heartbeat thread beats on its own
    /// and ends when told.
    #[test]
    fn the_heartbeat_thread_beats() {
        let dir = tempfile::tempdir().unwrap();
        let (previews, emu, _name) = a_live_preview(dir.path(), frozen_clock().1);
        let file = emu.specs.lock().unwrap()[0].heartbeat.join(HEARTBEAT_FILE);
        age(&file, 600);
        let previews = Arc::new(previews);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = spawn_heartbeat(previews.clone(), Duration::from_millis(20), stop.clone());
        let mut fresh = false;
        for _ in 0..100 {
            let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
            if modified.elapsed().unwrap() < Duration::from_secs(5) {
                fresh = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        thread.join().unwrap();
        assert!(fresh, "the thread touched the heartbeat");
        previews.stop("w");
    }

    /// (#3456 review F1) A stop that lands while the container is being
    /// created removes it once it exists; nothing ever runs.
    #[test]
    fn a_stop_while_the_container_is_created_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let (previews, emu) = emulated(dir.path());
        *emu.create_delay.lock().unwrap() = Duration::from_millis(300);
        let wt = a_workspace(dir.path(), "touch ran; sleep 30");
        previews
            .start("w", &wt, &RecipeSource::Checkout(wt.clone()))
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert!(previews.stop("w"));
        let name = until_removed(&emu);
        assert!(
            !emu.exists(&name) && !emu.running(&name),
            "no container is left"
        );
        std::thread::sleep(Duration::from_millis(200));
        assert!(!wt.join("ran").exists(), "nothing ran");
    }

    /// (#3456 review F1) A stop between create and start removes the
    /// created container, and the start then finds none to run.
    #[test]
    fn a_stop_between_create_and_start_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let (previews, emu) = emulated(dir.path());
        *emu.start_delay.lock().unwrap() = Duration::from_millis(300);
        let wt = a_workspace(dir.path(), "touch ran; sleep 30");
        previews
            .start("w", &wt, &RecipeSource::Checkout(wt.clone()))
            .unwrap();
        for _ in 0..100 {
            if previews.entries.lock().unwrap()["w"]
                .lock()
                .unwrap()
                .container
                .is_some()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(previews.stop("w"));
        let name = until_removed(&emu);
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            !emu.exists(&name) && !emu.running(&name),
            "no container is left"
        );
        assert!(!wt.join("ran").exists(), "nothing ran");
        assert_eq!(previews.status("w").unwrap().status, "stopped");
    }

    /// (#3456 review) When the docker CLI exits while its container runs on,
    /// the preview fails and the container is removed by name.
    #[test]
    fn a_container_whose_cli_exits_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let (previews, emu) = emulated(dir.path());
        *emu.cli_exits_early.lock().unwrap() = true;
        let wt = a_workspace(dir.path(), "sleep 30");
        previews
            .start("w", &wt, &RecipeSource::Checkout(wt.clone()))
            .unwrap();
        let v = until_failed(&previews, "w");
        assert_eq!(v["status"], "failed", "{v}");
        let name = until_removed(&emu);
        assert!(
            !emu.exists(&name) && !emu.running(&name),
            "the container was left running"
        );
    }

    /// (#3484) A workspace (in its own base folder) whose recipe installs
    /// with `echo installed >> installs.txt` unless `node_modules` exists,
    /// then serves with `sleep 30`; it has `node_modules` and a lockfile.
    fn an_installed_workspace(dir: &Path, install: &str) -> PathBuf {
        let wt = dir.join("base").join("w");
        std::fs::create_dir_all(wt.join(".darkmux")).unwrap();
        std::fs::create_dir_all(wt.join("node_modules")).unwrap();
        std::fs::write(wt.join("package-lock.json"), "{\"v\": 1}").unwrap();
        let recipe = serde_json::json!({
            "schema_version": "1.0", "command": "sleep 30",
            "install": {"command": install, "unless_exists": "node_modules"}
        });
        std::fs::write(wt.join(".darkmux/preview.json"), recipe.to_string()).unwrap();
        wt
    }

    /// Starts the preview, waits for the dev server line (the install, when
    /// it ran, succeeded), stops it, and says whether the install ran.
    fn start_once(previews: &Previews, emu: &HostEmulator, wt: &Path, log: &Path) -> bool {
        let _ = std::fs::remove_file(log);
        let before = emu.specs.lock().unwrap().len();
        previews
            .start("w", wt, &RecipeSource::Checkout(wt.to_path_buf()))
            .unwrap();
        for _ in 0..200 {
            let started = emu.specs.lock().unwrap().len() > before;
            if started && log_has(log, SERVING_LINE) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        std::thread::sleep(Duration::from_millis(400));
        previews.stop("w");
        let spec = emu.specs.lock().unwrap().last().unwrap().clone();
        spec.script.contains("installs.txt")
    }

    /// (#3484) The install reruns when its inputs change, even though
    /// `node_modules` exists: a changed lockfile, and a changed runtime image.
    /// An unchanged fingerprint skips it (the `unless_exists` fast path).
    #[test]
    fn a_changed_lockfile_or_image_reinstalls_and_an_unchanged_one_skips() {
        let dir = tempfile::tempdir().unwrap();
        let wt = an_installed_workspace(dir.path(), "echo installed >> installs.txt");
        let (previews, emu) = emulated(dir.path());
        let log = dir.path().join("p/w.log");
        assert!(
            start_once(&previews, &emu, &wt, &log),
            "no record yet: install"
        );
        assert!(
            !start_once(&previews, &emu, &wt, &log),
            "unchanged: skipped"
        );
        std::fs::write(wt.join("package-lock.json"), "{\"v\": 2}").unwrap();
        assert!(
            start_once(&previews, &emu, &wt, &log),
            "a changed lockfile reinstalls"
        );
        assert!(!start_once(&previews, &emu, &wt, &log), "recorded again");
        *emu.image_id.lock().unwrap() = Some("sha256:node22".into());
        assert!(
            start_once(&previews, &emu, &wt, &log),
            "a changed image reinstalls"
        );
        assert!(!start_once(&previews, &emu, &wt, &log), "and is recorded");
        let record = std::fs::read_to_string(dir.path().join("base/.w.install")).unwrap();
        assert_eq!(
            record.trim(),
            install_fingerprint(&wt, "echo installed >> installs.txt", "sha256:node22")
        );
    }

    /// (#3484) `clean` forgets the install record and deletes the
    /// `unless_exists` path, and nothing else in the workspace.
    #[test]
    fn clean_forgets_the_record_and_deletes_only_the_marker_path() {
        let dir = tempfile::tempdir().unwrap();
        let wt = an_installed_workspace(dir.path(), "echo installed >> installs.txt");
        let (previews, emu) = emulated(dir.path());
        let log = dir.path().join("p/w.log");
        assert!(start_once(&previews, &emu, &wt, &log));
        let record = dir.path().join("base/.w.install");
        assert!(record.exists(), "recorded");
        // The recipe comes from a checkout of the user's, never the workspace.
        let mine = dir.path().join("mine");
        std::fs::create_dir_all(mine.join(".darkmux")).unwrap();
        std::fs::copy(
            wt.join(".darkmux/preview.json"),
            mine.join(".darkmux/preview.json"),
        )
        .unwrap();
        let refused = previews.clean("w", &wt, &RecipeSource::Checkout(wt.clone()));
        assert!(refused.is_err(), "never cleans the user's own checkout");
        previews
            .clean("w", &wt, &RecipeSource::Checkout(mine))
            .unwrap();
        assert!(!record.exists(), "the record is forgotten");
        assert!(
            !wt.join("node_modules").exists(),
            "the marker path is deleted"
        );
        assert!(wt.join("package-lock.json").exists() && wt.join(".darkmux/preview.json").exists());
    }

    /// The spec of the emulated container whose name starts `prefix`, the
    /// newest first.
    fn wait_container(emu: &HostEmulator, count: usize) -> ContainerSpec {
        for _ in 0..200 {
            if let Some(spec) = emu.specs.lock().unwrap().get(count - 1) {
                return spec.clone();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("container {count} never created");
    }

    /// (#3485) Two starts back to back (here from two threads at once) give
    /// one live preview: the second joins the first and reports it, and only
    /// one container is created.
    #[test]
    fn back_to_back_starts_give_one_live_preview() {
        let dir = tempfile::tempdir().unwrap();
        let wt = a_workspace(dir.path(), "sleep 30");
        let (previews, emu) = emulated(dir.path());
        let previews = Arc::new(previews);
        let starts: Vec<_> = (0..4)
            .map(|_| {
                let (p, wt) = (previews.clone(), wt.clone());
                std::thread::spawn(move || {
                    p.start("w", &wt, &RecipeSource::Checkout(wt.clone()))
                        .unwrap()
                })
            })
            .collect();
        let replies: Vec<(PreviewState, bool)> =
            starts.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(
            replies.iter().filter(|(_, joined)| !joined).count(),
            1,
            "one new run"
        );
        assert!(
            replies.iter().all(|(v, _)| v.port == replies[0].0.port),
            "all report the same run"
        );
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(emu.specs.lock().unwrap().len(), 1, "one container");
        previews.stop("w");
    }

    /// (#3485) Each preview run has its own heartbeat folder, even when two
    /// runs of one workspace get the same port (as a stop then a start can):
    /// run A's end removes only A's folder, and run B keeps beating.
    #[test]
    fn a_runs_end_removes_only_its_own_heartbeat() {
        let dir = tempfile::tempdir().unwrap();
        let wt = a_workspace(dir.path(), "sleep 30");
        let (previews, emu) = emulated(dir.path());
        let previews = previews.with_ports(Arc::new(|| Ok(45_454)));
        let src = RecipeSource::Checkout(wt.clone());
        previews.start("w", &wt, &src).unwrap();
        let a = wait_container(&emu, 1);
        previews.stop("w");
        previews.start("w", &wt, &src).unwrap();
        let b = wait_container(&emu, 2);
        assert_ne!(a.heartbeat, b.heartbeat, "a heartbeat folder per run");
        assert_ne!(a.name, b.name, "a container name per run");
        // Run A's thread ends after its container does, and cleans up.
        std::thread::sleep(Duration::from_millis(1500));
        assert!(!a.heartbeat.exists(), "A's folder is removed");
        let beat = b.heartbeat.join(HEARTBEAT_FILE);
        assert!(beat.exists(), "B's folder survives A's end");
        age(&beat, 600);
        previews.beat();
        let modified = std::fs::metadata(&beat).unwrap().modified().unwrap();
        assert!(
            modified.elapsed().unwrap() < Duration::from_secs(5),
            "B keeps beating"
        );
        previews.stop("w");
    }

    /// (#3484) A failed install records nothing, so the next start retries.
    #[test]
    fn a_failed_install_leaves_no_record() {
        let dir = tempfile::tempdir().unwrap();
        let wt = an_installed_workspace(dir.path(), "exit 4");
        let (previews, _emu) = emulated(dir.path());
        previews
            .start("w", &wt, &RecipeSource::Checkout(wt.clone()))
            .unwrap();
        let v = until_failed(&previews, "w");
        assert_eq!(v["status"], "failed", "{v}");
        assert!(
            !dir.path().join("base/.w.install").exists(),
            "no record of a failed install"
        );
    }

    /// (#3484) The fingerprint covers the lockfiles that exist, else
    /// `package.json`, the install command and the image. A lockfile that is
    /// a symlink is hashed as its link text, never read: what it points at
    /// outside the workspace changes nothing.
    #[cfg(unix)]
    #[test]
    fn the_fingerprint_never_reads_through_a_symlinked_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().join("w");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join("package.json"), "{}").unwrap();
        let fp = |wt: &Path| install_fingerprint(wt, "npm ci", "img");
        let bare = fp(&wt);
        std::fs::write(wt.join("package.json"), "{\"x\": 1}").unwrap();
        assert_ne!(fp(&wt), bare, "with no lockfile, package.json counts");
        assert_ne!(
            install_fingerprint(&wt, "npm install", "img"),
            fp(&wt),
            "the command counts"
        );
        assert_ne!(
            install_fingerprint(&wt, "npm ci", "img2"),
            fp(&wt),
            "the image counts"
        );
        let outside = dir.path().join("outside.lock");
        std::fs::write(&outside, "secret one").unwrap();
        std::os::unix::fs::symlink(&outside, wt.join("yarn.lock")).unwrap();
        let linked = fp(&wt);
        std::fs::write(&outside, "secret two, changed").unwrap();
        assert_eq!(fp(&wt), linked, "the link's target is never read");
        std::fs::remove_file(wt.join("yarn.lock")).unwrap();
        std::fs::write(wt.join("yarn.lock"), "secret two, changed").unwrap();
        assert_ne!(fp(&wt), linked, "a link is not the file it points at");
    }

    /// The fingerprint reads `package.json` beside the lockfiles (a run can
    /// add a dependency there before any lockfile changes), and Bun's
    /// lockfiles count as lockfiles.
    #[test]
    fn the_fingerprint_reads_package_json_beside_a_lockfile_and_bun_lockfiles() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().join("w");
        std::fs::create_dir_all(&wt).unwrap();
        let fp = |wt: &Path| install_fingerprint(wt, "npm ci", "img");
        std::fs::write(wt.join("package.json"), "{}").unwrap();
        std::fs::write(wt.join("package-lock.json"), "{}").unwrap();
        let locked = fp(&wt);
        std::fs::write(wt.join("package.json"), "{\"dependencies\": {\"left-pad\": \"1\"}}").unwrap();
        assert_ne!(fp(&wt), locked, "a changed package.json counts beside a lockfile");
        for bun in ["bun.lock", "bun.lockb"] {
            std::fs::write(wt.join(bun), "one").unwrap();
            let before = fp(&wt);
            std::fs::write(wt.join(bun), "two").unwrap();
            assert_ne!(fp(&wt), before, "{bun} counts");
        }
    }

    /// A clean restart whose `unless_exists` ends in `/` or `.` never
    /// deletes through a symlink the run planted there: the recipe is
    /// refused, and what the link points at outside the workspace is left.
    #[cfg(unix)]
    #[test]
    fn clean_never_deletes_through_a_planted_link_named_with_a_trailing_slash() {
        for unless in ["node_modules/", "node_modules/."] {
            let dir = tempfile::tempdir().unwrap();
            let (wt, mine, outside) = (dir.path().join("w"), dir.path().join("mine"), dir.path().join("outside"));
            for d in [&wt, &mine.join(".darkmux"), &outside] {
                std::fs::create_dir_all(d).unwrap();
            }
            std::fs::write(outside.join("precious.txt"), "mine").unwrap();
            std::os::unix::fs::symlink(&outside, wt.join("node_modules")).unwrap();
            let recipe = serde_json::json!({
                "schema_version": "1.0", "command": "x", "install": {"command": "i", "unless_exists": unless}
            });
            std::fs::write(mine.join(".darkmux/preview.json"), recipe.to_string()).unwrap();
            let (previews, _emu) = emulated(dir.path());
            let out = previews.clean("w", &wt, &RecipeSource::Checkout(mine));
            assert!(outside.join("precious.txt").exists(), "{unless}: deleted outside the workspace through the link");
            assert!(out.is_err(), "{unless}: the recipe is refused");
        }
    }

    /// Deleting a path inside the workspace refuses one under a folder the
    /// run replaced with a link, and says it is a delete.
    #[cfg(unix)]
    #[test]
    fn remove_inside_refuses_a_path_under_a_symlinked_folder() {
        let dir = tempfile::tempdir().unwrap();
        let (wt, outside) = (dir.path().join("w"), dir.path().join("outside"));
        std::fs::create_dir_all(outside.join("deps")).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(outside.join("deps/precious.txt"), "mine").unwrap();
        std::os::unix::fs::symlink(&outside, wt.join("web")).unwrap();
        let err = remove_inside(&wt, "web/deps").unwrap_err();
        assert!(outside.join("deps/precious.txt").exists(), "deleted through the symlinked folder");
        assert!(err.contains("delete") && !err.contains("copy"), "{err}");
    }

    /// `env_from_source` never copies through a link in the user's own
    /// checkout (one a run could have planted while it had the checkout
    /// mounted): a `.env` that links to a key, or a folder that links out,
    /// is refused, and nothing lands in the workspace.
    #[cfg(unix)]
    #[test]
    fn env_from_source_never_reads_through_a_link_in_the_checkout() {
        for (file, link_at) in [(".env", ".env"), ("config/.env", "config")] {
            let dir = tempfile::tempdir().unwrap();
            let (wt, checkout, secrets) = (dir.path().join("w"), dir.path().join("mine"), dir.path().join("secrets"));
            for d in [&wt, &checkout.join(".darkmux"), &secrets] {
                std::fs::create_dir_all(d).unwrap();
            }
            std::fs::write(secrets.join(".env"), "PRIVATE KEY\n").unwrap();
            let target = match link_at {
                ".env" => secrets.join(".env"),
                _ => secrets.clone(),
            };
            std::os::unix::fs::symlink(&target, checkout.join(link_at)).unwrap();
            let recipe = serde_json::json!({
                "schema_version": "1.0", "command": "touch server-ran; exit 3", "env_from_source": [file]
            });
            std::fs::write(checkout.join(".darkmux/preview.json"), recipe.to_string()).unwrap();
            let (previews, _emu) = emulated(dir.path());
            previews.start("w", &wt, &RecipeSource::Checkout(checkout.clone())).unwrap();
            let v = until_failed(&previews, "w");
            assert!(!wt.join(file).exists(), "{file}: copied through the checkout's link");
            assert_eq!(v["status"], "failed", "{file}: {v}");
            assert!(v["error"].as_str().unwrap().contains("symlink"), "{file}: {v}");
        }
    }

    /// A workspace whose own recipe runs `command`.
    fn a_workspace(dir: &Path, command: &str) -> PathBuf {
        let wt = dir.join("w");
        std::fs::create_dir_all(wt.join(".darkmux")).unwrap();
        let recipe = serde_json::json!({"schema_version": "1.0", "command": command});
        std::fs::write(wt.join(".darkmux/preview.json"), recipe.to_string()).unwrap();
        wt
    }

    /// Waits for the emulator to have removed a container; returns its name.
    fn until_removed(emu: &HostEmulator) -> String {
        for _ in 0..200 {
            if let Some(name) = emu.removed.lock().unwrap().first() {
                return name.clone();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("nothing was removed");
    }

    /// (#3456 review F5) A dev server that never answered names why when
    /// the container's `/proc/net/tcp` shows it listening on loopback only
    /// (recorded from a Vite dev server with `HOST: 127.0.0.1`, port 5173 =
    /// 0x1435), and not when it also listens on every address.
    #[test]
    fn a_dev_server_listening_on_loopback_only_is_named() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:1435 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 61232 1 0000000000000000 100 0 0 10 0\n   1: 0B00007F:A5F1 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 59311 1 0000000000000000 100 0 0 10 0\n";
        let why = not_ready_reason(5173, Some(tcp));
        assert!(
            why.starts_with("the dev server listens on 127.0.0.1 inside its container; set it to bind 0.0.0.0 (the port is published on this Mac's loopback only)"),
            "{why}"
        );
        let v6 = "   0: 00000000000000000000000001000000:1435 00000000000000000000000000000000:0000 0A 0 0 0 0 0 0";
        assert!(listens_on_loopback_only(v6, 5173), "::1");
        let any = format!("{tcp}   2: 00000000:1435 00000000:0000 0A 0 0 0 0 0 0\n");
        assert!(
            !listens_on_loopback_only(&any, 5173),
            "also on 0.0.0.0: reachable"
        );
        assert!(!listens_on_loopback_only(tcp, 3000), "another port");
        let established = tcp.replace(" 0A ", " 01 ");
        assert!(
            !listens_on_loopback_only(&established, 5173),
            "not a listener"
        );
        assert!(not_ready_reason(5173, None).contains("didn't answer"));
        assert!(not_ready_reason(3000, Some(tcp)).contains("didn't answer"));
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
        previews
            .start("w", &wt, &RecipeSource::Checkout(wt.clone()))
            .unwrap();
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
        previews
            .start("w", &wt, &RecipeSource::Checkout(checkout.clone()))
            .unwrap();
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
        previews
            .start("w", &wt, &RecipeSource::Checkout(wt.clone()))
            .unwrap();
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
        previews
            .start("w", &wt, &RecipeSource::Checkout(wt.clone()))
            .unwrap();
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
        let (_, running) = previews
            .start("w", &wt, &RecipeSource::Checkout(wt.clone()))
            .unwrap();
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
    fn a_preview_runs_in_the_runtime_image_with_no_host_files_or_host_env() {
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
        let (state, _) = previews
            .start("ws", &wt, &RecipeSource::Checkout(wt.clone()))
            .unwrap();
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

    /// `git <args>` in `dir`, with an identity, asserting it succeeded.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repository whose one commit holds `files` (path, text); returns its
    /// directory and the commit.
    fn a_repo_with(dir: &Path, files: &[(&str, &str)]) -> (PathBuf, String) {
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--quiet"]);
        for (path, text) in files {
            let at = repo.join(path);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::write(at, text).unwrap();
        }
        git(&repo, &["add", "-A"]);
        git(
            &repo,
            &["commit", "--quiet", "--allow-empty", "-m", "first"],
        );
        let commit = git(&repo, &["rev-parse", "HEAD"]);
        (repo, commit)
    }

    fn at_commit(repo: &Path, commit: &str) -> RecipeSource {
        RecipeSource::TemplateCommit {
            git_dir: repo.join(".git"),
            commit: commit.to_string(),
        }
    }

    const TEMPLATE_RECIPE: &str =
        r#"{"schema_version": "1.0", "command": "npm run dev", "env": {"PORT": "{port}"}}"#;

    /// (#3460) A template project's recipe is the one in its first commit:
    /// an edit in the work tree, committed or not, changes nothing.
    #[test]
    fn a_template_recipe_is_read_from_the_first_commit_only() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, first) = a_repo_with(dir.path(), &[(RECIPE_PATH, TEMPLATE_RECIPE)]);
        let edited = r#"{"schema_version": "1.0", "command": "touch pwned"}"#;
        std::fs::write(repo.join(RECIPE_PATH), edited).unwrap();
        assert_eq!(
            at_commit(&repo, &first).load().unwrap().command,
            "npm run dev",
            "uncommitted edit"
        );
        git(&repo, &["commit", "--quiet", "-am", "a run"]);
        assert_eq!(
            at_commit(&repo, &first).load().unwrap().command,
            "npm run dev",
            "committed edit"
        );
        assert_eq!(
            read_recipe_at_commit(&repo.join(".git"), &first).unwrap(),
            TEMPLATE_RECIPE
        );
    }

    /// (#3460) The first commit's recipe must be a regular file within the
    /// size cap, there at all, and named by a full commit SHA.
    #[cfg(unix)]
    #[test]
    fn a_template_recipe_that_is_a_link_or_too_large_or_missing_is_refused() {
        let at = |files: &[(&str, &str)], link: Option<(&str, &str)>| {
            let dir = tempfile::tempdir().unwrap();
            let (repo, _) = a_repo_with(dir.path(), files);
            if let Some((from, to)) = link {
                std::fs::create_dir_all(repo.join(from).parent().unwrap()).unwrap();
                std::os::unix::fs::symlink(to, repo.join(from)).unwrap();
                git(&repo, &["add", "-A"]);
                git(&repo, &["commit", "--quiet", "-m", "link"]);
            }
            let commit = git(&repo, &["rev-parse", "HEAD"]);
            (dir, read_recipe_at_commit(&repo.join(".git"), &commit))
        };
        let (_d, linked) = at(
            &[("real.json", TEMPLATE_RECIPE)],
            Some((RECIPE_PATH, "../real.json")),
        );
        assert!(
            matches!(linked, Err(RecipeRefusal::Symlink(_))),
            "{linked:?}"
        );
        let (_d, linked_dir) = at(
            &[("real/preview.json", TEMPLATE_RECIPE)],
            Some((".darkmux", "real")),
        );
        assert!(
            matches!(linked_dir, Err(RecipeRefusal::Symlink(_))),
            "{linked_dir:?}"
        );
        let big = format!(
            r#"{{"schema_version": "1.0", "command": "{}"}}"#,
            "x".repeat(MAX_RECIPE_BYTES as usize)
        );
        let (_d, large) = at(&[(RECIPE_PATH, &big)], None);
        assert!(
            matches!(large, Err(RecipeRefusal::TooLarge(_))),
            "{large:?}"
        );
        let (_d, missing) = at(&[("README.md", "hi")], None);
        assert!(
            matches!(missing, Err(RecipeRefusal::NotFound(_))),
            "{missing:?}"
        );
        let (_d, dir_not_file) = at(&[(".darkmux/preview.json/x", "hi")], None);
        assert!(
            matches!(dir_not_file, Err(RecipeRefusal::Invalid(_))),
            "{dir_not_file:?}"
        );
        let dir = tempfile::tempdir().unwrap();
        let (repo, _) = a_repo_with(dir.path(), &[(RECIPE_PATH, TEMPLATE_RECIPE)]);
        let short = read_recipe_at_commit(&repo.join(".git"), "HEAD");
        assert!(
            matches!(short, Err(RecipeRefusal::Invalid(_))),
            "only a full SHA: {short:?}"
        );
    }

    /// The blob id of the recipe in `commit`.
    fn recipe_blob(repo: &Path, commit: &str) -> String {
        git(repo, &["rev-parse", &format!("{commit}:{RECIPE_PATH}")])
    }

    /// (#3460 review) A replace ref planted in the held repository changes
    /// nothing: the recipe read is the first commit's own.
    #[test]
    fn a_planted_replace_ref_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, first) = a_repo_with(dir.path(), &[(RECIPE_PATH, TEMPLATE_RECIPE)]);
        let forged = r#"{"schema_version": "1.0", "command": "touch pwned"}"#;
        std::fs::write(repo.join("forged.json"), forged).unwrap();
        let fake = git(&repo, &["hash-object", "-w", "forged.json"]);
        git(&repo, &["replace", &recipe_blob(&repo, &first), &fake]);
        assert_eq!(
            git(&repo, &["show", &format!("{first}:{RECIPE_PATH}")]),
            forged,
            "plain git honors it"
        );
        assert_eq!(
            read_recipe_at_commit(&repo.join(".git"), &first).unwrap(),
            TEMPLATE_RECIPE
        );
    }

    /// (#3460 review) An object overwritten in place (git does not check a
    /// loose object's hash when it reads it) is refused: every object on the
    /// recipe's path must hash to its id.
    #[test]
    fn an_overwritten_object_is_refused() {
        // Overwrites `which`'s object with a valid object of the same type
        // holding the forged recipe, so git reads it without complaint.
        let overwrite = |tree: bool| {
            let dir = tempfile::tempdir().unwrap();
            let (repo, first) = a_repo_with(dir.path(), &[(RECIPE_PATH, TEMPLATE_RECIPE)]);
            let forged_text = r#"{"schema_version": "1.0", "command": "touch pwned"}"#;
            std::fs::write(repo.join(RECIPE_PATH), forged_text).unwrap();
            git(&repo, &["add", RECIPE_PATH]);
            let forged_root = git(&repo, &["write-tree"]);
            let (victim, forged) = if tree {
                let at = |root: &str| git(&repo, &["rev-parse", &format!("{root}:.darkmux")]);
                (at(&first), at(&forged_root))
            } else {
                let at = |root: &str| git(&repo, &["rev-parse", &format!("{root}:{RECIPE_PATH}")]);
                (at(&first), at(&forged_root))
            };
            let object = |oid: &str| repo.join(".git/objects").join(&oid[..2]).join(&oid[2..]);
            let bytes = std::fs::read(object(&forged)).unwrap();
            let at = object(&victim);
            let mut perms = std::fs::metadata(&at).unwrap().permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            std::fs::set_permissions(&at, perms).unwrap();
            std::fs::write(&at, bytes).unwrap();
            let plain = git(&repo, &["show", &format!("{first}:{RECIPE_PATH}")]);
            assert_eq!(
                plain, forged_text,
                "plain git reads the forgery (tree={tree})"
            );
            read_recipe_at_commit(&repo.join(".git"), &first)
        };
        for tree in [false, true] {
            let read = overwrite(tree);
            assert!(
                matches!(read, Err(RecipeRefusal::Invalid(ref m)) if m.contains("does not match")),
                "tree={tree}: {read:?}"
            );
        }
    }

    /// (#3460) A template recipe is parsed as strictly as a checkout's (the
    /// Mux Bot template schema's keys are refused by name), and one that
    /// lists `env_from_source` files is refused naming the key: a template
    /// project has no checkout of the user's to copy them from.
    #[test]
    fn a_template_recipe_is_checked_like_a_checkouts_and_refuses_env_from_source() {
        let load = |text: &str| {
            let dir = tempfile::tempdir().unwrap();
            let (repo, first) = a_repo_with(dir.path(), &[(RECIPE_PATH, text)]);
            at_commit(&repo, &first).load()
        };
        let muxbot = r#"{"schema": 1, "command": "npm run dev", "pages": ["/"], "viewports": [], "conventions": {}, "page_file": "x"}"#;
        let e = load(muxbot).unwrap_err();
        assert!(matches!(e, RecipeRefusal::Invalid(_)), "{e:?}");
        let with_version = r#"{"schema_version": "1.0", "command": "npm run dev", "pages": ["/"]}"#;
        assert!(load(with_version)
            .unwrap_err()
            .message()
            .contains("unknown key `pages`"));
        let e = load(r#"{"schema_version": "1.0", "command": "x", "env_from_source": [".env"]}"#)
            .unwrap_err();
        assert!(
            matches!(e, RecipeRefusal::Invalid(_)) && e.message().contains("`env_from_source`"),
            "{e:?}"
        );
        assert!(
            load(r#"{"schema_version": "1.0", "command": "x", "env_from_source": []}"#).is_ok()
        );
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
