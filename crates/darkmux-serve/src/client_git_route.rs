//! (#3681) `POST /client/workspaces/{name}/git`: plain git on a paired
//! app's workspace, from a short list of verbs, so an app never rebuilds
//! git itself (#3597).
//!
//! | verb | does | lock |
//! |---|---|---|
//! | `status` | branch, head, ahead/behind, dirty, what holds the copy: local refs only | no |
//! | `fetch` | the remote's branches into `refs/remotes/origin/*` | yes |
//! | `push` | `HEAD` to the same branch on the remote, fast-forward only | yes |
//! | `merge_main` | `origin/<default>` (as last fetched) into the branch | yes |
//! | `revert` | a new commit undoing one of the branch's own commits | yes |
//! | `diff` | the branch's files against its base or its upstream | no |
//!
//! Anything else is refused by name, and each verb takes a fixed shape of
//! arguments: no free-form git. Every local call goes through
//! [`crate::client_git`] (the repository darkmux holds, never one a run can
//! write); the network calls through `client_workspace::git_network`, the
//! one spawn that keeps the operator's credential helpers.
//!
//! **The lock.** A verb that changes the copy or its refs holds it
//! (`git:<verb>`, [`crate::client_pairing::take_copy`]) for its whole run,
//! with the workspace's name held shared, so `client workspace rm` waits.
//! While anything else holds it (a run, an Undo, a hand edit, a clean
//! preview start, another verb) the verb is refused: 409 `work_copy_busy`
//! naming the holder. Not covered: a `darkmux dispatch --workdir` someone
//! runs by hand in a workspace.
//!
//! **The token.** `fetch` and `push` take GitHub's token per call. It reaches
//! git only through `GIT_ASKPASS` on that one git process: this darkmux
//! binary, started by git in askpass mode ([`ASKPASS_ENV`]), answers
//! `x-access-token` and the token, and only for a prompt naming
//! `https://github.com`. With a token no credential helper runs at all, so
//! nothing stores it. It is never written to disk, config, the Keychain, the
//! flow stream or a log, and it is scrubbed from any git error darkmux
//! repeats ([`scrub`]). A token for a remote not on `github.com` is refused.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use crate::client_git::{self, WorkCopy};
use crate::client_pairing::{self as cp, ClientPath, ClientServerState};
use crate::client_wire::{ClientError, GitBusy, GitMerged, GitPushed, GitReply, GitReverted, GitStatus, GitUpstream};
use darkmux_types::client_code::ClientCode;

/// Set to `1` on a git child: this binary, started as its `GIT_ASKPASS`,
/// answers the prompt instead of running as darkmux.
pub const ASKPASS_ENV: &str = "DARKMUX_GIT_ASKPASS";
/// The token, on that one git child only.
const TOKEN_ENV: &str = "DARKMUX_GIT_TOKEN";
/// How long one fetch or push may take.
const NET_TIMEOUT: Duration = Duration::from_secs(300);

/// How the git route reaches a remote.
#[derive(Debug)]
pub struct GitNet {
    /// The program git runs as `GIT_ASKPASS`: this darkmux binary.
    askpass: PathBuf,
    /// Settings after the transport rules (tests allow a `file://` remote
    /// and point `https://github.com/` at one).
    extra: Mutex<Vec<String>>,
}

impl GitNet {
    pub fn production(exe: PathBuf) -> Self {
        GitNet { askpass: exe, extra: Mutex::new(Vec::new()) }
    }

    /// Tests: local `file://` remotes allowed.
    pub fn for_tests() -> Self {
        GitNet { askpass: PathBuf::from("/nonexistent/darkmux-askpass"), extra: Mutex::new(vec!["protocol.file.allow=always".to_string()]) }
    }

    /// Tests: more settings for every network call.
    pub fn add_setting(&self, setting: &str) {
        self.extra.lock().unwrap().push(setting.to_string());
    }

    fn extra(&self) -> Vec<String> {
        self.extra.lock().unwrap().iter().flat_map(|s| ["-c".to_string(), s.clone()]).collect()
    }
}

/// GitHub's token, as an app sends it: never shown (`Debug` redacts it),
/// never serialized.
#[derive(Deserialize, Clone)]
#[serde(transparent)]
pub struct GitToken(String);

impl std::fmt::Debug for GitToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GitToken(<redacted>)")
    }
}

/// The route's body: a verb and its arguments.
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GitBody {
    verb: GitVerb,
    /// The verb's arguments (`{}` or left out when it takes none).
    #[serde(default)]
    #[schemars(with = "Option<GitArgsDoc>")]
    args: serde_json::Value,
}

/// The verbs, by their one wire spelling. Any other is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GitVerb {
    Status,
    Fetch,
    Push,
    MergeMain,
    Revert,
    Diff,
}

impl GitVerb {
    fn word(self) -> &'static str {
        match self {
            GitVerb::Status => "status",
            GitVerb::Fetch => "fetch",
            GitVerb::Push => "push",
            GitVerb::MergeMain => "merge_main",
            GitVerb::Revert => "revert",
            GitVerb::Diff => "diff",
        }
    }
}

/// Every verb's arguments, for the schema only.
#[derive(schemars::JsonSchema)]
#[allow(dead_code)]
#[serde(untagged)]
enum GitArgsDoc {
    Fetch(FetchArgs),
    Push(PushArgs),
    Revert(RevertArgs),
    Diff(DiffArgs),
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

/// `fetch`: GitHub's token, for a private repository.
#[derive(Deserialize, schemars::JsonSchema, Default)]
#[serde(deny_unknown_fields)]
struct FetchArgs {
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    token: Option<GitToken>,
}

/// `push`: GitHub's token, and for a project made from a template, the
/// empty repository (`owner/name`) its first push goes to.
#[derive(Deserialize, schemars::JsonSchema, Default)]
#[serde(deny_unknown_fields)]
struct PushArgs {
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    token: Option<GitToken>,
    #[serde(default)]
    repo: Option<String>,
}

/// `revert`: one of the branch's own commits (7 to 40 hex digits).
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct RevertArgs {
    commit: String,
}

/// `diff`: against the branch's base (what `/changes` answers) or its
/// upstream (what the remote's branch lacks).
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct DiffArgs {
    against: Against,
}

#[derive(Deserialize, schemars::JsonSchema, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum Against {
    Base,
    Upstream,
}

/// The verb's arguments, read strictly: `null` (left out) is `{}`.
fn args_of<T: serde::de::DeserializeOwned>(verb: GitVerb, args: &serde_json::Value) -> Result<T, Refused> {
    let v = match args {
        serde_json::Value::Null => serde_json::json!({}),
        other => other.clone(),
    };
    serde_json::from_value(v).map_err(|e| {
        Box::new(cp::err(StatusCode::BAD_REQUEST, ClientCode::BadRequest, format!("`{}` arguments: {e}", verb.word())))
    })
}

/// A verb with its arguments read.
enum Asked {
    Status,
    Diff(Against),
    Fetch(FetchArgs),
    Push(PushArgs),
    MergeMain,
    Revert(String),
}

impl Asked {
    /// `req` read strictly, before anything is held, so a malformed call
    /// never holds the copy.
    fn of(req: &GitBody) -> Result<Asked, Refused> {
        let (verb, args) = (req.verb, &req.args);
        match verb {
            GitVerb::Status => args_of::<NoArgs>(verb, args).map(|_| Asked::Status),
            GitVerb::MergeMain => args_of::<NoArgs>(verb, args).map(|_| Asked::MergeMain),
            GitVerb::Diff => args_of::<DiffArgs>(verb, args).map(|a| Asked::Diff(a.against)),
            GitVerb::Fetch => args_of(verb, args).map(Asked::Fetch),
            GitVerb::Push => args_of(verb, args).map(Asked::Push),
            GitVerb::Revert => args_of::<RevertArgs>(verb, args).map(|a| Asked::Revert(a.commit)),
        }
    }
}

pub(crate) async fn git_handler(
    State(state): State<ClientServerState>,
    ClientPath(name): ClientPath<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let client = match cp::authenticate(&state, &headers) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let req: GitBody = match cp::json_body(body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let st = state.clone();
    tokio::task::spawn_blocking(move || match git_verb(&st, &client, &name, &req) {
        Ok(reply) => Json(reply).into_response(),
        Err(r) => *r,
    })
    .await
    .unwrap_or_else(|e| cp::internal(anyhow::anyhow!("git route panicked: {e}")))
}

type Refused = cp::Refused;

fn conflict(msg: impl Into<String>) -> Refused {
    Box::new(cp::err(StatusCode::CONFLICT, ClientCode::Conflict, msg))
}

fn coded(code: ClientCode, msg: impl Into<String>) -> Refused {
    let status = match code {
        ClientCode::BadRequest | ClientCode::TokenHostRefused => StatusCode::BAD_REQUEST,
        ClientCode::NotFound => StatusCode::NOT_FOUND,
        ClientCode::PushFailed | ClientCode::FetchFailed => StatusCode::BAD_GATEWAY,
        _ => StatusCode::CONFLICT,
    };
    Box::new(cp::err(status, code, msg))
}

fn internal(e: impl std::fmt::Display) -> Refused {
    Box::new(cp::internal(anyhow::anyhow!("{e}")))
}

/// One verb on `client`'s workspace `name`, blocking.
fn git_verb(st: &ClientServerState, client: &cp::ApprovedClient, name: &str, req: &GitBody) -> Result<GitReply, Refused> {
    let asked = Asked::of(req)?;
    let dir = cp::owned_workspace(st, client, name)?;
    let wc = cp::work_copy_for(st, &dir).ok_or_else(|| conflict("darkmux holds no repository for this workspace"))?;
    match asked {
        Asked::Status => Ok(GitReply::Status(status(st, name, &wc, &dir))),
        Asked::Diff(against) => diff(st, name, &wc, against).map(GitReply::Diff),
        changing => {
            let _name = cp::held_in_use(st, name)?;
            let _copy = cp::take_copy(st, &dir, &format!("git:{}", req.verb.word()))?;
            changed(st, name, &wc, &dir, changing)
        }
    }
}

/// A verb that changes the copy or its refs, run while it is held.
fn changed(st: &ClientServerState, name: &str, wc: &WorkCopy, dir: &Path, asked: Asked) -> Result<GitReply, Refused> {
    match asked {
        Asked::Fetch(a) => fetch(st, name, wc, dir, a).map(GitReply::Fetch),
        Asked::Push(a) => push(st, name, wc, a).map(GitReply::Push),
        Asked::MergeMain => merge_main(wc).map(GitReply::MergeMain),
        Asked::Revert(commit) => revert(st, name, wc, &commit).map(GitReply::Revert),
        Asked::Status | Asked::Diff(_) => unreachable!("read-only verbs are answered without the lock"),
    }
}

// ─── status ──────────────────────────────────────────────────────────────

fn line(wc: &WorkCopy, args: &[&str]) -> Option<String> {
    client_git::line(wc, args).filter(|l| !l.is_empty())
}

/// Commits in `range` (`a..b`).
fn count(wc: &WorkCopy, range: &str) -> Option<u64> {
    line(wc, &["rev-list", "--count", range]).and_then(|n| n.parse().ok())
}

/// The commit this branch left its main version at: the merge-base with
/// `origin/<default>`, or a template project's own first commit
/// (`changes_base`); with that main branch's name.
fn base_of(st: &ClientServerState, name: &str, wc: &WorkCopy) -> Option<(String, String)> {
    let (default, branch) = cp::changes_base(st, wc, name).ok()?;
    let base = match default.starts_with("origin/") {
        true => line(wc, &["merge-base", "HEAD", &default])?,
        false => default,
    };
    Some((base, branch))
}

/// The work copy as its local refs have it: no network.
pub(crate) fn status(st: &ClientServerState, name: &str, wc: &WorkCopy, dir: &Path) -> GitStatus {
    let branch = line(wc, &["branch", "--show-current"]).unwrap_or_default();
    let head = line(wc, &["rev-parse", "--verify", "--quiet", "HEAD"]);
    let origin = crate::client_workspace::origin_of(wc);
    let based = head.as_ref().and_then(|_| base_of(st, name, wc));
    let default_branch = cp::changes_base(st, wc, name).ok().map(|(_, b)| b);
    let pending = based.as_ref().and_then(|(base, _)| count(wc, &format!("{base}..HEAD"))).unwrap_or(0);
    let upstream = (!branch.is_empty())
        .then(|| line(wc, &["rev-parse", "--verify", "--quiet", &format!("refs/remotes/origin/{branch}")]))
        .flatten()
        .map(|up| GitUpstream { reference: format!("origin/{branch}"), head: up });
    let (behind, ahead) = match (&upstream, &head) {
        (Some(up), Some(_)) => line(wc, &["rev-list", "--left-right", "--count", &format!("{}...HEAD", up.head)])
            .and_then(|l| {
                let mut n = l.split_whitespace().filter_map(|x| x.parse::<u64>().ok());
                Some((n.next()?, n.next()?))
            })
            .map(|(b, a)| (Some(b), Some(a)))
            .unwrap_or((None, None)),
        _ => (None, None),
    };
    let behind_main = match (crate::client_workspace::remote_default_of(wc), &head) {
        (Some(d), Some(_)) => count(wc, &format!("HEAD..{d}")),
        _ => None,
    };
    let dirty = client_git::run(wc, None, &["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|o| !o.stdout.is_empty());
    let busy = cp::holder_of(&st.works.lock().unwrap(), &st.holds, dir).map(|(holder, work_id)| GitBusy { holder, work_id });
    GitStatus {
        branch,
        head,
        origin,
        default_branch,
        base: based.map(|(b, _)| b),
        pending,
        upstream,
        ahead,
        behind,
        behind_main,
        dirty,
        busy,
    }
}

// ─── The network ─────────────────────────────────────────────────────────

/// `token`'s text taken out of `text` (git may repeat a URL or a prompt).
pub(crate) fn scrub(text: &str, token: Option<&GitToken>) -> String {
    match token {
        Some(t) if !t.0.is_empty() => text.replace(&t.0, "<token>"),
        _ => text.to_string(),
    }
}

/// How [`net_git`] reaches the remote: with a token (only for a
/// `github.com` remote, through askpass, no helper, to its https URL), or
/// without (the operator's own helpers and ssh, to `origin` or the URL).
fn net_plan(wc: &WorkCopy, what: &str, token: Option<&GitToken>, remote: &str, to_url: bool) -> Result<(Vec<String>, bool), Refused> {
    use crate::client_workspace as ws;
    match token {
        Some(_) if !remote.starts_with("github.com/") => Err(coded(
            ClientCode::TokenHostRefused,
            format!("not {what}: {remote} is not on github.com, and darkmux hands a token only to GitHub"),
        )),
        Some(_) => Ok((ws::token_network_config(), true)),
        None => Ok((ws::operator_network_config(&wc.work_tree), to_url)),
    }
}

/// `git --git-dir=<held> <args>` over the network, where [`TARGET`] in
/// `args` becomes the remote ([`net_plan`]). Refused before git runs when
/// the repository's own config sets a network key no override switches off.
fn net_git(
    st: &ClientServerState,
    wc: &WorkCopy,
    what: &str,
    token: Option<&GitToken>,
    (remote, to_url): (&str, bool),
    args: &[&str],
) -> Result<crate::client_workspace::NetOutput, Refused> {
    use crate::client_workspace as ws;
    let refused = ws::refused_keys_of(wc);
    if !refused.is_empty() {
        return Err(coded(ClientCode::NetworkKeyInRepoConfig, ws::refused_keys_words(what, &refused)));
    }
    let (net, use_url) = net_plan(wc, what, token, remote, to_url)?;
    let url = format!("https://{remote}.git");
    let target = if use_url { url.as_str() } else { "origin" };
    let mut full: Vec<std::ffi::OsString> = vec![format!("--git-dir={}", wc.git_dir.display()).into()];
    full.extend(args.iter().map(|a| if *a == TARGET { target.into() } else { (*a).into() }));
    let full: Vec<&std::ffi::OsStr> = full.iter().map(|a| a.as_os_str()).collect();
    let extra_owned = st.git_net.extra();
    let extra: Vec<&str> = extra_owned.iter().map(String::as_str).collect();
    let askpass = st.git_net.askpass.clone();
    let env: Vec<(&str, &std::ffi::OsStr)> = match token {
        Some(t) => vec![("GIT_ASKPASS", askpass.as_os_str()), (ASKPASS_ENV, "1".as_ref()), (TOKEN_ENV, t.0.as_ref())],
        None => Vec::new(),
    };
    let failed = if what == "pushed" { ClientCode::PushFailed } else { ClientCode::FetchFailed };
    ws::git_network(&net, &extra, &full, &env, NET_TIMEOUT).map_err(|e| coded(failed, format!("not {what}: {}", scrub(&e, token))))
}

/// Where [`net_git`] puts the remote in its arguments.
const TARGET: &str = "\u{0}target";

/// git's error lines, scrubbed, for a person.
fn why(out: &crate::client_workspace::NetOutput, token: Option<&GitToken>) -> String {
    let lines: Vec<&str> = out.stderr.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let text = match lines.is_empty() {
        true => format!("git exited with {}", out.status),
        false => lines.join("; "),
    };
    scrub(&text, token)
}

/// [`net_git`] that must succeed; `code` when git fails.
fn net_ok(
    st: &ClientServerState,
    wc: &WorkCopy,
    (what, code): (&str, ClientCode),
    token: Option<&GitToken>,
    remote: (&str, bool),
    args: &[&str],
) -> Result<crate::client_workspace::NetOutput, Refused> {
    let out = net_git(st, wc, what, token, remote, args)?;
    match out.status.success() {
        true => Ok(out),
        false => Err(coded(code, format!("not {what}: {}", why(&out, token)))),
    }
}

fn fetch(st: &ClientServerState, name: &str, wc: &WorkCopy, dir: &Path, args: FetchArgs) -> Result<GitStatus, Refused> {
    let origin = crate::client_workspace::origin_of(wc)
        .ok_or_else(|| coded(ClientCode::NoOrigin, "this project has no remote yet: push it first"))?;
    let args_list = ["fetch", "--quiet", "--no-recurse-submodules", TARGET, "+refs/heads/*:refs/remotes/origin/*"];
    net_ok(st, wc, ("fetched", ClientCode::FetchFailed), args.token.as_ref(), (&origin, false), &args_list)?;
    Ok(status(st, name, wc, dir))
}

/// `owner/name` or a repo URL, as `github.com/owner/name`.
fn repo_named(repo: &str) -> Option<String> {
    let url = match repo.contains("://") || repo.starts_with("git@") {
        true => repo.to_string(),
        false => format!("https://github.com/{repo}"),
    };
    crate::client_workspace::normalize_repo_url(&url)
}

/// Where a push goes, and whether it is a template project's first: the
/// work copy's own remote (`repo`, when sent, must name it), or for a
/// project made from a template with no remote yet, `repo`.
fn push_remote(st: &ClientServerState, name: &str, wc: &WorkCopy, repo: Option<&str>) -> Result<(String, bool), Refused> {
    let asked = repo
        .map(|r| repo_named(r).ok_or_else(|| coded(ClientCode::BadRequest, format!("{r:?} is not a repository (owner/name)"))))
        .transpose()?;
    match (crate::client_workspace::origin_of(wc), asked) {
        (Some(o), Some(a)) if o != a => Err(coded(ClientCode::BadRequest, format!("this work copy's remote is {o}, not {a}"))),
        (Some(o), _) => Ok((o, false)),
        (None, Some(a)) if is_template_project(st, name)? => Ok((a, true)),
        (None, Some(_)) => Err(coded(ClientCode::NoOrigin, "this work copy has no remote, and it is no project darkmux made from a template")),
        (None, None) => Err(coded(ClientCode::NoOrigin, "this project has no remote yet: send `repo`, an empty repository to push it to")),
    }
}

fn is_template_project(st: &ClientServerState, name: &str) -> Result<bool, Refused> {
    let base = (st.worktrees_base)().map_err(internal)?;
    Ok(crate::client_workspace::template_commit_of(&base, name).is_some())
}

/// A template project shares no history with anything, so its first push
/// goes only into a repository with no branches or tags at all.
fn check_empty_remote(st: &ClientServerState, wc: &WorkCopy, token: Option<&GitToken>, remote: &str) -> Result<(), Refused> {
    let out = net_ok(st, wc, ("pushed", ClientCode::PushFailed), token, (remote, true), &["ls-remote", TARGET])?;
    match out.stdout.trim().is_empty() {
        true => Ok(()),
        false => Err(coded(
            ClientCode::RemoteNotEmpty,
            format!("not pushed: {remote} already has branches, and a new project goes only into an empty repository"),
        )),
    }
}

/// Why a push git refused failed: the remote's branch moved (its
/// `--porcelain` line says rejected), or something else.
fn push_refusal(out: &crate::client_workspace::NetOutput, token: Option<&GitToken>, branch: &str, remote: &str) -> Refused {
    let moved = |l: &str| ["non-fast-forward", "fetch first", "rejected"].iter().any(|w| l.contains(w));
    match out.stdout.lines().any(|l| l.starts_with('!') && moved(l)) {
        true => coded(
            ClientCode::RemoteMoved,
            format!("not pushed: {branch} on {remote} has commits this work copy doesn't; fetch and bring them in first"),
        ),
        false => coded(ClientCode::PushFailed, format!("not pushed: {}", why(out, token))),
    }
}

/// After a push: a template project's new `origin`, and the
/// remote-tracking ref a push to a URL does not move, so `status` on every
/// device sees what the remote now has.
fn record_push(wc: &WorkCopy, branch: &str, head: &str, remote: &str, first: bool) -> Result<(), Refused> {
    if first {
        client_git::quiet(wc, &["remote", "add", "origin", &format!("https://{remote}.git")])
            .map_err(|e| internal(format!("pushed, but adding the remote failed: {e}")))?;
    }
    let tracking = format!("refs/remotes/origin/{branch}");
    client_git::quiet(wc, &["update-ref", &tracking, head]).map_err(|e| internal(format!("pushed, but recording it failed: {e}")))?;
    if first {
        let _ = client_git::quiet(wc, &["symbolic-ref", "refs/remotes/origin/HEAD", &tracking]);
    }
    Ok(())
}

/// The branch a push sends, and its head.
fn push_head(wc: &WorkCopy) -> Result<(String, String), Refused> {
    let branch = line(wc, &["branch", "--show-current"]).ok_or_else(|| conflict("the work copy isn't on a branch"))?;
    let head = line(wc, &["rev-parse", "--verify", "--quiet", "HEAD"]).ok_or_else(|| conflict("there is nothing to push yet"))?;
    Ok((branch, head))
}

/// A template project's first push goes only into an empty repository;
/// any other push only from a branch of the work copy's own.
fn check_push(st: &ClientServerState, name: &str, wc: &WorkCopy, token: Option<&GitToken>, (remote, first): (&str, bool), branch: &str) -> Result<(), Refused> {
    match first {
        true => check_empty_remote(st, wc, token, remote),
        false => cp::own_branch(name, branch, &cp::changes_base(st, wc, name)?.0),
    }
}

fn push(st: &ClientServerState, name: &str, wc: &WorkCopy, args: PushArgs) -> Result<GitPushed, Refused> {
    let token = args.token.as_ref();
    let (branch, head) = push_head(wc)?;
    let (remote, first) = push_remote(st, name, wc, args.repo.as_deref())?;
    check_push(st, name, wc, token, (&remote, first), &branch)?;
    let refspec = format!("HEAD:refs/heads/{branch}");
    let out = net_git(st, wc, "pushed", token, (&remote, first), &["push", "--porcelain", TARGET, &refspec])?;
    if !out.status.success() {
        return Err(push_refusal(&out, token, &branch, &remote));
    }
    record_push(wc, &branch, &head, &remote, first)?;
    Ok(GitPushed { branch, head, remote, added_origin: first })
}

// ─── merge_main ──────────────────────────────────────────────────────────

fn is_dirty(wc: &WorkCopy) -> bool {
    client_git::run(wc, None, &["status", "--porcelain", "--untracked-files=no"]).is_some_and(|o| !o.stdout.is_empty())
}

/// The remote's main branch (`origin/<default>`) a merge may bring in, and
/// its tip: refused for a copy with no remote, no fetched main, no branch,
/// or uncommitted changes.
fn mergeable_main(wc: &WorkCopy) -> Result<(String, String), Refused> {
    if crate::client_workspace::origin_of(wc).is_none() {
        return Err(coded(ClientCode::NoOrigin, "this project has no remote, so there is no main version to bring in"));
    }
    let main = crate::client_workspace::remote_default_of(wc)
        .ok_or_else(|| coded(ClientCode::NoUpstream, "the remote's main branch hasn't been fetched yet: fetch first"))?;
    line(wc, &["branch", "--show-current"]).ok_or_else(|| conflict("the work copy isn't on a branch"))?;
    if is_dirty(wc) {
        return Err(coded(ClientCode::Dirty, "the work copy has uncommitted changes, so nothing was brought in"));
    }
    let tip = line(wc, &["rev-parse", &main]).ok_or_else(|| conflict(format!("couldn't read {main}")))?;
    Ok((main, tip))
}

/// A merge git stopped: aborted, so the tree is as it was, and refused
/// naming the conflicting files when there are any.
fn merge_refusal(wc: &WorkCopy, main: &str, e: &str) -> Refused {
    let paths: Vec<String> =
        line(wc, &["diff", "--name-only", "--diff-filter=U"]).map(|l| l.lines().map(str::to_string).collect()).unwrap_or_default();
    let _ = client_git::quiet(wc, &["merge", "--abort"]);
    if paths.is_empty() {
        return conflict(format!("couldn't bring in {main}: {e}"));
    }
    Box::new(cp::err_with(
        StatusCode::CONFLICT,
        ClientError {
            error: format!("bringing in {main} conflicts in {} file(s), so nothing changed", paths.len()),
            code: ClientCode::MergeConflict,
            holder: None,
            work_id: None,
            paths: Some(paths),
        },
    ))
}

fn merge_main(wc: &WorkCopy) -> Result<GitMerged, Refused> {
    let (main, tip) = mergeable_main(wc)?;
    let before = line(wc, &["rev-parse", "HEAD"]).ok_or_else(|| conflict("the work copy has no commits"))?;
    if client_git::run(wc, None, &["merge-base", "--is-ancestor", &tip, "HEAD"]).is_some() {
        return Ok(GitMerged { head: before, up_to_date: true, fast_forward: false });
    }
    cp::ensure_identity(wc).map_err(internal)?;
    client_git::run_or_why(wc, None, &["merge", "--no-edit", &main]).map_err(|e| merge_refusal(wc, &main, &e))?;
    let head = line(wc, &["rev-parse", "HEAD"]).unwrap_or_default();
    Ok(GitMerged { fast_forward: head == tip, head, up_to_date: false })
}

// ─── revert ──────────────────────────────────────────────────────────────

/// A commit id an app may name: 7 to 40 lowercase hex digits.
fn valid_commit(c: &str) -> bool {
    (7..=40).contains(&c.len()) && c.chars().all(|ch| ch.is_ascii_digit() || ('a'..='f').contains(&ch))
}

/// The commit id in a "This reverts commit <id>." line.
fn reverted_in(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("This reverts commit ")?;
    let id: String = rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    (id.len() >= 7).then_some(id)
}

/// Which commit each revert in `range` undid, as (revert, reverted).
fn reverts_in(wc: &WorkCopy, range: &str) -> Vec<(String, String)> {
    let Some(log) = client_git::run(wc, None, &["log", "--format=%H%x1f%B%x1e", range]) else { return Vec::new() };
    String::from_utf8_lossy(&log.stdout)
        .split('\u{1e}')
        .filter_map(|rec| {
            let (sha, body) = rec.trim_start().split_once('\u{1f}')?;
            Some((sha.trim().to_string(), body.lines().find_map(reverted_in)?))
        })
        .collect()
}

/// `commit` as a full id, when it is one of this branch's own non-merge
/// commits (first-parent, since it left the main version).
fn own_commit(st: &ClientServerState, name: &str, wc: &WorkCopy, commit: &str) -> Result<String, Refused> {
    if !valid_commit(commit) {
        return Err(coded(ClientCode::BadRequest, format!("{commit:?} is not a commit id")));
    }
    let full = line(wc, &["rev-parse", "--verify", "--quiet", &format!("{commit}^{{commit}}")])
        .ok_or_else(|| coded(ClientCode::NotFound, format!("no commit {commit} in this work copy")))?;
    let (base, _) = base_of(st, name, wc).ok_or_else(|| conflict("couldn't find where this branch left the main version"))?;
    let own = line(wc, &["rev-list", "--first-parent", &format!("{base}..HEAD")]).unwrap_or_default();
    if !own.lines().any(|l| l == full) {
        return Err(conflict(format!("commit {commit} isn't one of this branch's own changes")));
    }
    let parents = line(wc, &["rev-list", "--parents", "-n", "1", &full]).unwrap_or_default();
    match parents.split_whitespace().count() {
        2 => Ok(full),
        _ => Err(conflict(format!("commit {commit} merged other work in, so it can't be undone here"))),
    }
}

/// Whether `id` (possibly short) names `sha`.
fn names(id: &str, sha: &str) -> bool {
    sha.starts_with(id) || id.starts_with(sha)
}

/// Refused when `full` is already undone, or when a later commit still
/// standing changed one of its `files` (undo that first). A later change
/// that was itself undone leaves the files as they were: it doesn't count.
fn check_undoable(wc: &WorkCopy, full: &str, files: &[String]) -> Result<(), Refused> {
    let later = format!("{full}..HEAD");
    let reverts = reverts_in(wc, &later);
    if reverts.iter().any(|(_, x)| names(x, full)) {
        return Err(conflict("already undone"));
    }
    if files.is_empty() {
        return Ok(());
    }
    let settled = |sha: &str| reverts.iter().any(|(r, x)| r == sha || names(x, sha));
    let mut log = vec!["--literal-pathspecs", "log", "--format=%H", later.as_str(), "--"];
    log.extend(files.iter().map(String::as_str));
    let touching = line(wc, &log).unwrap_or_default();
    match touching.lines().rfind(|sha| !settled(sha)) {
        Some(by) => Err(conflict(format!("commit {} changed the same files later; undo that first", &by[..by.len().min(7)]))),
        None => Ok(()),
    }
}

fn revert(st: &ClientServerState, name: &str, wc: &WorkCopy, commit: &str) -> Result<GitReverted, Refused> {
    let full = own_commit(st, name, wc, commit)?;
    let files: Vec<String> = line(wc, &["diff-tree", "--no-commit-id", "--name-only", "--no-renames", "-r", &full])
        .map(|l| l.lines().map(str::to_string).collect())
        .unwrap_or_default();
    check_undoable(wc, &full, &files)?;
    cp::ensure_identity(wc).map_err(internal)?;
    if let Err(e) = client_git::quiet(wc, &["revert", "--no-edit", &full]) {
        let _ = client_git::quiet(wc, &["revert", "--abort"]);
        return Err(conflict(format!("couldn't undo it: {e}")));
    }
    let revert = line(wc, &["rev-parse", "HEAD"]).unwrap_or_default();
    // A run darkmux still knows whose commit this was reads as undone too.
    for e in st.works.lock().unwrap().values_mut().filter(|e| e.commit.as_deref() == Some(full.as_str())) {
        e.decided.get_or_insert_with(|| format!("undone by {revert}"));
    }
    Ok(GitReverted { commit: full, revert, files })
}

// ─── diff ────────────────────────────────────────────────────────────────

fn diff(st: &ClientServerState, name: &str, wc: &WorkCopy, against: Against) -> Result<crate::client_wire::BranchChanges, Refused> {
    match against {
        Against::Base => cp::branch_changes_of(st, name, wc),
        Against::Upstream => upstream_diff(wc),
    }
}

fn upstream_diff(wc: &WorkCopy) -> Result<crate::client_wire::BranchChanges, Refused> {
    let branch = line(wc, &["branch", "--show-current"]).ok_or_else(|| conflict("the work copy isn't on a branch"))?;
    let upstream = line(wc, &["rev-parse", "--verify", "--quiet", &format!("refs/remotes/origin/{branch}")])
        .ok_or_else(|| coded(ClientCode::NoUpstream, format!("{branch} has never been pushed or fetched")))?;
    let head = line(wc, &["rev-parse", "HEAD"]).ok_or_else(|| conflict("the work copy has no commits"))?;
    let files = crate::client_changes::branch_files(wc, &upstream, &head, cp::MAX_CHANGES_BYTES).map_err(|e| Box::new(cp::changes_refusal(e)))?;
    Ok(crate::client_wire::BranchChanges { base: upstream, base_branch: branch.clone(), head, branch, files })
}

// ─── askpass ─────────────────────────────────────────────────────────────

/// What darkmux, as git's askpass, answers `prompt` with: `x-access-token`
/// for GitHub's username prompt and `token` for its password prompt; for
/// any other host, nothing.
pub fn askpass_answer(prompt: &str, token: &str) -> Option<String> {
    let (kind, rest) = prompt.split_once(" for '")?;
    let url = rest.rsplit_once('\'')?.0;
    // Only an https prompt (read by hand: endpoints are resolved elsewhere,
    // and this is no endpoint, only a check of git's prompt).
    const HTTPS: &str = "https://";
    let host = url.starts_with(HTTPS).then(|| &url[HTTPS.len()..])?;
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let host = host.split('/').next()?;
    if !host.eq_ignore_ascii_case("github.com") {
        return None;
    }
    match kind.trim() {
        "Username" => Some("x-access-token".to_string()),
        "Password" if !token.is_empty() => Some(token.to_string()),
        _ => None,
    }
}

/// This binary as git's askpass ([`ASKPASS_ENV`] set): the answer on
/// stdout, or nothing and exit 1.
pub fn askpass_main() -> std::process::ExitCode {
    let prompt = std::env::args().nth(1).unwrap_or_default();
    let token = std::env::var(TOKEN_ENV).unwrap_or_default();
    match askpass_answer(&prompt, &token) {
        Some(answer) => {
            println!("{answer}");
            std::process::ExitCode::SUCCESS
        }
        None => std::process::ExitCode::FAILURE,
    }
}

#[cfg(test)]
#[path = "client_git_route_tests.rs"]
mod tests;
