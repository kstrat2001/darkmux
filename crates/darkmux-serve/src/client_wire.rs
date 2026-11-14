//! The response bodies of `darkmux client serve`: one named type per JSON
//! reply, the way `wire.rs` names the viewer daemon's.
//!
//! The client listener's routes and these shapes are a contract (DESIGN.md,
//! "Public surfaces"): `routes::client_table()` names each route's response
//! type through its `client_json!` macro (which fails to compile if the type
//! does not exist), and `route-table.golden` pins every route and every
//! field below, read from the type's JSON schema. A handler builds no `json!`
//! object by hand. The viewer imports none of these, so no TypeScript twin is
//! generated for them; the golden is their only record.
//!
//! Fields the producer could not read are `null`; an optional field marked
//! `skip_serializing_if` is absent instead.

use schemars::JsonSchema;
use serde::Serialize;

use crate::client_pairing::ClientMachine;
use crate::client_preview::Recipe;
use crate::client_profiles::ClientProfileInfo;

/// Every refusal: `{"error": "...", "code": "..."}`. The status says the
/// class, `code` which refusal it is (#3468): an app branches on `code`,
/// never on `error`'s wording, which is for a person.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ClientError {
    pub error: String,
    pub code: darkmux_types::client_code::ClientCode,
}

/// `POST /client/pair`, 202: the request to poll.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PairAccepted {
    pub request_id: String,
}

/// What a paired app may run: `profiles` is this machine's
/// `clients.profiles`, or the word `"local"` when it is unset (any profile
/// whose work stays on this machine). `workspace` is always true.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ClientScopes {
    pub profiles: ProfilesGrant,
    pub workspace: bool,
}

/// The granted list, or `"local"`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum ProfilesGrant {
    Listed(Vec<String>),
    /// Always the word `local`.
    Local(String),
}

impl ClientScopes {
    pub fn of(policy: &Option<Vec<String>>) -> Self {
        ClientScopes {
            profiles: match policy {
                Some(list) => ProfilesGrant::Listed(list.clone()),
                None => ProfilesGrant::Local("local".to_string()),
            },
            workspace: true,
        }
    }
}

/// `GET /client/pair/{id}`: `pending`, `denied` or `expired`, or, once,
/// `approved` with the token.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PairPoll {
    Pending,
    Denied,
    Expired,
    Approved {
        client_name: String,
        token: String,
        scopes: ClientScopes,
    },
}

/// `GET /client/whoami`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Whoami {
    pub client_name: String,
    pub scopes: ClientScopes,
    pub machine: ClientMachine,
}

/// `GET /client/profiles`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ClientProfiles {
    pub role: String,
    pub profiles: Vec<ClientProfileInfo>,
    pub machine: ClientMachine,
}

/// `POST /client/work`, 202: the run and the profile it resolved to.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkAccepted {
    pub work_id: String,
    pub profile: String,
}

/// One file a run changed: lines added and removed (`null` for a binary
/// file).
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct FileChange {
    pub path: String,
    pub added: Option<u64>,
    pub removed: Option<u64>,
}

/// What a run changed in its work copy, against its baseline: the files and
/// the unified diff (capped).
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct RunChanges {
    pub files: Vec<FileChange>,
    pub diff: String,
    /// Repositories the run made inside the work copy (`git init sub`),
    /// left out of the snapshot and so of the run's changes and commit.
    pub skipped: Vec<String>,
}

/// `GET /client/work/{id}`: where a run stands.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WorkState {
    Running {
        /// (#3467) `starting_model` while the run's dispatch is starting its
        /// model (an engine server, a load), and (#3504) `queued` while it
        /// waits for a busy endpoint or memory, as the run lifecycle decides
        /// it (the same `activity` `GET /runs` and `run list --json` carry).
        /// Absent otherwise.
        #[serde(skip_serializing_if = "Option::is_none")]
        activity: Option<crate::runs::RunActivity>,
        /// With `activity`: how long the start (or the wait) has taken so far, ms.
        #[serde(skip_serializing_if = "Option::is_none")]
        activity_elapsed_ms: Option<u64>,
    },
    Cancelling,
    Done {
        /// The dispatch's own `--json` envelope (that verb's contract).
        result: serde_json::Value,
        /// `null` without a git work copy.
        changes: Option<RunChanges>,
        /// The run's commit (short id), `null` when nothing was committed.
        commit: Option<String>,
        branch: Option<String>,
        commit_error: Option<String>,
        /// Why the run's changes could not be read (`changes` is then
        /// `null` and nothing was committed): never "no changes".
        changes_error: Option<String>,
        undone: bool,
    },
    Error {
        error: String,
        reason: String,
        /// (#3468) Why it failed, for an app: the code its run's
        /// `dispatch.error` named (`busy`, `wont_fit`, ...), else
        /// `dispatch_failed`.
        code: darkmux_types::client_code::ClientCode,
        #[serde(flatten)]
        left: LeftBehind,
    },
    Cancelled {
        /// Always `cancelled`.
        reason: String,
        error: Option<String>,
        #[serde(flatten)]
        left: LeftBehind,
    },
}

/// What a run that failed or was cancelled had already changed in its work
/// copy. It is committed as that run's own commit, so the next run's commit
/// never carries it and Undo can revert it. Every field is absent when the
/// run changed nothing.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct LeftBehind {
    /// The commit (short id) holding what the run left.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The files it holds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Why what the run left could not be read or committed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_error: Option<String>,
    /// Undone (its commit reverted).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub undone: bool,
}

/// `POST /client/work/{id}/cancel` (202) and `DELETE
/// /client/workspaces/{name}/preview`: what is happening now (`cancelling`,
/// `stopped`).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StatusWord {
    pub status: String,
}

/// `POST /client/work/{id}/undo`: the revert commit (short id) and the files
/// it put back.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct UndoReply {
    pub revert: String,
    pub files: Vec<String>,
}

/// `POST /client/workspaces/{name}/edit` (#3533): a hand edit. `matches`
/// is how many places in the workspace's tracked files hold the old text
/// (counted up to 2). Exactly one: the file changed and was committed as a
/// work record, which `/client/work/{work_id}` reports and `/undo` reverts.
/// Any other count: nothing changed and every other field is `null`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EditReply {
    pub matches: u32,
    pub work_id: Option<String>,
    pub file: Option<String>,
    pub commit: Option<String>,
    pub branch: Option<String>,
}

/// `POST /client/workspaces/{name}/image` (#3545): an image added to the
/// workspace. `matches` is set only when `replace` was sent: how many places
/// held that reference (counted up to 2). Committed (`work_id` set): the
/// image is at `path` (repo-relative), the page reaches it at `url`, and
/// `/undo` reverts it. Not committed: a `replace` found other than once
/// (every other field `null`), or the same image already there with nothing
/// to point at it (`path` and `url` set, no `work_id`).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImageReply {
    pub matches: Option<u32>,
    pub path: Option<String>,
    pub url: Option<String>,
    pub work_id: Option<String>,
    pub commit: Option<String>,
    pub branch: Option<String>,
}

/// `POST /client/work/{id}/keep` (older apps): the run's commit.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct KeepReply {
    pub commit: Option<String>,
    pub branch: Option<String>,
    pub files: Vec<String>,
}

/// `POST /client/work/{id}/discard` (older apps): an Undo, reported as the
/// files restored (`removed` is always empty: an Undo is a revert commit).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DiscardReply {
    pub restored: Vec<String>,
    pub removed: Vec<String>,
}

/// `GET /client/recipe`: the workspace's preview recipe, as read from the
/// user's own checkout.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RecipeReply {
    pub recipe: Recipe,
}

/// `POST /client/workspaces` and `GET /client/workspaces/{name}`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkspaceInfo {
    pub workspace: String,
    /// Empty when detached.
    pub branch: String,
    pub head: String,
    /// The normalized origin (`host/owner/repo`), `null` with none.
    pub repo: Option<String>,
    pub created: bool,
    /// Where a new workspace started (a ref, or `<repo>@<ref>` for a
    /// template). A clone or a worktree is on a new `mux/<bot>-<id>` branch
    /// cut from there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Why the fetch of a reused (or source) checkout failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetch_error: Option<String>,
    /// (#3468) With `fetch_error`: its code (`network_key_in_repo_config`,
    /// `fetch_failed`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetch_error_code: Option<darkmux_types::client_code::ClientCode>,
}

/// `DELETE /client/workspaces/{name}`: what removing the workspace did.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkspaceRemoved {
    pub workspace: String,
    /// `clone`, `template` or `worktree`.
    pub kind: String,
    /// The branch it was on, `null` when detached.
    pub branch: Option<String>,
    /// Whether that branch is gone: a clone's or a template project's goes
    /// with it; a worktree's `mux/` branch in the user's checkout only once
    /// merged into another branch, or with `force`; a branch of the user's
    /// never.
    pub branch_removed: bool,
    /// Whether its live preview was stopped first.
    pub preview_stopped: bool,
}

/// One file on the workspace's branch that origin's default branch lacks:
/// its content (base64) and mode, or `deleted: true`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BranchFile {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// `GET /client/workspaces/{name}/changes`: the branch's committed changes
/// against origin's default branch, for an app to open a PR itself.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BranchChanges {
    pub base: String,
    pub base_branch: String,
    pub head: String,
    pub branch: String,
    pub files: Vec<BranchFile>,
}

/// `POST` / `GET /client/workspaces/{name}/preview`: a preview's state.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct PreviewState {
    pub workspace: String,
    /// `installing`, `starting`, `ready`, `failed` or `stopped`.
    pub status: String,
    pub url: String,
    pub port: u16,
    pub hmr_port: u16,
    /// Why it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The end of its log, when it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_tail: Option<String>,
    /// (#3561) What a starting preview is doing that an app may show;
    /// absent the rest of the time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<PreviewStage>,
}

/// (#3561) A step of a preview's start that an app may show.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct PreviewStage {
    /// Which step, for an app to branch on: `getting_node` (darkmux is
    /// fetching and verifying the project's Node.js for a sandbox on this
    /// machine, first use of that version only).
    pub code: String,
    /// The same for a person: `getting Node 26.13.1, first use only`.
    pub detail: String,
}

impl PreviewStage {
    /// The stage of a first use's fetch of a runtime darkmux manages
    /// (`name` `node`, shown as `display` `Node`): `getting_node`.
    pub fn getting(name: &str, display: &str, version: &str) -> Self {
        PreviewStage {
            code: format!("getting_{name}"),
            detail: format!("getting {display} {version}, first use only"),
        }
    }
}
