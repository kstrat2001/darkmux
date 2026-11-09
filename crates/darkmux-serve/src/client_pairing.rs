//! Client pairing (LOCAL PROTOTYPE): `darkmux client serve`.
//!
//! A third listener, beside the viewer daemon and the fleet listener and
//! sharing nothing with either, that lets an app on this machine (the Mux Bot
//! app) pair once and then run scoped work. It binds `127.0.0.1` ONLY
//! ([`bind_addr`]): never `0.0.0.0`, never the tailnet address. Every route
//! sits behind one gate ([`foreign_request`]): the request must be from this
//! machine as the viewer daemon decides it (`is_local_request`: a loopback
//! peer, no reverse-proxy header, a `Host` naming this listener), must carry
//! no `Origin` (a web page sends one; a native app does not), and a POST with
//! a body must be `Content-Type: application/json`. A browser page on any
//! site therefore cannot drive it, whatever DNS it rebinds.
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
//! (unset: any local profile, one that runs on this machine; a hosted one
//! must be granted by name). A `workdir` must be exactly one workspace
//! directly under the worktrees base (#3451 review): never the base itself,
//! a dot entry beside the workspaces (darkmux's held repositories and
//! records) or a path inside a workspace.
//! `GET /client/recipe?workspace=` returns a workspace's preview recipe, for
//! an app that cannot read the disk itself.
//!
//! **Previews run in a container** ([`crate::client_preview`]): one per
//! running preview, from the darkmux runtime image, the workspace its only
//! mount, its two ports published on 127.0.0.1 only, and no host
//! environment beyond the recipe's `env`. Stop, the idle check and
//! `client serve`'s own shutdown remove it.
//!
//! **Previews trust only the user's own checkout.** A preview's recipe
//! (`.darkmux/preview.json`) is a command darkmux runs, so it is read only
//! from the checkout the workspace's worktree was made from (`source_path`),
//! as darkmux recorded it when it made the workspace, and never from the
//! workspace, where a Coder run writes. A project made from a template
//! (#3460) trusts the recipe in its first commit, the one darkmux made,
//! read from the repository darkmux holds. A workspace cloned from a URL
//! gets no preview, and neither does one whose checkout has no recipe; the
//! refusal names why and says to add `.darkmux/preview.json` to your own
//! checkout of the repo ([`crate::client_preview`]).
//!
//! **Host git never reads what a run can write.** A workspace is mounted
//! read-write into the run's container, so its `.git` and config are the
//! run's. Every host git call (snapshot, diff, commit, Undo, `/changes`,
//! status, fetch) names the repository darkmux holds for the workspace,
//! outside it and recorded when darkmux made it, with every program-running
//! setting off ([`crate::client_git`]). A clone's repository is its
//! `--separate-git-dir`; a worktree's is its admin directory in the user's
//! checkout. A folder under the base that darkmux did not make gets no git.
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
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client_git::WorkCopy;
use crate::client_wire::*;
use darkmux_types::client_code::ClientCode;
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
            rewrite(guard.file(), &doc)?;
        }
        owner_only(&self.path)?;
        Ok(out)
    }
}

/// Replaces the locked file's contents with `doc`, synced.
fn rewrite(file: &mut std::fs::File, doc: &ClientsFile) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(doc)?;
    file.set_len(0)?;
    file.rewind()?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
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
        changes: Option<RunChanges>,
        /// Why the changes could not be read (then `changes` is `None` and
        /// nothing is committed): never read as "no changes".
        changes_error: Option<String>,
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
    /// (#3468) The `code` its run's `dispatch.error` named, as written
    /// (`busy`, `wont_fit`, ...): read from the flow, never from `error`.
    pub code: Option<String>,
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
    /// The work copy as host git may touch it: the repository darkmux holds
    /// for it, outside it (`None`: no workspace darkmux made, so no git).
    pub git: Option<WorkCopy>,
    /// The work copy as a tree before the run (or before the first run of a
    /// chain of tweaks): what Keep commits against and Discard restores to.
    pub baseline: Option<String>,
    /// Undone (its commit reverted): decided once.
    pub decided: Option<String>,
    /// The run this one follows up on (a Tweak).
    pub continues: Option<String>,
    /// The commit message's title.
    pub title: String,
    pub role: String,
    /// The run's commit (full id) on the work copy's branch, the files it
    /// holds, the branch, or why it couldn't be made.
    pub commit: Option<String>,
    pub files: Vec<String>,
    pub branch: Option<String>,
    pub commit_error: Option<String>,
}

/// (url, ref, dest) -> cloned, or why not.
/// (url, ref, work tree, git dir) -> cloned, the repository in `git dir`
/// (`--separate-git-dir`), outside the work tree a run is given; or why not.
pub type CloneRepo = dyn Fn(&str, Option<&str>, &Path, &Path) -> Result<(), String> + Send + Sync;

/// Everything the listener needs, injectable so tests drive the real router
/// with a fake clock and a fake executor.
/// A yes/no about a name (a role, a profile); `Err` when it can't be told.
pub type NameCheck = Arc<dyn Fn(&str) -> Result<bool, String> + Send + Sync>;
/// (role, requested profile) -> the profile that would run.
pub type ResolveProfile = Arc<dyn Fn(&str, Option<&str>) -> Result<String, String> + Send + Sync>;
/// `clients.profiles`: `Ok(None)` when unset.
pub type ProfilesPolicy = Arc<dyn Fn() -> Result<Option<Vec<String>>, String> + Send + Sync>;
/// (role, names) -> each named profile's description for that role, in order.
pub type DescribeProfiles =
    Arc<dyn Fn(&str, &[String]) -> Vec<Result<ClientProfileInfo, String>> + Send + Sync>;
/// (role, profile) -> a yes/no about that pair; `Err` when it can't be told.
pub type RoleProfileCheck = Arc<dyn Fn(&str, &str) -> Result<bool, String> + Send + Sync>;
/// A list of names (the registry's profiles).
pub type Lister = Arc<dyn Fn() -> Result<Vec<String>, String> + Send + Sync>;
/// A path check that returns the canonical path.
pub type PathCheck = Arc<dyn Fn(&Path) -> Result<PathBuf, String> + Send + Sync>;
/// A path this machine knows (the worktrees base).
pub type PathSource = Arc<dyn Fn() -> Result<PathBuf, String> + Send + Sync>;
/// Something done to a path (a fetch).
pub type PathAction = Arc<dyn Fn(&Path) -> Result<(), crate::client_workspace::Coded> + Send + Sync>;
/// `clients.preview.enabled`, read per request.
pub type PreviewSwitch = Arc<dyn Fn() -> Result<bool, String> + Send + Sync>;
/// `clients.preview.idle_minutes`, read per idle check.
pub type PreviewMinutes = Arc<dyn Fn() -> Result<u64, String> + Send + Sync>;
/// (role, work id) -> what that running work's dispatch is doing besides its
/// turns, as the run lifecycle decides it ([`work_activity_in`]).
pub type WorkActivity = Arc<dyn Fn(&str, &str) -> (Option<crate::runs::RunActivity>, Option<u64>) + Send + Sync>;
/// Runs one role execution, blocking.
pub type Execute = Arc<dyn Fn(WorkSpec, Arc<CancelFlag>) -> Result<Value, RunError> + Send + Sync>;

#[derive(Clone)]
pub struct ClientServerState {
    pub store: Arc<ClientStore>,
    /// Unix seconds.
    pub now: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// Whether a role is defined on this machine.
    pub role_known: NameCheck,
    /// (role, requested profile) -> the profile that would run, resolved as
    /// `darkmux dispatch <role> [--profile <p>]` resolves it; `Err` when
    /// nothing runnable resolves (a requested name never falls back).
    pub resolve_profile: ResolveProfile,
    /// This machine's `clients.profiles`, read per request: `Ok(None)` when
    /// unset (any local profile), `Err` when it cannot be
    /// read (work is then refused, never run on a guessed policy).
    pub profiles_policy: ProfilesPolicy,
    /// Each named profile's description, the model the role's work would
    /// run on, endpoint kind, and whether it answers now (`GET
    /// /client/profiles`), for (role, names), in order; `Err` for a name the
    /// registry doesn't have. One call per request, so the registry and each
    /// server are looked at once, not once per profile.
    pub profile_info: DescribeProfiles,
    /// (role, profile) -> whether that role's work on that profile stays on
    /// this machine: the resolved target's own classification, the one
    /// dispatch routes on (`crate::client_profiles::work_stays_here`). With
    /// `clients.profiles` unset, an app may use any local profile (the role's
    /// own included, judged the same way); a hosted one only when granted.
    /// `Err` refuses.
    pub profile_is_local: RoleProfileCheck,
    /// Every profile name in this machine's registry.
    pub registry_profiles: Lister,
    /// This machine, as a paired app keys its per-machine settings: a stable
    /// id (the same from the Mac app and the phone) and a display name.
    pub machine: ClientMachine,
    /// The workdir containment check; returns the canonical path.
    pub check_workdir: PathCheck,
    /// Runs one role execution, blocking; `Ok` is the dispatch's `--json` envelope.
    /// It watches the [`CancelFlag`] and stops the run when it is raised.
    pub execute: Execute,
    pub works: Arc<Mutex<HashMap<String, WorkEntry>>>,
    /// (#3467) What a running work item's dispatch is doing besides its
    /// turns (`starting_model`), read per poll.
    pub work_activity: WorkActivity,
    /// The canonical worktrees base workspaces live under.
    pub worktrees_base: PathSource,
    /// (url, ref, dest): clones a repo into `dest`, which must not exist.
    pub clone_repo: Arc<CloneRepo>,
    /// Updates a workspace's remote-tracking refs; never its branch or files.
    pub fetch_repo: PathAction,
    /// One workspace creation at a time, so two requests never race for a name.
    pub workspace_lock: Arc<Mutex<()>>,
    /// Workspaces' live previews (dev servers from `.darkmux/preview.json`).
    pub previews: Arc<crate::client_preview::Previews>,
    /// The address the listener bound, so a `Host` naming another port is
    /// not this listener's ([`crate::is_local_at`]); `None` until bound.
    pub bound: Option<SocketAddr>,
    /// This machine's `clients.preview.enabled`, read per request (`Err`:
    /// unreadable, so previews are refused).
    pub preview_enabled: PreviewSwitch,
    /// This machine's `clients.preview.idle_minutes`, read at every idle
    /// check (`Err`: unreadable, so that check stops nothing).
    pub preview_idle_minutes: PreviewMinutes,
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
            profile_info: Arc::new(crate::client_profiles::describe_profiles),
            profile_is_local: Arc::new(crate::client_profiles::work_stays_here),
            registry_profiles: Arc::new(|| {
                darkmux_profiles::profiles::load_registry_quiet(None)
                    .map(|l| l.registry.profiles.keys().cloned().collect())
                    .map_err(|e| format!("{e:#}"))
            }),
            machine: ClientMachine::this_one(),
            check_workdir: Arc::new(|p| {
                darkmux_types::workdir::validate_remote_workdir(p).map_err(|e| format!("{e:#}"))
            }),
            execute: Arc::new(move |spec, cancel| {
                run_dispatch_child(&exe, &spec, &cancel, CANCEL_GRACE, &ProductionTeardown)
            }),
            works: Arc::new(Mutex::new(HashMap::new())),
            work_activity: Arc::new(|role, work_id| {
                work_activity_in(&WORK_ACTIVITY_RECORDS, &darkmux_flow::flows_dir(), role, work_id)
            }),
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
            bound: None,
            preview_enabled: Arc::new(read_preview_enabled),
            preview_idle_minutes: Arc::new(read_preview_idle_minutes),
        })
    }
}

/// `clients.profiles` from `config.json` as it is NOW (the process-wide
/// config tier is read once, and `config set` must take effect without a
/// restart), env first. A missing file is "unset"; one that cannot be read or
/// parsed refuses rather than falling back to a policy nobody wrote.
fn read_profiles_policy() -> Result<Option<Vec<String>>, String> {
    read_profiles_policy_at(
        &darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser).config,
    )
}

/// The config gate `client serve` runs before it listens, the same one
/// `darkmux serve` runs (`config_enum::preflight`, which walks `config.json`
/// with the user-file key gate): a misspelled or wrong-typed key refuses the
/// start, naming the key and the closest valid one.
pub fn preflight() -> Result<()> {
    darkmux_types::config_enum::preflight(darkmux_types::config_enum::Scope::ClientServe)
        .map_err(|e| anyhow!("{e}"))
}

/// [`read_profiles_policy`] against the config at `path`.
pub(crate) fn read_profiles_policy_at(path: &Path) -> Result<Option<Vec<String>>, String> {
    read_config_at(path).map(|cfg| darkmux_types::config_access::clients_profiles_in(&cfg))
}

/// `clients.preview.enabled` from `config.json` as it is now, env first;
/// absent is off.
fn read_preview_enabled() -> Result<bool, String> {
    read_preview_enabled_at(
        &darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser).config,
    )
}

/// [`read_preview_enabled`] against the config at `path`.
pub(crate) fn read_preview_enabled_at(path: &Path) -> Result<bool, String> {
    read_config_at(path).map(|cfg| darkmux_types::config_access::clients_preview_enabled_in(&cfg))
}

/// `clients.preview.idle_minutes` from `config.json` as it is now, env
/// first; absent is 15.
fn read_preview_idle_minutes() -> Result<u64, String> {
    read_preview_idle_minutes_at(
        &darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser).config,
    )
}

/// [`read_preview_idle_minutes`] against the config at `path`.
pub(crate) fn read_preview_idle_minutes_at(path: &Path) -> Result<u64, String> {
    read_config_at(path)
        .map(|cfg| darkmux_types::config_access::clients_preview_idle_minutes_in(&cfg))
}

/// `config.json` at `path`, read now through the config gate (a missing file
/// is the defaults; one that cannot be read, parse or pass the gate refuses).
fn read_config_at(path: &Path) -> Result<darkmux_types::config::DarkmuxConfig, String> {
    let cfg = match std::fs::read_to_string(path) {
        Ok(text) => {
            // The same gate as the start's: a key written while serving is
            // checked too, so a misspelling refuses instead of reading as unset.
            if let Some(problem) = darkmux_types::user_files::config_check_text(path, &text).refusal {
                return Err(format!("{problem}; refusing client work"));
            }
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
    Ok(cfg)
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
    preflight()?;
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
        let mut state = state;
        state.bound = Some(listener.local_addr()?);
        let shutdown_previews = state.previews.clone();
        // (#3456) Preview containers a `client serve` that died left behind.
        let reaper = state.previews.clone();
        std::thread::spawn(move || {
            for name in reaper.reap_orphans() {
                eprintln!("darkmux client serve: removed orphaned preview container {name} (its darkmux host is gone)");
            }
        });
        // (#3459) Previews no app asks about any more are stopped.
        let idle_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // (#3456 review F2) Its preview containers stop themselves once
        // this heartbeat goes stale (a `client serve` killed without its
        // handlers).
        let _heartbeat = crate::client_preview::spawn_heartbeat(
            state.previews.clone(),
            crate::client_preview::BEAT_EVERY,
            idle_stop.clone(),
        );
        let _idle_check = spawn_preview_idle_check(
            state.clone(),
            PREVIEW_IDLE_CHECK_INTERVAL,
            idle_stop.clone(),
        );
        let served = axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("serving");
        idle_stop.store(true, std::sync::atomic::Ordering::SeqCst);
        // (#3456) Shutting down removes every preview container.
        let previews = shutdown_previews.clone();
        let removed = tokio::task::spawn_blocking(move || previews.stop_all())
            .await
            .unwrap_or_default();
        for name in removed {
            eprintln!("{}", preview_log_line(&name, "shutdown", "stopped"));
        }
        served
    })
}

/// The listener's router, built from the client route table
/// (`routes::client_table`), behind [`only_this_machines_apps`].
pub fn router(state: ClientServerState) -> Router {
    crate::routes::client_router(state)
}

/// A JSON 404 for a path the table does not list.
pub(crate) async fn no_such_route() -> Response {
    err(StatusCode::NOT_FOUND, ClientCode::NotFound, "no such route")
}

/// A JSON 405 for a method a listed path does not take.
pub(crate) async fn method_not_allowed() -> Response {
    err(StatusCode::METHOD_NOT_ALLOWED, ClientCode::MethodNotAllowed, "method not allowed")
}

/// The gate in front of every route: the request must come from this machine
/// the way the viewer daemon decides it ([`crate::is_local_at`]: a loopback
/// peer, no reverse-proxy header, a `Host` naming this listener), must not
/// come from a web page (it carries no `Origin`; a native app sends none,
/// and every browser sends one on a cross-site POST), and a POST with a body
/// must say `Content-Type: application/json` (a form post a page can send
/// without asking is `text/plain` or `application/x-www-form-urlencoded`).
pub(crate) async fn only_this_machines_apps(
    State(state): State<ClientServerState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    match foreign_request(&req, state.bound) {
        Some((status, code, why)) => err(status, code, why),
        None => next.run(req).await,
    }
}

/// Why [`only_this_machines_apps`] refuses `req`, or `None` to let it in.
fn foreign_request(
    req: &axum::extract::Request,
    bound: Option<SocketAddr>,
) -> Option<(StatusCode, ClientCode, &'static str)> {
    use axum::http::header::ORIGIN;
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    if !crate::is_local_at(peer, req.headers(), bound) {
        return Some((
            StatusCode::FORBIDDEN,
            ClientCode::NotLocal,
            "darkmux client serve answers only apps on this machine (a loopback connection naming this listener, through no proxy)",
        ));
    }
    if req.headers().contains_key(ORIGIN) {
        return Some((
            StatusCode::FORBIDDEN,
            ClientCode::NotLocal,
            "a request from a web page (it carries an Origin header) is refused: only an app on this machine may use darkmux client serve",
        ));
    }
    if req.method() == axum::http::Method::POST && has_body(req) && !is_json(req) {
        return Some((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ClientCode::UnsupportedMediaType,
            "a request body must be Content-Type: application/json",
        ));
    }
    None
}

/// Whether the request carries a body (an empty one, or a declared zero
/// length, does not count).
fn has_body(req: &axum::extract::Request) -> bool {
    use axum::body::HttpBody;
    req.body().size_hint().exact() != Some(0)
}

/// Whether the request says its body is JSON.
fn is_json(req: &axum::extract::Request) -> bool {
    req.headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"))
}

fn err(status: StatusCode, code: ClientCode, msg: impl Into<String>) -> Response {
    (status, Json(ClientError { error: msg.into(), code })).into_response()
}

/// (#3468) A path parameter, as every client route takes it: one the router
/// cannot read (a segment that is not UTF-8) is refused as a coded JSON 400
/// like every other refusal, never axum's plain-text rejection.
pub(crate) struct ClientPath<T>(pub T);

#[axum::async_trait]
impl<T, S> axum::extract::FromRequestParts<S> for ClientPath<T>
where
    T: serde::de::DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut axum::http::request::Parts, state: &S) -> Result<Self, Response> {
        use axum::response::IntoResponse as _;
        match UrlPath::<T>::from_request_parts(parts, state).await {
            Ok(UrlPath(v)) => Ok(ClientPath(v)),
            Err(e) => {
                let text = e.body_text();
                let _ = e.into_response();
                Err(err(StatusCode::BAD_REQUEST, ClientCode::BadRequest, text))
            }
        }
    }
}

/// (#3468) What a handler that panicked outside `spawn_blocking` answers:
/// a coded 500, never a dropped connection.
pub(crate) fn panic_response(_panic: Box<dyn std::any::Any + Send + 'static>) -> Response {
    eprintln!("darkmux client serve: a handler panicked");
    err(StatusCode::INTERNAL_SERVER_ERROR, ClientCode::Internal, "internal error; see the darkmux client serve log")
}

/// A refusal on its way back to a handler, boxed: a `Response` is large, and
/// these travel through `Result`s.
type Refused = Box<Response>;

fn refuse(status: StatusCode, code: ClientCode, msg: impl Into<String>) -> Refused {
    Box::new(err(status, code, msg))
}

fn not_found() -> Response {
    err(StatusCode::NOT_FOUND, ClientCode::NotFound, "not found")
}

fn internal(e: anyhow::Error) -> Response {
    eprintln!("darkmux client serve: {e:#}");
    err(
        StatusCode::INTERNAL_SERVER_ERROR, ClientCode::Internal,
        "internal error; see the darkmux client serve log",
    )
}

/// Read and parse a capped JSON body; every refusal is JSON.
async fn json_body<T: serde::de::DeserializeOwned>(body: Body) -> Result<T, Response> {
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| {
            err(
                StatusCode::PAYLOAD_TOO_LARGE, ClientCode::PayloadTooLarge,
                format!("body larger than {MAX_BODY_BYTES} bytes"),
            )
        })?;
    serde_json::from_slice(&bytes)
        .map_err(|e| err(StatusCode::BAD_REQUEST, ClientCode::BadRequest, format!("bad JSON body: {e}")))
}

fn clean_text(s: &str, field: &str, min: usize, max: usize) -> Result<(), Refused> {
    let n = s.chars().count();
    if n < min || n > max || s.chars().any(char::is_control) {
        return Err(refuse(
            StatusCode::BAD_REQUEST, ClientCode::BadRequest,
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

pub(crate) async fn pair_handler(State(state): State<ClientServerState>, body: Body) -> Response {
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
        return *r;
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
            Json(PairAccepted { request_id }),
        )
            .into_response(),
        Ok(false) => err(
            StatusCode::TOO_MANY_REQUESTS, ClientCode::TooManyRequests,
            "too many pairing requests are waiting; try again later",
        ),
        Err(e) => internal(e),
    }
}

/// What a poll answers, decided under the store's lock.
/// One poll of a pairing request: its status, or (once approved) the token,
/// minted now and collected exactly once.
fn poll_pending(f: &mut ClientsFile, request_id: &str, secret: &str, now: u64) -> Result<PollOutcome> {
    prune(f, now);
    let Some(i) = f.pending.iter().position(|p| p.request_id == request_id) else {
        return Ok(PollOutcome::NotFound);
    };
    if !secret_matches(&f.pending[i].pair_secret_hash, secret) {
        return Ok(PollOutcome::NotFound);
    }
    let p = f.pending[i].clone();
    if p.status == PendingStatus::Denied {
        return Ok(PollOutcome::Status(PairPoll::Denied));
    }
    if p.expired(now) {
        return Ok(PollOutcome::Status(PairPoll::Expired));
    }
    if p.status == PendingStatus::Pending {
        return Ok(PollOutcome::Status(PairPoll::Pending));
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
}

enum PollOutcome {
    NotFound,
    Status(PairPoll),
    Approved { client_name: String, token: String },
}

fn policy_or_503(state: &ClientServerState) -> Result<Option<Vec<String>>, Refused> {
    (state.profiles_policy)().map_err(|e| refuse(StatusCode::SERVICE_UNAVAILABLE, ClientCode::Unavailable, e))
}

pub(crate) async fn poll_handler(
    State(state): State<ClientServerState>,
    ClientPath(request_id): ClientPath<String>,
    headers: HeaderMap,
) -> Response {
    let Some(secret) = headers.get("x-pair-secret").and_then(|v| v.to_str().ok()) else {
        return not_found();
    };
    // Read before anything is consumed, so a policy that cannot be read
    // never costs the app its one token delivery.
    let policy = match policy_or_503(&state) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let now = (state.now)();
    let outcome = state.store.update(|f| poll_pending(f, &request_id, secret, now));
    match outcome {
        Ok(PollOutcome::NotFound) => not_found(),
        Ok(PollOutcome::Status(s)) => Json(s).into_response(),
        Ok(PollOutcome::Approved { client_name, token }) => Json(PairPoll::Approved {
            client_name,
            token,
            scopes: ClientScopes::of(&policy),
        })
        .into_response(),
        Err(e) => internal(e),
    }
}

/// The client the bearer token names. Every stored hash is compared (no early
/// exit), each in constant time.
fn authenticate(
    state: &ClientServerState,
    headers: &HeaderMap,
) -> Result<ApprovedClient, Refused> {
    let unauthorized = || err(StatusCode::UNAUTHORIZED, ClientCode::Unauthorized, "missing or unknown bearer token");
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
    found.ok_or_else(|| Box::new(unauthorized()))
}

/// This machine, as `/client/whoami` and `/client/profiles` name it.
#[derive(Debug, Clone, Serialize, PartialEq, schemars::JsonSchema)]
pub struct ClientMachine {
    /// `m-` and 16 hex digits of a hash of the hardware uuid (never the
    /// uuid itself): stable across renames and every device that pairs.
    pub id: String,
    /// The fleet name (`DARKMUX_MACHINE_ID`, else the hostname).
    pub name: String,
}

impl ClientMachine {
    pub fn from_parts(hardware_uid: Option<&str>, name: &str) -> ClientMachine {
        let basis = hardware_uid
            .map(|u| format!("uid:{u}"))
            .unwrap_or_else(|| format!("name:{name}"));
        let hash = blake3::hash(format!("darkmux client machine {basis}").as_bytes())
            .to_hex()
            .to_string();
        ClientMachine {
            id: format!("m-{}", &hash[..16]),
            name: name.to_string(),
        }
    }

    /// This process's machine.
    pub fn this_one() -> ClientMachine {
        let name = darkmux_flow::resolve_machine_id().unwrap_or_else(|| "this machine".to_string());
        ClientMachine::from_parts(darkmux_hardware::machine_uid(), &name)
    }
}

pub use crate::client_profiles::ClientProfileInfo;

#[derive(Deserialize)]
pub(crate) struct ProfilesQuery {
    role: Option<String>,
}

/// `GET /client/profiles[?role=coder]`: the profiles this app may run work
/// on here, in the order `clients.profiles` grants them (unset: the
/// role's own first, then every other local one), each with its model and
/// whether it answers now. Read-only;
/// an app picks among these per run.
pub(crate) async fn profiles_handler(
    State(state): State<ClientServerState>,
    query: axum::extract::Query<ProfilesQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authenticate(&state, &headers) {
        return *r;
    }
    let role = query.role.clone().unwrap_or_else(|| "coder".to_string());
    let names = match listable_profiles(&state, &role) {
        Ok(names) => names,
        Err(r) => return *r,
    };
    let st = state.clone();
    let asked = role.clone();
    let profiles = tokio::task::spawn_blocking(move || describe_listed(&st, &asked, &names))
        .await
        .unwrap_or_default();
    Json(ClientProfiles {
        role,
        profiles,
        machine: state.machine.clone(),
    })
    .into_response()
}

/// The profiles this app may run `role`'s work on here, in the order
/// `clients.profiles` grants them; unset: the role's own first when it is
/// local, then every other local profile work can run on.
fn listable_profiles(state: &ClientServerState, role: &str) -> Result<Vec<String>, Refused> {
    match (state.role_known)(role) {
        Ok(true) => {}
        Ok(false) => {
            return Err(refuse(
                StatusCode::BAD_REQUEST, ClientCode::BadRequest,
                format!("no role {role} on this machine"),
            ))
        }
        Err(e) => return Err(Box::new(internal(anyhow!(e)))),
    }
    local_profiles(state, role)
}

fn local_profiles(state: &ClientServerState, role: &str) -> Result<Vec<String>, Refused> {
    if let Some(list) = policy_or_503(state)? {
        return Ok(list);
    }
    let own = (state.resolve_profile)(role, None).map_err(forbidden)?;
    let all = (state.registry_profiles)().map_err(|e| Box::new(internal(anyhow!(e))))?;
    // The role's own is listed only when it runs here too.
    let mut names: Vec<String> = matches!((state.profile_is_local)(role, &own), Ok(true))
        .then(|| own.clone())
        .into_iter()
        .collect();
    names.extend(all.into_iter().filter(|n| {
        n != &own
            && !n.contains('@')
            && matches!((state.profile_is_local)(role, n), Ok(true))
            && (state.resolve_profile)(role, Some(n)).is_ok()
    }));
    Ok(names)
}

/// Each listed profile described; one the registry can't describe is listed
/// with its error, never dropped.
fn describe_listed(state: &ClientServerState, role: &str, names: &[String]) -> Vec<ClientProfileInfo> {
    let infos = (state.profile_info)(role, names);
    names
        .iter()
        .zip(infos)
        .map(|(name, info)| info.unwrap_or_else(|e| ClientProfileInfo::unknown(name, e)))
        .collect()
}

pub(crate) async fn whoami_handler(State(state): State<ClientServerState>, headers: HeaderMap) -> Response {
    let client = match authenticate(&state, &headers) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    match policy_or_503(&state) {
        Ok(policy) => {
            Json(Whoami {
                client_name: client.name,
                scopes: ClientScopes::of(&policy),
                machine: state.machine.clone(),
            })
            .into_response()
        }
        Err(r) => *r,
    }
}

/// A client-named workdir under the worktrees base
/// ([`ClientServerState::check_workdir`], which canonicalizes, so a relative
/// path is judged by where it really lands); the canonical path, or a 403.
fn checked_workdir(state: &ClientServerState, w: &str) -> Result<PathBuf, Refused> {
    (state.check_workdir)(Path::new(w)).map_err(|e| refuse(StatusCode::FORBIDDEN, ClientCode::Forbidden, e))
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
    /// A Tweak: the id of this client's finished run in the same work copy
    /// that this run follows up on.
    #[serde(default)]
    continues: Option<String>,
    /// A short title for the run's commit message (default: the message's
    /// first line).
    #[serde(default)]
    title: Option<String>,
}

pub(crate) async fn work_handler(
    State(state): State<ClientServerState>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let spec = match admitted_work(&state, &headers, body).await {
        Ok(spec) => spec,
        Err(r) => return *r,
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
    tokio::task::spawn_blocking(move || run_work(&execute, &works, spec, &cancel));
    (
        StatusCode::ACCEPTED,
        Json(WorkAccepted { work_id, profile }),
    )
        .into_response()
}

/// The request, authenticated and admitted ([`admit`]), off the async
/// workers: the checks read the role library, the registry, config.json and
/// the disk.
async fn admitted_work(
    state: &ClientServerState,
    headers: &HeaderMap,
    body: Body,
) -> Result<WorkSpec, Refused> {
    let client = authenticate(state, headers)?;
    let req: WorkBody = json_body(body).await.map_err(Box::new)?;
    if req.message.trim().is_empty() {
        return Err(refuse(StatusCode::BAD_REQUEST, ClientCode::BadRequest, "`message` is empty"));
    }
    let st = state.clone();
    match tokio::task::spawn_blocking(move || admit(&st, &client, req)).await {
        Ok(admitted) => admitted,
        Err(e) => Err(Box::new(internal(anyhow!("work admission panicked: {e}")))),
    }
}

/// Runs one admitted work item to its end, blocking, and records how it went.
fn run_work(
    execute: &Execute,
    works: &Mutex<HashMap<String, WorkEntry>>,
    spec: WorkSpec,
    cancel: &Arc<CancelFlag>,
) {
    let id = spec.work_id.clone();
    // Host git touches only the repository darkmux holds for the work copy.
    let git = works.lock().unwrap().get(&id).and_then(|e| e.git.clone());
    // The work copy as it is before the run, so what it held already is
    // never reported as the run's change. A run in the same work copy
    // as this client's undecided finished run continues it (a tweak):
    // it inherits that run's baseline, and that run is folded into this.
    let baseline = git.as_ref().map(snapshot_tree);
    if let Some(e) = works.lock().unwrap().get_mut(&id) {
        e.baseline = baseline.as_ref().and_then(|b| b.as_ref().ok()).map(|b| b.tree.clone());
    }
    let flag = cancel.clone();
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| execute(spec, flag)));
    let status = run_status(ran, cancel, git.as_ref(), baseline.as_ref());
    // A finished run's changes become one commit on the work copy's
    // branch: a checkpoint Undo can revert.
    let changed = changed_paths(&status);
    let (title, role) = works
        .lock()
        .unwrap()
        .get(&id)
        .map(|e| (e.title.clone(), e.role.clone()))
        .unwrap_or_default();
    let committed = match (&git, changed.is_empty()) {
        (Some(w), false) => Some(commit_run(
            w,
            &changed,
            &format!("{role}: {title} (work {id})"),
        )),
        _ => None,
    };
    if let Some(w) = works.lock().unwrap().get_mut(&id) {
        w.status = status;
        w.files = changed;
        match committed {
            Some(Ok((sha, branch))) => {
                w.commit = Some(sha);
                w.branch = Some(branch);
            }
            Some(Err(e)) => w.commit_error = Some(e),
            None => {}
        }
    }
}

/// What a finished run changed, or why that could not be read. No work
/// copy (or none darkmux holds) is neither.
fn run_changes(
    git: Option<&WorkCopy>,
    baseline: Option<&Result<Snapshot, String>>,
) -> (Option<RunChanges>, Option<String>) {
    match (git, baseline) {
        (Some(w), Some(Ok(before))) => match changes_since(w, &before.tree) {
            Ok(c) => (Some(c), None),
            Err(e) => (None, Some(format!("reading the run's changes failed: {e}"))),
        },
        (Some(_), Some(Err(e))) => (None, Some(format!("reading the work copy before the run failed: {e}"))),
        _ => (None, None),
    }
}

/// How a run ended. A cancelled run ends as cancelled, however its child
/// reported.
fn run_status(
    ran: std::thread::Result<Result<Value, RunError>>,
    cancel: &CancelFlag,
    git: Option<&WorkCopy>,
    baseline: Option<&Result<Snapshot, String>>,
) -> WorkStatus {
    match ran {
        Ok(r) if cancel.requested() => WorkStatus::Cancelled(r.err()),
        Err(_) if cancel.requested() => WorkStatus::Cancelled(None),
        Ok(Ok(result)) => {
            let (changes, changes_error) = run_changes(git, baseline);
            WorkStatus::Done {
                result,
                changes,
                changes_error,
            }
        }
        Ok(Err(e)) => WorkStatus::Error(e),
        Err(_) => WorkStatus::Error(RunError {
            reason: "panic".to_string(),
            error: "the dispatch runner panicked".to_string(),
            code: None,
        }),
    }
}

/// The paths a finished run changed.
fn changed_paths(status: &WorkStatus) -> Vec<String> {
    match status {
        WorkStatus::Done {
            changes: Some(c), ..
        } => c.files.iter().map(|f| f.path.clone()).collect(),
        _ => Vec::new(),
    }
}

/// Every refusal a work request can meet, in order; on success the work is
/// registered as running and its spec (with the RESOLVED profile) returned.
fn admit(
    state: &ClientServerState,
    client: &ApprovedClient,
    req: WorkBody,
) -> Result<WorkSpec, Refused> {
    check_role(state, &req.role)?;
    let workdir = admitted_workdir(state, &req)?;
    let profile = admitted_profile(state, &req)?;
    let work_id = format!("w-{}", random_hex(8).map_err(internal)?);
    let now = (state.now)();
    let mut works = state.works.lock().unwrap();
    check_capacity(&mut works, client, now)?;
    check_work_copy(&works, client, workdir.as_ref(), req.continues.as_deref())?;
    works.insert(
        work_id.clone(),
        WorkEntry {
            client_name: client.name.clone(),
            created: now,
            status: WorkStatus::Running,
            cancel: Arc::new(CancelFlag::default()),
            git: workdir.as_deref().and_then(|w| work_copy_for(state, w)),
            workdir: workdir.clone(),
            baseline: None,
            decided: None,
            continues: req.continues.clone(),
            title: work_title(&req),
            role: req.role.clone(),
            commit: None,
            files: Vec::new(),
            branch: None,
            commit_error: None,
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

/// The work copy host git may touch for a canonical workdir: a workspace
/// directly under the worktrees base whose repository darkmux recorded.
fn work_copy_for(state: &ClientServerState, dir: &Path) -> Option<WorkCopy> {
    let base = (state.worktrees_base)().ok()?;
    let name = dir.file_name()?.to_str()?;
    if dir.parent()? != base {
        return None;
    }
    crate::client_git::work_copy_of(&base, name, dir)
}

/// The role must be defined on this machine.
fn check_role(state: &ClientServerState, role: &str) -> Result<(), Refused> {
    match (state.role_known)(role) {
        Ok(true) => Ok(()),
        Ok(false) => Err(refuse(
            StatusCode::BAD_REQUEST, ClientCode::BadRequest,
            format!("role {role} is not defined on this machine"),
        )),
        Err(e) => Err(Box::new(internal(anyhow!(e)))),
    }
}

/// Where the run works: a client-named workdir or a workspace, never both.
fn admitted_workdir(state: &ClientServerState, req: &WorkBody) -> Result<Option<PathBuf>, Refused> {
    match (&req.workdir, &req.workspace) {
        (Some(_), Some(_)) => Err(refuse(
            StatusCode::BAD_REQUEST, ClientCode::BadRequest,
            "send `workdir` or `workspace`, not both",
        )),
        // (#3451 review) A raw `workdir` must be exactly one workspace
        // directly under the base: not the base, not a dot entry beside the
        // workspaces (darkmux's held repositories and records), not a path
        // inside one. It is then admitted as that workspace by name.
        (Some(w), None) => {
            let name = workspace_of_workdir(state, w)?;
            Ok(Some(workspace_dir(state, &name)?))
        }
        (None, Some(name)) => Ok(Some(workspace_dir(state, name)?)),
        (None, None) => Ok(None),
    }
}

fn forbidden(e: impl Into<String>) -> Refused {
    refuse(StatusCode::FORBIDDEN, ClientCode::Forbidden, e)
}

/// The profile that would run, if this machine lets a paired app use it:
/// never another machine's, and only a granted one (with `clients.profiles`
/// unset: any local one).
fn admitted_profile(state: &ClientServerState, req: &WorkBody) -> Result<String, Refused> {
    let policy = policy_or_503(state)?;
    let profile = (state.resolve_profile)(&req.role, req.profile.as_deref()).map_err(forbidden)?;
    if profile.contains('@') {
        return Err(forbidden(format!(
            "profile {profile} names another machine; client work runs here only"
        )));
    }
    match &policy {
        Some(allowed) => check_granted(allowed, &profile)?,
        // Unset: any profile that runs on this machine, the role's own (an
        // omitted profile) judged exactly like a named one. A hosted one
        // needs to be granted by name.
        None => check_local(state, &req.role, &profile)?,
    }
    Ok(profile)
}

fn check_granted(allowed: &[String], profile: &str) -> Result<(), Refused> {
    if allowed.iter().any(|p| p == profile) {
        return Ok(());
    }
    let list = if allowed.is_empty() {
        "empty".to_string()
    } else {
        allowed.join(", ")
    };
    Err(forbidden(format!(
        "profile {profile} is not in this machine's clients.profiles ({list})"
    )))
}

fn check_local(state: &ClientServerState, role: &str, profile: &str) -> Result<(), Refused> {
    match (state.profile_is_local)(role, profile) {
        Ok(true) => Ok(()),
        Ok(false) => Err(forbidden(format!(
            "profile {profile} isn't local (it sends work off this machine); \
             `darkmux config set clients.profiles <list>` can grant it"
        ))),
        Err(e) => Err(forbidden(e)),
    }
}

/// Forgets finished work past its keep time, then refuses a client already
/// at its cap of running work.
fn check_capacity(
    works: &mut HashMap<String, WorkEntry>,
    client: &ApprovedClient,
    now: u64,
) -> Result<(), Refused> {
    works.retain(|_, w| {
        matches!(w.status, WorkStatus::Running) || now.saturating_sub(w.created) <= KEEP_WORK_SECS
    });
    let running = works
        .values()
        .filter(|w| w.client_name == client.name && matches!(w.status, WorkStatus::Running))
        .count();
    if running >= MAX_RUNNING_PER_CLIENT {
        return Err(refuse(
            StatusCode::TOO_MANY_REQUESTS, ClientCode::TooManyRequests,
            format!(
                "client {} already has {running} work items running",
                client.name
            ),
        ));
    }
    Ok(())
}

/// One run at a time per work copy, and never two runs' uncommitted
/// changes mixed: a Tweak names the finished run of yours it continues.
/// Every finished run's changes are committed, so a new run needs no
/// decision first.
fn check_work_copy(
    works: &HashMap<String, WorkEntry>,
    client: &ApprovedClient,
    workdir: Option<&PathBuf>,
    continues: Option<&str>,
) -> Result<(), Refused> {
    let Some(w) = workdir else {
        return match continues {
            Some(_) => Err(refuse(
                StatusCode::CONFLICT, ClientCode::Conflict,
                "only a run in a work copy can continue another",
            )),
            None => Ok(()),
        };
    };
    if let Some((id, _)) = works
        .iter()
        .find(|(_, e)| e.workdir.as_ref() == Some(w) && matches!(e.status, WorkStatus::Running))
    {
        return Err(refuse(
            StatusCode::CONFLICT, ClientCode::Conflict,
            format!("run {id} is already working in this work copy"),
        ));
    }
    if let Some(c) = continues {
        let ok = works.get(c).is_some_and(|e| {
            e.client_name == client.name
                && e.workdir.as_ref() == Some(w)
                && matches!(e.status, WorkStatus::Done { .. })
        });
        if !ok {
            return Err(refuse(
                StatusCode::CONFLICT, ClientCode::Conflict,
                format!("run {c} isn't a finished run of yours in this work copy"),
            ));
        }
    }
    Ok(())
}

/// The run's title: the one sent, else the message's first line (72 chars).
fn work_title(req: &WorkBody) -> String {
    req.title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| req.message.lines().next().unwrap_or("").chars().take(72).collect())
}

pub use crate::client_preview::MAX_RECIPE_BYTES;

#[derive(Deserialize)]
pub(crate) struct RecipeQuery {
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
}

/// `GET /client/recipe?workspace=<name>` (or `workdir=<the workspace's
/// directory>`): the workspace's preview recipe, read and checked exactly as
/// a preview start reads it: from the user's own checkout the workspace was
/// made from, never from the workspace.
pub(crate) async fn recipe_handler(
    State(state): State<ClientServerState>,
    headers: HeaderMap,
    query: Result<axum::extract::Query<RecipeQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(r) = authenticate(&state, &headers) {
        return *r;
    }
    let q = match query {
        Ok(axum::extract::Query(q)) if q.workdir.is_some() != q.workspace.is_some() => q,
        _ => {
            return err(
                StatusCode::BAD_REQUEST, ClientCode::BadRequest,
                "one of the `workdir` or `workspace` query parameters is required",
            )
        }
    };
    let st = state.clone();
    tokio::task::spawn_blocking(move || {
        let name = match (&q.workdir, &q.workspace) {
            (Some(w), _) => workspace_of_workdir(&st, w),
            (_, Some(n)) => Ok(n.clone()),
            _ => unreachable!("checked above"),
        };
        match name.and_then(|n| workspace_and_recipe_source(&st, &n)) {
            Ok((_dir, source)) => read_recipe(&source),
            Err(r) => *r,
        }
    })
    .await
    .unwrap_or_else(|e| internal(anyhow!("recipe read panicked: {e}")))
}

/// The workspace a client-named workdir is: a directory directly under the
/// worktrees base (after the containment check); 404 for any other.
fn workspace_of_workdir(state: &ClientServerState, w: &str) -> Result<String, Refused> {
    let dir = checked_workdir(state, w)?;
    let base = (state.worktrees_base)().map_err(|e| Box::new(internal(anyhow!(e))))?;
    match (dir.parent(), dir.file_name().and_then(|n| n.to_str())) {
        (Some(parent), Some(name)) if parent == base => Ok(name.to_string()),
        _ => Err(refuse(
            StatusCode::NOT_FOUND, ClientCode::NotFound,
            format!("{} is not a workspace, so it has no recipe", dir.display()),
        )),
    }
}

/// The recipe from its trusted `source`, checked, as `{"recipe": {..}}`.
fn read_recipe(source: &crate::client_preview::RecipeSource) -> Response {
    match source.load() {
        Ok(recipe) => Json(RecipeReply { recipe }).into_response(),
        Err(e) => recipe_refusal(source, &no_recipe_in(source, e)),
    }
}

// ─── Workspaces ──────────────────────────────────────────────────────────

/// A workspace name resolved to its canonical directory under the worktrees
/// base: 400 for a name that isn't one plain segment, 404 when there is no
/// such workspace, 403 when it fails containment (a symlink out, say).
fn workspace_dir(state: &ClientServerState, name: &str) -> Result<PathBuf, Refused> {
    use crate::client_workspace::valid_name;
    if !valid_name(name) {
        return Err(refuse(
            StatusCode::BAD_REQUEST, ClientCode::BadRequest,
            format!("{name:?} is not a workspace name"),
        ));
    }
    let base = (state.worktrees_base)().map_err(|e| internal(anyhow!(e)))?;
    let dir = base.join(name);
    if std::fs::symlink_metadata(&dir).is_err() {
        return Err(refuse(
            StatusCode::NOT_FOUND, ClientCode::NotFound,
            format!("no workspace named {name}"),
        ));
    }
    (state.check_workdir)(&dir).map_err(|e| refuse(StatusCode::FORBIDDEN, ClientCode::Forbidden, e))
}

fn workspace_json(state: &ClientServerState, name: &str, dir: &Path, created: bool) -> WorkspaceInfo {
    let wc = work_copy_for(state, dir);
    let (branch, head) = wc
        .as_ref()
        .and_then(crate::client_workspace::status_of)
        .unwrap_or_default();
    WorkspaceInfo {
        workspace: name.to_string(),
        branch,
        head,
        repo: wc.as_ref().and_then(crate::client_workspace::origin_of),
        created,
        from: None,
        fetch_error: None,
        fetch_error_code: None,
    }
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
    /// `repo_url` is a template: make a new project from it (at `ref`) with
    /// none of its history and no remote, named for the bot; never reuse a
    /// workspace.
    #[serde(default)]
    template: bool,
}

/// What a workspace request asks for.
enum WorkspaceAsk {
    Clone { url: String, normalized: String },
    Template { url: String, normalized: String },
    Worktree { path: String },
}

fn workspace_ask(req: &WorkspaceBody) -> Result<WorkspaceAsk, Refused> {
    use crate::client_workspace as ws;
    match (&req.repo_url, &req.source_path) {
        (Some(url), None) => {
            let Some(normalized) = ws::normalize_repo_url(url) else {
                return Err(refuse(
                    StatusCode::BAD_REQUEST, ClientCode::BadRequest,
                    "repo_url must be https://host/owner/repo or git@host:owner/repo",
                ));
            };
            let url = url.clone();
            Ok(if req.template {
                WorkspaceAsk::Template { url, normalized }
            } else {
                WorkspaceAsk::Clone { url, normalized }
            })
        }
        (None, Some(path)) if !req.template => Ok(WorkspaceAsk::Worktree { path: path.clone() }),
        (None, Some(_)) => Err(refuse(StatusCode::BAD_REQUEST, ClientCode::BadRequest, "a template needs `repo_url`, not `source_path`")),
        _ => Err(refuse(
            StatusCode::BAD_REQUEST, ClientCode::BadRequest,
            "send `repo_url` or `source_path`, not both or neither",
        )),
    }
}

/// `POST /client/workspaces`: `{repo_url}` reuses the workspace whose origin
/// is that repo (fetching it) or clones it; `{source_path}` reuses this bot's
/// worktree of that checkout or adds one on a new `mux/<bot>-<id>` branch, so
/// the owner's own working tree is never touched. 201 when created, 200 when
/// reused.
pub(crate) async fn workspace_create_handler(
    State(state): State<ClientServerState>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    use crate::client_workspace as ws;
    if let Err(r) = authenticate(&state, &headers) {
        return *r;
    }
    let req: WorkspaceBody = match json_body(body).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    if let Some(r) = req.git_ref.as_deref().filter(|r| !ws::valid_ref(r)) {
        return err(
            StatusCode::BAD_REQUEST, ClientCode::BadRequest,
            format!("{r:?} is not a branch name"),
        );
    }
    let bot = bot_slug(req.bot.as_deref());
    let ask = match workspace_ask(&req) {
        Ok(ask) => ask,
        Err(r) => return *r,
    };
    let st = state.clone();
    let git_ref = req.git_ref;
    tokio::task::spawn_blocking(move || make_workspace(&st, ask, git_ref.as_deref(), &bot))
        .await
        .unwrap_or_else(|e| internal(anyhow!("workspace setup panicked: {e}")))
}

/// The bot's name as it goes into workspace and branch names ("app" when
/// it has none).
fn bot_slug(bot: Option<&str>) -> String {
    let slug = crate::client_workspace::slug(bot.unwrap_or("app"));
    if slug.is_empty() {
        "app".to_string()
    } else {
        slug
    }
}

fn make_workspace(state: &ClientServerState, ask: WorkspaceAsk, git_ref: Option<&str>, bot: &str) -> Response {
    match ask {
        WorkspaceAsk::Clone { url, normalized } => clone_workspace(state, &url, &normalized, git_ref),
        WorkspaceAsk::Template { url, normalized } => template_workspace(state, &url, &normalized, git_ref, bot),
        WorkspaceAsk::Worktree { path } => worktree_workspace(state, Path::new(&path), git_ref, bot),
    }
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
        return existing_clone(state, &name);
    }
    let taken = |n: &str| {
        std::fs::symlink_metadata(base.join(n)).is_ok()
            || std::fs::symlink_metadata(base.join(format!(".{n}.partial"))).is_ok()
    };
    let Some(name) = ws::name_for(normalized, taken) else {
        return err(StatusCode::CONFLICT, ClientCode::Conflict, "no free workspace name for this repo");
    };
    if let Err(r) = place_clone(state, url, normalized, git_ref, &base, &name) {
        return *r;
    }
    match workspace_dir(state, &name) {
        Ok(dir) => (StatusCode::CREATED, Json(workspace_json(state, &name, &dir, true))).into_response(),
        Err(r) => *r,
    }
}

/// A new project from a template: cloned at `git_ref` beside its final
/// name, its history and remote dropped, and started over as a fresh repo
/// on `main` with one commit, so runs commit to it and nothing points back
/// at the template. Named for the bot, never reused.
fn template_workspace(
    state: &ClientServerState,
    url: &str,
    normalized: &str,
    git_ref: Option<&str>,
    bot: &str,
) -> Response {
    let base = match (state.worktrees_base)() {
        Ok(b) => b,
        Err(e) => return internal(anyhow!(e)),
    };
    let _one_at_a_time = state.workspace_lock.lock().unwrap();
    let Some(name) = free_project_name(&base, bot) else {
        return err(StatusCode::CONFLICT, ClientCode::Conflict, "no free workspace name for this project");
    };
    if let Err(r) = place_clone(state, url, normalized, git_ref, &base, &name) {
        return *r;
    }
    let dir = base.join(&name);
    let from = match git_ref {
        Some(r) => format!("{normalized}@{r}"),
        None => normalized.to_string(),
    };
    // (#3460) The first commit is darkmux's own: its recipe is the one a
    // preview of this project trusts, recorded beside the workspace.
    let started = start_fresh(&base, &name, &format!("Start from {from}"))
        .and_then(|commit| crate::client_workspace::record_template_commit(&base, &name, &commit));
    if let Err(e) = started {
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(crate::client_git::clone_git_dir(&base, &name));
        crate::client_git::forget_git_dir(&base, &name);
        crate::client_workspace::forget_template_commit(&base, &name);
        return internal(anyhow!("starting {name} from the template: {e}"));
    }
    match workspace_dir(state, &name) {
        Ok(dir) => {
            let mut v = workspace_json(state, &name, &dir, true);
            v.from = Some(from);
            (StatusCode::CREATED, Json(v)).into_response()
        }
        Err(r) => *r,
    }
}

/// `<bot>`, or with a number after it, free under the base (a partial
/// clone's name counts as taken).
fn free_project_name(base: &Path, bot: &str) -> Option<String> {
    use crate::client_workspace as ws;
    let taken = |n: &str| {
        std::fs::symlink_metadata(base.join(n)).is_ok()
            || std::fs::symlink_metadata(base.join(format!(".{n}.partial"))).is_ok()
    };
    std::iter::once(bot.to_string())
        .chain((2..100).map(|n| format!("{bot}-{n}")))
        .find(|n| ws::valid_name(n) && !taken(n))
}

/// The branch a project made from a template starts on.
const TEMPLATE_BRANCH: &str = "main";

/// Drops the clone's history and remote and makes it a new repo on
/// [`TEMPLATE_BRANCH`] with everything in one commit, its repository again
/// held by darkmux outside the work tree. Returns that commit's SHA.
fn start_fresh(base: &Path, name: &str, message: &str) -> Result<String, String> {
    let dir = base.join(name);
    let git_dir = crate::client_git::clone_git_dir(base, name);
    std::fs::remove_dir_all(&git_dir).map_err(|e| format!("removing the template's history: {e}"))?;
    let _ = std::fs::remove_file(dir.join(".git"));
    let (gd, wd) = (git_dir.to_string_lossy(), dir.to_string_lossy());
    let initial = format!("--initial-branch={TEMPLATE_BRANCH}");
    let init = darkmux_types::host_git::HostGit::trusted(base)
        .command(&["init", "--quiet", &initial, "--separate-git-dir", &gd, &wd])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("running git: {e}"))?;
    if !init.status.success() {
        return Err(String::from_utf8_lossy(&init.stderr).trim().to_string());
    }
    let canon = |p: &Path| p.canonicalize().map_err(|e| format!("{}: {e}", p.display()));
    let dir = &WorkCopy {
        work_tree: canon(&dir)?,
        git_dir: canon(&git_dir)?,
    };
    first_commit(dir, message)
}

/// Commits everything in the fresh project as its first commit; returns
/// its SHA.
fn first_commit(dir: &WorkCopy, message: &str) -> Result<String, String> {
    // A Mac with no git identity yet can still commit here: this project
    // gets its own, and the owner's takes over once there is one.
    if git_line(dir, &["config", "user.email"]).unwrap_or_default().is_empty() {
        git_quiet(dir, &["config", "user.name", "darkmux"])?;
        git_quiet(dir, &["config", "user.email", "darkmux@localhost"])?;
    }
    git_quiet(dir, &["add", "-A"])?;
    // (#3460 review) The recipe a preview of this project trusts is read
    // from this commit, so it goes in even when an ignore rule (the
    // template's `.gitignore`, the user's global excludes) leaves it out.
    let recipe = crate::client_preview::RECIPE_PATH;
    if std::fs::symlink_metadata(dir.work_tree.join(recipe)).is_ok() {
        git_quiet(dir, &["add", "-f", "--", recipe])?;
    }
    git_quiet(dir, &["commit", "--quiet", "-m", message])?;
    git_line(dir, &["rev-parse", "HEAD"]).ok_or_else(|| "reading the first commit".to_string())
}

/// The repo's workspace already here, its remote-tracking refs fetched.
fn existing_clone(state: &ClientServerState, name: &str) -> Response {
    let dir = match workspace_dir(state, name) {
        Ok(d) => d,
        Err(r) => return *r,
    };
    let mut v = workspace_json(state, name, &dir, false);
    let held = work_copy_for(state, &dir).map(|wc| wc.git_dir);
    let no_repo = || crate::client_workspace::Coded::new(ClientCode::FetchFailed, "darkmux holds no repository for it");
    if let Err(e) = held.ok_or_else(no_repo).and_then(|g| (state.fetch_repo)(&g)) {
        v.fetch_error_code = Some(e.code());
        v.fetch_error = Some(e.message);
    }
    Json(v).into_response()
}

/// Clones beside the final name, then renames: a failed clone never leaves
/// a half workspace behind under a real name.
fn place_clone(
    state: &ClientServerState,
    url: &str,
    normalized: &str,
    git_ref: Option<&str>,
    base: &Path,
    name: &str,
) -> Result<(), Refused> {
    use crate::client_git as cg;
    let partial = base.join(format!(".{name}.partial"));
    let git_dir = cg::clone_git_dir(base, name);
    // A repository left by an earlier workspace of this name is not this one's.
    let _ = std::fs::remove_dir_all(&git_dir);
    if let Err(e) = (state.clone_repo)(url, git_ref, &partial, &git_dir) {
        let _ = std::fs::remove_dir_all(&partial);
        let _ = std::fs::remove_dir_all(&git_dir);
        return Err(refuse(
            StatusCode::BAD_GATEWAY, ClientCode::CloneFailed,
            format!("cloning {normalized}: {e}"),
        ));
    }
    if let Err(e) = std::fs::rename(&partial, base.join(name)) {
        let _ = std::fs::remove_dir_all(&partial);
        let _ = std::fs::remove_dir_all(&git_dir);
        return Err(Box::new(internal(anyhow!("placing workspace {name}: {e}"))));
    }
    // A clone (or a template's fresh project) is no checkout of the user's:
    // no source record, whatever an earlier workspace of this name had. Its
    // repository is the one darkmux cloned it into, outside the work tree.
    crate::client_workspace::forget_source(base, name);
    crate::client_workspace::forget_template_commit(base, name);
    cg::record_git_dir(base, name, &git_dir).map_err(|e| Box::new(internal(anyhow!(e))))
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
    let (source, common) = match worktree_source(source, &base) {
        Ok(found) => found,
        Err(r) => return *r,
    };
    let _one_at_a_time = state.workspace_lock.lock().unwrap();
    if let Some(name) = ws::find_worktree_of(&base, &common, bot) {
        return reused_worktree(state, &base, &name, &source);
    }
    let Some(name) = free_worktree_name(&base, &source, bot) else {
        return err(
            StatusCode::CONFLICT, ClientCode::Conflict,
            "no free workspace name for this checkout",
        );
    };
    match add_bot_worktree(state, &source, git_ref, bot, &base.join(&name)) {
        Ok(added) => match record_worktree(&base, &name, &source, &common) {
            Ok(()) => created_worktree(state, &name, added.0, added.1),
            Err(e) => {
                remove_worktree(&source, &base.join(&name), &added.2);
                internal(anyhow!(e))
            }
        },
        Err(r) => *r,
    }
}

/// Takes back a worktree darkmux added but could not record: the worktree
/// (from the user's checkout, with its admin dir) and its `mux/` branch, so
/// a retry never leaves an orphan behind.
fn remove_worktree(source: &Path, dest: &Path, branch: &str) {
    let in_checkout = |args: &[&str]| {
        let _ = darkmux_types::host_git::HostGit::trusted(source)
            .command(args)
            .stdin(std::process::Stdio::null())
            .output();
    };
    in_checkout(&["worktree", "remove", "--force", &dest.to_string_lossy()]);
    let _ = std::fs::remove_dir_all(dest);
    in_checkout(&["worktree", "prune"]);
    in_checkout(&["branch", "-D", branch]);
}

/// Records a new worktree workspace: the user's checkout it was made from,
/// and its repository (the worktree's admin directory under that checkout's
/// `.git/worktrees/`), read once now, before any run has been in it.
fn record_worktree(base: &Path, name: &str, source: &Path, common: &Path) -> Result<(), String> {
    let admin = crate::client_git::worktree_admin_dir(&base.join(name), common)?;
    crate::client_git::record_git_dir(base, name, &admin)?;
    crate::client_workspace::record_source(base, name, source)
}

/// This bot's existing worktree of the checkout, its checkout recorded again
/// (the one previews read their recipe from).
fn reused_worktree(state: &ClientServerState, base: &Path, name: &str, source: &Path) -> Response {
    if let Err(e) = crate::client_workspace::record_source(base, name, source) {
        return internal(anyhow!(e));
    }
    match workspace_dir(state, name) {
        Ok(dir) => Json(workspace_json(state, name, &dir, false)).into_response(),
        Err(r) => *r,
    }
}

/// Adds the bot's worktree on a new `mux/<bot>-<hex>` branch. It starts from
/// the remote's default branch as it is now, not from wherever the owner's
/// checkout happens to be: the fetch moves remote-tracking refs only; the
/// owner's branch, index and files are never touched. Returns where it
/// started, the fetch's error if any, and the new branch.
fn add_bot_worktree(
    state: &ClientServerState,
    source: &Path,
    git_ref: Option<&str>,
    bot: &str,
    dest: &Path,
) -> Result<(String, Option<crate::client_workspace::Coded>, String), Refused> {
    use crate::client_workspace as ws;
    let short = random_hex(3).map_err(|e| Box::new(internal(e)))?;
    let branch = format!("mux/{bot}-{short}");
    let fetch_error = (state.fetch_repo)(source).err();
    let start = git_ref
        .map(str::to_string)
        .or_else(|| ws::remote_default(source))
        .unwrap_or_else(|| "HEAD".to_string());
    ws::add_worktree(source, &branch, dest, &start).map_err(|e| {
        refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            e.code(),
            format!("adding a worktree of {}: {e}", source.display()),
        )
    })?;
    Ok((start, fetch_error, branch))
}

fn created_worktree(
    state: &ClientServerState,
    name: &str,
    start: String,
    fetch_error: Option<crate::client_workspace::Coded>,
) -> Response {
    match workspace_dir(state, name) {
        Ok(dir) => {
            let mut v = workspace_json(state, name, &dir, true);
            v.from = Some(start);
            if let Some(e) = fetch_error {
                v.fetch_error_code = Some(e.code());
                v.fetch_error = Some(e.message);
            }
            (StatusCode::CREATED, Json(v)).into_response()
        }
        Err(r) => *r,
    }
}

/// The checkout to make a worktree of (under containment) and its git
/// common dir; a 403 for one that isn't allowed or isn't a git repository.
fn worktree_source(source: &Path, base: &Path) -> Result<(PathBuf, PathBuf), Refused> {
    use crate::client_workspace as ws;
    let source = ws::check_source(source, base).map_err(forbidden)?;
    let Some(common) = ws::common_dir(&source) else {
        return Err(forbidden(format!(
            "{} is not a git repository",
            source.display()
        )));
    };
    Ok((source, common))
}

/// `<repo>-<bot>`, or with a number after it, free under the base.
fn free_worktree_name(base: &Path, source: &Path, bot: &str) -> Option<String> {
    use crate::client_workspace as ws;
    let repo = ws::slug(
        &source
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
    );
    let stem = format!("{}-{bot}", if repo.is_empty() { "repo" } else { &repo });
    std::iter::once(stem.clone())
        .chain((2..100).map(|n| format!("{stem}-{n}")))
        .find(|n| ws::valid_name(n) && std::fs::symlink_metadata(base.join(n)).is_err())
}

/// The status and code a recipe refusal answers with. A recipe the trusted
/// `source` lacks is `template_no_recipe` when that is a template project's
/// first commit (#3460), `no_recipe` when it is the user's checkout.
fn recipe_refusal(source: &crate::client_preview::RecipeSource, r: &crate::client_preview::RecipeRefusal) -> Response {
    use crate::client_preview::{RecipeRefusal as R, RecipeSource};
    let missing = match source {
        RecipeSource::TemplateCommit { .. } => ClientCode::TemplateNoRecipe,
        RecipeSource::Checkout(_) => ClientCode::NoRecipe,
    };
    let (status, code) = match r {
        R::NotFound(_) => (StatusCode::NOT_FOUND, missing),
        R::Symlink(_) => (StatusCode::FORBIDDEN, ClientCode::RecipeInvalid),
        R::TooLarge(_) => (StatusCode::PAYLOAD_TOO_LARGE, ClientCode::RecipeInvalid),
        R::Invalid(_) => (StatusCode::UNPROCESSABLE_ENTITY, ClientCode::RecipeInvalid),
        R::Unreadable(_) => (StatusCode::INTERNAL_SERVER_ERROR, ClientCode::Internal),
    };
    err(status, code, r.message())
}

/// Workspace `name`'s directory and where its recipe is trusted from: the
/// first commit of a project darkmux made from a template (#3460), else the
/// user's own checkout it was made from; a 404 naming why there is neither.
fn workspace_and_recipe_source(
    state: &ClientServerState,
    name: &str,
) -> Result<(PathBuf, crate::client_preview::RecipeSource), Refused> {
    use crate::client_preview::RecipeSource;
    let dir = workspace_dir(state, name)?;
    let base = (state.worktrees_base)().map_err(|e| Box::new(internal(anyhow!(e))))?;
    if let Some(commit) = crate::client_workspace::template_commit_of(&base, name) {
        let held = crate::client_git::work_copy_of(&base, name, &dir).ok_or_else(|| {
            refuse(
                StatusCode::NOT_FOUND,
                ClientCode::NotFound,
                format!(
                    "darkmux holds no repository for template project {name}, so it has no preview"
                ),
            )
        })?;
        let git_dir = held.git_dir;
        return Ok((dir, RecipeSource::TemplateCommit { git_dir, commit }));
    }
    let checkout = crate::client_workspace::own_checkout_of(&base, name, &dir)
        .map_err(|e| refuse(StatusCode::NOT_FOUND, e.code(), e.to_string()))?;
    Ok((dir, RecipeSource::Checkout(checkout)))
}

/// A recipe the trusted source lacks, said with what to do about it.
fn no_recipe_in(
    source: &crate::client_preview::RecipeSource,
    r: crate::client_preview::RecipeRefusal,
) -> crate::client_preview::RecipeRefusal {
    use crate::client_preview::{RecipeRefusal as R, RecipeSource};
    match (r, source) {
        (R::NotFound(_), RecipeSource::Checkout(checkout)) => {
            let repo = crate::client_workspace::checkout_origin(checkout).unwrap_or_else(|| "this repo".to_string());
            R::NotFound(format!(
                "no .darkmux/preview.json in your checkout {}: add .darkmux/preview.json to your own \
                 checkout of {repo} (a copy in the workspace is never read)",
                checkout.display()
            ))
        }
        (R::NotFound(_), RecipeSource::TemplateCommit { commit, .. }) => R::NotFound(format!(
            "the template this project was made from had no .darkmux/preview.json in its first commit \
             {commit}, so it has no preview (a recipe added in the workspace is never read)"
        )),
        (other, _) => other,
    }
}

/// `POST /client/workspaces/{name}/preview`: runs the workspace's dev
/// server, from the `.darkmux/preview.json` in the user's own checkout the
/// workspace was made from, on free ports. 202 while it starts, 200 when it
/// was already running; poll GET for `ready` and `url`. 404 when there is no
/// recipe to run (a cloned workspace, or none in the checkout).
pub(crate) async fn preview_start_handler(
    State(state): State<ClientServerState>,
    ClientPath(name): ClientPath<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if let Err(r) = authenticate(&state, &headers) {
        return *r;
    }
    let ask = match preview_start_body(body).await {
        Ok(ask) => ask,
        Err(r) => return r,
    };
    let st = state.clone();
    tokio::task::spawn_blocking(move || {
        let reply = start_preview(&st, &name, ask.clean);
        let outcome = match reply.status() {
            StatusCode::OK => "already running (200)".to_string(),
            StatusCode::ACCEPTED => "starting (202)".to_string(),
            code => format!("refused ({})", code.as_u16()),
        };
        eprintln!("{}", preview_log_line(&name, "start", &outcome));
        reply
    })
    .await
    .unwrap_or_else(|e| internal(anyhow!("preview start panicked: {e}")))
}

/// Why previews are refused on this machine right now, if they are: off
/// unless `clients.preview.enabled` is true (a 403 that says what a preview
/// does and how to turn it on), and refused when it cannot be read (503).
fn previews_allowed(state: &ClientServerState) -> Result<(), Refused> {
    match (state.preview_enabled)() {
        Ok(true) => Ok(()),
        Ok(false) => Err(refuse(
            StatusCode::FORBIDDEN, ClientCode::PreviewDisabled,
            "previews are off on this machine: a preview runs the workspace's own code (its \
             package.json scripts and whatever they call, which a model may have written) in a \
             container with network access, the workspace mounted read-write and its ports \
             published on 127.0.0.1. To allow it: \
             `darkmux config set clients.preview.enabled true`",
        )),
        Err(e) => Err(refuse(StatusCode::SERVICE_UNAVAILABLE, ClientCode::Unavailable, e)),
    }
}

/// (#3484) `POST /client/workspaces/{name}/preview`'s optional body.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct PreviewStartBody {
    /// Stop the preview, delete its install record and the recipe's
    /// `install.unless_exists` path in the workspace, then start it: a fresh
    /// install.
    #[serde(default)]
    clean: bool,
}

/// The start body: none (or empty) is a plain start; anything else is
/// parsed strictly, an unknown key refused with `bad_request`.
async fn preview_start_body(body: Body) -> Result<PreviewStartBody, Response> {
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES).await.map_err(|_| {
        err(
            StatusCode::PAYLOAD_TOO_LARGE, ClientCode::PayloadTooLarge,
            format!("body larger than {MAX_BODY_BYTES} bytes"),
        )
    })?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(PreviewStartBody::default());
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| err(StatusCode::BAD_REQUEST, ClientCode::BadRequest, format!("bad JSON body: {e}")))
}

/// Starts workspace `name`'s preview from its trusted recipe; with `clean`,
/// first stops it and clears its install ([`crate::client_preview::Previews::clean`]).
fn start_preview(state: &ClientServerState, name: &str, clean: bool) -> Response {
    if let Err(r) = previews_allowed(state) {
        return *r;
    }
    let (dir, source) = match workspace_and_recipe_source(state, name) {
        Ok(found) => found,
        Err(r) => return *r,
    };
    if clean {
        if let Err(e) = state.previews.clean(name, &dir, &source) {
            return recipe_refusal(&source, &no_recipe_in(&source, e));
        }
    }
    match state.previews.start(name, &dir, &source) {
        Ok((v, true)) => Json(v).into_response(),
        Ok((v, false)) => (StatusCode::ACCEPTED, Json(v)).into_response(),
        Err(e) => recipe_refusal(&source, &no_recipe_in(&source, e)),
    }
}

pub(crate) async fn preview_status_handler(
    State(state): State<ClientServerState>,
    ClientPath(name): ClientPath<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authenticate(&state, &headers).and_then(|_| previews_allowed(&state)) {
        return *r;
    }
    let status = state.previews.status(&name);
    let now = status
        .as_ref()
        .map(|v| v.status.clone())
        .unwrap_or_else(|| "none running (404)".to_string());
    // The app polls every second: log a status only when it changes.
    if PREVIEW_LOG_SEEN.changed(&name, &now) {
        eprintln!("{}", preview_log_line(&name, "status", &now));
    }
    match status {
        Some(v) => Json(v).into_response(),
        None => err(StatusCode::NOT_FOUND, ClientCode::NotFound, format!("no preview for {name}")),
    }
}

/// One stderr line per preview request worth seeing in the serve log.
fn preview_log_line(workspace: &str, request: &str, outcome: &str) -> String {
    format!("darkmux client serve: preview {workspace}: {request} → {outcome}")
}

/// The last preview status logged per workspace.
#[derive(Default)]
struct PreviewLogSeen(Mutex<HashMap<String, String>>);

impl PreviewLogSeen {
    /// True when `status` differs from the last one seen for `workspace`.
    fn changed(&self, workspace: &str, status: &str) -> bool {
        let mut seen = self.0.lock().unwrap();
        if seen.get(workspace).map(String::as_str) == Some(status) {
            return false;
        }
        seen.insert(workspace.to_string(), status.to_string());
        true
    }
}

static PREVIEW_LOG_SEEN: std::sync::LazyLock<PreviewLogSeen> =
    std::sync::LazyLock::new(PreviewLogSeen::default);

/// How often `client serve` looks for previews no app has asked about.
pub(crate) const PREVIEW_IDLE_CHECK_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(60);

/// (#3459) One idle check: reads `clients.preview.idle_minutes` now (so a
/// `darkmux config set` applies at the next check) and stops every preview
/// no app has asked about for that long. Returns the serve-log lines.
/// `skipped` carries the last "could not read the setting" reason between
/// checks, so an unreadable config is said once, not every minute; while
/// it is unreadable nothing is stopped on a guessed value.
pub fn preview_idle_tick(state: &ClientServerState, skipped: &mut Option<String>) -> Vec<String> {
    let minutes = match (state.preview_idle_minutes)() {
        Ok(m) => m,
        Err(e) => {
            if skipped.as_deref() == Some(e.as_str()) {
                return Vec::new();
            }
            let line = format!(
                "darkmux client serve: preview idle check skipped, stopping nothing: clients.preview.idle_minutes \
                 could not be read ({e})"
            );
            *skipped = Some(e);
            return vec![line];
        }
    };
    *skipped = None;
    state
        .previews
        .stop_idle(minutes)
        .into_iter()
        .map(|name| {
            preview_log_line(
                &name,
                "idle",
                &format!("stopped: no app asked about it for {minutes} minutes (clients.preview.idle_minutes)"),
            )
        })
        .collect()
}

/// Starts the idle check's thread: every `interval` until `stop` is set.
pub(crate) fn spawn_preview_idle_check(
    state: ClientServerState,
    interval: std::time::Duration,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    use std::sync::atomic::Ordering;
    std::thread::spawn(move || {
        let mut skipped = None;
        let poll = interval.min(std::time::Duration::from_millis(200));
        loop {
            let mut slept = std::time::Duration::ZERO;
            while slept < interval {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(poll);
                slept += poll;
            }
            for line in preview_idle_tick(&state, &mut skipped) {
                eprintln!("{line}");
            }
        }
    })
}

pub(crate) async fn preview_stop_handler(
    State(state): State<ClientServerState>,
    ClientPath(name): ClientPath<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authenticate(&state, &headers) {
        return *r;
    }
    let st = state.clone();
    let stopped = tokio::task::spawn_blocking(move || st.previews.stop(&name))
        .await
        .unwrap_or(false);
    if stopped {
        Json(StatusWord {
            status: "stopped".to_string(),
        })
        .into_response()
    } else {
        not_found()
    }
}

/// The most file content `/changes` returns in one reply.
pub const MAX_CHANGES_BYTES: usize = 25 * 1024 * 1024;

/// `GET /client/workspaces/{name}/changes` (read-only): what the work
/// copy's branch holds that origin's default branch doesn't, for an app to
/// open a PR with its own GitHub sign-in (darkmux doesn't author PRs). Only
/// committed changes: {base, base_branch, head, branch, files: [{path,
/// content (base64), mode} | {path, deleted: true}]}.
pub(crate) async fn workspace_changes_handler(
    State(state): State<ClientServerState>,
    ClientPath(name): ClientPath<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authenticate(&state, &headers) {
        return *r;
    }
    let st = state.clone();
    tokio::task::spawn_blocking(move || {
        use crate::client_workspace as ws;
        let dir = match workspace_dir(&st, &name) {
            Ok(d) => d,
            Err(r) => return *r,
        };
        let Some(dir) = work_copy_for(&st, &dir) else {
            return err(StatusCode::CONFLICT, ClientCode::Conflict, "darkmux holds no repository for this workspace");
        };
        let (default, base_branch) = match changes_base(&st, &dir, &name) {
            Ok(b) => b,
            Err(r) => return *r,
        };
        let branch = git_line(&dir, &["branch", "--show-current"]).unwrap_or_default();
        if branch.is_empty() {
            return err(StatusCode::CONFLICT, ClientCode::Conflict, "the work copy isn't on a branch");
        }
        let (Some(base), Some(head)) = (git_line(&dir, &["merge-base", "HEAD", &default]), git_line(&dir, &["rev-parse", "HEAD"])) else {
            return err(StatusCode::CONFLICT, ClientCode::Conflict, "couldn't find where this branch left the default branch");
        };
        let Some(listing) = git(&dir, None, &["diff", "--no-renames", "--name-status", "-z", &base, &head]) else {
            return internal(anyhow!("listing the branch's changes failed"));
        };
        let fields: Vec<String> = String::from_utf8_lossy(&listing.stdout)
            .split('\0')
            .filter(|f| !f.is_empty())
            .map(str::to_string)
            .collect();
        let mut files = Vec::new();
        let mut total = 0usize;
        for pair in fields.chunks(2) {
            let [status, path] = pair else { continue };
            if status.starts_with('D') {
                files.push(BranchFile {
                    path: path.clone(),
                    deleted: Some(true),
                    mode: None,
                    content: None,
                });
                continue;
            }
            let Some(blob) = git(&dir, None, &["show", &format!("{head}:{path}")]) else {
                return internal(anyhow!("reading {path} failed"));
            };
            total += blob.stdout.len();
            if total > MAX_CHANGES_BYTES {
                return err(StatusCode::PAYLOAD_TOO_LARGE, ClientCode::PayloadTooLarge, "the branch's changes are too large to send");
            }
            let mode = git_line(&dir, &["--literal-pathspecs", "ls-tree", &head, "--", path])
                .and_then(|l| l.split_whitespace().next().map(str::to_string))
                .unwrap_or_else(|| "100644".to_string());
            files.push(BranchFile {
                path: path.clone(),
                deleted: None,
                mode: Some(mode),
                content: Some(ws::base64_encode(&blob.stdout)),
            });
        }
        Json(BranchChanges {
            base,
            base_branch,
            head,
            branch,
            files,
        })
        .into_response()
    })
    .await
    .unwrap_or_else(|e| internal(anyhow!("changes panicked: {e}")))
}

/// What `/changes` compares the work copy's branch with, and that base's
/// branch name: origin's default branch; or, for a project darkmux made
/// from a template, which has no origin (#3480), darkmux's own recorded
/// first commit on the branch it started. Any other work copy with no
/// origin is a 409.
fn changes_base(state: &ClientServerState, dir: &WorkCopy, name: &str) -> Result<(String, String), Refused> {
    if let Some(d) = crate::client_workspace::remote_default_of(dir) {
        let branch = d.trim_start_matches("origin/").to_string();
        return Ok((d, branch));
    }
    let base = (state.worktrees_base)().map_err(|e| Box::new(internal(anyhow!(e))))?;
    crate::client_workspace::template_commit_of(&base, name)
        .map(|first| (first, TEMPLATE_BRANCH.to_string()))
        .ok_or_else(|| refuse(StatusCode::CONFLICT, ClientCode::Conflict, "this work copy has no origin default branch to compare with"))
}

/// `GET /client/workspaces/{name}`: branch, head and repo of one workspace.
pub(crate) async fn workspace_status_handler(
    State(state): State<ClientServerState>,
    ClientPath(name): ClientPath<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authenticate(&state, &headers) {
        return *r;
    }
    let st = state.clone();
    tokio::task::spawn_blocking(move || match workspace_dir(&st, &name) {
        Ok(dir) => Json(workspace_json(&st, &name, &dir, false)).into_response(),
        Err(r) => *r,
    })
    .await
    .unwrap_or_else(|e| internal(anyhow!("workspace status panicked: {e}")))
}

pub(crate) async fn work_status_handler(
    State(state): State<ClientServerState>,
    ClientPath(work_id): ClientPath<String>,
    headers: HeaderMap,
) -> Response {
    let client = match authenticate(&state, &headers) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let entry = state.works.lock().unwrap().get(&work_id).cloned();
    match entry {
        Some(w) if w.client_name == client.name => {
            let st = state.clone();
            tokio::task::spawn_blocking(move || Json(work_status_json(&st, &work_id, w)).into_response())
                .await
                .unwrap_or_else(|e| internal(anyhow!("work status panicked: {e}")))
        }
        _ => not_found(),
    }
}

/// (#3467) The flow records a running work item's activity is read from:
/// its dispatch's bookends and `dispatch.model.*` records, kept per day file
/// and read again only past what was read (`flow_window`), so a poll costs
/// what was appended since the last one.
static WORK_ACTIVITY_RECORDS: crate::flow_window::FlowRecordStore =
    crate::flow_window::FlowRecordStore::new(feeds_work_activity);

/// Whether a record can change a client work item's activity: a dispatch
/// bookend or `dispatch.model.*` record of a crew-of-one dispatch session
/// (`darkmux dispatch <role> --name <work_id>` names it `.adhoc.<role>.<work_id>`).
pub(crate) fn feeds_work_activity(v: &Value) -> bool {
    use darkmux_flow::FlowAction as A;
    let adhoc = v.get("session_id").and_then(Value::as_str).is_some_and(|s| s.contains(".adhoc."));
    adhoc
        && matches!(
            darkmux_flow::reader::action_of(v),
            Some(A::DispatchStart | A::DispatchModelStart | A::DispatchModelLoading | A::DispatchModelReady | A::DispatchComplete | A::DispatchError)
        )
}

/// (#3467) What `role`'s running work `work_id` is doing besides its turns:
/// its dispatch session's records (today's and yesterday's day files),
/// folded and settled by the run lifecycle, the reading `GET /runs` takes.
pub(crate) fn work_activity_in(
    store: &crate::flow_window::FlowRecordStore,
    flows_dir: &Path,
    role: &str,
    work_id: &str,
) -> (Option<crate::runs::RunActivity>, Option<u64>) {
    let yesterday = crate::runs::day_string_from_epoch_ms(unix_now().saturating_sub(86_400) * 1_000);
    let records = store.read(flows_dir, &yesterday);
    crate::runs::running_session_activity(|fold| {
        records.for_each(|v| {
            if is_work_session(v, role, work_id) {
                fold(v);
            }
        })
    })
}

/// Whether record `v` is of `role`'s work `work_id`: its session is the
/// crew-of-one dispatch session `darkmux dispatch <role> --name <work_id>`
/// made. Read through the session id's own grammar (`SessionId::parse`),
/// which unescapes the role and nonce, so a role id with a byte outside
/// `[A-Za-z0-9-]` (escaped on the wire) is found too.
pub(crate) fn is_work_session(v: &Value, role: &str, work_id: &str) -> bool {
    use darkmux_types::session_id::{SessionId, SessionKind};
    let Some(wire) = v.get("session_id").and_then(Value::as_str) else { return false };
    SessionId::parse(wire).is_ok_and(|s| matches!(s.kind(), SessionKind::Adhoc { role: r, nonce } if r == role && nonce == work_id))
}

/// (#3468) The `code` the `dispatch.error` of `role`'s work `work_id` names
/// ([`is_work_session`]), from today's and
/// yesterday's day files; `None` when it names none or wrote no terminal.
pub(crate) fn dispatch_error_code_in(
    store: &crate::flow_window::FlowRecordStore,
    flows_dir: &Path,
    role: &str,
    work_id: &str,
) -> Option<String> {
    let yesterday = crate::runs::day_string_from_epoch_ms(unix_now().saturating_sub(86_400) * 1_000);
    let mut code = None;
    store.read(flows_dir, &yesterday).for_each(|v| {
        let ours = is_work_session(v, role, work_id);
        if ours && darkmux_flow::reader::action_of(v) == Some(darkmux_flow::FlowAction::DispatchError) {
            code = v.pointer("/payload/code").and_then(Value::as_str).map(str::to_string);
        }
    });
    code
}

/// A work item as its client polls it.
fn work_status_json(state: &ClientServerState, work_id: &str, w: WorkEntry) -> WorkState {
    match w.status {
        WorkStatus::Running if w.cancel.requested() => WorkState::Cancelling,
        WorkStatus::Running => {
            let (activity, activity_elapsed_ms) = (state.work_activity)(&w.role, work_id);
            WorkState::Running { activity, activity_elapsed_ms }
        }
        WorkStatus::Done {
            result,
            changes,
            changes_error,
        } => WorkState::Done {
            changes_error,
            result,
            changes,
            commit: w.commit.as_deref().map(short),
            branch: w.branch,
            commit_error: w.commit_error,
            undone: w.decided.is_some(),
        },
        WorkStatus::Error(e) => WorkState::Error {
            code: ClientCode::of_dispatch(e.code.as_deref()),
            error: e.error,
            reason: e.reason,
        },
        WorkStatus::Cancelled(e) => WorkState::Cancelled {
            reason: "cancelled".to_string(),
            error: e.map(|e| e.error),
        },
    }
}

/// `POST /client/work/{id}/cancel`: only the client that started the run may
/// stop it (anyone else's id reads as unknown). The stop itself is the
/// executor's: SIGTERM, then SIGKILL after [`CANCEL_GRACE`].
pub(crate) async fn cancel_handler(
    State(state): State<ClientServerState>,
    ClientPath(work_id): ClientPath<String>,
    headers: HeaderMap,
) -> Response {
    let client = match authenticate(&state, &headers) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let works = state.works.lock().unwrap();
    match works.get(&work_id) {
        Some(w) if w.client_name == client.name => match w.status {
            WorkStatus::Running => {
                w.cancel.request();
                (
                    StatusCode::ACCEPTED,
                    Json(StatusWord {
                        status: "cancelling".to_string(),
                    }),
                )
                    .into_response()
            }
            _ => err(StatusCode::CONFLICT, ClientCode::Conflict, "not running"),
        },
        _ => not_found(),
    }
}

// ─── Commits and Undo ────────────────────────────────────────────────────

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

fn git_line(wc: &WorkCopy, args: &[&str]) -> Option<String> {
    crate::client_git::line(wc, args)
}

/// Runs host git in the work copy ([`crate::client_git`]: the repository
/// darkmux holds, never one the run could write; hooks and every
/// program-running setting off); the last stderr line on failure.
fn git_quiet(wc: &WorkCopy, args: &[&str]) -> Result<(), String> {
    crate::client_git::quiet(wc, args)
}

/// Commits exactly `paths` to the work copy's branch. Anything else in the
/// work copy, staged or not, stays out. The paths are a run's file names,
/// passed to git literally (`--literal-pathspecs`): a file named `[ab].txt`
/// is that file, never a glob that also takes the owner's `b.txt`. Returns
/// (full id, branch).
pub(crate) fn commit_run(
    wd: &WorkCopy,
    paths: &[String],
    message: &str,
) -> Result<(String, String), String> {
    let branch = git_line(wd, &["branch", "--show-current"]).unwrap_or_default();
    if branch.is_empty() {
        return Err("the work copy isn't on a branch, so the run wasn't committed".to_string());
    }
    let mut add = vec!["--literal-pathspecs", "add", "-A", "--"];
    add.extend(paths.iter().map(String::as_str));
    git_quiet(wd, &add)?;
    let mut commit = vec!["--literal-pathspecs", "commit", "--quiet", "-m", message, "--"];
    commit.extend(paths.iter().map(String::as_str));
    git_quiet(wd, &commit)?;
    let sha = git_line(wd, &["rev-parse", "HEAD"]).ok_or("reading the new commit failed")?;
    Ok((sha, branch))
}

/// This client's finished run with a commit, for Undo (and old clients'
/// Keep): 404 for anyone else's or an unknown id, 409 otherwise.
fn committed_run(
    state: &ClientServerState,
    headers: &HeaderMap,
    work_id: &str,
) -> Result<WorkEntry, Refused> {
    let client = authenticate(state, headers)?;
    let entry = state.works.lock().unwrap().get(work_id).cloned();
    let Some(w) = entry.filter(|w| w.client_name == client.name) else {
        return Err(Box::new(not_found()));
    };
    if !matches!(w.status, WorkStatus::Done { .. }) {
        return Err(refuse(StatusCode::CONFLICT, ClientCode::Conflict, "the run hasn't finished"));
    }
    if w.commit.is_none() {
        let why = w
            .commit_error
            .clone()
            .unwrap_or_else(|| "the run changed nothing".to_string());
        return Err(refuse(
            StatusCode::CONFLICT, ClientCode::Conflict,
            format!("{why}; there's nothing to undo"),
        ));
    }
    if w.decided.is_some() {
        return Err(refuse(StatusCode::CONFLICT, ClientCode::Conflict, "already undone"));
    }
    Ok(w)
}

/// `POST /client/work/{id}/undo`: reverts the run's commit (a new commit;
/// history is never rewritten). Refused while a later commit changed the
/// same files, naming the run that made it. 200 {revert, files}.
pub(crate) async fn undo_handler(
    State(state): State<ClientServerState>,
    ClientPath(work_id): ClientPath<String>,
    headers: HeaderMap,
) -> Response {
    let st = state.clone();
    tokio::task::spawn_blocking(move || {
        let w = match committed_run(&st, &headers, &work_id) {
            Ok(w) => w,
            Err(r) => return *r,
        };
        let (Some(wd), Some(commit)) = (w.git.clone(), w.commit.clone()) else {
            return err(StatusCode::CONFLICT, ClientCode::Conflict, "the run has no git work copy");
        };
        let mut log = vec![
            "--literal-pathspecs".to_string(),
            "log".to_string(),
            "--format=%H".to_string(),
            format!("{commit}..HEAD"),
            "--".to_string(),
        ];
        log.extend(w.files.iter().cloned());
        let later = git(
            &wd,
            None,
            &log.iter().map(String::as_str).collect::<Vec<_>>(),
        )
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
        // A later run that was itself undone (its commit and the revert of
        // it) leaves the files as they were: it doesn't count.
        let settled: std::collections::HashSet<String> = st
            .works
            .lock()
            .unwrap()
            .values()
            .filter_map(|e| {
                Some((
                    e.commit.clone()?,
                    e.decided.as_ref()?.strip_prefix("undone by ")?.to_string(),
                ))
            })
            .flat_map(|(c, r)| [c, r])
            .collect();
        let later: Vec<String> = later
            .into_iter()
            .filter(|sha| !settled.contains(sha))
            .collect();
        if let Some(sha) = later.last() {
            let by = st
                .works
                .lock()
                .unwrap()
                .iter()
                .find(|(_, e)| e.commit.as_deref() == Some(sha.as_str()))
                .map(|(id, _)| format!("run {id}"))
                .unwrap_or_else(|| format!("commit {}", short(sha)));
            return err(
                StatusCode::CONFLICT, ClientCode::Conflict,
                format!("{by} changed the same files later; undo that first"),
            );
        }
        if let Err(e) = git_quiet(&wd, &["revert", "--no-edit", &commit]) {
            let _ = git_quiet(&wd, &["revert", "--abort"]);
            return err(StatusCode::CONFLICT, ClientCode::Conflict, format!("couldn't undo it: {e}"));
        }
        let revert = git_line(&wd, &["rev-parse", "HEAD"]).unwrap_or_default();
        if let Some(e) = st.works.lock().unwrap().get_mut(&work_id) {
            e.decided = Some(format!("undone by {revert}"));
        }
        Json(UndoReply {
            revert: short(&revert),
            files: w.files,
        })
        .into_response()
    })
    .await
    .unwrap_or_else(|e| internal(anyhow!("undo panicked: {e}")))
}

/// Old clients' Keep: the run is already committed; this reports it.
pub(crate) async fn keep_handler(
    State(state): State<ClientServerState>,
    ClientPath(work_id): ClientPath<String>,
    headers: HeaderMap,
    _body: Body,
) -> Response {
    match committed_run(&state, &headers, &work_id) {
        Ok(w) => Json(KeepReply {
            commit: w.commit.as_deref().map(short),
            branch: w.branch,
            files: w.files,
        })
        .into_response(),
        Err(r) => *r,
    }
}

/// Old clients' Discard: an Undo.
pub(crate) async fn discard_handler(
    state: State<ClientServerState>,
    work_id: ClientPath<String>,
    headers: HeaderMap,
) -> Response {
    let reply = undo_handler(state, work_id, headers).await;
    if reply.status() != StatusCode::OK {
        return reply;
    }
    let bytes = axum::body::to_bytes(reply.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let restored = v["files"]
        .as_array()
        .map(|f| f.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    Json(DiscardReply {
        restored,
        removed: Vec::new(),
    })
    .into_response()
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
    let failed = |reason: &str, error: String| RunError {
        reason: reason.to_string(),
        error,
        code: None,
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
    let (status, killed) = wait_cancellable(&mut child, cancel, grace).map_err(|e| {
        failed(
            "wait failed",
            format!("waiting for the dispatch child: {e}"),
        )
    })?;
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
    let tail = stderr_tail(&stderr_bytes);
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    let mut e = failed(&exit_reason(status.code(), signal), tail);
    e.code = teardown.dispatch_error_code(spec);
    Err(e)
}

/// The most of a failed child's stderr a status keeps.
const STDERR_TAIL_CHARS: usize = 4000;

/// A short machine reason for a child that ended without a `--json`
/// envelope: `exit 143/SIGTERM` (dispatch's own exit on a caught signal is
/// 128 + its number), `exit <n>`, `signal 9/SIGKILL` (killed outright), or
/// `no output` (a clean exit that printed no envelope).
/// Waits for the child; once cancel is raised, SIGTERM, then SIGKILL after
/// `grace`. Returns its status and whether it had to be killed.
fn wait_cancellable(
    child: &mut std::process::Child,
    cancel: &CancelFlag,
    grace: std::time::Duration,
) -> std::io::Result<(std::process::ExitStatus, bool)> {
    use darkmux_types::child_registry::{kill_pid, SIGKILL, SIGTERM};
    let pid = child.id();
    let mut termed_at: Option<std::time::Instant> = None;
    let mut killed = false;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((status, killed));
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
    }
}

/// The last `STDERR_TAIL_CHARS` characters of a child's stderr, trimmed.
fn stderr_tail(bytes: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(bytes);
    let chars: Vec<char> = stderr.trim().chars().collect();
    chars[chars.len().saturating_sub(STDERR_TAIL_CHARS)..].iter().collect()
}

pub(crate) fn exit_reason(code: Option<i32>, signal: Option<i32>) -> String {
    let named = |what: &str, n: i32, sig: i32| match signal_name(sig) {
        Some(name) => format!("{what} {n}/{name}"),
        None => format!("{what} {n}"),
    };
    match (code, signal) {
        (Some(0), _) => "no output".to_string(),
        (Some(c), _) if c > 128 => named("exit", c, c - 128),
        (Some(c), _) => format!("exit {c}"),
        (None, Some(sig)) => named("signal", sig, sig),
        (None, None) => "no exit status".to_string(),
    }
}

/// The signals a dispatch child is usually stopped by.
fn signal_name(n: i32) -> Option<&'static str> {
    match n {
        1 => Some("SIGHUP"),
        2 => Some("SIGINT"),
        9 => Some("SIGKILL"),
        15 => Some("SIGTERM"),
        _ => None,
    }
}

// ─── What a run changed ──────────────────────────────────────────────────

/// The most of a run's unified diff a status carries.
pub const MAX_DIFF_BYTES: usize = 32 * 1024;
/// Appended to a diff cut at [`MAX_DIFF_BYTES`].
pub const DIFF_TRUNCATED_MARKER: &str = "\n[diff truncated at 32 KiB]\n";

fn git(wd: &WorkCopy, index: Option<&Path>, args: &[&str]) -> Option<std::process::Output> {
    crate::client_git::run(wd, index, args)
}

/// The whole work copy as a git tree id: tracked AND untracked files (not
/// ignored ones), staged or not, written through a throwaway index so the
/// work copy's own index, branch and files are never touched (only blob and
/// tree objects are added to its object store). `None` when `wd` is not in
/// a git work copy.
pub(crate) fn snapshot_tree(wd: &WorkCopy) -> Result<Snapshot, String> {
    let scratch = tempfile::tempdir().map_err(|e| format!("a scratch index: {e}"))?;
    let index = scratch.path().join("index");
    let skipped = nested_repositories(wd)?;
    // A repository inside the work copy (an empty `git init sub` makes
    // `git add` fail outright; one with commits would be added as a link)
    // is left out, by name, literally.
    let excludes: Vec<String> = skipped.iter().map(|p| format!(":(exclude,literal){p}")).collect();
    let mut add = vec!["add", "-A", "--", "."];
    add.extend(excludes.iter().map(String::as_str));
    crate::client_git::run_or_why(wd, Some(&index), &add)?;
    let tree = crate::client_git::run_or_why(wd, Some(&index), &["write-tree"])?;
    Ok(Snapshot {
        tree: String::from_utf8_lossy(&tree.stdout).trim().to_string(),
        skipped,
    })
}

/// A work copy as one tree, and the repositories inside it the tree leaves
/// out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub tree: String,
    pub skipped: Vec<String>,
}

/// The untracked repositories inside the work copy: git lists each as one
/// `path/` entry and does not descend into it.
fn nested_repositories(wd: &WorkCopy) -> Result<Vec<String>, String> {
    let out = crate::client_git::run_or_why(wd, None, &["ls-files", "-z", "--others", "--exclude-standard"])?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter_map(|e| e.strip_suffix('/'))
        .filter(|p| std::fs::symlink_metadata(wd.work_tree.join(p).join(".git")).is_ok())
        .map(str::to_string)
        .collect())
}

/// What changed in `wd` since the tree `before` ([`snapshot_tree`]): the
/// files (lines added and removed, `null` for a binary file), the unified
/// diff capped at [`MAX_DIFF_BYTES`], and the nested repositories left out.
/// `Err` says why the work copy could not be read: never "no changes".
pub(crate) fn changes_since(wd: &WorkCopy, before: &str) -> Result<RunChanges, String> {
    let after = snapshot_tree(wd)?;
    let numstat = crate::client_git::run_or_why(
        wd,
        None,
        &["diff", "--no-renames", "--no-ext-diff", "--no-textconv", "--numstat", "-z", before, &after.tree],
    )?;
    let files: Vec<FileChange> = String::from_utf8_lossy(&numstat.stdout)
        .split('\0')
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            let mut parts = l.splitn(3, '\t');
            let (added, removed, path) = (parts.next()?, parts.next()?, parts.next()?);
            let n = |s: &str| s.parse::<u64>().ok();
            Some(FileChange {
                path: path.to_string(),
                added: n(added),
                removed: n(removed),
            })
        })
        .collect();
    let patch = crate::client_git::run_or_why(
        wd,
        None,
        &["diff", "--no-renames", "--no-ext-diff", "--no-textconv", "--no-color", before, &after.tree],
    )?;
    Ok(RunChanges {
        files,
        diff: capped_diff(&String::from_utf8_lossy(&patch.stdout)),
        skipped: after.skipped,
    })
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
    /// (#3468) The `code` the work's `dispatch.error` named, when the child
    /// failed: what an app branches on, read from the flow.
    fn dispatch_error_code(&self, spec: &WorkSpec) -> Option<String>;
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
    fn dispatch_error_code(&self, spec: &WorkSpec) -> Option<String> {
        dispatch_error_code_in(&WORK_ACTIVITY_RECORDS, &darkmux_flow::flows_dir(), &spec.role, &spec.work_id)
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
/// <work_id>` made ([`is_work_session`]).
pub(crate) fn find_start_in(dir: &Path, spec: &WorkSpec) -> Option<StartedRun> {
    records_in(dir).find_map(|r| {
        if darkmux_flow::reader::action_of(&r) != Some(darkmux_flow::FlowAction::DispatchStart) {
            return None;
        }
        let session = r.get("session_id")?.as_str()?;
        if !is_work_session(&r, &spec.role, &spec.work_id) {
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
