//! Client pairing (LOCAL PROTOTYPE): `darkmux client serve`.
//!
//! A third listener, beside the viewer daemon and the fleet listener and
//! sharing nothing with either, that lets an app on this machine (the Mux Bot
//! app) pair once and then run scoped work. It binds `127.0.0.1` ONLY
//! ([`bind_addr`]): never `0.0.0.0`, never the tailnet address.
//!
//! **Pairing.** The app `POST`s `/client/pair` with a device name, a code it
//! shows its user, a random `pair_secret`, and the scopes it asks for. The
//! operator runs `darkmux client approve` here, sees the device, code, scopes
//! and age, and approves (or denies). The app polls `/client/pair/{id}` with
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
//! crew-of-one run) holds unchanged. The profile must be one the client was
//! granted, must not route to another machine (`<p>@<machine>`), and must be
//! defined here (dispatch would otherwise fall back to `default_profile`). A
//! `workdir` needs the `workspace` grant and must lie under the worktrees base
//! (`validate_remote_workdir`).
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
pub const CLIENTS_SCHEMA_VERSION: &str = "1.0";

// ─── Store ───────────────────────────────────────────────────────────────

/// What a client may do: run on these profiles, and name a workdir.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Scopes {
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub workspace: bool,
}

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
    pub scopes: Scopes,
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
    pub scopes: Scopes,
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
    let doc: Value = serde_json::from_str(text)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    if let Some(v) = darkmux_types::data_version::newer(&doc, CLIENTS_SCHEMA_VERSION) {
        bail!("{} was written by a newer darkmux (schema_version {v}; this one reads {CLIENTS_SCHEMA_VERSION}). Upgrade darkmux.", path.display());
    }
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
    Done(Value),
    Error(String),
}

#[derive(Debug, Clone)]
pub struct WorkEntry {
    pub client_name: String,
    pub created: u64,
    pub status: WorkStatus,
}

/// Everything the listener needs, injectable so tests drive the real router
/// with a fake clock and a fake executor.
#[derive(Clone)]
pub struct ClientServerState {
    pub store: Arc<ClientStore>,
    /// Unix seconds.
    pub now: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// Whether a role is defined on this machine.
    pub role_known: Arc<dyn Fn(&str) -> Result<bool, String> + Send + Sync>,
    /// (role, profile): `Ok` when the profile is defined here and runnable
    /// for the role.
    pub resolve_profile: Arc<dyn Fn(&str, &str) -> Result<(), String> + Send + Sync>,
    /// The workdir containment check; returns the canonical path.
    pub check_workdir: Arc<dyn Fn(&Path) -> Result<PathBuf, String> + Send + Sync>,
    /// Runs one role execution, blocking; `Ok` is the dispatch's `--json` envelope.
    pub execute: Arc<dyn Fn(WorkSpec) -> Result<Value, String> + Send + Sync>,
    pub works: Arc<Mutex<HashMap<String, WorkEntry>>>,
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
            resolve_profile: Arc::new(|role, profile| {
                match darkmux_fleet::resolve_work_profile(role, Some(profile), "this machine") {
                    darkmux_fleet::ProfileResolution::Work {
                        profile: resolved, ..
                    } if resolved == profile => Ok(()),
                    darkmux_fleet::ProfileResolution::Work {
                        profile: resolved, ..
                    } => Err(format!(
                        "profile {profile} resolves to {resolved} here; refusing"
                    )),
                    darkmux_fleet::ProfileResolution::UtilityOnly(p) => Err(format!(
                        "profile {p} holds only the utility model; work never runs on it"
                    )),
                    darkmux_fleet::ProfileResolution::Unresolved(why) => Err(why),
                }
            }),
            check_workdir: Arc::new(|p| {
                darkmux_types::workdir::validate_remote_workdir(p).map_err(|e| format!("{e:#}"))
            }),
            execute: Arc::new(move |spec| run_dispatch_child(&exe, &spec)),
            works: Arc::new(Mutex::new(HashMap::new())),
        })
    }
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
    scopes: Scopes,
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
    if req.scopes.profiles.len() > 32 {
        return err(StatusCode::BAD_REQUEST, "at most 32 profiles");
    }
    for p in &req.scopes.profiles {
        if let Err(r) = clean_text(p, "scopes.profiles[]", 1, 128) {
            return r;
        }
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
        scopes: req.scopes,
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
    Approved {
        client_name: String,
        token: String,
        scopes: Scopes,
    },
}

async fn poll_handler(
    State(state): State<ClientServerState>,
    UrlPath(request_id): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    let Some(secret) = headers.get("x-pair-secret").and_then(|v| v.to_str().ok()) else {
        return not_found();
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
            scopes: p.scopes.clone(),
            token_hash: hash_hex(&token),
        });
        f.pending.remove(i);
        Ok(PollOutcome::Approved {
            client_name,
            token,
            scopes: p.scopes,
        })
    });
    match outcome {
        Ok(PollOutcome::NotFound) => not_found(),
        Ok(PollOutcome::Status(s)) => Json(serde_json::json!({"status": s})).into_response(),
        Ok(PollOutcome::Approved {
            client_name,
            token,
            scopes,
        }) => Json(serde_json::json!({
            "status": "approved", "client_name": client_name, "token": token, "scopes": scopes,
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
    match authenticate(&state, &headers) {
        Ok(c) => {
            Json(serde_json::json!({"client_name": c.name, "scopes": c.scopes})).into_response()
        }
        Err(r) => r,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkBody {
    role: String,
    message: String,
    profile: String,
    #[serde(default)]
    workdir: Option<String>,
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
    // The checks below read the role library, the registry and the disk:
    // off the async workers.
    let st = state.clone();
    let checked = tokio::task::spawn_blocking(move || admit(&st, &client, req)).await;
    let spec = match checked {
        Ok(Ok(spec)) => spec,
        Ok(Err(r)) => return r,
        Err(e) => return internal(anyhow!("work admission panicked: {e}")),
    };
    let work_id = spec.work_id.clone();
    let execute = state.execute.clone();
    let works = state.works.clone();
    tokio::task::spawn_blocking(move || {
        let id = spec.work_id.clone();
        let status = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| execute(spec)))
        {
            Ok(Ok(v)) => WorkStatus::Done(v),
            Ok(Err(e)) => WorkStatus::Error(e),
            Err(_) => WorkStatus::Error("the dispatch runner panicked".to_string()),
        };
        if let Some(w) = works.lock().unwrap().get_mut(&id) {
            w.status = status;
        }
    });
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"work_id": work_id})),
    )
        .into_response()
}

/// Every refusal a work request can meet, in order; on success the work is
/// registered as running and its spec returned.
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
    if !client.scopes.profiles.iter().any(|p| p == &req.profile) {
        return Err(err(
            StatusCode::FORBIDDEN,
            format!(
                "profile {} is not in client {}'s scopes",
                req.profile, client.name
            ),
        ));
    }
    if req.profile.contains('@') {
        return Err(err(
            StatusCode::FORBIDDEN,
            format!(
                "profile {} names another machine; client work runs here only",
                req.profile
            ),
        ));
    }
    let workdir = match &req.workdir {
        None => None,
        Some(_) if !client.scopes.workspace => {
            return Err(err(
                StatusCode::FORBIDDEN,
                format!(
                    "client {} was not granted workspace access; omit `workdir`",
                    client.name
                ),
            ));
        }
        Some(w) => {
            Some((state.check_workdir)(Path::new(w)).map_err(|e| err(StatusCode::FORBIDDEN, e))?)
        }
    };
    (state.resolve_profile)(&req.role, &req.profile).map_err(|e| err(StatusCode::FORBIDDEN, e))?;
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
    works.insert(
        work_id.clone(),
        WorkEntry {
            client_name: client.name.clone(),
            created: now,
            status: WorkStatus::Running,
        },
    );
    Ok(WorkSpec {
        work_id,
        role: req.role,
        profile: req.profile,
        workdir,
        message: req.message,
    })
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
            WorkStatus::Running => Json(serde_json::json!({"status": "running"})).into_response(),
            WorkStatus::Done(v) => {
                Json(serde_json::json!({"status": "done", "result": v})).into_response()
            }
            WorkStatus::Error(e) => {
                Json(serde_json::json!({"status": "error", "error": e})).into_response()
            }
        },
        _ => not_found(),
    }
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

/// Run `exe dispatch ...` with the message on stdin and return its `--json`
/// envelope. An envelope is returned whatever the exit code (its `result`
/// says how the dispatch ended); no envelope is an error carrying the exit
/// status and the tail of stderr.
pub(crate) fn run_dispatch_child(exe: &Path, spec: &WorkSpec) -> Result<Value, String> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(exe)
        .args(dispatch_args(spec))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", exe.display()))?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or("no stdin for the dispatch child")?;
        // A child that exits before reading is reported by its status below.
        let _ = stdin.write_all(spec.message.as_bytes());
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("waiting for the dispatch child: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(stdout.trim()) {
        return Ok(v);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let tail: String = {
        let chars: Vec<char> = stderr.trim().chars().collect();
        chars[chars.len().saturating_sub(2000)..].iter().collect()
    };
    Err(format!(
        "dispatch exited {} with no JSON envelope: {tail}",
        out.status
            .code()
            .map_or("by signal".to_string(), |c| c.to_string())
    ))
}

#[cfg(test)]
#[path = "client_pairing_tests.rs"]
mod tests;
