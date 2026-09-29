//! Secure work submission between machines (#2916, stage 1).
//!
//! A machine takes work from another machine only when BOTH hold:
//!
//! 1. the request carries the **fleet token** (the serve token, #881: one
//!    shared secret, Keychain item `darkmux-serve-token` or
//!    `DARKMUX_SERVE_TOKEN`). The check is that the caller HOLDS the shared
//!    token. That keeps out something on an allowed machine that cannot
//!    read it (an agent container, which reaches overlay addresses but not
//!    the Keychain); any process running as the operator's user can read it
//!    (`security find-generic-password`), and `machine list --deep` sends it
//!    to every roster peer.
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
use darkmux_types::session_id::SessionId;
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

/// What a reply says happened. Parsed once, at the wire (serde); every
/// consumer matches on it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReplyStatus {
    /// The job ran; the reply carries its exit code and output.
    Completed,
    /// The job was accepted and is running; the sender is not waiting.
    Accepted,
    /// (#2916 stage 2) The seat is busy and the receiver's
    /// `fleet.busy_policy` is `queue`: the job waits for its seat.
    Queued,
    /// The job was accepted but the dispatch itself failed.
    Error,
    /// The job was not run; `reason` says why.
    Refused,
}

/// The reply, for every outcome.
///
/// A reply body is newline-delimited JSON, one reply per line. It is one
/// line for every answer except a job the sender waits on that was queued:
/// that body carries a `queued` line when it is queued (again at every
/// heartbeat while it waits), then the final line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubmissionReply {
    pub status: ReplyStatus,
    /// The machine that answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// The receiver's relay session for the job. (#2916 re-review C6) The
    /// echoed id is printed and stored, so only a well-formed one is kept:
    /// one outside the session grammar reads as none, and the reply itself
    /// still reads.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "well_formed_session")]
    pub session_id: Option<SessionId>,
    /// The profile the job ran on (after the receiver resolved it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    /// Why, for `refused` and `error` (and what a `queued` job waits for).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A reply's `session_id`, kept only when it parses in the session grammar.
fn well_formed_session<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<SessionId>, D::Error> {
    let raw = Option::<String>::deserialize(d)?;
    Ok(raw.and_then(|wire| SessionId::parse(&wire).ok()))
}

impl SubmissionReply {
    /// A reply with `status` and nothing else set.
    pub fn of(status: ReplyStatus) -> Self {
        Self {
            status,
            machine: None,
            session_id: None,
            profile: None,
            exit_code: None,
            stdout: None,
            stderr: None,
            reason: None,
        }
    }
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
    RoleOutOfScope { peer: String, role: String, allowed: Vec<String> },
    ImageOutOfScope { peer: String, image: String },
    /// The connection came from THIS machine's own node.
    FromSelf,
    ProfileOutOfScope { peer: String, profile: String, allowed: Vec<String> },
    UtilityProfile { profile: String },
    NoWorkProfile { role: String, detail: String },
    SchemaMismatch { got: String },
    BadRequest(String),
    /// (#2916 stage 2) The job's seat is busy and this machine refuses
    /// (`fleet.busy_policy = refuse`). `what` names what is running.
    Busy { what: String },
    /// (#2916 stage 2) The seat is busy, this machine queues, and the peer
    /// already has as many jobs queued here as it may.
    QueueFull { peer: String, what: String },
    /// (#2916 stage 2 review M1) While the job was queued, the profile it
    /// resolved to changed to one on a different seat than the one it
    /// waited for.
    SeatChanged { profile: String },
    /// (#2916 round 3 C5) One node already has its cap of requests in flight.
    TooManyAtOnce { peer: String },
    /// (#2947) This machine's own config has an unregistered value in an
    /// enum-valued setting a dispatch reads; the job would refuse at its
    /// own preflight, so it is refused before it is accepted. `detail` is
    /// the preflight refusal (setting key, the bad value, where it was
    /// set, the valid values): config values, never secrets.
    BadConfig { detail: String },
}

impl Refusal {
    /// HTTP status for the reply.
    pub fn http_status(&self) -> u16 {
        match self {
            Refusal::Token => 401,
            Refusal::Misaddressed { .. } => 421,
            Refusal::SchemaMismatch { .. } | Refusal::BadRequest(_) => 400,
            Refusal::NoWorkProfile { .. } => 422,
            Refusal::Busy { .. }
            | Refusal::QueueFull { .. }
            | Refusal::SeatChanged { .. }
            | Refusal::TooManyAtOnce { .. }
            | Refusal::NoTokenConfigured
            | Refusal::IdentityUnavailable { .. }
            | Refusal::BadConfig { .. } => 503,
            _ => 403,
        }
    }

    /// The sentence the sender prints. `receiver` is this machine's name.
    /// Never contains a node id, a token, or a hardware id.
    pub fn reason(&self, receiver: &str) -> String {
        match self {
            Refusal::NoTokenConfigured => format!(
                "{receiver} has no fleet token configured, so it takes no work from other machines \
                 (the fleet token is the serve token: Keychain item `darkmux-serve-token`, read only \
                 when `serve.token_keychain` is on, or DARKMUX_SERVE_TOKEN; the same value on every machine)"
            ),
            Refusal::Token => format!(
                "{receiver} refused the request: the fleet token is missing or does not match \
                 (the serve token: Keychain item `darkmux-serve-token`, read only when \
                 `serve.token_keychain` is on, or DARKMUX_SERVE_TOKEN; every machine in the fleet holds \
                 the same value)"
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
                "this is {receiver}, not {target}: the address the sender used for {target} reaches \
                 {receiver}. On the sender, `darkmux machine list` shows the entry; point {target} at \
                 {target}'s own tailnet DNS name, or send the job to {receiver} by that name"
            ),
            Refusal::RoleOutOfScope { peer, role, allowed } => format!(
                "not in the allow-list scope: role {role} ({receiver} lets {peer} run roles: {})",
                if allowed.is_empty() { "none".to_string() } else { allowed.join(", ") }
            ),
            Refusal::ImageOutOfScope { peer, image } => format!(
                "not in the allow-list scope: image {image} ({receiver} lets {peer} use only darkmux's own \
                 runtime image unless the entry lists others)"
            ),
            Refusal::FromSelf => format!(
                "{receiver} does not take fleet work from itself; run it locally (address the \
                 profile without `@{receiver}`)"
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
            Refusal::TooManyAtOnce { peer } => format!(
                "{receiver} is already handling as many requests from {peer} as it takes at once; \
                 retry when one finishes"
            ),
            Refusal::Busy { what } => format!(
                "busy: {receiver} is running other work on that seat ({what}). Retry when it \
                 finishes ({receiver}'s `fleet.busy_policy` is `refuse`)"
            ),
            Refusal::SeatChanged { profile } => format!(
                "{receiver}'s profile {profile} now runs on a different model than the one this job \
                 waited for, so the queued job was not run; send it again"
            ),
            Refusal::QueueFull { peer, what } => format!(
                "busy: {receiver} is running other work on that seat ({what}), and {peer} already \
                 has as many jobs queued on {receiver} as it may. Retry when one finishes"
            ),
            Refusal::BadConfig { detail } => format!(
                "{receiver} cannot run work until its own config is fixed (on {receiver}: \
                 `darkmux doctor`, then restart `darkmux serve`, which reads config once at \
                 start). {detail}"
            ),
        }
    }

    pub fn reply(&self, receiver: &str) -> SubmissionReply {
        SubmissionReply {
            machine: Some(receiver.to_string()),
            reason: Some(self.reason(receiver)),
            ..SubmissionReply::of(ReplyStatus::Refused)
        }
    }
}

/// A peer that passed [`admit`]: its allow-list name and scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admitted {
    /// The allow-list key (the peer's machine name).
    pub peer_name: String,
    /// (#2916 stage 2) The node the network named, so a queued job can
    /// require the SAME node when it passes [`admit`] again as its seat
    /// frees. Never printed.
    pub node_id: String,
    pub profiles: Vec<String>,
    pub roles: Vec<String>,
    pub images: Vec<String>,
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
///
/// `allow` reads the allow-list, and is called only AFTER the identity
/// lookup (which runs the provider's tool, up to its 3 s bound), so an
/// `untrust` that lands while the provider answers is still seen. An
/// allow-list that cannot be read refuses everything.
pub fn admit(
    token: TokenCheck,
    identity: impl FnOnce() -> std::result::Result<Option<NodeIdentity>, String>,
    provider: &str,
    peer_addr: IpAddr,
    local_node_id: Option<&str>,
    allow: impl FnOnce() -> std::result::Result<BTreeMap<String, AcceptWorkEntry>, String>,
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
    // A machine never takes fleet work from its own node: an allow-list
    // entry naming itself (by mistake) must not turn a local process into a
    // "trusted peer" that skips the local dispatch path.
    if local_node_id.is_some_and(|me| !me.is_empty() && me == node.node_id) {
        return Err(Refusal::FromSelf);
    }
    let allow = allow().map_err(|e| {
        Refusal::BadRequest(format!("this machine's allow-list cannot be read ({e}); refusing everything"))
    })?;
    match_entry(&node.node_id, &node.name, &allow)
}

/// The allow-list entry for the node `node_id` (named `node_name` on the
/// network): exactly one entry must carry that node id. [`admit`]'s match
/// rule.
fn match_entry(
    node_id: &str,
    node_name: &str,
    allow: &BTreeMap<String, AcceptWorkEntry>,
) -> std::result::Result<Admitted, Refusal> {
    let matches: Vec<(&String, &AcceptWorkEntry)> = allow
        .iter()
        .filter(|(_, e)| e.node_id.as_deref().is_some_and(|id| !id.is_empty() && id == node_id))
        .collect();
    match matches.as_slice() {
        [] => Err(Refusal::NotAllowed { node_name: node_name.to_string() }),
        [(name, entry)] => Ok(Admitted {
            peer_name: (*name).clone(),
            node_id: node_id.to_string(),
            profiles: entry.profiles.clone().unwrap_or_default(),
            roles: entry.roles.clone().unwrap_or_default(),
            images: entry.images.clone().unwrap_or_default(),
            workspace: entry.workspace.unwrap_or(false),
        }),
        many => Err(Refusal::AmbiguousEntry { names: many.iter().map(|(n, _)| (*n).clone()).collect() }),
    }
}

/// The allow-list as the config.json at `path` holds it NOW (not the
/// process-start snapshot every other setting uses), so trust changes apply
/// at once. A missing file is an empty list; a file that exists but does
/// not parse is an error, and the listener refuses everything on it: an
/// unreadable allow-list is fail closed, never "empty by accident".
pub fn read_allow_list(path: &std::path::Path) -> std::result::Result<BTreeMap<String, AcceptWorkEntry>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    let cfg: darkmux_types::config::DarkmuxConfig =
        serde_json::from_str(&raw).map_err(|e| format!("{} does not parse: {e}", path.display()))?;
    Ok(cfg.fleet.and_then(|f| f.accept_work).unwrap_or_default())
}

/// [`read_allow_list`] at this machine's user-scope config.json.
pub fn read_user_allow_list() -> std::result::Result<BTreeMap<String, AcceptWorkEntry>, String> {
    read_allow_list(&darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser).config)
}

/// What the receiver's profile resolution made of a job's (role, profile).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileResolution {
    /// A work profile, by the name it resolved to, and the seat it invokes
    /// (#2916 stage 2: busy is decided per seat).
    Work { profile: String, seat: crate::seats::WorkSeat },
    /// The profile's only model is the machine's utility model.
    UtilityOnly(String),
    /// Nothing runnable resolves; the detail says why.
    Unresolved(String),
}

/// What passed [`check_scope`]: the RESOLVED profile (so what runs is
/// exactly what was checked) and the seat it invokes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedJob {
    pub profile: String,
    pub seat: crate::seats::WorkSeat,
}

/// The job against the admitted peer's scope.
pub fn check_scope(
    receiver: &str,
    admitted: &Admitted,
    job: &WorkJob,
    resolution: ProfileResolution,
) -> std::result::Result<ScopedJob, Refusal> {
    if !crate::job::same_machine(&job.target_machine, receiver) {
        return Err(Refusal::Misaddressed { target: job.target_machine.clone() });
    }
    if !admitted.roles.iter().any(|r| r == &job.role_id) {
        return Err(Refusal::RoleOutOfScope {
            peer: admitted.peer_name.clone(),
            role: job.role_id.clone(),
            allowed: admitted.roles.clone(),
        });
    }
    if let Some(image) = &job.image {
        if !admitted.images.iter().any(|i| i == image) {
            return Err(Refusal::ImageOutOfScope { peer: admitted.peer_name.clone(), image: image.clone() });
        }
    }
    if job.workdir.is_some() && !admitted.workspace {
        return Err(Refusal::WorkspaceOutOfScope { peer: admitted.peer_name.clone() });
    }
    let (profile, seat) = match resolution {
        ProfileResolution::Work { profile, seat } => (profile, seat),
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
    Ok(ScopedJob { profile, seat })
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
        Ok(Resolution::Target(t)) => {
            let model = t.model.id.clone();
            let seat = if t.kind.is_managed() {
                crate::seats::WorkSeat::Local { model }
            } else {
                crate::seats::WorkSeat::Hosted { model }
            };
            ProfileResolution::Work { profile: t.profile_name, seat }
        }
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

/// The largest reply body a sender reads (a finished job's stdout and
/// stderr ride in it).
const MAX_REPLY_BYTES: u64 = 16 * 1024 * 1024;

/// (#2916 stage 2 review C1) The connection failed after the request may
/// have reached the receiver: the job may be running there, and the sender
/// cannot tell. [`submit_work`] adds the session id to follow it by.
#[derive(Debug)]
pub struct AnswerLost {
    pub detail: String,
}

impl std::fmt::Display for AnswerLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for AnswerLost {}

fn read_reply(
    where_: &str,
    resp: std::result::Result<ureq::Response, ureq::Error>,
    on_progress: &mut dyn FnMut(&SubmissionReply),
) -> Result<(u16, SubmissionReply)> {
    let (code, resp) = match resp {
        Ok(r) => (r.status(), r),
        Err(ureq::Error::Status(code, r)) => (code, r),
        Err(ureq::Error::Transport(t)) => return Err(transport_failure(where_, &t)),
    };
    read_reply_lines(where_, code, std::io::Read::take(resp.into_reader(), MAX_REPLY_BYTES), on_progress)
}

/// (#2916 stage 2 review C6) A reply line this darkmux cannot read. A line
/// with a `status` it does not know came from a darkmux listener, probably
/// a newer one, which may have taken the job ([`AnswerLost`]); anything else
/// is not a listener's reply.
fn unreadable_line(where_: &str, line: &str, not_a_listener: &dyn Fn(&str) -> anyhow::Error) -> anyhow::Error {
    let status = serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("status")?.as_str().map(str::to_string));
    match status {
        Some(st) => AnswerLost {
            detail: format!(
                "unknown reply status `{}` from {where_} (a newer darkmux?)",
                sanitize_remote_line(&truncate_chars(&st, 40))
            ),
        }
        .into(),
        None => not_a_listener(line),
    }
}

/// A transport failure: nothing was sent when the connection never opened;
/// otherwise the answer was lost and the job may be running ([`AnswerLost`]).
fn transport_failure(where_: &str, t: &ureq::Transport) -> anyhow::Error {
    use ureq::ErrorKind;
    match t.kind() {
        ErrorKind::Dns | ErrorKind::ConnectionFailed | ErrorKind::InvalidUrl | ErrorKind::UnknownScheme => {
            anyhow!("no answer from {where_}: {t}; nothing was sent")
        }
        _ => AnswerLost { detail: format!("no answer from {where_}: {t}") }.into(),
    }
}

/// (#2916 stage 2) Read a newline-delimited reply body: every line before
/// the last is a `queued` progress line, handed to `on_progress` as it
/// arrives; the last line is the answer.
pub(crate) fn read_reply_lines(
    where_: &str,
    code: u16,
    body: impl std::io::Read,
    on_progress: &mut dyn FnMut(&SubmissionReply),
) -> Result<(u16, SubmissionReply)> {
    use std::io::BufRead;
    let not_a_listener = |text: &str| {
        anyhow!(
            "{where_} answered HTTP {code} but not as a darkmux fleet listener (is `fleet.listener` \
             enabled on that machine, on the same port as here?): {}",
            sanitize_remote_text(&text.chars().take(200).collect::<String>())
        )
    };
    let mut last: Option<SubmissionReply> = None;
    for line in std::io::BufReader::new(body).lines() {
        let line = line.map_err(|e| AnswerLost { detail: format!("the answer from {where_} broke off: {e}") })?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let reply: SubmissionReply = serde_json::from_str(line).map_err(|_| unreadable_line(where_, line, &not_a_listener))?;
        // Only a `queued` line may be followed by another.
        if last.take().is_some_and(|prev| prev.status != ReplyStatus::Queued) {
            return Err(not_a_listener(line));
        }
        // Reported as it arrives: that is the point of sending it.
        if reply.status == ReplyStatus::Queued {
            on_progress(&reply);
        }
        last = Some(reply);
    }
    last.map(|r| (code, r)).ok_or_else(|| not_a_listener(""))
}

/// Send one job to a VERIFIED target's fleet listener (the token is
/// attached by `peer`, the only place that does). `Ok(reply)` for every
/// answer the receiver gave (including a refusal); `Err` only when no
/// answer came. `on_progress` hears each `queued` line as it arrives.
pub fn send_submission(
    target: &crate::peer::PeerTarget,
    submission: &WorkSubmission,
    read_timeout: Duration,
    on_progress: &mut dyn FnMut(&SubmissionReply),
) -> Result<(u16, SubmissionReply)> {
    let body = serde_json::to_string(submission).context("serializing the work submission")?;
    let where_ = format!("{}{SUBMISSION_PATH}", target.base());
    read_reply(&where_, crate::peer::fleet_post_json(target, SUBMISSION_PATH, &body, read_timeout), on_progress)
}

/// Tests only: send one job to `url` with an explicit token, unverified.
#[cfg(any(test, feature = "test-support"))]
pub fn post_submission(
    url: &str,
    token: &str,
    submission: &WorkSubmission,
    read_timeout: Duration,
) -> Result<(u16, SubmissionReply)> {
    let body = serde_json::to_string(submission).context("serializing the work submission")?;
    let base = url.strip_suffix(SUBMISSION_PATH).unwrap_or(url);
    let target = crate::peer::unverified_target_for_test(base);
    read_reply(
        url,
        crate::peer::post_json_with_token_for_test(&target, SUBMISSION_PATH, &body, read_timeout, token),
        &mut |_| {},
    )
}

/// Tests only: [`post_submission`], collecting every `queued` progress line.
#[cfg(any(test, feature = "test-support"))]
pub fn post_submission_with_progress(
    url: &str,
    token: &str,
    submission: &WorkSubmission,
    read_timeout: Duration,
    on_progress: &mut dyn FnMut(&SubmissionReply),
) -> Result<(u16, SubmissionReply)> {
    let body = serde_json::to_string(submission).context("serializing the work submission")?;
    let base = url.strip_suffix(SUBMISSION_PATH).unwrap_or(url);
    let target = crate::peer::unverified_target_for_test(base);
    read_reply(
        url,
        crate::peer::post_json_with_token_for_test(&target, SUBMISSION_PATH, &body, read_timeout, token),
        on_progress,
    )
}

/// (#2916 review C1, re-review MUST 4) Text that came back from another
/// machine, safe to print: every control character except newline and tab
/// is dropped, so a reply cannot move the cursor, rewrite earlier lines or
/// set the terminal title with escape sequences; and the bidirectional
/// overrides and zero-width characters (U+202A-202E, U+2066-2069,
/// U+200B-200F, U+FEFF) are dropped, so it cannot reorder or hide text.
pub fn sanitize_remote_text(s: &str) -> String {
    s.chars().filter(|c| !is_unsafe_remote_char(*c)).collect()
}

fn is_unsafe_remote_char(c: char) -> bool {
    (c.is_control() && c != '\n' && c != '\t')
        || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200B}'..='\u{200F}' | '\u{FEFF}')
}

/// Every string inside a JSON value another machine sent, through
/// [`sanitize_remote_text`] (keys included), so no field of it can carry a
/// terminal escape into anything that prints it.
pub fn sanitize_remote_json(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::String(s) => *s = sanitize_remote_text(s),
        serde_json::Value::Array(a) => a.iter_mut().for_each(sanitize_remote_json),
        serde_json::Value::Object(o) => {
            let entries: Vec<(String, serde_json::Value)> = std::mem::take(o).into_iter().collect();
            for (k, mut val) in entries {
                sanitize_remote_json(&mut val);
                o.insert(sanitize_remote_text(&k), val);
            }
        }
        _ => {}
    }
}

/// (#2916 round 3 MUST) Every string inside a JSON value another machine
/// sent, made SINGLE-LINE ([`sanitize_remote_line`]: no newline, no tab, no
/// control, bidi or zero-width character) and cut to `max_chars` (keys
/// included). For peer payloads whose fields are rendered as table cells or
/// one-line fields: a peer can then neither start a forged line (a fake row,
/// a fake warning) nor pad a field out into other columns.
pub fn sanitize_remote_json_lines(v: &mut serde_json::Value, max_chars: usize) {
    match v {
        serde_json::Value::String(s) => *s = truncate_chars(&sanitize_remote_line(s), max_chars),
        serde_json::Value::Array(a) => a.iter_mut().for_each(|x| sanitize_remote_json_lines(x, max_chars)),
        serde_json::Value::Object(o) => {
            let entries: Vec<(String, serde_json::Value)> = std::mem::take(o).into_iter().collect();
            for (k, mut val) in entries {
                sanitize_remote_json_lines(&mut val, max_chars);
                o.insert(truncate_chars(&sanitize_remote_line(&k), max_chars), val);
            }
        }
        _ => {}
    }
}

/// At most `max` characters of `s`, the last one an ellipsis when cut.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// [`sanitize_remote_text`] for a one-line value (a table cell): newlines
/// and tabs are dropped too.
pub fn sanitize_remote_line(s: &str) -> String {
    sanitize_remote_text(s).chars().filter(|c| *c != '\n' && *c != '\t').collect()
}

/// The provider the SENDER verifies a target with. Tests in this crate may
/// substitute one; production always uses the configured provider.
fn sender_provider() -> Result<Box<dyn crate::identity::IdentityProvider>> {
    #[cfg(test)]
    if let Some(p) = test_sender_provider::take() {
        return Ok(p);
    }
    crate::identity::configured_provider()
}

#[cfg(test)]
pub(crate) mod test_sender_provider {
    use crate::identity::IdentityProvider;
    use std::sync::Mutex;
    static NEXT: Mutex<Option<Box<dyn IdentityProvider>>> = Mutex::new(None);
    pub(crate) fn set(p: Box<dyn IdentityProvider>) {
        *NEXT.lock().unwrap() = Some(p);
    }
    pub(crate) fn take() -> Option<Box<dyn IdentityProvider>> {
        NEXT.lock().unwrap().take()
    }
}

/// Order resolver answers IPv4 first (stable otherwise): a fleet listener
/// binds its node's IPv4 overlay address.
pub(crate) fn prefer_ipv4(ips: &mut [IpAddr]) {
    ips.sort_by_key(|ip| !ip.is_ipv4());
}

/// What the sender verified about the target before sending anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedTarget {
    /// The overlay address to dial (the one that was verified; the name is
    /// not re-resolved, so a DNS change between check and send cannot
    /// redirect the request).
    pub ip: IpAddr,
    pub node: NodeIdentity,
    /// True when this send pinned the node for the first time.
    pub newly_pinned: bool,
}

/// (#2916 review C1) Check the target BEFORE the fleet token or the prompt
/// leaves this machine: the roster address must resolve to an address the
/// overlay network says is a node, and that node must be the one pinned for
/// the entry (pinned now, if the entry has no pin yet). Anything else is
/// refused: a LAN host, a public address, a changed DNS answer.
pub fn verify_target(
    target: &str,
    entry: &crate::MachineEntry,
    provider: &dyn crate::identity::IdentityProvider,
) -> Result<VerifiedTarget> {
    let mut ips = crate::roster::resolve_host_addrs(&entry.address);
    if ips.is_empty() {
        return Err(anyhow!("the roster address for {target} (`{}`) does not resolve; nothing was sent", entry.address));
    }
    // (#2916 round 3) Try the answers IPv4 first: a fleet listener binds the
    // node's IPv4 overlay address, so an IPv6-first resolver answer must not
    // decide which address is dialed. The first answer the provider names as
    // a node is the one verified and used.
    prefer_ipv4(&mut ips);
    let mut last_err = None;
    let mut found = None;
    for ip in ips {
        match provider.identify(ip) {
            Ok(Some(n)) => {
                found = Some((ip, n));
                break;
            }
            Ok(None) => {}
            Err(e) => last_err = Some(e),
        }
    }
    let (ip, node) = match (found, last_err) {
        (Some(x), _) => x,
        (None, Some(e)) => {
            return Err(anyhow!(
                "cannot verify {target}'s address with {} ({e:#}); nothing was sent",
                provider.provider_name()
            ))
        }
        (None, None) => {
            return Err(anyhow!(
                "the roster address for {target} (`{}`) is not a node on the {} network, so nothing \
                 was sent to it (not the fleet token, not the request). Point the entry at {target}'s \
                 tailnet DNS name: `darkmux machine add {target} --address <its tailnet DNS name>`",
                entry.address,
                provider.provider_name()
            ))
        }
    };
    match entry.node_id.as_deref().filter(|p| !p.is_empty()) {
        Some(pinned) if pinned != node.node_id => Err(anyhow!(
            "the node at {target}'s address (`{}`) is not the one this roster pinned for {target}; \
             nothing was sent. If {target} really was replaced, re-pin it with `darkmux machine add \
             {target} --address <its tailnet DNS name>`",
            node.name
        )),
        Some(_) => Ok(VerifiedTarget { ip, node, newly_pinned: false }),
        None => Ok(VerifiedTarget { ip, node, newly_pinned: true }),
    }
}

/// Submit `job` to the machine it is addressed to: look the machine up in
/// this machine's roster (case-insensitively), verify the node at its
/// address ([`verify_target`]), then dial that verified address's fleet
/// listener with the fleet token. A refusal comes back as `Err` carrying
/// the receiver's reason (control characters removed).
pub fn submit_work(job: WorkJob, wait: bool) -> Result<SubmissionReply> {
    job.validate().context("validating the job before it leaves")?;
    let target = job.target_machine.clone();
    let roster = crate::load_roster().context("reading the fleet roster")?;
    let entry = crate::find_machine(&roster, &target)?.cloned().ok_or_else(|| {
        anyhow!(
            "machine `{target}` is not in this machine's roster ({}); add it with \
             `darkmux machine add {target} --address <its tailnet DNS name>`",
            crate::roster_path().display()
        )
    })?;
    if !darkmux_flow::serve_token_present() {
        return Err(anyhow!(
            "no fleet token on this machine: submitting work to {target} needs the serve token \
             (Keychain item `darkmux-serve-token`, read only when `serve.token_keychain` is on, or \
             DARKMUX_SERVE_TOKEN), the same value {target} holds"
        ));
    }
    let provider = sender_provider()?;
    let port = darkmux_types::config_access::fleet_listener_port();
    // Every address is verified for work, loopback included (the listener
    // never binds loopback, so a real provider refuses it).
    let peer = crate::peer::peer_target(&target, &entry, None, Some(port), port, false, provider.as_ref())?;
    if peer.newly_pinned.is_some() {
        crate::peer::persist_pin(&entry.id, &peer).context("pinning the target's node in the roster")?;
        eprintln!(
            "darkmux dispatch: pinned {target} to its {} node (first contact); later sends check it",
            provider.provider_name()
        );
    }
    let read_timeout = if wait {
        Duration::from_secs(u64::from(job.timeout_seconds).saturating_add(120))
    } else {
        Duration::from_secs(60)
    };
    let session_id = job.session_id.clone();
    let submission = WorkSubmission::new(job, wait);
    // (#2916 stage 2) A waited-on job the receiver queued says so as it
    // happens, verbatim (control characters removed). Without `--wait` the
    // one `queued` reply is the answer, reported by the caller.
    let mut on_progress = |r: &SubmissionReply| {
        if wait {
            if let Some(reason) = &r.reason {
                eprintln!("darkmux dispatch: {}", sanitize_remote_text(reason));
            }
        }
    };
    let (code, reply) =
        send_submission(&peer, &submission, read_timeout, &mut on_progress).map_err(|e| match e.downcast::<AnswerLost>() {
            Ok(lost) => {
                // The receiver names the run after the allow-list entry it
                // trusts this machine under, normally this machine_id.
                let me = darkmux_flow::resolve_machine_id().unwrap_or_else(|| "<this machine>".into());
                let theirs = SessionId::relay(session_id.clone(), me);
                anyhow!(
                    "{lost}. The job may still be running on {target} (session {theirs}); follow it \
                     there in its viewer or with `darkmux flow tail`"
                )
            }
            Err(other) => other,
        })?;
    match reply.status {
        ReplyStatus::Completed | ReplyStatus::Accepted | ReplyStatus::Queued => Ok(reply),
        ReplyStatus::Error => Err(anyhow!(
            "{target} accepted the job but the dispatch failed: {}",
            sanitize_remote_text(reply.reason.as_deref().unwrap_or("no reason given"))
        )),
        ReplyStatus::Refused => Err(anyhow!(
            "{}",
            reply
                .reason
                .as_deref()
                .map(sanitize_remote_text)
                .unwrap_or_else(|| format!("{target} refused the job (HTTP {code})"))
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::test_node;

    /// (#2988 review) A machine whose Keychain item exists can still have no
    /// token, because `serve.token_keychain` is the switch that reads it. The
    /// no-token sentences name the switch, not only the item.
    #[test]
    fn the_no_token_sentence_names_the_switch_that_reads_the_keychain() {
        let said = Refusal::NoTokenConfigured.reason("studio");
        assert!(said.contains("serve.token_keychain"), "{said}");
        let said = Refusal::Token.reason("studio");
        assert!(said.contains("serve.token_keychain"), "{said}");
    }

    fn entry(node_id: Option<&str>, profiles: &[&str], workspace: bool) -> AcceptWorkEntry {
        AcceptWorkEntry {
            node_id: node_id.map(str::to_string),
            profiles: Some(profiles.iter().map(|s| s.to_string()).collect()),
            roles: Some(vec!["radio-host".into()]),
            images: Some(vec!["rust:slim".into()]),
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
            session_id: crate::test_session("s-1"),
            profile: profile.map(str::to_string),
            workdir: None,
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
        let admitted = admit(token, identity, "tailscale", peer, Some("nSTUDIO"), || Ok(allow()))?;
        let resolution = match prof {
            Prof::InScope => work("host"),
            Prof::OutOfScope => work("coder-big"),
            Prof::Utility => ProfileResolution::UtilityOnly("utility".into()),
        };
        check_scope("studio", &admitted, &job(None), resolution).map(|s| s.profile)
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
            let r = admit(t, || panic!("identity looked up without a token"), "tailscale", peer, Some("nSTUDIO"), || Ok(allow()));
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
        let mut a = allow();
        // A hand-edited entry with an EMPTY id must not match a node the
        // provider reports with an empty id either.
        a.insert("blank".into(), entry(Some(""), &["host"], false));
        let r = admit(TokenCheck::Match, || Ok(Some(n)), "tailscale", peer, Some("nSTUDIO"), || Ok(a.clone()));
        assert!(matches!(r, Err(Refusal::NotAllowed { .. })), "{r:?}");
    }

    #[test]
    fn two_names_for_one_node_are_refused_not_guessed() {
        let mut a = allow();
        a.insert("laptop".into(), entry(Some("nLAPTOP"), &["host"], true));
        let peer: IpAddr = "100.64.0.7".parse().unwrap();
        let r = admit(TokenCheck::Match, || Ok(Some(test_node("nLAPTOP", "macbook-pro", "100.64.0.7"))), "tailscale", peer, Some("nSTUDIO"), || Ok(a.clone()));
        assert!(matches!(r, Err(Refusal::AmbiguousEntry { ref names }) if names.len() == 2), "{r:?}");
    }

    /// (#2916 review) A machine never admits its own node; the role must be
    /// listed; a named image must be listed; machine names compare
    /// case-insensitively; the receiver's session id always carries the peer.
    #[test]
    fn self_role_image_and_case_are_scoped() {
        let peer: IpAddr = "100.64.0.2".parse().unwrap();
        let mut a = allow();
        a.insert("studio".into(), entry(Some("nSTUDIO"), &["host"], false));
        let r = admit(TokenCheck::Match, || Ok(Some(test_node("nSTUDIO", "studio", "100.64.0.2"))), "tailscale", peer, Some("nSTUDIO"), || Ok(a.clone()));
        assert_eq!(r, Err(Refusal::FromSelf));

        let admitted = Admitted {
            node_id: "nLAPTOP".into(),
            peer_name: "laptop".into(),
            profiles: vec!["host".into()],
            roles: vec!["radio-host".into()],
            images: vec!["rust:slim".into()],
            workspace: false,
        };
        let work = || work("host");
        let mut j = job(None);
        j.role_id = "coder".into();
        assert!(matches!(check_scope("studio", &admitted, &j, work()), Err(Refusal::RoleOutOfScope { ref role, .. }) if role == "coder"));
        let no_roles = Admitted { roles: vec![], ..admitted.clone() };
        assert!(matches!(check_scope("studio", &no_roles, &job(None), work()), Err(Refusal::RoleOutOfScope { .. })), "absent roles = none");
        let mut j = job(None);
        j.image = Some("evil.example/x:latest".into());
        assert!(matches!(check_scope("studio", &admitted, &j, work()), Err(Refusal::ImageOutOfScope { .. })));
        j.image = Some("rust:slim".into());
        assert_eq!(check_scope("studio", &admitted, &j, work()).unwrap().profile, "host");
        let mut j = job(None);
        j.target_machine = "Studio".into();
        assert_eq!(check_scope("studio", &admitted, &j, work()).unwrap().profile, "host", "case-insensitive");
    }

    #[test]
    fn scope_refuses_a_misaddressed_job_and_a_workdir_without_workspace() {
        let admitted = Admitted {
            node_id: "nLAPTOP".into(),
            peer_name: "macbook-pro".into(),
            profiles: vec!["host".into()],
            roles: vec!["radio-host".into()],
            images: vec![],
            workspace: false,
        };
        let mut j = job(None);
        j.target_machine = "mini".into();
        assert!(matches!(
            check_scope("studio", &admitted, &j, work("host")),
            Err(Refusal::Misaddressed { .. })
        ));
        let mut j = job(None);
        j.workdir = Some("/tmp/x".into());
        assert!(matches!(
            check_scope("studio", &admitted, &j, work("host")),
            Err(Refusal::WorkspaceOutOfScope { .. })
        ));
        let with_ws = Admitted { workspace: true, ..admitted.clone() };
        assert_eq!(check_scope("studio", &with_ws, &j, work("host")).unwrap().profile, "host");
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
        assert_eq!(Refusal::Busy { what: "s".into() }.http_status(), 503);
        assert_eq!(Refusal::QueueFull { peer: "p".into(), what: "s".into() }.http_status(), 503);
        let reply = Refusal::Token.reply("studio");
        assert_eq!(reply.status, ReplyStatus::Refused);
        assert!(!serde_json::to_string(&reply).unwrap().contains("nLAPTOP"));
    }

    #[test]
    fn an_unreadable_allow_list_is_an_error_not_an_empty_one() {
        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().join("config.json");
        assert!(read_allow_list(&p).unwrap().is_empty(), "no file: nothing trusted");
        std::fs::write(&p, r#"{"fleet":{"accept_work":{"laptop":{"node_id":"n1","profiles":["host"]}}}}"#).unwrap();
        assert_eq!(read_allow_list(&p).unwrap()["laptop"].node_id.as_deref(), Some("n1"));
        std::fs::write(&p, "{ not json").unwrap();
        assert!(read_allow_list(&p).is_err());
    }

    /// A 3.x sender speaks v6, whose `session_id` is a free-form string.
    /// Its job must get the version remedy, never a field error from the
    /// 4.0 session grammar it could not have known.
    #[test]
    fn a_v6_job_with_a_pre_4_0_session_gets_the_version_remedy() {
        let good = serde_json::to_vec(&WorkSubmission::new(job(Some("host")), true)).unwrap();
        let mut v: serde_json::Value = serde_json::from_slice(&good).unwrap();
        v["schema"] = "6".into();
        v["job"]["session_id"] = "crew-dispatch-coder-1788254029192466-0".into();
        assert_eq!(
            WorkSubmission::parse(&serde_json::to_vec(&v).unwrap()).unwrap_err(),
            Refusal::SchemaMismatch { got: "6".into() }
        );
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

    fn roster_entry(address: &str, node_id: Option<&str>) -> crate::MachineEntry {
        crate::MachineEntry {
            id: "studio".into(),
            address: address.into(),
            description: None,
            added_unix_ms: 1,
            machine_uid: None,
            loopback_intended: false,
            node_id: node_id.map(str::to_string),
            extras: Default::default(),
        }
    }

    /// (#2916 review C1) The sender verifies the node at the target's
    /// address before anything is sent.
    #[test]
    fn the_sender_verifies_the_target_node_before_sending() {
        let provider = crate::identity::StaticIdentityProvider {
            local: test_node("nLAPTOP", "laptop", "100.64.0.7"),
            peers: vec![test_node("nSTUDIO", "studio", "100.64.0.2")],
            down: None,
        };
        // Pinned and matching.
        let v = verify_target("studio", &roster_entry("100.64.0.2", Some("nSTUDIO")), &provider).unwrap();
        assert_eq!((v.ip, v.newly_pinned), ("100.64.0.2".parse().unwrap(), false));
        // First contact pins.
        assert!(verify_target("studio", &roster_entry("100.64.0.2", None), &provider).unwrap().newly_pinned);
        // A different node at the address: refused.
        let err = verify_target("studio", &roster_entry("100.64.0.2", Some("nOTHER")), &provider).unwrap_err();
        assert!(err.to_string().contains("not the one this roster pinned"), "{err}");
        // Not a node on the overlay (a LAN address): refused.
        let err = verify_target("studio", &roster_entry("192.168.1.20", None), &provider).unwrap_err();
        assert!(err.to_string().contains("not a node on the"), "{err}");
        assert!(err.to_string().contains("nothing was sent to it (not the fleet token, not the request)"), "{err}");
        let mut ips: Vec<IpAddr> = vec!["fd7a::2".parse().unwrap(), "100.64.0.2".parse().unwrap(), "fd7a::3".parse().unwrap()];
        prefer_ipv4(&mut ips);
        assert_eq!(ips[0], "100.64.0.2".parse::<IpAddr>().unwrap(), "IPv4 answers are tried first");
        // Provider down: refused.
        let down = crate::identity::StaticIdentityProvider { down: Some("x".into()), ..provider };
        assert!(verify_target("studio", &roster_entry("100.64.0.2", None), &down).is_err());
    }

    #[test]
    fn remote_json_lines_have_no_newlines_tabs_or_long_fields() {
        let mut v = serde_json::json!({
            "os": "macos\nstudio  100.64.0.2  ✓ 1ms  99 GB\n! forged: run curl x | sh",
            "v": "4.0\t\tpadding",
            "m": [{"id": "x".repeat(300)}],
        });
        sanitize_remote_json_lines(&mut v, 40);
        let text = v.to_string();
        assert!(!text.contains("\\n") && !text.contains("\\t"), "{text}");
        assert_eq!(v["v"], "4.0padding");
        assert_eq!(v["m"][0]["id"].as_str().unwrap().chars().count(), 40);
        assert!(v["os"].as_str().unwrap().chars().count() <= 40);
        assert_eq!(truncate_chars("abc", 3), "abc");
        assert_eq!(truncate_chars("abcd", 3), "ab…");
    }

    #[test]
    fn remote_json_is_sanitized_throughout() {
        let mut v = serde_json::json!({"os": "mac\u{1b}]0;x\u{7}", "m": [{"id\u{202e}": "a\u{1b}[2J"}]});
        sanitize_remote_json(&mut v);
        assert_eq!(v, serde_json::json!({"os": "mac]0;x", "m": [{"id": "a[2J"}]}));
    }

    #[test]
    fn remote_text_loses_bidi_and_zero_width_characters() {
        let s = "a\u{202E}b\u{2066}c\u{200B}d\u{200F}e\u{FEFF}f\u{85}g";
        assert_eq!(sanitize_remote_text(s), "abcdefg");
        assert_eq!(sanitize_remote_line("a\nb\tc"), "abc");
        assert_eq!(sanitize_remote_text("naïve 日本"), "naïve 日本", "ordinary text is untouched");
    }

    #[test]
    fn remote_text_loses_its_control_characters() {
        assert_eq!(sanitize_remote_text("ok\x1b[2J\x1b]0;pwned\x07done\r\nline2\tx"), "ok[2J]0;pwneddone\nline2\tx");
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

    /// A work resolution on a local model.
    fn work(profile: &str) -> ProfileResolution {
        ProfileResolution::Work { profile: profile.into(), seat: crate::seats::WorkSeat::Local { model: "big".into() } }
    }

    fn registry(json: &str) -> darkmux_types::ProfileRegistry {
        let mut r: darkmux_types::ProfileRegistry = serde_json::from_str(json).unwrap();
        r.materialize_endpoints();
        r
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
              "internal":{"utility":{"id":"small"}}}"#,
        );
        let r = role();
        assert_eq!(classify_profile(&reg, &r, Some("host"), None, "studio"), work("host"));
        assert_eq!(classify_profile(&reg, &r, None, None, "studio"), work("host"));
        assert_eq!(classify_profile(&reg, &r, Some("utility"), None, "studio"), ProfileResolution::UtilityOnly("utility".into()));
        assert!(matches!(
            classify_profile(&reg, &r, Some("nope"), None, "studio"),
            ProfileResolution::Unresolved(ref d) if d.contains("not defined on studio")
        ));
    }

    /// (#2916 stage 2) Busy is decided per seat, so the resolution says what
    /// the job invokes: a managed (local) model, or a hosted endpoint.
    #[test]
    fn classify_profile_names_the_seat_a_job_invokes() {
        let reg = registry(
            r#"{"profiles":{
                "host":{"models":[{"id":"big","n_ctx":32000}]},
                "cloud":{"models":[{"id":"gpt-x","n_ctx":32000,"endpoint":"api"}]}},
              "endpoints":{"api":{"url":"https://api.example/v1"}},
              "default_profile":"host"}"#,
        );
        let r = role();
        assert_eq!(
            classify_profile(&reg, &r, Some("host"), None, "studio"),
            ProfileResolution::Work { profile: "host".into(), seat: crate::seats::WorkSeat::Local { model: "big".into() } }
        );
        assert_eq!(
            classify_profile(&reg, &r, Some("cloud"), None, "studio"),
            ProfileResolution::Work { profile: "cloud".into(), seat: crate::seats::WorkSeat::Hosted { model: "gpt-x".into() } }
        );
    }

    /// (#2916 stage 2) A reply body is newline-delimited: queued lines are
    /// reported as they arrive, the last line is the answer, and anything
    /// but a `queued` line followed by another is not a listener's reply.
    #[test]
    fn a_reply_body_reports_queued_lines_and_returns_the_last() {
        let body = "{\"status\":\"queued\",\"reason\":\"busy (a)\"}\n{\"status\":\"queued\",\"reason\":\"busy (b)\"}\n{\"status\":\"completed\",\"exit_code\":0}\n";
        let mut seen = Vec::new();
        let (code, r) = read_reply_lines("x", 200, body.as_bytes(), &mut |p| seen.push(p.reason.clone().unwrap())).unwrap();
        assert_eq!((code, r.status, r.exit_code), (200, ReplyStatus::Completed, Some(0)));
        assert_eq!(seen, vec!["busy (a)".to_string(), "busy (b)".to_string()]);
        // One line: the answer, as before.
        let (_, r) = read_reply_lines("x", 202, "{\"status\":\"accepted\"}".as_bytes(), &mut |_| panic!("no progress")).unwrap();
        assert_eq!(r.status, ReplyStatus::Accepted);
        // A final answer followed by more is not a listener's reply.
        let bad = "{\"status\":\"completed\"}\n{\"status\":\"completed\"}\n";
        assert!(read_reply_lines("x", 200, bad.as_bytes(), &mut |_| {}).is_err());
        assert!(read_reply_lines("x", 200, "".as_bytes(), &mut |_| {}).is_err(), "an empty body is not an answer");
        // drift-guard:allow darkmux fleet — noun use: the listener, not the retired verb
        assert!(read_reply_lines("x", 200, "<html>".as_bytes(), &mut |_| {}).unwrap_err().to_string().contains("not as a darkmux fleet listener"));
        // (#2916 stage 2 review C6) A status this darkmux does not know is a
        // newer listener's answer, not a stranger's: the job may be running.
        let newer = read_reply_lines("x", 200, "{\"status\":\"deferred\"}\n".as_bytes(), &mut |_| {}).unwrap_err();
        assert!(newer.downcast_ref::<AnswerLost>().is_some(), "{newer}");
        assert!(newer.to_string().contains("unknown reply status `deferred` from x (a newer darkmux?)"), "{newer}");
    }
}
