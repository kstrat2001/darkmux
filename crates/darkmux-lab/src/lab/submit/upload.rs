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
use darkmux_platform::{Platform, SecretStore};
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

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

/// The keychain account: `$USER`.
fn user() -> String {
    plain_user(std::env::var("USER").ok())
}

/// A user name only when it is a plain name (it reaches the `security -i` command line).
fn plain_user(name: Option<String>) -> String {
    name.filter(|u| !u.is_empty() && u.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)))
        .unwrap_or_else(|| "darkmux".to_string())
}

/// The stored token, or `None` when none is stored.
pub fn read_token() -> Option<String> {
    read_token_from(darkmux_platform::current(), &token_file())
}

pub fn store_token(token: &str) -> Result<String> {
    store_token_in(darkmux_platform::current(), &token_file(), token)
}

pub fn forget_token() -> Result<()> {
    forget_token_in(darkmux_platform::current(), &token_file());
    Ok(())
}

/// [`read_token`] where `platform` keeps secrets, `file` being the token file.
fn read_token_from(platform: &dyn Platform, file: &Path) -> Option<String> {
    let value = match platform.secret_store() {
        SecretStore::Keychain => read_keychain()?,
        SecretStore::PrivateFile => std::fs::read_to_string(file).ok()?,
    };
    let value = value.trim().to_string();
    is_token(&value).then_some(value)
}

fn read_keychain() -> Option<String> {
    let account = user();
    match darkmux_flow::read_keychain_bounded(TOKEN_ITEM, Some(account.as_str()), std::time::Duration::from_secs(5)) {
        darkmux_flow::KeychainRead::Found(v) => Some(v),
        _ => None,
    }
}

fn store_token_in(platform: &dyn Platform, file: &Path, token: &str) -> Result<String> {
    if !is_token(token) {
        bail!("that is not an upload token (they start with {TOKEN_PREFIX}); make one on the site's account page");
    }
    match platform.secret_store() {
        SecretStore::Keychain => store_in_keychain(platform, file, token).map(|()| "the login keychain".to_string()),
        SecretStore::PrivateFile => store_in_file(file, token),
    }
}

fn store_in_keychain(platform: &dyn Platform, file: &Path, token: &str) -> Result<()> {
    let kept = platform.keychain_store(TOKEN_ITEM, &user(), token);
    // `security -i` can exit 0 when its command failed: the read back is the check.
    if read_token_from(platform, file).as_deref() != Some(token) {
        bail!("the login keychain did not keep the token: {}", kept.err().unwrap_or_default());
    }
    Ok(())
}

fn store_in_file(path: &Path, token: &str) -> Result<String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_private(path, format!("{token}\n").as_bytes())?;
    Ok(path.display().to_string())
}

fn forget_token_in(platform: &dyn Platform, file: &Path) {
    match platform.secret_store() {
        SecretStore::Keychain => platform.keychain_forget(TOKEN_ITEM, &user()),
        SecretStore::PrivateFile => {
            let _ = std::fs::remove_file(file);
        }
    }
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
    use darkmux_types::endpoint::{url_host, url_names_this_machine};
    if site.contains(char::is_whitespace) {
        return false;
    }
    if site.starts_with("https://") {
        return true;
    }
    // The address must be `http://` and then the very host curl dials: no userinfo to hide another behind.
    let Some(hostport) = url_host(site) else {
        return false;
    };
    if !site.starts_with(&format!("http://{hostport}")) {
        return false;
    }
    let host = hostport.rsplit_once(':').map_or(hostport.as_str(), |(h, _)| h).to_ascii_lowercase();
    url_names_this_machine(site) || (host.ends_with(".localhost") && host.len() > ".localhost".len())
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
        for ok in ["https://evals.darkmux.com", "http://localhost:3419", "http://de.localhost:3419", "http://127.0.0.1", "http://[::1]:3419"] {
            assert!(site_is_safe(ok), "{ok}");
        }
        for bad in [
            "http://localhost.evil.com",
            "http://evil.com/.localhost:",
            "http://evil.com",
            "http://localhost@evil.com",
            "http://me@localhost:3419",
            "http://.localhost",
            "https://evals.darkmux.com x",
            "ftp://x",
        ] {
            assert!(!site_is_safe(bad), "{bad}");
        }
    }

    fn token() -> String {
        format!("dmx_evals_{}", "ab12".repeat(8))
    }

    #[test]
    fn a_token_kept_in_a_file_is_private_read_back_and_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested").join("evals-token");
        let fake = darkmux_platform::FakePlatform::default();
        assert_eq!(read_token_from(&fake, &file), None);
        assert!(store_token_in(&fake, &file, "dmx_evals_short").is_err(), "a malformed token is never stored");
        assert_eq!(store_token_in(&fake, &file, &token()).unwrap(), file.display().to_string());
        assert_eq!(read_token_from(&fake, &file), Some(token()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        }
        store_token_in(&fake, &file, &format!("{}x", token())).unwrap();
        assert_eq!(read_token_from(&fake, &file), Some(format!("{}x", token())), "a new token replaces the old");
        std::fs::write(&file, "not a token\n").unwrap();
        assert_eq!(read_token_from(&fake, &file), None, "only a token-shaped value is read back");
        forget_token_in(&fake, &file);
        assert!(!file.exists());
    }

    #[test]
    fn a_keychain_that_does_not_keep_the_token_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("evals-token");
        let fake = darkmux_platform::FakePlatform { secret_store: SecretStore::Keychain, ..Default::default() };
        let err = store_token_in(&fake, &file, &token()).unwrap_err().to_string();
        assert!(err.contains("did not keep the token"), "{err}");
        assert!(!file.exists(), "a keychain machine never falls back to a file");
        forget_token_in(&fake, &file);
    }

    #[test]
    fn the_keychain_account_is_a_plain_name() {
        assert_eq!(plain_user(Some("kain".into())), "kain");
        assert_eq!(plain_user(Some("a.b_c-1".into())), "a.b_c-1");
        assert_eq!(plain_user(Some("x\" -s y".into())), "darkmux");
        assert_eq!(plain_user(Some(String::new())), "darkmux");
        assert_eq!(plain_user(None), "darkmux");
    }

    /// A one-request HTTP server on this machine: answers `status` and
    /// `body`, and hands back the request it read.
    fn serve_once(status: u16, body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let site = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let handle = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            conn.set_read_timeout(Some(std::time::Duration::from_millis(500))).unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            while let Ok(n) = conn.read(&mut buf) {
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
            }
            let reply = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            conn.write_all(reply.as_bytes()).unwrap();
            String::from_utf8_lossy(&got).to_string()
        });
        (site, handle)
    }

    #[test]
    fn a_call_sends_the_token_as_a_header_and_reads_the_answer() {
        let (site, server) = serve_once(200, "{\"email\":\"me@example.com\"}");
        let answer = call(&format!("{site}/"), "GET", "/api/v1/account", &token(), None).unwrap();
        let request = server.join().unwrap();
        assert_eq!(answer.status, 200);
        assert_eq!(answer.body["email"], "me@example.com");
        assert!(request.starts_with("GET /api/v1/account "), "{request}");
        assert!(request.contains(&format!("Authorization: Bearer {}", token())), "{request}");
    }

    #[test]
    fn a_call_posts_the_body_file_and_keeps_a_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let body = dir.path().join("bundle \"1\".json");
        std::fs::write(&body, "{\"runs\":[]}").unwrap();
        let (site, server) = serve_once(422, "{\"error\":\"no runs\"}");
        let answer = call(&site, "POST", "/api/v1/uploads", &token(), Some(&body)).unwrap();
        let request = server.join().unwrap();
        assert_eq!(answer.refusal(), "no runs (HTTP 422)");
        assert!(request.starts_with("POST /api/v1/uploads "), "{request}");
        assert!(request.contains("Content-Type: application/json"), "{request}");
        assert!(request.ends_with("{\"runs\":[]}"), "{request}");
    }

    #[test]
    fn a_site_that_is_not_there_is_named() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let site = format!("http://127.0.0.1:{port}");
        let err = call(&site, "GET", "/api/v1/account", &token(), None).unwrap_err().to_string();
        assert!(err.contains("could not reach"), "{err}");
    }

    #[test]
    fn the_site_must_be_https_or_local() {
        let err = call("http://evals.example.com", "GET", "/api/v1/account", "dmx_evals_x", None).unwrap_err();
        assert!(err.to_string().contains("https"));
    }
}
