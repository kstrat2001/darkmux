//! Secure work submission between machines (#2916, stage 1).
//!
//! A machine takes work from another machine only when BOTH hold:
//!
//! 1. the request carries the **fleet token** (the serve token, #881: one
//!    shared secret, Keychain item `darkmux-serve-token` or
//!    `DARKMUX_SERVE_TOKEN`). It proves the caller is darkmux, not merely
//!    something running on an allowed machine: an agent container on an
//!    allowed laptop can reach overlay addresses, but not the Keychain.
//! 2. the connection comes from a **node on the receiver's allow-list**
//!    (`fleet.accept_work`), as the overlay network itself reports it
//!    ([`crate::identity`]). A leaked token is useless from a node that is not
//!    on the list.
//!
//! and the job fits that node's **scope**: a work-class profile the entry
//! lists (never one that resolves only to the machine's utility model,
//! #2914), and a `workdir` only when the entry grants `workspace` (#755).
//! Deny by default; fail closed (no identity answer = refused); every refusal
//! is answered at once, with the reason, never a silent timeout.
//!
//! The pure decision is two functions, [`admit`] (token + identity +
//! allow-list, which needs no request body) and [`check_scope`] (the job
//! against the admitted entry). The daemon's listener (`darkmux-serve`,
//! `fleet_listener.rs`) wires them to a socket; the sender side is
//! [`submit_work`].
//!
//! Work is never taken off Redis any more: the `darkmux:work` queue could not
//! say who wrote an entry, and every peer could write it (#2916). Redis stays
//! the shared observability stream.

use crate::job::{WorkJob, WORK_JOB_SCHEMA_VERSION};
use crate::identity::NodeIdentity;
use anyhow::{anyhow, Context, Result};
use darkmux_types::config::AcceptWorkEntry;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::Duration;

/// The listener's one route.
pub const SUBMISSION_PATH: &str = "/fleet/work";

/// The request body: the wire version, whether the sender waits for the
/// result, and the job. `schema` is read first, so a sender on another
/// version is told so by name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkSubmission {
    pub schema: String,
    /// `true`: the reply comes when the dispatch finishes and carries its
    /// result. `false`: the reply comes as soon as the job is accepted.
    pub wait: bool,
    pub job: WorkJob,
}

impl WorkSubmission {
    pub fn new(job: WorkJob, wait: bool) -> Self {
        Self { schema: WORK_JOB_SCHEMA_VERSION.to_string(), wait, job }
    }

    /// Parse a request body, checking the version before the job's shape.
    pub fn parse(body: &[u8]) -> std::result::Result<Self, Refusal> {
        let v: serde_json::Value = serde_json::from_slice(body)
            .map_err(|e| Refusal::BadRequest(format!("the body is not JSON: {e}")))?;
        match v.get("schema").and_then(|s| s.as_str()) {
            Some(s) if s == WORK_JOB_SCHEMA_VERSION => {}
            Some(other) => {
                return Err(Refusal::SchemaMismatch { got: other.to_string() });
            }
            None => return Err(Refusal::BadRequest("the body has no `schema`".into())),
        }
        let sub: WorkSubmission = serde_json::from_value(v)
            .map_err(|e| Refusal::BadRequest(format!("the job is malformed: {e}")))?;
        sub.job.validate().map_err(|e| Refusal::BadRequest(format!("{e:#}")))?;
        Ok(sub)
    }
}

/// The reply, for every outcome. `status` is `completed`, `accepted`,
/// `error` (the dispatch itself failed) or `refused`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubmissionReply {
    pub status: String,
    /// The machine that answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The profile the job ran on (after the receiver resolved it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    /// Why, for `refused` and `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// How the presented token compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenCheck {
    Match,
    /// Absent, malformed, or a different value.
    Mismatch,
    /// This machine has no fleet token, so it can verify nothing.
    NotConfigured,
}

/// Why a request was refused. Each variant maps to an HTTP status and a
/// sentence the SENDER's operator can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    NoTokenConfigured,
    Token,
    /// The provider could not answer (down, missing, timed out).
    IdentityUnavailable { provider: String, detail: String },
    /// No node on the overlay holds the connecting address.
    NotOnOverlay { provider: String, addr: String },
    /// A node, but not one on the allow-list.
    NotAllowed { node_name: String },
    /// The allow-list names the same node under several machine names.
    AmbiguousEntry { names: Vec<String> },
    /// Addressed to another machine name.
    Misaddressed { target: String },
    WorkspaceOutOfScope { peer: String },
    ProfileOutOfScope { peer: String, profile: String, allowed: Vec<String> },
    UtilityProfile { profile: String },
    NoWorkProfile { role: String, detail: String },
    SchemaMismatch { got: String },
    BadRequest(String),
    Busy { session_id: String },
}

impl Refusal {
    /// HTTP status for the reply.
    pub fn http_status(&self) -> u16 {
        match self {
            Refusal::Token => 401,
            Refusal::Misaddressed { .. } => 421,
            Refusal::SchemaMismatch { .. } | Refusal::BadRequest(_) => 400,
            Refusal::NoWorkProfile { .. } => 422,
            Refusal::Busy { .. } | Refusal::NoTokenConfigured | Refusal::IdentityUnavailable { .. } => 503,
            _ => 403,
        }
    }

    /// The sentence the sender prints. `receiver` is this machine's name.
    /// Never contains a node id, a token, or a hardware id.
    pub fn reason(&self, receiver: &str) -> String {
        match self {
            Refusal::NoTokenConfigured => format!(
                "{receiver} has no fleet token configured, so it takes no work from other machines \
                 (the fleet token is the serve token: Keychain item `darkmux-serve-token` or \
                 DARKMUX_SERVE_TOKEN, the same value on every machine)"
            ),
            Refusal::Token => format!(
                "{receiver} refused the request: the fleet token is missing or does not match \
                 (the serve token: Keychain item `darkmux-serve-token` or DARKMUX_SERVE_TOKEN; \
                 every machine in the fleet holds the same value)"
            ),
            Refusal::IdentityUnavailable { provider, detail } => format!(
                "{receiver} cannot tell which machine sent this request ({provider}: {detail}), \
                 so it refuses it"
            ),
            Refusal::NotOnOverlay { provider, addr } => format!(
                "{receiver} does not accept work from {addr}: the connection did not come from a \
                 node on the {provider} network (a LAN or public address is never accepted)"
            ),
            Refusal::NotAllowed { node_name } => format!(
                "{receiver} does not accept work from {node_name} (on {receiver}: `darkmux machine \
                 trust {node_name} --profiles <profile>`)"
            ),
            Refusal::AmbiguousEntry { names } => format!(
                "{receiver}'s allow-list names the sending machine more than once ({}); \
                 remove all but one with `darkmux machine untrust <name>` on {receiver}",
                names.join(", ")
            ),
            Refusal::Misaddressed { target } => format!(
                "this is {receiver}, not {target}: the sender's roster entry for {target} points at \
                 {receiver}'s address (fix it with `darkmux machine add {target} --address <its \
                 tailnet DNS name>` on the sender)"
            ),
            Refusal::WorkspaceOutOfScope { peer } => format!(
                "not in the allow-list scope: {receiver} does not let {peer} name a working \
                 directory (workspace: false)"
            ),
            Refusal::ProfileOutOfScope { peer, profile, allowed } => format!(
                "not in the allow-list scope: profile {profile} ({receiver} lets {peer} run: {})",
                if allowed.is_empty() { "nothing".to_string() } else { allowed.join(", ") }
            ),
            Refusal::UtilityProfile { profile } => format!(
                "profile {profile} resolves only to {receiver}'s utility model; utility work is never \
                 taken from another machine (#2914)"
            ),
            Refusal::NoWorkProfile { role, detail } => format!(
                "{receiver} cannot resolve a work profile for role {role}: {detail}"
            ),
            Refusal::SchemaMismatch { got } => format!(
                "{receiver} speaks work-submission schema v{WORK_JOB_SCHEMA_VERSION}, the sender sent \
                 v{got}; run the same darkmux version on both machines"
            ),
            Refusal::BadRequest(detail) => format!("{receiver} refused a malformed request: {detail}"),
            Refusal::Busy { session_id } => format!(
                "{receiver} is busy running {session_id}; it runs one submitted job at a time. \
                 Retry when that finishes"
            ),
        }
    }

    pub fn reply(&self, receiver: &str) -> SubmissionReply {
        SubmissionReply {
            status: "refused".into(),
            machine: Some(receiver.to_string()),
            reason: Some(self.reason(receiver)),
            ..Default::default()
        }
    }
}

/// A peer that passed [`admit`]: its allow-list name and scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admitted {
    /// The allow-list key (the peer's machine name).
    pub peer_name: String,
    pub profiles: Vec<String>,
    pub workspace: bool,
}

/// Token + network identity + allow-list. Checked in that order, and the
/// order is deliberate: the token comparison is constant-time and free,
/// while the identity lookup runs the provider's tool, so a caller without
/// the token never makes this machine spawn anything.
///
/// `identity` is the provider's answer for the connecting address:
/// `Ok(Some)` a node, `Ok(None)` no node holds it, `Err(detail)` the
/// provider could not answer. The last two refuse (fail closed). An entry
/// without a `node_id` never matches.
pub fn admit(
    token: TokenCheck,
    identity: impl FnOnce() -> std::result::Result<Option<NodeIdentity>, String>,
    provider: &str,
    peer_addr: IpAddr,
    allow: &BTreeMap<String, AcceptWorkEntry>,
) -> std::result::Result<Admitted, Refusal> {
    match token {
        TokenCheck::Match => {}
        TokenCheck::Mismatch => return Err(Refusal::Token),
        TokenCheck::NotConfigured => return Err(Refusal::NoTokenConfigured),
    }
    let node = match identity() {
        Ok(Some(node)) => node,
        Ok(None) => {
            return Err(Refusal::NotOnOverlay { provider: provider.to_string(), addr: peer_addr.to_canonical().to_string() })
        }
        Err(detail) => return Err(Refusal::IdentityUnavailable { provider: provider.to_string(), detail }),
    };
    let matches: Vec<(&String, &AcceptWorkEntry)> = allow
        .iter()
        .filter(|(_, e)| e.node_id.as_deref().is_some_and(|id| !id.is_empty() && id == node.node_id))
        .collect();
    match matches.as_slice() {
        [] => Err(Refusal::NotAllowed { node_name: node.name }),
        [(name, entry)] => Ok(Admitted {
            peer_name: (*name).clone(),
            profiles: entry.profiles.clone().unwrap_or_default(),
            workspace: entry.workspace.unwrap_or(false),
        }),
        many => Err(Refusal::AmbiguousEntry { names: many.iter().map(|(n, _)| (*n).clone()).collect() }),
    }
}

/// What the receiver's profile resolution made of a job's (role, profile).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileResolution {
    /// A work profile, by the name it resolved to.
    Work(String),
    /// The profile's only model is the machine's utility model.
    UtilityOnly(String),
    /// Nothing runnable resolves; the detail says why.
    Unresolved(String),
}

/// The job against the admitted peer's scope. Returns the profile to run
/// (always the RESOLVED one, so what runs is exactly what was checked).
pub fn check_scope(
    receiver: &str,
    admitted: &Admitted,
    job: &WorkJob,
    resolution: ProfileResolution,
) -> std::result::Result<String, Refusal> {
    if job.target_machine != receiver {
        return Err(Refusal::Misaddressed { target: job.target_machine.clone() });
    }
    if job.workdir.is_some() && !admitted.workspace {
        return Err(Refusal::WorkspaceOutOfScope { peer: admitted.peer_name.clone() });
    }
    let profile = match resolution {
        ProfileResolution::Work(p) => p,
        ProfileResolution::UtilityOnly(p) => return Err(Refusal::UtilityProfile { profile: p }),
        ProfileResolution::Unresolved(detail) => {
            return Err(Refusal::NoWorkProfile { role: job.role_id.clone(), detail })
        }
    };
    if !admitted.profiles.iter().any(|p| p == &profile) {
        return Err(Refusal::ProfileOutOfScope {
            peer: admitted.peer_name.clone(),
            profile,
            allowed: admitted.profiles.clone(),
        });
    }
    Ok(profile)
}

/// Resolve the profile a job would run on, against an already-loaded
/// registry: an explicit profile must exist here (never the #1054 fall to
/// `default_profile`, which would run something the sender did not ask
/// for), then the same `resolve_in` every dispatch uses, with the utility
/// model set aside. `mapped` is the role's `role_profiles` binding.
pub fn classify_profile(
    registry: &darkmux_types::ProfileRegistry,
    role: &darkmux_crew::types::Role,
    requested: Option<&str>,
    mapped: Option<String>,
    receiver: &str,
) -> ProfileResolution {
    use darkmux_crew::target::Resolution;
    if let Some(p) = requested {
        if let Some(msg) = registry.quarantine_error_for(p) {
            return ProfileResolution::Unresolved(msg);
        }
        if !registry.profiles.contains_key(p) {
            return ProfileResolution::Unresolved(format!("profile {p} is not defined on {receiver}"));
        }
    }
    let mapped = if requested.is_some() { None } else { mapped };
    match darkmux_crew::target::resolve_in(registry, role, requested, mapped, false) {
        Err(e) => ProfileResolution::Unresolved(format!("{e:#}")),
        Ok(Resolution::NoProfile) => ProfileResolution::Unresolved(format!(
            "no profile resolves (no `role_profiles` binding for the role and no `default_profile` on {receiver})"
        )),
        Ok(Resolution::NoModel { profile_name, profile, error }) => {
            let utility = registry.utility_model_id();
            if utility.is_some() && !profile.models.is_empty() && profile.models.iter().all(|m| Some(m.id.as_str()) == utility) {
                ProfileResolution::UtilityOnly(profile_name)
            } else {
                ProfileResolution::Unresolved(error)
            }
        }
        Ok(Resolution::Target(t)) => ProfileResolution::Work(t.profile_name),
    }
}

/// [`classify_profile`] against this machine's registry, role library and
/// `role_profiles` binding.
pub fn resolve_work_profile(role_id: &str, requested: Option<&str>, receiver: &str) -> ProfileResolution {
    let roles = match darkmux_crew::loader::load_roles() {
        Ok(r) => r,
        Err(e) => return ProfileResolution::Unresolved(format!("the role library could not be read: {e:#}")),
    };
    let Some(role) = roles.iter().find(|r| r.id == role_id) else {
        return ProfileResolution::Unresolved(format!("role {role_id} is not defined on {receiver}"));
    };
    let loaded = match darkmux_profiles::profiles::load_registry(None) {
        Ok(l) => l,
        Err(e) => return ProfileResolution::Unresolved(format!("the profile registry could not be read: {e:#}")),
    };
    let mapped = darkmux_types::config_access::role_profile(role_id);
    classify_profile(&loaded.registry, role, requested, mapped, receiver)
}

// ─── Sender side ──────────────────────────────────────────────────────────

/// The submission URL for a roster address: the roster entry names a HOST
/// (#2924: the machine's tailnet DNS name), and the fleet listener is that
/// host on the fleet's one submission port. Any port or scheme written in
/// the roster address belongs to the viewer daemon and is dropped.
pub fn submission_url(roster_address: &str, port: u16) -> Result<String> {
    let host = crate::roster::address_host(roster_address)
        .ok_or_else(|| anyhow!("roster address `{roster_address}` names no host"))?;
    let host = if host.contains(':') { format!("[{host}]") } else { host };
    Ok(format!("http://{host}:{port}{SUBMISSION_PATH}"))
}

/// Send one job to `url` with the fleet token. `Ok(reply)` for every answer
/// the receiver gave (including a refusal); `Err` only when no answer came
/// (unreachable, timed out, not darkmux).
pub fn post_submission(
    url: &str,
    token: &str,
    submission: &WorkSubmission,
    read_timeout: Duration,
) -> Result<(u16, SubmissionReply)> {
    let body = serde_json::to_string(submission).context("serializing the work submission")?;
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(read_timeout)
        .timeout_write(Duration::from_secs(30))
        .build();
    let resp = agent
        .post(url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .send_string(&body);
    let (code, resp) = match resp {
        Ok(r) => (r.status(), r),
        Err(ureq::Error::Status(code, r)) => (code, r),
        Err(ureq::Error::Transport(t)) => {
            return Err(anyhow!("no answer from {url}: {t}"));
        }
    };
    let text = resp.into_string().context("reading the submission reply")?;
    let reply: SubmissionReply = serde_json::from_str(&text).map_err(|_| {
        anyhow!(
            "{url} answered HTTP {code} but not as a darkmux fleet listener (is `fleet.listener` \
             enabled on that machine, on the same port as here?): {}",
            text.chars().take(200).collect::<String>()
        )
    })?;
    Ok((code, reply))
}

/// Submit `job` to the machine it is addressed to: look the machine up in
/// this machine's roster, dial its fleet listener, present the fleet token.
/// A refusal comes back as `Err` carrying the receiver's reason.
pub fn submit_work(job: WorkJob, wait: bool) -> Result<SubmissionReply> {
    job.validate().context("validating the job before it leaves")?;
    let target = job.target_machine.clone();
    let roster = crate::load_roster().context("reading the fleet roster")?;
    let entry = roster.machines.get(&target).ok_or_else(|| {
        anyhow!(
            "machine `{target}` is not in this machine's roster ({}); add it with \
             `darkmux machine add {target} --address <its tailnet DNS name>`",
            crate::roster_path().display()
        )
    })?;
    let token = darkmux_flow::serve_token().ok_or_else(|| {
        anyhow!(
            "no fleet token on this machine: submitting work to {target} needs the serve token \
             (Keychain item `darkmux-serve-token` or DARKMUX_SERVE_TOKEN), the same value {target} holds"
        )
    })?;
    let port = darkmux_types::config_access::fleet_listener_port();
    let url = submission_url(&entry.address, port)?;
    let read_timeout = if wait {
        Duration::from_secs(u64::from(job.timeout_seconds).saturating_add(120))
    } else {
        Duration::from_secs(60)
    };
    let submission = WorkSubmission::new(job, wait);
    let (code, reply) = post_submission(&url, token.expose_for_compare(), &submission, read_timeout)?;
    match reply.status.as_str() {
        "completed" | "accepted" => Ok(reply),
        "error" => Err(anyhow!(
            "{target} accepted the job but the dispatch failed: {}",
            reply.reason.as_deref().unwrap_or("no reason given")
        )),
        _ => Err(anyhow!(
            "{}",
            reply.reason.clone().unwrap_or_else(|| format!("{target} refused the job (HTTP {code})"))
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::test_node;

    fn entry(node_id: Option<&str>, profiles: &[&str], workspace: bool) -> AcceptWorkEntry {
        AcceptWorkEntry {
            node_id: node_id.map(str::to_string),
            profiles: Some(profiles.iter().map(|s| s.to_string()).collect()),
            workspace: Some(workspace),
            extras: Default::default(),
        }
    }

    fn allow() -> BTreeMap<String, AcceptWorkEntry> {
        let mut m = BTreeMap::new();
        m.insert("macbook-pro".to_string(), entry(Some("nLAPTOP"), &["host", "coder-studio"], false));
        // An entry that was never resolved matches nothing.
        m.insert("ghost".to_string(), entry(None, &["host"], true));
        m
    }

    fn job(profile: Option<&str>) -> WorkJob {
        WorkJob {
            target_machine: "studio".into(),
            role_id: "radio-host".into(),
            message: "hi".into(),
            session_id: "s-1".into(),
            profile: profile.map(str::to_string),
            workdir: None,
            phase_id: None,
            image: None,
            timeout_seconds: 60,
            published_at_unix_ms: 1,
            published_by_machine: Some("macbook-pro".into()),
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum Node {
        Allowed,
        Unknown,
        NotOnOverlay,
        Unresolvable,
    }

    #[derive(Clone, Copy, Debug)]
    enum Prof {
        InScope,
        OutOfScope,
        Utility,
    }

    /// The whole decision: admit, then scope.
    fn authorize(token: TokenCheck, node: Node, prof: Prof) -> std::result::Result<String, Refusal> {
        let peer: IpAddr = "100.64.0.7".parse().unwrap();
        let identity = move || match node {
            Node::Allowed => Ok(Some(test_node("nLAPTOP", "macbook-pro", "100.64.0.7"))),
            Node::Unknown => Ok(Some(test_node("nPHONE", "phone", "100.64.0.7"))),
            Node::NotOnOverlay => Ok(None),
            Node::Unresolvable => Err("daemon not running".to_string()),
        };
        let admitted = admit(token, identity, "tailscale", peer, &allow())?;
        let resolution = match prof {
            Prof::InScope => ProfileResolution::Work("host".into()),
            Prof::OutOfScope => ProfileResolution::Work("coder-big".into()),
            Prof::Utility => ProfileResolution::UtilityOnly("utility".into()),
        };
        check_scope("studio", &admitted, &job(None), resolution)
    }

    /// The auth matrix: token yes/no × node allowed/unknown/not-on-overlay/
    /// unresolvable × profile in/out of scope/utility. Exactly one cell of
    /// 48 runs work; every other cell names why not.
    #[test]
    fn auth_matrix() {
        let tokens = [TokenCheck::Match, TokenCheck::Mismatch, TokenCheck::NotConfigured];
        let nodes = [Node::Allowed, Node::Unknown, Node::NotOnOverlay, Node::Unresolvable];
        let profs = [Prof::InScope, Prof::OutOfScope, Prof::Utility];
        let mut ran = 0;
        for t in tokens {
            for n in nodes {
                for p in profs {
                    let got = authorize(t, n, p);
                    let want: std::result::Result<String, fn(&Refusal) -> bool> = match (t, n, p) {
                        (TokenCheck::Mismatch, _, _) => Err(|r| matches!(r, Refusal::Token)),
                        (TokenCheck::NotConfigured, _, _) => Err(|r| matches!(r, Refusal::NoTokenConfigured)),
                        (_, Node::Unknown, _) => Err(|r| matches!(r, Refusal::NotAllowed { node_name } if node_name == "phone")),
                        (_, Node::NotOnOverlay, _) => Err(|r| matches!(r, Refusal::NotOnOverlay { .. })),
                        (_, Node::Unresolvable, _) => Err(|r| matches!(r, Refusal::IdentityUnavailable { .. })),
                        (_, Node::Allowed, Prof::InScope) => Ok("host".to_string()),
                        (_, Node::Allowed, Prof::OutOfScope) => Err(|r| matches!(r, Refusal::ProfileOutOfScope { profile, .. } if profile == "coder-big")),
                        (_, Node::Allowed, Prof::Utility) => Err(|r| matches!(r, Refusal::UtilityProfile { .. })),
                    };
                    match (&got, want) {
                        (Ok(p), Ok(w)) => {
                            assert_eq!(p, &w);
                            ran += 1;
                        }
                        (Err(r), Err(pred)) => assert!(pred(r), "{t:?} {n:?} {p:?}: unexpected refusal {r:?}"),
                        _ => panic!("{t:?} {n:?} {p:?}: got {got:?}"),
                    }
                }
            }
        }
        assert_eq!(ran, 1, "exactly one cell may run work");
    }

    /// The token is checked before the identity lookup runs, so a caller
    /// without it never makes the receiver run the provider's tool.
    #[test]
    fn the_identity_lookup_never_runs_without_the_token() {
        let peer: IpAddr = "100.64.0.7".parse().unwrap();
        for t in [TokenCheck::Mismatch, TokenCheck::NotConfigured] {
            let r = admit(t, || panic!("identity looked up without a token"), "tailscale", peer, &allow());
            assert!(r.is_err());
        }
    }

    /// An entry with no node id never matches, even a node the provider
    /// reports with an empty id.
    #[test]
    fn an_unresolved_entry_matches_nothing() {
        let peer: IpAddr = "100.64.0.8".parse().unwrap();
        let mut n = test_node("", "ghost", "100.64.0.8");
        n.node_id = String::new();
        let r = admit(TokenCheck::Match, || Ok(Some(n)), "tailscale", peer, &allow());
        assert!(matches!(r, Err(Refusal::NotAllowed { .. })), "{r:?}");
    }

    #[test]
    fn two_names_for_one_node_are_refused_not_guessed() {
        let mut a = allow();
        a.insert("laptop".into(), entry(Some("nLAPTOP"), &["host"], true));
        let peer: IpAddr = "100.64.0.7".parse().unwrap();
        let r = admit(TokenCheck::Match, || Ok(Some(test_node("nLAPTOP", "macbook-pro", "100.64.0.7"))), "tailscale", peer, &a);
        assert!(matches!(r, Err(Refusal::AmbiguousEntry { ref names }) if names.len() == 2), "{r:?}");
    }

    #[test]
    fn scope_refuses_a_misaddressed_job_and_a_workdir_without_workspace() {
        let admitted = Admitted { peer_name: "macbook-pro".into(), profiles: vec!["host".into()], workspace: false };
        let mut j = job(None);
        j.target_machine = "mini".into();
        assert!(matches!(
            check_scope("studio", &admitted, &j, ProfileResolution::Work("host".into())),
            Err(Refusal::Misaddressed { .. })
        ));
        let mut j = job(None);
        j.workdir = Some("/tmp/x".into());
        assert!(matches!(
            check_scope("studio", &admitted, &j, ProfileResolution::Work("host".into())),
            Err(Refusal::WorkspaceOutOfScope { .. })
        ));
        let with_ws = Admitted { workspace: true, ..admitted.clone() };
        assert_eq!(check_scope("studio", &with_ws, &j, ProfileResolution::Work("host".into())).unwrap(), "host");
        assert!(matches!(
            check_scope("studio", &admitted, &job(None), ProfileResolution::Unresolved("x".into())),
            Err(Refusal::NoWorkProfile { .. })
        ));
    }

    /// The replies say what the operator needs, and never carry a node id.
    #[test]
    fn refusal_replies_name_the_reason() {
        let r = Refusal::NotAllowed { node_name: "macbook-pro".into() };
        assert!(r.reason("studio").starts_with("studio does not accept work from macbook-pro"), "{}", r.reason("studio"));
        assert_eq!(r.http_status(), 403);
        let r = Refusal::ProfileOutOfScope { peer: "macbook-pro".into(), profile: "x".into(), allowed: vec!["host".into()] };
        assert!(r.reason("studio").starts_with("not in the allow-list scope: profile x"), "{}", r.reason("studio"));
        assert_eq!(Refusal::Token.http_status(), 401);
        assert_eq!(Refusal::Busy { session_id: "s".into() }.http_status(), 503);
        let reply = Refusal::Token.reply("studio");
        assert_eq!(reply.status, "refused");
        assert!(!serde_json::to_string(&reply).unwrap().contains("nLAPTOP"));
    }

    #[test]
    fn the_version_is_checked_before_the_shape() {
        let good = serde_json::to_vec(&WorkSubmission::new(job(Some("host")), true)).unwrap();
        assert_eq!(WorkSubmission::parse(&good).unwrap().job.profile.as_deref(), Some("host"));
        let old = br#"{"schema":"4","record":{"role_id":"x","attempt":1}}"#;
        assert_eq!(WorkSubmission::parse(old).unwrap_err(), Refusal::SchemaMismatch { got: "4".into() });
        let mut v: serde_json::Value = serde_json::from_slice(&good).unwrap();
        v["job"]["role_id"] = "../etc".into();
        assert!(matches!(WorkSubmission::parse(&serde_json::to_vec(&v).unwrap()), Err(Refusal::BadRequest(_))));
    }

    #[test]
    fn the_submission_url_is_the_roster_host_on_the_fleet_port() {
        assert_eq!(submission_url("studio", 8766).unwrap(), "http://studio:8766/fleet/work");
        assert_eq!(submission_url("studio.tailnet-example.ts.net:8765", 8766).unwrap(), "http://studio.tailnet-example.ts.net:8766/fleet/work");
        assert_eq!(submission_url("https://studio.tailnet-example.ts.net/", 9000).unwrap(), "http://studio.tailnet-example.ts.net:9000/fleet/work");
        assert_eq!(submission_url("100.64.0.2", 8766).unwrap(), "http://100.64.0.2:8766/fleet/work");
        assert_eq!(submission_url("[fd7a::2]:8765", 8766).unwrap(), "http://[fd7a::2]:8766/fleet/work");
        assert_eq!(submission_url("fd7a::2", 8766).unwrap(), "http://[fd7a::2]:8766/fleet/work");
    }

    fn registry(json: &str) -> darkmux_types::ProfileRegistry {
        serde_json::from_str(json).unwrap()
    }

    fn role() -> darkmux_crew::types::Role {
        serde_json::from_value(serde_json::json!({
            "id": "radio-host", "description": "t",
            "tool_palette": {"allow": [], "deny": []},
            "escalation_contract": "bail-with-explanation"
        }))
        .unwrap()
    }

    /// A profile that lists only the utility model is utility work; an
    /// undefined requested profile never falls to the default.
    #[test]
    fn classify_profile_sets_utility_and_undefined_apart() {
        let reg = registry(
            r#"{"profiles":{
                "host":{"models":[{"id":"big","n_ctx":32000}]},
                "utility":{"models":[{"id":"small","n_ctx":8000}]}},
              "default_profile":"host",
              "internal":{"utility":"small"}}"#,
        );
        let r = role();
        assert_eq!(classify_profile(&reg, &r, Some("host"), None, "studio"), ProfileResolution::Work("host".into()));
        assert_eq!(classify_profile(&reg, &r, None, None, "studio"), ProfileResolution::Work("host".into()));
        assert_eq!(classify_profile(&reg, &r, Some("utility"), None, "studio"), ProfileResolution::UtilityOnly("utility".into()));
        assert!(matches!(
            classify_profile(&reg, &r, Some("nope"), None, "studio"),
            ProfileResolution::Unresolved(ref d) if d.contains("not defined on studio")
        ));
    }
}
