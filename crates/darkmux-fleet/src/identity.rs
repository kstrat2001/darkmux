//! Network identity for fleet work submission (#2916).
//!
//! The fleet token proves a request came from darkmux; this module answers
//! the other half: WHICH machine is on the other end of a connection. The
//! answer comes from the overlay network the machines share, whose own
//! daemon binds every node's address to that node's key and can say "the
//! node at this address is X". darkmux never decides that from anything the
//! sender writes (not a header, not `published_by_machine`, not a
//! `machine_uid`, which every flow record carries and any peer can repeat).
//!
//! The source is an [`IdentityProvider`], chosen by the VALUE of
//! `fleet.identity.provider`. No field, type or function here is named for a
//! vendor (operator rule, #2916): the one implementation today,
//! [`WhoisCli`], drives the network's command-line tool (`tailscale whois
//! --json` / `tailscale status --json`), and the provider value `"tailscale"`
//! is the only place that name is spelled.
//!
//! Every answer is three-valued, and the middle one matters: `Ok(Some(node))`
//! (a node on the overlay is at that address), `Ok(None)` (no node is: the
//! connection did not arrive over the overlay, e.g. an office LAN), and `Err`
//! (the provider could not answer: its daemon is down, the tool is missing,
//! the call timed out). The submission gate refuses on both of the last two.

use anyhow::{anyhow, bail, Context, Result};
use std::io::Read;
use std::net::IpAddr;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The identity provider values this darkmux knows. `config set
/// fleet.identity.provider` refuses anything else; a hand-edited unknown
/// value resolves to no provider, which refuses every submission.
pub const KNOWN_IDENTITY_PROVIDERS: &[&str] = &["tailscale"];

/// How long one provider call may take before it counts as "cannot answer".
/// Measured 2026-09-27 on the laptop: ~25 ms per call (10 runs, 25-41 ms).
const PROVIDER_CALL_TIMEOUT: Duration = Duration::from_secs(3);

/// One node on the overlay network, as its provider reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeIdentity {
    /// The provider's STABLE id for the node. This is what an allow-list
    /// entry stores and matches on. Never printed by darkmux's own surfaces.
    pub node_id: String,
    /// The node's short network name (the first label of its DNS name,
    /// lowercased), e.g. `macbook-pro`. What refusals and `machine trust`
    /// name the node by.
    pub name: String,
    /// The full network DNS name without its trailing dot, when reported.
    pub dns_name: Option<String>,
    /// The host name the node's OS reports, when reported.
    pub host_name: Option<String>,
    /// The node's overlay addresses.
    pub addresses: Vec<IpAddr>,
    /// Whether the provider currently sees the node online (`None` when it
    /// does not say, as for the local node or a whois answer).
    pub online: Option<bool>,
}

impl NodeIdentity {
    /// Whether `query` names this node: its short name, its full DNS name
    /// (with or without a trailing dot), or its OS host name, compared
    /// case-insensitively.
    pub fn answers_to(&self, query: &str) -> bool {
        let q = query.trim().trim_end_matches('.').to_ascii_lowercase();
        if q.is_empty() {
            return false;
        }
        self.name == q
            || self.dns_name.as_deref().map(str::to_ascii_lowercase).as_deref() == Some(q.as_str())
            || self.host_name.as_deref().map(str::to_ascii_lowercase).as_deref() == Some(q.as_str())
    }
}

/// The source of network identity. See the module doc for the three-valued
/// answer [`IdentityProvider::identify`] gives.
pub trait IdentityProvider: Send + Sync {
    /// The provider value this was built from (`"tailscale"`), for messages.
    fn provider_name(&self) -> &str;
    /// Which node is at `peer`? `Ok(None)`: no node on the overlay has that
    /// address. `Err`: the provider could not answer.
    fn identify(&self, peer: IpAddr) -> Result<Option<NodeIdentity>>;
    /// This machine's own node (its overlay addresses are what the
    /// submission listener binds).
    fn local_node(&self) -> Result<NodeIdentity>;
    /// Every node this machine can see, itself included (`machine trust`
    /// resolves a name through this; doctor reports from it).
    fn nodes(&self) -> Result<Vec<NodeIdentity>>;
}

/// Build the provider a config value names. `bin` overrides the tool's
/// command (`fleet.identity.bin`). An unknown value is an error, and the
/// caller treats "no provider" as "refuse everything".
pub fn provider_for(value: &str, bin: Option<&str>) -> Result<Box<dyn IdentityProvider>> {
    match value.trim().to_ascii_lowercase().as_str() {
        "tailscale" => Ok(Box::new(WhoisCli::new("tailscale", bin.unwrap_or("tailscale")))),
        other => bail!(
            "unknown identity provider `{other}` (fleet.identity.provider) — valid: {}",
            KNOWN_IDENTITY_PROVIDERS.join(", ")
        ),
    }
}

/// The configured provider: `fleet.identity.provider` + `fleet.identity.bin`.
pub fn configured_provider() -> Result<Box<dyn IdentityProvider>> {
    let value = darkmux_types::config_access::fleet_identity_provider();
    let bin = darkmux_types::config_access::fleet_identity_bin();
    provider_for(&value, bin.as_deref())
}

/// A provider that asks the overlay network's command-line tool: `whois
/// --json <addr>` for a connection, `status --json` for the node list. The
/// JSON field names it reads are the tool's own.
pub struct WhoisCli {
    provider: String,
    bin: String,
}

impl WhoisCli {
    pub fn new(provider: &str, bin: &str) -> Self {
        Self { provider: provider.to_string(), bin: bin.to_string() }
    }

    fn run(&self, args: &[&str]) -> Result<(bool, Vec<u8>, String)> {
        run_with_timeout(&self.bin, args, PROVIDER_CALL_TIMEOUT)
            .with_context(|| format!("running `{} {}`", self.bin, args.join(" ")))
    }

    fn status(&self) -> Result<(NodeIdentity, Vec<NodeIdentity>)> {
        let (ok, stdout, stderr) = self.run(&["status", "--json"])?;
        if !ok {
            bail!("`{} status --json` failed: {}", self.bin, first_line(&stderr));
        }
        parse_status_json(&stdout)
    }
}

impl IdentityProvider for WhoisCli {
    fn provider_name(&self) -> &str {
        &self.provider
    }

    fn identify(&self, peer: IpAddr) -> Result<Option<NodeIdentity>> {
        let peer = peer.to_canonical();
        let addr = peer.to_string();
        let (ok, stdout, stderr) = self.run(&["whois", "--json", &addr])?;
        if !ok {
            // The tool's own wording for "no node has this address". Any
            // other failure (daemon down, logged out) is an error: the
            // provider could not answer, which is not the same as "no".
            if stderr.to_ascii_lowercase().contains("not found") {
                return Ok(None);
            }
            bail!("`{} whois` failed: {}", self.bin, first_line(&stderr));
        }
        let node = parse_whois_json(&stdout)?;
        // Defense in depth: an answer about some other address is no answer.
        if !node.addresses.contains(&peer) {
            bail!("the provider answered for a node that does not hold {addr}");
        }
        Ok(Some(node))
    }

    fn local_node(&self) -> Result<NodeIdentity> {
        Ok(self.status()?.0)
    }

    fn nodes(&self) -> Result<Vec<NodeIdentity>> {
        let (me, mut peers) = self.status()?;
        peers.insert(0, me);
        Ok(peers)
    }
}

fn first_line(s: &str) -> String {
    let line = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("no output").trim();
    // A timestamped log prefix (`2026/09/27 01:05:12 peer not found`) adds
    // nothing for the reader.
    line.to_string()
}

/// Run `bin args` with a wall-clock bound, reading stdout and stderr on
/// their own threads (a large `status` document must not fill the pipe
/// while we wait). Returns (exited successfully, stdout, stderr). A timeout
/// kills the child and is an error.
fn run_with_timeout(bin: &str, args: &[&str], timeout: Duration) -> Result<(bool, Vec<u8>, String)> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("`{bin}` could not be started (is it installed, and on PATH? `fleet.identity.bin` names it explicitly)"))?;
    let mut out = child.stdout.take().expect("piped stdout");
    let mut err = child.stderr.take().expect("piped stderr");
    let out_t = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out.read_to_end(&mut b);
        b
    });
    let err_t = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = err.read_to_end(&mut b);
        b
    });
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("`{bin}` did not answer within {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let stdout = out_t.join().unwrap_or_default();
    let stderr = String::from_utf8_lossy(&err_t.join().unwrap_or_default()).into_owned();
    Ok((status.success(), stdout, stderr))
}

/// The short network name from a DNS name: first label, lowercased.
fn short_name(dns: &str) -> String {
    dns.trim_end_matches('.').split('.').next().unwrap_or("").to_ascii_lowercase()
}

fn parse_addr(s: &str) -> Option<IpAddr> {
    // whois reports prefixes (`100.64.0.1/32`), status bare addresses.
    s.split('/').next()?.parse::<IpAddr>().ok().map(|ip| ip.to_canonical())
}

fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(str::to_string).filter(|s| !s.is_empty())
}

/// Parse `whois --json`: `{ "Node": { "StableID", "Name", "ComputedName",
/// "Addresses": ["<ip>/<bits>"], "Hostinfo": { "Hostname" } }, ... }`.
pub(crate) fn parse_whois_json(raw: &[u8]) -> Result<NodeIdentity> {
    let v: serde_json::Value = serde_json::from_slice(raw).context("parsing the provider's whois answer")?;
    let node = v.get("Node").ok_or_else(|| anyhow!("whois answer has no Node"))?;
    let node_id = str_field(node, "StableID").ok_or_else(|| anyhow!("whois answer has no stable node id"))?;
    let dns_name = str_field(node, "Name").map(|n| n.trim_end_matches('.').to_string());
    let name = dns_name
        .as_deref()
        .map(short_name)
        .filter(|s| !s.is_empty())
        .or_else(|| str_field(node, "ComputedName").map(|s| s.to_ascii_lowercase()))
        .ok_or_else(|| anyhow!("whois answer has no node name"))?;
    let addresses = node
        .get("Addresses")
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).filter_map(parse_addr).collect())
        .unwrap_or_default();
    let host_name = node.get("Hostinfo").and_then(|h| str_field(h, "Hostname"));
    Ok(NodeIdentity { node_id, name, dns_name, host_name, addresses, online: None })
}

fn status_node(v: &serde_json::Value) -> Option<NodeIdentity> {
    let node_id = str_field(v, "ID")?;
    let dns_name = str_field(v, "DNSName").map(|n| n.trim_end_matches('.').to_string());
    let host_name = str_field(v, "HostName");
    let name = dns_name
        .as_deref()
        .map(short_name)
        .filter(|s| !s.is_empty())
        .or_else(|| host_name.as_deref().map(|h| h.to_ascii_lowercase().replace(' ', "-")))?;
    let addresses = v
        .get("TailscaleIPs")
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).filter_map(parse_addr).collect())
        .unwrap_or_default();
    let online = v.get("Online").and_then(|o| o.as_bool());
    Some(NodeIdentity { node_id, name, dns_name, host_name, addresses, online })
}

/// Parse `status --json`: `{ "BackendState", "Self": {..}, "Peer": { k: {..} } }`,
/// each node carrying `ID` (the stable id), `DNSName`, `HostName`,
/// `TailscaleIPs`, `Online`. A backend that is not running is an error.
pub(crate) fn parse_status_json(raw: &[u8]) -> Result<(NodeIdentity, Vec<NodeIdentity>)> {
    let v: serde_json::Value = serde_json::from_slice(raw).context("parsing the provider's status answer")?;
    if let Some(state) = v.get("BackendState").and_then(|s| s.as_str()) {
        if state != "Running" {
            bail!("the overlay network is not running on this machine (state: {state})");
        }
    }
    let me = v
        .get("Self")
        .and_then(status_node)
        .ok_or_else(|| anyhow!("the provider's status has no usable Self node"))?;
    let mut peers: Vec<NodeIdentity> = v
        .get("Peer")
        .and_then(|p| p.as_object())
        .map(|m| m.values().filter_map(status_node).collect())
        .unwrap_or_default();
    peers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((me, peers))
}

/// A provider with a fixed answer table, for tests (and only tests: no
/// config value builds one).
#[cfg(any(test, feature = "test-support"))]
pub struct StaticIdentityProvider {
    pub local: NodeIdentity,
    pub peers: Vec<NodeIdentity>,
    /// When set, every call fails with this message (a provider that is down).
    pub down: Option<String>,
}

#[cfg(any(test, feature = "test-support"))]
impl IdentityProvider for StaticIdentityProvider {
    fn provider_name(&self) -> &str {
        "static"
    }
    fn identify(&self, peer: IpAddr) -> Result<Option<NodeIdentity>> {
        if let Some(d) = &self.down {
            bail!("{d}");
        }
        let peer = peer.to_canonical();
        Ok(std::iter::once(&self.local)
            .chain(self.peers.iter())
            .find(|n| n.addresses.contains(&peer))
            .cloned())
    }
    fn local_node(&self) -> Result<NodeIdentity> {
        if let Some(d) = &self.down {
            bail!("{d}");
        }
        Ok(self.local.clone())
    }
    fn nodes(&self) -> Result<Vec<NodeIdentity>> {
        if let Some(d) = &self.down {
            bail!("{d}");
        }
        Ok(std::iter::once(self.local.clone()).chain(self.peers.iter().cloned()).collect())
    }
}

/// A node for tests.
#[cfg(any(test, feature = "test-support"))]
pub fn test_node(node_id: &str, name: &str, addr: &str) -> NodeIdentity {
    NodeIdentity {
        node_id: node_id.to_string(),
        name: name.to_string(),
        dns_name: Some(format!("{name}.tailnet-example.ts.net")),
        host_name: None,
        addresses: vec![addr.parse().unwrap()],
        online: Some(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHOIS: &str = r#"{"Node":{"ID":123,"StableID":"nSTABLE1","Name":"laptop.tailnet-example.ts.net.",
        "ComputedName":"laptop","Addresses":["100.64.0.7/32","fd7a:115c:a1e0::7/128"],
        "Hostinfo":{"Hostname":"MacBook Pro"}},"UserProfile":{"LoginName":"x"},"CapMap":{}}"#;

    const STATUS: &str = r#"{"BackendState":"Running",
        "Self":{"ID":"nSELF","DNSName":"studio.tailnet-example.ts.net.","HostName":"Studio",
                "TailscaleIPs":["100.64.0.2","fd7a:115c:a1e0::2"],"Online":true},
        "Peer":{"nodekey:a":{"ID":"nSTABLE1","DNSName":"laptop.tailnet-example.ts.net.","HostName":"MacBook Pro",
                "TailscaleIPs":["100.64.0.7"],"Online":false},
                "nodekey:b":{"ID":"nPHONE","DNSName":"peer.tailnet-example.ts.net.","HostName":"phone","TailscaleIPs":["100.64.0.9"]}}}"#;

    #[test]
    fn whois_answer_parses_to_the_stable_id_and_short_name() {
        let n = parse_whois_json(WHOIS.as_bytes()).unwrap();
        assert_eq!(n.node_id, "nSTABLE1", "the STABLE id, not the numeric one");
        assert_eq!(n.name, "laptop");
        assert_eq!(n.dns_name.as_deref(), Some("laptop.tailnet-example.ts.net"));
        assert_eq!(n.host_name.as_deref(), Some("MacBook Pro"));
        assert!(n.addresses.contains(&"100.64.0.7".parse().unwrap()));
        assert!(n.addresses.contains(&"fd7a:115c:a1e0::7".parse().unwrap()));
    }

    #[test]
    fn whois_answer_without_a_stable_id_is_an_error() {
        let raw = r#"{"Node":{"ID":1,"Name":"host.tailnet-example.ts.net.","Addresses":[]}}"#;
        assert!(parse_whois_json(raw.as_bytes()).is_err());
    }

    #[test]
    fn status_parses_self_and_peers() {
        let (me, peers) = parse_status_json(STATUS.as_bytes()).unwrap();
        assert_eq!(me.name, "studio");
        assert_eq!(me.addresses[0], "100.64.0.2".parse::<IpAddr>().unwrap());
        assert_eq!(peers.len(), 2);
        let mbp = peers.iter().find(|p| p.node_id == "nSTABLE1").unwrap();
        assert_eq!(mbp.online, Some(false));
        assert!(mbp.answers_to("laptop"));
        assert!(mbp.answers_to("MacBook Pro"), "the OS host name, case-insensitively");
        assert!(mbp.answers_to("laptop.tailnet-example.ts.net."));
        assert!(!mbp.answers_to("lap"));
    }

    #[test]
    fn a_stopped_backend_is_an_error_not_an_empty_network() {
        let raw = r#"{"BackendState":"Stopped","Self":{"ID":"n","DNSName":"a.b.","TailscaleIPs":[]}}"#;
        let err = parse_status_json(raw.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("not running"), "{err}");
    }

    #[test]
    fn an_unknown_provider_value_builds_nothing() {
        let err = provider_for("wireguard-magic", None).err().unwrap();
        assert!(err.to_string().contains("unknown identity provider"), "{err}");
        assert!(provider_for("tailscale", None).is_ok());
        assert!(provider_for(" Tailscale ", None).is_ok(), "the value is compared trimmed and case-insensitively");
    }

    /// A missing tool is an error ("cannot answer"), never `Ok(None)`.
    #[test]
    fn a_missing_tool_cannot_answer() {
        let p = WhoisCli::new("tailscale", "/nonexistent/darkmux-test-no-such-tool");
        assert!(p.identify("100.64.0.7".parse().unwrap()).is_err());
        assert!(p.local_node().is_err());
    }

    /// The tool's "not found" is `Ok(None)`; any other failure is `Err`.
    #[test]
    fn whois_not_found_is_none_and_other_failures_are_errors() {
        let dir = tempfile::TempDir::new().unwrap();
        let script = |name: &str, body: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p.to_string_lossy().into_owned()
        };
        let nf = script("nf", "echo '2026/09/27 01:05:12 peer not found' >&2; exit 1");
        assert_eq!(WhoisCli::new("t", &nf).identify("100.64.0.7".parse().unwrap()).unwrap(), None);
        let down = script("down", "echo 'failed to connect to local daemon' >&2; exit 1");
        assert!(WhoisCli::new("t", &down).identify("100.64.0.7".parse().unwrap()).is_err());
        // An answer for a different address is not an answer.
        let other = script("other", &format!("cat <<'EOF'\n{WHOIS}\nEOF"));
        assert!(WhoisCli::new("t", &other).identify("100.64.0.99".parse().unwrap()).is_err());
        assert_eq!(
            WhoisCli::new("t", &other).identify("100.64.0.7".parse().unwrap()).unwrap().unwrap().node_id,
            "nSTABLE1"
        );
        // A hung tool is cut off and is an error.
        let hang = script("hang", "sleep 30");
        let start = Instant::now();
        let r = run_with_timeout(&hang, &[], Duration::from_millis(300));
        assert!(r.is_err());
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
