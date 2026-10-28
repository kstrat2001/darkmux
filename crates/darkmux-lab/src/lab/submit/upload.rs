//! The upload token and the two calls `lab submit` makes to the site: who the
//! token belongs to (`GET /api/v1/account`) and the upload itself (`POST
//! /api/v1/uploads`).
//!
//! The token is made on the site's account page and handed to `darkmux lab
//! submit login`. It is kept where the platform keeps secrets (the login
//! keychain on macOS, a 0600 file elsewhere) and never on a command line: it
//! reaches `curl` in a config curl reads on stdin, so no file holds it while
//! a call is in flight.

use anyhow::{anyhow, bail, Context, Result};
use darkmux_platform::SecretStore;
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

/// A site's origin, the one thing its token is kept and sent by (#3400): the
/// scheme, the host (lowercase) and the port (the scheme's default dropped).
/// `http` or `https` only, a host of letter, digit and `-` labels or a
/// bracketed IPv6 address, and a numeric port. Anything else is refused, so
/// what reaches the keychain command and a file name is always plain, and
/// darkmux and curl never read two different hosts out of one address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    https: bool,
    host: String,
    port: Option<u16>,
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}://{}", self.scheme(), self.host)?;
        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }
        Ok(())
    }
}

impl Origin {
    /// The origin of a site address, or why it cannot be read as one.
    pub fn of(site: &str) -> Result<Origin> {
        split_site(site).map(|(origin, _)| origin)
    }

    fn scheme(&self) -> &'static str {
        if self.https {
            "https"
        } else {
            "http"
        }
    }

    /// evals.darkmux.com, whose token stays where the one token was kept
    /// before tokens had origins: no migration, no new login.
    fn is_default(&self) -> bool {
        Origin::of(super::DEFAULT_SITE).is_ok_and(|default| default == *self)
    }

    /// The keychain account: `$USER` for evals.darkmux.com (the account its
    /// token always had), `$USER@<origin>` for any other.
    fn keychain_account(&self, user: &str) -> String {
        if self.is_default() {
            user.to_string()
        } else {
            format!("{user}@{self}")
        }
    }

    /// The token file: `evals-token` under `root` for evals.darkmux.com,
    /// `evals-tokens/<scheme>_<host>[_<port>]` for any other (an IPv6
    /// host's `:` written `_`).
    fn token_file(&self, root: &Path) -> PathBuf {
        if self.is_default() {
            return root.join("evals-token");
        }
        let port = self.port.map(|p| format!("_{p}")).unwrap_or_default();
        root.join("evals-tokens").join(format!("{}_{}{port}", self.scheme(), self.host.replace(':', "_")))
    }
}

/// A site address split into its origin and its path (no trailing `/`).
/// The host and path are whitelists, so userinfo (`@`), a query, a fragment,
/// `%`, `\`, a space and anything not ASCII are refused wherever they are.
fn split_site(site: &str) -> Result<(Origin, String)> {
    let refuse = |why: &str| anyhow!("cannot use {site} as a site: {why}");
    let (scheme, rest) = site.split_once("://").ok_or_else(|| refuse("it does not start with `https://`"))?;
    let https = match scheme.to_ascii_lowercase().as_str() {
        "https" => true,
        "http" => false,
        _ => return Err(refuse("it must be https (or a preview on this machine)")),
    };
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    if !path.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~/".contains(&b)) {
        return Err(refuse("its path has characters other than letters, digits and `-._~/`"));
    }
    let (host, port) =
        split_authority(authority).ok_or_else(|| refuse("its host is not a host name or address with an optional numeric port"))?;
    let default_port = if https { 443 } else { 80 };
    Ok((Origin { https, host, port: port.filter(|p| *p != default_port) }, path.trim_end_matches('/').to_string()))
}

/// The host (lowercase; an IPv6 address bracketed, in its short form) and
/// the port of an authority, or `None` when it is not one. A name is at most
/// 253 characters of labels of at most 63 (DNS's limits), so no origin is
/// longer than 267 and the keychain command an origin reaches stays short.
fn split_authority(authority: &str) -> Option<(String, Option<u16>)> {
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (addr, after) = v6.split_once(']')?;
        (format!("[{}]", addr.parse::<std::net::Ipv6Addr>().ok()?), after)
    } else {
        let (host, after) = authority.split_at(authority.find(':').unwrap_or(authority.len()));
        (dns_name(host)?, after)
    };
    Some((host, port_after_host(port)?))
}

/// A host name lowercased, when it is one within DNS's limits.
fn dns_name(host: &str) -> Option<String> {
    let host = host.to_ascii_lowercase();
    let label_ok = |l: &str| (1..=63).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    (host.len() <= 253 && host.split('.').all(label_ok)).then_some(host)
}

/// What follows the host: nothing (`Some(None)`), or `:` and a non-zero
/// port of digits only. `None` when it is anything else.
fn port_after_host(rest: &str) -> Option<Option<u16>> {
    if rest.is_empty() {
        return Some(None);
    }
    let digits = rest.strip_prefix(':').filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))?;
    Some(Some(digits.parse::<u16>().ok().filter(|p| *p != 0)?))
}

/// An upload token and the origin it belongs to: [`call`] sends it there
/// and nowhere else.
pub struct Credential {
    origin: Origin,
    token: String,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Credential({}, <token>)", self.origin)
    }
}

impl Credential {
    /// A token pasted for `site` (at login), not yet checked with the site.
    pub fn pasted(site: &str, token: &str) -> Result<Credential> {
        if !is_token(token) {
            bail!("that is not an upload token (they start with {TOKEN_PREFIX}); make one on the site's account page");
        }
        Ok(Credential { origin: Origin::of(site)?, token: token.to_string() })
    }

    pub fn origin(&self) -> &Origin {
        &self.origin
    }
}

/// The keychain account's user: `$USER`.
fn user() -> String {
    plain_user(std::env::var("USER").ok())
}

/// A user name only when it is a plain name (it reaches the `security -i` command line).
fn plain_user(name: Option<String>) -> String {
    name.filter(|u| !u.is_empty() && u.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)))
        .unwrap_or_else(|| "darkmux".to_string())
}

/// The token stored for `site`'s origin. Refuses, naming the login to run,
/// when none is: another origin's token is never used in its place.
pub fn read_token(site: &str) -> Result<Credential> {
    TokenStore::current().credential(site)
}

/// Keeps `credential` under its origin, saying where. No other origin's token is touched.
pub fn store_token(credential: &Credential) -> Result<String> {
    TokenStore::current().store(credential)
}

/// Forgets `origin`'s token, and only that one.
pub fn forget_token(origin: &Origin) -> Result<()> {
    TokenStore::current().forget(origin);
    Ok(())
}

/// The login keychain as upload tokens use it: one generic password of item
/// [`TOKEN_ITEM`] per account. A trait so a test never reaches the real one.
trait Keychain {
    fn read(&self, account: &str) -> Option<String>;
    /// Can return `Ok` and keep nothing (`security -i`): the caller reads back.
    fn store(&self, account: &str, token: &str) -> Result<(), String>;
    fn forget(&self, account: &str);
}

/// This machine's login keychain, through its platform.
struct LoginKeychain;

impl Keychain for LoginKeychain {
    fn read(&self, account: &str) -> Option<String> {
        match darkmux_flow::read_keychain_bounded(TOKEN_ITEM, Some(account), std::time::Duration::from_secs(5)) {
            darkmux_flow::KeychainRead::Found(v) => Some(v),
            _ => None,
        }
    }

    fn store(&self, account: &str, token: &str) -> Result<(), String> {
        darkmux_platform::current().store_secret(TOKEN_ITEM, account, token)
    }

    fn forget(&self, account: &str) {
        darkmux_platform::current().forget_secret(TOKEN_ITEM, account)
    }
}

/// Where upload tokens are kept, one per origin: the keychain or files under
/// `root`, as the platform answers.
struct TokenStore<'a> {
    secret_store: SecretStore,
    keychain: &'a dyn Keychain,
    root: PathBuf,
    user: String,
}

impl TokenStore<'static> {
    /// This machine's: where its platform keeps secrets, `~/.darkmux`, `$USER`.
    fn current() -> TokenStore<'static> {
        TokenStore {
            secret_store: darkmux_platform::current().secret_store(),
            keychain: &LoginKeychain,
            root: crate::lab::paths::resolve(crate::lab::paths::ResolveScope::ForceUser).root,
            user: user(),
        }
    }
}

impl TokenStore<'_> {
    /// The token stored for `origin`, when one is and it is shaped like one.
    fn read(&self, origin: &Origin) -> Option<String> {
        let value = match self.secret_store {
            SecretStore::Keychain => self.keychain.read(&origin.keychain_account(&self.user))?,
            SecretStore::PrivateFile => std::fs::read_to_string(origin.token_file(&self.root)).ok()?,
        };
        let value = value.trim().to_string();
        is_token(&value).then_some(value)
    }

    fn credential(&self, site: &str) -> Result<Credential> {
        let origin = Origin::of(site)?;
        let Some(token) = self.read(&origin) else {
            bail!("no upload token for {origin}: run `darkmux lab submit login --site {origin}`");
        };
        Ok(Credential { origin, token })
    }

    fn store(&self, credential: &Credential) -> Result<String> {
        let Credential { origin, token } = credential;
        if !is_token(token) {
            bail!("that is not an upload token (they start with {TOKEN_PREFIX}); make one on the site's account page");
        }
        match self.secret_store {
            SecretStore::Keychain => {
                // `security -i` can exit 0 when its command failed: the read
                // back is the check. Its error is never shown: the command's
                // own words can echo the token.
                let _ = self.keychain.store(&origin.keychain_account(&self.user), token);
                if self.read(origin).as_deref() != Some(token.as_str()) {
                    bail!("the login keychain did not keep the token");
                }
                Ok("the login keychain".to_string())
            }
            SecretStore::PrivateFile => store_in_file(&origin.token_file(&self.root), token),
        }
    }

    fn forget(&self, origin: &Origin) {
        match self.secret_store {
            SecretStore::Keychain => self.keychain.forget(&origin.keychain_account(&self.user)),
            SecretStore::PrivateFile => {
                let _ = std::fs::remove_file(origin.token_file(&self.root));
            }
        }
    }
}

fn store_in_file(path: &Path, token: &str) -> Result<String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_private(path, format!("{token}\n").as_bytes())?;
    Ok(path.display().to_string())
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

/// One call to the site through `curl`, with `credential`'s token: sent
/// only when `site` is the origin the token belongs to. The token and URL
/// ride in a config curl reads on stdin, never in a file or the process
/// list; a body is read from `body_file`.
pub fn call(site: &str, method: &str, path: &str, credential: &Credential, body_file: Option<&Path>) -> Result<Answer> {
    let config = request_config(site, method, path, credential, body_file)?;
    let mut child = Command::new("curl")
        // `-q` first: no `~/.curlrc`, whose `verbose` would print the token.
        .args(["-q", "-sS", "--max-time", "120", "-w", "\n%{http_code}", "-K", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("running curl (darkmux uses it for HTTPS)")?;
    // Dropping stdin after the write closes it, so curl reads the whole config.
    child.stdin.take().expect("piped").write_all(config.as_bytes()).context("handing curl its config")?;
    let out = child.wait_with_output().context("running curl (darkmux uses it for HTTPS)")?;
    if !out.status.success() {
        bail!("could not reach {site}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    parse_answer(&String::from_utf8_lossy(&out.stdout))
}

/// The curl config for one call: refused, before anything is sent, unless
/// `site` reads as an origin and a path, is safe for a token
/// ([`site_is_safe`]), and is the origin `credential` was stored for. The URL
/// is rebuilt from what darkmux read, so curl dials the host the token
/// belongs to, and never through a proxy (`noproxy`: an `http_proxy` would
/// read a preview's token in clear, and any proxy is another host).
fn request_config(site: &str, method: &str, path: &str, credential: &Credential, body_file: Option<&Path>) -> Result<String> {
    let (origin, base) = split_site(site)?;
    // Judged on the address as rebuilt, which is what curl is handed.
    if !site_is_safe(&format!("{origin}{base}")) {
        bail!("the site must be https (or a preview on this machine): {site}");
    }
    if origin != credential.origin {
        bail!("the upload token for {} is never sent to {origin}", credential.origin);
    }
    let url = format!("{origin}{base}{path}");
    let quote = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let mut config = format!(
        "url = \"{}\"\nnoproxy = \"*\"\nrequest = \"{method}\"\nheader = \"Authorization: Bearer {}\"\nheader = \"Accept: application/json\"\n",
        quote(&url),
        quote(&credential.token)
    );
    if let Some(file) = body_file {
        config.push_str("header = \"Content-Type: application/json\"\n");
        config.push_str(&format!("data-binary = \"@{}\"\n", quote(&file.display().to_string())));
    }
    Ok(config)
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

    fn preview_token() -> String {
        format!("dmx_evals_{}", "cd34".repeat(8))
    }

    const PREVIEW: &str = "http://evals.localhost:3419";

    /// A keychain in memory: `keeps: false` is `security -i` exiting 0
    /// having kept nothing.
    struct FakeKeychain {
        items: std::cell::RefCell<std::collections::BTreeMap<String, String>>,
        keeps: bool,
    }

    impl FakeKeychain {
        fn new() -> Self {
            FakeKeychain { items: Default::default(), keeps: true }
        }
    }

    impl Keychain for FakeKeychain {
        fn read(&self, account: &str) -> Option<String> {
            self.items.borrow().get(account).cloned()
        }
        fn store(&self, account: &str, token: &str) -> Result<(), String> {
            if self.keeps {
                self.items.borrow_mut().insert(account.to_string(), token.to_string());
            }
            Ok(())
        }
        fn forget(&self, account: &str) {
            self.items.borrow_mut().remove(account);
        }
    }

    fn store_of<'a>(secret_store: SecretStore, keychain: &'a FakeKeychain, root: &Path) -> TokenStore<'a> {
        TokenStore { secret_store, keychain, root: root.to_path_buf(), user: "kain".to_string() }
    }

    /// Both ways a machine keeps tokens, each with a fake keychain and a temp root.
    fn each_store(check: impl Fn(&TokenStore<'_>, &FakeKeychain, &Path)) {
        for secret_store in [SecretStore::Keychain, SecretStore::PrivateFile] {
            let dir = tempfile::tempdir().unwrap();
            let keychain = FakeKeychain::new();
            check(&store_of(secret_store, &keychain, dir.path()), &keychain, dir.path());
        }
    }

    fn origin(site: &str) -> String {
        Origin::of(site).map(|o| o.to_string()).unwrap_or_else(|e| format!("refused: {e}"))
    }

    #[test]
    fn an_origin_is_scheme_host_and_port_normalized() {
        for same in ["https://evals.darkmux.com", "HTTPS://Evals.Darkmux.com/", "https://evals.darkmux.com:443", "https://evals.darkmux.com/x"] {
            assert_eq!(origin(same), "https://evals.darkmux.com", "{same}");
        }
        assert_eq!(origin("http://Evals.Localhost:3419/"), "http://evals.localhost:3419");
        assert_eq!(origin("http://localhost:80"), "http://localhost");
        assert_eq!(origin("https://evals.darkmux.com:8443"), "https://evals.darkmux.com:8443");
        assert_eq!(origin("http://[0:0::1]:3419"), "http://[::1]:3419");
        for other in ["https://evals.darkmux.com.evil.net", "https://evil.net/evals.darkmux.com", "http://evals.darkmux.com"] {
            assert_ne!(origin(other), "https://evals.darkmux.com", "{other}");
        }
        for odd in [
            "https://evals.darkmux.com@evil.net",
            "https://evals.darkmux.com\\@evil.net",
            "https://evals.darkmux.com%2eevil.net",
            "https://evals.darkmux.com#@evil.net",
            "https://evals.darkmux.com?x=1",
            "https://evals.darkmux.com:",
            "https://evals.darkmux.com:+443",
            "https://evals.darkmux.com:99999",
            "https://evals.darkmux.com.",
            "https://.evals.darkmux.com",
            "https://evals_darkmux.com",
            "https://evals.darkmux.com/a\"b",
            "https://evals.darkmux.com x",
            "https://",
            "evals.darkmux.com",
            "ftp://evals.darkmux.com",
            "http://[not-v6]:1",
        ] {
            assert!(origin(odd).starts_with("refused: cannot use"), "{odd}: {}", origin(odd));
        }
    }

    /// (#3400) A token stored for evals.darkmux.com is never handed to
    /// another site: there is no token for it, and the request that would
    /// carry evals.darkmux.com's is refused before curl runs.
    #[test]
    fn a_production_token_is_never_sent_to_another_site() {
        each_store(|store, _, _| {
            store.store(&Credential::pasted(super::super::DEFAULT_SITE, &token()).unwrap()).unwrap();
            for other in ["https://evil.net", "https://evals.darkmux.com.evil.net", "https://evil.net/evals.darkmux.com", PREVIEW] {
                let want = Origin::of(other).unwrap();
                let err = store.credential(other).unwrap_err().to_string();
                assert_eq!(err, format!("no upload token for {want}: run `darkmux lab submit login --site {want}`"));
            }
            let production = store.credential("HTTPS://Evals.Darkmux.com/").unwrap();
            let config = request_config("https://evals.darkmux.com", "GET", "/api/v1/account", &production, None).unwrap();
            assert!(config.contains(&format!("Authorization: Bearer {}", token())) && config.contains("url = \"https://evals.darkmux.com/api/v1/account\""), "{config}");
            for other in ["https://evil.net", "https://evals.darkmux.com.evil.net", "https://evil.net/evals.darkmux.com", PREVIEW] {
                let err = request_config(other, "POST", "/api/v1/uploads", &production, None).unwrap_err().to_string();
                assert_eq!(err, format!("the upload token for https://evals.darkmux.com is never sent to {}", Origin::of(other).unwrap()));
            }
        });
    }

    /// (#3400) Logging in to a preview keeps its token beside production's, which stays as it was.
    #[test]
    fn logging_in_to_a_preview_leaves_the_production_token() {
        each_store(|store, _, _| {
            store.store(&Credential::pasted(super::super::DEFAULT_SITE, &token()).unwrap()).unwrap();
            store.store(&Credential::pasted(PREVIEW, &preview_token()).unwrap()).unwrap();
            assert_eq!(store.read(&Origin::of(super::super::DEFAULT_SITE).unwrap()), Some(token()));
            assert_eq!(store.read(&Origin::of(PREVIEW).unwrap()), Some(preview_token()));
        });
    }

    /// (#3400) The token kept before tokens had origins (keychain account
    /// `$USER`, or the `evals-token` file) is evals.darkmux.com's, read with no
    /// migration, and no other origin's.
    #[test]
    fn the_token_kept_before_origins_is_evals_darkmux_coms() {
        let keychain = FakeKeychain::new();
        keychain.items.borrow_mut().insert("kain".into(), token());
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("evals-token"), format!("{}\n", token())).unwrap();
        for secret_store in [SecretStore::Keychain, SecretStore::PrivateFile] {
            let store = store_of(secret_store, &keychain, dir.path());
            assert_eq!(store.read(&Origin::of("https://evals.darkmux.com").unwrap()), Some(token()));
            assert_eq!(store.read(&Origin::of(PREVIEW).unwrap()), None);
        }
    }

    /// (#3400) Logout forgets one origin's token and leaves every other.
    #[test]
    fn logout_forgets_only_its_origin() {
        each_store(|store, _, _| {
            let production = Origin::of(super::super::DEFAULT_SITE).unwrap();
            let preview = Origin::of(PREVIEW).unwrap();
            store.store(&Credential::pasted(super::super::DEFAULT_SITE, &token()).unwrap()).unwrap();
            store.store(&Credential::pasted(PREVIEW, &preview_token()).unwrap()).unwrap();
            store.forget(&preview);
            assert_eq!(store.read(&preview), None);
            assert_eq!(store.read(&production), Some(token()));
            store.store(&Credential::pasted(PREVIEW, &preview_token()).unwrap()).unwrap();
            store.forget(&production);
            assert_eq!(store.read(&production), None);
            assert_eq!(store.read(&preview), Some(preview_token()));
        });
    }

    /// What reaches the `security` command line and a file name is plain:
    /// production keeps `$USER`, any other origin is `$USER@<origin>`.
    #[test]
    fn keychain_accounts_and_file_names_are_plain_and_one_per_origin() {
        let root = Path::new("/r");
        let names: Vec<(String, PathBuf)> = ["https://evals.darkmux.com", PREVIEW, "http://[::1]:3419", "https://evals.darkmux.com:8443", "http://evals.localhost:3419/x"]
            .iter()
            .map(|s| Origin::of(s).unwrap())
            .map(|o| (o.keychain_account("kain"), o.token_file(root)))
            .collect();
        assert_eq!(names[0], ("kain".to_string(), root.join("evals-token")));
        assert_eq!(names[1], ("kain@http://evals.localhost:3419".to_string(), root.join("evals-tokens/http_evals.localhost_3419")));
        assert_eq!(names[2], ("kain@http://[::1]:3419".to_string(), root.join("evals-tokens/http_[__1]_3419")));
        assert_eq!(names[3].1, root.join("evals-tokens/https_evals.darkmux.com_8443"));
        assert_eq!(names[4], names[1], "a path is not part of the origin");
        for (account, _) in &names {
            assert!(account.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-@:/[]".contains(&b)), "{account}");
        }
    }

    /// (#3400 review) A host is at most 253 characters of labels of at most
    /// 63 (DNS's limits), so an origin is at most 267 and the keychain
    /// command it reaches never nears `security -i`'s line cutoff.
    #[test]
    fn an_origin_is_bounded_by_dns_limits() {
        let label = "a".repeat(63);
        let longest = format!("{label}.{label}.{label}.{}", "b".repeat(61)); // 253
        assert_eq!(longest.len(), 253);
        let at_limit = Origin::of(&format!("https://{longest}:65535")).unwrap().to_string();
        assert_eq!(at_limit.len(), 267, "the longest origin there can be");
        assert!(origin(&format!("https://{longest}b")).starts_with("refused: cannot use"), "254 characters");
        assert!(origin(&format!("https://{}.localhost", "a".repeat(64))).starts_with("refused: cannot use"), "a 64-character label");
        let huge = format!("http://{}.localhost:18906", vec!["c".repeat(60); 66].join("."));
        assert!(origin(&huge).starts_with("refused: cannot use"), "the review's ~4000-character host");
    }

    /// (#3400 review) What a failed keychain store says never carries the
    /// keychain command's own words, which can echo the token.
    #[test]
    fn a_failed_keychain_store_never_echoes_the_command() {
        struct Leaky;
        impl Keychain for Leaky {
            fn read(&self, _: &str) -> Option<String> {
                None
            }
            fn store(&self, _: &str, token: &str) -> Result<(), String> {
                Err(format!("security: unknown command \"{}\"", &token[token.len() - 20..]))
            }
            fn forget(&self, _: &str) {}
        }
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore { secret_store: SecretStore::Keychain, keychain: &Leaky, root: dir.path().to_path_buf(), user: "kain".into() };
        let err = store.store(&Credential::pasted(PREVIEW, &token()).unwrap()).unwrap_err().to_string();
        assert_eq!(err, "the login keychain did not keep the token");
    }

    /// (#3400 review) The URL curl is handed is rebuilt from the origin that
    /// was checked, never the address as typed; a token never goes through a
    /// proxy (an `http_proxy` would see a preview's in clear).
    #[test]
    fn the_request_is_rebuilt_from_the_checked_origin_and_skips_proxies() {
        let production = Credential::pasted(super::super::DEFAULT_SITE, &token()).unwrap();
        let config = request_config("HTTPS://EVALS.DARKMUX.COM:00443/", "GET", "/api/v1/account", &production, None).unwrap();
        assert!(config.contains("url = \"https://evals.darkmux.com/api/v1/account\"\n"), "{config}");
        assert!(config.contains("noproxy = \"*\"\n"), "{config}");
        let preview = Credential::pasted(PREVIEW, &token()).unwrap();
        assert!(request_config(PREVIEW, "GET", "/api/v1/account", &preview, None).unwrap().contains("noproxy = \"*\"\n"));
    }

    /// (#3400 review) Safety is judged on the rebuilt address: an address
    /// [`site_is_safe`] refuses as typed is taken once it reads as a preview.
    #[test]
    fn safety_is_judged_on_the_rebuilt_address() {
        let typed = "HTTP://Evals.Localhost:3419/";
        assert!(!site_is_safe(typed), "as typed, the scheme is not `http://`");
        let preview = Credential::pasted(PREVIEW, &token()).unwrap();
        let config = request_config(typed, "POST", "/api/v1/uploads", &preview, None).unwrap();
        assert!(config.contains("url = \"http://evals.localhost:3419/api/v1/uploads\""), "{config}");
    }

    #[test]
    fn a_token_kept_in_a_file_is_private_read_back_and_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("nested");
        let keychain = FakeKeychain::new();
        let store = store_of(SecretStore::PrivateFile, &keychain, &root);
        let production = Origin::of(super::super::DEFAULT_SITE).unwrap();
        let file = root.join("evals-token");
        assert_eq!(store.read(&production), None);
        let malformed = Credential { origin: production.clone(), token: "dmx_evals_short".into() };
        assert!(store.store(&malformed).is_err(), "a malformed token is never stored");
        assert!(Credential::pasted(super::super::DEFAULT_SITE, "dmx_evals_short").is_err());
        let credential = Credential::pasted(super::super::DEFAULT_SITE, &token()).unwrap();
        assert_eq!(store.store(&credential).unwrap(), file.display().to_string());
        assert_eq!(store.read(&production), Some(token()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
            store.store(&Credential::pasted(PREVIEW, &preview_token()).unwrap()).unwrap();
            let preview_file = root.join("evals-tokens/http_evals.localhost_3419");
            assert_eq!(std::fs::metadata(&preview_file).unwrap().permissions().mode() & 0o777, 0o600);
        }
        store.store(&Credential::pasted(super::super::DEFAULT_SITE, &format!("{}x", token())).unwrap()).unwrap();
        assert_eq!(store.read(&production), Some(format!("{}x", token())), "a new token replaces the old");
        std::fs::write(&file, "not a token\n").unwrap();
        assert_eq!(store.read(&production), None, "only a token-shaped value is read back");
        store.forget(&production);
        assert!(!file.exists());
    }

    #[test]
    fn a_keychain_that_does_not_keep_the_token_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = FakeKeychain { keeps: false, ..FakeKeychain::new() };
        let store = store_of(SecretStore::Keychain, &keychain, dir.path());
        let err = store.store(&Credential::pasted(PREVIEW, &token()).unwrap()).unwrap_err().to_string();
        assert!(err.contains("did not keep the token"), "{err}");
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none(), "a keychain machine never falls back to a file");
    }

    /// The test token, pasted for `site`.
    fn cred(site: &str) -> Credential {
        Credential::pasted(site, &token()).unwrap()
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
        let answer = call(&format!("{site}/"), "GET", "/api/v1/account", &cred(&site), None).unwrap();
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
        let answer = call(&site, "POST", "/api/v1/uploads", &cred(&site), Some(&body)).unwrap();
        let request = server.join().unwrap();
        assert_eq!(answer.refusal(), "no runs (HTTP 422)");
        assert!(request.starts_with("POST /api/v1/uploads "), "{request}");
        assert!(request.contains("Content-Type: application/json"), "{request}");
        assert!(request.ends_with("{\"runs\":[]}"), "{request}");
    }

    /// The token reaches curl on stdin: no file holds it while the call is in
    /// flight, so one killed mid-upload leaves nothing behind.
    #[test]
    fn the_token_is_never_written_to_disk() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let site = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let secret = token();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = conn.read(&mut buf);
            // Mid-call: look for any small temp file that holds the token.
            // Regular files only: opening a FIFO a shared temp dir holds
            // (macOS's does) would block this thread, and the call with it.
            let holders: Vec<String> = std::fs::read_dir(std::env::temp_dir())
                .unwrap()
                .flatten()
                .filter(|e| e.metadata().is_ok_and(|m| m.is_file() && m.len() < 64 * 1024))
                .filter(|e| std::fs::read_to_string(e.path()).is_ok_and(|t| t.contains(&secret)))
                .map(|e| e.path().display().to_string())
                .collect();
            let reply = "HTTP/1.1 200 X\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
            conn.write_all(reply.as_bytes()).unwrap();
            holders
        });
        call(&site, "GET", "/api/v1/account", &cred(&site), None).unwrap();
        let holders = server.join().unwrap();
        assert!(holders.is_empty(), "the token was on disk mid-call: {holders:?}");
    }

    #[test]
    fn a_site_that_is_not_there_is_named() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let site = format!("http://127.0.0.1:{port}");
        let err = call(&site, "GET", "/api/v1/account", &cred(&site), None).unwrap_err().to_string();
        assert!(err.contains("could not reach"), "{err}");
    }

    #[test]
    fn the_site_must_be_https_or_local() {
        let site = "http://evals.example.invalid";
        let err = call(site, "GET", "/api/v1/account", &cred(site), None).unwrap_err();
        assert!(err.to_string().contains("https"));
    }
}
