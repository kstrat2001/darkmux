//! (#3560) This machine's overlay network, through the network's own
//! command-line tool: where the tool is ([`tool_path`]), how it is run
//! ([`run_bounded`], every call wall-clock bounded), what it says about this
//! machine ([`local_network`]: installed, running, logged in, this machine's
//! DNS name, whether it can serve HTTPS), and the one serve entry darkmux
//! publishes ([`ServeEntry`]: `client serve`'s port over HTTPS).
//!
//! Every caller that runs the tool comes here: the fleet identity provider
//! (`identity::WhoisCli`), `darkmux doctor`'s tailnet viewer URL, and
//! `client serve`'s remote reach. No field, type or function is named for a
//! vendor (operator rule, #2916): the tool's name is spelled once, in
//! [`TOOL`], and its JSON field names are its own.
//!
//! **What darkmux may change.** Only the one entry [`ServeEntry`] describes,
//! and only through [`publish`] and [`remove`]. [`at_port`] tells darkmux's
//! entry from anything else at the same port by its whole shape, so an
//! operator's own entry (another handler, another target, Funnel on) is
//! never darkmux's, and [`remove`] is only ever called on an entry that
//! [`at_port`] answered [`AtPort::Exactly`] for. darkmux never runs the
//! tool's `reset`, which clears every entry.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The overlay network's command-line tool, the provider value
/// `"tailscale"` names (`fleet.identity.provider`, `clients.remote`).
pub const TOOL: &str = "tailscale";

/// The oldest tool whose `serve` takes `--bg`, `--https=<port>` and `off`,
/// the syntax [`ServeEntry::publish_args`] and [`ServeEntry::remove_args`]
/// use (the serve CLI was rewritten in 1.52). An older one is refused,
/// never driven with arguments it may read differently.
pub const MIN_SERVE_VERSION: (u32, u32) = (1, 52);

/// How long one call to the tool may take. Measured 2026-09-27 on the
/// laptop: ~25 ms per `status` call.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(3);

/// How long `serve --bg` may take: it returns once the entry is written, but
/// on a tailnet whose Serve is not enabled it waits for an admin, and that
/// wait is cut off here and read as a failure.
pub const PUBLISH_TIMEOUT: Duration = Duration::from_secs(8);

/// The tool, found the way every darkmux caller finds it: on `PATH`, else at
/// the platform's known install locations
/// ([`darkmux_platform::Platform::overlay_tool_locations`]), else the bare
/// name (so an error names the tool).
pub fn tool_path() -> String {
    let known = darkmux_platform::current().overlay_tool_locations();
    let known: Vec<&str> = known.iter().map(String::as_str).collect();
    find_tool(TOOL, &known, std::env::var_os("PATH").as_deref())
}

/// `name` found on `path_var`, else the first of `known` that exists, else
/// `name`.
pub fn find_tool(name: &str, known: &[&str], path_var: Option<&std::ffi::OsStr>) -> String {
    if let Some(p) = path_var {
        for dir in std::env::split_paths(p) {
            let cand = dir.join(name);
            if cand.is_file() {
                return cand.to_string_lossy().into_owned();
            }
        }
    }
    known
        .iter()
        .find(|k| std::path::Path::new(k).is_file())
        .map(|k| k.to_string())
        .unwrap_or_else(|| name.to_string())
}

/// What one bounded call returned: whether it exited successfully, its
/// stdout, and its stderr.
pub type CallOutput = (bool, Vec<u8>, String);

/// Run `bin args` with a wall-clock bound, reading stdout and stderr on
/// their own threads (a large `status` document must not fill the pipe
/// while we wait), stdin closed so the tool can never wait on a prompt. A
/// timeout kills the child and is an error; so is a tool that cannot be
/// started.
pub fn run_bounded(bin: &str, args: &[&str], timeout: Duration) -> Result<CallOutput> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("`{bin}` could not be started (is it installed, and on PATH? `fleet.identity.bin` names it explicitly)"))?;
    let out_t = drain(child.stdout.take().expect("piped stdout"));
    let err_t = drain(child.stderr.take().expect("piped stderr"));
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("`{bin}` did not answer within {:.1}s", timeout.as_secs_f64());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let stdout = out_t.join().unwrap_or_default();
    let stderr = String::from_utf8_lossy(&err_t.join().unwrap_or_default()).into_owned();
    Ok((status.success(), stdout, stderr))
}

/// Reads `r` to its end on a thread of its own.
fn drain(mut r: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = r.read_to_end(&mut b);
        b
    })
}

/// The first non-empty line of a tool's output, for a message.
pub fn first_line(s: &str) -> String {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or("no output").trim().to_string()
}

// ─── This machine on the network ─────────────────────────────────────────

/// Why the network cannot carry darkmux's serve entry here, and the fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unusable {
    pub reason: String,
    pub fix: String,
}

/// This machine on the overlay network, as the tool reports it. Read with
/// [`local_network`]: one `version` call and one `status --json` call, both
/// read-only.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LocalNetwork {
    /// The tool could be run.
    pub installed: bool,
    /// The tool's version (`1.102.4`), when it said.
    pub version: Option<String>,
    /// The tool's backend state (`Running`, `NeedsLogin`, `Stopped`, ...),
    /// when `status` answered.
    pub backend_state: Option<String>,
    /// This machine's DNS name on the network, without the trailing dot
    /// (`laptop.example.ts.net`), when it has one.
    pub dns_name: Option<String>,
    /// The network will provision an HTTPS certificate for that name.
    pub https: bool,
    /// Why `status` could not be read, when it could not.
    pub status_error: Option<String>,
}

impl LocalNetwork {
    /// The backend runs and is logged in.
    pub fn running(&self) -> bool {
        self.backend_state.as_deref() == Some("Running")
    }

    /// The machine is logged in to the network (any state but the two that
    /// wait on a login).
    pub fn logged_in(&self) -> bool {
        !matches!(self.backend_state.as_deref(), None | Some("NeedsLogin") | Some("NeedsMachineAuth"))
    }

    /// Why darkmux cannot publish an HTTPS serve entry here, or `None` when
    /// it can (the DNS name is then set).
    pub fn unusable(&self) -> Option<Unusable> {
        let u = |reason: String, fix: &str| Some(Unusable { reason, fix: fix.to_string() });
        if !self.installed {
            return u(format!("`{TOOL}` is not installed (or not on PATH)"), "install Tailscale and log in");
        }
        if let Some(why) = self.version_problem() {
            return u(why, "update Tailscale");
        }
        if let Some(e) = &self.status_error {
            return u(format!("`{TOOL} status` failed: {e}"), "start Tailscale");
        }
        if !self.logged_in() {
            return u("this machine is not logged in to Tailscale".to_string(), "log in: `tailscale up`, or the Tailscale app");
        }
        if !self.running() {
            let state = self.backend_state.as_deref().unwrap_or("unknown");
            return u(format!("Tailscale is not running (state: {state})"), "start Tailscale: `tailscale up`, or the Tailscale app");
        }
        if self.dns_name.is_none() {
            return u("this machine has no MagicDNS name".to_string(), "turn on MagicDNS in the Tailscale admin console (DNS page)");
        }
        if !self.https {
            return u(
                "HTTPS certificates are off for this tailnet".to_string(),
                "turn on HTTPS Certificates in the Tailscale admin console (DNS page)",
            );
        }
        None
    }

    /// Why this tool's version cannot run darkmux's serve commands.
    fn version_problem(&self) -> Option<String> {
        let Some(v) = &self.version else {
            return Some(format!("`{TOOL} version` did not say which version it is"));
        };
        match parse_version(v) {
            Some(have) if have >= MIN_SERVE_VERSION => None,
            Some(_) => Some(format!(
                "Tailscale {v} is older than {}.{}, whose `tailscale serve --bg` darkmux uses",
                MIN_SERVE_VERSION.0, MIN_SERVE_VERSION.1
            )),
            None => Some(format!("darkmux cannot read Tailscale's version (`{v}`)")),
        }
    }
}

/// `major.minor` from the first line of `version` output (`1.102.4`).
pub fn parse_version(raw: &str) -> Option<(u32, u32)> {
    let mut parts = raw.lines().next()?.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()?;
    Some((major, minor))
}

/// This machine on the network, from the tool at `bin` (read-only calls).
pub fn local_network(bin: &str, timeout: Duration) -> LocalNetwork {
    let Ok((ok, out, _)) = run_bounded(bin, &["version"], timeout) else {
        return LocalNetwork::default();
    };
    let mut net = LocalNetwork { installed: true, ..Default::default() };
    if ok {
        net.version = Some(first_line(&String::from_utf8_lossy(&out)));
    }
    match run_bounded(bin, &["status", "--json"], timeout) {
        Ok((true, out, _)) => read_status_into(&mut net, &out),
        Ok((false, _, err)) => net.status_error = Some(first_line(&err)),
        Err(e) => net.status_error = Some(format!("{e:#}")),
    }
    net
}

/// `status --json`'s `BackendState`, `Self.DNSName` and `CertDomains` into
/// `net`. Every other field is ignored.
pub fn read_status_into(net: &mut LocalNetwork, raw: &[u8]) {
    let v: serde_json::Value = match serde_json::from_slice(raw) {
        Ok(v) => v,
        Err(e) => {
            net.status_error = Some(format!("its status is not JSON ({e})"));
            return;
        }
    };
    net.backend_state = v.get("BackendState").and_then(|s| s.as_str()).map(str::to_string);
    net.dns_name = v
        .pointer("/Self/DNSName")
        .and_then(|s| s.as_str())
        .map(|s| s.trim_end_matches('.').to_string())
        .filter(|s| !s.is_empty());
    let certs = v.get("CertDomains").and_then(|c| c.as_array());
    net.https = match (&net.dns_name, certs) {
        (Some(name), Some(list)) => list.iter().filter_map(|d| d.as_str()).any(|d| d.trim_end_matches('.') == name),
        _ => false,
    };
}

// ─── The one serve entry darkmux publishes ───────────────────────────────

/// The serve entry darkmux publishes for `client serve`: HTTPS on `port` of
/// this machine's DNS name, with exactly one handler, at `/`, proxying to
/// `http://127.0.0.1:<port>` (the client listener, which binds loopback
/// only). The same port on both sides, so the URL names the port the
/// operator chose.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ServeEntry {
    /// This machine's DNS name, without the trailing dot.
    pub host: String,
    pub port: u16,
    /// What the handler proxies to.
    pub proxy: String,
}

impl ServeEntry {
    /// The entry for the client listener on loopback `port`.
    pub fn for_port(host: &str, port: u16) -> Self {
        ServeEntry { host: host.to_string(), port, proxy: format!("http://127.0.0.1:{port}") }
    }

    /// The URL another device on the network opens.
    pub fn url(&self) -> String {
        match self.port {
            443 => format!("https://{}/", self.host),
            p => format!("https://{}:{p}/", self.host),
        }
    }

    /// The key the serve configuration files the entry under.
    fn web_key(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// `serve --bg --https=<port> <proxy>`: publish in the background (the
    /// entry outlives the call), HTTPS on `port`, at `/`.
    pub fn publish_args(&self) -> Vec<String> {
        vec!["serve".into(), "--bg".into(), format!("--https={}", self.port), self.proxy.clone()]
    }

    /// `serve --https=<port> off`: removes the handler at `/` on that port
    /// (and the port, once it has no handler left). Nothing else.
    pub fn remove_args(&self) -> Vec<String> {
        vec!["serve".into(), format!("--https={}", self.port), "off".into()]
    }

    /// The command an operator runs to remove it by hand.
    pub fn remove_command(&self) -> String {
        format!("{TOOL} {}", self.remove_args().join(" "))
    }
}

/// What the serve configuration holds at an entry's port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtPort {
    /// Nothing is served on that port.
    Free,
    /// Exactly the entry: its one HTTPS port record, its one web key, one
    /// handler at `/` whose only field is the proxy target, and no Funnel.
    Exactly,
    /// Something else is on that port: another host's key, another handler
    /// or target, a TCP forward, Funnel. Never darkmux's to change.
    Other,
}

/// Where the serve configuration (`serve status --json`) stands at
/// `entry.port`. An empty configuration (`{}` or `null`) is [`AtPort::Free`];
/// a document that is not JSON is an error.
pub fn at_port(serve_status: &[u8], entry: &ServeEntry) -> Result<AtPort> {
    let v: serde_json::Value = serde_json::from_slice(serve_status).context("the serve status is not JSON")?;
    let port = entry.port.to_string();
    let suffix = format!(":{port}");
    let tcp = v.pointer(&format!("/TCP/{port}"));
    let web: Vec<(&String, &serde_json::Value)> = v
        .get("Web")
        .and_then(|w| w.as_object())
        .map(|w| w.iter().filter(|(k, _)| k.ends_with(&suffix)).collect())
        .unwrap_or_default();
    if tcp.is_none() && web.is_empty() {
        return Ok(AtPort::Free);
    }
    Ok(if is_exactly(&v, tcp, &web, entry) { AtPort::Exactly } else { AtPort::Other })
}

/// Whether what is at `entry.port` (its `TCP` record and its `Web` keys) is
/// exactly `entry` and nothing more, with Funnel off.
fn is_exactly(
    status: &serde_json::Value,
    tcp: Option<&serde_json::Value>,
    web: &[(&String, &serde_json::Value)],
    entry: &ServeEntry,
) -> bool {
    let key = entry.web_key();
    let funnel = status.pointer("/AllowFunnel").and_then(|f| f.get(&key)).and_then(|b| b.as_bool()).unwrap_or(false);
    let [(only_key, only)] = web else { return false };
    tcp == Some(&serde_json::json!({ "HTTPS": true }))
        && **only_key == key
        && only.get("Handlers") == Some(&serde_json::json!({ "/": { "Proxy": entry.proxy } }))
        && only.as_object().is_some_and(|o| o.len() == 1)
        && !funnel
}

/// `serve status --json` from the tool at `bin`.
pub fn serve_status(bin: &str, timeout: Duration) -> Result<Vec<u8>> {
    let (ok, out, err) = run_bounded(bin, &["serve", "status", "--json"], timeout)?;
    if !ok {
        bail!("`{TOOL} serve status` failed: {}", first_line(&err));
    }
    Ok(out)
}

/// Publishes `entry` ([`ServeEntry::publish_args`]). Its caller has seen the
/// port [`AtPort::Free`].
pub fn publish(bin: &str, entry: &ServeEntry, timeout: Duration) -> Result<()> {
    let args = entry.publish_args();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (ok, _, err) = run_bounded(bin, &args, timeout)?;
    if !ok {
        bail!("`{TOOL} serve` refused: {}", first_line(&err));
    }
    Ok(())
}

/// Removes `entry` ([`ServeEntry::remove_args`]). Its caller has seen it
/// [`AtPort::Exactly`].
pub fn remove(bin: &str, entry: &ServeEntry, timeout: Duration) -> Result<()> {
    let args = entry.remove_args();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (ok, _, err) = run_bounded(bin, &args, timeout)?;
    if !ok {
        bail!("`{TOOL} serve ... off` refused: {}", first_line(&err));
    }
    Ok(())
}

// ─── The record of what darkmux published ────────────────────────────────

/// The directory under the darkmux root holding one record per serve entry
/// darkmux published and has not yet removed (`<root>/client-remote/`).
pub const PUBLISHED_DIR: &str = "client-remote";

/// What darkmux keeps of a serve entry it published, written BEFORE the
/// entry is published and deleted once it is removed: a record still on
/// disk at the next start is an entry a killed `client serve` left. It is
/// the only thing that makes an entry darkmux's to remove: an entry with no
/// record is never removed, whatever it looks like.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PublishedRecord {
    pub entry: ServeEntry,
    /// The process that published it.
    pub pid: u32,
}

/// The record for `port` in `dir`.
pub fn record_path(dir: &std::path::Path, port: u16) -> std::path::PathBuf {
    dir.join(format!("{port}.json"))
}

/// The record at `path`, when there is a readable one.
pub fn read_record(path: &std::path::Path) -> Option<PublishedRecord> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Writes `rec` at `path` (private to the user, created that way),
/// replacing it in one rename, the shape `darkmux_types::engine_record::
/// write_in` writes its records in. A failed write leaves no temp file.
pub fn write_record(path: &std::path::Path, rec: &PublishedRecord) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(rec)?;
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp))
        .and_then(|mut f| f.write_all(&bytes).and_then(|()| f.sync_all()))
        .and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::Error::new(e).context(format!("writing {}", path.display())));
    }
    Ok(())
}

/// Every readable record in `dir`, by port.
pub fn records(dir: &std::path::Path) -> Vec<PublishedRecord> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<PublishedRecord> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| read_record(&e.path()))
        .collect();
    out.sort_by_key(|r| r.entry.port);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG: Duration = Duration::from_secs(60);

    /// A fake tool at `dir/name` running `body`.
    fn script(dir: &std::path::Path, name: &str, body: &str) -> String {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().into_owned()
    }

    /// A fake tool answering `version` with `version` and `status --json`
    /// with `status`.
    fn fake(dir: &std::path::Path, version: &str, status: &str) -> String {
        std::fs::write(dir.join("status.json"), status).unwrap();
        script(
            dir,
            "tool",
            &format!(
                "case \"$1\" in\n  version) echo '{version}'; echo '  tailscale commit: abc' ;;\n  status) cat \"{}/status.json\" ;;\n  *) exit 9 ;;\nesac",
                dir.display()
            ),
        )
    }

    const RUNNING: &str = r#"{"BackendState":"Running","Self":{"DNSName":"laptop.example.ts.net."},"CertDomains":["laptop.example.ts.net"]}"#;

    /// With no tool override, the tool is found on PATH, else at a known
    /// absolute path, else its bare name (the error then names it).
    #[test]
    fn the_tool_is_found_on_path_then_at_a_known_location() {
        let d = tempfile::TempDir::new().unwrap();
        let known = d.path().join("known-tool");
        std::fs::write(&known, "").unwrap();
        let known_s = known.to_string_lossy().into_owned();
        let empty_path = d.path().join("empty");
        std::fs::create_dir_all(&empty_path).unwrap();
        assert_eq!(find_tool("tailscale", &[&known_s], Some(empty_path.as_os_str())), known_s);
        let on_path = d.path().join("bin");
        std::fs::create_dir_all(&on_path).unwrap();
        std::fs::write(on_path.join("tailscale"), "").unwrap();
        assert_eq!(
            find_tool("tailscale", &[&known_s], Some(on_path.as_os_str())),
            on_path.join("tailscale").to_string_lossy()
        );
        assert_eq!(find_tool("tailscale", &["/nonexistent/x"], Some(empty_path.as_os_str())), "tailscale");
    }

    /// A hung tool is cut off and is an error, and its output pipes never
    /// hold the call open.
    #[test]
    fn a_hung_tool_is_cut_off() {
        let d = tempfile::TempDir::new().unwrap();
        let hang = script(d.path(), "hang", "sleep 30");
        let start = Instant::now();
        assert!(run_bounded(&hang, &[], Duration::from_millis(300)).is_err());
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn the_version_is_read_from_the_first_line() {
        assert_eq!(parse_version("1.102.4\n  tailscale commit: x"), Some((1, 102)));
        assert_eq!(parse_version("1.52.0-dev2023"), Some((1, 52)));
        assert_eq!(parse_version("1.8"), Some((1, 8)));
        assert_eq!(parse_version("garbage"), None);
        assert!((1, 102) >= MIN_SERVE_VERSION && (1, 51) < MIN_SERVE_VERSION);
    }

    /// A running, logged-in machine with a DNS name and HTTPS certificates
    /// is usable, and its name is read without the trailing dot.
    #[test]
    fn a_running_logged_in_machine_with_https_is_usable() {
        let d = tempfile::TempDir::new().unwrap();
        let net = local_network(&fake(d.path(), "1.102.4", RUNNING), LONG);
        assert!(net.installed && net.running() && net.logged_in() && net.https, "{net:?}");
        assert_eq!(net.dns_name.as_deref(), Some("laptop.example.ts.net"));
        assert_eq!(net.version.as_deref(), Some("1.102.4"));
        assert_eq!(net.unusable(), None);
    }

    /// Each way the network cannot carry the entry is named, with its fix,
    /// in the order an operator would fix them.
    #[test]
    fn each_unusable_network_says_why_and_the_fix() {
        let d = tempfile::TempDir::new().unwrap();
        let why = |version: &str, status: &str| {
            local_network(&fake(d.path(), version, status), LONG).unusable().map(|u| u.reason).unwrap_or_default()
        };
        let missing = local_network("/nonexistent/darkmux-test-no-such-tool", LONG);
        assert!(!missing.installed);
        assert!(missing.unusable().unwrap().reason.contains("not installed"), "{missing:?}");
        assert!(why("1.40.0", RUNNING).contains("older than 1.52"));
        assert!(why("weird", RUNNING).contains("cannot read Tailscale's version"));
        assert!(why("1.102.4", r#"{"BackendState":"NeedsLogin","Self":{"DNSName":""}}"#).contains("not logged in"));
        assert!(why("1.102.4", r#"{"BackendState":"Stopped","Self":{"DNSName":"a.b."}}"#).contains("not running (state: Stopped)"));
        assert!(why("1.102.4", r#"{"BackendState":"Running","Self":{"DNSName":""},"CertDomains":null}"#).contains("no MagicDNS name"));
        assert!(why("1.102.4", r#"{"BackendState":"Running","Self":{"DNSName":"a.b."},"CertDomains":null}"#).contains("HTTPS certificates are off"));
        assert!(why("1.102.4", r#"{"BackendState":"Running","Self":{"DNSName":"a.b."},"CertDomains":["other.b"]}"#).contains("HTTPS certificates are off"));
        assert!(why("1.102.4", "not json").contains("not JSON"));
        let failing = script(d.path(), "failing", "case \"$1\" in version) echo 1.102.4 ;; *) echo 'failed to connect to local tailscaled' >&2; exit 1 ;; esac");
        let net = local_network(&failing, LONG);
        assert!(net.unusable().unwrap().reason.contains("failed to connect"), "{net:?}");
    }

    fn entry() -> ServeEntry {
        ServeEntry::for_port("laptop.example.ts.net", 8767)
    }

    fn ours() -> serde_json::Value {
        serde_json::json!({
            "TCP": { "8767": { "HTTPS": true } },
            "Web": { "laptop.example.ts.net:8767": { "Handlers": { "/": { "Proxy": "http://127.0.0.1:8767" } } } }
        })
    }

    fn at(v: &serde_json::Value) -> AtPort {
        at_port(v.to_string().as_bytes(), &entry()).unwrap()
    }

    /// The URL and the two commands, exactly as the tool is driven.
    #[test]
    fn the_entry_is_published_and_removed_by_port() {
        let e = entry();
        assert_eq!(e.url(), "https://laptop.example.ts.net:8767/");
        assert_eq!(ServeEntry::for_port("host.example.ts.net", 443).url(), "https://host.example.ts.net/");
        assert_eq!(e.publish_args(), ["serve", "--bg", "--https=8767", "http://127.0.0.1:8767"]);
        assert_eq!(e.remove_args(), ["serve", "--https=8767", "off"]);
        assert_eq!(e.remove_command(), "tailscale serve --https=8767 off");
    }

    /// darkmux's entry is recognized only by its whole shape: anything an
    /// operator added, changed or put on the same port is someone else's.
    #[test]
    fn only_darkmuxs_exact_entry_is_its_own() {
        assert_eq!(at_port(b"{}", &entry()).unwrap(), AtPort::Free);
        assert_eq!(at_port(b"null", &entry()).unwrap(), AtPort::Free);
        assert!(at_port(b"nope", &entry()).is_err());
        assert_eq!(at(&ours()), AtPort::Exactly);
        // Other ports are not this port's business.
        let mut other_ports = ours();
        other_ports["TCP"]["80"] = serde_json::json!({ "HTTP": true });
        other_ports["Web"]["laptop.example.ts.net:80"] = serde_json::json!({ "Handlers": { "/": { "Proxy": "http://127.0.0.1:8765" } } });
        assert_eq!(at(&other_ports), AtPort::Exactly);
        let mut v = ours();
        v["Web"]["laptop.example.ts.net:8767"]["Handlers"]["/"]["Proxy"] = "http://127.0.0.1:9999".into();
        assert_eq!(at(&v), AtPort::Other, "another target");
        let mut v = ours();
        v["Web"]["laptop.example.ts.net:8767"]["Handlers"]["/api"] = serde_json::json!({ "Proxy": "http://127.0.0.1:1" });
        assert_eq!(at(&v), AtPort::Other, "a second handler the operator added");
        let mut v = ours();
        v["Web"]["laptop.example.ts.net:8767"]["Handlers"]["/"]["Path"] = "/srv".into();
        assert_eq!(at(&v), AtPort::Other, "a handler with more than a proxy");
        let mut v = ours();
        v["Web"]["laptop.example.ts.net:8767"]["Extra"] = true.into();
        assert_eq!(at(&v), AtPort::Other, "more than its handlers on the key");
        let mut v = ours();
        v["TCP"]["8767"] = serde_json::json!({ "HTTP": true });
        assert_eq!(at(&v), AtPort::Other, "plain HTTP on the port");
        let mut v = ours();
        v["TCP"]["8767"] = serde_json::json!({ "TCPForward": "127.0.0.1:8767" });
        v["Web"] = serde_json::json!({});
        assert_eq!(at(&v), AtPort::Other, "a TCP forward");
        let mut v = ours();
        v["AllowFunnel"] = serde_json::json!({ "laptop.example.ts.net:8767": true });
        assert_eq!(at(&v), AtPort::Other, "Funnel on");
        let mut v = ours();
        v["Web"]["mini.example.ts.net:8767"] = v["Web"]["laptop.example.ts.net:8767"].clone();
        assert_eq!(at(&v), AtPort::Other, "another host's key on the port");
        let renamed = ServeEntry::for_port("box.example.ts.net", 8767);
        assert_eq!(at_port(ours().to_string().as_bytes(), &renamed).unwrap(), AtPort::Other, "a different host is not this entry");
    }

    /// `publish` and `remove` run exactly the entry's commands, and a
    /// refusal is an error naming the tool's reason.
    #[test]
    fn publish_and_remove_run_the_entrys_commands() {
        let d = tempfile::TempDir::new().unwrap();
        let log = d.path().join("args.log");
        let ok = script(d.path(), "ok", &format!("echo \"$*\" >> \"{}\"", log.display()));
        publish(&ok, &entry(), LONG).unwrap();
        remove(&ok, &entry(), LONG).unwrap();
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "serve --bg --https=8767 http://127.0.0.1:8767\nserve --https=8767 off\n"
        );
        let no = script(d.path(), "no", "echo 'Access denied: serve config denied' >&2; exit 1");
        assert!(publish(&no, &entry(), LONG).unwrap_err().to_string().contains("Access denied"));
        assert!(remove(&no, &entry(), LONG).unwrap_err().to_string().contains("Access denied"));
        assert!(serve_status(&no, LONG).unwrap_err().to_string().contains("Access denied"));
    }

    /// A record survives a round trip, is private to the user, and a
    /// directory's records are listed by port, unreadable ones skipped.
    #[test]
    fn records_round_trip_private_and_listed_by_port() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::TempDir::new().unwrap();
        let dir = d.path().join(PUBLISHED_DIR);
        assert!(records(&dir).is_empty(), "no directory, no records");
        let a = PublishedRecord { entry: ServeEntry::for_port("host.example.ts.net", 9001), pid: 7 };
        let b = PublishedRecord { entry: ServeEntry::for_port("host.example.ts.net", 8767), pid: 8 };
        write_record(&record_path(&dir, 9001), &a).unwrap();
        write_record(&record_path(&dir, 8767), &b).unwrap();
        std::fs::write(dir.join("junk.json"), "not json").unwrap();
        assert_eq!(read_record(&record_path(&dir, 9001)), Some(a.clone()));
        assert_eq!(records(&dir), [b.clone(), a]);
        let mode = std::fs::metadata(record_path(&dir, 9001)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // A directory that cannot be made: an error naming the record, and
        // no temp file anywhere.
        let blocked = d.path().join("a-file");
        std::fs::write(&blocked, "").unwrap();
        let err = write_record(&record_path(&blocked.join("sub"), 1), &b).unwrap_err();
        assert!(format!("{err:#}").contains("1.json"), "{err:#}");
        let unwritable = d.path().join("ro");
        std::fs::create_dir_all(&unwritable).unwrap();
        std::fs::set_permissions(&unwritable, std::fs::Permissions::from_mode(0o500)).unwrap();
        assert!(write_record(&record_path(&unwritable, 2), &b).is_err());
        std::fs::set_permissions(&unwritable, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(std::fs::read_dir(&unwritable).unwrap().count(), 0, "nothing made");
        // A rename that fails (a non-empty directory where the record goes)
        // takes its temp file with it.
        let taken = d.path().join("taken");
        std::fs::create_dir_all(record_path(&taken, 3).join("inside")).unwrap();
        assert!(write_record(&record_path(&taken, 3), &b).is_err());
        let left: Vec<_> = std::fs::read_dir(&taken).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, [std::ffi::OsString::from("3.json")], "no temp file left");
    }
}
