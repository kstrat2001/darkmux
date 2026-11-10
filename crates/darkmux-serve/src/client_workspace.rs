//! Workspaces for paired apps: a repo checkout darkmux owns under the
//! worktrees base, named, never addressed by path from the app.
//!
//! `POST /client/workspaces {repo_url, ref?, bot?}` reuses the bot's clone
//! of that repo (fetching it), or clones it into a new one with the
//! machine's own git credentials, on a new `mux/<bot>-<id>` branch.
//! `GET /client/workspaces/{name}` reports it.
//! `/client/work` and `/client/recipe` take `workspace` in place of a raw
//! `workdir` and resolve it here, under the same containment check.
//!
//! What a client can name is narrow on purpose: an `https://` or `git@` URL of
//! the form `host/owner/repo` (no other transport, no options), a branch name
//! that can't be read as an option, and workspace names that are one plain
//! path segment.

use darkmux_types::client_code::ClientCode;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The longest workspace name.
pub const MAX_NAME_LEN: usize = 64;

/// The longest numbered fallback a name gets (`<stem>-99`).
pub const NAME_SUFFIX_LEN: usize = "-99".len();

/// Workspace names: one path segment, lowercase, no leading dot.
pub fn valid_name(name: &str) -> bool {
    let len_ok = (1..=MAX_NAME_LEN).contains(&name.len());
    let first_ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    len_ok
        && first_ok
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
        && !name.contains("..")
}

/// A repo URL a client may ask darkmux to clone: `https://host/owner/repo`
/// or `git@host:owner/repo`, optionally ending in `.git`. Returns the
/// normalized `host/owner/repo` (lowercase host, no `.git`).
pub fn normalize_repo_url(url: &str) -> Option<String> {
    let (host, path) = split_repo_url(url)?;
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let parts: Vec<&str> = path.split('/').collect();
    (valid_host(&host) && parts.len() == 2 && parts.iter().all(|p| valid_url_segment(p)))
        .then(|| format!("{host}/{}/{}", parts[0], parts[1]))
}

/// The lowercase host and the raw path of an `https://` or `git@` URL; any
/// other transport is `None`.
fn split_repo_url(url: &str) -> Option<(String, String)> {
    let (host, path) = if let Some(r) = url.strip_prefix("https://") {
        r.split_once('/')?
    } else {
        url.strip_prefix("git@")?.split_once(':')?
    };
    Some((host.to_ascii_lowercase(), path.to_string()))
}

/// A host name that can't be read as an option.
fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && !host.starts_with('-')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'))
}

/// One owner or repo segment: no dot paths, no leading dash.
fn valid_url_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A branch or tag a client may ask for: git's ref rules, minus anything
/// that could be read as an option.
pub fn valid_ref(r: &str) -> bool {
    (1..=200).contains(&r.len())
        && !r.starts_with('-')
        && !r.starts_with('/')
        && !r.ends_with('/')
        && !r.ends_with(".lock")
        && !r.contains("..")
        && !r.contains("//")
        && !r.contains("@{")
        && r.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

/// The name a new workspace gets: the repo's name, or `owner-repo` when the
/// repo's name is taken by another checkout, each cut short enough that a
/// numbered fallback still fits.
pub fn name_for(normalized: &str, taken: impl Fn(&str) -> bool) -> Option<String> {
    let mut parts = normalized.rsplit('/');
    let repo = parts.next()?.to_ascii_lowercase();
    let owner = parts.next()?.to_ascii_lowercase();
    let cut = |s: String| -> String { s.chars().take(MAX_NAME_LEN - NAME_SUFFIX_LEN).collect() };
    let both = cut(format!("{owner}-{repo}"));
    [cut(repo), both.clone()]
        .into_iter()
        .chain((2..100).map(|n| format!("{both}-{n}")))
        .find(|n| valid_name(n) && !taken(n))
}

fn git_out(wd: &Path, args: &[&str]) -> Option<String> {
    let out = darkmux_types::host_git::HostGit::trusted(wd)
        .command(args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The normalized origin of a workspace, read from the repository darkmux
/// holds for it, if it has one darkmux would accept.
pub fn origin_of(wc: &crate::client_git::WorkCopy) -> Option<String> {
    crate::client_git::line(wc, &["remote", "get-url", "origin"]).and_then(|u| normalize_repo_url(&u))
}

/// The normalized origin of the user's own checkout (its own config, which
/// no run writes).
pub fn checkout_origin(checkout: &Path) -> Option<String> {
    git_out(checkout, &["remote", "get-url", "origin"]).and_then(|u| normalize_repo_url(&u))
}

/// The workspaces under `base` that darkmux holds a repository for, by
/// name, sorted: plain directories only, never a symlink.
fn held_workspaces(base: &Path) -> Vec<(String, crate::client_git::WorkCopy)> {
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| valid_name(n))
        .collect();
    names.sort();
    names
        .into_iter()
        .filter_map(|n| crate::client_git::work_copy_of(base, &n, &base.join(&n)).map(|wc| (n, wc)))
        .collect()
}

/// Bot `bot`'s clone of the repo whose origin is `normalized`, among the
/// workspaces darkmux holds a repository for, that paired client `client`
/// may be handed ([`pick_for`]): one it owns, on one of this bot's branches
/// ([`is_bot_branch`]), and a clone, never a worktree of a checkout (whose
/// admin dir reads that checkout's `origin` too).
pub fn find_by_origin(base: &Path, normalized: &str, bot: &str, client: &str) -> Option<String> {
    let names = held_workspaces(base)
        .into_iter()
        .filter(|(_, wc)| {
            origin_of(wc).as_deref() == Some(normalized)
                && crate::client_git::common_of(&wc.git_dir).as_ref() == Some(&wc.git_dir)
                && status_of(wc).is_some_and(|(branch, _)| is_bot_branch(&branch, bot))
        })
        .map(|(n, _)| n)
        .collect();
    pick_for(base, names, client)
}

/// Branch (empty when detached) and short head of a workspace.
pub fn status_of(wc: &crate::client_git::WorkCopy) -> Option<(String, String)> {
    let head = crate::client_git::line(wc, &["rev-parse", "--short", "HEAD"])?;
    let branch = crate::client_git::line(wc, &["branch", "--show-current"]).unwrap_or_default();
    Some((branch, head))
}

/// Runs git with no terminal (credential prompts fail instead of hanging),
/// only the https and ssh transports, and a deadline. For clone and fetch
/// only: `darkmux_types::host_git::HostGit` clears the credential helper and
/// ssh command a network operation needs, so this is the one spawn in this
/// file its lint allows (#3457).
///
/// A fetch runs in the user's checkout (or a worktree of it), whose own
/// config a run may have written. So hooks and fsmonitor are off, and the
/// settings that name a program a network operation runs come from the
/// operator's GLOBAL config only ([`global_network_config`]): the
/// checkout's own `credential.helper`, `core.sshCommand` and `core.askPass`
/// are overridden. The network settings an override cannot switch off are
/// refused before git runs ([`refused_network_keys`]). What this does NOT
/// cover: a setting that names no program and that this list misses
/// (a fetch only moves remote-tracking refs, so the worst it can do is fetch
/// the wrong objects).
///
/// `extra` settings go after the transport rules (tests allow a local
/// `file://` remote this way).
fn git_bounded_with(
    config_dir: &Path,
    extra: &[&str],
    args: &[&std::ffi::OsStr],
    timeout: Duration,
) -> Result<(), String> {
    let child = std::process::Command::new("git")
        .args([
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "protocol.ssh.allow=always",
        ])
        .args(global_network_config(config_dir))
        .args(extra)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("running git: {e}"))?;
    wait_bounded(child, timeout)
}

/// The keys whose checkout-local values a network operation overrides with
/// the operator's global ones (lowercase, as git lists them).
const GLOBAL_ONLY_KEYS: [&str; 5] =
    ["credential.helper", "core.sshcommand", "core.askpass", "core.hookspath", "core.fsmonitor"];

/// `-c` settings for a network operation run for `dir` (where its config
/// is read; `/` before a clone exists): hooks and fsmonitor off, the
/// operator's own credential helpers ([`operator_credential_helpers`]), and
/// the ssh command and askpass program the operator's GLOBAL config names
/// (`ssh` and none when it names none).
fn global_network_config(dir: &Path) -> Vec<String> {
    let global = |key: &str| -> Vec<String> {
        darkmux_types::host_git::HostGit::trusted("/")
            .stdout(&["config", "--global", "--get-all", key])
            .map(|o| o.lines().map(str::to_string).collect())
            .unwrap_or_default()
    };
    let mut c: Vec<String> = ["core.hooksPath=/dev/null", "core.fsmonitor=false", "credential.helper="]
        .iter()
        .map(|s| s.to_string())
        .collect();
    c.extend(operator_credential_helpers(dir));
    c.push(format!("core.sshCommand={}", global("core.sshCommand").pop().unwrap_or_else(|| "ssh".to_string())));
    c.push(format!("core.askPass={}", global("core.askPass").pop().unwrap_or_default()));
    c.into_iter().flat_map(|s| ["-c".to_string(), s]).collect()
}

/// The config scopes whose credential helpers a network operation keeps:
/// the system config, the global one, and the one Apple's git reads from
/// Xcode (`unknown` scope), which holds `osxkeychain`. Never `local` or
/// `worktree` (the checkout's own config, which a run may have written) or
/// `command`.
const OPERATOR_SCOPES: [&str; 3] = ["system", "global", "unknown"];

/// The credential helpers the operator's own git would run, as `key=value`
/// settings for after the empty `credential.helper` that resets the list a
/// checkout's own config built. Generic and per-URL entries (`gh auth
/// setup-git` writes an empty reset and its own helper for its host) come
/// in the order git read them, so git's own URL matching builds, for any
/// URL, the list the operator's git would, minus the checkout's entries.
/// Read in `dir`, so a global `includeIf "gitdir:..."` applies as it does
/// for the operator; with the system config on, which other host git calls
/// leave off. An entry whose key holds `=` cannot be given as `-c`, and is
/// left out.
fn operator_credential_helpers(dir: &Path) -> Vec<String> {
    let out = darkmux_types::host_git::HostGit::trusted(dir)
        .command(&["config", "-z", "--show-scope", "--get-regexp", r"^credential\.(.+\.)?helper$"])
        .env_remove("GIT_CONFIG_NOSYSTEM")
        .stdin(std::process::Stdio::null())
        .output();
    let Some(out) = out.ok().filter(|o| o.status.success()) else {
        return Vec::new();
    };
    out.stdout
        .split(|b| *b == 0)
        .collect::<Vec<_>>()
        .chunks_exact(2)
        .filter(|p| OPERATOR_SCOPES.iter().any(|s| p[0] == s.as_bytes()))
        .filter_map(|p| {
            let entry = String::from_utf8_lossy(p[1]);
            let (key, value) = entry.split_once('\n').unwrap_or((&entry, ""));
            (!key.contains('=')).then(|| format!("{key}={value}"))
        })
        .collect()
}

/// The keys the repository at `dir` sets in its OWN config (local or
/// worktree scope, includes followed), lowercase as git lists them.
fn repo_local_keys(dir: &Path) -> Vec<String> {
    let Some(out) = darkmux_types::host_git::HostGit::trusted(dir)
        .output(&["config", "-z", "--show-scope", "--list"])
        .ok()
        .filter(|o| o.status.success())
    else {
        return Vec::new();
    };
    let fields: Vec<&[u8]> = out.stdout.split(|b| *b == 0).collect();
    fields
        .chunks_exact(2)
        .filter(|p| p[0] == b"local" || p[0] == b"worktree")
        .map(|p| String::from_utf8_lossy(p[1].split(|b| *b == b'\n').next().unwrap_or_default()).to_lowercase())
        .collect()
}

/// Of `keys`, the network settings no override can switch off, so a fetch
/// refuses them: a proxy or any other `http.*` setting (TLS verification,
/// CA file, extra headers), a URL rewrite, a per-URL credential helper, a
/// transport allowed or a remote helper named, a proxy command, the command
/// a remote runs to serve the fetch (over ssh, possibly this machine), and
/// the command a fetch runs to list an alternate object store's refs.
fn refused_network_keys(keys: &[String]) -> Vec<String> {
    keys.iter().filter(|k| is_refused_network_key(k)).cloned().collect()
}

/// (section prefix, key suffix) pairs [`refused_network_keys`] refuses; an
/// empty suffix is the whole section.
const REFUSED_NETWORK_KEYS: [(&str, &str); 9] = [
    ("http.", ""),
    ("protocol.", ""),
    ("url.", ".insteadof"),
    ("url.", ".pushinsteadof"),
    ("remote.", ".proxy"),
    ("remote.", ".vcs"),
    ("remote.", ".uploadpack"),
    ("core.gitproxy", ""),
    ("core.alternaterefscommand", ""),
];

fn is_refused_network_key(key: &str) -> bool {
    let per_url_helper = key.starts_with("credential.") && key.ends_with(".helper") && key != "credential.helper";
    per_url_helper || REFUSED_NETWORK_KEYS.iter().any(|(p, s)| key.starts_with(p) && key.ends_with(s))
}

/// How a checkout's own network setting is moved where darkmux reads it.
const NETWORK_FIX: &str = "darkmux reads network settings from your global git config only (a run that \
    had the checkout mounted could have written its own config): set it there, or for this one \
    repository in a `[includeIf \"gitdir:<checkout>/\"]` section of your global config";

/// Waits for git, killing it past `timeout`; a failure names git's last
/// stderr line.
fn wait_bounded(mut child: std::process::Child, timeout: Duration) -> Result<(), String> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                let last = last_stderr_line(&mut child);
                return Err(format!("git exited with {status}: {last}"));
            }
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("git took longer than {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("waiting for git: {e}")),
        }
    }
}

/// The last line git wrote to stderr, trimmed; empty when it wrote none.
fn last_stderr_line(child: &mut std::process::Child) -> String {
    let mut err = String::new();
    if let Some(mut s) = child.stderr.take() {
        use std::io::Read;
        let _ = s.read_to_string(&mut err);
    }
    err.lines().last().unwrap_or("").trim().to_string()
}

/// Production clone: `git clone --separate-git-dir <git_dir> [--branch
/// <ref>] -- <url> <dest>`: the repository lives in `git_dir`, outside the
/// work tree a run is given.
pub fn clone_with_git(url: &str, git_ref: Option<&str>, dest: &Path, git_dir: &Path) -> Result<(), String> {
    clone_at(url, git_ref, dest, git_dir, &[])
}

fn clone_at(url: &str, git_ref: Option<&str>, dest: &Path, git_dir: &Path, extra: &[&str]) -> Result<(), String> {
    // A commit SHA (what a pinned template sends) is no branch: clone
    // without checking out, then check out that commit, detached.
    let sha = git_ref.filter(|r| is_commit_sha(r));
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "clone".as_ref(),
        "--quiet".as_ref(),
        "--separate-git-dir".as_ref(),
        git_dir.as_os_str(),
    ];
    match (git_ref, sha) {
        (_, Some(_)) => args.push("--no-checkout".as_ref()),
        (Some(r), None) => {
            args.push("--branch".as_ref());
            args.push(r.as_ref());
        }
        (None, None) => {}
    }
    args.push("--".as_ref());
    args.push(url.as_ref());
    args.push(dest.as_os_str());
    git_bounded_with(Path::new("/"), extra, &args, Duration::from_secs(600))?;
    let Some(sha) = sha else { return Ok(()) };
    fn in_dest<'a>(dest: &'a Path, rest: &[&'a str]) -> Vec<&'a std::ffi::OsStr> {
        let mut a: Vec<&std::ffi::OsStr> = vec!["-C".as_ref(), dest.as_os_str()];
        a.extend(rest.iter().map(|s| std::ffi::OsStr::new(*s)));
        a
    }
    let object = format!("{sha}^{{commit}}");
    // A commit off every branch the clone fetched: ask the remote for it.
    if git_bounded_with(dest, extra, &in_dest(dest, &["cat-file", "-e", &object]), Duration::from_secs(30)).is_err() {
        let fetch = ["fetch", "--quiet", "--no-recurse-submodules", "origin", sha];
        git_bounded_with(dest, extra, &in_dest(dest, &fetch), Duration::from_secs(600))?;
    }
    git_bounded_with(dest, extra, &in_dest(dest, &["checkout", "--quiet", "--detach", sha]), Duration::from_secs(120))
}

/// A full object name (SHA-1 or SHA-256, lowercase hex): a ref that names
/// one commit, which `git clone --branch` cannot take. A short one is
/// ambiguous and could be a branch, so it stays a branch or tag name.
pub fn is_commit_sha(r: &str) -> bool {
    matches!(r.len(), 40 | 64) && r.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// (#3468) A refusal, with the code an app branches on beside the sentence
/// for a person. Shown as its sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coded {
    pub code: ClientCode,
    pub message: String,
}

impl Coded {
    pub fn new(code: ClientCode, message: impl Into<String>) -> Self {
        Coded { code, message: message.into() }
    }
    pub fn code(&self) -> ClientCode {
        self.code
    }
}

impl std::fmt::Display for Coded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Production fetch: updates remote-tracking refs only; the checkout's
/// branch and files are never touched, and its submodules are never
/// fetched (a child fetch there would read the submodule's own config,
/// which the key check below never sees). A network key the checkout's own
/// config sets is refused (`network_key_in_repo_config`); any other failure
/// is `fetch_failed`.
pub fn fetch_with_git(dir: &Path) -> Result<(), Coded> {
    fetch_at(dir, &[])
}

/// [`fetch_with_git`] with `extra` settings after the transport rules (tests
/// allow a local `file://` remote this way).
fn fetch_at(dir: &Path, extra: &[&str]) -> Result<(), Coded> {
    let local = repo_local_keys(dir);
    let mut refused = refused_network_keys(&local);
    refused.dedup();
    if !refused.is_empty() {
        return Err(Coded::new(
            ClientCode::NetworkKeyInRepoConfig,
            format!("not fetched: {}'s own git config sets {}. {NETWORK_FIX}.", dir.display(), refused.join(", ")),
        ));
    }
    let mut ignored: Vec<&str> = GLOBAL_ONLY_KEYS.iter().copied().filter(|k| local.iter().any(|l| l == k)).collect();
    ignored.dedup();
    let args: Vec<&std::ffi::OsStr> = vec![
        "-C".as_ref(),
        dir.as_os_str(),
        "fetch".as_ref(),
        "--quiet".as_ref(),
        "--no-recurse-submodules".as_ref(),
        "origin".as_ref(),
    ];
    git_bounded_with(dir, extra, &args, Duration::from_secs(120)).map_err(|e| {
        Coded::new(
            ClientCode::FetchFailed,
            match ignored.is_empty() {
                true => e,
                false => format!(
                    "{e} (darkmux ignored {} from {}'s own git config: {NETWORK_FIX})",
                    ignored.iter().map(|k| spelled(k)).collect::<Vec<_>>().join(", "),
                    dir.display()
                ),
            },
        )
    })
}

/// A [`GLOBAL_ONLY_KEYS`] key as git's documentation spells it.
fn spelled(key: &str) -> &str {
    match key {
        "core.sshcommand" => "core.sshCommand",
        "core.askpass" => "core.askPass",
        "core.hookspath" => "core.hooksPath",
        other => other,
    }
}

/// A label safe inside workspace and branch names: lowercase letters,
/// digits and single dashes, at most 32 characters.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out: String = out.trim_matches('-').chars().take(32).collect();
    out.trim_end_matches('-').to_string()
}

/// A local checkout a client may start a workspace from: an absolute path
/// to an existing directory, not a symlink, owned by the user darkmux runs
/// as, outside the worktrees base, and the top of a git work tree. Returns
/// the canonical top-level path.
pub fn check_source(path: &Path, canon_base: &Path) -> Result<PathBuf, String> {
    check_plain_dir(path)?;
    let canon = path
        .canonicalize()
        .map_err(|e| format!("resolving {}: {e}", path.display()))?;
    if canon.starts_with(canon_base) {
        return Err("source_path is already inside the worktrees base".into());
    }
    check_owned_by_me(&canon)?;
    check_git_top(&canon)?;
    Ok(canon)
}

/// An absolute path to a directory that exists and is not a symlink.
fn check_plain_dir(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("source_path must be absolute".into());
    }
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            Err(format!("{} is a symlink; refusing", path.display()))
        }
        Ok(m) if !m.is_dir() => Err(format!("{} is not a folder", path.display())),
        Ok(_) => Ok(()),
        Err(_) => Err(format!("{} does not exist", path.display())),
    }
}

/// Owned by the user darkmux runs as (unix; elsewhere there is no owner to
/// compare).
fn check_owned_by_me(canon: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let me = unsafe { libc::geteuid() };
        let owner = std::fs::metadata(canon).map_err(|e| e.to_string())?.uid();
        if owner != me {
            return Err(format!("{} is not owned by you", canon.display()));
        }
    }
    #[cfg(not(unix))]
    let _ = canon;
    Ok(())
}

/// `canon` is the top folder of a git work tree, not a folder inside one.
fn check_git_top(canon: &Path) -> Result<(), String> {
    let top = git_out(canon, &["rev-parse", "--show-toplevel"])
        .ok_or_else(|| format!("{} is not a git repository", canon.display()))?;
    let top = PathBuf::from(top)
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if top != canon {
        return Err(format!(
            "{} is inside a git repository; pick its top folder, {}",
            canon.display(),
            top.display()
        ));
    }
    Ok(())
}

/// The shared `.git` directory of a checkout or any of its worktrees.
pub fn common_dir(dir: &Path) -> Option<PathBuf> {
    git_out(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .and_then(|d| PathBuf::from(d).canonicalize().ok())
}

/// The workspace under `base` that is a worktree of the repo whose shared
/// `.git` is `common`, on one of this bot's branches ([`is_bot_branch`]),
/// judged by the repository darkmux recorded for it (never its own `.git`),
/// that paired client `client` may be handed ([`pick_for`]).
pub fn find_worktree_of(base: &Path, common: &Path, bot: &str, client: &str) -> Option<String> {
    let names = held_workspaces(base)
        .into_iter()
        .filter(|(_, wc)| {
            crate::client_git::common_of(&wc.git_dir).as_deref() == Some(common)
                && status_of(wc).is_some_and(|(branch, _)| is_bot_branch(&branch, bot))
        })
        .map(|(n, _)| n)
        .collect();
    pick_for(base, names, client)
}

/// How many hex digits follow `mux/<bot>-` in a bot's branch name.
pub const BOT_BRANCH_SUFFIX_LEN: usize = 6;

/// A new branch of bot `bot`'s: `mux/<bot>-<suffix>`, `suffix` being
/// [`BOT_BRANCH_SUFFIX_LEN`] random hex digits.
pub fn bot_branch(bot: &str, suffix: &str) -> String {
    format!("mux/{bot}-{suffix}")
}

/// Whether `branch` is one of bot `bot`'s ([`bot_branch`]): its prefix and
/// exactly a suffix's worth of lowercase hex after it, so bot `bot` never
/// claims bot `bot-a`'s `mux/bot-a-1f2e3d`.
pub fn is_bot_branch(branch: &str, bot: &str) -> bool {
    branch
        .strip_prefix(&format!("mux/{bot}-"))
        .is_some_and(|s| s.len() == BOT_BRANCH_SUFFIX_LEN && s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')))
}

/// The remote default branch to start new work from, as a ref that
/// exists in the checkout: `origin/HEAD`, else `origin/main`, else
/// `origin/master`; `None` when the checkout has no remote branches.
pub fn remote_default(dir: &Path) -> Option<String> {
    remote_default_with(|args| git_out(dir, args))
}

/// [`remote_default`] for a workspace, through the repository darkmux holds.
pub fn remote_default_of(wc: &crate::client_git::WorkCopy) -> Option<String> {
    remote_default_with(|args| crate::client_git::line(wc, args))
}

fn remote_default_with(git: impl Fn(&[&str]) -> Option<String>) -> Option<String> {
    if let Some(r) = git(&["rev-parse", "--abbrev-ref", "origin/HEAD"]) {
        if r.starts_with("origin/") && r != "origin/HEAD" {
            return Some(r);
        }
    }
    ["origin/main", "origin/master"].into_iter().find_map(|r| {
        git(&["rev-parse", "--verify", "--quiet", &format!("refs/remotes/{r}")]).map(|_| r.to_string())
    })
}

/// `git worktree add -b <branch> <dest> <commit>` from `source`, with the
/// repo's hooks off (an app asked for this, not the owner at a terminal),
/// through `HostGit`'s trusted mode (#3457): no network, so it needs none of
/// the credentials [`git_bounded_with`] keeps, and the checkout's own config
/// cannot run a program while git checks the files out.
pub fn add_worktree(source: &Path, branch: &str, dest: &Path, commit: &str) -> Result<(), Coded> {
    // (#3457) A filter the checkout's own config defines never runs here,
    // so a worktree would hold its filtered form: refused instead.
    let drivers = darkmux_types::host_git::HostGit::trusted(source).repo_filter_drivers();
    if !drivers.is_empty() {
        return Err(Coded::new(
            ClientCode::FilterInRepoConfig,
            crate::client_git::repo_filter_refusal(&drivers, "adding a worktree"),
        ));
    }
    add_worktree_unfiltered(source, branch, dest, commit).map_err(|e| Coded::new(ClientCode::Unprocessable, e))
}

/// [`add_worktree`] once the checkout's filters are known to be none.
fn add_worktree_unfiltered(source: &Path, branch: &str, dest: &Path, commit: &str) -> Result<(), String> {
    let dest = dest.to_string_lossy();
    let child = darkmux_types::host_git::HostGit::trusted(source)
        .command(&["worktree", "add", "--quiet", "-b", branch, &dest, commit])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("running git: {e}"))?;
    wait_bounded(child, Duration::from_secs(120))
}

/// Where darkmux records the user's own checkout a workspace was made from:
/// beside the workspace under the base, never inside it, so nothing a run
/// does in the workspace can change it.
fn source_record(base: &Path, name: &str) -> PathBuf {
    base.join(format!(".{name}.source"))
}

/// Record that workspace `name` is a worktree of the user's own `checkout`
/// (already [`check_source`]d). Replaces any earlier record, never through a
/// link.
pub fn record_source(base: &Path, name: &str, checkout: &Path) -> Result<(), String> {
    use std::io::Write;
    // A worktree of the user's checkout is no template project.
    forget_template_commit(base, name);
    let path = source_record(base, name);
    let _ = std::fs::remove_file(&path);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut f| writeln!(f, "{}", checkout.display()))
        .map_err(|e| format!("recording {}'s checkout: {e}", path.display()))
}

/// Forget workspace `name`'s checkout, and that it was a template project
/// (a new workspace under a name an old one had), and its preview install.
pub fn forget_source(base: &Path, name: &str) {
    let _ = std::fs::remove_file(source_record(base, name));
    let _ = std::fs::remove_file(template_record(base, name));
    let _ = std::fs::remove_file(install_record(base, name));
}

/// (#3484) Where darkmux records the fingerprint of workspace `name`'s last
/// successful preview install: beside the workspace, like
/// [`source_record`], outside anything a run can write.
fn install_record(base: &Path, name: &str) -> PathBuf {
    base.join(format!(".{name}.install"))
}

/// Record workspace `name`'s last successful preview install. Replaces any
/// earlier record, never through a link.
pub fn record_install(base: &Path, name: &str, fingerprint: &str) -> Result<(), String> {
    use std::io::Write;
    if !valid_name(name) {
        return Err(format!("{name:?} is not a workspace name"));
    }
    let path = install_record(base, name);
    let _ = std::fs::remove_file(&path);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut f| writeln!(f, "{fingerprint}"))
        .map_err(|e| format!("recording {}'s install: {e}", path.display()))
}

/// Forget workspace `name`'s preview install, so the next start installs.
pub fn forget_install(base: &Path, name: &str) {
    let _ = std::fs::remove_file(install_record(base, name));
}

/// The fingerprint of workspace `name`'s last successful preview install,
/// if any: a plain file (a link is no record).
pub fn install_fingerprint_of(base: &Path, name: &str) -> Option<String> {
    let path = install_record(base, name);
    if !valid_name(name) || !std::fs::symlink_metadata(&path).ok()?.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    Some(text.trim_end_matches('\n').to_string()).filter(|t| !t.is_empty())
}

/// (#3460) Where darkmux records the first commit of a project it made from
/// a template: beside the workspace, like [`source_record`], outside
/// anything a run can write.
fn template_record(base: &Path, name: &str) -> PathBuf {
    base.join(format!(".{name}.template"))
}

/// Record that workspace `name` is a project darkmux made from a template,
/// whose first commit (darkmux's own) is `commit`. Replaces any earlier
/// record, never through a link.
pub fn record_template_commit(base: &Path, name: &str, commit: &str) -> Result<(), String> {
    use std::io::Write;
    if !is_commit_sha(commit) {
        return Err(format!("{commit:?} is not a full commit SHA"));
    }
    let path = template_record(base, name);
    let _ = std::fs::remove_file(&path);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut f| writeln!(f, "{commit}"))
        .map_err(|e| format!("recording {}'s first commit: {e}", path.display()))
}

/// Forget that workspace `name` was made from a template.
pub fn forget_template_commit(base: &Path, name: &str) {
    let _ = std::fs::remove_file(template_record(base, name));
}

/// The first commit recorded for template project `name`, if any: a plain
/// file (a link is no record) holding a full commit SHA.
pub fn template_commit_of(base: &Path, name: &str) -> Option<String> {
    let path = template_record(base, name);
    if !std::fs::symlink_metadata(&path).ok()?.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let commit = text.trim_end_matches('\n');
    is_commit_sha(commit).then(|| commit.to_string())
}

/// Where darkmux records the paired client workspace `name` was made for:
/// beside the workspace, like [`source_record`], outside anything a run can
/// write.
fn owner_record(base: &Path, name: &str) -> PathBuf {
    base.join(format!(".{name}.client"))
}

/// Record that workspace `name` belongs to the paired client whose id is
/// `client`. Nothing is written when the record already says so; otherwise
/// a new record replaces the old in one rename (never through a link), so a
/// reader meanwhile sees the old owner or the new, never none.
pub fn record_owner(base: &Path, name: &str, client: &str) -> Result<(), String> {
    use std::io::Write;
    if !valid_name(name) {
        return Err(format!("{name:?} is not a workspace name"));
    }
    if owner_of(base, name).as_deref() == Some(client) {
        return Ok(());
    }
    let path = owner_record(base, name);
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = base.join(format!(".{name}.client.{}-{n}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut f| writeln!(f, "{client}"))
        .and_then(|()| std::fs::rename(&tmp, &path));
    written.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("recording {}'s app: {e}", path.display())
    })
}

/// Forget which paired client workspace `name` was made for.
pub fn forget_owner(base: &Path, name: &str) {
    let _ = std::fs::remove_file(owner_record(base, name));
}

/// The paired client workspace `name` was made for, if darkmux recorded one:
/// a plain file (a link is no record). A folder under the base that no
/// client asked darkmux for (a mission's worktrees, anything made by hand)
/// has none.
pub fn owner_of(base: &Path, name: &str) -> Option<String> {
    let path = owner_record(base, name);
    if !valid_name(name) || !std::fs::symlink_metadata(&path).ok()?.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    Some(text.trim_end_matches('\n').to_string()).filter(|t| !t.is_empty())
}

/// The first of `names` that paired client `client` owns. Another client's,
/// or one no client owns, never.
fn pick_for(base: &Path, names: Vec<String>, client: &str) -> Option<String> {
    names.into_iter().find(|n| owner_of(base, n).as_deref() == Some(client))
}

/// The workspaces under `base` darkmux holds a repository for (made for a
/// paired app), by name, sorted, with the client id each is recorded for.
pub fn held_owners(base: &Path) -> Vec<(String, Option<String>)> {
    held_workspaces(base).into_iter().map(|(n, _)| {
        let owner = owner_of(base, &n);
        (n, owner)
    }).collect()
}

/// The checkout recorded for workspace `name`, if any (a record that is a
/// link is no record).
fn recorded_source(base: &Path, name: &str) -> Option<PathBuf> {
    let path = source_record(base, name);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    Some(PathBuf::from(text.trim_end_matches('\n')))
}

/// The user's own checkout workspace `name` (at `dir`) was made from: as
/// darkmux recorded it when it made the workspace, still a checkout of the
/// user's ([`check_source`]), and still the repository the workspace is a
/// worktree of. `Err` says why not, and what to do. This is the only place
/// a preview recipe is read from: never the workspace, which a run writes.
pub fn own_checkout_of(base: &Path, name: &str, dir: &Path) -> Result<PathBuf, Coded> {
    let held = crate::client_git::work_copy_of(base, name, dir);
    let repo = held.as_ref().and_then(origin_of).unwrap_or_else(|| "this repo".to_string());
    let gone = |message: String| Coded::new(ClientCode::NoSourceCheckout, message);
    let Some(source) = recorded_source(base, name) else {
        return Err(gone(format!(
            "workspace {name} was not made from a checkout of yours (it was cloned), so it has no \
             preview: a preview runs only from .darkmux/preview.json in your own checkout. Make the \
             workspace from your checkout (`source_path`), and add .darkmux/preview.json to your own \
             checkout of {repo}"
        )));
    };
    let source = check_source(&source, base)
        .map_err(|e| gone(format!("workspace {name}'s checkout {}: {e}; refusing its preview", source.display())))?;
    let held_common = held.and_then(|wc| crate::client_git::common_of(&wc.git_dir));
    if held_common.is_none() || held_common != common_dir(&source) {
        return Err(gone(format!(
            "workspace {name} is no longer a worktree of your checkout {}; refusing its preview",
            source.display()
        )));
    }
    Ok(source)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding (file contents for an app-opened PR).
pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The inverse of [`base64_encode`]; `None` on anything that isn't base64.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut buf = 0u32;
    let mut bits = 0;
    for c in text.bytes().filter(|c| *c != b'=') {
        let v = B64.iter().position(|x| *x == c)? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_urls_normalize_to_host_owner_repo() {
        for (url, want) in [
            (
                "https://github.com/kstrat2001/hcr",
                "github.com/kstrat2001/hcr",
            ),
            (
                "https://GitHub.com/kstrat2001/hcr.git",
                "github.com/kstrat2001/hcr",
            ),
            (
                "https://github.com/kstrat2001/hcr/",
                "github.com/kstrat2001/hcr",
            ),
            (
                "git@github.com:kstrat2001/hcr.git",
                "github.com/kstrat2001/hcr",
            ),
        ] {
            assert_eq!(normalize_repo_url(url).as_deref(), Some(want), "{url}");
        }
    }

    #[test]
    fn only_https_and_ssh_repo_urls_of_one_owner_and_repo_are_accepted() {
        for url in [
            "file:///etc",
            "/Users/someone/repo",
            "ext::sh -c touch% /tmp/pwned",
            "http://github.com/a/b",
            "ssh://git@github.com/a/b",
            "https://github.com/a",
            "https://github.com/a/b/c",
            "https://github.com/../b",
            "https://github.com/a/..",
            "https://github.com/-oProxyCommand=x/b",
            "https://github.com/-x/b",
            "https://github.com/a/-b",
            "https://-x.com/a/b",
            "https://github.com/a/b c",
            "git@github.com:a/b;rm",
            "https://user:pw@github.com/a/b",
        ] {
            assert_eq!(normalize_repo_url(url), None, "{url} must be refused");
        }
    }

    #[test]
    fn refs_cannot_be_options_or_escapes() {
        for good in ["main", "feature/x", "v1.2.3", "claude/muxbot-preview-test"] {
            assert!(valid_ref(good), "{good}");
        }
        for bad in [
            "-b",
            "--upload-pack=x",
            "a..b",
            "/main",
            "main/",
            "a//b",
            "x.lock",
            "a@{1}",
            "a b",
            "",
        ] {
            assert!(!valid_ref(bad), "{bad:?} must be refused");
        }
    }

    #[test]
    fn workspace_names_are_one_plain_segment() {
        for good in ["hcr", "hcr-preview", "kstrat2001-hcr", "a.b_c"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "a/b",
            "../x",
            "A",
            "a..b",
            "-x",
            &"x".repeat(65),
        ] {
            assert!(!valid_name(bad), "{bad:?} must be refused");
        }
    }

    #[test]
    fn base64_round_trips() {
        for bytes in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"hello, world\n",
            &[0u8, 255, 128, 7],
        ] {
            assert_eq!(base64_decode(&base64_encode(bytes)).unwrap(), bytes);
        }
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_decode("!!"), None);
    }

    #[test]
    fn slugs_are_safe_in_names_and_branches() {
        assert_eq!(slug("humancodereader"), "humancodereader");
        assert_eq!(slug("My Website!"), "my-website");
        assert_eq!(slug("../../etc"), "etc");
        assert_eq!(slug("--x--"), "x");
        assert_eq!(slug("***"), "");
        assert!(slug(&"ab ".repeat(40)).len() <= 32);
    }

    #[test]
    fn a_failing_git_names_its_last_stderr_line() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let args: Vec<&std::ffi::OsStr> =
            vec!["-C".as_ref(), missing.as_os_str(), "status".as_ref()];
        let err = git_bounded_with(Path::new("/"), &[], &args, Duration::from_secs(30)).unwrap_err();
        assert!(err.starts_with("git exited with "), "{err}");
        assert!(err.contains("cannot change to"), "{err}");
    }

    /// A checkout with one commit (`.gitattributes` gives every path filter
    /// `x`), at `<tmp>/src`; returns (tmp root, checkout, marker path).
    fn checkout_for_test(tmp: &tempfile::TempDir) -> (PathBuf, PathBuf, PathBuf) {
        let root = tmp.path().canonicalize().unwrap();
        let (src, marker) = (root.join("src"), root.join("host-ran"));
        std::fs::create_dir_all(&src).unwrap();
        let fx = darkmux_types::host_git::fixture::git;
        fx(&src, &["init", "-q"]);
        std::fs::write(src.join(".gitattributes"), "* filter=x\n").unwrap();
        std::fs::write(src.join("a.txt"), "a\n").unwrap();
        fx(&src, &["add", "."]);
        fx(&src, &["commit", "-qm", "one"]);
        (root, src, marker)
    }

    /// (#3457) `add_worktree` checks files out of the user's checkout, whose
    /// config a run may have written (an earlier dispatch with it mounted):
    /// a post-checkout hook it names never runs.
    #[test]
    fn add_worktree_runs_no_hook_the_checkout_config_names() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, src, marker) = checkout_for_test(&tmp);
        let fx = darkmux_types::host_git::fixture::git;
        let hooks = src.join(".git").join("hooks");
        std::fs::write(hooks.join("post-checkout"), format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(hooks.join("post-checkout"), std::fs::Permissions::from_mode(0o755)).unwrap();
        fx(&src, &["config", "core.hooksPath", hooks.to_str().unwrap()]);

        add_worktree(&src, "mux/t", &root.join("wt"), "HEAD").expect("the worktree is added");
        assert!(root.join("wt").join("a.txt").exists());
        assert!(!marker.exists(), "the checkout's own hook ran");
    }

    /// (#3457) A filter driver the checkout's OWN config defines (git-crypt,
    /// `git lfs install --local`, or one a run wrote) is refused, naming the
    /// driver and the fix, rather than a worktree of its filtered form.
    #[test]
    fn add_worktree_refuses_a_checkout_whose_own_config_defines_a_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, src, marker) = checkout_for_test(&tmp);
        let fx = darkmux_types::host_git::fixture::git;
        fx(&src, &["config", "filter.x.smudge", &format!("touch '{}'; cat", marker.display())]);

        let coded = add_worktree(&src, "mux/t", &root.join("wt"), "HEAD").unwrap_err();
        assert_eq!(coded.code, ClientCode::FilterInRepoConfig, "(#3468) its code");
        let err = coded.message;
        assert!(err.contains("filter.x"), "names the driver: {err}");
        assert!(err.contains("--global"), "names the fix: {err}");
        assert!(!root.join("wt").exists(), "nothing is made");
        assert!(!marker.exists(), "the checkout's own smudge filter ran");
    }

    /// An origin nothing listens on: an ssh fetch fails fast, after git has
    /// started the ssh command.
    const DEAD_SSH_ORIGIN: &str = "ssh://127.0.0.1:1/x.git";

    /// (#3457) A fetch in the user's checkout reads its config, which a run
    /// may have written: a `core.sshCommand` there never runs, and the
    /// failed fetch says the setting was ignored and where it belongs.
    #[test]
    fn fetch_runs_no_ssh_command_the_checkout_config_names() {
        let tmp = tempfile::tempdir().unwrap();
        let (_root, src, marker) = checkout_for_test(&tmp);
        let fx = darkmux_types::host_git::fixture::git;
        fx(&src, &["remote", "add", "origin", DEAD_SSH_ORIGIN]);
        fx(&src, &["config", "core.sshCommand", &format!("touch '{}'; false", marker.display())]);

        let coded = fetch_with_git(&src).unwrap_err();
        assert_eq!(coded.code, ClientCode::FetchFailed, "(#3468) an ignored key leaves a plain failure");
        let err = coded.message;
        assert!(!marker.exists(), "the checkout's own core.sshCommand ran");
        assert!(err.contains("core.sshCommand") && err.contains("global"), "{err}");
    }

    /// (#3457) Network settings darkmux cannot switch off by override (a
    /// proxy, a URL rewrite, a per-URL credential helper, a protocol) in the
    /// checkout's own config refuse the fetch, naming the key and the fix.
    #[test]
    fn fetch_refuses_a_checkout_whose_own_config_sets_network_settings() {
        for (key, value) in [
            ("http.proxy", "http://127.0.0.1:9"),
            ("url.ext::sh.insteadOf", "ssh://"),
            ("credential.https://example.com.helper", "!touch x"),
            ("protocol.ext.allow", "always"),
            ("remote.origin.uploadpack", "touch x; git-upload-pack"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let (_root, src, marker) = checkout_for_test(&tmp);
            let fx = darkmux_types::host_git::fixture::git;
            fx(&src, &["remote", "add", "origin", DEAD_SSH_ORIGIN]);
            fx(&src, &["config", key, value]);
            fx(&src, &["config", "core.sshCommand", &format!("touch '{}'; false", marker.display())]);
            let coded = fetch_with_git(&src).unwrap_err();
            assert_eq!(coded.code, ClientCode::NetworkKeyInRepoConfig, "(#3468) {key}: its code");
            let err = coded.message;
            assert!(err.to_lowercase().contains(&key.to_lowercase()), "{key}: {err}");
            assert!(err.contains("includeIf"), "{key}: names the per-repo fix: {err}");
            assert!(!marker.exists(), "{key}: git ran");
        }
    }

    /// A new commit in `repo`, so the next fetch has something to fetch.
    fn bump(repo: &Path, what: &str) {
        let fx = darkmux_types::host_git::fixture::git;
        std::fs::write(repo.join("page.txt"), what).unwrap();
        fx(repo, &["commit", "-qam", what]);
    }

    /// Plain git's fetch, to prove a fixture arms.
    fn plain_fetch(dir: &Path) {
        let out = std::process::Command::new("git")
            .args(["-c", "protocol.file.allow=always", "-C"])
            .arg(dir)
            .args(["fetch", "-q", "origin"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    /// A checkout that borrows objects from an alternate runs its
    /// `core.alternateRefsCommand` while a fetch negotiates. One the
    /// checkout's own config names is refused, and never runs.
    #[test]
    fn fetch_runs_no_alternate_refs_command_the_checkout_config_names() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let fx = darkmux_types::host_git::fixture::git;
        let (remote, other, src) = (root.join("remote"), root.join("other"), root.join("src"));
        two_commit_repo(&remote);
        two_commit_repo(&other);
        fx(&root, &["clone", "-q", remote.to_str().unwrap(), src.to_str().unwrap()]);
        let alternates = src.join(".git").join("objects").join("info").join("alternates");
        std::fs::write(&alternates, format!("{}\n", other.join(".git").join("objects").display())).unwrap();
        let marker = root.join("alternate-refs-ran");
        fx(&src, &["config", "core.alternateRefsCommand", &format!("touch '{}'; true", marker.display())]);
        bump(&remote, "third");
        plain_fetch(&src);
        assert!(marker.exists(), "the fixture must arm: plain git's fetch runs the command");
        std::fs::remove_file(&marker).unwrap();
        bump(&remote, "fourth");

        let out = fetch_at(&src, ALLOW_FILE);
        assert!(!marker.exists(), "the checkout's own core.alternateRefsCommand ran");
        let coded = out.unwrap_err();
        assert_eq!(coded.code, ClientCode::NetworkKeyInRepoConfig);
        assert!(coded.message.contains("core.alternaterefscommand"), "{}", coded.message);
    }

    /// A fetch never recurses into the checkout's submodules: the child
    /// fetch git starts there reads the submodule's own repository, whose
    /// config the checkout's own key checks never see.
    #[test]
    fn fetch_never_recurses_into_a_submodule() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let fx = darkmux_types::host_git::fixture::git;
        let (sub_src, remote, src) = (root.join("subsrc"), root.join("remote"), root.join("src"));
        two_commit_repo(&sub_src);
        two_commit_repo(&remote);
        fx(&remote, &["submodule", "add", "-q", sub_src.to_str().unwrap(), "lib"]);
        fx(&remote, &["commit", "-qm", "with lib"]);
        fx(&root, &["clone", "-q", "--recurse-submodules", remote.to_str().unwrap(), src.to_str().unwrap()]);
        // The submodule's repository borrows objects and names a command.
        let sub_git = src.join(".git").join("modules").join("lib");
        let alternates = sub_git.join("objects").join("info").join("alternates");
        std::fs::write(&alternates, format!("{}\n", sub_src.join(".git").join("objects").display())).unwrap();
        let marker = root.join("submodule-alternate-refs-ran");
        fx(&sub_git, &["config", "core.alternateRefsCommand", &format!("touch '{}'; true", marker.display())]);
        let move_lib = |what: &str| {
            bump(&sub_src, what);
            fx(&remote.join("lib"), &["pull", "-q"]);
            fx(&remote, &["commit", "-qam", what]);
        };
        move_lib("third");
        plain_fetch(&src);
        assert!(marker.exists(), "the fixture must arm: plain git's fetch recurses and runs the command");
        std::fs::remove_file(&marker).unwrap();
        move_lib("fourth");

        fetch_at(&src, ALLOW_FILE).expect("the fetch itself succeeds");
        assert!(!marker.exists(), "the fetch recursed into the submodule and ran its config");
    }

    /// Sets `vars` (`None` removes one) for the duration of `f`, then puts
    /// back what was there. Callers are `#[serial]`.
    fn with_env<T>(vars: &[(&str, Option<&std::ffi::OsStr>)], f: impl FnOnce() -> T) -> T {
        let prev: Vec<(&str, Option<std::ffi::OsString>)> = vars.iter().map(|(k, _)| (*k, std::env::var_os(k))).collect();
        for (k, v) in vars {
            // SAFETY: #[serial] callers; restored below.
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        let out = f();
        for (k, v) in prev {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        out
    }

    /// The credential helpers a network operation runs are the ones the
    /// operator's own git would run for that URL, from the system config
    /// (where Xcode's git keeps `osxkeychain`) and the global one, per-URL
    /// entries included (`gh auth setup-git` writes an empty reset and then
    /// its own helper for its host); a helper the checkout's own config
    /// names is never among them.
    #[test]
    #[serial_test::serial]
    fn the_credential_helpers_are_the_operators_own_never_the_checkouts() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, src, _marker) = checkout_for_test(&tmp);
        let log = root.join("helpers-ran");
        let helper = |name: &str| format!("!f() {{ echo {name} >> '{}'; }}; f", log.display());
        // Quoted in a config file, where `;` would start a comment.
        let quoted = |name: &str| format!("\"{}\"", helper(name));
        let system = root.join("system-config");
        std::fs::write(&system, format!("[credential]\n\thelper = {}\n", quoted("system"))).unwrap();
        let global = root.join("global-config");
        std::fs::write(
            &global,
            format!(
                "[credential]\n\thelper = {}\n[credential \"https://forge.example.invalid\"]\n\thelper =\n\thelper = {}\n\
                 [credential \"https://other.invalid\"]\n\thelper = {}\n",
                quoted("global"),
                quoted("gh"),
                quoted("other-host")
            ),
        )
        .unwrap();
        darkmux_types::host_git::fixture::git(&src, &["config", "credential.helper", &helper("checkout")]);
        let fill = |args: &[String], host: &str| -> Vec<String> {
            use std::io::Write;
            let _ = std::fs::remove_file(&log);
            let mut child = std::process::Command::new("git")
                .args(args)
                .arg("-C")
                .arg(&src)
                .args(["credential", "fill"])
                .env("GIT_TERMINAL_PROMPT", "0")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(format!("protocol=https\nhost={host}\n\n").as_bytes()).unwrap();
            child.wait().unwrap();
            std::fs::read_to_string(&log).unwrap_or_default().lines().map(str::to_string).collect()
        };
        let env = [
            ("GIT_CONFIG_GLOBAL", Some(global.as_os_str())),
            ("GIT_CONFIG_SYSTEM", Some(system.as_os_str())),
            ("GIT_CONFIG_NOSYSTEM", None),
        ];
        let (plain, gh) = with_env(&env, || {
            let args = global_network_config(&src);
            (fill(&args, "plain.invalid"), fill(&args, "forge.example.invalid"))
        });
        assert_eq!(plain, ["system", "global"], "the system and global helpers, never the checkout's");
        assert_eq!(gh, ["gh"], "a per-URL reset and helper apply to their host as they do for the operator");
    }

    /// The operator's GLOBAL `core.sshCommand` is the one a fetch runs.
    #[test]
    #[serial_test::serial]
    fn fetch_runs_the_global_ssh_command() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, src, _marker) = checkout_for_test(&tmp);
        let fx = darkmux_types::host_git::fixture::git;
        fx(&src, &["remote", "add", "origin", DEAD_SSH_ORIGIN]);
        let global_marker = root.join("global-ssh-ran");
        let global = root.join("gitconfig");
        // Quoted: in a config file an unquoted `;` starts a comment, which
        // left `touch '<marker>'` with git's ssh arguments after it, and GNU
        // touch reads the `-p` among them as an option and touches nothing.
        std::fs::write(&global, format!("[core]\n\tsshCommand = \"touch '{}'; false\"\n", global_marker.display())).unwrap();
        let prev = std::env::var_os("GIT_CONFIG_GLOBAL");
        // SAFETY: #[serial]; restored below.
        unsafe { std::env::set_var("GIT_CONFIG_GLOBAL", &global) };
        let out = fetch_with_git(&src);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("GIT_CONFIG_GLOBAL", v),
                None => std::env::remove_var("GIT_CONFIG_GLOBAL"),
            }
        }
        assert!(out.is_err());
        assert!(global_marker.exists(), "the global core.sshCommand must be the one used");
    }

    #[test]
    fn a_slow_git_is_killed_at_its_deadline() {
        let args: Vec<&std::ffi::OsStr> =
            vec!["-c".as_ref(), "alias.nap=!sleep 5".as_ref(), "nap".as_ref()];
        let started = Instant::now();
        let err = git_bounded_with(Path::new("/"), &[], &args, Duration::from_secs(1)).unwrap_err();
        assert_eq!(err, "git took longer than 1s");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "killed, not waited out"
        );
    }

    #[test]
    fn a_taken_repo_name_falls_back_to_owner_repo() {
        assert_eq!(
            name_for("github.com/kstrat2001/hcr", |_| false).as_deref(),
            Some("hcr")
        );
        assert_eq!(
            name_for("github.com/kstrat2001/hcr", |n| n == "hcr").as_deref(),
            Some("kstrat2001-hcr")
        );
        assert_eq!(
            name_for("github.com/Owner/Repo", |n| n == "repo"
                || n == "owner-repo")
            .as_deref(),
            Some("owner-repo-2")
        );
    }

    /// A bot's branch is its prefix and exactly the random suffix darkmux
    /// gives one: never another bot's whose name starts with this one's, nor
    /// a branch someone named by hand.
    #[test]
    fn a_bots_branch_is_its_prefix_and_exactly_a_suffix() {
        assert!(is_bot_branch(&bot_branch("bot", "1f2e3d"), "bot"));
        assert!(is_bot_branch("mux/bot-a-1f2e3d", "bot-a"));
        for (branch, bot) in [("mux/bot-a-1f2e3d", "bot"), ("mux/bot-cafe", "bot"), ("mux/bot-1f2e3d4", "bot"), ("mux/bot-1F2E3D", "bot"), ("main", "bot")] {
            assert!(!is_bot_branch(branch, bot), "{branch} {bot}");
        }
    }

    /// A repo whose name is longer than a workspace name may be still gets
    /// one, numbered fallbacks included.
    #[test]
    fn a_long_repo_name_still_gets_a_workspace_name() {
        let normalized = format!("github.com/{}/{}", "o".repeat(30), "r".repeat(70));
        // The first `taken` names asked about are taken; the last is `-99`.
        for taken in [0, 1, 2, 99] {
            let asked = std::cell::Cell::new(0);
            let n = name_for(&normalized, |_| {
                asked.set(asked.get() + 1);
                asked.get() <= taken
            });
            assert!(n.as_deref().is_some_and(valid_name), "{taken} taken: {n:?}");
        }
    }

    /// A git repo at `dir` with two commits on `main`; returns the first
    /// commit's full SHA (behind the branch tip, so only a SHA reaches it).
    fn two_commit_repo(dir: &Path) -> String {
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        std::fs::create_dir_all(dir).unwrap();
        git(&["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(dir.join("page.txt"), "first").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "--quiet", "-m", "first"]);
        let first = git(&["rev-parse", "HEAD"]);
        std::fs::write(dir.join("page.txt"), "second").unwrap();
        git(&["commit", "--quiet", "-am", "second"]);
        first
    }

    const ALLOW_FILE: &[&str] = &["-c", "protocol.file.allow=always"];

    #[test]
    fn a_template_pinned_to_a_commit_sha_clones_at_that_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("template");
        let first = two_commit_repo(&src);
        let url = format!("file://{}", src.display());
        let dest = tmp.path().join("project");
        let git_dir = tmp.path().join("project.git");
        clone_at(&url, Some(&first), &dest, &git_dir, ALLOW_FILE).unwrap();
        assert_eq!(std::fs::read_to_string(dest.join("page.txt")).unwrap(), "first");
    }

    #[test]
    fn a_commit_on_no_branch_is_fetched_by_its_sha() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("template");
        two_commit_repo(&src);
        // A commit kept only by a non-branch ref (like a pull request's),
        // which a clone does not fetch.
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git").arg("-C").arg(&src).args(args).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let tree = run(&["rev-parse", "HEAD^{tree}"]);
        let lone = run(&["-c", "user.name=t", "-c", "user.email=t@t", "commit-tree", &tree, "-m", "lone"]);
        run(&["update-ref", "refs/pull/1/head", &lone]);
        let url = format!("file://{}", src.display());
        let dest = tmp.path().join("project");
        let git_dir = tmp.path().join("project.git");
        clone_at(&url, Some(&lone), &dest, &git_dir, ALLOW_FILE).unwrap();
        let head = std::process::Command::new("git").arg("-C").arg(&dest).args(["rev-parse", "HEAD"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), lone);
    }

    #[test]
    fn a_branch_ref_still_clones_that_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("template");
        two_commit_repo(&src);
        let url = format!("file://{}", src.display());
        let dest = tmp.path().join("project");
        let git_dir = tmp.path().join("project.git");
        clone_at(&url, Some("main"), &dest, &git_dir, ALLOW_FILE).unwrap();
        assert_eq!(std::fs::read_to_string(dest.join("page.txt")).unwrap(), "second");
    }

    #[test]
    fn a_commit_sha_the_remote_does_not_have_fails_naming_git() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("template");
        two_commit_repo(&src);
        let url = format!("file://{}", src.display());
        let dest = tmp.path().join("project");
        let git_dir = tmp.path().join("project.git");
        let missing = "0123456789abcdef0123456789abcdef01234567";
        let err = clone_at(&url, Some(missing), &dest, &git_dir, ALLOW_FILE).unwrap_err();
        assert!(err.starts_with("git exited with "), "{err}");
    }

    /// (#3460) A template project's first commit is recorded beside the
    /// workspace as a plain file holding a full SHA: a record that is a link,
    /// or holds anything else, is no record, and a worktree recorded under
    /// the name forgets it.
    #[cfg(unix)]
    #[test]
    fn a_template_record_is_a_plain_file_holding_a_full_sha() {
        let base = tempfile::tempdir().unwrap();
        let sha = "e044900cd94d086c841616b05ed63638aa402be4";
        assert!(record_template_commit(base.path(), "p", "HEAD").is_err());
        record_template_commit(base.path(), "p", sha).unwrap();
        assert_eq!(template_commit_of(base.path(), "p").as_deref(), Some(sha));
        let elsewhere = base.path().join("elsewhere");
        std::fs::write(&elsewhere, format!("{sha}\n")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, base.path().join(".q.template")).unwrap();
        assert_eq!(template_commit_of(base.path(), "q"), None, "a link is no record");
        std::fs::write(base.path().join(".r.template"), "main\n").unwrap();
        assert_eq!(template_commit_of(base.path(), "r"), None, "not a SHA");
        record_source(base.path(), "p", Path::new("/tmp/mine")).unwrap();
        assert_eq!(
            template_commit_of(base.path(), "p"),
            None,
            "a worktree is no template project"
        );
    }

    #[test]
    fn only_full_hex_object_names_count_as_commit_shas() {
        assert!(is_commit_sha("e044900cd94d086c841616b05ed63638aa402be4"));
        assert!(is_commit_sha(&"a".repeat(64)));
        assert!(!is_commit_sha("e044900"), "a short SHA is ambiguous, and could be a branch");
        assert!(!is_commit_sha("main"));
        assert!(!is_commit_sha("E044900CD94D086C841616B05ED63638AA402BE4"));
        assert!(!is_commit_sha(&"g".repeat(40)));
    }
}
