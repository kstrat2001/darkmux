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
    let rest = if let Some(r) = url.strip_prefix("https://") {
        let (host, path) = r.split_once('/')?;
        (host.to_ascii_lowercase(), path.to_string())
    } else {
        let (host, path) = url.strip_prefix("git@")?.split_once(':')?;
        (host.to_ascii_lowercase(), path.to_string())
    };
    let (host, path) = rest;
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let segment = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && !s.starts_with('-')
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    let host_ok = !host.is_empty()
        && !host.starts_with('-')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'));
    let parts: Vec<&str> = path.split('/').collect();
    (host_ok && parts.len() == 2 && parts.iter().all(|p| segment(p)))
        .then(|| format!("{host}/{}/{}", parts[0], parts[1]))
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
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(wd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The normalized origin of a checkout, if it has one darkmux would accept.
pub fn origin_of(dir: &Path) -> Option<String> {
    git_out(dir, &["remote", "get-url", "origin"]).and_then(|u| normalize_repo_url(&u))
}

/// The workspace under `base` whose origin is `normalized`: plain
/// directories only, never a symlink.
pub fn find_by_origin(base: &Path, normalized: &str) -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir(base)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| valid_name(n))
        .collect();
    names.sort();
    names
        .into_iter()
        .find(|n| origin_of(&base.join(n)).as_deref() == Some(normalized))
}

/// Branch (empty when detached) and short head of a checkout.
pub fn status_of(dir: &Path) -> Option<(String, String)> {
    let head = git_out(dir, &["rev-parse", "--short", "HEAD"])?;
    let branch = git_out(dir, &["branch", "--show-current"]).unwrap_or_default();
    Some((branch, head))
}

/// Runs git with no terminal (credential prompts fail instead of hanging),
/// only the https and ssh transports, and a deadline.
fn git_bounded(args: &[&std::ffi::OsStr], timeout: Duration) -> Result<(), String> {
    let mut child = std::process::Command::new("git")
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
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                let mut err = String::new();
                if let Some(mut s) = child.stderr.take() {
                    use std::io::Read;
                    let _ = s.read_to_string(&mut err);
                }
                let last = err.lines().last().unwrap_or("").trim().to_string();
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

/// Production clone: `git clone [--branch <ref>] -- <url> <dest>`.
pub fn clone_with_git(url: &str, git_ref: Option<&str>, dest: &Path) -> Result<(), String> {
    let mut args: Vec<&std::ffi::OsStr> = vec!["clone".as_ref(), "--quiet".as_ref()];
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
    if !path.is_absolute() {
        return Err("source_path must be absolute".into());
    }
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            return Err(format!("{} is a symlink; refusing", path.display()))
        }
        Ok(m) if !m.is_dir() => return Err(format!("{} is not a folder", path.display())),
        Ok(_) => {}
        Err(_) => return Err(format!("{} does not exist", path.display())),
    }
    let canon = path
        .canonicalize()
        .map_err(|e| format!("resolving {}: {e}", path.display()))?;
    if canon.starts_with(canon_base) {
        return Err("source_path is already inside the worktrees base".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let me = unsafe { libc::geteuid() };
        let owner = std::fs::metadata(&canon).map_err(|e| e.to_string())?.uid();
        if owner != me {
            return Err(format!("{} is not owned by you", canon.display()));
        }
    }
    let top = git_out(&canon, &["rev-parse", "--show-toplevel"])
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
    Ok(canon)
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
/// `.git` is `common`, on one of this bot's `mux/<bot>-` branches.
pub fn find_worktree_of(base: &Path, common: &Path, bot: &str) -> Option<String> {
    let prefix = format!("mux/{bot}-");
    let mut names: Vec<String> = std::fs::read_dir(base)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| valid_name(n))
        .collect();
    names.sort();
    names.into_iter().find(|n| {
        let dir = base.join(n);
        common_dir(&dir).as_deref() == Some(common)
            && status_of(&dir).is_some_and(|(branch, _)| branch.starts_with(&prefix))
    })
}

/// The remote default branch to start new work from, as a ref that
/// exists in the checkout: `origin/HEAD`, else `origin/main`, else
/// `origin/master`; `None` when the checkout has no remote branches.
pub fn remote_default(dir: &Path) -> Option<String> {
    if let Some(r) = git_out(dir, &["rev-parse", "--abbrev-ref", "origin/HEAD"]) {
        if r.starts_with("origin/") && r != "origin/HEAD" {
            return Some(r);
        }
    }
    ["origin/main", "origin/master"].into_iter().find_map(|r| {
        git_out(
            dir,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/remotes/{r}"),
            ],
        )
        .map(|_| r.to_string())
    })
}

/// `git worktree add -b <branch> <dest> <commit>` from `source`, with the
/// repo's hooks off (an app asked for this, not the owner at a terminal).
pub fn add_worktree(source: &Path, branch: &str, dest: &Path, commit: &str) -> Result<(), String> {
    let args: Vec<&std::ffi::OsStr> = vec![
        "-c".as_ref(),
        "core.hooksPath=/dev/null".as_ref(),
        "-C".as_ref(),
        source.as_os_str(),
        "worktree".as_ref(),
        "add".as_ref(),
        "--quiet".as_ref(),
        "-b".as_ref(),
        branch.as_ref(),
        dest.as_os_str(),
        commit.as_ref(),
    ];
    git_bounded(&args, Duration::from_secs(120))
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
    let repo = origin_of(dir).unwrap_or_else(|| "this repo".to_string());
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
    if common_dir(dir).is_none() || common_dir(dir) != common_dir(&source) {
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
