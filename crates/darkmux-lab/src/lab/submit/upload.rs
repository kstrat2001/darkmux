//! The upload token and the two calls `lab submit` makes to the site: who the
//! token belongs to (`GET /api/v1/account`) and the upload itself (`POST
//! /api/v1/uploads`).
//!
//! The token is made on the site's account page and handed to `darkmux lab
//! submit login`. It is kept where the platform keeps secrets (the login
//! keychain on macOS, a 0600 file elsewhere) and never on a command line: it
//! reaches `curl` through a 0600 config file, removed after the call, the way
//! hosted dispatch sends its key.

use anyhow::{anyhow, bail, Context, Result};
use darkmux_platform::SecretStore;
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The keychain item, and the file name under `~/.darkmux` elsewhere.
pub const TOKEN_ITEM: &str = "darkmux-evals-token";
pub const TOKEN_PREFIX: &str = "dmx_evals_";

/// Whether a value is shaped like an upload token. Checked before it is
/// stored, so nothing else reaches the keychain command.
pub fn is_token(value: &str) -> bool {
    value.strip_prefix(TOKEN_PREFIX).is_some_and(|rest| {
        (16..=200).contains(&rest.len()) && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

fn token_file() -> PathBuf {
    crate::lab::paths::resolve(crate::lab::paths::ResolveScope::ForceUser).root.join("evals-token")
}

/// The keychain account: `$USER`, when it is a plain name (it reaches the `security -i` command line).
fn user() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty() && u.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)))
        .unwrap_or_else(|| "darkmux".to_string())
}

/// The stored token, or `None` when none is stored.
pub fn read_token() -> Option<String> {
    let value = match darkmux_platform::current().secret_store() {
        SecretStore::Keychain => {
            let account = user();
            match darkmux_flow::read_keychain_bounded(
                TOKEN_ITEM,
                Some(account.as_str()),
                std::time::Duration::from_secs(5),
            ) {
                darkmux_flow::KeychainRead::Found(v) => v,
                _ => return None,
            }
        }
        SecretStore::PrivateFile => std::fs::read_to_string(token_file()).ok()?,
    };
    let value = value.trim().to_string();
    is_token(&value).then_some(value)
}

pub fn store_token(token: &str) -> Result<String> {
    if !is_token(token) {
        bail!("that is not an upload token (they start with {TOKEN_PREFIX}); make one on the site's account page");
    }
    match darkmux_platform::current().secret_store() {
        SecretStore::Keychain => {
            // `security -i` reads the command from stdin, so the token is never on an argv.
            let mut child = Command::new("security")
                .arg("-i")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .context("running `security` to store the token in the login keychain")?;
            let line = format!("add-generic-password -U -a \"{}\" -s {TOKEN_ITEM} -w \"{token}\"\n", user());
            child.stdin.take().expect("piped").write_all(line.as_bytes())?;
            let out = child.wait_with_output()?;
            // `security -i` can exit 0 when its command failed: the read back is the check.
            if !out.status.success() || read_token().as_deref() != Some(token) {
                bail!("the login keychain did not keep the token: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
            Ok("the login keychain".to_string())
        }
        SecretStore::PrivateFile => {
            let path = token_file();
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            write_private(&path, format!("{token}\n").as_bytes())?;
            Ok(path.display().to_string())
        }
    }
}

pub fn forget_token() -> Result<()> {
    match darkmux_platform::current().secret_store() {
        SecretStore::Keychain => {
            let _ = Command::new("security")
                .args(["delete-generic-password", "-a", &user(), "-s", TOKEN_ITEM])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        SecretStore::PrivateFile => {
            let _ = std::fs::remove_file(token_file());
        }
    }
    Ok(())
}

/// A file only this user can read, created that way (never widened after).
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let _ = std::fs::remove_file(path);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).with_context(|| format!("creating {}", path.display()))?;
    f.write_all(bytes)?;
    Ok(())
}

/// What the site answered: the HTTP status and the JSON body.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub body: Value,
}

impl Answer {
    /// The site's error text, with its listed problems, for a refusal.
    pub fn refusal(&self) -> String {
        let mut text = self.body.get("error").and_then(Value::as_str).unwrap_or("the site refused it").to_string();
        if let Some(errors) = self.body.get("errors").and_then(Value::as_array) {
            for e in errors.iter().filter_map(Value::as_str) {
                text.push_str("\n  ");
                text.push_str(e);
            }
        }
        format!("{text} (HTTP {})", self.status)
    }
}

static CFG_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One call to the site through `curl`. The token and URL ride in a 0600
/// config file; a body is read from `body_file`.
pub fn call(site: &str, method: &str, path: &str, token: &str, body_file: Option<&Path>) -> Result<Answer> {
    let url = format!("{}{path}", site.trim_end_matches('/'));
    if !site_is_safe(site) {
        bail!("the site must be https (or a preview on this machine): {site}");
    }
    let n = CFG_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let cfg = std::env::temp_dir().join(format!("darkmux-evals-{}-{n}.curl", std::process::id()));
    let quote = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let mut config = format!(
        "url = \"{}\"\nrequest = \"{method}\"\nheader = \"Authorization: Bearer {}\"\nheader = \"Accept: application/json\"\n",
        quote(&url),
        quote(token)
    );
    if let Some(file) = body_file {
        config.push_str("header = \"Content-Type: application/json\"\n");
        config.push_str(&format!("data-binary = \"@{}\"\n", quote(&file.display().to_string())));
    }
    write_private(&cfg, config.as_bytes())?;
    let out = Command::new("curl")
        // `-q` first: no `~/.curlrc`, whose `verbose` would print the token.
        .args(["-q", "-sS", "--max-time", "120", "-w", "\n%{http_code}", "-K"])
        .arg(&cfg)
        .output();
    let _ = std::fs::remove_file(&cfg);
    let out = out.context("running curl (darkmux uses it for HTTPS)")?;
    if !out.status.success() {
        bail!("could not reach {site}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    parse_answer(&String::from_utf8_lossy(&out.stdout))
}

/// The token goes only over https, or plain http to this machine
/// (`localhost`, `127.0.0.1`, a `*.localhost` preview): judged on the host
/// itself, not on how the address starts.
pub fn site_is_safe(site: &str) -> bool {
    if site.starts_with("https://") {
        return !site.contains(char::is_whitespace);
    }
    let Some(rest) = site.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return false;
    }
    let host = match authority.rsplit_once(':') {
        Some((h, port)) if port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => authority,
    }
    .to_ascii_lowercase();
    host == "localhost" || host == "127.0.0.1" || (host.ends_with(".localhost") && host.len() > ".localhost".len())
}

/// curl's output with `-w "\n%{http_code}"`: the body, then the status line.
pub fn parse_answer(text: &str) -> Result<Answer> {
    let (body, status) = text.rsplit_once('\n').ok_or_else(|| anyhow!("the site gave no answer"))?;
    let status: u16 = status.trim().parse().map_err(|_| anyhow!("the site gave no status"))?;
    let body = serde_json::from_str(body).unwrap_or_else(|_| {
        serde_json::json!({ "error": format!("the site answered something that is not JSON: {}", body.chars().take(200).collect::<String>()) })
    });
    Ok(Answer { status, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_the_prefix_and_safe_characters_only() {
        assert!(is_token(&format!("dmx_evals_{}", "ab12".repeat(16))));
        assert!(!is_token("dmx_evals_short"));
        assert!(!is_token(&format!("dmx_evals_{}\" -s x", "a".repeat(20))), "nothing that could reach the keychain command");
        assert!(!is_token(&format!("dmx_lead_{}", "a".repeat(64))));
    }

    #[test]
    fn the_answer_is_the_body_then_the_status() {
        let a = parse_answer("{\"error\":\"This eval is waiting for approval\",\"errors\":[\"runs[0]: x\"]}\n422").unwrap();
        assert_eq!(a.status, 422);
        assert_eq!(a.refusal(), "This eval is waiting for approval\n  runs[0]: x (HTTP 422)");
        let html = parse_answer("<html>bad gateway</html>\n502").unwrap();
        assert!(html.refusal().contains("not JSON"));
        assert!(parse_answer("no status").is_err());
    }

    #[test]
    fn plain_http_only_to_this_machine() {
        for ok in ["https://evals.darkmux.com", "http://localhost:3419", "http://de.localhost:3419", "http://127.0.0.1"] {
            assert!(site_is_safe(ok), "{ok}");
        }
        for bad in [
            "http://localhost.evil.com",
            "http://evil.com/.localhost:",
            "http://evil.com",
            "http://localhost@evil.com",
            "ftp://x",
        ] {
            assert!(!site_is_safe(bad), "{bad}");
        }
    }

    #[test]
    fn the_site_must_be_https_or_local() {
        let err = call("http://evals.example.com", "GET", "/api/v1/account", "dmx_evals_x", None).unwrap_err();
        assert!(err.to_string().contains("https"));
    }
}
