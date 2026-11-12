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
//! and ignored: what an app may run is machine policy, not the app's ask). A
//! Mux Bot on this machine whose code signature verifies (a verified team, or
//! an app the operator allowed before) is approved at once; any other app gets
//! a native Allow dialog ([`crate::client_trust`], #3544). Either way, the
//! operator can run `darkmux client approve --code <CODE>` here with the code
//! the app shows, see the device, code and age, and approve (or deny). A
//! pairing records identity only. The app polls `/client/pair/{id}` with
//! `X-Pair-Secret`; on the first poll after approval this listener mints the
//! token (32 random bytes, hex), stores ONLY its BLAKE3 hash as an approved
//! client, deletes the pending record and returns the token. A later poll is
//! a 404: the token is delivered exactly once and is never on disk. A wrong
//! secret is a 404 too, so a request id alone reveals nothing. A request
//! expires ten minutes after it was made, approved or not; until the app
//! collects it, `client revoke <name>` (or a deny) takes an approval back.
//! A container reaches this listener through Docker's one forwarding
//! process, with any `Host` it likes, so no one process on this machine may
//! have more than [`PAIR_REQUESTS_PER_PROCESS`] requests waiting: its next
//! replaces its oldest, and it can never fill the queue ([`make_room`]).
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
//! records) or a path inside a workspace. And it must be one darkmux made
//! for this client: every workspace records the id of the client it was
//! made for (`.<name>.client` beside it; an id, never the client's name,
//! which a later pairing can reuse), and every route that names a workspace
//! refuses another app's, a revoked client's, a mission's worktrees, or any
//! folder no app asked darkmux for ([`owned_workspace`]). One made before
//! workspaces had an owner goes to the client that was the only one paired
//! when the store moved to client ids, while it stays paired
//! ([`adopt_unowned`]); otherwise only `darkmux client claim <workspace>
//! <client>` gives it to one, and `darkmux doctor` lists it.
//! `GET /client/recipe?workspace=` returns a workspace's preview recipe, for
//! an app that cannot read the disk itself.
//!
//! **A work copy's history is the runs'.** Whatever a run changed in its
//! work copy becomes one commit of its own however it ended (a failed or
//! cancelled run's commit message says so), so the next run's commit never
//! carries it and Undo can revert it. A run that produced its result is
//! done, even when a cancel lands after it finished. Undo waits while
//! another run works in the same copy, and no run starts there while an
//! Undo reverts.
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
//! **What a client started ends with it.** A run is admitted and run by one
//! task the request cannot cancel, so an app that hangs up mid-request never
//! leaves a run registered with nothing running it. `client revoke` edits
//! the store; this listener sees the store change (and looks again every
//! minute) and cancels that client's running work and stops its previews
//! ([`unpaired_tick`]). As `client serve` stops it cancels every running
//! item and waits, within a bound, for each to end as a canceled run does;
//! it never waits on a run without one ([`end_running_work`]).
//!
//! **State.** `~/.darkmux/clients.json` (0600), every read-modify-write under
//! an exclusive `flock` on the file itself, so `client approve` in one process
//! and this listener in another never lose a write. A change replaces the
//! file in one rename, so it is never empty or half written, whatever
//! happens mid-write ([`replace_store`]). It is darkmux-written
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
use std::io::{Read, Write};
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
/// Live requests one process on this machine may have waiting at once; its
/// next replaces its oldest.
pub const PAIR_REQUESTS_PER_PROCESS: usize = 2;
/// The largest request body any route reads.
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// Work a single client may have running at once.
const MAX_RUNNING_PER_CLIENT: usize = 2;
/// Ended work is remembered this long after it ended, for polls.
const KEEP_WORK_SECS: u64 = 3600;
/// The `schema_version` this binary writes and reads.
///
/// 1.1: records hold identity only; the per-record `scopes` of 1.0 is
/// dropped when a 1.0 store is read (what a client may run is the machine's
/// `clients.profiles` setting).
///
/// 1.2: every client has a stable random `id`, minted at approval, which its
/// workspaces and runs are keyed by (never its display name, which a later
/// pairing can reuse). A 1.1 store's clients get one when it is read.
/// Additive in 1.2 (#3544, unreleased with it): `auto_approved` on a request
/// and a client (the signing identity that approved it with no operator
/// step) and `remembered_apps` (apps the operator allowed in the dialog,
/// which then pair at once).
pub const CLIENTS_SCHEMA_VERSION: &str = "1.2";

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
    /// The id the client will have, minted at approval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// (#3544) The signing identity that approved it with no operator step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_approved: Option<SigningIdentity>,
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
    /// Stable and random (`c-` and 16 hex digits), minted at approval: what
    /// its workspaces and runs belong to. A later client given the same
    /// name never inherits them.
    pub id: String,
    /// Unix seconds.
    pub created: u64,
    /// BLAKE3 of the bearer token, hex. The token itself is never stored.
    pub token_hash: String,
    /// (#3544) The signing identity that approved it with no operator step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_approved: Option<SigningIdentity>,
}

/// (#3544) An app as its code signature names it: the developer team and
/// the bundle id. Written `<team>/<bundle>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SigningIdentity {
    pub team: String,
    pub bundle: String,
}

impl SigningIdentity {
    /// `<team>/<bundle>`, as `client list` writes it.
    pub fn parse(s: &str) -> Option<Self> {
        let (team, bundle) = s.trim().split_once('/')?;
        (!team.is_empty() && !bundle.is_empty()).then(|| Self { team: team.to_string(), bundle: bundle.to_string() })
    }
}

impl From<&darkmux_platform::SignedApp> for SigningIdentity {
    fn from(app: &darkmux_platform::SignedApp) -> Self {
        Self { team: app.team.clone(), bundle: app.bundle.clone() }
    }
}

impl std::fmt::Display for SigningIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.team, self.bundle)
    }
}

/// (#3544) An app the operator allowed in the pairing dialog, remembered by
/// its signing identity: a later pairing from a process that verifies as it
/// is approved at once, until `client forget`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RememberedApp {
    pub team: String,
    pub bundle: String,
    /// Unix seconds.
    pub allowed: u64,
}

impl RememberedApp {
    pub fn identity(&self) -> SigningIdentity {
        SigningIdentity { team: self.team.clone(), bundle: self.bundle.clone() }
    }
}

/// `~/.darkmux/clients.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ClientsFile {
    pub schema_version: String,
    #[serde(default)]
    pub pending: Vec<PendingRequest>,
    #[serde(default)]
    pub clients: Vec<ApprovedClient>,
    /// The id of the one client paired when this store was migrated to
    /// 1.2, if exactly one was: the workspaces darkmux made before they had
    /// owners are that client's while it stays paired ([`adopt_unowned`]).
    /// Decided once, at the migration; never set otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_owner: Option<String>,
    /// (#3544) Apps the operator allowed in the pairing dialog.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remembered_apps: Vec<RememberedApp>,
}

impl Default for ClientsFile {
    fn default() -> Self {
        Self {
            schema_version: CLIENTS_SCHEMA_VERSION.to_string(),
            pending: Vec::new(),
            clients: Vec::new(),
            legacy_owner: None,
            remembered_apps: Vec::new(),
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

    /// The store as it is now, read under its shared lock (so never half
    /// written) and never written (an older store is migrated in memory
    /// only; an absent one reads as empty and is not created): for a reader
    /// that must change nothing, `darkmux doctor`.
    pub fn peek(&self) -> Result<ClientsFile> {
        let Some(mut guard) = self.lock_shared_current()? else {
            return Ok(ClientsFile::default());
        };
        let mut text = String::new();
        guard
            .file()
            .take(darkmux_types::user_files::MAX_USER_FILE_BYTES + 1)
            .read_to_string(&mut text)
            .with_context(|| format!("reading {}", self.path.display()))?;
        Ok(parse_clients(&self.path, &text)?.0)
    }

    /// Load, change, save, all under the file's exclusive lock. `f`'s error
    /// saves nothing.
    pub fn update<T>(&self, f: impl FnOnce(&mut ClientsFile) -> Result<T>) -> Result<T> {
        let mut guard = self.lock_current()?;
        let file = guard.file();
        let mut text = String::new();
        file.take(darkmux_types::user_files::MAX_USER_FILE_BYTES + 1)
            .read_to_string(&mut text)
            .with_context(|| format!("reading {}", self.path.display()))?;
        let (mut doc, migrated) = parse_clients(&self.path, &text)?;
        let before = doc.clone();
        let out = f(&mut doc)?;
        // A migration mints ids, so it is written at once: they must be the
        // same at the next read.
        if migrated || doc != before {
            replace_store(&self.path, &doc)?;
        }
        owner_only(&self.path)?;
        Ok(out)
    }

    /// The shared lock on the store's file, `None` when there is none. A
    /// lock that was waited for can be on the file a change replaced
    /// meanwhile ([`ClientStore::lock_current`]); then the file that change
    /// left is locked instead, once.
    fn lock_shared_current(&self) -> Result<Option<darkmux_types::flock::FlockGuard>> {
        let Some(mut guard) = darkmux_types::flock::lock_shared_existing(&self.path)? else {
            return Ok(None);
        };
        if is_file_at(guard.file(), &self.path)? {
            return Ok(Some(guard));
        }
        darkmux_types::flock::lock_shared_existing(&self.path)
    }

    /// The exclusive lock on the file now at the store's path. A change
    /// replaces that file ([`replace_store`]), so a lock that was waited
    /// for can be on the file the change before it replaced; that one is
    /// let go and the new file locked, so a change always builds on the
    /// one before it.
    fn lock_current(&self) -> Result<darkmux_types::flock::FlockGuard> {
        loop {
            let mut guard = darkmux_types::flock::lock_exclusive(&self.path)?;
            if is_file_at(guard.file(), &self.path)? {
                return Ok(guard);
            }
        }
    }
}

/// Whether `file` is the file now at `path` (not one since replaced there;
/// a path with no file now is not it either).
fn is_file_at(file: &std::fs::File, path: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let held = file.metadata().with_context(|| format!("reading {}", path.display()))?;
    match std::fs::metadata(path) {
        Ok(there) => Ok((held.dev(), held.ino()) == (there.dev(), there.ino())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(anyhow!("reading {}: {e}", path.display())),
    }
}

/// Puts `doc` at `path` in one rename, as a workspace's owner record is
/// written: a new owner-only file beside it, synced, takes its place, so
/// the path holds the old store or the new one at every moment, never an
/// emptied or half-written one (an empty file reads as an empty store,
/// which would drop every pairing). Called under the store's lock, so one
/// temp name serves.
fn replace_store(path: &Path, doc: &ClientsFile) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(doc)?;
    bytes.push(b'\n');
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let _ = std::fs::remove_file(&tmp);
    let written = write_new_owner_only(&tmp, &bytes).and_then(|()| std::fs::rename(&tmp, path));
    written.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow!("writing {}: {e}", path.display())
    })
}

/// A new file at `path` (which must not exist) holding `bytes`, readable
/// and writable by its owner only, synced to disk.
fn write_new_owner_only(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Parse the store's text: empty is an empty store; an unknown key, a wrong
/// type or a newer `schema_version` refuses, naming the file and the key.
/// `true` beside it when an older store was migrated.
fn parse_clients(path: &Path, text: &str) -> Result<(ClientsFile, bool)> {
    if text.trim().is_empty() {
        return Ok((ClientsFile::default(), false));
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
    let migrated = doc.get("schema_version").and_then(Value::as_str) != Some(CLIENTS_SCHEMA_VERSION);
    migrate_1_0(&mut doc);
    migrate_1_1(&mut doc)?;
    let issues = darkmux_types::user_files::key_issues::<ClientsFile>(
        &doc,
        &darkmux_types::user_files::no_retired,
    );
    if !issues.is_empty() {
        let lines: Vec<String> = issues.iter().map(ToString::to_string).collect();
        bail!("client store {}: {}", path.display(), lines.join("; "));
    }
    let file = serde_json::from_value(doc).with_context(|| format!("reading {}", path.display()))?;
    Ok((file, migrated))
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
    doc["schema_version"] = Value::from("1.1");
}

/// A 1.1 store's clients had no id; each gets one now. When exactly one
/// client is paired, its id is recorded as `legacy_owner`: the workspaces
/// from before owners are its.
fn migrate_1_1(doc: &mut Value) -> Result<()> {
    if doc.get("schema_version").and_then(Value::as_str) != Some("1.1") {
        return Ok(());
    }
    let mut ids = Vec::new();
    if let Some(items) = doc.get_mut("clients").and_then(Value::as_array_mut) {
        for item in items.iter_mut().filter_map(Value::as_object_mut) {
            if !item.contains_key("id") {
                item.insert("id".to_string(), Value::from(new_client_id()?));
            }
            ids.push(item["id"].clone());
        }
    }
    if let [only] = ids.as_slice() {
        doc["legacy_owner"] = only.clone();
    }
    doc["schema_version"] = Value::from(CLIENTS_SCHEMA_VERSION);
    Ok(())
}

/// A new client id: `c-` and 16 random hex digits.
fn new_client_id() -> Result<String> {
    Ok(format!("c-{}", random_hex(8)?))
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

/// How a pairing request is approved (#3544). Every way goes through
/// [`approve_as`], the one approval path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    /// The operator: `darkmux client approve`.
    Operator,
    /// Allow in the dialog; `remember` is the requesting app's signing
    /// identity when it had one, kept so it pairs at once next time.
    Dialog { remember: Option<SigningIdentity> },
    /// No operator step: the requesting process verified as this identity.
    Verified(SigningIdentity),
}

/// Approve a live request; the token is minted when the app collects it.
/// Returns the client name it will have.
pub fn approve(store: &ClientStore, request_id: &str, now: u64) -> Result<String> {
    approve_as(store, request_id, now, &Approval::Operator)
}

/// [`approve`], saying how it was approved: one write records the approval,
/// the identity that approved it on its own, and the app the dialog's Allow
/// remembers.
pub fn approve_as(store: &ClientStore, request_id: &str, now: u64, how: &Approval) -> Result<String> {
    store.update(|f| {
        let name = approve_in(f, request_id, now)?;
        match how {
            Approval::Operator | Approval::Dialog { remember: None } => {}
            Approval::Verified(app) => {
                if let Some(p) = f.pending.iter_mut().find(|p| p.request_id == request_id) {
                    p.auto_approved = Some(app.clone());
                }
            }
            Approval::Dialog { remember: Some(app) } => remember_in(f, app, now),
        }
        Ok(name)
    })
}

/// Adds `app` to the remembered apps, once. A container engine's team is
/// never remembered: every container behind it would then pair on its own.
fn remember_in(f: &mut ClientsFile, app: &SigningIdentity, now: u64) {
    if darkmux_platform::container_engine_of_team(&app.team).is_some() {
        return;
    }
    if !f.remembered_apps.iter().any(|r| &r.identity() == app) {
        f.remembered_apps.push(RememberedApp { team: app.team.clone(), bundle: app.bundle.clone(), allowed: now });
    }
}

/// The apps the operator allowed in the dialog (`client list`).
pub fn remembered_apps(store: &ClientStore) -> Result<Vec<RememberedApp>> {
    Ok(store.read()?.remembered_apps)
}

/// Forgets a remembered app: its next pairing asks again. `false` when it
/// was not remembered. Its paired clients stay paired (`client revoke`).
pub fn forget(store: &ClientStore, app: &SigningIdentity) -> Result<bool> {
    store.update(|f| {
        let before = f.remembered_apps.len();
        f.remembered_apps.retain(|r| &r.identity() != app);
        Ok(f.remembered_apps.len() != before)
    })
}

/// [`approve`]'s change to the store: the request marked approved, with the
/// client name and id it will have.
fn approve_in(f: &mut ClientsFile, request_id: &str, now: u64) -> Result<String> {
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
    p.client_id = Some(new_client_id()?);
    Ok(name)
}

/// Deny a live request, or take back an approval its app has not collected
/// yet: its token is then never minted, and the app hears `denied`.
pub fn deny(store: &ClientStore, request_id: &str, now: u64) -> Result<()> {
    store.update(|f| {
        let p = f
            .pending
            .iter_mut()
            .find(|p| p.request_id == request_id)
            .ok_or_else(|| anyhow!("no pairing request {request_id}"))?;
        if p.expired(now) || p.status == PendingStatus::Denied {
            bail!("pairing request {request_id} is no longer pending (expired or already denied)");
        }
        p.status = PendingStatus::Denied;
        p.client_name = None;
        p.client_id = None;
        p.auto_approved = None;
        Ok(())
    })
}

pub fn list_clients(store: &ClientStore) -> Result<Vec<ApprovedClient>> {
    Ok(store.read()?.clients)
}

/// Approvals whose app has not collected its token yet (unexpired), oldest
/// first: each will be a client by its `client_name`, and `revoke` of that
/// name takes it back.
pub fn list_uncollected(store: &ClientStore, now: u64) -> Result<Vec<PendingRequest>> {
    let mut out: Vec<PendingRequest> = store
        .read()?
        .pending
        .into_iter()
        .filter(|p| p.status == PendingStatus::Approved && !p.expired(now))
        .collect();
    out.sort_by_key(|p| p.created);
    Ok(out)
}

/// Remove a client by name, and take back an approval its app has not
/// collected yet under that name (denied, so its token is never minted);
/// `false` when there is neither.
pub fn revoke(store: &ClientStore, name: &str) -> Result<bool> {
    store.update(|f| {
        let before = f.clients.len();
        f.clients.retain(|c| c.name != name);
        let mut found = f.clients.len() != before;
        for p in f.pending.iter_mut().filter(|p| p.status == PendingStatus::Approved) {
            if p.client_name.as_deref() == Some(name) {
                p.status = PendingStatus::Denied;
                p.client_name = None;
                p.client_id = None;
                p.auto_approved = None;
                found = true;
            }
        }
        Ok(found)
    })
}

/// The store's `legacy_owner`, while that client is still paired.
fn legacy_heir(store: &ClientStore) -> Result<Option<String>> {
    let f = store.read()?;
    Ok(f.legacy_owner.filter(|id| f.clients.iter().any(|c| &c.id == id)))
}

/// Gives every workspace darkmux made for a paired app (it holds its
/// repository) that is recorded for no client to the store's
/// `legacy_owner`: the one client paired when the store moved to ids, while
/// it stays paired. An app that had workspaces before they had owners keeps
/// them. A store that moved with none or several paired, or whose legacy
/// owner was revoked, gives nothing: no app may use those workspaces until
/// the operator runs `darkmux client claim`. A workspace recorded for a
/// client since revoked is not given away either. Returns the names given.
pub fn adopt_unowned(store: &ClientStore, base: &Path) -> Result<Vec<String>> {
    let Some(id) = legacy_heir(store)? else {
        return Ok(Vec::new());
    };
    let mut given = Vec::new();
    for (name, owner) in crate::client_workspace::held_owners(base) {
        if owner.is_none() {
            crate::client_workspace::record_owner(base, &name, &id).map_err(|e| anyhow!(e))?;
            given.push(name);
        }
    }
    Ok(given)
}

/// The workspaces darkmux made for a paired app that no paired client owns
/// now: never recorded for one, or recorded for a client since revoked. No
/// app may use them until `darkmux client claim` gives one to a client.
/// Reads the store without writing it ([`ClientStore::peek`]).
pub fn unowned_workspaces(store: &ClientStore, base: &Path) -> Result<Vec<String>> {
    let live: Vec<String> = store.peek()?.clients.into_iter().map(|c| c.id).collect();
    Ok(crate::client_workspace::held_owners(base)
        .into_iter()
        .filter(|(_, owner)| !owner.as_ref().is_some_and(|o| live.contains(o)))
        .map(|(n, _)| n)
        .collect())
}

/// `darkmux client claim <workspace> <client>`: gives a workspace darkmux
/// made for a paired app to the paired client named `client` (by its
/// `client list` name), whoever it was recorded for before. Refuses a
/// folder darkmux did not make for an app (a mission's worktrees, anything
/// made by hand). Returns the client's id.
pub fn claim(store: &ClientStore, base: &Path, workspace: &str, client: &str) -> Result<String> {
    let id = store
        .read()?
        .clients
        .into_iter()
        .find(|c| c.name == client)
        .map(|c| c.id)
        .ok_or_else(|| anyhow!("no paired client named {client} (see `darkmux client list`)"))?;
    if !crate::client_workspace::held_owners(base).iter().any(|(n, _)| n == workspace) {
        bail!(
            "{workspace} is not a workspace darkmux made for a paired app under {}",
            base.display()
        );
    }
    crate::client_workspace::record_owner(base, workspace, &id).map_err(|e| anyhow!(e))?;
    Ok(id)
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
    /// (`busy`, `wont_fit`, ...): read from the flow, or (#3505) from the
    /// refusal envelope the child printed, never from `error`.
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
    /// The client's id (never its name, which a later pairing can reuse).
    pub client_id: String,
    pub created: u64,
    /// When it ended (`None` while it runs): what its keep time counts from.
    pub finished: Option<u64>,
    pub status: WorkStatus,
    pub cancel: Arc<CancelFlag>,
    /// The canonical work copy, when the run has one.
    pub workdir: Option<PathBuf>,
    /// The work copy as host git may touch it: the repository darkmux holds
    /// for it, outside it (`None`: no workspace darkmux made, so no git).
    pub git: Option<WorkCopy>,
    /// The work copy as a tree before the run: what its changes are read
    /// against.
    pub baseline: Option<String>,
    /// Undone (its commit reverted): decided once.
    pub decided: Option<String>,
    /// Its Undo is reverting now: no run starts in its work copy meanwhile.
    pub undoing: bool,
    /// The run this one follows up on (a Tweak), when darkmux still knew
    /// it: a label only.
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
/// (#3112) This machine's sandbox as `config.json` says now, read per
/// preview start.
pub type PreviewSandbox = Arc<dyn Fn() -> Result<darkmux_sandbox::BackendKind, String> + Send + Sync>;
/// `clients.preview.idle_minutes`, read per idle check.
pub type PreviewMinutes = Arc<dyn Fn() -> Result<u64, String> + Send + Sync>;
/// (role, work id) -> what that running work's dispatch is doing besides its
/// turns, as the run lifecycle decides it ([`work_activity_in`]).
pub type WorkActivity = Arc<dyn Fn(&str, &str) -> (Option<crate::runs::RunActivity>, Option<u64>) + Send + Sync>;
/// (peer, the listener's address) -> the process on this machine that sent
/// a request from `peer`; `None` when that cannot be told.
/// (the listener's address, the peers asked about) -> every process on this
/// machine holding each of those peers' ends that can be told (sorted, each
/// once: a socket can be held by more than one process, one that inherited
/// it), from one look at every connection to the listener; `None` when none
/// can be told.
pub type PeerProcesses =
    Arc<dyn Fn(Option<SocketAddr>, &[SocketAddr]) -> Option<HashMap<SocketAddr, Vec<u32>>> + Send + Sync>;

/// Who sent each waiting pairing request, found in rounds: one look at every
/// connection to the listener ([`ClientServerState::peer_processes`]) at a
/// time, answering every request that was waiting when it began. A request
/// that arrives while a round runs is answered by the next, so however many
/// connections a flood holds open, a request waits for at most two rounds.
#[derive(Default)]
pub struct PeerRounds {
    /// Held through a round; the requests that need one queue here.
    pub(crate) turn: Arc<tokio::sync::Mutex<()>>,
    /// Rounds begun so far.
    begun: std::sync::atomic::AtomicU64,
    /// The latest finished round's number and answers.
    last: Mutex<(u64, HashMap<SocketAddr, Vec<u32>>)>,
    /// The peers of the requests waiting for an answer, each once per
    /// request.
    waiting: Mutex<Vec<SocketAddr>>,
}

impl PeerRounds {
    /// The answer for `peer` from a round that began after `after`, if one
    /// has finished.
    fn answer_since(&self, after: u64, peer: SocketAddr) -> Option<Vec<u32>> {
        let last = self.last.lock().unwrap();
        (last.0 > after).then(|| last.1.get(&peer).cloned().unwrap_or_default())
    }
}

/// A request waiting for its peer's answer; it stops waiting when dropped.
struct Waiting<'a>(&'a PeerRounds, SocketAddr);

impl<'a> Waiting<'a> {
    fn new(rounds: &'a PeerRounds, peer: SocketAddr) -> Self {
        rounds.waiting.lock().unwrap().push(peer);
        Waiting(rounds, peer)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let mut waiting = self.0.waiting.lock().unwrap();
        if let Some(i) = waiting.iter().position(|p| *p == self.1) {
            waiting.remove(i);
        }
    }
}
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
    /// turns (`starting_model`, `queued`), read per poll.
    pub work_activity: WorkActivity,
    /// The canonical worktrees base workspaces live under.
    pub worktrees_base: PathSource,
    /// (url, ref, dest): clones a repo into `dest`, which must not exist.
    pub clone_repo: Arc<CloneRepo>,
    /// Updates a workspace's remote-tracking refs; never its branch or files.
    pub fetch_repo: PathAction,
    /// One request at a time per thing a workspace is asked for (a bot's
    /// clone of one repo, its worktree of one checkout). Names are held one
    /// by one (`client_workspace::try_hold`), never all at once.
    pub workspace_asks: Arc<crate::client_workspace::Asks>,
    /// Workspaces' live previews (dev servers from `.darkmux/preview.json`).
    pub previews: Arc<crate::client_preview::Previews>,
    /// The address the listener bound, so a `Host` naming another port is
    /// not this listener's ([`crate::is_local_at`]); `None` until bound.
    pub bound: Option<SocketAddr>,
    /// This machine's `clients.preview.enabled`, read per request (`Err`:
    /// unreadable, so previews are refused).
    pub preview_enabled: PreviewSwitch,
    /// (#3112 review) This machine's `runtime.sandbox`, read per preview
    /// start, so the refusal ends with the `config set` it recommends.
    pub preview_sandbox: PreviewSandbox,
    /// This machine's `clients.preview.idle_minutes`, read at every idle
    /// check (`Err`: unreadable, so that check stops nothing).
    pub preview_idle_minutes: PreviewMinutes,
    /// Which process on this machine sent a pairing request.
    pub peer_processes: PeerProcesses,
    /// The rounds [`ClientServerState::peer_processes`] is asked in: one at a
    /// time (each asks the OS, one `lsof` on macOS), each answering every
    /// request waiting when it began.
    pub pair_lookups: Arc<PeerRounds>,
    /// The process each waiting pairing request came from, by request id,
    /// when it could be told: no process holds more than
    /// [`PAIR_REQUESTS_PER_PROCESS`] of them. In memory only; a restart
    /// forgets it (those requests then count only toward [`MAX_PENDING`]).
    pub pair_peers: Arc<Mutex<HashMap<String, u32>>>,
    /// (#3544) `clients.pairing.*`, read per pairing request.
    pub pairing_policy: crate::client_trust::PairingPolicySource,
    /// (#3544) Who signed a process on this machine.
    pub signed_app: crate::client_trust::SignedAppCheck,
    /// (#3544) The Allow dialog, blocking until answered or given up.
    pub ask_allow: crate::client_trust::AskAllow,
    /// (#3544) A notice nobody waits on.
    pub notify: crate::client_trust::Notify,
    /// (#3544) The Allow dialogs open now.
    pub dialogs: Arc<crate::client_trust::DialogSlots>,
}

impl ClientServerState {
    /// Production wiring: this machine's role library and registry, the real
    /// worktrees base, and `darkmux dispatch` as a child of this binary.
    pub fn production() -> Result<Self> {
        let exe = std::env::current_exe().context("locating this darkmux binary")?;
        Ok(Self {
            store: Arc::new(ClientStore::production()),
            now: Arc::new(unix_now),
            role_known: Arc::new(role_known_here),
            resolve_profile: Arc::new(|role, requested| {
                profile_verdict(requested, darkmux_fleet::resolve_work_profile(role, requested, "this machine"))
            }),
            profiles_policy: Arc::new(read_profiles_policy),
            profile_info: Arc::new(crate::client_profiles::describe_profiles),
            profile_is_local: Arc::new(crate::client_profiles::work_stays_here),
            registry_profiles: Arc::new(registry_profiles_here),
            machine: ClientMachine::this_one(),
            check_workdir: Arc::new(check_workdir_here),
            execute: Arc::new(move |spec, cancel| {
                run_dispatch_child(&exe, &spec, &cancel, CANCEL_GRACE, &ProductionTeardown)
            }),
            works: Arc::new(Mutex::new(HashMap::new())),
            work_activity: Arc::new(|role, work_id| {
                work_activity_in(&WORK_ACTIVITY_RECORDS, &darkmux_flow::flows_dir(), role, work_id)
            }),
            worktrees_base: Arc::new(|| canonical_base(&darkmux_types::workdir::worktrees_base_dir())),
            clone_repo: Arc::new(crate::client_workspace::clone_with_git),
            fetch_repo: Arc::new(crate::client_workspace::fetch_with_git),
            workspace_asks: Arc::new(crate::client_workspace::Asks::default()),
            previews: Arc::new(crate::client_preview::Previews::production()),
            bound: None,
            preview_enabled: Arc::new(read_preview_enabled),
            preview_sandbox: Arc::new(read_preview_sandbox),
            preview_idle_minutes: Arc::new(read_preview_idle_minutes),
            peer_processes: Arc::new(peer_processes_here),
            pair_peers: Arc::new(Mutex::new(HashMap::new())),
            pair_lookups: Arc::new(PeerRounds::default()),
            pairing_policy: Arc::new(read_pairing_policy),
            signed_app: Arc::new(|pid| darkmux_platform::current().signed_app(pid)),
            ask_allow: Arc::new(|message, icon, timeout| darkmux_platform::current().ask_allow(message, icon, timeout)),
            notify: Arc::new(|message| darkmux_platform::current().notify(message)),
            dialogs: Arc::new(crate::client_trust::DialogSlots::default()),
        })
    }
}

/// Whether `role` is defined in this machine's role library.
fn role_known_here(role: &str) -> Result<bool, String> {
    let roles = darkmux_crew::loader::load_roles().map_err(|e| format!("{e:#}"))?;
    Ok(roles.iter().any(|r| r.id == role))
}

/// Every profile name in this machine's registry.
fn registry_profiles_here() -> Result<Vec<String>, String> {
    let loaded = darkmux_profiles::profiles::load_registry_quiet(None).map_err(|e| format!("{e:#}"))?;
    Ok(loaded.registry.profiles.keys().cloned().collect())
}

/// The workdir containment check against this machine's worktrees base.
fn check_workdir_here(p: &Path) -> Result<PathBuf, String> {
    darkmux_types::workdir::validate_remote_workdir(p).map_err(|e| format!("{e:#}"))
}

/// What a work request may run on, from how `darkmux dispatch <role>
/// [--profile <p>]` resolves it here: the resolved work profile, never one
/// other than the profile asked for, never a utility-only one.
fn profile_verdict(requested: Option<&str>, resolved: darkmux_fleet::ProfileResolution) -> Result<String, String> {
    use darkmux_fleet::ProfileResolution as R;
    match resolved {
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
}

/// `base`, made if missing, canonical.
fn canonical_base(base: &Path) -> Result<PathBuf, String> {
    let named = |e: std::io::Error| format!("{}: {e}", base.display());
    std::fs::create_dir_all(base).map_err(named)?;
    base.canonicalize().map_err(named)
}

/// Every process holding each of `wanted`'s ends, from one look at every
/// connection to the listener at `bound`
/// ([`darkmux_platform::Platform::tcp_peers_of`]).
fn peer_processes_here(bound: Option<SocketAddr>, wanted: &[SocketAddr]) -> Option<HashMap<SocketAddr, Vec<u32>>> {
    let all = darkmux_platform::current().tcp_peers_of(bound?)?;
    Some(holders_by_peer(all.into_iter().filter(|(peer, _)| wanted.contains(peer))))
}

/// `(peer, pid)` pairs grouped by peer: each peer's holders sorted, each once.
pub(crate) fn holders_by_peer(pairs: impl Iterator<Item = (SocketAddr, u32)>) -> HashMap<SocketAddr, Vec<u32>> {
    let mut by_peer: HashMap<SocketAddr, Vec<u32>> = HashMap::new();
    for (peer, pid) in pairs {
        by_peer.entry(peer).or_default().push(pid);
    }
    for pids in by_peer.values_mut() {
        pids.sort_unstable();
        pids.dedup();
    }
    by_peer
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

/// (#3544) `clients.pairing.*` from `config.json` as it is now, env first
/// (read per request, so `config set` applies without a restart).
fn read_pairing_policy() -> Result<crate::client_trust::PairingPolicy, String> {
    read_pairing_policy_at(&darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser).config)
}

/// [`read_pairing_policy`] against the config at `path`.
pub(crate) fn read_pairing_policy_at(path: &Path) -> Result<crate::client_trust::PairingPolicy, String> {
    use darkmux_types::config_access as ca;
    read_config_at(path).map(|cfg| crate::client_trust::PairingPolicy {
        verified_teams: ca::clients_pairing_verified_teams_in(&cfg),
        dialog: ca::clients_pairing_dialog_in(&cfg),
    })
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

/// (#3112 review) `runtime.sandbox` from `config.json` as it is now, env
/// first, for a preview start.
fn read_preview_sandbox() -> Result<darkmux_sandbox::BackendKind, String> {
    read_preview_sandbox_at(&darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser).config)
}

/// [`read_preview_sandbox`] against the config at `path`.
pub(crate) fn read_preview_sandbox_at(path: &Path) -> Result<darkmux_sandbox::BackendKind, String> {
    darkmux_sandbox::selected_in(&read_config_at(path)?)
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

/// Serve until Ctrl-C, SIGTERM or SIGHUP (blocking; builds its own
/// runtime). Each runs the same cleanup on the way out: running work is
/// canceled and waited for within a bound ([`end_running_work`]), every
/// preview is stopped, and the runtime is ended without waiting on a run
/// that outlived its cancel ([`end_runtime`]).
pub fn run(port: u16) -> Result<()> {
    preflight()?;
    let state = ClientServerState::production()?;
    // Refuse a store this binary cannot read before listening.
    state.store.read()?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let served = rt.block_on(async move {
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
        log_lines(swept_partials(&state));
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
        let (previews, works) = (state.previews.clone(), state.works.clone());
        let stopped_at = Arc::new(std::sync::OnceLock::new());
        let (stopping, cancel_from, stop_began) = (previews.clone(), works.clone(), stopped_at.clone());
        let served = serve_until_stopped(listener, router(state), move || {
            let _ = stop_began.set(begin_stop(&stopping, &cancel_from));
        })
        .await;
        idle_stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let began = stopped_at.get().copied().unwrap_or_else(std::time::Instant::now);
        finish_stop(previews, works, began + SHUTDOWN_WORK_WAIT).await;
        served
    });
    end_runtime(rt, RUNTIME_EXIT_WAIT);
    served
}

/// Sweeps the clones a `client serve` stopped mid-clone left half done
/// ([`crate::client_workspace::sweep_partials`]); a serve-log line each.
pub(crate) fn swept_partials(state: &ClientServerState) -> Vec<String> {
    let Ok(base) = (state.worktrees_base)() else {
        return Vec::new();
    };
    crate::client_workspace::sweep_partials(&base)
        .into_iter()
        .map(|name| format!("darkmux client serve: removed workspace {name}'s unfinished clone (a stop interrupted it)"))
        .collect()
}

/// How long a stopping `client serve` waits, from its stop signal, for the
/// work it canceled to end, and for docker to remove its previews'
/// containers: the executor's own [`CANCEL_GRACE`] (after
/// which it kills a child that would not stop), then room for what follows
/// a kill (its containers stopped, its terminal record written) and for
/// the run's commit. With [`RUNTIME_EXIT_WAIT`] the whole stop fits
/// launchd's 20 s before SIGKILL.
const SHUTDOWN_WORK_WAIT: std::time::Duration = std::time::Duration::from_secs(CANCEL_GRACE.as_secs() + 5);

/// How long the runtime's own tasks get once everything was stopped or
/// given up on.
const RUNTIME_EXIT_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// The stop begins, at its signal: from here nothing new starts
/// ([`refused_while_stopping`], [`admit`]) and every running work item is
/// canceled, so the runs end while open connections drain. Returns when.
fn begin_stop(previews: &crate::client_preview::Previews, works: &Mutex<HashMap<String, WorkEntry>>) -> std::time::Instant {
    previews.begin_shutdown();
    let canceled = cancel_running_work(works);
    if !canceled.is_empty() {
        eprintln!("darkmux client serve: canceling running work ({})", canceled.join(", "));
    }
    std::time::Instant::now()
}

/// The rest of the stop, once the listener is done: every preview's
/// container is removed (#3456) while the canceled work is waited for,
/// each until `work_until` at the latest ([`end_running_work`]); a docker
/// that answers no `stop` or `rm` by then is no longer waited for, and its
/// containers stop themselves once the heartbeat goes stale. Returns the
/// work still running then.
async fn finish_stop(
    previews: Arc<crate::client_preview::Previews>,
    works: Arc<Mutex<HashMap<String, WorkEntry>>>,
    work_until: std::time::Instant,
) -> Vec<String> {
    let ending = tokio::task::spawn_blocking(move || end_running_work(&works, work_until));
    let stopping = tokio::task::spawn_blocking(move || previews.stop_all());
    match tokio::time::timeout_at(work_until.into(), stopping).await {
        Ok(removed) => {
            let removed = removed.unwrap_or_default();
            for name in &removed {
                eprintln!("{}", preview_log_line(name, "shutdown", "stopped"));
            }
            eprintln!("darkmux client serve: previews stopped ({})", removed.len());
        }
        Err(_) => eprintln!(
            "darkmux client serve: docker had not removed every preview container by the stop's bound; \
             no longer waiting (each stops itself once this process's heartbeat goes stale)"
        ),
    }
    let left = ending.await.unwrap_or_default();
    if !left.is_empty() {
        eprintln!(
            "darkmux client serve: work still running after its cancel; no longer waiting for it ({})",
            left.join(", ")
        );
    }
    left
}

/// Raises the cancel of every running work item, as its client's cancel
/// would, and returns their ids, sorted. Raising one twice changes nothing.
fn cancel_running_work(works: &Mutex<HashMap<String, WorkEntry>>) -> Vec<String> {
    let works = works.lock().unwrap();
    let mut running: Vec<String> = works
        .iter()
        .filter(|(_, w)| matches!(w.status, WorkStatus::Running))
        .map(|(id, w)| {
            w.cancel.request();
            id.clone()
        })
        .collect();
    running.sort();
    running
}

/// What `client serve` does about running work as it stops (blocking):
/// cancels every running item and waits for each to end as a canceled run
/// does (its terminal state, its edits committed), but never past `until`.
/// Returns the ids still running then, which the exit no longer waits for.
pub(crate) fn end_running_work(works: &Mutex<HashMap<String, WorkEntry>>, until: std::time::Instant) -> Vec<String> {
    loop {
        let running = cancel_running_work(works);
        if running.is_empty() || std::time::Instant::now() >= until {
            return running;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Ends the runtime without waiting on its blocking tasks past `within`: a
/// run that outlived its cancel is on one of them, and dropping the runtime
/// would wait for it without bound.
pub(crate) fn end_runtime(rt: tokio::runtime::Runtime, within: std::time::Duration) {
    rt.shutdown_timeout(within);
}

/// Ends what a client that is no longer paired left going: its running
/// work is canceled and its live previews stopped. `client revoke` only
/// edits the store, and once it has, no caller owns them, so nothing could
/// stop them over the API. Returns the serve-log lines (each thing is
/// ended, and said, once). A store that cannot be read ends nothing.
pub fn unpaired_tick(state: &ClientServerState) -> Vec<String> {
    let Ok(store) = state.store.peek() else {
        return Vec::new();
    };
    let paired: std::collections::HashSet<String> = store.clients.into_iter().map(|c| c.id).collect();
    let mut lines = cancel_unpaired_work(&state.works, &paired);
    let Ok(base) = (state.worktrees_base)() else {
        return lines;
    };
    let unpaired = |workspace: &str| {
        crate::client_workspace::owner_of(&base, workspace).is_some_and(|owner| !paired.contains(&owner))
    };
    for name in state.previews.stop_live(unpaired) {
        lines.push(preview_log_line(&name, "revoked", "stopped: its app is no longer paired"));
    }
    lines
}

/// Raises the cancel of every running work item whose client is not in
/// `paired`, once each. Returns a serve-log line for each, sorted.
fn cancel_unpaired_work(
    works: &Mutex<HashMap<String, WorkEntry>>,
    paired: &std::collections::HashSet<String>,
) -> Vec<String> {
    let works = works.lock().unwrap();
    let mut lines: Vec<String> = works
        .iter()
        .filter(|(_, w)| matches!(w.status, WorkStatus::Running) && !w.cancel.requested())
        .filter(|(_, w)| !paired.contains(&w.client_id))
        .map(|(id, w)| {
            w.cancel.request();
            format!("darkmux client serve: work {id}: canceled: its app is no longer paired")
        })
        .collect();
    lines.sort();
    lines
}

/// What tells one version of the store's file from the next, from one
/// `stat`: every change replaces the file ([`replace_store`]).
fn store_stamp(store: &ClientStore) -> Option<(Option<std::time::SystemTime>, u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(store.path()).ok()?;
    Some((meta.modified().ok(), meta.len(), meta.ino()))
}

/// How long a stopping `client serve` waits for open connections before it
/// stops its previews. Then the previews stop all at once, each `docker
/// stop -t 5` (about five seconds for a server that traps TERM), so the
/// whole shutdown fits launchd's 20 s before SIGKILL; only a wedged docker
/// call (bounded at 30 s) can outlast it.
const SHUTDOWN_DRAIN: std::time::Duration = std::time::Duration::from_secs(5);

/// Serves until a signal. Then `on_stop` runs (from here on, a request that
/// could start something is refused: [`refused_while_stopping`]), new
/// connections are no longer accepted, and open ones get at most
/// [`SHUTDOWN_DRAIN`], or until a second signal. Past that, darkmux stops
/// waiting for them and moves on to its cleanup; a connection still open
/// (one holding half a request, which a container can do) is dropped when
/// the process exits, and anything it finishes asking for meanwhile is
/// refused.
async fn serve_until_stopped(
    listener: tokio::net::TcpListener,
    app: Router,
    on_stop: impl FnOnce(),
) -> Result<()> {
    let stop = Arc::new(tokio::sync::Notify::new());
    let graceful = stop.clone();
    let server = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move { graceful.notified().await });
    let mut serving = tokio::spawn(std::future::IntoFuture::into_future(server));
    let ended = tokio::select! {
        ended = &mut serving => Some(ended),
        signal = shutdown_signal() => {
            eprintln!("darkmux client serve: stopping ({signal})");
            None
        }
    };
    if ended.is_none() {
        on_stop();
    }
    let ended = match ended {
        Some(ended) => ended,
        None => {
            stop.notify_one();
            tokio::select! {
                ended = &mut serving => ended,
                _ = tokio::time::sleep(SHUTDOWN_DRAIN) => {
                    eprintln!(
                        "darkmux client serve: connections still open after {}s; no longer waiting for them",
                        SHUTDOWN_DRAIN.as_secs()
                    );
                    serving.abort();
                    Ok(Ok(()))
                }
                signal = shutdown_signal() => {
                    eprintln!("darkmux client serve: {signal} again; no longer waiting for open connections");
                    serving.abort();
                    Ok(Ok(()))
                }
            }
        }
    };
    ended.context("the server task")?.context("serving")
}

/// The first of SIGINT (Ctrl-C), SIGTERM (launchd stopping the job, a
/// plain `kill`) and SIGHUP (its terminal closed), by name. Caught through
/// `tokio::signal` like the viewer daemon's own shutdown, so each reaches
/// the cleanup after the server stops instead of ending the process where
/// it stands.
async fn shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let wait = |kind: SignalKind| async move {
            match signal(kind) {
                Ok(mut s) => {
                    s.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => "SIGINT",
            _ = wait(SignalKind::terminate()) => "SIGTERM",
            _ = wait(SignalKind::hangup()) => "SIGHUP",
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "Ctrl-C"
    }
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
    if let Some((status, code, why)) = foreign_request(&req, state.bound) {
        return err(status, code, why);
    }
    match refused_while_stopping(&state, req).await {
        Ok(req) => next.run(req).await,
        Err(refused) => refused,
    }
}

/// Once `client serve` has begun to stop, a request that could start or
/// change something (every method but GET and HEAD) is refused with
/// `unavailable`. Its body is read first (the limit every handler applies),
/// so the check lands when the whole request has arrived: one whose body
/// was still coming when the stop began is refused too, never acted on
/// after the previews were stopped.
async fn refused_while_stopping(
    state: &ClientServerState,
    req: axum::extract::Request,
) -> Result<axum::extract::Request, Response> {
    use axum::http::Method;
    if matches!(*req.method(), Method::GET | Method::HEAD) {
        return Ok(req);
    }
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES).await.map_err(|_| {
        err(
            StatusCode::PAYLOAD_TOO_LARGE, ClientCode::PayloadTooLarge,
            format!("body larger than {MAX_BODY_BYTES} bytes"),
        )
    })?;
    if state.previews.stopping() {
        return Err(stopping_refusal());
    }
    Ok(axum::extract::Request::from_parts(parts, Body::from(bytes)))
}

/// The refusal of a request that could start something once `client serve`
/// has begun to stop.
fn stopping_refusal() -> Response {
    err(
        StatusCode::SERVICE_UNAVAILABLE, ClientCode::Unavailable,
        "darkmux client serve is stopping; send the request again once it is back",
    )
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

pub(crate) async fn pair_handler(
    State(state): State<ClientServerState>,
    peer: Option<axum::extract::ConnectInfo<SocketAddr>>,
    body: Body,
) -> Response {
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
    let (record, holders) = match new_request(&state, req, peer, now).await {
        Ok(made) => made,
        Err(e) => return internal(e),
    };
    let (request_id, code) = (record.request_id.clone(), record.code.clone());
    // The per-process bound counts a shared connection against its lowest
    // holder; trust needs exactly one ([`crate::client_trust::sender_of`]).
    let from = holders.first().copied();
    let sender = crate::client_trust::sender_of(&state, &holders).await;
    match add_pending(&state, record, from, now) {
        Ok(true) => {
            let peer = peer.map(|axum::extract::ConnectInfo(p)| p);
            log_lines(crate::client_trust::decide(&state, (&request_id, &code), peer, from, sender).await);
            (StatusCode::ACCEPTED, Json(PairAccepted { request_id })).into_response()
        }
        Ok(false) => err(
            StatusCode::TOO_MANY_REQUESTS, ClientCode::TooManyRequests,
            "too many pairing requests are waiting; try again later",
        ),
        Err(e) => internal(e),
    }
}

/// A new pending request for `req`, and the processes holding the
/// connection it came on, as far as that can be told ([`peer_holders_of`]).
async fn new_request(
    state: &ClientServerState,
    req: PairBody,
    peer: Option<axum::extract::ConnectInfo<SocketAddr>>,
    now: u64,
) -> Result<(PendingRequest, Vec<u32>)> {
    let request_id = format!("r-{}", random_hex(16)?);
    let from = peer_holders_of(state, peer).await;
    let record = PendingRequest {
        request_id,
        device_name: req.device_name,
        code: req.code,
        pair_secret_hash: hash_hex(&req.pair_secret),
        created: now,
        status: PendingStatus::Pending,
        client_name: None,
        client_id: None,
        auto_approved: None,
    };
    Ok((record, from))
}

/// Every process on this machine holding the connection a pairing request
/// came on (empty when that cannot be told), from a round ([`PeerRounds`])
/// that began after it arrived: the one that finished while it queued, else
/// one it runs itself, off the async workers, for every request waiting. A
/// request whose sender went away while it queued is dropped before it
/// starts a round.
pub(crate) async fn peer_holders_of(state: &ClientServerState, peer: Option<axum::extract::ConnectInfo<SocketAddr>>) -> Vec<u32> {
    use std::sync::atomic::Ordering::SeqCst;
    let Some(axum::extract::ConnectInfo(p)) = peer else {
        return Vec::new();
    };
    let rounds = state.pair_lookups.clone();
    let _waiting = Waiting::new(&rounds, p);
    let arrived = rounds.begun.load(SeqCst);
    let turn = rounds.turn.clone().lock_owned().await;
    if let Some(answer) = rounds.answer_since(arrived, p) {
        return answer;
    }
    let round = rounds.begun.fetch_add(1, SeqCst) + 1;
    let wanted = rounds.waiting.lock().unwrap().clone();
    let (st, r) = (state.clone(), rounds.clone());
    // The turn and the answers go with the round, so it runs to its end and
    // is the only one running even if this request's sender goes away.
    tokio::task::spawn_blocking(move || {
        let _one_round_at_a_time = turn;
        let found = (st.peer_processes)(st.bound, &wanted).unwrap_or_default();
        let holders = found.get(&p).cloned().unwrap_or_default();
        *r.last.lock().unwrap() = (round, found);
        holders
    })
    .await
    .unwrap_or_default()
}

/// Stores a new pairing request from process `from` under the bounds
/// ([`make_room`], then [`MAX_PENDING`]); `false` when the queue is full.
fn add_pending(state: &ClientServerState, record: PendingRequest, from: Option<u32>, now: u64) -> Result<bool> {
    state.store.update(|f| {
        prune(f, now);
        let mut peers = state.pair_peers.lock().unwrap();
        make_room(f, &mut peers, from, now);
        if f.pending.iter().filter(|p| p.live(now)).count() >= MAX_PENDING {
            return Ok(false);
        }
        if let Some(pid) = from {
            peers.insert(record.request_id.clone(), pid);
        }
        f.pending.push(record);
        Ok(true)
    })
}

/// Forgets the processes of requests no longer waiting; then, when process
/// `from` already has [`PAIR_REQUESTS_PER_PROCESS`] requests waiting, drops
/// its oldest (in the order they were made), so no one process on this
/// machine can fill the queue. Every container's connection arrives from
/// Docker's one forwarding process, with any `Host` it forges; the app on
/// this machine is a process of its own.
fn make_room(f: &mut ClientsFile, peers: &mut HashMap<String, u32>, from: Option<u32>, now: u64) {
    peers.retain(|id, _| f.pending.iter().any(|p| &p.request_id == id && p.live(now)));
    let Some(pid) = from else { return };
    let mut mine: Vec<String> = f
        .pending
        .iter()
        .filter(|p| p.live(now) && peers.get(&p.request_id) == Some(&pid))
        .map(|p| p.request_id.clone())
        .collect();
    while mine.len() >= PAIR_REQUESTS_PER_PROCESS {
        let oldest = mine.remove(0);
        f.pending.retain(|p| p.request_id != oldest);
        peers.remove(&oldest);
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
    let id = match p.client_id.clone() {
        Some(id) => id,
        None => new_client_id()?,
    };
    f.clients.push(ApprovedClient {
        name: client_name.clone(),
        id,
        created: now,
        token_hash: hash_hex(&token),
        auto_approved: p.auto_approved.clone(),
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
    let (client, req) = match work_request(&state, &headers, body).await {
        Ok(asked) => asked,
        Err(r) => return *r,
    };
    let (answer, answered) = tokio::sync::oneshot::channel();
    tokio::task::spawn_blocking(move || admit_and_run(&state, &client, req, answer));
    match answered.await {
        Ok(Ok(accepted)) => (StatusCode::ACCEPTED, Json(accepted)).into_response(),
        Ok(Err(r)) => *r,
        Err(_) => internal(anyhow!("work admission panicked")),
    }
}

/// The request's client and body, before anything is registered.
async fn work_request(
    state: &ClientServerState,
    headers: &HeaderMap,
    body: Body,
) -> Result<(ApprovedClient, WorkBody), Refused> {
    let client = authenticate(state, headers)?;
    let req: WorkBody = json_body(body).await.map_err(Box::new)?;
    if req.message.trim().is_empty() {
        return Err(refuse(StatusCode::BAD_REQUEST, ClientCode::BadRequest, "`message` is empty"));
    }
    Ok((client, req))
}

/// Admits the request ([`admit`]) and runs what it admitted, in one task
/// off the async workers (the checks read the role library, the registry,
/// config.json and the disk), answering through `answer` in between. The
/// request's own future only waits for that answer: a client that hangs up
/// while its request is being admitted drops the future, never the task,
/// so work registered as running always has its runner.
fn admit_and_run(
    state: &ClientServerState,
    client: &ApprovedClient,
    req: WorkBody,
    answer: tokio::sync::oneshot::Sender<Result<WorkAccepted, Refused>>,
) {
    let (spec, cancel, _in_use) = match admit(state, client, req) {
        Ok(admitted) => admitted,
        Err(r) => {
            let _ = answer.send(Err(r));
            return;
        }
    };
    // Nobody waiting for the answer changes nothing: the work is registered.
    let _ = answer.send(Ok(WorkAccepted { work_id: spec.work_id.clone(), profile: spec.profile.clone() }));
    run_work(&state.execute, &state.works, spec, &cancel, &*state.now);
}

/// Runs one admitted work item to its end, blocking, and records how it went.
fn run_work(
    execute: &Execute,
    works: &Mutex<HashMap<String, WorkEntry>>,
    spec: WorkSpec,
    cancel: &Arc<CancelFlag>,
    now: &(dyn Fn() -> u64 + Send + Sync),
) {
    let id = spec.work_id.clone();
    // Host git touches only the repository darkmux holds for the work copy.
    let git = works.lock().unwrap().get(&id).and_then(|e| e.git.clone());
    // The work copy as it is before the run, so what it held already is
    // never reported as the run's change. Every earlier run's changes are
    // committed by then, a tweak's included.
    let baseline = git.as_ref().map(snapshot_tree);
    if let Some(e) = works.lock().unwrap().get_mut(&id) {
        e.baseline = baseline.as_ref().and_then(|b| b.as_ref().ok()).map(|b| b.tree.clone());
    }
    let flag = cancel.clone();
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| execute(spec, flag)));
    let status = run_status(ran, cancel, git.as_ref(), baseline.as_ref());
    // What the run changed becomes one commit on the work copy's branch,
    // however it ended: a checkpoint Undo can revert, and never left
    // uncommitted for the next run's commit to carry.
    let (changed, unread) = run_files(&status, git.as_ref(), baseline.as_ref());
    let (title, role) = works
        .lock()
        .unwrap()
        .get(&id)
        .map(|e| (e.title.clone(), e.role.clone()))
        .unwrap_or_default();
    let committed = match (&git, changed.is_empty()) {
        (Some(w), false) => Some(commit_run(w, &changed, &commit_message(&role, &title, &id, &status))),
        _ => None,
    };
    if let Some(w) = works.lock().unwrap().get_mut(&id) {
        w.status = status;
        w.finished = Some(now());
        w.files = changed;
        w.commit_error = unread;
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

/// The paths an ended run changed, and (for a run that failed or was
/// cancelled) why they could not be read. A finished run's changes are in
/// its status already; one that failed or was cancelled is read now, so
/// what it left is committed as its own.
fn run_files(
    status: &WorkStatus,
    git: Option<&WorkCopy>,
    baseline: Option<&Result<Snapshot, String>>,
) -> (Vec<String>, Option<String>) {
    match status {
        WorkStatus::Error(_) | WorkStatus::Cancelled(_) => match run_changes(git, baseline) {
            (Some(c), _) => (c.files.into_iter().map(|f| f.path).collect(), None),
            (None, why) => (Vec::new(), why),
        },
        _ => (changed_paths(status), None),
    }
}

/// A run's commit message: its role and title, and how it ended when it did
/// not finish.
fn commit_message(role: &str, title: &str, id: &str, status: &WorkStatus) -> String {
    let ended = match status {
        WorkStatus::Error(_) => ", failed",
        WorkStatus::Cancelled(_) => ", cancelled",
        _ => "",
    };
    format!("{role}: {title} (work {id}{ended})")
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

/// How a run ended. A run that produced its result finished, whenever a
/// cancel landed (one after `dispatch.complete` stops nothing); one that
/// did not and was cancelled ends as cancelled, however its child reported.
fn run_status(
    ran: std::thread::Result<Result<Value, RunError>>,
    cancel: &CancelFlag,
    git: Option<&WorkCopy>,
    baseline: Option<&Result<Snapshot, String>>,
) -> WorkStatus {
    match ran {
        Ok(Err(e)) if cancel.requested() => WorkStatus::Cancelled(Some(e)),
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
/// registered as running and its spec (with the RESOLVED profile) returned,
/// with the flag that cancels it and the hold on its workspace's name.
fn admit(
    state: &ClientServerState,
    client: &ApprovedClient,
    req: WorkBody,
) -> Result<(WorkSpec, Arc<CancelFlag>, Option<crate::client_workspace::NameHold>), Refused> {
    check_role(state, &req.role)?;
    let workdir = admitted_workdir(state, client, &req)?;
    let profile = admitted_profile(state, &req)?;
    let work_id = format!("w-{}", random_hex(8).map_err(internal)?);
    let now = (state.now)();
    let git = workdir.as_deref().and_then(|w| work_copy_for(state, w));
    // Held until the run has ended and committed, so the workspace cannot
    // be removed under it, by this process or `client workspace rm`.
    let in_use = held_work_copy(state, git.as_ref())?;
    let cancel = Arc::new(CancelFlag::default());
    let mut works = state.works.lock().unwrap();
    // Read under the lock the stop takes to look at the running work
    // ([`end_running_work`]): a request admitted before the stop began is
    // registered by the time it looks, and none is registered after.
    if state.previews.stopping() {
        return Err(Box::new(stopping_refusal()));
    }
    check_capacity(&mut works, client, now)?;
    check_work_copy(&works, client, workdir.as_ref(), req.continues.as_deref())?;
    let continues = req.continues.clone().filter(|c| works.contains_key(c));
    works.insert(
        work_id.clone(),
        WorkEntry {
            client_id: client.id.clone(),
            created: now,
            finished: None,
            status: WorkStatus::Running,
            cancel: cancel.clone(),
            git: git.clone(),
            workdir: workdir.clone(),
            baseline: None,
            decided: None,
            undoing: false,
            continues,
            title: work_title(&req),
            role: req.role.clone(),
            commit: None,
            files: Vec::new(),
            branch: None,
            commit_error: None,
        },
    );
    // Registered as running, so nothing else starts in the copy while it
    // is moved off its base branch.
    drop(works);
    if let Err(r) = onto_own_branch(state, git.as_ref()) {
        state.works.lock().unwrap().remove(&work_id);
        return Err(r);
    }
    let spec = WorkSpec {
        work_id,
        role: req.role,
        profile,
        workdir,
        message: req.message,
    };
    Ok((spec, cancel, in_use))
}

/// A work copy with an origin that is not on a `mux/` branch of its own (a
/// clone made before clones got one, on the default branch or the ref it
/// was cloned at, which an app still uses by name) is moved onto a new
/// branch of its bot's before a run, so the run never commits to a branch
/// of the user's and a PR never pushes onto one. The commits already on it
/// go along. A template project has no origin, and stays on its own `main`.
fn onto_own_branch(state: &ClientServerState, wc: Option<&WorkCopy>) -> Result<(), Refused> {
    let Some(wc) = wc else { return Ok(()) };
    let Some(on) = not_on_own_branch(wc) else { return Ok(()) };
    let base = (state.worktrees_base)().map_err(|e| Box::new(internal(anyhow!(e))))?;
    let name = wc.work_tree.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let branch = new_bot_branch(&bot_slug(crate::client_workspace::bot_of(&base, &name).as_deref()))?;
    head_onto_new_branch(wc, &branch).map_err(|e| branch_failed(&format!("moving {name} off {on} onto {branch}: {e}")))
}

/// The branch `wc` is checked out on (empty when detached) when it has an
/// origin and that is no `mux/` branch of darkmux's.
fn not_on_own_branch(wc: &WorkCopy) -> Option<String> {
    crate::client_workspace::remote_default_of(wc)?;
    let branch = git_line(wc, &["branch", "--show-current"])?;
    (!is_own_branch(&branch)).then_some(branch)
}

/// Whether `branch` is one darkmux made for a workspace's own work.
fn is_own_branch(branch: &str) -> bool {
    branch.starts_with("mux/")
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

/// Where the run works: a client-named workdir or a workspace, never both,
/// and always one darkmux made for this client ([`owned_workspace`]).
fn admitted_workdir(
    state: &ClientServerState,
    client: &ApprovedClient,
    req: &WorkBody,
) -> Result<Option<PathBuf>, Refused> {
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
            Ok(Some(owned_workspace(state, client, &name)?))
        }
        (None, Some(name)) => Ok(Some(owned_workspace(state, client, name)?)),
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

/// Forgets ended work past its keep time (counted from when it ended), then
/// refuses a client already at its cap of running work.
fn check_capacity(
    works: &mut HashMap<String, WorkEntry>,
    client: &ApprovedClient,
    now: u64,
) -> Result<(), Refused> {
    works.retain(|_, w| {
        let ended = w.finished.unwrap_or(w.created);
        matches!(w.status, WorkStatus::Running) || w.undoing || now.saturating_sub(ended) <= KEEP_WORK_SECS
    });
    let running = works
        .values()
        .filter(|w| w.client_id == client.id && matches!(w.status, WorkStatus::Running))
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

/// One run at a time per work copy. Every ended run's changes are
/// committed, so a new run needs no decision first. A Tweak's `continues`
/// is a label: one naming a run darkmux still knows must be this client's,
/// in this work copy (however it ended: a failed or cancelled run's edits
/// are committed like a finished one's); one it has forgotten (past its
/// keep time, or before a restart) makes a plain new run.
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
    if let Some(r) = busy_copy(works, w) {
        return Err(r);
    }
    let foreign = continues
        .and_then(|c| works.get(c).map(|e| (c, e)))
        .filter(|(_, e)| e.client_id != client.id || e.workdir.as_ref() != Some(w));
    if let Some((c, _)) = foreign {
        return Err(refuse(
            StatusCode::CONFLICT, ClientCode::Conflict,
            format!("run {c} isn't a run of yours in this work copy"),
        ));
    }
    Ok(())
}

/// Why nothing may start in work copy `w` now, if something is using it:
/// a run working in it, or an Undo reverting in it.
fn busy_copy(works: &HashMap<String, WorkEntry>, w: &PathBuf) -> Option<Refused> {
    let mut here = works.iter().filter(|(_, e)| e.workdir.as_ref() == Some(w));
    let (id, e) = here.find(|(_, e)| e.undoing || matches!(e.status, WorkStatus::Running))?;
    let why = if e.undoing {
        format!("run {id} is being undone in this work copy")
    } else {
        format!("run {id} is already working in this work copy")
    };
    Some(refuse(StatusCode::CONFLICT, ClientCode::Conflict, why))
}

/// Holds a work copy for one Undo: while held, no run starts there and no
/// other Undo reverts there ([`busy_copy`]). Released when dropped.
struct UndoHold {
    works: Arc<Mutex<HashMap<String, WorkEntry>>>,
    id: String,
}

impl UndoHold {
    /// Holds run `id`'s work copy, or says what is using it (or that an
    /// Undo that ran meanwhile already undid the run).
    fn take(works: &Arc<Mutex<HashMap<String, WorkEntry>>>, id: &str) -> Result<UndoHold, Refused> {
        let mut all = works.lock().unwrap();
        let entry = all.get(id).ok_or_else(|| Box::new(not_found()))?;
        if entry.decided.is_some() {
            return Err(refuse(StatusCode::CONFLICT, ClientCode::Conflict, "already undone"));
        }
        let w = entry.workdir.clone().ok_or_else(|| Box::new(not_found()))?;
        if let Some(r) = busy_copy(&all, &w) {
            return Err(r);
        }
        if let Some(e) = all.get_mut(id) {
            e.undoing = true;
        }
        Ok(UndoHold { works: works.clone(), id: id.to_string() })
    }
}

impl Drop for UndoHold {
    fn drop(&mut self) {
        if let Some(e) = self.works.lock().unwrap().get_mut(&self.id) {
            e.undoing = false;
        }
    }
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
    let client = match authenticate(&state, &headers) {
        Ok(c) => c,
        Err(r) => return *r,
    };
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
    tokio::task::spawn_blocking(move || recipe_of(&st, &client, &q))
        .await
        .unwrap_or_else(|e| internal(anyhow!("recipe read panicked: {e}")))
}

/// The recipe of the workspace `q` names (by name or by its directory),
/// when it is `client`'s.
fn recipe_of(state: &ClientServerState, client: &ApprovedClient, q: &RecipeQuery) -> Response {
    let name = match (&q.workdir, &q.workspace) {
        (Some(w), _) => workspace_of_workdir(state, w),
        (_, Some(n)) => Ok(n.clone()),
        _ => unreachable!("checked above"),
    };
    let owned = name.and_then(|n| owned_workspace(state, client, &n).map(|_| n));
    match owned.and_then(|n| workspace_and_recipe_source(state, &n)) {
        Ok((_dir, source)) => read_recipe(&source),
        Err(r) => *r,
    }
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

/// [`workspace_dir`], and only when darkmux made that workspace for
/// `client` (its owner record, by client id, written when the workspace was
/// made, by [`adopt_unowned`] or by `client claim`): a 403 for another app's
/// workspace, a revoked client's, a mission's worktrees or any folder no app
/// asked darkmux for.
fn owned_workspace(state: &ClientServerState, client: &ApprovedClient, name: &str) -> Result<PathBuf, Refused> {
    let dir = workspace_dir(state, name)?;
    let base = (state.worktrees_base)().map_err(|e| internal(anyhow!(e)))?;
    let owner = match crate::client_workspace::owner_of(&base, name) {
        None => {
            adopt_unowned(&state.store, &base).map_err(internal)?;
            crate::client_workspace::owner_of(&base, name)
        }
        recorded => recorded,
    };
    if owner.as_deref() != Some(client.id.as_str()) {
        return Err(forbidden(format!(
            "workspace {name} was not made for this app: an app works only in a workspace darkmux \
             made for it (POST /client/workspaces)"
        )));
    }
    Ok(dir)
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

/// `POST /client/workspaces`: `{repo_url}` reuses this bot's clone of that
/// repo (fetching it) or clones it onto a new `mux/<bot>-<id>` branch;
/// `{source_path}` reuses this bot's worktree of that checkout or adds one on
/// a new `mux/<bot>-<id>` branch, so the owner's own working tree is never
/// touched. Never another bot's workspace or branch. 201 when created, 200
/// when reused.
pub(crate) async fn workspace_create_handler(
    State(state): State<ClientServerState>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    use crate::client_workspace as ws;
    let client = match authenticate(&state, &headers) {
        Ok(c) => c.id,
        Err(r) => return *r,
    };
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
    let exact = req.bot.clone().unwrap_or_default();
    let ask = match workspace_ask(&req) {
        Ok(ask) => ask,
        Err(r) => return *r,
    };
    let st = state.clone();
    let git_ref = req.git_ref;
    tokio::task::spawn_blocking(move || {
        let bot = crate::client_workspace::BotName { slug: &bot, exact: &exact };
        make_workspace(&st, ask, Made { git_ref: git_ref.as_deref(), client: &client, bot })
    })
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

/// A workspace reused with no bot's exact name recorded (made before the
/// name was recorded, matched by its branch) becomes this bot's now, so a
/// later bot whose name makes the same slug gets its own; then `reuse` it.
fn claimed(base: &Path, name: &str, made: &Made, reuse: impl FnOnce() -> Response) -> Response {
    if crate::client_workspace::bot_of(base, name).is_none() {
        if let Err(e) = crate::client_workspace::record_bot(base, name, made.bot.exact) {
            return internal(anyhow!(e));
        }
    }
    reuse()
}

/// The client's own workspace `find` names, if any, after
/// [`adopt_unowned`] has given out what it may.
fn reusable(state: &ClientServerState, base: &Path, find: impl FnOnce() -> Option<String>) -> Result<Option<String>, Refused> {
    adopt_unowned(&state.store, base).map_err(|e| Box::new(internal(e)))?;
    Ok(find())
}

/// Makes (or hands back) a workspace for paired client `client`: every
/// workspace darkmux makes records the client it was made for, and only
/// that client may use it ([`owned_workspace`]).
fn make_workspace(state: &ClientServerState, ask: WorkspaceAsk, made: Made) -> Response {
    match ask {
        WorkspaceAsk::Clone { url, normalized } => clone_workspace(state, &url, &normalized, &made),
        WorkspaceAsk::Template { url, normalized } => template_workspace(state, &url, &normalized, &made),
        WorkspaceAsk::Worktree { path } => worktree_workspace(state, Path::new(&path), &made),
    }
}

/// What every way of making a workspace is asked with: the branch to start
/// on, the paired client it is for, and the bot asking.
struct Made<'a> {
    git_ref: Option<&'a str>,
    client: &'a str,
    bot: crate::client_workspace::BotName<'a>,
}

/// This bot's clone of the repo (fetched), else a new clone on a new
/// `mux/<bot>-<id>` branch of its own, so its runs never commit to the
/// branch the clone checked out.
fn clone_workspace(state: &ClientServerState, url: &str, normalized: &str, made: &Made) -> Response {
    use crate::client_workspace as ws;
    let base = match (state.worktrees_base)() {
        Ok(b) => b,
        Err(e) => return internal(anyhow!(e)),
    };
    let _asked = state.workspace_asks.hold(ask_key("clone", made, normalized));
    let found = reusable(state, &base, || ws::find_by_origin(&base, normalized, made.bot, made.client));
    reuse_or_make(
        &base,
        found,
        |name| claimed(&base, name, made, || existing_clone(state, name)),
        || new_clone(state, &base, (url, normalized), made),
    )
}

/// The workspace `found` names, held for reuse and handed to `reuse`, or a
/// new one from `make` when there is none (or it was removed meanwhile).
fn reuse_or_make(
    base: &Path,
    found: Result<Option<String>, Refused>,
    reuse: impl FnOnce(&str) -> Response,
    make: impl FnOnce() -> Response,
) -> Response {
    let name = match found {
        Ok(Some(name)) => name,
        Ok(None) => return make(),
        Err(r) => return *r,
    };
    match held_to_reuse(base, &name) {
        Ok(Some(_held)) => reuse(&name),
        Ok(None) => make(),
        Err(r) => *r,
    }
}

/// What one bot of one client asks for (`what` of `of`), as an
/// [`crate::client_workspace::Asks`] key. By the bot's slug, the name its
/// branches carry, so bots whose names make one slug take turns.
fn ask_key(what: &str, made: &Made, of: &str) -> String {
    format!("{what}\0{}\0{}\0{of}", made.client, made.bot.slug)
}

/// Holds workspace `name` to reuse it ([`Hold::Shared`]), and only while it
/// is still there: `None` when it was removed after it was found; a 409
/// while it is being removed.
///
/// [`Hold::Shared`]: crate::client_workspace::Hold::Shared
fn held_to_reuse(base: &Path, name: &str) -> Result<Option<crate::client_workspace::NameHold>, Refused> {
    use crate::client_workspace as ws;
    match ws::try_hold(base, name, ws::Hold::Shared) {
        Ok(Some(held)) => Ok(crate::client_git::work_copy_of(base, name, &base.join(name)).map(|_| held)),
        Ok(None) => Err(being_removed(name)),
        Err(e) => Err(Box::new(internal(anyhow!(e)))),
    }
}

/// Workspace `name` held for use ([`crate::client_workspace::Hold::Shared`]):
/// a 409 while it is being removed.
fn held_in_use(state: &ClientServerState, name: &str) -> Result<crate::client_workspace::NameHold, Refused> {
    use crate::client_workspace as ws;
    let base = (state.worktrees_base)().map_err(|e| Box::new(internal(anyhow!(e))))?;
    match ws::try_hold(&base, name, ws::Hold::Shared) {
        Ok(Some(held)) => Ok(held),
        Ok(None) => Err(being_removed(name)),
        Err(e) => Err(Box::new(internal(anyhow!(e)))),
    }
}

/// The workspace work copy `wc` is (its folder's name, directly under the
/// worktrees base), held for use as [`held_in_use`] holds it; `None` for a
/// work copy that is no workspace.
fn held_work_copy(state: &ClientServerState, wc: Option<&WorkCopy>) -> Result<Option<crate::client_workspace::NameHold>, Refused> {
    let Some(wc) = wc else { return Ok(None) };
    let base = (state.worktrees_base)().map_err(|e| Box::new(internal(anyhow!(e))))?;
    match (wc.work_tree.parent(), wc.work_tree.file_name().and_then(|n| n.to_str())) {
        (Some(parent), Some(name)) if parent == base && crate::client_workspace::valid_name(name) => {
            held_in_use(state, name).map(Some)
        }
        _ => Ok(None),
    }
}

/// A workspace whose name another request holds alone: it is being made
/// or removed.
fn being_removed(name: &str) -> Refused {
    refuse(
        StatusCode::CONFLICT, ClientCode::Conflict,
        format!("workspace {name} is being made or removed; ask again in a moment"),
    )
}

/// A free name from `names`, held alone ([`crate::client_workspace::hold_free_name`]),
/// or the 409 that says there is none for `what`.
fn hold_new_name(base: &Path, names: Vec<String>, what: &str) -> Result<(String, crate::client_workspace::NameHold), Refused> {
    match crate::client_workspace::hold_free_name(base, names, |n| name_taken(base, n)) {
        Ok(Some(held)) => Ok(held),
        Ok(None) => Err(refuse(StatusCode::CONFLICT, ClientCode::Conflict, format!("no free workspace name for this {what}"))),
        Err(e) => Err(Box::new(internal(anyhow!(e)))),
    }
}

/// A new clone of the repo for `made`'s client, on a new branch of the bot's.
fn new_clone(state: &ClientServerState, base: &Path, (url, normalized): (&str, &str), made: &Made) -> Response {
    let (name, held) = match hold_new_name(base, crate::client_workspace::name_candidates(normalized), "repo") {
        Ok(found) => found,
        Err(r) => return *r,
    };
    if let Err(r) = place_clone(state, url, normalized, made, base, &name) {
        held.forget();
        return *r;
    }
    let from = match start_bot_branch(state, base, &name, made.git_ref, made.bot.slug) {
        Ok(from) => from,
        Err(r) => {
            forget_clone(base, &name);
            held.forget();
            return *r;
        }
    };
    created_from(state, &name, from)
}

/// Puts new clone `name` on a new branch of the bot's, at the commit the
/// clone checked out (the ref asked for, else the default branch). Only
/// `HEAD` moves: the files and the index are already that commit's.
/// Returns where it started.
fn start_bot_branch(state: &ClientServerState, base: &Path, name: &str, git_ref: Option<&str>, bot: &str) -> Result<String, Refused> {
    let Some(wc) = work_copy_for(state, &base.join(name)) else {
        return Err(Box::new(internal(anyhow!("darkmux holds no repository for {name}"))));
    };
    let from = git_ref
        .map(str::to_string)
        .or_else(|| crate::client_workspace::remote_default_of(&wc))
        .unwrap_or_else(|| "HEAD".to_string());
    let branch = new_bot_branch(bot)?;
    head_onto_new_branch(&wc, &branch).map_err(|e| branch_failed(&format!("starting {name} on {branch}: {e}")))?;
    Ok(from)
}

/// Why a workspace could not be put on a branch of its own (git's reason).
fn branch_failed(why: &str) -> Refused {
    refuse(StatusCode::UNPROCESSABLE_ENTITY, ClientCode::BranchFailed, why)
}

/// Moves `wc`'s `HEAD` onto new branch `branch`, made where `HEAD` is (with
/// whatever commits it holds). Only `HEAD` moves, never a file or the index.
/// On an unborn `HEAD` (an empty repo) there is no commit to make it at, so
/// `HEAD` just names it, and the first commit makes it.
fn head_onto_new_branch(wc: &WorkCopy, branch: &str) -> Result<(), String> {
    if git_line(wc, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_some() {
        git_quiet(wc, &["branch", branch])?;
    }
    git_quiet(wc, &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")])
}

/// A new `mux/<bot>-<hex>` branch name: every workspace darkmux makes for a
/// bot gets one of its own.
fn new_bot_branch(bot: &str) -> Result<String, Refused> {
    use crate::client_workspace as ws;
    let short = random_hex(ws::BOT_BRANCH_SUFFIX_LEN / 2).map_err(|e| Box::new(internal(e)))?;
    Ok(ws::bot_branch(bot, &short))
}

/// Takes back clone (or template project) `name`: its work tree, the
/// repository darkmux holds for it and every record beside it.
fn forget_clone(base: &Path, name: &str) {
    let _ = std::fs::remove_dir_all(base.join(name));
    let _ = std::fs::remove_dir_all(crate::client_git::clone_git_dir(base, name));
    crate::client_git::forget_git_dir(base, name);
    crate::client_workspace::forget_template_commit(base, name);
    crate::client_workspace::forget_owner(base, name);
    crate::client_workspace::forget_bot(base, name);
}

/// A new project from a template: cloned at `git_ref` beside its final
/// name, its history and remote dropped, and started over as a fresh repo
/// on `main` with one commit, so runs commit to it and nothing points back
/// at the template. Named for the bot, never reused.
fn template_workspace(state: &ClientServerState, url: &str, normalized: &str, made: &Made) -> Response {
    let git_ref = made.git_ref;
    let base = match (state.worktrees_base)() {
        Ok(b) => b,
        Err(e) => return internal(anyhow!(e)),
    };
    let (name, held) = match hold_new_name(&base, project_names(made.bot.slug), "project") {
        Ok(found) => found,
        Err(r) => return *r,
    };
    if let Err(r) = place_clone(state, url, normalized, made, &base, &name) {
        held.forget();
        return *r;
    }
    let from = match git_ref {
        Some(r) => format!("{normalized}@{r}"),
        None => normalized.to_string(),
    };
    // (#3460) The first commit is darkmux's own: its recipe is the one a
    // preview of this project trusts, recorded beside the workspace.
    let started = start_fresh(&base, &name, &format!("Start from {from}"))
        .and_then(|commit| crate::client_workspace::record_template_commit(&base, &name, &commit));
    if let Err(e) = started {
        forget_clone(&base, &name);
        held.forget();
        return internal(anyhow!("starting {name} from the template: {e}"));
    }
    created_from(state, &name, from)
}

/// The 201 for new workspace `name`, started from `from`.
fn created_from(state: &ClientServerState, name: &str, from: String) -> Response {
    match workspace_dir(state, name) {
        Ok(dir) => {
            let mut v = workspace_json(state, name, &dir, true);
            v.from = Some(from);
            (StatusCode::CREATED, Json(v)).into_response()
        }
        Err(r) => *r,
    }
}

/// The names a template project of bot `bot` may get: `<bot>`, or with a
/// number after it.
fn project_names(bot: &str) -> Vec<String> {
    std::iter::once(bot.to_string())
        .chain((2..100).map(|n| format!("{bot}-{n}")))
        .collect()
}

/// Whether workspace name `n` is taken under `base`: by a folder, or by a
/// clone still in progress (its `.partial`).
fn name_taken(base: &Path, n: &str) -> bool {
    std::fs::symlink_metadata(base.join(n)).is_ok()
        || std::fs::symlink_metadata(base.join(format!(".{n}.partial"))).is_ok()
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

/// A Mac with no git identity yet can still commit in a fresh project: it
/// gets its own, and the owner's takes over once there is one.
fn ensure_identity(dir: &WorkCopy) -> Result<(), String> {
    if git_line(dir, &["config", "user.email"]).unwrap_or_default().is_empty() {
        git_quiet(dir, &["config", "user.name", "darkmux"])?;
        git_quiet(dir, &["config", "user.email", "darkmux@localhost"])?;
    }
    Ok(())
}

/// Commits everything in the fresh project as its first commit; returns
/// its SHA.
fn first_commit(dir: &WorkCopy, message: &str) -> Result<String, String> {
    ensure_identity(dir)?;
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
    made: &Made,
    base: &Path,
    name: &str,
) -> Result<(), Refused> {
    use crate::client_git as cg;
    let git_ref = made.git_ref;
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
    cg::record_git_dir(base, name, &git_dir)
        .and_then(|()| crate::client_workspace::record_bot(base, name, made.bot.exact))
        .and_then(|()| crate::client_workspace::record_owner(base, name, made.client))
        .map_err(|e| Box::new(internal(anyhow!(e))))
}

fn worktree_workspace(state: &ClientServerState, source: &Path, made: &Made) -> Response {
    use crate::client_workspace as ws;
    let base = match (state.worktrees_base)() {
        Ok(b) => b,
        Err(e) => return internal(anyhow!(e)),
    };
    let (source, common) = match worktree_source(source, &base) {
        Ok(found) => found,
        Err(r) => return *r,
    };
    let _asked = state.workspace_asks.hold(ask_key("worktree", made, &common.to_string_lossy()));
    let found = reusable(state, &base, || ws::find_worktree_of(&base, &common, made.bot, made.client));
    reuse_or_make(
        &base,
        found,
        |name| claimed(&base, name, made, || reused_worktree(state, &base, name, &source)),
        || new_worktree(state, &base, (&source, &common), made),
    )
}

/// A new worktree workspace of `source` (its git common dir beside it) for
/// `made`'s client, recorded; taken back again if it cannot be recorded.
fn new_worktree(state: &ClientServerState, base: &Path, (source, common): (&Path, &Path), made: &Made) -> Response {
    let (name, held) = match hold_new_name(base, worktree_names(source, made.bot.slug), "checkout") {
        Ok(found) => found,
        Err(r) => return *r,
    };
    match add_bot_worktree(state, source, made.git_ref, made.bot.slug, &base.join(&name)) {
        Ok(added) => match record_worktree(base, &name, source, common, made) {
            Ok(()) => created_worktree(state, &name, added.0, added.1),
            Err(e) => {
                remove_worktree(source, &base.join(&name), &added.2);
                held.forget();
                internal(anyhow!(e))
            }
        },
        Err(r) => {
            held.forget();
            *r
        }
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
/// its repository (the worktree's admin directory under that checkout's
/// `.git/worktrees/`), read once now, before any run has been in it, the
/// bot's exact name and the paired client it is for.
fn record_worktree(base: &Path, name: &str, source: &Path, common: &Path, made: &Made) -> Result<(), String> {
    let admin = crate::client_git::worktree_admin_dir(&base.join(name), common)?;
    crate::client_git::record_git_dir(base, name, &admin)?;
    crate::client_workspace::record_source(base, name, source)?;
    crate::client_workspace::record_bot(base, name, made.bot.exact)?;
    crate::client_workspace::record_owner(base, name, made.client)
}

/// This bot's existing worktree of the checkout (one of this client's), its
/// checkout recorded again (the one previews read their recipe from).
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
    let branch = new_bot_branch(bot)?;
    // One fetch of a checkout at a time: two bots' worktrees of it are
    // made side by side, and git refuses a second fetch's ref updates.
    let fetching = state.workspace_asks.hold(format!("fetch\0{}", source.display()));
    let fetch_error = (state.fetch_repo)(source).err();
    drop(fetching);
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

/// The names a worktree of `source` for bot `bot` may get: `<repo>-<bot>`,
/// or with a number after it. The checkout's folder name is cut short
/// enough that every one of them, the numbered ones included, is a valid
/// name; the bot's (at most 32 characters) is kept whole.
fn worktree_names(source: &Path, bot: &str) -> Vec<String> {
    use crate::client_workspace as ws;
    let repo = ws::slug(
        &source
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
    );
    let room = ws::MAX_NAME_LEN.saturating_sub(ws::NAME_SUFFIX_LEN + 1 + bot.len());
    let repo: String = repo.chars().take(room).collect();
    let repo = repo.trim_end_matches('-');
    let stem = format!("{}-{bot}", if repo.is_empty() { "repo" } else { repo });
    std::iter::once(stem.clone())
        .chain((2..100).map(|n| format!("{stem}-{n}")))
        .collect()
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
    let client = match authenticate(&state, &headers) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let ask = match preview_start_body(body).await {
        Ok(ask) => ask,
        Err(r) => return r,
    };
    let st = state.clone();
    tokio::task::spawn_blocking(move || {
        let reply = start_preview(&st, &client, &name, ask.clean);
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

/// (#3112) Why a preview cannot start under `selected` (the configured
/// sandbox), or `None` under Docker. A preview runs in a Docker container
/// (`client_preview`) and has no other backend yet, so any other sandbox
/// refuses it, naming the setting that would allow it.
pub(crate) fn preview_sandbox_refusal(selected: &Result<darkmux_sandbox::BackendKind, String>) -> Option<String> {
    let docker = darkmux_sandbox::setting_value(darkmux_sandbox::BackendKind::Docker);
    match selected {
        Ok(darkmux_sandbox::BackendKind::Docker) => None,
        Ok(kind) => Some(format!(
            "previews need `{key}` `{docker}` for now: a preview runs in a Docker container, and this \
             machine's sandbox is `{}`. To run previews here: `darkmux config set {key} {docker}` \
             (Docker installed and running)",
            darkmux_sandbox::setting_value(*kind),
            key = darkmux_sandbox::SANDBOX_KEY,
        )),
        Err(why) => Some(format!("no preview can start: {why}")),
    }
}

/// (#3112) [`preview_sandbox_refusal`] for this machine's sandbox as
/// `config.json` says now, as a 403 `forbidden`.
fn previews_run_here(state: &ClientServerState) -> Result<(), Refused> {
    match preview_sandbox_refusal(&(state.preview_sandbox)()) {
        None => Ok(()),
        Some(why) => Err(refuse(StatusCode::FORBIDDEN, ClientCode::Forbidden, why)),
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
    optional_json_body(body).await
}

/// Starts workspace `name`'s preview from its trusted recipe; with `clean`,
/// first stops it and clears its install ([`crate::client_preview::Previews::clean`]).
fn start_preview(state: &ClientServerState, client: &ApprovedClient, name: &str, clean: bool) -> Response {
    let allowed = previews_allowed(state)
        .and_then(|()| owned_workspace(state, client, name).map(|_| ()))
        .and_then(|()| previews_run_here(state));
    if let Err(r) = allowed {
        return *r;
    }
    let (dir, source) = match workspace_and_recipe_source(state, name) {
        Ok(found) => found,
        Err(r) => return *r,
    };
    let held = match held_in_use(state, name) {
        Ok(held) => held,
        Err(r) => return *r,
    };
    let started = if clean { state.previews.clean(name, &dir, &source) } else { Ok(()) }
        .and_then(|()| state.previews.start_holding(name, &dir, &source, held));
    match started {
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
    let owned = authenticate(&state, &headers)
        .and_then(|client| previews_allowed(&state).map(|()| client))
        .and_then(|client| owned_workspace(&state, &client, &name));
    if let Err(r) = owned {
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

/// Starts the idle check's thread: every `interval` until `stop` is set,
/// one [`preview_idle_tick`] and one [`unpaired_tick`]. Between checks it
/// looks at the store's file (one `stat` each 200 ms) and runs
/// [`unpaired_tick`] as soon as that changed, so a `client revoke` ends the
/// client's work and previews within a moment, not at the next minute.
pub(crate) fn spawn_preview_idle_check(
    state: ClientServerState,
    interval: std::time::Duration,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    use std::sync::atomic::Ordering;
    std::thread::spawn(move || {
        let mut skipped = None;
        let mut stamp = store_stamp(&state.store);
        let poll = interval.min(std::time::Duration::from_millis(200));
        loop {
            let mut slept = std::time::Duration::ZERO;
            while slept < interval {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(poll);
                slept += poll;
                let now = store_stamp(&state.store);
                if now != stamp {
                    stamp = now;
                    log_lines(unpaired_tick(&state));
                }
            }
            log_lines(preview_idle_tick(&state, &mut skipped));
            log_lines(unpaired_tick(&state));
        }
    })
}

/// Writes serve-log lines to stderr.
fn log_lines(lines: Vec<String>) {
    for line in lines {
        eprintln!("{line}");
    }
}

pub(crate) async fn preview_stop_handler(
    State(state): State<ClientServerState>,
    ClientPath(name): ClientPath<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authenticate(&state, &headers).and_then(|c| owned_workspace(&state, &c, &name)) {
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
    let client = match authenticate(&state, &headers) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let st = state.clone();
    tokio::task::spawn_blocking(move || branch_changes(&st, &client, &name))
        .await
        .unwrap_or_else(|e| internal(anyhow!("changes panicked: {e}")))
}

/// `/changes`, blocking, for `client`'s workspace `name`.
fn branch_changes(st: &ClientServerState, client: &ApprovedClient, name: &str) -> Response {
    let dir = match owned_workspace(st, client, name) {
        Ok(d) => d,
        Err(r) => return *r,
    };
    let Some(dir) = work_copy_for(st, &dir) else {
        return err(StatusCode::CONFLICT, ClientCode::Conflict, "darkmux holds no repository for this workspace");
    };
    let (default, base_branch) = match changes_base(st, &dir, name) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let branch = git_line(&dir, &["branch", "--show-current"]).unwrap_or_default();
    if let Err(r) = own_branch(name, &branch, &default) {
        return *r;
    }
    let (Some(base), Some(head)) = (git_line(&dir, &["merge-base", "HEAD", &default]), git_line(&dir, &["rev-parse", "HEAD"])) else {
        return err(StatusCode::CONFLICT, ClientCode::Conflict, "couldn't find where this branch left the default branch");
    };
    let files = match crate::client_changes::branch_files(&dir, &base, &head, MAX_CHANGES_BYTES) {
        Ok(f) => f,
        Err(e) => return changes_refusal(e),
    };
    Json(BranchChanges {
        base,
        base_branch,
        head,
        branch,
        files,
    })
    .into_response()
}

/// The reply for a branch whose files could not be read.
fn changes_refusal(e: crate::client_changes::ChangesError) -> Response {
    use crate::client_changes::ChangesError;
    match e {
        ChangesError::Listing => internal(anyhow!("listing the branch's changes failed")),
        ChangesError::Unreadable(path) => internal(anyhow!("reading {path} failed")),
        ChangesError::TooLarge => {
            err(StatusCode::PAYLOAD_TOO_LARGE, ClientCode::PayloadTooLarge, "the branch's changes are too large to send")
        }
    }
}

/// The work copy must be on a branch, and (with an origin) on a `mux/`
/// branch of its own: a PR or backup from the default branch, or from the
/// ref an old clone was made at, would push its commits straight onto that
/// branch of the user's. A template project's base is its own first commit
/// (`default` is that commit, no `origin/` ref), so its `main` is its own.
fn own_branch(name: &str, branch: &str, default: &str) -> Result<(), Refused> {
    if branch.is_empty() {
        return Err(refuse(StatusCode::CONFLICT, ClientCode::Conflict, "the work copy isn't on a branch"));
    }
    if default.starts_with("origin/") && !is_own_branch(branch) {
        return Err(refuse(
            StatusCode::CONFLICT, ClientCode::NoOwnBranch,
            format!(
                "workspace {name} is on {branch}, not a branch of its own, so a PR or backup from it \
                 would push onto {branch}; the next run in it moves it onto one, its commits included"
            ),
        ));
    }
    Ok(())
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
    let client = match authenticate(&state, &headers) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let st = state.clone();
    tokio::task::spawn_blocking(move || match owned_workspace(&st, &client, &name) {
        Ok(dir) => Json(workspace_json(&st, &name, &dir, false)).into_response(),
        Err(r) => *r,
    })
    .await
    .unwrap_or_else(|e| internal(anyhow!("workspace status panicked: {e}")))
}

/// `DELETE /client/workspaces/{name}`: removes one of this client's
/// workspaces and what darkmux made for it
/// ([`crate::client_workspace::remove_workspace`]), its live preview
/// stopped first. Refused while a run or an Undo is in it. An optional body
/// `{"force": true}` also removes a `mux/` branch nothing else holds, and
/// a clone holding commits nothing else does.
pub(crate) async fn workspace_remove_handler(
    State(state): State<ClientServerState>,
    ClientPath(name): ClientPath<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let client = match authenticate(&state, &headers) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let ask: RemoveBody = match optional_json_body(body).await {
        Ok(ask) => ask,
        Err(r) => return r,
    };
    let st = state.clone();
    tokio::task::spawn_blocking(move || {
        let reply = remove_owned(&st, &client, &name, ask.force);
        eprintln!("darkmux client serve: workspace {name}: remove: {}", reply.status().as_u16());
        reply
    })
    .await
    .unwrap_or_else(|e| internal(anyhow!("workspace removal panicked: {e}")))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RemoveBody {
    /// Also remove what holds commits nothing else does.
    #[serde(default)]
    force: bool,
}

/// How long a removal waits, after stopping a preview, for that preview to
/// let go of the workspace (its `docker start -a` returning).
const PREVIEW_LETGO_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Removes workspace `name` for `client`, which must own it: refused while
/// one of its runs or an Undo is in it; its live preview stopped first.
fn remove_owned(state: &ClientServerState, client: &ApprovedClient, name: &str, force: bool) -> Response {
    use crate::client_workspace as ws;
    let dir = match owned_workspace(state, client, name) {
        Ok(dir) => dir,
        Err(r) => return *r,
    };
    if let Some(r) = busy_copy(&state.works.lock().unwrap(), &dir) {
        return *r;
    }
    let base = match (state.worktrees_base)() {
        Ok(b) => b,
        Err(e) => return internal(anyhow!(e)),
    };
    // Refused for what the workspace is before anything is stopped: a
    // refused removal leaves its preview running.
    if let Err(e) = ws::check_removable(&base, name, force) {
        return err(status_of_code(e.code()), e.code(), e.message);
    }
    let preview_stopped = state.previews.stop(name);
    let wait = if preview_stopped { PREVIEW_LETGO_WAIT } else { std::time::Duration::ZERO };
    match ws::remove_workspace_within(&base, name, force, wait) {
        Ok(r) => Json(WorkspaceRemoved {
            workspace: name.to_string(),
            kind: r.kind.word().to_string(),
            branch: r.branch,
            branch_removed: r.branch_removed,
            preview_stopped,
        })
        .into_response(),
        Err(e) => err(status_of_code(e.code()), e.code(), e.message),
    }
}

/// The status a workspace refusal's code answers with.
fn status_of_code(code: ClientCode) -> StatusCode {
    match code {
        ClientCode::BadRequest => StatusCode::BAD_REQUEST,
        ClientCode::NotFound => StatusCode::NOT_FOUND,
        ClientCode::Forbidden => StatusCode::FORBIDDEN,
        ClientCode::Conflict | ClientCode::UnsavedCommits => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// A body that may be absent (or empty): its default then; anything else
/// is parsed strictly, an unknown key refused with `bad_request`.
async fn optional_json_body<T: serde::de::DeserializeOwned + Default>(body: Body) -> Result<T, Response> {
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES).await.map_err(|_| {
        err(
            StatusCode::PAYLOAD_TOO_LARGE, ClientCode::PayloadTooLarge,
            format!("body larger than {MAX_BODY_BYTES} bytes"),
        )
    })?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| err(StatusCode::BAD_REQUEST, ClientCode::BadRequest, format!("bad JSON body: {e}")))
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
        Some(w) if w.client_id == client.id => {
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
/// bookend, `dispatch.model.*` or `dispatch.queue.*` record of a crew-of-one dispatch session
/// (`darkmux dispatch <role> --name <work_id>` names it `.adhoc.<role>.<work_id>`).
pub(crate) fn feeds_work_activity(v: &Value) -> bool {
    use darkmux_flow::FlowAction as A;
    let adhoc = v.get("session_id").and_then(Value::as_str).is_some_and(|s| s.contains(".adhoc."));
    adhoc
        && matches!(
            darkmux_flow::reader::action_of(v),
            Some(
                A::DispatchStart
                    | A::DispatchModelStart
                    | A::DispatchModelLoading
                    | A::DispatchModelReady
                    | A::DispatchQueueWait
                    | A::DispatchQueueResume
                    | A::DispatchComplete
                    | A::DispatchError
            )
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
    let left = left_behind(&w);
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
            left,
        },
        WorkStatus::Cancelled(e) => WorkState::Cancelled {
            reason: "cancelled".to_string(),
            error: e.map(|e| e.error),
            left,
        },
    }
}

/// What a failed or cancelled run left in its work copy, as its commit.
fn left_behind(w: &WorkEntry) -> LeftBehind {
    LeftBehind {
        commit: w.commit.as_deref().map(short),
        branch: w.branch.clone(),
        files: w.files.clone(),
        commit_error: w.commit_error.clone(),
        undone: w.decided.is_some(),
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
        Some(w) if w.client_id == client.id => match w.status {
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

// ─── Hand edits ──────────────────────────────────────────────────────────

/// The longest selection a hand edit replaces, and the longest replacement.
pub const MAX_EDIT_OLD_CHARS: usize = 2_000;
pub const MAX_EDIT_NEW_CHARS: usize = 10_000;
/// A tracked file larger than this is not searched (a bundle, a lockfile).
const MAX_EDIT_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditBody {
    /// The text as the page shows it (whitespace is collapsed for the match).
    old: String,
    /// The text the page is to show instead: written as sent, except that
    /// in a markup file what would read as markup is written as text
    /// ([`crate::client_edit::as_source`]).
    new: String,
}

/// `POST /client/workspaces/{name}/edit` (#3533): a person's own edit to
/// text they selected in the preview. darkmux finds the one place in the
/// workspace's tracked files that holds it ([`crate::client_edit`]),
/// replaces it, and commits the file as a work record with no model (role
/// `hand-edit`), so `/client/work/{id}/undo` reverts it like a run. Found
/// zero times or more than once: nothing changes, and the reply says how
/// many. Not a dispatch: no model runs, so no `dispatch.*` records.
pub(crate) async fn workspace_edit_handler(
    State(state): State<ClientServerState>,
    ClientPath(name): ClientPath<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let client = match authenticate(&state, &headers) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let req: EditBody = match json_body(body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let st = state.clone();
    tokio::task::spawn_blocking(move || hand_edit(&st, &client, &name, &req))
        .await
        .unwrap_or_else(|e| internal(anyhow!("edit panicked: {e}")))
}

/// A hand edit, blocking: checked, the work copy held, the file changed and
/// committed.
fn hand_edit(
    st: &ClientServerState,
    client: &ApprovedClient,
    name: &str,
    req: &EditBody,
) -> Response {
    match edit_checked(st, client, name, req) {
        Ok(reply) => Json(reply).into_response(),
        Err(r) => *r,
    }
}

/// [`hand_edit`] with every refusal an `Err`.
fn edit_checked(
    st: &ClientServerState,
    client: &ApprovedClient,
    name: &str,
    req: &EditBody,
) -> Result<EditReply, Refused> {
    let old = edit_input(req)?;
    let dir = owned_workspace(st, client, name)?;
    let wc = work_copy_for(st, &dir).ok_or_else(|| {
        refuse(
            StatusCode::CONFLICT,
            ClientCode::Conflict,
            "darkmux holds no repository for this workspace",
        )
    })?;
    // Held until the edit has committed, as a run holds it, so the
    // workspace cannot be removed under it, by this process or
    // `client workspace rm`.
    let _in_use = held_work_copy(st, Some(&wc))?;
    let work_id = format!("w-{}", random_hex(8).map_err(|e| Box::new(internal(e)))?);
    let held = hold_for_edit(st, client, &dir, &wc, &work_id, edit_title(&old, &req.new))?;
    let outcome = edit_in(st, &wc, &old, &req.new, &work_id);
    let landed = held.finish(outcome)?;
    Ok(EditReply {
        matches: 1,
        work_id: Some(landed.work_id),
        file: landed.files.into_iter().next(),
        commit: Some(landed.commit),
        branch: Some(landed.branch),
    })
}

/// What a hand edit or an added image committed: its work record, the files
/// (the first is the one it was for), the short commit, the branch.
struct Landed {
    work_id: String,
    files: Vec<String>,
    commit: String,
    branch: String,
}

/// A hand edit registered as running in its work copy ([`busy_copy`]), so
/// no run or Undo starts there while it edits. Dropped without
/// [`EditHold::finish`] (the edit panicked), its record ends as failed, as
/// a run's does when its runner panics, so the copy is never held by an
/// edit that has ended.
struct EditHold {
    works: Arc<Mutex<HashMap<String, WorkEntry>>>,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
    id: String,
    ended: bool,
}

impl EditHold {
    /// The edit's work record once [`edit_in`] ended: done with its commit,
    /// or gone when nothing was edited.
    fn finish(mut self, outcome: Result<EditDone, Refused>) -> Result<Landed, Refused> {
        let now = (self.now)();
        let mut works = self.works.lock().unwrap();
        self.ended = true;
        let done = match outcome {
            Ok(d) => d,
            Err(r) => {
                works.remove(&self.id);
                return Err(r);
            }
        };
        let reply = Landed {
            work_id: self.id.clone(),
            files: done.files.clone(),
            commit: short(&done.commit),
            branch: done.branch.clone(),
        };
        if let Some(e) = works.get_mut(&self.id) {
            record_edit(e, done, now);
        }
        Ok(reply)
    }
}

impl Drop for EditHold {
    fn drop(&mut self) {
        if self.ended {
            return;
        }
        let now = (self.now)();
        if let Some(e) = self.works.lock().unwrap().get_mut(&self.id) {
            e.status = WorkStatus::Error(RunError {
                reason: "panic".to_string(),
                error: "the hand edit panicked".to_string(),
                code: None,
            });
            e.finished = Some(now);
        }
    }
}

/// The old text as the page reads it, once the body is checked.
fn edit_input(req: &EditBody) -> Result<String, Refused> {
    let old = crate::client_edit::normalize(&req.old);
    if old.is_empty() || req.old.chars().count() > MAX_EDIT_OLD_CHARS {
        return Err(refuse(
            StatusCode::BAD_REQUEST,
            ClientCode::BadRequest,
            format!("`old` must hold text, at most {MAX_EDIT_OLD_CHARS} characters"),
        ));
    }
    if req.new.chars().count() > MAX_EDIT_NEW_CHARS || req.new.contains('\0') {
        return Err(refuse(
            StatusCode::BAD_REQUEST,
            ClientCode::BadRequest,
            format!("`new` must be text, at most {MAX_EDIT_NEW_CHARS} characters"),
        ));
    }
    Ok(old)
}

/// Registers the edit as running ([`EditHold`]), so no run or Undo starts
/// in the copy while it edits; refused when one is busy there now.
fn hold_for_edit(
    st: &ClientServerState,
    client: &ApprovedClient,
    dir: &Path,
    wc: &WorkCopy,
    work_id: &str,
    title: String,
) -> Result<EditHold, Refused> {
    let mut works = st.works.lock().unwrap();
    if st.previews.stopping() {
        return Err(Box::new(stopping_refusal()));
    }
    if let Some(r) = busy_copy(&works, &dir.to_path_buf()) {
        return Err(r);
    }
    works.insert(
        work_id.to_string(),
        WorkEntry {
            client_id: client.id.clone(),
            created: (st.now)(),
            finished: None,
            status: WorkStatus::Running,
            cancel: Arc::new(CancelFlag::default()),
            git: Some(wc.clone()),
            workdir: Some(dir.to_path_buf()),
            baseline: None,
            decided: None,
            undoing: false,
            continues: None,
            title,
            role: "hand-edit".to_string(),
            commit: None,
            files: Vec::new(),
            branch: None,
            commit_error: None,
        },
    );
    Ok(EditHold { works: st.works.clone(), now: st.now.clone(), id: work_id.to_string(), ended: false })
}

/// The edit's work record once it landed: done, with its commit.
fn record_edit(e: &mut WorkEntry, done: EditDone, now: u64) {
    e.status = WorkStatus::Done {
        result: done.result,
        changes: done.changes,
        changes_error: done.changes_error,
    };
    e.finished = Some(now);
    e.baseline = Some(done.baseline);
    e.files = done.files;
    e.commit = Some(done.commit);
    e.branch = Some(done.branch);
}

/// What a hand edit that landed did.
struct EditDone {
    /// The work record's `result`.
    result: serde_json::Value,
    files: Vec<String>,
    commit: String,
    branch: String,
    baseline: String,
    changes: Option<RunChanges>,
    changes_error: Option<String>,
}

/// One place `old` was found: the file, the span, and the file's text.
type EditHit = (String, (usize, usize), String);

/// Where `old` is in the work copy's tracked regular text files, counted up
/// to two (more than one is already too many).
fn find_in_tracked(wc: &WorkCopy, old: &str) -> Result<Vec<EditHit>, Refused> {
    let listing = crate::client_git::run_or_why(wc, None, &["ls-files", "-z"]).map_err(|e| {
        Box::new(internal(anyhow!(
            "listing the workspace's files failed: {e}"
        )))
    })?;
    let mut hits: Vec<EditHit> = Vec::new();
    let paths = String::from_utf8_lossy(&listing.stdout).into_owned();
    for path in paths.split('\0').filter(|p| !p.is_empty()) {
        let Some(text) = tracked_text(wc, path) else {
            continue;
        };
        for span in crate::client_edit::find(&text, old, 2 - hits.len()) {
            hits.push((path.to_string(), span, text.clone()));
        }
        if hits.len() >= 2 {
            break;
        }
    }
    Ok(hits)
}

/// A tracked file's text: a regular file of at most
/// [`MAX_EDIT_FILE_BYTES`], read as UTF-8, reached with no link followed at
/// any name on the way ([`crate::client_preview::at`]).
fn tracked_text(wc: &WorkCopy, path: &str) -> Option<String> {
    use std::io::Read;
    let (file, _) = crate::client_preview::at::open_regular(&wc.work_tree, path).ok()?;
    if file.metadata().ok()?.len() > MAX_EDIT_FILE_BYTES {
        return None;
    }
    let mut text = String::new();
    file.take(MAX_EDIT_FILE_BYTES + 1).read_to_string(&mut text).ok()?;
    (text.len() as u64 <= MAX_EDIT_FILE_BYTES).then_some(text)
}

/// Finds `old` once in the work copy's tracked files and writes `new` over
/// it, committed; else the reply to send (a miss is a 200 naming the
/// count, nothing changed).
fn edit_in(
    st: &ClientServerState,
    wc: &WorkCopy,
    old: &str,
    new: &str,
    work_id: &str,
) -> Result<EditDone, Refused> {
    let mut hits = find_in_tracked(wc, old)?;
    if hits.len() != 1 {
        return Err(Box::new(
            Json(EditReply {
                matches: hits.len() as u32,
                work_id: None,
                file: None,
                commit: None,
                branch: None,
            })
            .into_response(),
        ));
    }
    let (file, span, text) = hits.remove(0);
    refuse_unless_clean(wc, &file)?;
    onto_own_branch(st, Some(wc))?;
    let message = format!("hand-edit: {} (work {work_id})", edit_title(old, new));
    let new = crate::client_edit::as_source(&file, new);
    write_and_commit(wc, file, span, &text, &new, &message)
}

/// Refused unless `file` has no uncommitted change. A status git could not
/// read counts as one: an edit never lands on changes it could not see.
fn refuse_unless_clean(wc: &WorkCopy, file: &str) -> Result<(), Refused> {
    let status = crate::client_git::run_or_why(wc, None, &["--literal-pathspecs", "status", "--porcelain", "--", file]);
    let why = match status {
        Ok(out) if out.stdout.iter().all(u8::is_ascii_whitespace) => return Ok(()),
        Ok(_) => format!("{file} has changes that aren't committed, so darkmux won't edit it"),
        Err(e) => format!("darkmux couldn't read whether {file} has uncommitted changes ({e}), so it won't edit it"),
    };
    Err(refuse(StatusCode::CONFLICT, ClientCode::Conflict, why))
}

/// Writes `new` over `span` of `file`, through the file read as `text` and
/// never through a link ([`crate::client_preview::at::replace_regular`]),
/// and commits the file; a failed commit puts the file back.
fn write_and_commit(
    wc: &WorkCopy,
    file: String,
    (start, end): (usize, usize),
    text: &str,
    new: &str,
    message: &str,
) -> Result<EditDone, Refused> {
    let before = snapshot_tree(wc)
        .map_err(|e| Box::new(internal(anyhow!("reading the work copy failed: {e}"))))?;
    let edited = format!("{}{}{}", &text[..start], new, &text[end..]);
    crate::client_preview::at::replace_regular(&wc.work_tree, &file, text.as_bytes(), edited.as_bytes()).map_err(|e| {
        refuse(StatusCode::CONFLICT, ClientCode::Conflict, format!("darkmux didn't edit {file}: {e}"))
    })?;
    let (changes, changes_error) = match changes_since(wc, &before.tree) {
        Ok(c) => (Some(c), None),
        Err(e) => (None, Some(format!("reading the edit's changes failed: {e}"))),
    };
    #[cfg(test)]
    before_edit_commit(wc);
    let (commit, branch) = commit_run(wc, std::slice::from_ref(&file), message).map_err(|e| {
        // From HEAD: `add` may have staged the edit before the commit failed.
        let _ = git_quiet(wc, &["--literal-pathspecs", "checkout", "HEAD", "--", &file]);
        refuse(
            StatusCode::CONFLICT,
            ClientCode::Conflict,
            format!("couldn't commit the edit: {e}"),
        )
    })?;
    Ok(EditDone {
        result: serde_json::json!({"hand_edit": {"file": file}}),
        files: vec![file],
        commit,
        branch,
        baseline: before.tree,
        changes,
        changes_error,
    })
}

/// What a test runs inside a hand edit in a work tree, once the file is
/// written and before it is committed.
#[cfg(test)]
type EditHook = (PathBuf, Arc<dyn Fn() + Send + Sync>);
#[cfg(test)]
pub(crate) static EDIT_HOOKS: Mutex<Vec<EditHook>> = Mutex::new(Vec::new());

/// Runs the test hook for `wc`'s work tree, if one is set; outside the
/// lock, so a hook that panics leaves the list usable.
#[cfg(test)]
fn before_edit_commit(wc: &WorkCopy) {
    let hook = EDIT_HOOKS.lock().unwrap().iter().find(|(at, _)| *at == wc.work_tree).map(|(_, f)| f.clone());
    if let Some(f) = hook {
        f();
    }
}

/// A hand edit's title: the old words and the new, shortened.
fn edit_title(old: &str, new: &str) -> String {
    let cut = |s: &str| {
        let s = crate::client_edit::normalize(s);
        if s.chars().count() > 30 {
            format!("{}…", s.chars().take(30).collect::<String>())
        } else {
            s
        }
    };
    format!("\"{}\" to \"{}\"", cut(old), cut(new))
}

// ─── Added images (#3545) ────────────────────────────────────────────────

/// The largest body the image route reads: the largest image as base64,
/// plus room for the other fields. Only this route reads this much.
const MAX_IMAGE_BODY_BYTES: usize =
    crate::client_image::MAX_IMAGE_BYTES.div_ceil(3) * 4 + 64 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageBody {
    /// The picked file's name: only a hint for the name darkmux gives it.
    name: String,
    /// The image's bytes, base64.
    data: String,
    /// The image reference as the page reads it (an `<img>`'s `src`
    /// attribute): its one place in the tracked files is pointed at the new
    /// image, in the same commit.
    #[serde(default)]
    replace: Option<String>,
}

/// `POST /client/workspaces/{name}/image` (#3545): an image a person picked
/// on the device the app runs on, sent as bytes (never a path: darkmux
/// never reads a file a client names). darkmux checks what it is (PNG,
/// JPEG, GIF, WebP; [`crate::client_image::sniff`]), names it itself under
/// [`crate::client_image::IMAGE_FOLDER`], and commits it as a work record
/// with no model, like a hand edit, so `/client/work/{id}/undo` reverts it.
/// With `replace`, the one place that holds that reference now points at
/// the image; found zero times or more than once, nothing changes and the
/// reply says how many.
pub(crate) async fn workspace_image_handler(
    State(state): State<ClientServerState>,
    ClientPath(name): ClientPath<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let client = match authenticate(&state, &headers) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let bytes = match axum::body::to_bytes(body, MAX_IMAGE_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => {
            return err(
                StatusCode::PAYLOAD_TOO_LARGE,
                ClientCode::PayloadTooLarge,
                format!(
                    "an image is at most {} MB",
                    crate::client_image::MAX_IMAGE_BYTES / (1024 * 1024)
                ),
            )
        }
    };
    let req: ImageBody = match serde_json::from_slice(&bytes) {
        Ok(b) => b,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                ClientCode::BadRequest,
                format!("bad JSON body: {e}"),
            )
        }
    };
    let st = state.clone();
    tokio::task::spawn_blocking(move || match image_checked(&st, &client, &name, &req) {
        Ok(reply) => Json(reply).into_response(),
        Err(r) => *r,
    })
    .await
    .unwrap_or_else(|e| internal(anyhow!("adding an image panicked: {e}")))
}

/// The image's bytes and kind, and the reference to point at it, once the
/// body is checked.
fn image_input(req: &ImageBody) -> Result<(Vec<u8>, &'static str, Option<String>), Refused> {
    use crate::client_image::{decode_base64, sniff, MAX_IMAGE_BYTES};
    clean_text(&req.name, "name", 0, 255)?;
    let bytes = decode_base64(&req.data).ok_or_else(|| {
        refuse(
            StatusCode::BAD_REQUEST,
            ClientCode::BadRequest,
            "`data` must be base64",
        )
    })?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(refuse(
            StatusCode::PAYLOAD_TOO_LARGE,
            ClientCode::PayloadTooLarge,
            format!("an image is at most {} MB", MAX_IMAGE_BYTES / (1024 * 1024)),
        ));
    }
    let kind = sniff(&bytes).ok_or_else(|| {
        refuse(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ClientCode::UnsupportedMediaType,
            "`data` must be a PNG, JPEG, GIF or WebP image",
        )
    })?;
    let replace = match &req.replace {
        None => None,
        Some(r) => {
            let r = crate::client_edit::normalize(r);
            if r.is_empty() || r.chars().count() > MAX_EDIT_OLD_CHARS {
                return Err(refuse(
                    StatusCode::BAD_REQUEST,
                    ClientCode::BadRequest,
                    format!("`replace` must hold text, at most {MAX_EDIT_OLD_CHARS} characters"),
                ));
            }
            Some(r)
        }
    };
    Ok((bytes, kind, replace))
}

/// [`workspace_image_handler`], blocking, with every refusal an `Err`.
fn image_checked(
    st: &ClientServerState,
    client: &ApprovedClient,
    name: &str,
    req: &ImageBody,
) -> Result<ImageReply, Refused> {
    let (bytes, kind, replace) = image_input(req)?;
    let dir = owned_workspace(st, client, name)?;
    let wc = work_copy_for(st, &dir).ok_or_else(|| {
        refuse(
            StatusCode::CONFLICT,
            ClientCode::Conflict,
            "darkmux holds no repository for this workspace",
        )
    })?;
    let _in_use = held_work_copy(st, Some(&wc))?;
    let file_name = crate::client_image::file_name(&req.name, &bytes, kind);
    let image = Image {
        path: format!("{}/{file_name}", crate::client_image::IMAGE_FOLDER),
        url: format!("/images/{file_name}"),
        bytes,
    };
    let work_id = format!("w-{}", random_hex(8).map_err(|e| Box::new(internal(e)))?);
    let held = hold_for_edit(
        st,
        client,
        &dir,
        &wc,
        &work_id,
        format!("image {file_name}"),
    )?;
    let outcome = image_in(st, &wc, &image, replace.as_deref(), &work_id);
    let landed = held.finish(outcome)?;
    Ok(ImageReply {
        matches: replace.map(|_| 1),
        path: Some(image.path),
        url: Some(image.url),
        work_id: Some(landed.work_id),
        commit: Some(landed.commit),
        branch: Some(landed.branch),
    })
}

/// An image to add: where it goes, how the page reaches it, its bytes.
struct Image {
    path: String,
    url: String,
    bytes: Vec<u8>,
}

/// A reply sent with nothing committed: a `replace` found `matches` times
/// (not once), or the same image already there and nothing to point at it.
fn image_not_committed(matches: Option<u32>, image: Option<&Image>) -> Refused {
    Box::new(
        Json(ImageReply {
            matches,
            path: image.map(|i| i.path.clone()),
            url: image.map(|i| i.url.clone()),
            work_id: None,
            commit: None,
            branch: None,
        })
        .into_response(),
    )
}

fn conflict(why: String) -> Refused {
    refuse(StatusCode::CONFLICT, ClientCode::Conflict, why)
}

/// Writes the image (kept when the same bytes are already there), points
/// the one place holding `replace` at it, and commits both; a failed step
/// puts back what it changed.
fn image_in(
    st: &ClientServerState,
    wc: &WorkCopy,
    image: &Image,
    replace: Option<&str>,
    work_id: &str,
) -> Result<EditDone, Refused> {
    let hit = match replace {
        Some(old) => Some(one_reference(wc, old)?),
        None => None,
    };
    onto_own_branch(st, Some(wc))?;
    let already = image_already_there(wc, image)?;
    if already && hit.is_none() {
        return Err(image_not_committed(None, Some(image)));
    }
    let before = snapshot_tree(wc)
        .map_err(|e| Box::new(internal(anyhow!("reading the work copy failed: {e}"))))?;
    let files = place_image(wc, image, already, hit.as_ref())?;
    commit_image(wc, image, already, files, before.tree, work_id)
}

/// Commits the placed image (and the file pointed at it); a failed commit
/// puts back what was placed.
fn commit_image(
    wc: &WorkCopy,
    image: &Image,
    already: bool,
    files: Vec<String>,
    baseline: String,
    work_id: &str,
) -> Result<EditDone, Refused> {
    let (changes, changes_error) = match changes_since(wc, &baseline) {
        Ok(c) => (Some(c), None),
        Err(e) => (None, Some(format!("reading the image's changes failed: {e}"))),
    };
    let message = format!("image: {} (work {work_id})", image.path);
    let (commit, branch) = commit_run(wc, &files, &message).map_err(|e| {
        let _ = git_quiet(wc, &["--literal-pathspecs", "reset", "--quiet", "HEAD", "--", &image.path]);
        put_image_back(wc, image, already, files.get(1).map(String::as_str));
        conflict(format!("couldn't commit the image: {e}"))
    })?;
    Ok(EditDone {
        result: serde_json::json!({"image": {"path": image.path, "url": image.url}}),
        files,
        commit,
        branch,
        baseline,
        changes,
        changes_error,
    })
}

/// The one place `old` is in the tracked files, in a file with no
/// uncommitted change; else the reply naming how many places hold it.
fn one_reference(wc: &WorkCopy, old: &str) -> Result<EditHit, Refused> {
    let mut hits = find_in_tracked(wc, old)?;
    if hits.len() != 1 {
        return Err(image_not_committed(Some(hits.len() as u32), None));
    }
    let hit = hits.remove(0);
    refuse_unless_clean(wc, &hit.0)?;
    Ok(hit)
}

/// Whether the image is already at its path (the same bytes); refused when
/// a different file, or anything darkmux won't read, is there.
fn image_already_there(wc: &WorkCopy, image: &Image) -> Result<bool, Refused> {
    use std::io::Read;
    let didnt = |e: std::io::Error| conflict(format!("darkmux didn't add {}: {e}", image.path));
    let file = match crate::client_preview::at::open_regular(&wc.work_tree, &image.path) {
        Ok((file, _)) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(didnt(e)),
    };
    let mut held = Vec::new();
    file.take(image.bytes.len() as u64 + 1).read_to_end(&mut held).map_err(didnt)?;
    if held != image.bytes {
        return Err(conflict(format!("{} already holds a different file", image.path)));
    }
    Ok(true)
}

/// Writes the image (unless `already` there) and points `hit` at it: the
/// files to commit, image first. A failed step puts back what it changed.
fn place_image(wc: &WorkCopy, image: &Image, already: bool, hit: Option<&EditHit>) -> Result<Vec<String>, Refused> {
    use crate::client_preview::at;
    if !already {
        at::write_new_making_folders(&wc.work_tree, &image.path, &image.bytes, 0o644)
            .map_err(|e| conflict(format!("darkmux didn't add {}: {e}", image.path)))?;
    }
    let mut files = vec![image.path.clone()];
    if let Some((file, (start, end), text)) = hit {
        let edited = format!("{}{}{}", &text[..*start], image.url, &text[*end..]);
        if let Err(e) = at::replace_regular(&wc.work_tree, file, text.as_bytes(), edited.as_bytes()) {
            put_image_back(wc, image, already, None);
            return Err(conflict(format!("darkmux didn't edit {file}: {e}")));
        }
        files.push(file.clone());
    }
    Ok(files)
}

/// Takes out an image darkmux wrote (never one that was already there) and
/// puts `edited` back as committed.
fn put_image_back(wc: &WorkCopy, image: &Image, already: bool, edited: Option<&str>) {
    if !already {
        let _ = crate::client_preview::at::remove(&wc.work_tree, &image.path);
    }
    if let Some(f) = edited {
        let _ = git_quiet(wc, &["--literal-pathspecs", "checkout", "HEAD", "--", f]);
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
/// is that file, never a glob that also takes the owner's `b.txt`. A
/// submodule among them ([`submodule_paths`]) is left out. Returns (full id,
/// branch).
pub(crate) fn commit_run(
    wd: &WorkCopy,
    paths: &[String],
    message: &str,
) -> Result<(String, String), String> {
    let branch = git_line(wd, &["branch", "--show-current"]).unwrap_or_default();
    if branch.is_empty() {
        return Err("the work copy isn't on a branch, so the run wasn't committed".to_string());
    }
    let submodules = submodule_paths(wd, paths)?;
    let inside = |p: &str| submodules.iter().any(|s| p == s || p.strip_prefix(s.as_str()).is_some_and(|r| r.starts_with('/')));
    let paths: Vec<&str> = paths.iter().map(String::as_str).filter(|p| !inside(p)).collect();
    if paths.is_empty() {
        return Err(format!(
            "the run changed only submodules ({}), which darkmux does not commit",
            submodules.join(", ")
        ));
    }
    let mut add = vec!["--literal-pathspecs", "add", "-A", "--"];
    add.extend(paths.iter().copied());
    git_quiet(wd, &add)?;
    let mut commit = vec!["--literal-pathspecs", "commit", "--quiet", "-m", message, "--"];
    commit.extend(paths.iter().copied());
    git_quiet(wd, &commit)?;
    let sha = git_line(wd, &["rev-parse", "HEAD"]).ok_or("reading the new commit failed")?;
    Ok((sha, branch))
}

/// The submodules among a run's changed `paths`: each one the index records
/// as a gitlink, and each folder that holds a `.git` and is one of the paths
/// or contains one. `git add` on a populated submodule whose commit matches
/// the index spawns git INSIDE it to see whether its files changed; that git
/// reads the repository the run left there, filters and fsmonitor included,
/// and takes no `--ignore-submodules`. So a submodule is never handed to
/// `add`.
fn submodule_paths(wd: &WorkCopy, paths: &[String]) -> Result<Vec<String>, String> {
    let mut ls = vec!["--literal-pathspecs", "ls-files", "-s", "-z", "--"];
    ls.extend(paths.iter().map(String::as_str));
    let out = crate::client_git::run_or_why(wd, None, &ls)?;
    let mut found: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter_map(|e| e.strip_prefix("160000 ")?.split_once('\t').map(|(_, p)| p.to_string()))
        .collect();
    for p in paths {
        let mut prefix = PathBuf::new();
        for part in Path::new(p).components() {
            prefix.push(part);
            if std::fs::symlink_metadata(wd.work_tree.join(&prefix).join(".git")).is_ok() {
                found.push(prefix.to_string_lossy().into_owned());
                break;
            }
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
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
    let Some(w) = entry.filter(|w| w.client_id == client.id) else {
        return Err(Box::new(not_found()));
    };
    if matches!(w.status, WorkStatus::Running) {
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
/// same files, naming the run that made it, and while another run works in
/// the same work copy (its changes would be read against the revert).
/// 200 {revert, files}.
pub(crate) async fn undo_handler(
    State(state): State<ClientServerState>,
    ClientPath(work_id): ClientPath<String>,
    headers: HeaderMap,
) -> Response {
    let st = state.clone();
    tokio::task::spawn_blocking(move || undo_run(&st, &headers, &work_id))
        .await
        .unwrap_or_else(|e| internal(anyhow!("undo panicked: {e}")))
}

/// Undo, blocking: the run checked, its work copy held, the revert made.
fn undo_run(st: &ClientServerState, headers: &HeaderMap, work_id: &str) -> Response {
    let w = match committed_run(st, headers, work_id) {
        Ok(w) => w,
        Err(r) => return *r,
    };
    let (Some(wd), Some(commit)) = (w.git.clone(), w.commit.clone()) else {
        return err(StatusCode::CONFLICT, ClientCode::Conflict, "the run has no git work copy");
    };
    let _held = match UndoHold::take(&st.works, work_id).and_then(|undo| Ok((undo, held_work_copy(st, Some(&wd))?))) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    if let Some(by) = changed_later(st, &wd, &commit, &w.files) {
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
    if let Some(e) = st.works.lock().unwrap().get_mut(work_id) {
        e.decided = Some(format!("undone by {revert}"));
    }
    Json(UndoReply {
        revert: short(&revert),
        files: w.files,
    })
    .into_response()
}

/// What committed to `files` after `commit` and still stands (a run, by
/// id, else a commit), if anything: Undo waits for that to be undone first.
fn changed_later(st: &ClientServerState, wd: &WorkCopy, commit: &str, files: &[String]) -> Option<String> {
    let mut log = vec![
        "--literal-pathspecs".to_string(),
        "log".to_string(),
        "--format=%H".to_string(),
        format!("{commit}..HEAD"),
        "--".to_string(),
    ];
    log.extend(files.iter().cloned());
    let later = git(
        wd,
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
    let sha = later.into_iter().rfind(|sha| !settled.contains(sha))?;
    let by = st
        .works
        .lock()
        .unwrap()
        .iter()
        .find(|(_, e)| e.commit.as_deref() == Some(sha.as_str()))
        .map(|(id, _)| format!("run {id}"))
        .unwrap_or_else(|| format!("commit {}", short(&sha)));
    Some(by)
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
    // (#3505) A refused dispatch prints its refusal envelope: a failed run,
    // carrying the envelope's code.
    let mut refused_code = None;
    if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(stdout.trim()) {
        if v.get("result").and_then(Value::as_str) != Some("refused") {
            return Ok(v);
        }
        refused_code = v.get("code").and_then(Value::as_str).map(str::to_string);
    }
    let tail = stderr_tail(&stderr_bytes);
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    let mut e = failed(&exit_reason(status.code(), signal), tail);
    e.code = refused_code.or_else(|| teardown.dispatch_error_code(spec));
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
        if let Err(e) = write_cancelled(run, darkmux_flow::record) {
            eprintln!("darkmux client serve: {e}");
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
/// Builds `run`'s cancelled terminal and hands it to `sink` (the flow
/// stream, in production); why it could not, otherwise.
pub(crate) fn write_cancelled(
    run: &StartedRun,
    sink: impl FnOnce(darkmux_flow::FlowRecord) -> anyhow::Result<()>,
) -> Result<(), String> {
    let rec = cancelled_record(run)?;
    sink(rec).map_err(|e| format!("writing the cancelled terminal for {}: {e:#}", run.execution))
}

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
