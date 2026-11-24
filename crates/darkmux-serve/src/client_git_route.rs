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
fn args_of<T: serde::de::DeserializeOwned>(verb: GitVerb, args: &serde_json::Value) -> Result<T, Response> {
    let v = match args {
        serde_json::Value::Null => serde_json::json!({}),
        other => other.clone(),
    };
    serde_json::from_value(v).map_err(|e| {
        cp::err(StatusCode::BAD_REQUEST, ClientCode::BadRequest, format!("`{}` arguments: {e}", verb.word()))
    })
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
    tokio::task::spawn_blocking(move || match git_verb(&st, &client, &name, req) {
        Ok(reply) => Json(reply).into_response(),
        Err(r) => r,
    })
    .await
    .unwrap_or_else(|e| cp::internal(anyhow::anyhow!("git route panicked: {e}")))
}

fn conflict(msg: impl Into<String>) -> Response {
    cp::err(StatusCode::CONFLICT, ClientCode::Conflict, msg)
}

fn coded(code: ClientCode, msg: impl Into<String>) -> Response {
    let status = match code {
        ClientCode::BadRequest | ClientCode::TokenHostRefused => StatusCode::BAD_REQUEST,
        ClientCode::NotFound => StatusCode::NOT_FOUND,
        ClientCode::PushFailed | ClientCode::FetchFailed => StatusCode::BAD_GATEWAY,
        _ => StatusCode::CONFLICT,
    };
    cp::err(status, code, msg)
}

/// One verb on `client`'s workspace `name`, blocking.
fn git_verb(st: &ClientServerState, client: &cp::ApprovedClient, name: &str, req: GitBody) -> Result<GitReply, Response> {
    let dir = cp::owned_workspace(st, client, name).map_err(|r| *r)?;
    let wc = cp::work_copy_for(st, &dir).ok_or_else(|| conflict("darkmux holds no repository for this workspace"))?;
    let verb = req.verb;
    match verb {
        GitVerb::Status => {
            let _: NoArgs = args_of(verb, &req.args)?;
            return Ok(GitReply::Status(status(st, name, &wc, &dir)));
        }
        GitVerb::Diff => {
            let a: DiffArgs = args_of(verb, &req.args)?;
            return diff(st, name, &wc, a.against).map(GitReply::Diff);
        }
        _ => {}
    }
    // Arguments are read before the lock is taken, so a malformed call
    // never holds the copy.
    let (fetch_args, push_args, revert_args) = match verb {
        GitVerb::Fetch => (Some(args_of::<FetchArgs>(verb, &req.args)?), None, None),
        GitVerb::Push => (None, Some(args_of::<PushArgs>(verb, &req.args)?), None),
        GitVerb::Revert => (None, None, Some(args_of::<RevertArgs>(verb, &req.args)?)),
        _ => {
            let _: NoArgs = args_of(verb, &req.args)?;
            (None, None, None)
        }
    };
    let _name = cp::held_in_use(st, name).map_err(|r| *r)?;
    let _copy = cp::take_copy(st, &dir, &format!("git:{}", verb.word()))?;
    match verb {
        GitVerb::Fetch => fetch(st, name, &wc, &dir, fetch_args.unwrap_or_default()).map(GitReply::Fetch),
        GitVerb::Push => push(st, name, &wc, push_args.unwrap_or_default()).map(GitReply::Push),
        GitVerb::MergeMain => merge_main(&wc).map(GitReply::MergeMain),
        GitVerb::Revert => revert(st, name, &wc, &revert_args.expect("read above").commit).map(GitReply::Revert),
        GitVerb::Status | GitVerb::Diff => unreachable!("answered above"),
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

/// `git --git-dir=<held> <args>` over the network, where `target` (the
/// remote's https URL, or `origin`) is filled into `args`. With a token:
/// only for a `github.com` remote, the token through askpass, no helper.
/// Without: the operator's own helpers and ssh, as a workspace fetch has
/// them. Refused before git runs when the repository's own config sets a
/// network key no override switches off.
fn net_git(
    st: &ClientServerState,
    wc: &WorkCopy,
    what: &str,
    token: Option<&GitToken>,
    remote: &str,
    target_url: bool,
    args: &[&str],
) -> Result<crate::client_workspace::NetOutput, Response> {
    use crate::client_workspace as ws;
    let refused = ws::refused_keys_of(wc);
    if !refused.is_empty() {
        return Err(coded(ClientCode::NetworkKeyInRepoConfig, ws::refused_keys_words(what, &refused)));
    }
    let url = format!("https://{remote}.git");
    let (net, use_url) = match token {
        Some(_) => {
            if !remote.starts_with("github.com/") {
                return Err(coded(
                    ClientCode::TokenHostRefused,
                    format!("not {what}: {remote} is not on github.com, and darkmux hands a token only to GitHub"),
                ));
            }
            (ws::token_network_config(), true)
        }
        None => (ws::operator_network_config(&wc.work_tree), target_url),
    };
    let target = if use_url { url.as_str() } else { "origin" };
    let git_dir = format!("--git-dir={}", wc.git_dir.display());
    let mut full: Vec<std::ffi::OsString> = vec![git_dir.into()];
    full.extend(args.iter().map(|a| if *a == TARGET { target.into() } else { (*a).into() }));
    let full: Vec<&std::ffi::OsStr> = full.iter().map(|a| a.as_os_str()).collect();
    let extra_owned = st.git_net.extra();
    let extra: Vec<&str> = extra_owned.iter().map(String::as_str).collect();
    let askpass = st.git_net.askpass.clone();
    let env: Vec<(&str, &std::ffi::OsStr)> = match token {
        Some(t) => vec![
            ("GIT_ASKPASS", askpass.as_os_str()),
            (ASKPASS_ENV, "1".as_ref()),
            (TOKEN_ENV, t.0.as_ref()),
        ],
        None => Vec::new(),
    };
    ws::git_network(&net, &extra, &full, &env, NET_TIMEOUT).map_err(|e| coded(
        if what == "pushed" { ClientCode::PushFailed } else { ClientCode::FetchFailed },
        format!("not {what}: {}", scrub(&e, token)),
    ))
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

fn fetch(st: &ClientServerState, name: &str, wc: &WorkCopy, dir: &Path, args: FetchArgs) -> Result<GitStatus, Response> {
    let origin = crate::client_workspace::origin_of(wc)
        .ok_or_else(|| coded(ClientCode::NoOrigin, "this project has no remote yet: push it first"))?;
    let token = args.token.as_ref();
    let out = net_git(
        st,
        wc,
        "fetched",
        token,
        &origin,
        false,
        &["fetch", "--quiet", "--no-recurse-submodules", TARGET, "+refs/heads/*:refs/remotes/origin/*"],
    )?;
    if !out.status.success() {
        return Err(coded(ClientCode::FetchFailed, format!("not fetched: {}", why(&out, token))));
    }
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

fn push(st: &ClientServerState, name: &str, wc: &WorkCopy, args: PushArgs) -> Result<GitPushed, Response> {
    let token = args.token.as_ref();
    let branch = line(wc, &["branch", "--show-current"]).ok_or_else(|| conflict("the work copy isn't on a branch"))?;
    let head = line(wc, &["rev-parse", "--verify", "--quiet", "HEAD"]).ok_or_else(|| conflict("there is nothing to push yet"))?;
    let origin = crate::client_workspace::origin_of(wc);
    let asked = match &args.repo {
        Some(r) => Some(repo_named(r).ok_or_else(|| coded(ClientCode::BadRequest, format!("{r:?} is not a repository (owner/name)")))?),
        None => None,
    };
    let base = (st.worktrees_base)().map_err(|e| cp::internal(anyhow::anyhow!(e)))?;
    let (remote, first) = match (origin, asked) {
        (Some(o), Some(a)) if o != a => {
            return Err(coded(ClientCode::BadRequest, format!("this work copy's remote is {o}, not {a}")));
        }
        (Some(o), _) => (o, false),
        (None, Some(a)) if crate::client_workspace::template_commit_of(&base, name).is_some() => (a, true),
        (None, Some(_)) => return Err(coded(ClientCode::NoOrigin, "this work copy has no remote, and it is no project darkmux made from a template")),
        (None, None) => {
            return Err(coded(ClientCode::NoOrigin, "this project has no remote yet: send `repo`, an empty repository to push it to"));
        }
    };
    if first {
        // Only into an empty repository: a template project shares no
        // history with anything, so its branch would replace what is there.
        let out = net_git(st, wc, "pushed", token, &remote, true, &["ls-remote", TARGET])?;
        if !out.status.success() {
            return Err(coded(ClientCode::PushFailed, format!("not pushed: {}", why(&out, token))));
        }
        if !out.stdout.trim().is_empty() {
            return Err(coded(
                ClientCode::RemoteNotEmpty,
                format!("not pushed: {remote} already has branches, and a new project goes only into an empty repository"),
            ));
        }
    } else {
        let (default, _) = cp::changes_base(st, wc, name).map_err(|r| *r)?;
        cp::own_branch(name, &branch, &default).map_err(|r| *r)?;
    }
    let refspec = format!("HEAD:refs/heads/{branch}");
    let out = net_git(st, wc, "pushed", token, &remote, first, &["push", "--porcelain", TARGET, &refspec])?;
    if !out.status.success() {
        let rejected = out.stdout.lines().any(|l| l.starts_with('!') && (l.contains("non-fast-forward") || l.contains("fetch first") || l.contains("rejected")));
        return Err(match rejected {
            true => coded(
                ClientCode::RemoteMoved,
                format!("not pushed: {branch} on {remote} has commits this work copy doesn't; fetch and bring them in first"),
            ),
            false => coded(ClientCode::PushFailed, format!("not pushed: {}", why(&out, token))),
        });
    }
    if first {
        client_git::quiet(wc, &["remote", "add", "origin", &format!("https://{remote}.git")])
            .map_err(|e| cp::internal(anyhow::anyhow!("pushed, but adding the remote failed: {e}")))?;
    }
    // A push to a URL moves no remote-tracking ref: darkmux moves it, so
    // `status` on every device sees what the remote now has.
    client_git::quiet(wc, &["update-ref", &format!("refs/remotes/origin/{branch}"), &head])
        .map_err(|e| cp::internal(anyhow::anyhow!("pushed, but recording it failed: {e}")))?;
    if first {
        let _ = client_git::quiet(wc, &["symbolic-ref", "refs/remotes/origin/HEAD", &format!("refs/remotes/origin/{branch}")]);
    }
    Ok(GitPushed { branch, head, remote, added_origin: first })
}

// ─── merge_main ──────────────────────────────────────────────────────────

fn merge_main(wc: &WorkCopy) -> Result<GitMerged, Response> {
    if crate::client_workspace::origin_of(wc).is_none() {
        return Err(coded(ClientCode::NoOrigin, "this project has no remote, so there is no main version to bring in"));
    }
    let main = crate::client_workspace::remote_default_of(wc)
        .ok_or_else(|| coded(ClientCode::NoUpstream, "the remote's main branch hasn't been fetched yet: fetch first"))?;
    line(wc, &["branch", "--show-current"]).ok_or_else(|| conflict("the work copy isn't on a branch"))?;
    let dirty = client_git::run(wc, None, &["status", "--porcelain", "--untracked-files=no"]).is_some_and(|o| !o.stdout.is_empty());
    if dirty {
        return Err(coded(ClientCode::Dirty, "the work copy has uncommitted changes, so nothing was brought in"));
    }
    let before = line(wc, &["rev-parse", "HEAD"]).ok_or_else(|| conflict("the work copy has no commits"))?;
    let tip = line(wc, &["rev-parse", &main]).ok_or_else(|| conflict(format!("couldn't read {main}")))?;
    if client_git::run(wc, None, &["merge-base", "--is-ancestor", &tip, "HEAD"]).is_some() {
        return Ok(GitMerged { head: before, up_to_date: true, fast_forward: false });
    }
    cp::ensure_identity(wc).map_err(|e| cp::internal(anyhow::anyhow!(e)))?;
    if let Err(e) = client_git::run_or_why(wc, None, &["merge", "--no-edit", &main]) {
        let paths: Vec<String> = line(wc, &["diff", "--name-only", "--diff-filter=U"])
            .map(|l| l.lines().map(str::to_string).collect())
            .unwrap_or_default();
        let _ = client_git::quiet(wc, &["merge", "--abort"]);
        return Err(match paths.is_empty() {
            true => conflict(format!("couldn't bring in {main}: {e}")),
            false => cp::err_with(
                StatusCode::CONFLICT,
                ClientError {
                    error: format!("bringing in {main} conflicts in {} file(s), so nothing changed", paths.len()),
                    code: ClientCode::MergeConflict,
                    holder: None,
                    work_id: None,
                    paths: Some(paths),
                },
            ),
        });
    }
    let head = line(wc, &["rev-parse", "HEAD"]).unwrap_or_default();
    Ok(GitMerged { fast_forward: head == tip, head, up_to_date: false })
}

// ─── revert ──────────────────────────────────────────────────────────────

/// A commit id an app may name: 7 to 40 lowercase hex digits.
fn valid_commit(c: &str) -> bool {
    (7..=40).contains(&c.len()) && c.chars().all(|ch| ch.is_ascii_digit() || ('a'..='f').contains(&ch))
}

/// Which commit each "This reverts commit <id>." in `range` undid, as
/// (revert, reverted).
fn reverts_in(wc: &WorkCopy, range: &str) -> Vec<(String, String)> {
    let Some(log) = client_git::run(wc, None, &["log", "--format=%H%x1f%B%x1e", range]) else { return Vec::new() };
    String::from_utf8_lossy(&log.stdout)
        .split('\u{1e}')
        .filter_map(|rec| {
            let (sha, body) = rec.trim_start().split_once('\u{1f}')?;
            let reverted = body.lines().find_map(|l| {
                let rest = l.trim().strip_prefix("This reverts commit ")?;
                let id: String = rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
                (id.len() >= 7).then_some(id)
            })?;
            Some((sha.trim().to_string(), reverted))
        })
        .collect()
}

fn revert(st: &ClientServerState, name: &str, wc: &WorkCopy, commit: &str) -> Result<GitReverted, Response> {
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
    if parents.split_whitespace().count() != 2 {
        return Err(conflict(format!("commit {commit} merged other work in, so it can't be undone here")));
    }
    let files: Vec<String> = line(wc, &["diff-tree", "--no-commit-id", "--name-only", "--no-renames", "-r", &full])
        .map(|l| l.lines().map(str::to_string).collect())
        .unwrap_or_default();
    let later = format!("{full}..HEAD");
    let reverts = reverts_in(wc, &later);
    let undid = |r: &str, x: &str| x.starts_with(r) || r.starts_with(x);
    if reverts.iter().any(|(_, x)| undid(x, &full)) {
        return Err(conflict("already undone"));
    }
    // A later change to the same files that was itself undone leaves them
    // as they were: it doesn't count.
    let settled = |sha: &str| reverts.iter().any(|(r, x)| r == sha || undid(x, sha));
    if !files.is_empty() {
        let mut log = vec!["--literal-pathspecs", "log", "--format=%H", later.as_str(), "--"];
        log.extend(files.iter().map(String::as_str));
        let touching = line(wc, &log).unwrap_or_default();
        if let Some(by) = touching.lines().rfind(|sha| !settled(sha)) {
            return Err(conflict(format!("commit {} changed the same files later; undo that first", &by[..by.len().min(7)])));
        }
    }
    cp::ensure_identity(wc).map_err(|e| cp::internal(anyhow::anyhow!(e)))?;
    if let Err(e) = client_git::quiet(wc, &["revert", "--no-edit", &full]) {
        let _ = client_git::quiet(wc, &["revert", "--abort"]);
        return Err(conflict(format!("couldn't undo it: {e}")));
    }
    let revert = line(wc, &["rev-parse", "HEAD"]).unwrap_or_default();
    // A run darkmux still knows whose commit this was reads as undone too.
    for e in st.works.lock().unwrap().values_mut() {
        if e.commit.as_deref() == Some(full.as_str()) && e.decided.is_none() {
            e.decided = Some(format!("undone by {revert}"));
        }
    }
    Ok(GitReverted { commit: full, revert, files })
}

// ─── diff ────────────────────────────────────────────────────────────────

fn diff(st: &ClientServerState, name: &str, wc: &WorkCopy, against: Against) -> Result<crate::client_wire::BranchChanges, Response> {
    match against {
        Against::Base => cp::branch_changes_of(st, name, wc),
        Against::Upstream => {
            let branch = line(wc, &["branch", "--show-current"]).ok_or_else(|| conflict("the work copy isn't on a branch"))?;
            let upstream = line(wc, &["rev-parse", "--verify", "--quiet", &format!("refs/remotes/origin/{branch}")])
                .ok_or_else(|| coded(ClientCode::NoUpstream, format!("{branch} has never been pushed or fetched")))?;
            let head = line(wc, &["rev-parse", "HEAD"]).ok_or_else(|| conflict("the work copy has no commits"))?;
            let files = crate::client_changes::branch_files(wc, &upstream, &head, cp::MAX_CHANGES_BYTES).map_err(cp::changes_refusal)?;
            Ok(crate::client_wire::BranchChanges { base: upstream, base_branch: branch.clone(), head, branch, files })
        }
    }
}

// ─── askpass ─────────────────────────────────────────────────────────────

/// What darkmux, as git's askpass, answers `prompt` with: `x-access-token`
/// for GitHub's username prompt and `token` for its password prompt; for
/// any other host, nothing.
pub fn askpass_answer(prompt: &str, token: &str) -> Option<String> {
    let (kind, rest) = prompt.split_once(" for '")?;
    let url = rest.rsplit_once('\'')?.0;
    let host = url.strip_prefix("https://")?;
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
