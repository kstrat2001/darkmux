//! Workspaces for paired apps: a repo checkout darkmux owns under the
//! worktrees base, named, never addressed by path from the app.
//!
//! `POST /client/workspaces {repo_url, ref?}` reuses the workspace whose
//! `origin` is that repo (fetching it), or clones it into a new one with the
//! machine's own git credentials. `GET /client/workspaces/{name}` reports it.
//! `/client/work` and `/client/recipe` take `workspace` in place of a raw
//! `workdir` and resolve it here, under the same containment check.
//!
//! What a client can name is narrow on purpose: an `https://` or `git@` URL of
//! the form `host/owner/repo` (no other transport, no options), a branch name
//! that can't be read as an option, and workspace names that are one plain
//! path segment.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Workspace names: one path segment, lowercase, no leading dot.
pub fn valid_name(name: &str) -> bool {
    let len_ok = (1..=64).contains(&name.len());
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
/// repo's name is taken by another checkout.
pub fn name_for(normalized: &str, taken: impl Fn(&str) -> bool) -> Option<String> {
    let mut parts = normalized.rsplit('/');
    let repo = parts.next()?.to_ascii_lowercase();
    let owner = parts.next()?.to_ascii_lowercase();
    [repo.clone(), format!("{owner}-{repo}")]
        .into_iter()
        .chain((2..100).map(|n| format!("{owner}-{repo}-{n}")))
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

/// The workspace under `base` whose origin is `normalized`, among those
/// darkmux holds a repository for.
pub fn find_by_origin(base: &Path, normalized: &str) -> Option<String> {
    held_workspaces(base)
        .into_iter()
        .find(|(_, wc)| origin_of(wc).as_deref() == Some(normalized))
        .map(|(n, _)| n)
}

/// Branch (empty when detached) and short head of a workspace.
pub fn status_of(wc: &crate::client_git::WorkCopy) -> Option<(String, String)> {
    let head = crate::client_git::line(wc, &["rev-parse", "--short", "HEAD"])?;
    let branch = crate::client_git::line(wc, &["branch", "--show-current"]).unwrap_or_default();
    Some((branch, head))
}

/// Runs git with no terminal (credential prompts fail instead of hanging),
/// only the https and ssh transports, and a deadline. For clone and fetch
/// only, which need the operator's own credential helper and ssh command:
/// `darkmux_types::host_git::HostGit` clears both, so this is the one spawn
/// in this file its lint allows (#3457). A fetch runs against the repository
/// darkmux holds, never one discovered in a work tree.
fn git_bounded(args: &[&std::ffi::OsStr], timeout: Duration) -> Result<(), String> {
    let child = std::process::Command::new("git")
        .args([
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "protocol.ssh.allow=always",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("running git: {e}"))?;
    wait_bounded(child, timeout)
}

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
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "clone".as_ref(),
        "--quiet".as_ref(),
        "--separate-git-dir".as_ref(),
        git_dir.as_os_str(),
    ];
    if let Some(r) = git_ref {
        args.push("--branch".as_ref());
        args.push(r.as_ref());
    }
    args.push("--".as_ref());
    args.push(url.as_ref());
    args.push(dest.as_os_str());
    git_bounded(&args, Duration::from_secs(600))
}

/// Production fetch: updates remote-tracking refs only; the checkout's
/// branch and files are never touched.
pub fn fetch_with_git(dir: &Path) -> Result<(), String> {
    let args: Vec<&std::ffi::OsStr> = vec![
        "-C".as_ref(),
        dir.as_os_str(),
        "fetch".as_ref(),
        "--quiet".as_ref(),
        "origin".as_ref(),
    ];
    git_bounded(&args, Duration::from_secs(120))
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
/// `.git` is `common`, on one of this bot's `mux/<bot>-` branches, judged
/// by the repository darkmux recorded for it (never its own `.git`).
pub fn find_worktree_of(base: &Path, common: &Path, bot: &str) -> Option<String> {
    let prefix = format!("mux/{bot}-");
    held_workspaces(base)
        .into_iter()
        .find(|(_, wc)| {
            crate::client_git::common_of(&wc.git_dir).as_deref() == Some(common)
                && status_of(wc).is_some_and(|(branch, _)| branch.starts_with(&prefix))
        })
        .map(|(n, _)| n)
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
/// the credentials [`git_bounded`] keeps, and the checkout's own config
/// cannot run a program while git checks the files out.
pub fn add_worktree(source: &Path, branch: &str, dest: &Path, commit: &str) -> Result<(), String> {
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
    let path = source_record(base, name);
    let _ = std::fs::remove_file(&path);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut f| writeln!(f, "{}", checkout.display()))
        .map_err(|e| format!("recording {}'s checkout: {e}", path.display()))
}

/// Forget workspace `name`'s checkout (a new workspace under a name an old
/// one had).
pub fn forget_source(base: &Path, name: &str) {
    let _ = std::fs::remove_file(source_record(base, name));
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
pub fn own_checkout_of(base: &Path, name: &str, dir: &Path) -> Result<PathBuf, String> {
    let held = crate::client_git::work_copy_of(base, name, dir);
    let repo = held.as_ref().and_then(origin_of).unwrap_or_else(|| "this repo".to_string());
    let Some(source) = recorded_source(base, name) else {
        return Err(format!(
            "workspace {name} was not made from a checkout of yours (it was cloned), so it has no \
             preview: a preview runs only from .darkmux/preview.json in your own checkout. Make the \
             workspace from your checkout (`source_path`), and add .darkmux/preview.json to your own \
             checkout of {repo}"
        ));
    };
    let source = check_source(&source, base).map_err(|e| {
        format!("workspace {name}'s checkout {}: {e}; refusing its preview", source.display())
    })?;
    let held_common = held.and_then(|wc| crate::client_git::common_of(&wc.git_dir));
    if held_common.is_none() || held_common != common_dir(&source) {
        return Err(format!(
            "workspace {name} is no longer a worktree of your checkout {}; refusing its preview",
            source.display()
        ));
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
        let err = git_bounded(&args, Duration::from_secs(30)).unwrap_err();
        assert!(err.starts_with("git exited with "), "{err}");
        assert!(err.contains("cannot change to"), "{err}");
    }

    /// (#3457) `add_worktree` checks files out of the user's checkout, whose
    /// config a run may have written (an earlier dispatch with it mounted):
    /// a smudge filter or a post-checkout hook it names never runs.
    #[test]
    fn add_worktree_runs_no_filter_or_hook_the_checkout_config_names() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (src, marker) = (root.join("src"), root.join("host-ran"));
        std::fs::create_dir_all(&src).unwrap();
        let fx = darkmux_types::host_git::fixture::git;
        fx(&src, &["init", "-q"]);
        std::fs::write(src.join(".gitattributes"), "* filter=x\n").unwrap();
        std::fs::write(src.join("a.txt"), "a\n").unwrap();
        fx(&src, &["add", "."]);
        fx(&src, &["commit", "-qm", "one"]);
        fx(&src, &["config", "filter.x.smudge", &format!("touch '{}'; cat", marker.display())]);
        fx(&src, &["config", "filter.x.required", "true"]);
        let hooks = src.join(".git").join("hooks");
        std::fs::write(hooks.join("post-checkout"), format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(hooks.join("post-checkout"), std::fs::Permissions::from_mode(0o755)).unwrap();
        fx(&src, &["config", "core.hooksPath", hooks.to_str().unwrap()]);

        add_worktree(&src, "mux/t", &root.join("wt"), "HEAD").expect("the worktree is added");
        assert!(root.join("wt").join("a.txt").exists());
        assert!(!marker.exists(), "the checkout's own smudge filter or hook ran");
    }

    #[test]
    fn a_slow_git_is_killed_at_its_deadline() {
        let args: Vec<&std::ffi::OsStr> =
            vec!["-c".as_ref(), "alias.nap=!sleep 5".as_ref(), "nap".as_ref()];
        let started = Instant::now();
        let err = git_bounded(&args, Duration::from_secs(1)).unwrap_err();
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
}
