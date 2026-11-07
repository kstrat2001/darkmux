//! Client pairing (LOCAL PROTOTYPE): `darkmux client serve`.
//!
//! A third listener, beside the viewer daemon and the fleet listener and
//! sharing nothing with either, that lets an app on this machine (the Mux Bot
//! app) pair once and then run scoped work. It binds `127.0.0.1` ONLY
//! ([`bind_addr`]): never `0.0.0.0`, never the tailnet address.
//!
//! **Pairing.** The app `POST`s `/client/pair` with a device name, a code it
//! shows its user and a random `pair_secret` (a `scopes` field is accepted
//! and ignored: what an app may run is machine policy, not the app's ask). The
//! operator runs `darkmux client approve` here, sees the device, code and age,
//! and approves (or denies). A pairing records identity only. The app polls `/client/pair/{id}` with
//! `X-Pair-Secret`; on the first poll after approval this listener mints the
//! token (32 random bytes, hex), stores ONLY its BLAKE3 hash as an approved
//! client, deletes the pending record and returns the token. A later poll is
//! a 404: the token is delivered exactly once and is never on disk. A wrong
//! secret is a 404 too, so a request id alone reveals nothing. A request
//! expires ten minutes after it was made, approved or not.
//!
//! **Work.** `POST /client/work` with `Authorization: Bearer <token>` runs one
//! role execution through the existing dispatch path: this binary is spawned
//! as `darkmux dispatch <role> --profile <p> [--workdir <w>] --json --name
//! <work_id>` with the message on stdin ([`dispatch_args`]), so every dispatch
//! contract (the user-file preflight, liveness bookends, residency, the
//! crew-of-one run) holds unchanged. The profile is optional: absent, it
//! resolves exactly as `darkmux dispatch <role>` would (`role_profiles`, else
//! `default_profile`), and the resolved name is passed to the child explicitly
//! so what runs is what was checked. It must be defined here, must not route
//! to another machine (`<p>@<machine>`), and must be allowed by the machine's
//! `clients.profiles` setting, read fresh from `config.json` on every request
//! (unset: only the role's own resolved profile). A `workdir` must lie under
//! the worktrees base (`validate_remote_workdir`).
//! `GET /client/recipe?workdir=` reads `<workdir>/.darkmux/preview.json`
//! under the same containment, for an app that cannot read the disk itself.
//!
//! **State.** `~/.darkmux/clients.json` (0600), every read-modify-write under
//! an exclusive `flock` on the file itself, so `client approve` in one process
//! and this listener in another never lose a write. It is darkmux-written
//! state, but a hand edit is still checked with the user-file gate's key walk
//! (`darkmux_types::user_files::key_issues`): an unknown key refuses every
//! consumer, naming the file, the key path and the closest valid key. Work
//! status lives in memory only: a restart forgets running and finished work.

use axum::{
    body::Body,
    extract::{Path as UrlPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Seek, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};

/// The listener's default port (the viewer is 8765, the fleet listener 8766).
pub const DEFAULT_PORT: u16 = 8767;
/// A pairing request lives this long from when it was made, approved or not.
pub const PENDING_TTL_SECS: u64 = 600;
/// Expired and denied requests are kept this long past their TTL, so the app
/// can still hear `expired` / `denied`; after that a poll is a 404.
const KEEP_FINISHED_SECS: u64 = 3600;
/// Live (unexpired, undecided) requests at once; more is a 429.
pub const MAX_PENDING: usize = 16;
/// The largest request body any route reads.
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// Work a single client may have running at once.
const MAX_RUNNING_PER_CLIENT: usize = 2;
/// Finished work is remembered this long for polls.
const KEEP_WORK_SECS: u64 = 3600;
/// The `schema_version` this binary writes and reads.
///
/// 1.1: records hold identity only; the per-record `scopes` of 1.0 is
/// dropped when a 1.0 store is read (what a client may run is the machine's
/// `clients.profiles` setting).
pub const CLIENTS_SCHEMA_VERSION: &str = "1.1";

// ─── Store ───────────────────────────────────────────────────────────────

/// Where a pairing request stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PendingStatus {
    Pending,
    /// Approved by the operator; the token is minted when the app collects it.
    Approved,
    Denied,
}

/// One pairing request, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PendingRequest {
    pub request_id: String,
    pub device_name: String,
    pub code: String,
    /// BLAKE3 of the app's `pair_secret`, hex. The secret itself is never stored.
    pub pair_secret_hash: String,
    /// Unix seconds.
    pub created: u64,
    pub status: PendingStatus,
    /// The name the client will have, set at approval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
}

impl PendingRequest {
    pub fn expired(&self, now: u64) -> bool {
        now.saturating_sub(self.created) > PENDING_TTL_SECS
    }
    /// Offered to `client approve`: undecided and unexpired.
    pub fn live(&self, now: u64) -> bool {
        self.status == PendingStatus::Pending && !self.expired(now)
    }
}

/// One paired client, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ApprovedClient {
    pub name: String,
    /// Unix seconds.
    pub created: u64,
    /// BLAKE3 of the bearer token, hex. The token itself is never stored.
    pub token_hash: String,
}

/// `~/.darkmux/clients.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ClientsFile {
    pub schema_version: String,
    #[serde(default)]
    pub pending: Vec<PendingRequest>,
    #[serde(default)]
    pub clients: Vec<ApprovedClient>,
}

impl Default for ClientsFile {
    fn default() -> Self {
        Self {
            schema_version: CLIENTS_SCHEMA_VERSION.to_string(),
            pending: Vec::new(),
            clients: Vec::new(),
        }
    }
}

/// The client store at one path.
pub struct ClientStore {
    path: PathBuf,
}

impl ClientStore {
    /// `<darkmux root>/clients.json` (`DARKMUX_HOME` relocates the root).
    pub fn production() -> Self {
        let root =
            darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser).root;
        Self {
            path: root.join("clients.json"),
        }
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The store as it is now (absent reads as empty).
    pub fn read(&self) -> Result<ClientsFile> {
        self.update(|f| Ok(f.clone()))
    }

    /// Load, change, save, all under the file's exclusive lock. `f`'s error
    /// saves nothing.
    pub fn update<T>(&self, f: impl FnOnce(&mut ClientsFile) -> Result<T>) -> Result<T> {
        let mut guard = darkmux_types::flock::lock_exclusive(&self.path)?;
        let file = guard.file();
        let mut text = String::new();
        file.take(darkmux_types::user_files::MAX_USER_FILE_BYTES + 1)
            .read_to_string(&mut text)
            .with_context(|| format!("reading {}", self.path.display()))?;
        let mut doc = parse_clients(&self.path, &text)?;
        let before = doc.clone();
        let out = f(&mut doc)?;
        if doc != before {
            let bytes = serde_json::to_vec_pretty(&doc)?;
            let file = guard.file();
            file.set_len(0)?;
            file.rewind()?;
            file.write_all(&bytes)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
        }
        owner_only(&self.path)?;
        Ok(out)
    }
}

/// Parse the store's text: empty is an empty store; an unknown key, a wrong
/// type or a newer `schema_version` refuses, naming the file and the key.
fn parse_clients(path: &Path, text: &str) -> Result<ClientsFile> {
    if text.trim().is_empty() {
        return Ok(ClientsFile::default());
    }
    if text.len() as u64 > darkmux_types::user_files::MAX_USER_FILE_BYTES {
        bail!(
            "{} is larger than the {}-byte cap",
            path.display(),
            darkmux_types::user_files::MAX_USER_FILE_BYTES
        );
    }
    let mut doc: Value = serde_json::from_str(text)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    if let Some(v) = darkmux_types::data_version::newer(&doc, CLIENTS_SCHEMA_VERSION) {
        bail!("{} was written by a newer darkmux (schema_version {v}; this one reads {CLIENTS_SCHEMA_VERSION}). Upgrade darkmux.", path.display());
    }
    migrate_1_0(&mut doc);
    let issues = darkmux_types::user_files::key_issues::<ClientsFile>(
        &doc,
        &darkmux_types::user_files::no_retired,
    );
    if !issues.is_empty() {
        let lines: Vec<String> = issues.iter().map(ToString::to_string).collect();
        bail!("client store {}: {}", path.display(), lines.join("; "));
    }
    serde_json::from_value(doc).with_context(|| format!("reading {}", path.display()))
}

/// A 1.0 store carried `scopes` on every record; 1.1 holds identity only.
fn migrate_1_0(doc: &mut Value) {
    if doc.get("schema_version").and_then(Value::as_str) != Some("1.0") {
        return;
    }
    for list in ["pending", "clients"] {
        if let Some(items) = doc.get_mut(list).and_then(Value::as_array_mut) {
            for item in items.iter_mut().filter_map(Value::as_object_mut) {
                item.remove("scopes");
            }
        }
    }
    doc["schema_version"] = Value::from(CLIENTS_SCHEMA_VERSION);
}

/// Keep the store owner-only, also when it existed with a wider mode.
fn owner_only(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting {} to 0600", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Drop requests no poll can still use.
fn prune(f: &mut ClientsFile, now: u64) {
    f.pending
        .retain(|p| now.saturating_sub(p.created) <= PENDING_TTL_SECS + KEEP_FINISHED_SECS);
}

// ─── Secrets ─────────────────────────────────────────────────────────────

/// BLAKE3 of `s`, hex.
pub(crate) fn hash_hex(s: &str) -> String {
    blake3::hash(s.as_bytes()).to_hex().to_string()
}

/// Whether `presented` hashes to `stored_hex`. Compares `blake3::Hash`
/// values, whose `PartialEq` is constant-time; a stored value that is not a
/// hash matches nothing.
pub(crate) fn secret_matches(stored_hex: &str, presented: &str) -> bool {
    match blake3::Hash::from_hex(stored_hex) {
        Ok(stored) => stored == blake3::hash(presented.as_bytes()),
        Err(_) => false,
    }
}

/// `n` random bytes from the OS (`/dev/urandom`; no RNG crate is in the
/// workspace), hex-encoded.
pub(crate) fn random_hex(n: usize) -> Result<String> {
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .context("reading /dev/urandom")?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

// ─── CLI-side verbs ──────────────────────────────────────────────────────

/// Requests `client approve` offers: live ones, oldest first.
pub fn list_pending(store: &ClientStore, now: u64) -> Result<Vec<PendingRequest>> {
    let mut out: Vec<PendingRequest> = store
        .read()?
        .pending
        .into_iter()
        .filter(|p| p.live(now))
        .collect();
    out.sort_by_key(|p| p.created);
    Ok(out)
}

/// The one live request showing `code` (case-insensitive).
pub fn find_by_code(store: &ClientStore, code: &str, now: u64) -> Result<PendingRequest> {
    let found: Vec<PendingRequest> = list_pending(store, now)?
        .into_iter()
        .filter(|p| p.code.eq_ignore_ascii_case(code.trim()))
        .collect();
    match found.len() {
        0 => bail!("no live pairing request shows code {code}"),
        1 => Ok(found.into_iter().next().unwrap()),
        n => bail!(
            "{n} live pairing requests show code {code}; approve them one at a time without --code"
        ),
    }
}

/// Approve a live request; the token is minted when the app collects it.
/// Returns the client name it will have.
pub fn approve(store: &ClientStore, request_id: &str, now: u64) -> Result<String> {
    store.update(|f| {
        prune(f, now);
        let mut taken: Vec<String> = f.clients.iter().map(|c| c.name.clone()).collect();
        taken.extend(f.pending.iter().filter_map(|p| p.client_name.clone()));
        let p = f
            .pending
            .iter_mut()
            .find(|p| p.request_id == request_id)
            .ok_or_else(|| anyhow!("no pairing request {request_id}"))?;
        if !p.live(now) {
            bail!("pairing request {request_id} is no longer pending (expired or already decided)");
        }
        let name = client_name_for(&p.device_name, &taken);
        p.status = PendingStatus::Approved;
        p.client_name = Some(name.clone());
        Ok(name)
    })
}

/// Deny a live request.
pub fn deny(store: &ClientStore, request_id: &str, now: u64) -> Result<()> {
    store.update(|f| {
        let p = f
            .pending
            .iter_mut()
            .find(|p| p.request_id == request_id)
            .ok_or_else(|| anyhow!("no pairing request {request_id}"))?;
        if !p.live(now) {
            bail!("pairing request {request_id} is no longer pending (expired or already decided)");
        }
        p.status = PendingStatus::Denied;
        Ok(())
    })
}

pub fn list_clients(store: &ClientStore) -> Result<Vec<ApprovedClient>> {
    Ok(store.read()?.clients)
}

/// Remove a client by name; `false` when there is none.
pub fn revoke(store: &ClientStore, name: &str) -> Result<bool> {
    store.update(|f| {
        let before = f.clients.len();
        f.clients.retain(|c| c.name != name);
        Ok(f.clients.len() != before)
    })
}

/// A slug of the device name, unique against `taken`.
pub(crate) fn client_name_for(device: &str, taken: &[String]) -> String {
    let mut slug = String::new();
    for c in device.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let mut base: String = slug.trim_matches('-').chars().take(32).collect();
    base = base.trim_end_matches('-').to_string();
    if base.is_empty() {
        base = "client".to_string();
    }
    if !taken.contains(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|c| !taken.contains(c))
        .unwrap()
}

// ─── HTTP ────────────────────────────────────────────────────────────────

/// One work request as the executor receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkSpec {
    pub work_id: String,
    pub role: String,
    pub profile: String,
    /// Already validated and canonical.
    pub workdir: Option<PathBuf>,
    pub message: String,
}

/// Where one piece of work stands.
#[derive(Debug, Clone)]
pub enum WorkStatus {
    Running,
    /// The dispatch's `--json` envelope, and what the run changed in its
    /// workdir (`None`: no workdir, or not a git work copy).
    Done {
        result: Value,
        changes: Option<Value>,
    },
    Error(RunError),
    /// Its owner cancelled it and it has ended; how its child ended, if it
    /// reported an error.
    Cancelled(Option<RunError>),
}

/// How a run failed: a short machine `reason` (`exit 143/SIGTERM`, `exit 2`,
/// `signal 9/SIGKILL`, `no output`, `spawn failed`) and the raw `error` text
/// (the child's stderr tail).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunError {
    pub reason: String,
    pub error: String,
}

/// A request to stop one running work item, read by its executor.
#[derive(Debug, Default)]
pub struct CancelFlag(std::sync::atomic::AtomicBool);

impl CancelFlag {
    pub fn request(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    pub fn requested(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[derive(Debug, Clone)]
pub struct WorkEntry {
    pub client_name: String,
    pub created: u64,
    pub status: WorkStatus,
    pub cancel: Arc<CancelFlag>,
    /// The canonical work copy, when the run has one.
    pub workdir: Option<PathBuf>,
    /// The work copy as a tree before the run (or before the first run of a
    /// chain of tweaks): what Keep commits against and Discard restores to.
    pub baseline: Option<String>,
    /// Kept, discarded, or folded into a later run: decided once.
    pub decided: Option<String>,
    /// The run this one continues (a Tweak), whose baseline it takes over.
    pub continues: Option<String>,
}

/// (url, ref, dest) -> cloned, or why not.
pub type CloneRepo = dyn Fn(&str, Option<&str>, &Path) -> Result<(), String> + Send + Sync;

/// Everything the listener needs, injectable so tests drive the real router
/// with a fake clock and a fake executor.
#[derive(Clone)]
pub struct ClientServerState {
    pub store: Arc<ClientStore>,
    /// Unix seconds.
    pub now: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// Whether a role is defined on this machine.
    pub role_known: Arc<dyn Fn(&str) -> Result<bool, String> + Send + Sync>,
    /// (role, requested profile) -> the profile that would run, resolved as
    /// `darkmux dispatch <role> [--profile <p>]` resolves it; `Err` when
    /// nothing runnable resolves (a requested name never falls back).
    pub resolve_profile: Arc<dyn Fn(&str, Option<&str>) -> Result<String, String> + Send + Sync>,
    /// This machine's `clients.profiles`, read per request: `Ok(None)` when
    /// unset (only the role's own resolved profile), `Err` when it cannot be
    /// read (work is then refused, never run on a guessed policy).
    pub profiles_policy: Arc<dyn Fn() -> Result<Option<Vec<String>>, String> + Send + Sync>,
    /// The workdir containment check; returns the canonical path.
    pub check_workdir: Arc<dyn Fn(&Path) -> Result<PathBuf, String> + Send + Sync>,
    /// Runs one role execution, blocking; `Ok` is the dispatch's `--json` envelope.
    /// It watches the [`CancelFlag`] and stops the run when it is raised.
    pub execute: Arc<dyn Fn(WorkSpec, Arc<CancelFlag>) -> Result<Value, RunError> + Send + Sync>,
    pub works: Arc<Mutex<HashMap<String, WorkEntry>>>,
    /// The canonical worktrees base workspaces live under.
    pub worktrees_base: Arc<dyn Fn() -> Result<PathBuf, String> + Send + Sync>,
    /// (url, ref, dest): clones a repo into `dest`, which must not exist.
    pub clone_repo: Arc<CloneRepo>,
    /// Updates a workspace's remote-tracking refs; never its branch or files.
    pub fetch_repo: Arc<dyn Fn(&Path) -> Result<(), String> + Send + Sync>,
    /// One workspace creation at a time, so two requests never race for a name.
    pub workspace_lock: Arc<Mutex<()>>,
    /// Workspaces' live previews (dev servers from `.darkmux/preview.json`).
    pub previews: Arc<crate::client_preview::Previews>,
}

impl ClientServerState {
    /// Production wiring: this machine's role library and registry, the real
    /// worktrees base, and `darkmux dispatch` as a child of this binary.
    pub fn production() -> Result<Self> {
        let exe = std::env::current_exe().context("locating this darkmux binary")?;
        Ok(Self {
            store: Arc::new(ClientStore::production()),
            now: Arc::new(unix_now),
            role_known: Arc::new(|role| {
                darkmux_crew::loader::load_roles()
                    .map(|rs| rs.iter().any(|r| r.id == role))
                    .map_err(|e| format!("{e:#}"))
            }),
            resolve_profile: Arc::new(|role, requested| {
                use darkmux_fleet::ProfileResolution as R;
                match darkmux_fleet::resolve_work_profile(role, requested, "this machine") {
                    R::Work { profile, .. } => match requested {
                        Some(asked) if asked != profile => Err(format!(
                            "profile {asked} resolves to {profile} here; refusing"
                        )),
                        _ => Ok(profile),
                    },
                    R::UtilityOnly(p) => Err(format!(
                        "profile {p} holds only the utility model; work never runs on it"
                    )),
                    R::Unresolved(why) => Err(why),
                }
            }),
            profiles_policy: Arc::new(read_profiles_policy),
            check_workdir: Arc::new(|p| {
                darkmux_types::workdir::validate_remote_workdir(p).map_err(|e| format!("{e:#}"))
            }),
            execute: Arc::new(move |spec, cancel| {
                run_dispatch_child(&exe, &spec, &cancel, CANCEL_GRACE, &ProductionTeardown)
            }),
            works: Arc::new(Mutex::new(HashMap::new())),
            worktrees_base: Arc::new(|| {
                let base = darkmux_types::workdir::worktrees_base_dir();
                std::fs::create_dir_all(&base).map_err(|e| format!("{}: {e}", base.display()))?;
                base.canonicalize()
                    .map_err(|e| format!("{}: {e}", base.display()))
            }),
            clone_repo: Arc::new(crate::client_workspace::clone_with_git),
            fetch_repo: Arc::new(crate::client_workspace::fetch_with_git),
            workspace_lock: Arc::new(Mutex::new(())),
            previews: Arc::new(crate::client_preview::Previews::production()),
        })
    }
}

/// `clients.profiles` from `config.json` as it is NOW (the process-wide
/// config tier is read once, and `config set` must take effect without a
/// restart), env first. A missing file is "unset"; one that cannot be read or
/// parsed refuses rather than falling back to a policy nobody wrote.
fn read_profiles_policy() -> Result<Option<Vec<String>>, String> {
    let path = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser).config;
    let cfg = match std::fs::read_to_string(&path) {
        Ok(text) => {
            serde_json::from_str::<darkmux_types::config::DarkmuxConfig>(&text).map_err(|e| {
                format!(
                    "{} does not parse ({e}); refusing client work",
                    path.display()
                )
            })?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Default::default(),
        Err(e) => {
            return Err(format!(
                "{} could not be read ({e}); refusing client work",
                path.display()
            ))
        }
    };
    Ok(darkmux_types::config_access::clients_profiles_in(&cfg))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The address the listener binds: loopback, always.
pub fn bind_addr(port: u16) -> SocketAddr {
    SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port))
}

/// Bind [`bind_addr`].
pub async fn bind_listener(port: u16) -> std::io::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(bind_addr(port)).await
}

/// Serve until Ctrl-C (blocking; builds its own runtime).
pub fn run(port: u16) -> Result<()> {
    let state = ClientServerState::production()?;
    // Refuse a store this binary cannot read before listening.
    state.store.read()?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let listener = bind_listener(port)
            .await
            .with_context(|| format!("binding {}", bind_addr(port)))?;
        eprintln!(
            "darkmux client serve: listening on http://{}",
            listener.local_addr()?
        );
        eprintln!(
            "darkmux client serve: store {}",
            state.store.path().display()
        );
        axum::serve(listener, router(state))
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await
            .context("serving")
    })
}

pub fn router(state: ClientServerState) -> Router {
    Router::new()
        .route("/client/pair", post(pair_handler))
        .route("/client/pair/:request_id", get(poll_handler))
        .route("/client/whoami", get(whoami_handler))
        .route("/client/work", post(work_handler))
        .route("/client/work/:work_id", get(work_status_handler))
        .route("/client/work/:work_id/cancel", post(cancel_handler))
        .route("/client/work/:work_id/keep", post(keep_handler))
        .route("/client/work/:work_id/discard", post(discard_handler))
        .route("/client/recipe", get(recipe_handler))
        .route("/client/workspaces", post(workspace_create_handler))
        .route("/client/workspaces/:name", get(workspace_status_handler))
        .route(
            "/client/workspaces/:name/preview",
            post(preview_start_handler)
                .get(preview_status_handler)
                .delete(preview_stop_handler),
        )
        .fallback(|| async { err(StatusCode::NOT_FOUND, "no such route") })
        .method_not_allowed_fallback(|| async {
            err(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
        })
        .with_state(state)
}

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(serde_json::json!({"error": msg.into()}))).into_response()
}

fn not_found() -> Response {
    err(StatusCode::NOT_FOUND, "not found")
}

fn internal(e: anyhow::Error) -> Response {
    eprintln!("darkmux client serve: {e:#}");
    err(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal error; see the darkmux client serve log",
    )
}

/// Read and parse a capped JSON body; every refusal is JSON.
async fn json_body<T: serde::de::DeserializeOwned>(body: Body) -> Result<T, Response> {
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| {
            err(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("body larger than {MAX_BODY_BYTES} bytes"),
            )
        })?;
    serde_json::from_slice(&bytes)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("bad JSON body: {e}")))
}

fn clean_text(s: &str, field: &str, min: usize, max: usize) -> Result<(), Response> {
    let n = s.chars().count();
    if n < min || n > max || s.chars().any(char::is_control) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("`{field}` must be {min}-{max} characters with no control characters"),
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PairBody {
    device_name: String,
    code: String,
    pair_secret: String,
    /// Accepted for apps built against the first contract, and ignored: what
    /// a paired app may run is this machine's `clients.profiles`.
    #[serde(default)]
    #[allow(dead_code)]
    scopes: Option<serde::de::IgnoredAny>,
}

async fn pair_handler(State(state): State<ClientServerState>, body: Body) -> Response {
    let req: PairBody = match json_body(body).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let checks = [
        clean_text(&req.device_name, "device_name", 1, 64),
        clean_text(&req.code, "code", 1, 16),
        clean_text(&req.pair_secret, "pair_secret", 16, 256),
    ];
    if let Some(Err(r)) = checks.into_iter().find(Result::is_err) {
        return r;
    }
    let now = (state.now)();
    let request_id = match random_hex(16) {
        Ok(h) => format!("r-{h}"),
        Err(e) => return internal(e),
    };
    let record = PendingRequest {
        request_id: request_id.clone(),
        device_name: req.device_name,
        code: req.code,
        pair_secret_hash: hash_hex(&req.pair_secret),
        created: now,
        status: PendingStatus::Pending,
        client_name: None,
    };
    let added = state.store.update(|f| {
        prune(f, now);
        if f.pending.iter().filter(|p| p.live(now)).count() >= MAX_PENDING {
            return Ok(false);
        }
        f.pending.push(record);
        Ok(true)
    });
    match added {
        Ok(true) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"request_id": request_id})),
        )
            .into_response(),
        Ok(false) => err(
            StatusCode::TOO_MANY_REQUESTS,
            "too many pairing requests are waiting; try again later",
        ),
        Err(e) => internal(e),
    }
}

/// What a poll answers, decided under the store's lock.
enum PollOutcome {
    NotFound,
    Status(&'static str),
    Approved { client_name: String, token: String },
}

/// What a paired client may run, as it reads it: `{"profiles": [..] | null,
/// "workspace": true}`. `profiles` is this machine's `clients.profiles`;
/// `null` means unset, so each role runs on its own resolved profile.
/// `workspace` is always true: a `workdir` under the worktrees base is
/// allowed to every paired client.
fn scopes_json(policy: &Option<Vec<String>>) -> Value {
    serde_json::json!({"profiles": policy, "workspace": true})
}

fn policy_or_503(state: &ClientServerState) -> Result<Option<Vec<String>>, Response> {
    (state.profiles_policy)().map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, e))
}

async fn poll_handler(
    State(state): State<ClientServerState>,
    UrlPath(request_id): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    let Some(secret) = headers.get("x-pair-secret").and_then(|v| v.to_str().ok()) else {
        return not_found();
    };
    // Read before anything is consumed, so a policy that cannot be read
    // never costs the app its one token delivery.
    let policy = match policy_or_503(&state) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let now = (state.now)();
    let outcome = state.store.update(|f| {
        prune(f, now);
        let Some(i) = f.pending.iter().position(|p| p.request_id == request_id) else {
            return Ok(PollOutcome::NotFound);
        };
        if !secret_matches(&f.pending[i].pair_secret_hash, secret) {
            return Ok(PollOutcome::NotFound);
        }
        let p = f.pending[i].clone();
        if p.status == PendingStatus::Denied {
            return Ok(PollOutcome::Status("denied"));
        }
        if p.expired(now) {
            return Ok(PollOutcome::Status("expired"));
        }
        if p.status == PendingStatus::Pending {
            return Ok(PollOutcome::Status("pending"));
        }
        // Approved: mint the token, store only its hash, and drop the
        // request so no later poll can collect it again.
        let token = random_hex(32)?;
        let client_name = p.client_name.clone().unwrap_or_else(|| {
            let taken: Vec<String> = f.clients.iter().map(|c| c.name.clone()).collect();
            client_name_for(&p.device_name, &taken)
        });
        f.clients.push(ApprovedClient {
            name: client_name.clone(),
            created: now,
            token_hash: hash_hex(&token),
        });
        f.pending.remove(i);
        Ok(PollOutcome::Approved { client_name, token })
    });
    match outcome {
        Ok(PollOutcome::NotFound) => not_found(),
        Ok(PollOutcome::Status(s)) => Json(serde_json::json!({"status": s})).into_response(),
        Ok(PollOutcome::Approved { client_name, token }) => Json(serde_json::json!({
            "status": "approved", "client_name": client_name, "token": token, "scopes": scopes_json(&policy),
        }))
        .into_response(),
        Err(e) => internal(e),
    }
}

/// The client the bearer token names. Every stored hash is compared (no early
/// exit), each in constant time.
fn authenticate(
    state: &ClientServerState,
    headers: &HeaderMap,
) -> Result<ApprovedClient, Response> {
    let unauthorized = || err(StatusCode::UNAUTHORIZED, "missing or unknown bearer token");
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(unauthorized)?;
    let clients = state.store.read().map_err(internal)?.clients;
    let found = clients.into_iter().fold(None, |hit, c| {
        let ok = secret_matches(&c.token_hash, token);
        if ok && hit.is_none() {
            Some(c)
        } else {
            hit
        }
    });
    found.ok_or_else(unauthorized)
}

async fn whoami_handler(State(state): State<ClientServerState>, headers: HeaderMap) -> Response {
    let client = match authenticate(&state, &headers) {
        Ok(c) => c,
        Err(r) => return r,
    };
    match policy_or_503(&state) {
        Ok(policy) => {
            Json(serde_json::json!({"client_name": client.name, "scopes": scopes_json(&policy)}))
                .into_response()
        }
        Err(r) => r,
    }
}

/// A client-named workdir under the worktrees base
/// ([`ClientServerState::check_workdir`], which canonicalizes, so a relative
/// path is judged by where it really lands); the canonical path, or a 403.
fn checked_workdir(state: &ClientServerState, w: &str) -> Result<PathBuf, Response> {
    (state.check_workdir)(Path::new(w)).map_err(|e| err(StatusCode::FORBIDDEN, e))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkBody {
    role: String,
    message: String,
    /// Absent or null: resolved as `darkmux dispatch <role>` would.
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    workdir: Option<String>,
    /// A workspace name (`POST /client/workspaces`), in place of `workdir`.
    #[serde(default)]
    workspace: Option<String>,
    /// A Tweak: the id of this client's finished, undecided run in the same
    /// work copy that this run continues (its changes stay together).
    #[serde(default)]
    continues: Option<String>,
}

async fn work_handler(
    State(state): State<ClientServerState>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let client = match authenticate(&state, &headers) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let req: WorkBody = match json_body(body).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    if req.message.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "`message` is empty");
    }
    // The checks below read the role library, the registry, config.json and
    // the disk: off the async workers.
    let st = state.clone();
    let checked = tokio::task::spawn_blocking(move || admit(&st, &client, req)).await;
    let spec = match checked {
        Ok(Ok(spec)) => spec,
        Ok(Err(r)) => return r,
        Err(e) => return internal(anyhow!("work admission panicked: {e}")),
    };
    let work_id = spec.work_id.clone();
    let profile = spec.profile.clone();
    let execute = state.execute.clone();
    let works = state.works.clone();
    let Some(cancel) = works
        .lock()
        .unwrap()
        .get(&work_id)
        .map(|w| w.cancel.clone())
    else {
        return internal(anyhow!("work {work_id} was admitted but is not registered"));
    };
    tokio::task::spawn_blocking(move || {
        let id = spec.work_id.clone();
        let workdir = spec.workdir.clone();
        // The work copy as it is before the run, so what it held already is
        // never reported as the run's change. A run in the same work copy
        // as this client's undecided finished run continues it (a tweak):
        // it inherits that run's baseline, and that run is folded into this.
        // A Tweak continues the run it names: it takes over that run's
        // baseline (so its changes are cumulative) and that run is folded in.
        // Any other run starts from the work copy as it is.
        let inherited = {
            let mut works = works.lock().unwrap();
            let continues = works.get(&id).and_then(|e| e.continues.clone());
            continues.and_then(|c| {
                let prior = works.get_mut(&c)?;
                prior.decided = Some(format!("folded into {id}"));
                prior.baseline.clone()
            })
        };
        let baseline = inherited.or_else(|| workdir.as_deref().and_then(snapshot_tree));
        if let Some(e) = works.lock().unwrap().get_mut(&id) {
            e.baseline = baseline.clone();
        }
        let flag = cancel.clone();
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| execute(spec, flag)));
        // A cancelled run ends as cancelled, however its child reported.
        let status = match ran {
            Ok(r) if cancel.requested() => WorkStatus::Cancelled(r.err()),
            Err(_) if cancel.requested() => WorkStatus::Cancelled(None),
            Ok(Ok(result)) => {
                let changes = match (&workdir, &baseline) {
                    (Some(w), Some(before)) => changes_since(w, before),
                    _ => None,
                };
                WorkStatus::Done { result, changes }
            }
            Ok(Err(e)) => WorkStatus::Error(e),
            Err(_) => WorkStatus::Error(RunError {
                reason: "panic".to_string(),
                error: "the dispatch runner panicked".to_string(),
            }),
        };
        if let Some(w) = works.lock().unwrap().get_mut(&id) {
            w.status = status;
        }
    });
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"work_id": work_id, "profile": profile})),
    )
        .into_response()
}

/// Every refusal a work request can meet, in order; on success the work is
/// registered as running and its spec (with the RESOLVED profile) returned.
fn admit(
    state: &ClientServerState,
    client: &ApprovedClient,
    req: WorkBody,
) -> Result<WorkSpec, Response> {
    match (state.role_known)(&req.role) {
        Ok(true) => {}
        Ok(false) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!("role {} is not defined on this machine", req.role),
            ))
        }
        Err(e) => return Err(internal(anyhow!(e))),
    }
    let workdir = match (&req.workdir, &req.workspace) {
        (Some(_), Some(_)) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "send `workdir` or `workspace`, not both",
            ))
        }
        (Some(w), None) => Some(checked_workdir(state, w)?),
        (None, Some(name)) => Some(workspace_dir(state, name)?),
        (None, None) => None,
    };
    let policy = policy_or_503(state)?;
    let forbidden = |e: String| err(StatusCode::FORBIDDEN, e);
    let profile = (state.resolve_profile)(&req.role, req.profile.as_deref()).map_err(forbidden)?;
    if profile.contains('@') {
        return Err(forbidden(format!(
            "profile {profile} names another machine; client work runs here only"
        )));
    }
    match &policy {
        Some(allowed) => {
            if !allowed.iter().any(|p| p == &profile) {
                return Err(forbidden(format!(
                    "profile {profile} is not in this machine's clients.profiles ({})",
                    if allowed.is_empty() {
                        "empty".to_string()
                    } else {
                        allowed.join(", ")
                    }
                )));
            }
        }
        // Unset: only what darkmux itself resolves for the role. An omitted
        // profile IS that; a named one must equal it.
        None if req.profile.is_some() => {
            let own = (state.resolve_profile)(&req.role, None).map_err(forbidden)?;
            if own != profile {
                return Err(forbidden(format!(
                    "profile {profile} is not available to apps: with clients.profiles unset, role {} runs only on \
                     its own profile ({own}); `darkmux config set clients.profiles <list>` widens it",
                    req.role
                )));
            }
        }
        None => {}
    }
    let work_id = format!("w-{}", random_hex(8).map_err(internal)?);
    let now = (state.now)();
    let mut works = state.works.lock().unwrap();
    works.retain(|_, w| {
        matches!(w.status, WorkStatus::Running) || now.saturating_sub(w.created) <= KEEP_WORK_SECS
    });
    let running = works
        .values()
        .filter(|w| w.client_name == client.name && matches!(w.status, WorkStatus::Running))
        .count();
    if running >= MAX_RUNNING_PER_CLIENT {
        return Err(err(
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "client {} already has {running} work items running",
                client.name
            ),
        ));
    }
    // One run at a time per work copy, and never two runs' uncommitted
    // changes mixed: a new run waits until the last run's changes are kept
    // or discarded; a Tweak names the run it continues.
    if let Some(w) = &workdir {
        if let Some((id, _)) = works
            .iter()
            .find(|(_, e)| e.workdir.as_ref() == Some(w) && matches!(e.status, WorkStatus::Running))
        {
            return Err(err(
                StatusCode::CONFLICT,
                format!("run {id} is already working in this work copy"),
            ));
        }
        match &req.continues {
            Some(c) => {
                let ok = works.get(c).is_some_and(|e| {
                    e.client_name == client.name
                        && e.workdir.as_ref() == Some(w)
                        && e.decided.is_none()
                        && matches!(e.status, WorkStatus::Done { .. })
                });
                if !ok {
                    return Err(err(
                        StatusCode::CONFLICT,
                        format!(
                            "run {c} isn't a finished, undecided run of yours in this work copy"
                        ),
                    ));
                }
            }
            None => {
                let has_changes = |e: &WorkEntry| match &e.status {
                    WorkStatus::Done {
                        changes: Some(c), ..
                    } => c["files"].as_array().is_some_and(|f| !f.is_empty()),
                    _ => false,
                };
                if let Some((id, _)) = works.iter().find(|(_, e)| {
                    e.workdir.as_ref() == Some(w) && e.decided.is_none() && has_changes(e)
                }) {
                    return Err(err(
                        StatusCode::CONFLICT,
                        format!(
                            "run {id}'s changes in this work copy are still waiting: keep or discard them first, or continue that run"
                        ),
                    ));
                }
            }
        }
    } else if req.continues.is_some() {
        return Err(err(
            StatusCode::CONFLICT,
            "only a run in a work copy can continue another",
        ));
    }
    works.insert(
        work_id.clone(),
        WorkEntry {
            client_name: client.name.clone(),
            created: now,
            status: WorkStatus::Running,
            cancel: Arc::new(CancelFlag::default()),
            workdir: workdir.clone(),
            baseline: None,
            decided: None,
            continues: req.continues.clone(),
        },
    );
    Ok(WorkSpec {
        work_id,
        role: req.role,
        profile,
        workdir,
        message: req.message,
    })
}

/// The largest `preview.json` `/client/recipe` reads.
pub const MAX_RECIPE_BYTES: u64 = 16 * 1024;

#[derive(Deserialize)]
struct RecipeQuery {
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
}

async fn recipe_handler(
    State(state): State<ClientServerState>,
    headers: HeaderMap,
    query: Result<axum::extract::Query<RecipeQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(r) = authenticate(&state, &headers) {
        return r;
    }
    let q = match query {
        Ok(axum::extract::Query(q)) if q.workdir.is_some() != q.workspace.is_some() => q,
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                "one of the `workdir` or `workspace` query parameters is required",
            )
        }
    };
    let st = state.clone();
    tokio::task::spawn_blocking(move || {
        let wd = match (&q.workdir, &q.workspace) {
            (Some(w), _) => checked_workdir(&st, w),
            (_, Some(n)) => workspace_dir(&st, n),
            _ => unreachable!("checked above"),
        };
        match wd {
            Ok(wd) => read_recipe(&wd),
            Err(r) => r,
        }
    })
    .await
    .unwrap_or_else(|e| internal(anyhow!("recipe read panicked: {e}")))
}

/// `<workdir>/.darkmux/preview.json` under the same containment as work. A
/// symlinked `.darkmux` or `preview.json` is refused, and the file itself is
/// opened without following a link.
fn read_recipe(wd: &Path) -> Response {
    let dir = wd.join(".darkmux");
    let file = dir.join("preview.json");
    for p in [&dir, &file] {
        match std::fs::symlink_metadata(p) {
            Ok(m) if m.file_type().is_symlink() => {
                return err(
                    StatusCode::FORBIDDEN,
                    format!("{} is a symlink; refusing", p.display()),
                )
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return err(StatusCode::NOT_FOUND, "no recipe")
            }
            Err(e) => return internal(anyhow!("reading {}: {e}", p.display())),
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let mut text = String::new();
    let read = opts.open(&file).and_then(|f| {
        if !f.metadata()?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "not a file",
            ));
        }
        f.take(MAX_RECIPE_BYTES + 1).read_to_string(&mut text)
    });
    match read {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return err(StatusCode::NOT_FOUND, "no recipe")
        }
        Err(e) => return internal(anyhow!("reading {}: {e}", file.display())),
    }
    if text.len() as u64 > MAX_RECIPE_BYTES {
        return err(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("preview.json is larger than {MAX_RECIPE_BYTES} bytes"),
        );
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(v @ Value::Object(_)) => Json(serde_json::json!({"recipe": v})).into_response(),
        _ => err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "preview.json is not a JSON object",
        ),
    }
}

// ─── Workspaces ──────────────────────────────────────────────────────────

/// A workspace name resolved to its canonical directory under the worktrees
/// base: 400 for a name that isn't one plain segment, 404 when there is no
/// such workspace, 403 when it fails containment (a symlink out, say).
fn workspace_dir(state: &ClientServerState, name: &str) -> Result<PathBuf, Response> {
    use crate::client_workspace::valid_name;
    if !valid_name(name) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("{name:?} is not a workspace name"),
        ));
    }
    let base = (state.worktrees_base)().map_err(|e| internal(anyhow!(e)))?;
    let dir = base.join(name);
    if std::fs::symlink_metadata(&dir).is_err() {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("no workspace named {name}"),
        ));
    }
    (state.check_workdir)(&dir).map_err(|e| err(StatusCode::FORBIDDEN, e))
}

fn workspace_json(name: &str, dir: &Path, created: bool) -> Value {
    let (branch, head) = crate::client_workspace::status_of(dir).unwrap_or_default();
    serde_json::json!({
        "workspace": name,
        "branch": branch,
        "head": head,
        "repo": crate::client_workspace::origin_of(dir),
        "created": created,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceBody {
    /// Clone (or reuse a checkout of) this repo.
    #[serde(default)]
    repo_url: Option<String>,
    /// Or start from this local checkout, as a git worktree on a new branch.
    #[serde(default)]
    source_path: Option<String>,
    #[serde(default, rename = "ref")]
    git_ref: Option<String>,
    /// The asking bot's name, for the workspace and branch names.
    #[serde(default)]
    bot: Option<String>,
}

/// `POST /client/workspaces`: `{repo_url}` reuses the workspace whose origin
/// is that repo (fetching it) or clones it; `{source_path}` reuses this bot's
/// worktree of that checkout or adds one on a new `mux/<bot>-<id>` branch, so
/// the owner's own working tree is never touched. 201 when created, 200 when
/// reused.
async fn workspace_create_handler(
    State(state): State<ClientServerState>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    use crate::client_workspace as ws;
    if let Err(r) = authenticate(&state, &headers) {
        return r;
    }
    let req: WorkspaceBody = match json_body(body).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    if let Some(r) = &req.git_ref {
        if !ws::valid_ref(r) {
            return err(
                StatusCode::BAD_REQUEST,
                format!("{r:?} is not a branch name"),
            );
        }
    }
    let bot = ws::slug(req.bot.as_deref().unwrap_or("app"));
    let bot = if bot.is_empty() {
        "app".to_string()
    } else {
        bot
    };
    let st = state.clone();
    let made = match (req.repo_url, req.source_path) {
        (Some(url), None) => {
            let Some(normalized) = ws::normalize_repo_url(&url) else {
                return err(
                    StatusCode::BAD_REQUEST,
                    "repo_url must be https://host/owner/repo or git@host:owner/repo",
                );
            };
            tokio::task::spawn_blocking(move || {
                clone_workspace(&st, &url, &normalized, req.git_ref.as_deref())
            })
            .await
        }
        (None, Some(path)) => {
            tokio::task::spawn_blocking(move || {
                worktree_workspace(&st, Path::new(&path), req.git_ref.as_deref(), &bot)
            })
            .await
        }
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                "send `repo_url` or `source_path`, not both or neither",
            )
        }
    };
    made.unwrap_or_else(|e| internal(anyhow!("workspace setup panicked: {e}")))
}

fn clone_workspace(
    state: &ClientServerState,
    url: &str,
    normalized: &str,
    git_ref: Option<&str>,
) -> Response {
    use crate::client_workspace as ws;
    let base = match (state.worktrees_base)() {
        Ok(b) => b,
        Err(e) => return internal(anyhow!(e)),
    };
    let _one_at_a_time = state.workspace_lock.lock().unwrap();
    if let Some(name) = ws::find_by_origin(&base, normalized) {
        let dir = match workspace_dir(state, &name) {
            Ok(d) => d,
            Err(r) => return r,
        };
        let mut v = workspace_json(&name, &dir, false);
        if let Err(e) = (state.fetch_repo)(&dir) {
            v["fetch_error"] = e.into();
        }
        return Json(v).into_response();
    }
    let taken = |n: &str| {
        std::fs::symlink_metadata(base.join(n)).is_ok()
            || std::fs::symlink_metadata(base.join(format!(".{n}.partial"))).is_ok()
    };
    let Some(name) = ws::name_for(normalized, taken) else {
        return err(StatusCode::CONFLICT, "no free workspace name for this repo");
    };
    // Clone beside the final name, then rename: a failed clone never leaves a
    // half workspace behind under a real name.
    let partial = base.join(format!(".{name}.partial"));
    if let Err(e) = (state.clone_repo)(url, git_ref, &partial) {
        let _ = std::fs::remove_dir_all(&partial);
        return err(
            StatusCode::BAD_GATEWAY,
            format!("cloning {normalized}: {e}"),
        );
    }
    if let Err(e) = std::fs::rename(&partial, base.join(&name)) {
        let _ = std::fs::remove_dir_all(&partial);
        return internal(anyhow!("placing workspace {name}: {e}"));
    }
    match workspace_dir(state, &name) {
        Ok(dir) => (StatusCode::CREATED, Json(workspace_json(&name, &dir, true))).into_response(),
        Err(r) => r,
    }
}

fn worktree_workspace(
    state: &ClientServerState,
    source: &Path,
    git_ref: Option<&str>,
    bot: &str,
) -> Response {
    use crate::client_workspace as ws;
    let base = match (state.worktrees_base)() {
        Ok(b) => b,
        Err(e) => return internal(anyhow!(e)),
    };
    let source = match ws::check_source(source, &base) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::FORBIDDEN, e),
    };
    let Some(common) = ws::common_dir(&source) else {
        return err(
            StatusCode::FORBIDDEN,
            format!("{} is not a git repository", source.display()),
        );
    };
    let _one_at_a_time = state.workspace_lock.lock().unwrap();
    if let Some(name) = ws::find_worktree_of(&base, &common, bot) {
        return match workspace_dir(state, &name) {
            Ok(dir) => Json(workspace_json(&name, &dir, false)).into_response(),
            Err(r) => r,
        };
    }
    let repo = ws::slug(
        &source
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
    );
    let stem = format!("{}-{bot}", if repo.is_empty() { "repo" } else { &repo });
    let Some(name) = std::iter::once(stem.clone())
        .chain((2..100).map(|n| format!("{stem}-{n}")))
        .find(|n| ws::valid_name(n) && std::fs::symlink_metadata(base.join(n)).is_err())
    else {
        return err(
            StatusCode::CONFLICT,
            "no free workspace name for this checkout",
        );
    };
    let short = match random_hex(3) {
        Ok(h) => h,
        Err(e) => return internal(e),
    };
    let branch = format!("mux/{bot}-{short}");
    let dest = base.join(&name);
    // Start from the remote's default branch as it is now, not from wherever
    // the owner's checkout happens to be. The fetch moves remote-tracking
    // refs only; the owner's branch, index and files are never touched.
    let fetch_error = (state.fetch_repo)(&source).err();
    let start = git_ref
        .map(str::to_string)
        .or_else(|| ws::remote_default(&source))
        .unwrap_or_else(|| "HEAD".to_string());
    if let Err(e) = ws::add_worktree(&source, &branch, &dest, &start) {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("adding a worktree of {}: {e}", source.display()),
        );
    }
    match workspace_dir(state, &name) {
        Ok(dir) => {
            let mut v = workspace_json(&name, &dir, true);
            v["from"] = start.into();
            if let Some(e) = fetch_error {
                v["fetch_error"] = e.into();
            }
            (StatusCode::CREATED, Json(v)).into_response()
        }
        Err(r) => r,
    }
}

/// `POST /client/workspaces/{name}/preview`: runs the workspace's dev
/// server from its `.darkmux/preview.json` on free ports. 202 while it
/// starts, 200 when it was already running; poll GET for `ready` and `url`.
async fn preview_start_handler(
    State(state): State<ClientServerState>,
    UrlPath(name): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authenticate(&state, &headers) {
        return r;
    }
    let st = state.clone();
    tokio::task::spawn_blocking(move || {
        let dir = match workspace_dir(&st, &name) {
            Ok(d) => d,
            Err(r) => return r,
        };
        match st.previews.start(&name, &dir) {
            Ok((v, true)) => Json(v).into_response(),
            Ok((v, false)) => (StatusCode::ACCEPTED, Json(v)).into_response(),
            Err(e) if e.contains("no .darkmux/preview.json") => err(StatusCode::NOT_FOUND, e),
            Err(e) => err(StatusCode::UNPROCESSABLE_ENTITY, e),
        }
    })
    .await
    .unwrap_or_else(|e| internal(anyhow!("preview start panicked: {e}")))
}

async fn preview_status_handler(
    State(state): State<ClientServerState>,
    UrlPath(name): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authenticate(&state, &headers) {
        return r;
    }
    match state.previews.status(&name) {
        Some(v) => Json(v).into_response(),
        None => err(StatusCode::NOT_FOUND, format!("no preview for {name}")),
    }
}

async fn preview_stop_handler(
    State(state): State<ClientServerState>,
    UrlPath(name): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authenticate(&state, &headers) {
        return r;
    }
    let st = state.clone();
    let stopped = tokio::task::spawn_blocking(move || st.previews.stop(&name))
        .await
        .unwrap_or(false);
    if stopped {
        Json(serde_json::json!({"status": "stopped"})).into_response()
    } else {
        not_found()
    }
}

/// `GET /client/workspaces/{name}`: branch, head and repo of one workspace.
async fn workspace_status_handler(
    State(state): State<ClientServerState>,
    UrlPath(name): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authenticate(&state, &headers) {
        return r;
    }
    let st = state.clone();
    tokio::task::spawn_blocking(move || match workspace_dir(&st, &name) {
        Ok(dir) => Json(workspace_json(&name, &dir, false)).into_response(),
        Err(r) => r,
    })
    .await
    .unwrap_or_else(|e| internal(anyhow!("workspace status panicked: {e}")))
}

async fn work_status_handler(
    State(state): State<ClientServerState>,
    UrlPath(work_id): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    let client = match authenticate(&state, &headers) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let entry = state.works.lock().unwrap().get(&work_id).cloned();
    match entry {
        Some(w) if w.client_name == client.name => match w.status {
            WorkStatus::Running if w.cancel.requested() => {
                Json(serde_json::json!({"status": "cancelling"})).into_response()
            }
            WorkStatus::Running => Json(serde_json::json!({"status": "running"})).into_response(),
            WorkStatus::Done { result, changes } => {
                Json(serde_json::json!({"status": "done", "result": result, "changes": changes}))
                    .into_response()
            }
            WorkStatus::Error(e) => {
                Json(serde_json::json!({"status": "error", "error": e.error, "reason": e.reason}))
                    .into_response()
            }
            WorkStatus::Cancelled(e) => Json(serde_json::json!({
                "status": "cancelled",
                "reason": "cancelled",
                "error": e.map(|e| e.error),
            }))
            .into_response(),
        },
        _ => not_found(),
    }
}

/// `POST /client/work/{id}/cancel`: only the client that started the run may
/// stop it (anyone else's id reads as unknown). The stop itself is the
/// executor's: SIGTERM, then SIGKILL after [`CANCEL_GRACE`].
async fn cancel_handler(
    State(state): State<ClientServerState>,
    UrlPath(work_id): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    let client = match authenticate(&state, &headers) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let works = state.works.lock().unwrap();
    match works.get(&work_id) {
        Some(w) if w.client_name == client.name => match w.status {
            WorkStatus::Running => {
                w.cancel.request();
                (
                    StatusCode::ACCEPTED,
                    Json(serde_json::json!({"status": "cancelling"})),
                )
                    .into_response()
            }
            _ => err(StatusCode::CONFLICT, "not running"),
        },
        _ => not_found(),
    }
}

// ─── Keep and Discard ────────────────────────────────────────────────────

/// A finished, undecided run of this client's with a git work copy, for
/// Keep or Discard: (workdir, baseline, paths the run changed). Refusals
/// are responses: 404 for anyone else's or an unknown id, 409 otherwise.
fn decidable(
    state: &ClientServerState,
    headers: &HeaderMap,
    work_id: &str,
) -> Result<(PathBuf, String, Vec<String>), Response> {
    let client = authenticate(state, headers)?;
    let entry = state.works.lock().unwrap().get(work_id).cloned();
    let Some(w) = entry.filter(|w| w.client_name == client.name) else {
        return Err(not_found());
    };
    if let Some(d) = &w.decided {
        return Err(err(StatusCode::CONFLICT, format!("already decided: {d}")));
    }
    if !matches!(w.status, WorkStatus::Done { .. }) {
        return Err(err(StatusCode::CONFLICT, "the run hasn't finished"));
    }
    let (Some(wd), Some(base)) = (w.workdir, w.baseline) else {
        return Err(err(StatusCode::CONFLICT, "the run has no git work copy"));
    };
    let now = snapshot_tree(&wd).ok_or_else(|| {
        err(
            StatusCode::CONFLICT,
            "the work copy is no longer a git repository",
        )
    })?;
    let out = git(
        &wd,
        None,
        &["diff", "--no-renames", "--name-only", "-z", &base, &now],
    )
    .ok_or_else(|| internal(anyhow!("diffing the work copy failed")))?;
    let paths: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    Ok((wd, base, paths))
}

fn mark_decided(state: &ClientServerState, work_id: &str, how: &str) {
    if let Some(w) = state.works.lock().unwrap().get_mut(work_id) {
        w.decided = Some(how.to_string());
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct KeepBody {
    #[serde(default)]
    message: Option<String>,
}

/// `POST /client/work/{id}/keep {message?}`: commits exactly the files the
/// run (and its tweaks) changed to the work copy's branch, with the repo's
/// hooks off. Anything else in the work copy, staged or not, stays as it
/// was. 200 {commit, branch, files}.
async fn keep_handler(
    State(state): State<ClientServerState>,
    UrlPath(work_id): UrlPath<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let req: KeepBody = match json_body(body).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let st = state.clone();
    tokio::task::spawn_blocking(move || {
        let (wd, _base, paths) = match decidable(&st, &headers, &work_id) {
            Ok(d) => d,
            Err(r) => return r,
        };
        if paths.is_empty() {
            return err(
                StatusCode::CONFLICT,
                "the run changed nothing; there is nothing to keep",
            );
        }
        let branch = git(&wd, None, &["branch", "--show-current"])
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if branch.is_empty() {
            return err(
                StatusCode::CONFLICT,
                "the work copy isn't on a branch; nothing to commit to",
            );
        }
        let message = req
            .message
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "Changes made by a darkmux run".to_string());
        let mut add = vec!["add", "-A", "--"];
        add.extend(paths.iter().map(String::as_str));
        let mut commit = vec![
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "--quiet",
            "-m",
            message.as_str(),
            "--",
        ];
        commit.extend(paths.iter().map(String::as_str));
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&wd)
                .args(args)
                .stdin(std::process::Stdio::null())
                .output()
        };
        for args in [&add[..], &commit[..]] {
            match run(args) {
                Ok(o) if o.status.success() => {}
                Ok(o) => {
                    let why = String::from_utf8_lossy(&o.stderr);
                    return internal(anyhow!(
                        "git {}: {}",
                        args[0],
                        why.lines().last().unwrap_or("").trim()
                    ));
                }
                Err(e) => return internal(anyhow!("running git: {e}")),
            }
        }
        let sha = git(&wd, None, &["rev-parse", "--short", "HEAD"])
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        mark_decided(&st, &work_id, "kept");
        Json(serde_json::json!({"commit": sha, "branch": branch, "files": paths})).into_response()
    })
    .await
    .unwrap_or_else(|e| internal(anyhow!("keep panicked: {e}")))
}

/// `POST /client/work/{id}/discard`: puts every file the run (and its
/// tweaks) changed back as it was before the first run, and removes files
/// it added. Nothing else is touched. 200 {restored, removed}.
async fn discard_handler(
    State(state): State<ClientServerState>,
    UrlPath(work_id): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    let st = state.clone();
    tokio::task::spawn_blocking(move || {
        let (wd, base, paths) = match decidable(&st, &headers, &work_id) {
            Ok(d) => d,
            Err(r) => return r,
        };
        let (mut restored, mut removed) = (Vec::new(), Vec::new());
        for path in &paths {
            let in_base = git(&wd, None, &["cat-file", "-e", &format!("{base}:{path}")]).is_some();
            if in_base {
                if git(
                    &wd,
                    None,
                    &[
                        "restore",
                        &format!("--source={base}"),
                        "--worktree",
                        "--",
                        path,
                    ],
                )
                .is_none()
                {
                    return internal(anyhow!("restoring {path} failed"));
                }
                restored.push(path.clone());
            } else {
                let file = wd.join(path);
                // git reports paths inside the work tree; never follow one out.
                let inside = file
                    .parent()
                    .and_then(|p| p.canonicalize().ok())
                    .is_some_and(|p| p.starts_with(&wd));
                if inside && std::fs::symlink_metadata(&file).is_ok() {
                    if let Err(e) = std::fs::remove_file(&file) {
                        return internal(anyhow!("removing {path}: {e}"));
                    }
                }
                removed.push(path.clone());
            }
        }
        mark_decided(&st, &work_id, "discarded");
        Json(serde_json::json!({"restored": restored, "removed": removed})).into_response()
    })
    .await
    .unwrap_or_else(|e| internal(anyhow!("discard panicked: {e}")))
}

// ─── The production executor ─────────────────────────────────────────────

/// The child's argv after the binary: the existing `dispatch` verb. The
/// message is NOT here; it goes on stdin, so it never reaches `ps` or ARG_MAX
/// and can never be read as a flag.
pub(crate) fn dispatch_args(spec: &WorkSpec) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = vec![
        "dispatch".into(),
        spec.role.clone().into(),
        "--profile".into(),
        spec.profile.clone().into(),
    ];
    if let Some(w) = &spec.workdir {
        args.push("--workdir".into());
        args.push(w.clone().into_os_string());
    }
    args.extend([
        "--json".into(),
        "--name".into(),
        spec.work_id.clone().into(),
    ]);
    args
}

/// How long a cancelled dispatch child gets after SIGTERM before SIGKILL.
/// `darkmux dispatch` handles TERM itself (its interrupt flag stops the run,
/// `docker kill`s its container by name and writes its own `dispatch.error`
/// with `stop_reason: "SIGTERM"`), which takes well under a second; the grace
/// is for a child stuck somewhere that does not poll the flag.
pub const CANCEL_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// Run `exe dispatch ...` with the message on stdin and return its `--json`
/// envelope. An envelope is returned whatever the exit code (its `result`
/// says how the dispatch ended); no envelope is an error carrying the exit
/// status and the tail of stderr.
///
/// **Cancel.** When `cancel` is raised the child gets SIGTERM; one still
/// alive `grace` later gets SIGKILL, and since a killed child runs none of
/// its own teardown, [`after_hard_kill`] does it: stop its containers, and
/// end its execution with the terminal it could not write.
pub(crate) fn run_dispatch_child(
    exe: &Path,
    spec: &WorkSpec,
    cancel: &CancelFlag,
    grace: std::time::Duration,
    teardown: &dyn HardKillTeardown,
) -> Result<Value, RunError> {
    use darkmux_types::child_registry::{kill_pid, SIGKILL, SIGTERM};
    let failed = |reason: &str, error: String| RunError {
        reason: reason.to_string(),
        error,
    };
    use std::process::{Command, Stdio};
    let mut child = Command::new(exe)
        .args(dispatch_args(spec))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            failed(
                "spawn failed",
                format!("could not start {}: {e}", exe.display()),
            )
        })?;
    let pid = child.id();
    let mut stdin = child.stdin.take().ok_or_else(|| {
        failed(
            "spawn failed",
            "no stdin for the dispatch child".to_string(),
        )
    })?;
    let message = spec.message.clone();
    // A child that exits before reading is reported by its status below.
    let feeder = std::thread::spawn(move || {
        let _ = stdin.write_all(message.as_bytes());
    });
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out_reader = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let err_reader = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let mut termed_at: Option<std::time::Instant> = None;
    let mut killed = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                return Err(failed(
                    "wait failed",
                    format!("waiting for the dispatch child: {e}"),
                ))
            }
        }
        if cancel.requested() {
            match termed_at {
                None => {
                    let _ = kill_pid(pid, SIGTERM);
                    termed_at = Some(std::time::Instant::now());
                }
                Some(t) if !killed && t.elapsed() >= grace => {
                    let _ = kill_pid(pid, SIGKILL);
                    killed = true;
                }
                Some(_) => {}
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    };
    let _ = feeder.join();
    if killed {
        after_hard_kill(teardown, spec, pid);
    }
    let stdout_bytes = out_reader.join().unwrap_or_default();
    let stderr_bytes = err_reader.join().unwrap_or_default();
    let stdout = String::from_utf8_lossy(&stdout_bytes);
    if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(stdout.trim()) {
        return Ok(v);
    }
    let stderr = String::from_utf8_lossy(&stderr_bytes);
    let chars: Vec<char> = stderr.trim().chars().collect();
    let tail: String = chars[chars.len().saturating_sub(STDERR_TAIL_CHARS)..]
        .iter()
        .collect();
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    Err(failed(&exit_reason(status.code(), signal), tail))
}

/// The most of a failed child's stderr a status keeps.
const STDERR_TAIL_CHARS: usize = 4000;

/// A short machine reason for a child that ended without a `--json`
/// envelope: `exit 143/SIGTERM` (dispatch's own exit on a caught signal is
/// 128 + its number), `exit <n>`, `signal 9/SIGKILL` (killed outright), or
/// `no output` (a clean exit that printed no envelope).
pub(crate) fn exit_reason(code: Option<i32>, signal: Option<i32>) -> String {
    let name = |n: i32| match n {
        1 => Some("SIGHUP"),
        2 => Some("SIGINT"),
        9 => Some("SIGKILL"),
        15 => Some("SIGTERM"),
        _ => None,
    };
    match (code, signal) {
        (Some(0), _) => "no output".to_string(),
        (Some(c), _) if c > 128 => match name(c - 128) {
            Some(n) => format!("exit {c}/{n}"),
            None => format!("exit {c}"),
        },
        (Some(c), _) => format!("exit {c}"),
        (None, Some(sig)) => match name(sig) {
            Some(n) => format!("signal {sig}/{n}"),
            None => format!("signal {sig}"),
        },
        (None, None) => "no exit status".to_string(),
    }
}

// ─── What a run changed ──────────────────────────────────────────────────

/// The most of a run's unified diff a status carries.
pub const MAX_DIFF_BYTES: usize = 32 * 1024;
/// Appended to a diff cut at [`MAX_DIFF_BYTES`].
pub const DIFF_TRUNCATED_MARKER: &str = "\n[diff truncated at 32 KiB]\n";

fn git(wd: &Path, index: Option<&Path>, args: &[&str]) -> Option<std::process::Output> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C")
        .arg(wd)
        .args(args)
        .stdin(std::process::Stdio::null());
    if let Some(i) = index {
        cmd.env("GIT_INDEX_FILE", i);
    }
    let out = cmd.output().ok()?;
    out.status.success().then_some(out)
}

/// The whole work copy as a git tree id: tracked AND untracked files (not
/// ignored ones), staged or not, written through a throwaway index so the
/// work copy's own index, branch and files are never touched (only blob and
/// tree objects are added to its object store). `None` when `wd` is not in
/// a git work copy.
pub(crate) fn snapshot_tree(wd: &Path) -> Option<String> {
    // Outside a git work copy `git add` fails, which is the `None`.
    let scratch = tempfile::tempdir().ok()?;
    let index = scratch.path().join("index");
    git(wd, Some(&index), &["add", "-A"])?;
    let tree = git(wd, Some(&index), &["write-tree"])?;
    Some(String::from_utf8_lossy(&tree.stdout).trim().to_string())
}

/// What changed in `wd` since the tree `before` ([`snapshot_tree`]):
/// `{"files": [{"path","added","removed"}], "diff": "<unified diff>"}`, the
/// diff capped at [`MAX_DIFF_BYTES`]. `added`/`removed` are null for a
/// binary file. `None` when the work copy can no longer be read.
pub(crate) fn changes_since(wd: &Path, before: &str) -> Option<Value> {
    let after = snapshot_tree(wd)?;
    let numstat = git(
        wd,
        None,
        &[
            "diff",
            "--no-renames",
            "--no-ext-diff",
            "--numstat",
            "-z",
            before,
            &after,
        ],
    )?;
    let files: Vec<Value> = String::from_utf8_lossy(&numstat.stdout)
        .split('\0')
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            let mut parts = l.splitn(3, '\t');
            let (added, removed, path) = (parts.next()?, parts.next()?, parts.next()?);
            let n = |s: &str| s.parse::<u64>().ok();
            Some(serde_json::json!({"path": path, "added": n(added), "removed": n(removed)}))
        })
        .collect();
    let patch = git(
        wd,
        None,
        &[
            "diff",
            "--no-renames",
            "--no-ext-diff",
            "--no-color",
            before,
            &after,
        ],
    )?;
    Some(
        serde_json::json!({"files": files, "diff": capped_diff(&String::from_utf8_lossy(&patch.stdout))}),
    )
}

fn capped_diff(diff: &str) -> String {
    if diff.len() <= MAX_DIFF_BYTES {
        return diff.to_string();
    }
    let mut cut = MAX_DIFF_BYTES;
    while !diff.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{DIFF_TRUNCATED_MARKER}", &diff[..cut])
}

// ─── Teardown after a hard kill ──────────────────────────────────────────

/// A dispatch child's role execution, as its `dispatch.start` names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedRun {
    pub session: String,
    pub execution: String,
    pub role: String,
    pub model: Option<String>,
}

/// What [`after_hard_kill`] needs from the outside world, injectable so a
/// test needs no Docker and no flow directory.
pub trait HardKillTeardown: Send + Sync {
    /// Stop the dispatch containers whose host was `host_pid`.
    fn stop_containers(&self, host_pid: u32);
    /// The execution the child started, from its `dispatch.start`.
    fn find_start(&self, spec: &WorkSpec) -> Option<StartedRun>;
    /// Whether that execution already has its terminal record.
    fn has_terminal(&self, run: &StartedRun) -> bool;
    /// Write the execution's `dispatch.error` naming the cancel.
    fn emit_cancelled(&self, run: &StartedRun);
}

/// Do for a SIGKILLed dispatch child what its own TERM handling would have
/// done: stop its containers, then end its execution, once. A child killed
/// before its `dispatch.start` has no execution to end; one that wrote its
/// terminal before the KILL landed is not given a second.
pub(crate) fn after_hard_kill(teardown: &dyn HardKillTeardown, spec: &WorkSpec, host_pid: u32) {
    teardown.stop_containers(host_pid);
    if let Some(run) = teardown.find_start(spec) {
        if !teardown.has_terminal(&run) {
            teardown.emit_cancelled(&run);
        }
    }
}

/// The real world: the orphan reaper's Docker path and the flow stream.
pub struct ProductionTeardown;

impl HardKillTeardown for ProductionTeardown {
    fn stop_containers(&self, host_pid: u32) {
        // The serve daemon's reaper path (#3200): every dispatch container is
        // labeled with its host's pid, start, machine and pid namespace. Only
        // `host_pid` is judged gone; every other host reads as alive with an
        // unknown start, which the rule keeps, so no other run is touched.
        let local = darkmux_crew::orphan_reaper::LocalHost::this_host();
        let _ = darkmux_crew::orphan_reaper::reap_tick(
            local.as_ref(),
            only_this_host_is_gone(host_pid),
        );
    }
    fn find_start(&self, spec: &WorkSpec) -> Option<StartedRun> {
        find_start_in(&darkmux_flow::flows_dir(), spec)
    }
    fn has_terminal(&self, run: &StartedRun) -> bool {
        terminal_in(&darkmux_flow::flows_dir(), &run.execution)
    }
    fn emit_cancelled(&self, run: &StartedRun) {
        match cancelled_record(run) {
            Ok(rec) => {
                if let Err(e) = darkmux_flow::record(rec) {
                    eprintln!(
                        "darkmux client serve: writing the cancelled terminal for {}: {e:#}",
                        run.execution
                    );
                }
            }
            Err(e) => eprintln!("darkmux client serve: {e}"),
        }
    }
}

/// The reaper's liveness answer scoped to one killed child: `host_pid` is
/// gone, every other host is alive with an unknown start, which the reaper's
/// rule always keeps.
pub(crate) fn only_this_host_is_gone(
    host_pid: u32,
) -> impl Fn(u32) -> darkmux_crew::orphan_reaper::HostLiveness {
    use darkmux_crew::orphan_reaper::HostLiveness;
    move |pid| {
        if pid == host_pid {
            HostLiveness::Gone
        } else {
            HostLiveness::Running { start: None }
        }
    }
}

/// The day files a just-killed run's records can be in: today's and
/// yesterday's (a run that started before midnight).
const START_SEARCH_DAYS: usize = 2;

fn records_in(dir: &Path) -> impl Iterator<Item = Value> {
    darkmux_flow::reader::recent_day_files(dir, START_SEARCH_DAYS)
        .into_iter()
        .flat_map(|p| darkmux_flow::reader::day_file_records(&p))
}

/// The `dispatch.start` of the crew-of-one run `darkmux dispatch <role> --name
/// <work_id>` made: its session ends `.adhoc.<role>.<work_id>`.
pub(crate) fn find_start_in(dir: &Path, spec: &WorkSpec) -> Option<StartedRun> {
    let suffix = format!(".adhoc.{}.{}", spec.role, spec.work_id);
    records_in(dir).find_map(|r| {
        if darkmux_flow::reader::action_of(&r) != Some(darkmux_flow::FlowAction::DispatchStart) {
            return None;
        }
        let session = r.get("session_id")?.as_str()?;
        if !session.ends_with(&suffix) {
            return None;
        }
        Some(StartedRun {
            session: session.to_string(),
            execution: r.get("execution_id")?.as_str()?.to_string(),
            role: spec.role.clone(),
            model: r.get("model").and_then(Value::as_str).map(str::to_string),
        })
    })
}

/// Whether `execution` has a `dispatch.complete` or `dispatch.error`.
pub(crate) fn terminal_in(dir: &Path, execution: &str) -> bool {
    use darkmux_flow::FlowAction;
    records_in(dir).any(|r| {
        r.get("execution_id").and_then(Value::as_str) == Some(execution)
            && matches!(
                darkmux_flow::reader::action_of(&r),
                Some(FlowAction::DispatchComplete | FlowAction::DispatchError)
            )
    })
}

/// The execution's terminal: a `dispatch.error` (its action comes from the
/// payload variant) at `Error`, `stop_reason: "cancelled"`, built through the
/// same `build_dispatch_record` dispatch's own bookends use.
pub(crate) fn cancelled_record(run: &StartedRun) -> Result<darkmux_flow::FlowRecord, String> {
    let session =
        darkmux_types::session_id::SessionId::parse(&run.session).map_err(|e| e.to_string())?;
    let execution = darkmux_types::execution_id::ExecutionId::parse(&run.execution)
        .map_err(|e| e.to_string())?;
    let mut payload = darkmux_flow::payload::DispatchEndPayload::aborted(None);
    payload.error =
        Some("cancelled by the client that started it (SIGKILL after the grace)".to_string());
    payload.stop_reason = Some("cancelled".to_string());
    Ok(darkmux_crew::dispatch::build_dispatch_record(
        darkmux_flow::Level::Error,
        &run.role,
        &session,
        &execution,
        run.model.as_deref(),
        None,
        darkmux_flow::Payload::DispatchError(payload),
    ))
}

#[cfg(test)]
#[path = "client_pairing_tests.rs"]
mod tests;
