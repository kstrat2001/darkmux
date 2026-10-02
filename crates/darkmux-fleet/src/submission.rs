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
//!    (`security find-generic-password`), and `machine list` sends it
//!    to every roster peer.
//! 2. the connection comes from a **node on the receiver's allow-list**
//!    (`fleet.accept_work`), as the overlay network itself reports it
//!    ([`crate::identity`]). A leaked token is useless from a node that is not
//!    on the list.
//!
//! and the job fits that node's **scope**: a work-class profile the entry
//! lists (never one that resolves only to the machine's utility model,
//! #2914), and a `workdir` only when the entry grants `workspace`, which
//! grants a receiver PATH only: it never authorizes a fetch, a checkout or a
//! push (git handoff, #755, gets its own grant).
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

use crate::job::{Boundary, SubmissionMode, WorkJob, WorkVersion, WORK_JOB_SCHEMA_VERSION};
use crate::identity::{IdentityProvider, NodeIdentity};
use crate::peer::TargetError;
use anyhow::{anyhow, Context, Result};
use darkmux_types::config::AcceptWorkEntry;
use darkmux_types::session_id::SessionId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::Duration;

/// The listener's work route.
pub const SUBMISSION_PATH: &str = "/fleet/work";

/// The listener's card route: this machine's card, with what it accepts from
/// the verified caller.
pub const CARD_PATH: &str = "/fleet/card";

/// The request body: the wire version, whether the sender waits for the
/// result, and the job. `schema` is read first, so a sender on another
/// version is told so by name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
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
        Self { schema: job.wire_version().to_string(), wait, job }
    }

    /// Parse a request body, checking the version before the job's shape.
    pub fn parse(body: &[u8]) -> std::result::Result<Self, Refusal> {
        Self::parse_for(body, WorkVersion::current())
    }

    /// [`parse`](Self::parse) for a receiver speaking `receiver`.
    fn parse_for(body: &[u8], receiver: WorkVersion) -> std::result::Result<Self, Refusal> {
        let v: serde_json::Value = serde_json::from_slice(body)
            .map_err(|e| Refusal::BadRequest(format!("the body is not JSON: {e}")))?;
        let Some(schema) = v.get("schema").and_then(|s| s.as_str()) else {
            return Err(Refusal::BadRequest("the body has no `schema`".into()));
        };
        // The same major, and a minor at or below this receiver's own: an
        // older minor is taken, a newer one (or another major, or a spelling
        // that is not `major.minor`) is refused, naming both versions.
        if !WorkVersion::parse(schema).is_some_and(|sent| receiver.takes(&sent)) {
            return Err(Refusal::SchemaMismatch { got: schema.to_string() });
        }
        let sub: WorkSubmission = serde_json::from_value(v)
            .map_err(|e| Refusal::BadRequest(format!("the job is malformed: {e}")))?;
        sub.job.validate().map_err(|e| Refusal::BadRequest(format!("{e:#}")))?;
        Ok(sub)
    }
}

/// What a reply says happened. Parsed once, at the wire (serde); every
/// consumer matches on it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
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
    /// The job was not run; `reason` says why, and `refusal` names the
    /// kind.
    Refused,
    /// A `check` submission ([`SubmissionMode::Check`]): a run of the job
    /// would be taken, and `check` says what it would meet. Nothing ran.
    Checked,
    /// A status a newer darkmux sends that this one does not know. It is
    /// never read as any known status: not as a success, not as a refusal.
    #[serde(other)]
    Unknown,
}

/// The reply, for every outcome.
///
/// A reply body is newline-delimited JSON, one reply per line. It is one
/// line for every answer except a job the sender waits on that was queued:
/// that body carries a `queued` line when it is queued (again at every
/// heartbeat while it waits), then the final line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
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
    /// For `refused`: the kind of refusal, for consumers to act on. The
    /// `reason` is the sentence for a person and is never parsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<RefusalCode>,
    /// For `checked`: what a run of the job would meet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<CheckReport>,
}

/// Why a job was refused, as a kind a consumer can match on. Every
/// [`Refusal`] maps to exactly one ([`Refusal::code`]).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    /// The fleet token is missing or wrong, or the receiver has none.
    Token,
    /// The receiver cannot place the caller on the overlay network, or its
    /// identity provider cannot answer.
    Identity,
    /// The caller is not on the receiver's allow-list.
    NotListed,
    /// The role is outside the caller's entry.
    RoleNotAllowed,
    /// The profile is outside the caller's entry, or is the utility model.
    ProfileNotAllowed,
    /// The image is outside the caller's entry.
    ImageNotAllowed,
    /// The caller may not name a working directory.
    WorkspaceNotAllowed,
    /// No work profile resolves on the receiver (undefined, quarantined, or
    /// no binding).
    ProfileUndefined,
    /// The job's boundary is not met by the profile it resolves to, or names
    /// a boundary the receiver cannot enforce.
    Boundary,
    /// The seat is busy, the queue is full, or the caller has too many
    /// requests in flight.
    Busy,
    /// A queued job's profile moved to another seat while it waited.
    SeatChanged,
    /// The wire version is not one the receiver takes.
    Version,
    /// The job is addressed to another machine name.
    Misaddressed,
    /// The request came from the receiver's own node.
    #[serde(rename = "self")]
    FromSelf,
    /// The receiver's own config must be fixed first.
    BadConfig,
    /// A malformed request.
    BadRequest,
    /// A code a newer darkmux sends that this one does not know. Never read
    /// as any known kind.
    #[serde(other)]
    Unknown,
}

/// What a `check` found: the answer to "would a run of this job be taken, and
/// what would it meet". The resolved profile rides in the reply's `profile`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct CheckReport {
    /// What the resolved profile's model is, as the boundary sees it.
    pub endpoint: EndpointClass,
    /// What a run would meet at its seat.
    pub seat: SeatOutlook,
}

/// Whether the resolved profile's model is served by the receiver itself.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EndpointClass {
    /// A managed endpoint: a model this machine loads and serves.
    Managed,
    /// A hosted endpoint this machine only sends requests to.
    Unmanaged,
    #[serde(other)]
    Unknown,
}

/// What a run would meet at its seat right now (a fact of this moment: the
/// seat can be taken before the real job arrives).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SeatOutlook {
    /// The seat is free: a run starts at once.
    Free,
    /// The seat is busy and the receiver queues: a run waits its turn.
    WouldQueue,
    #[serde(other)]
    Unknown,
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
            refusal: None,
            check: None,
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
    /// A node, but not one on the allow-list. `ask` says which
    /// `darkmux machine trust` command admits it.
    NotAllowed { node_name: String, ask: TrustAsk },
    /// The allow-list names the same node under several machine names.
    AmbiguousEntry { names: Vec<String> },
    /// Addressed to another machine name.
    Misaddressed { target: String },
    /// Part of the job is outside the peer's allow-list entry.
    OutOfScope { peer: String, item: OutOfScope },
    /// The connection came from THIS machine's own node.
    FromSelf,
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
    /// The job's boundary is `managed_only` and the profile it resolves to
    /// runs on a hosted endpoint.
    BoundaryUnmanaged { profile: String },
    /// The job names a boundary this receiver cannot enforce.
    BoundaryUnknown,
    /// (#2916 round 3 C5) One node already has its cap of requests in flight.
    TooManyAtOnce { peer: String },
    /// (#2947) This machine's own config has an unregistered value in an
    /// enum-valued setting a dispatch reads; the job would refuse at its
    /// own preflight, so it is refused before it is accepted. `detail` is
    /// the preflight refusal (setting key, the bad value, where it was
    /// set, the valid values): config values, never secrets.
    BadConfig { detail: String },
}

/// What of a job is outside a peer's allow-list entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutOfScope {
    Role { role: String, allowed: Vec<String> },
    Image { image: String },
    Workspace,
    Profile { profile: String, allowed: Vec<String> },
}

impl OutOfScope {
    fn code(&self) -> RefusalCode {
        match self {
            OutOfScope::Role { .. } => RefusalCode::RoleNotAllowed,
            OutOfScope::Image { .. } => RefusalCode::ImageNotAllowed,
            OutOfScope::Workspace => RefusalCode::WorkspaceNotAllowed,
            OutOfScope::Profile { .. } => RefusalCode::ProfileNotAllowed,
        }
    }

    /// The sentence, for `peer` on `receiver`.
    fn reason(&self, receiver: &str, peer: &str) -> String {
        match self {
            OutOfScope::Role { role, allowed } => format!(
                "not in the allow-list scope: role {role} ({receiver} lets {peer} run roles: {})",
                if allowed.is_empty() { "none".to_string() } else { allowed.join(", ") }
            ),
            OutOfScope::Image { image } => format!(
                "not in the allow-list scope: image {image} ({receiver} lets {peer} use only darkmux's own \
                 runtime image unless the entry lists others)"
            ),
            OutOfScope::Workspace => format!(
                "not in the allow-list scope: {receiver} does not let {peer} name a working \
                 directory (workspace: false)"
            ),
            OutOfScope::Profile { profile, allowed } => format!(
                "not in the allow-list scope: profile {profile} ({receiver} lets {peer} run: {})",
                if allowed.is_empty() { "nothing".to_string() } else { allowed.join(", ") }
            ),
        }
    }
}

/// What the receiver's `darkmux machine trust` needs to admit a sender that
/// is on no allow-list entry. Resolved once by [`TrustAsk::for_sender`] and
/// printed by [`TrustAsk::command`], so the sentence names an entry the
/// receiver's own tools resolve.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustAsk {
    /// The NAME argument: the receiver's roster id for the verified node,
    /// or the machine id the sender claimed. `None` names the node itself.
    pub name: Option<String>,
    /// `--node`: needed when NAME is not the roster entry of the node
    /// (a claimed id), because the lookup then has no roster address to use.
    pub node: Option<String>,
    /// The role the job asked for.
    pub role: Option<String>,
}

impl TrustAsk {
    /// The ask for `node`, refused with `body` (the job it posted, read
    /// loosely: only its role and claimed machine id, each kept only when
    /// well formed). When the receiver's `roster` has an entry for the node,
    /// that entry's id is NAME and the roster address finds the node. Else
    /// the sender's claimed id is NAME, with `--node` naming the node.
    pub fn for_sender(node: &NodeIdentity, roster: &crate::roster::FleetRoster, body: &[u8]) -> Self {
        let job = serde_json::from_slice::<serde_json::Value>(body).ok().and_then(|v| v.get("job").cloned());
        let text = |field: &str| job.as_ref().and_then(|j| j.get(field)).and_then(|f| f.as_str()).map(str::to_string);
        let role = text("role_id").filter(|r| crate::job::validate_work_identifier("role_id", r).is_ok());
        let on_roster = roster.machines.values().find(|m| {
            node.answers_to(&m.id) || crate::roster::address_host(&m.address).is_some_and(|h| node.answers_to(&h))
        });
        if let Some(entry) = on_roster {
            return Self { name: Some(entry.id.clone()), node: None, role };
        }
        let claimed = text("published_by_machine")
            .filter(|c| darkmux_types::profile_address::machine_name_problem(c).is_none());
        match claimed {
            Some(c) => Self { name: Some(c), node: Some(node.name.clone()), role },
            None => Self { name: None, node: None, role },
        }
    }

    /// The command, for a node the network calls `node_name`.
    fn command(&self, node_name: &str) -> String {
        let name = self.name.as_deref().unwrap_or(node_name);
        let node = self.node.as_ref().map(|n| format!(" --node {n}")).unwrap_or_default();
        let role = self.role.as_deref().unwrap_or("<role>");
        format!("darkmux machine trust {name}{node} --profiles <profile> --roles {role}")
    }
}

impl Refusal {
    /// This refusal with `ask` as the trust command, when it is a
    /// [`Refusal::NotAllowed`]; any other refusal is returned unchanged.
    pub fn with_trust_ask(self, ask: TrustAsk) -> Self {
        match self {
            Refusal::NotAllowed { node_name, .. } => Refusal::NotAllowed { node_name, ask },
            other => other,
        }
    }

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
            Refusal::NotOnOverlay { .. }
            | Refusal::NotAllowed { .. }
            | Refusal::AmbiguousEntry { .. }
            | Refusal::OutOfScope { .. }
            | Refusal::FromSelf
            | Refusal::UtilityProfile { .. }
            | Refusal::BoundaryUnmanaged { .. }
            | Refusal::BoundaryUnknown => 403,
        }
    }

    /// The kind of this refusal, for the reply's `refusal` field. The one
    /// place a refusal is classified.
    pub fn code(&self) -> RefusalCode {
        match self {
            Refusal::NoTokenConfigured | Refusal::Token => RefusalCode::Token,
            Refusal::IdentityUnavailable { .. } | Refusal::NotOnOverlay { .. } => RefusalCode::Identity,
            Refusal::NotAllowed { .. } => RefusalCode::NotListed,
            Refusal::AmbiguousEntry { .. } | Refusal::BadConfig { .. } => RefusalCode::BadConfig,
            Refusal::Misaddressed { .. } => RefusalCode::Misaddressed,
            Refusal::OutOfScope { item, .. } => item.code(),
            Refusal::FromSelf => RefusalCode::FromSelf,
            Refusal::UtilityProfile { .. } => RefusalCode::ProfileNotAllowed,
            Refusal::NoWorkProfile { .. } => RefusalCode::ProfileUndefined,
            Refusal::SchemaMismatch { .. } => RefusalCode::Version,
            Refusal::BadRequest(_) => RefusalCode::BadRequest,
            Refusal::Busy { .. } | Refusal::QueueFull { .. } | Refusal::TooManyAtOnce { .. } => RefusalCode::Busy,
            Refusal::SeatChanged { .. } => RefusalCode::SeatChanged,
            Refusal::BoundaryUnmanaged { .. } | Refusal::BoundaryUnknown => RefusalCode::Boundary,
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
            Refusal::NotAllowed { node_name, ask } => {
                format!("{receiver} does not accept work from {node_name} (on {receiver}: `{}`)", ask.command(node_name))
            }
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
            Refusal::OutOfScope { peer, item } => item.reason(receiver, peer),
            Refusal::FromSelf => format!(
                "{receiver} does not take fleet work from itself; run it locally (address the \
                 profile without `@{receiver}`)"
            ),
            Refusal::UtilityProfile { profile } => format!(
                "profile {profile} resolves only to {receiver}'s utility model; utility work is never \
                 taken from another machine (#2914)"
            ),
            Refusal::NoWorkProfile { role, detail } => format!(
                "{receiver} cannot resolve a work profile for role {role}: {detail}"
            ),
            Refusal::SchemaMismatch { got } => format!(
                "{receiver} speaks work-submission schema v{WORK_JOB_SCHEMA_VERSION} (it takes the same major \
                 version with a minor up to its own), the sender sent v{got}; run a darkmux with the same \
                 major version on both machines, and the newer minor on the receiver"
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
            Refusal::BoundaryUnmanaged { profile } => format!(
                "{receiver}'s profile {profile} runs on a hosted endpoint, and this job may go only to \
                 a model {receiver} serves itself (boundary managed_only)"
            ),
            Refusal::BoundaryUnknown => format!(
                "{receiver} does not know the boundary this job carries, so it cannot enforce it and \
                 does not run the job; run the same darkmux version on both machines"
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
            refusal: Some(self.code()),
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

impl Admitted {
    /// The refusal for something in a job outside this peer's entry.
    fn out_of_scope(&self, item: OutOfScope) -> Refusal {
        Refusal::OutOfScope { peer: self.peer_name.clone(), item }
    }
}

/// Who is calling: the token, then the network identity, then "not this
/// machine". This is the whole of what a READ needs (a card): a caller that
/// holds the fleet token from a node the overlay names is a verified fleet
/// peer whether or not this machine lets it run anything. Checked in that
/// order, and the order is deliberate: the token comparison is constant-time
/// and free, while the identity lookup runs the provider's tool, so a caller
/// without the token never makes this machine spawn anything.
///
/// `identity` is the provider's answer for the connecting address:
/// `Ok(Some)` a node, `Ok(None)` no node holds it, `Err(detail)` the
/// provider could not answer. The last two refuse (fail closed).
pub fn authenticate(
    token: TokenCheck,
    identity: impl FnOnce() -> std::result::Result<Option<NodeIdentity>, String>,
    provider: &str,
    peer_addr: IpAddr,
    local_node_id: Option<&str>,
) -> std::result::Result<NodeIdentity, Refusal> {
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
    Ok(node)
}

/// What an authenticated node may do here: its entry on the allow-list
/// (`fleet.accept_work`). Work needs it; a card read reports it as data.
///
/// `allow` reads the allow-list, and is called only when this runs, which is
/// AFTER the identity lookup (which runs the provider's tool, up to its 3 s
/// bound), so an `untrust` that lands while the provider answers is still
/// seen. An allow-list that cannot be read refuses everything.
pub fn authorize(
    node: &NodeIdentity,
    allow: impl FnOnce() -> std::result::Result<BTreeMap<String, AcceptWorkEntry>, String>,
) -> std::result::Result<Admitted, Refusal> {
    let allow = allow().map_err(|e| {
        Refusal::BadRequest(format!("this machine's allow-list cannot be read ({e}); refusing everything"))
    })?;
    match_entry(&node.node_id, &node.name, &allow)
}

/// [`authenticate`], then [`authorize`]: the one admission path for work.
pub fn admit(
    token: TokenCheck,
    identity: impl FnOnce() -> std::result::Result<Option<NodeIdentity>, String>,
    provider: &str,
    peer_addr: IpAddr,
    local_node_id: Option<&str>,
    allow: impl FnOnce() -> std::result::Result<BTreeMap<String, AcceptWorkEntry>, String>,
) -> std::result::Result<Admitted, Refusal> {
    let node = authenticate(token, identity, provider, peer_addr, local_node_id)?;
    authorize(&node, allow)
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
        [] => Err(Refusal::NotAllowed { node_name: node_name.to_string(), ask: TrustAsk::default() }),
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

/// The job against the admitted peer's scope, then its boundary
/// ([`check_boundary`]).
pub fn check_scope(
    receiver: &str,
    receiver_uid: Option<&str>,
    admitted: &Admitted,
    job: &WorkJob,
    resolution: ProfileResolution,
) -> std::result::Result<ScopedJob, Refusal> {
    if !job.is_addressed_to(receiver, receiver_uid) {
        return Err(Refusal::Misaddressed { target: job.target_machine.clone() });
    }
    if !admitted.roles.iter().any(|r| r == &job.role_id) {
        return Err(admitted.out_of_scope(OutOfScope::Role { role: job.role_id.clone(), allowed: admitted.roles.clone() }));
    }
    if let Some(image) = &job.image {
        if !admitted.images.iter().any(|i| i == image) {
            return Err(admitted.out_of_scope(OutOfScope::Image { image: image.clone() }));
        }
    }
    if job.workdir.is_some() && !admitted.workspace {
        return Err(admitted.out_of_scope(OutOfScope::Workspace));
    }
    let (profile, seat) = match resolution {
        ProfileResolution::Work { profile, seat } => (profile, seat),
        ProfileResolution::UtilityOnly(p) => return Err(Refusal::UtilityProfile { profile: p }),
        ProfileResolution::Unresolved(detail) => {
            return Err(Refusal::NoWorkProfile { role: job.role_id.clone(), detail })
        }
    };
    if !admitted.profiles.iter().any(|p| p == &profile) {
        return Err(admitted.out_of_scope(OutOfScope::Profile { profile, allowed: admitted.profiles.clone() }));
    }
    let scoped = ScopedJob { profile, seat };
    check_boundary(job, &scoped)?;
    Ok(scoped)
}

/// The job's boundary against the seat its profile RESOLVED to: the one
/// enforcement, run by [`check_scope`] when a job arrives and again when a
/// queued job gets its seat. The seat's kind comes from the same resolution
/// every dispatch uses (`darkmux_crew::target::target_for`, through
/// [`classify_profile`]), the resolution the machine card reports as a
/// model's `endpoint_kind`.
fn check_boundary(job: &WorkJob, scoped: &ScopedJob) -> std::result::Result<(), Refusal> {
    match (job.boundary, &scoped.seat) {
        (None, _) | (Some(Boundary::ManagedOnly), crate::seats::WorkSeat::Local { .. }) => Ok(()),
        (Some(Boundary::ManagedOnly), crate::seats::WorkSeat::Unmanaged { .. }) => {
            Err(Refusal::BoundaryUnmanaged { profile: scoped.profile.clone() })
        }
        (Some(Boundary::Unknown), _) => Err(Refusal::BoundaryUnknown),
    }
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
                // (#3035) An unmanaged endpoint's own `limits.concurrent_calls`
                // bounds the jobs that may hold it at once.
                let slot = darkmux_crew::step_kinds::EndpointSlot::of(&t.endpoint);
                crate::seats::WorkSeat::Unmanaged {
                    endpoint: slot.key().to_string(),
                    model,
                    concurrent_calls: slot.declared(),
                }
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

/// Whether a transport failure means the peer's listener never accepted the
/// connection (off, or its daemon is down): the ONE classifier for that case,
/// used by the submission error below and by `/fleet/view`'s `listener_off`.
pub fn is_listener_off(t: &ureq::Transport) -> bool {
    t.kind() == ureq::ErrorKind::ConnectionFailed
}

/// The one sentence for [`is_listener_off`], naming where it was dialed.
pub fn listener_off_sentence(where_: &str) -> String {
    format!("the fleet listener at {where_} is not accepting connections (off, or the daemon is down); nothing was sent")
}

/// A transport failure: nothing was sent when the connection never opened;
/// otherwise the answer was lost and the job may be running ([`AnswerLost`]).
fn transport_failure(where_: &str, t: &ureq::Transport) -> anyhow::Error {
    use ureq::ErrorKind;
    if is_listener_off(t) {
        return anyhow!("{}", listener_off_sentence(where_));
    }
    match t.kind() {
        ErrorKind::Dns | ErrorKind::InvalidUrl | ErrorKind::UnknownScheme => {
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
        // A status this darkmux does not know parses as `Unknown`; the line
        // still names it, and the job may be running, so it reports the same
        // way an unparseable status does.
        if reply.status == ReplyStatus::Unknown {
            return Err(unreadable_line(where_, line, &not_a_listener));
        }
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
    target: &crate::peer::SettledTarget,
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
) -> std::result::Result<VerifiedTarget, TargetError> {
    let mut ips = crate::roster::resolve_host_addrs(&entry.address);
    if ips.is_empty() {
        return Err(TargetError::DoesNotResolve { target: target.to_string(), address: entry.address.clone() });
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
            return Err(TargetError::IdentityUnavailable {
                target: target.to_string(),
                provider: provider.provider_name().to_string(),
                detail: format!("{e:#}"),
            })
        }
        (None, None) => {
            return Err(TargetError::NotOnOverlay {
                target: target.to_string(),
                address: entry.address.clone(),
                provider: provider.provider_name().to_string(),
            })
        }
    };
    match entry.node_id.as_deref().filter(|p| !p.is_empty()) {
        Some(pinned) if pinned != node.node_id => {
            Err(TargetError::PinMismatch { target: target.to_string(), node_name: node.name })
        }
        Some(_) => Ok(VerifiedTarget { ip, node, newly_pinned: false }),
        None => Ok(VerifiedTarget { ip, node, newly_pinned: true }),
    }
}

/// Whether [`send_job`] may write a first-contact pin into the roster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PinPolicy {
    /// Pin the verified node on first contact (a send that is about to submit).
    PinOnFirstContact,
    /// Never write the roster: an unpinned target is not contacted at all.
    RequirePinned,
}

/// The target's node is not pinned in the roster yet, so a read-only send
/// stopped before contacting it.
#[derive(Debug)]
struct NotPinned {
    machine: String,
}

impl std::fmt::Display for NotPinned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} has no pinned node in the roster yet", self.machine)
    }
}

impl std::error::Error for NotPinned {}

/// First contact with a verified node: pin it in the roster, or, for a
/// read-only caller, stop before anything is sent to it. Every verified peer is
/// checked against the SAVED roster entry, not the caller's snapshot: a first
/// contact is written compare-and-set under the roster lock, an already-pinned
/// peer is re-read. Only an `Ok` yields a [`crate::peer::SettledTarget`].
fn settle_pin(
    mut peer: crate::peer::PeerTarget,
    entry: &crate::MachineEntry,
    target: &str,
    pins: PinPolicy,
    provider: &str,
) -> Result<crate::peer::SettledTarget> {
    if peer.node_id.is_none() {
        return Ok(crate::peer::SettledTarget::new(peer));
    }
    if peer.newly_pinned.is_none() {
        peer.node_id = Some(crate::peer::confirm_pin(entry, &peer)?);
        return Ok(crate::peer::SettledTarget::new(peer));
    }
    if pins == PinPolicy::RequirePinned {
        return Err(NotPinned { machine: target.to_string() }.into());
    }
    peer.node_id = Some(crate::peer::persist_pin(entry, &peer).context("pinning the target's node in the roster")?);
    eprintln!("darkmux: pinned {target} to its {provider} node (first contact); later contacts check it");
    Ok(crate::peer::SettledTarget::new(peer))
}

/// Pin a freshly verified peer's node in the roster before the fleet token is
/// sent to it, exactly as a work submission does ([`send_job`]): the ONE
/// first-contact pin, and the one way to a token-bearing target for a verified
/// peer. `entry` is the roster entry the peer was verified from.
pub fn pin_on_first_contact(
    peer: crate::peer::PeerTarget,
    entry: &crate::MachineEntry,
    provider: &dyn IdentityProvider,
) -> Result<crate::peer::SettledTarget> {
    settle_pin(peer, entry, &entry.id, PinPolicy::PinOnFirstContact, provider.provider_name())
}

/// Send `job` to the machine it is addressed to and return the receiver's
/// final reply, whatever it says: look the machine up in this machine's
/// roster (case-insensitively), verify the node at its address
/// ([`verify_target`]), then dial that verified address's fleet listener with
/// the fleet token. `Err` only when no answer came (or the target could not
/// be verified, so nothing was sent).
fn send_job(mut job: WorkJob, wait: bool, pins: PinPolicy) -> Result<(u16, SubmissionReply)> {
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
    // (#3028) The job names the machine by the identity this roster learned
    // from its card, so the receiver accepts it under whatever name the
    // machine goes by now. An entry that has learned none (or holds a value
    // that is not a uid) sends none, and the receiver checks the name.
    job.target_machine_uid =
        entry.machine_uid.clone().filter(|u| crate::job::validate_machine_uid(u).is_ok());
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
    let peer = crate::peer::peer_target(&target, &entry, None, port, false, provider.as_ref())?.at_listener(port);
    let peer = settle_pin(peer, &entry, &target, pins, provider.provider_name())?;
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
    })
}

/// Submit `job` to the machine it is addressed to ([`send_job`]). A refusal
/// comes back as `Err` carrying a [`SubmitRefused`] (the receiver's reason,
/// control characters removed, and its typed code).
pub fn submit_work(job: WorkJob, wait: bool) -> Result<SubmissionReply> {
    let target = job.target_machine.clone();
    let (code, reply) = send_job(job, wait, PinPolicy::PinOnFirstContact)?;
    reply_outcome(&target, code, reply)
}

/// A receiver refused a job. Carried by [`submit_work`]'s `Err` so a caller
/// reads the [`RefusalCode`], not the sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitRefused {
    pub code: RefusalCode,
    /// The receiver's sentence, control characters removed. For a person.
    pub reason: String,
}

impl std::fmt::Display for SubmitRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for SubmitRefused {}

/// What a receiver's final reply means to the sender: the reply for a job it
/// took, an error for one it did not, ran badly, or answered in a status this
/// darkmux does not know (never read as taken).
fn reply_outcome(target: &str, code: u16, reply: SubmissionReply) -> Result<SubmissionReply> {
    match reply.status {
        ReplyStatus::Completed | ReplyStatus::Accepted | ReplyStatus::Queued => Ok(reply),
        ReplyStatus::Error => Err(anyhow!(
            "{target} accepted the job but the dispatch failed: {}",
            sanitize_remote_text(reply.reason.as_deref().unwrap_or("no reason given"))
        )),
        ReplyStatus::Unknown => Err(anyhow!(
            "{target} answered with a status this darkmux does not recognize (is it on a newer darkmux? \
             run the same version on both machines)"
        )),
        ReplyStatus::Checked => Err(anyhow!(
            "{target} answered a job with `checked`, which answers only a check: it ran nothing"
        )),
        ReplyStatus::Refused => Err(SubmitRefused {
            code: reply.refusal.unwrap_or(RefusalCode::Unknown),
            reason: reply
                .reason
                .as_deref()
                .map(sanitize_remote_text)
                .unwrap_or_else(|| format!("{target} refused the job (HTTP {code})")),
        }
        .into()),
    }
}

/// What asking a peer "would this route work" found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// A run would be taken: the profile it would run on, and what it meets.
    Routable { profile: String, report: CheckReport },
    /// A run would be refused, with the receiver's typed code and sentence.
    Refused { code: RefusalCode, reason: String },
    /// No usable answer: the peer could not be reached or verified, or
    /// answered something that is not a check's answer. `detail` says which.
    Unanswered { detail: String },
}

/// The one authoritative "would this route work": ask the machine in
/// `profile_address` (`profile@machine`) whether it would take a job for
/// `role` on that profile under `boundary`, by submitting a check
/// ([`SubmissionMode::Check`]). The receiver runs every gate a real job
/// meets (its token, the network identity, the allow-list, role and profile
/// scope, the boundary, the seat, the version) and answers without running
/// anything, so the answer cannot drift from the decision a run would get.
/// Radio calls this (its submit follows, so pinning on first contact is fine);
/// doctor calls [`check_route_read_only`]. Neither re-derives it from
/// allow-lists or cards.
pub fn check_route(profile_address: &str, role: &str, boundary: Option<Boundary>) -> CheckOutcome {
    run_check(profile_address, role, boundary, PinPolicy::PinOnFirstContact)
        .unwrap_or_else(|e| CheckOutcome::Unanswered { detail: format!("{e:#}") })
}

/// What a read-only route check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOnlyCheck {
    /// The machine's node is not pinned in the roster, so nothing was sent
    /// (no token crossed) and the roster was not written. The first real
    /// send to it pins the node.
    NotPinned { machine: String },
    /// The target was pinned and was asked.
    Asked(CheckOutcome),
}

/// [`check_route`] for a caller that must not mutate state (doctor): the same
/// check, except that a target whose node is not pinned yet is not contacted
/// and the roster is never written.
pub fn check_route_read_only(profile_address: &str, role: &str, boundary: Option<Boundary>) -> ReadOnlyCheck {
    match run_check(profile_address, role, boundary, PinPolicy::RequirePinned) {
        Err(e) => match e.downcast::<NotPinned>() {
            Ok(n) => ReadOnlyCheck::NotPinned { machine: n.machine },
            Err(other) => ReadOnlyCheck::Asked(CheckOutcome::Unanswered { detail: format!("{other:#}") }),
        },
        Ok(outcome) => ReadOnlyCheck::Asked(outcome),
    }
}

fn run_check(
    profile_address: &str,
    role: &str,
    boundary: Option<Boundary>,
    pins: PinPolicy,
) -> Result<CheckOutcome> {
    let job = check_job(profile_address, role, boundary)?;
    let target = job.target_machine.clone();
    let (code, reply) = send_job(job, false, pins)?;
    Ok(check_outcome(&target, code, reply))
}

/// The job a check submits: the route's role, profile and boundary, an empty
/// message (a check sends no prompt text), and a session of its own.
fn check_job(profile_address: &str, role: &str, boundary: Option<Boundary>) -> Result<WorkJob> {
    let address = darkmux_types::profile_address::ProfileAddress::parse(profile_address)
        .map_err(|e| anyhow!("profile address `{profile_address}`: {e}"))?;
    let machine = address
        .machine
        .ok_or_else(|| anyhow!("profile address `{profile_address}` names no machine, so there is no route to check"))?;
    let run = darkmux_types::session_id::RunId::standalone("fleet-check").expect("a literal run id is never empty");
    let session = SessionId::adhoc(run, role, darkmux_crew::dispatch::fresh_nonce());
    let mut job = crate::build_work_job(
        machine,
        role.to_string(),
        String::new(),
        session,
        Some(address.profile),
        None,
        None,
        CHECK_TIMEOUT_SECONDS,
        darkmux_flow::resolve_machine_id(),
    );
    job.boundary = boundary;
    job.mode = SubmissionMode::Check;
    Ok(job)
}

/// A check's `timeout_seconds`: it bounds nothing (nothing runs) and only has
/// to pass the job's shape check.
const CHECK_TIMEOUT_SECONDS: u32 = 60;

/// A check's final reply, read into a [`CheckOutcome`].
fn check_outcome(target: &str, code: u16, reply: SubmissionReply) -> CheckOutcome {
    let unanswered = |detail: String| CheckOutcome::Unanswered { detail };
    match reply.status {
        ReplyStatus::Checked => match (reply.profile, reply.check) {
            (Some(profile), Some(report)) => CheckOutcome::Routable { profile, report },
            _ => unanswered(format!("{target} answered a check without saying what it found")),
        },
        ReplyStatus::Refused => CheckOutcome::Refused {
            code: reply.refusal.unwrap_or(RefusalCode::Unknown),
            reason: reply
                .reason
                .as_deref()
                .map(sanitize_remote_text)
                .unwrap_or_else(|| format!("{target} refused the check (HTTP {code})")),
        },
        ReplyStatus::Completed | ReplyStatus::Accepted | ReplyStatus::Queued | ReplyStatus::Error | ReplyStatus::Unknown => {
            unanswered(format!(
                "{target} answered a check with `{}`, which is not a check's answer (is it on a different darkmux version?)",
                serde_json::to_value(reply.status).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
            ))
        }
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
            repos: None,
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

    /// (#755) `repos` is reserved for the git workspace handoff and read by
    /// nothing here: an allow-list entry carrying it loads from a real
    /// config.json and is admitted and scoped exactly like the same entry
    /// without it, `workspace: true` included.
    #[test]
    fn a_repos_grant_changes_no_admission_decision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let cfg = serde_json::json!({"fleet": {"accept_work": {"macbook-pro": {
            "node_id": "nLAPTOP", "profiles": ["host"], "roles": ["radio-host"],
            "images": ["rust:slim"], "workspace": true, "repos": ["darkmux", "docs"]}}}});
        std::fs::write(&path, cfg.to_string()).unwrap();
        let with = read_allow_list(&path).unwrap();
        assert_eq!(
            with["macbook-pro"].repos.as_deref(),
            Some(&["darkmux".to_string(), "docs".to_string()][..]),
            "the field is carried, not dropped into extras"
        );
        let mut without = with.clone();
        without.get_mut("macbook-pro").unwrap().repos = None;

        let admit_with = match_entry("nLAPTOP", "macbook-pro", &with).unwrap();
        assert_eq!(admit_with, match_entry("nLAPTOP", "macbook-pro", &without).unwrap());
        let mut j = job(None);
        j.workdir = Some("/anywhere".into());
        let scoped_with = check_scope("studio", None, &admit_with, &j, work("host"));
        let admit_without = match_entry("nLAPTOP", "macbook-pro", &without).unwrap();
        assert_eq!(scoped_with, check_scope("studio", None, &admit_without, &j, work("host")));
        assert!(scoped_with.is_ok(), "workspace still grants the path: {scoped_with:?}");

        // And an entry with `repos` but no `workspace` still refuses a workdir.
        let mut no_ws = with.clone();
        no_ws.get_mut("macbook-pro").unwrap().workspace = None;
        let a = match_entry("nLAPTOP", "macbook-pro", &no_ws).unwrap();
        assert!(matches!(
            check_scope("studio", None, &a, &j, work("host")),
            Err(Refusal::OutOfScope { item: OutOfScope::Workspace, .. })
        ));
    }

    fn job(profile: Option<&str>) -> WorkJob {
        WorkJob {
            target_machine: "studio".into(),
            target_machine_uid: None,
            role_id: "radio-host".into(),
            message: "hi".into(),
            session_id: crate::test_session("s-1"),
            profile: profile.map(str::to_string),
            workdir: None,
            image: None,
            timeout_seconds: 60,
            published_at_unix_ms: 1,
            published_by_machine: Some("macbook-pro".into()),
            single_shot: None,
            boundary: None,
            mode: SubmissionMode::Run,
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
    fn decide(token: TokenCheck, node: Node, prof: Prof) -> std::result::Result<String, Refusal> {
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
        check_scope("studio", None, &admitted, &job(None), resolution).map(|s| s.profile)
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
                    let got = decide(t, n, p);
                    let want: std::result::Result<String, fn(&Refusal) -> bool> = match (t, n, p) {
                        (TokenCheck::Mismatch, _, _) => Err(|r| matches!(r, Refusal::Token)),
                        (TokenCheck::NotConfigured, _, _) => Err(|r| matches!(r, Refusal::NoTokenConfigured)),
                        (_, Node::Unknown, _) => Err(|r| matches!(r, Refusal::NotAllowed { node_name, .. } if node_name == "phone")),
                        (_, Node::NotOnOverlay, _) => Err(|r| matches!(r, Refusal::NotOnOverlay { .. })),
                        (_, Node::Unresolvable, _) => Err(|r| matches!(r, Refusal::IdentityUnavailable { .. })),
                        (_, Node::Allowed, Prof::InScope) => Ok("host".to_string()),
                        (_, Node::Allowed, Prof::OutOfScope) => Err(|r| matches!(r, Refusal::OutOfScope { item: OutOfScope::Profile { profile, .. }, .. } if profile == "coder-big")),
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

    /// The promise: a caller is AUTHENTICATED without being authorized. A node
    /// that holds the token and that the network names is a verified fleet
    /// peer, and the allow-list is not read to say so (a card read needs no
    /// grant); authorizing it is a separate step that reads the allow-list.
    #[test]
    fn authenticating_needs_no_allow_list_and_authorizing_is_a_separate_step() {
        let peer: IpAddr = "100.64.0.7".parse().unwrap();
        let stranger = test_node("nPHONE", "phone", "100.64.0.7");
        let node = authenticate(TokenCheck::Match, || Ok(Some(stranger.clone())), "tailscale", peer, Some("nSTUDIO"))
            .expect("a node the network names, holding the token, is authenticated");
        assert_eq!(node.node_id, "nPHONE");
        assert!(matches!(authorize(&node, || Ok(allow())), Err(Refusal::NotAllowed { node_name, .. }) if node_name == "phone"));
        let listed = test_node("nLAPTOP", "macbook-pro", "100.64.0.7");
        assert_eq!(authorize(&listed, || Ok(allow())).unwrap().peer_name, "macbook-pro");
        let unreadable = authorize(&listed, || Err("no such file".into()));
        assert!(matches!(unreadable, Err(Refusal::BadRequest(_))), "an unreadable allow-list refuses: {unreadable:?}");
    }

    /// Authentication still refuses this machine's own node, a node the
    /// network does not name, and a provider that cannot answer.
    #[test]
    fn authenticate_refuses_self_unplaced_and_unanswered_callers() {
        let peer: IpAddr = "100.64.0.7".parse().unwrap();
        let me = test_node("nSTUDIO", "studio", "100.64.0.7");
        assert!(matches!(authenticate(TokenCheck::Match, || Ok(Some(me)), "t", peer, Some("nSTUDIO")), Err(Refusal::FromSelf)));
        assert!(matches!(authenticate(TokenCheck::Match, || Ok(None), "t", peer, None), Err(Refusal::NotOnOverlay { .. })));
        assert!(matches!(
            authenticate(TokenCheck::Match, || Err("down".into()), "t", peer, None),
            Err(Refusal::IdentityUnavailable { .. })
        ));
    }

    /// A status this darkmux does not know is an error to the sender, whatever
    /// else the reply carries: never a job that was taken.
    #[test]
    fn an_unrecognized_final_status_is_an_error_not_a_taken_job() {
        let reply = SubmissionReply { exit_code: Some(0), ..SubmissionReply::of(ReplyStatus::Unknown) };
        let err = reply_outcome("studio", 200, reply).unwrap_err().to_string();
        assert!(err.contains("does not recognize"), "{err}");
        assert!(reply_outcome("studio", 200, SubmissionReply::of(ReplyStatus::Completed)).is_ok());
    }

    /// A reply status a newer darkmux sends reads as `unknown`, not as a parse
    /// failure that discards the reply.
    #[test]
    fn a_reply_status_this_darkmux_does_not_know_reads_as_unknown() {
        let r: SubmissionReply = serde_json::from_str(r#"{"status": "paused", "machine": "studio"}"#).unwrap();
        assert_eq!(r.status, ReplyStatus::Unknown);
        assert_eq!(r.machine.as_deref(), Some("studio"));
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
        assert!(matches!(check_scope("studio", None, &admitted, &j, work()), Err(Refusal::OutOfScope { item: OutOfScope::Role { ref role, .. }, .. }) if role == "coder"));
        let no_roles = Admitted { roles: vec![], ..admitted.clone() };
        assert!(matches!(check_scope("studio", None, &no_roles, &job(None), work()), Err(Refusal::OutOfScope { item: OutOfScope::Role { .. }, .. })), "absent roles = none");
        let mut j = job(None);
        j.image = Some("evil.example/x:latest".into());
        assert!(matches!(check_scope("studio", None, &admitted, &j, work()), Err(Refusal::OutOfScope { item: OutOfScope::Image { .. }, .. })));
        j.image = Some("rust:slim".into());
        assert_eq!(check_scope("studio", None, &admitted, &j, work()).unwrap().profile, "host");
        let mut j = job(None);
        j.target_machine = "Studio".into();
        assert_eq!(check_scope("studio", None, &admitted, &j, work()).unwrap().profile, "host", "case-insensitive");
    }

    /// (#3028) A submission is written at the lowest version that can say it:
    /// a job with no target uid is an 8.0 submission an 8.0 receiver takes,
    /// one carrying the uid is 8.1 and an 8.0 receiver refuses it by version.
    #[test]
    fn a_submission_is_written_at_the_lowest_version_that_expresses_it() {
        let plain = WorkSubmission::new(job(None), true);
        assert_eq!(plain.schema, "8.0");
        let mut with_uid = job(None);
        with_uid.target_machine_uid = Some("UID-S".into());
        let with_uid = WorkSubmission::new(with_uid, true);
        assert_eq!(with_uid.schema, "8.1");
        let eight_oh = WorkVersion::parse("8.0").unwrap();
        let body = |s: &WorkSubmission| serde_json::to_vec(s).unwrap();
        assert!(WorkSubmission::parse_for(&body(&plain), eight_oh).is_ok(), "an 8.0 receiver takes the uid-less job");
        assert_eq!(
            WorkSubmission::parse_for(&body(&with_uid), eight_oh).unwrap_err(),
            Refusal::SchemaMismatch { got: "8.1".into() }
        );
        assert!(WorkSubmission::parse(&body(&plain)).is_ok() && WorkSubmission::parse(&body(&with_uid)).is_ok());
    }

    fn laptop_admitted() -> Admitted {
        Admitted {
            node_id: "nLAPTOP".into(),
            peer_name: "macbook-pro".into(),
            profiles: vec!["host".into()],
            roles: vec!["radio-host".into()],
            images: vec![],
            workspace: false,
        }
    }

    /// (#3028) The headline: the receiver renamed itself `studio`; a job
    /// addressed to its old name carries its uid and is taken.
    #[test]
    fn a_job_naming_the_receivers_uid_is_taken_under_any_name() {
        let mut j = job(None);
        j.target_machine = "m1-max-32gb-studio".into();
        j.target_machine_uid = Some("abcd-1234".into());
        let scoped = check_scope("studio", Some("ABCD-1234"), &laptop_admitted(), &j, work("host"));
        assert_eq!(scoped.unwrap().profile, "host", "the uid decides, case-insensitively, whatever the name");
    }

    /// (#3028) A different machine answering to the receiver's name is not
    /// the machine the sender meant.
    #[test]
    fn a_job_naming_another_machines_uid_is_misaddressed_even_under_the_receivers_name() {
        let mut j = job(None);
        j.target_machine_uid = Some("OTHER-UID".into());
        let refusal = check_scope("studio", Some("MY-UID"), &laptop_admitted(), &j, work("host")).unwrap_err();
        assert!(matches!(refusal, Refusal::Misaddressed { .. }), "{refusal:?}");
        let sentence = refusal.reason("studio");
        assert!(!sentence.contains("OTHER-UID") && !sentence.contains("MY-UID"), "no uid is printed: {sentence}");
    }

    /// (#3028) No uid on the job (an 8.0 sender, or one that has not learned
    /// it), or none on the receiver (it cannot read its own): the name check
    /// is what it was.
    #[test]
    fn without_a_uid_on_either_side_the_name_decides() {
        let mut j = job(None);
        j.target_machine = "m1-max-32gb-studio".into();
        assert!(matches!(
            check_scope("studio", Some("MY-UID"), &laptop_admitted(), &j, work("host")),
            Err(Refusal::Misaddressed { .. })
        ));
        j.target_machine_uid = Some("SOME-UID".into());
        assert!(
            matches!(check_scope("studio", None, &laptop_admitted(), &j, work("host")), Err(Refusal::Misaddressed { .. })),
            "a receiver that cannot name its own uid cannot confirm one"
        );
        j.target_machine = "studio".into();
        assert!(check_scope("studio", None, &laptop_admitted(), &j, work("host")).is_ok());
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
            check_scope("studio", None, &admitted, &j, work("host")),
            Err(Refusal::Misaddressed { .. })
        ));
        let mut j = job(None);
        j.workdir = Some("/tmp/x".into());
        assert!(matches!(
            check_scope("studio", None, &admitted, &j, work("host")),
            Err(Refusal::OutOfScope { item: OutOfScope::Workspace, .. })
        ));
        let with_ws = Admitted { workspace: true, ..admitted.clone() };
        assert_eq!(check_scope("studio", None, &with_ws, &j, work("host")).unwrap().profile, "host");
        assert!(matches!(
            check_scope("studio", None, &admitted, &job(None), ProfileResolution::Unresolved("x".into())),
            Err(Refusal::NoWorkProfile { .. })
        ));
    }

    fn roster_with(entries: &[(&str, &str)]) -> crate::roster::FleetRoster {
        let machines: serde_json::Map<String, serde_json::Value> = entries
            .iter()
            .map(|(id, address)| (id.to_string(), serde_json::json!({ "id": id, "address": address, "added_unix_ms": 1 })))
            .collect();
        serde_json::from_value(serde_json::json!({ "machines": machines })).expect("a roster")
    }

    fn body(role: &str, claimed: Option<&str>) -> Vec<u8> {
        let mut j = serde_json::to_value(job(None)).unwrap();
        j["role_id"] = role.into();
        match claimed {
            Some(c) => j["published_by_machine"] = c.into(),
            None => {
                j.as_object_mut().unwrap().remove("published_by_machine");
            }
        }
        serde_json::to_vec(&serde_json::json!({ "job": j })).unwrap()
    }

    /// The remedy names the receiver's roster entry for the verified node
    /// (its id, not the network name) and the role that was asked for.
    #[test]
    fn the_trust_remedy_names_the_roster_entry_for_the_node() {
        let node = test_node("nLAPTOP", "laptop", "100.64.0.2");
        let roster = roster_with(&[("Laptop-Mac", "laptop.tailnet-example.ts.net:8765"), ("mini", "mini:8765")]);
        let ask = TrustAsk::for_sender(&node, &roster, &body("radio-host", Some("workbook")));
        let said = Refusal::NotAllowed { node_name: "laptop".into(), ask }.reason("studio");
        assert!(
            said.contains("`darkmux machine trust Laptop-Mac --profiles <profile> --roles radio-host`"),
            "{said}"
        );
        assert!(!said.contains("--node"), "the roster address already finds the node: {said}");
    }

    /// No roster entry: the sender's claimed machine id is NAME, and `--node`
    /// names the node. A claim that is not a machine name is ignored.
    #[test]
    fn the_trust_remedy_falls_back_to_the_claimed_id_then_the_node() {
        let node = test_node("nLAPTOP", "laptop", "100.64.0.2");
        let empty = crate::roster::FleetRoster::default();
        let said = |claimed: Option<&str>| {
            let ask = TrustAsk::for_sender(&node, &empty, &body("radio-host", claimed));
            Refusal::NotAllowed { node_name: "laptop".into(), ask }.reason("studio")
        };
        assert!(said(Some("Laptop-Mac")).contains("`darkmux machine trust Laptop-Mac --node laptop --profiles <profile> --roles radio-host`"), "{}", said(Some("Laptop-Mac")));
        for hostile in [None, Some("a b; rm -rf"), Some("")] {
            let s = said(hostile);
            assert!(s.contains("`darkmux machine trust laptop --profiles <profile> --roles radio-host`"), "{s}");
        }
        let junk_role = TrustAsk::for_sender(&node, &empty, &body("no spaces allowed", None));
        assert_eq!(junk_role.role, None);
    }

    /// The replies say what the operator needs, and never carry a node id.
    #[test]
    fn refusal_replies_name_the_reason() {
        let r = Refusal::NotAllowed { node_name: "macbook-pro".into(), ask: TrustAsk::default() };
        assert!(r.reason("studio").starts_with("studio does not accept work from macbook-pro"), "{}", r.reason("studio"));
        assert_eq!(r.http_status(), 403);
        let r = Refusal::OutOfScope { peer: "macbook-pro".into(), item: OutOfScope::Profile { profile: "x".into(), allowed: vec!["host".into()] } };
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

    fn body_at(schema: &str) -> Vec<u8> {
        let mut v: serde_json::Value = serde_json::to_value(WorkSubmission::new(job(Some("host")), true)).unwrap();
        v["schema"] = schema.into();
        serde_json::to_vec(&v).unwrap()
    }

    /// The version rule: the same major with a minor at or below the
    /// receiver's own is taken; a newer minor and another major are refused
    /// naming both versions, and so is a schema that is not `major.minor`.
    #[test]
    fn the_receiver_takes_an_older_minor_and_refuses_a_newer_one_or_another_major() {
        let receiver = WorkVersion { major: 8, minor: 2 };
        for taken in ["8.0", "8.1", "8.2"] {
            assert!(WorkSubmission::parse_for(&body_at(taken), receiver).is_ok(), "{taken}");
        }
        for refused in ["8.3", "9.0", "7.2", "8", "v8.0", "8.0.0"] {
            assert_eq!(
                WorkSubmission::parse_for(&body_at(refused), receiver).unwrap_err(),
                Refusal::SchemaMismatch { got: refused.into() },
                "{refused}"
            );
        }
        // The current version parses through the public entry point.
        assert!(WorkSubmission::parse(&body_at(WORK_JOB_SCHEMA_VERSION)).is_ok());
        let newer = Refusal::SchemaMismatch { got: "8.9".into() }.reason("studio");
        assert!(newer.contains(&format!("v{WORK_JOB_SCHEMA_VERSION}")) && newer.contains("v8.9"), "both versions named: {newer}");
    }

    /// Every refusal maps to exactly one code, and its reply carries it.
    #[test]
    fn every_refusal_carries_its_code_on_the_wire() {
        let s = || "x".to_string();
        let all: Vec<(Refusal, RefusalCode)> = vec![
            (Refusal::NoTokenConfigured, RefusalCode::Token),
            (Refusal::Token, RefusalCode::Token),
            (Refusal::IdentityUnavailable { provider: s(), detail: s() }, RefusalCode::Identity),
            (Refusal::NotOnOverlay { provider: s(), addr: s() }, RefusalCode::Identity),
            (Refusal::NotAllowed { node_name: s(), ask: TrustAsk::default() }, RefusalCode::NotListed),
            (Refusal::AmbiguousEntry { names: vec![s()] }, RefusalCode::BadConfig),
            (Refusal::Misaddressed { target: s() }, RefusalCode::Misaddressed),
            (Refusal::OutOfScope { peer: s(), item: OutOfScope::Workspace }, RefusalCode::WorkspaceNotAllowed),
            (Refusal::OutOfScope { peer: s(), item: OutOfScope::Role { role: s(), allowed: vec![] } }, RefusalCode::RoleNotAllowed),
            (Refusal::OutOfScope { peer: s(), item: OutOfScope::Image { image: s() } }, RefusalCode::ImageNotAllowed),
            (Refusal::FromSelf, RefusalCode::FromSelf),
            (Refusal::OutOfScope { peer: s(), item: OutOfScope::Profile { profile: s(), allowed: vec![] } }, RefusalCode::ProfileNotAllowed),
            (Refusal::UtilityProfile { profile: s() }, RefusalCode::ProfileNotAllowed),
            (Refusal::NoWorkProfile { role: s(), detail: s() }, RefusalCode::ProfileUndefined),
            (Refusal::SchemaMismatch { got: s() }, RefusalCode::Version),
            (Refusal::BadRequest(s()), RefusalCode::BadRequest),
            (Refusal::Busy { what: s() }, RefusalCode::Busy),
            (Refusal::QueueFull { peer: s(), what: s() }, RefusalCode::Busy),
            (Refusal::SeatChanged { profile: s() }, RefusalCode::SeatChanged),
            (Refusal::TooManyAtOnce { peer: s() }, RefusalCode::Busy),
            (Refusal::BoundaryUnmanaged { profile: s() }, RefusalCode::Boundary),
            (Refusal::BoundaryUnknown, RefusalCode::Boundary),
            (Refusal::BadConfig { detail: s() }, RefusalCode::BadConfig),
        ];
        for (refusal, code) in all {
            assert_eq!(refusal.code(), code, "{refusal:?}");
            let line = serde_json::to_string(&refusal.reply("studio")).unwrap();
            let back: SubmissionReply = serde_json::from_str(&line).unwrap();
            assert_eq!((back.status, back.refusal), (ReplyStatus::Refused, Some(code)), "{line}");
        }
    }

    /// The wire spellings consumers match on, and a code from a newer darkmux
    /// reads as `unknown`, never as a known kind.
    #[test]
    fn refusal_codes_are_snake_case_and_a_newer_code_is_unknown() {
        let spelled = |c: RefusalCode| serde_json::to_value(c).unwrap();
        assert_eq!(spelled(RefusalCode::FromSelf), "self");
        assert_eq!(spelled(RefusalCode::NotListed), "not_listed");
        assert_eq!(spelled(RefusalCode::ProfileUndefined), "profile_undefined");
        assert_eq!(spelled(RefusalCode::Boundary), "boundary");
        let newer: SubmissionReply = serde_json::from_str(r#"{"status":"refused","refusal":"quota_exceeded"}"#).unwrap();
        assert_eq!(newer.refusal, Some(RefusalCode::Unknown));
        let plain: SubmissionReply = serde_json::from_str(r#"{"status":"refused"}"#).unwrap();
        assert_eq!(plain.refusal, None);
    }

    fn admitted_for_boundary() -> Admitted {
        Admitted {
            node_id: "nLAPTOP".into(),
            peer_name: "macbook-pro".into(),
            profiles: vec!["host".into(), "cloud".into()],
            roles: vec!["radio-host".into()],
            images: vec![],
            workspace: false,
        }
    }

    fn hosted(profile: &str) -> ProfileResolution {
        ProfileResolution::Work { profile: profile.into(), seat: crate::seats::WorkSeat::Unmanaged { endpoint: "azure".into(), model: "gpt".into(), concurrent_calls: None } }
    }

    /// The boundary is checked against the profile the job RESOLVES to:
    /// `managed_only` is met by a local model and refused on a hosted one; a
    /// job with no boundary is unrestricted; a boundary this receiver cannot
    /// enforce is refused whatever the profile is.
    #[test]
    fn scope_enforces_the_boundary_against_the_resolved_profile() {
        let admitted = admitted_for_boundary();
        let with = |boundary| WorkJob { boundary, ..job(None) };
        let managed = Some(Boundary::ManagedOnly);
        assert_eq!(check_scope("studio", None, &admitted, &with(managed), work("host")).unwrap().profile, "host");
        assert_eq!(
            check_scope("studio", None, &admitted, &with(managed), hosted("cloud")).unwrap_err(),
            Refusal::BoundaryUnmanaged { profile: "cloud".into() }
        );
        assert_eq!(check_scope("studio", None, &admitted, &with(None), hosted("cloud")).unwrap().profile, "cloud");
        for resolution in [work("host"), hosted("cloud")] {
            assert_eq!(
                check_scope("studio", None, &admitted, &with(Some(Boundary::Unknown)), resolution).unwrap_err(),
                Refusal::BoundaryUnknown
            );
        }
        // An out-of-scope profile is refused as such before its kind is told.
        let narrow = Admitted { profiles: vec!["host".into()], ..admitted };
        assert!(matches!(
            check_scope("studio", None, &narrow, &with(managed), hosted("cloud")),
            Err(Refusal::OutOfScope { item: OutOfScope::Profile { .. }, .. })
        ));
    }

    /// The `managed`/`unmanaged` decision comes from the resolution every
    /// dispatch uses: a registry's endpoint kinds decide the seat the
    /// boundary reads, and the machine card's `endpoint_kind` reads the same
    /// `target_for` kind.
    #[test]
    fn the_boundary_reads_the_kind_target_for_resolves() {
        let reg = registry(
            r#"{"profiles":{"local":{"models":[{"id":"m","n_ctx":8000}]},"cloud":{"models":[{"id":"h","n_ctx":8000,"endpoint":"az"}]}},
                "endpoints":{"az":{"url":"https://example.invalid/v1"}}}"#,
        );
        let role = darkmux_crew::loader::load_roles().unwrap().into_iter().find(|r| r.id == "radio-host").unwrap();
        let admitted = Admitted { profiles: vec!["local".into(), "cloud".into()], ..admitted_for_boundary() };
        let job = |profile: &str| WorkJob { profile: Some(profile.into()), boundary: Some(Boundary::ManagedOnly), ..job(None) };
        let scoped = |profile: &str| {
            check_scope("studio", None, &admitted, &job(profile), classify_profile(&reg, &role, Some(profile), None, "studio"))
        };
        assert_eq!(scoped("local").unwrap().profile, "local");
        assert_eq!(scoped("cloud").unwrap_err(), Refusal::BoundaryUnmanaged { profile: "cloud".into() });
    }

    /// A check's reply reads into a typed outcome; anything that is not a
    /// check's answer is unanswered, never routable.
    #[test]
    fn a_checks_reply_reads_into_a_typed_outcome() {
        let checked = SubmissionReply {
            profile: Some("deep".into()),
            check: Some(CheckReport { endpoint: EndpointClass::Managed, seat: SeatOutlook::Free }),
            ..SubmissionReply::of(ReplyStatus::Checked)
        };
        assert_eq!(
            check_outcome("studio", 200, checked.clone()),
            CheckOutcome::Routable { profile: "deep".into(), report: CheckReport { endpoint: EndpointClass::Managed, seat: SeatOutlook::Free } }
        );
        assert!(matches!(
            check_outcome("studio", 200, SubmissionReply { check: None, ..checked }),
            CheckOutcome::Unanswered { .. }
        ));
        let refused = Refusal::BoundaryUnmanaged { profile: "cloud".into() }.reply("studio");
        assert!(matches!(
            check_outcome("studio", 403, refused),
            CheckOutcome::Refused { code: RefusalCode::Boundary, .. }
        ));
        for status in [ReplyStatus::Completed, ReplyStatus::Accepted, ReplyStatus::Queued, ReplyStatus::Error, ReplyStatus::Unknown] {
            assert!(matches!(check_outcome("studio", 200, SubmissionReply::of(status)), CheckOutcome::Unanswered { .. }), "{status:?}");
        }
    }

    /// A refused job's sender gets the code, not just the sentence; a
    /// `checked` answer to a job is an error, never a success.
    #[test]
    fn a_refusal_reaches_the_sender_as_a_typed_error() {
        let err = reply_outcome("studio", 403, Refusal::BoundaryUnmanaged { profile: "cloud".into() }.reply("studio")).unwrap_err();
        let refused = err.downcast_ref::<SubmitRefused>().expect("a typed refusal");
        assert_eq!(refused.code, RefusalCode::Boundary);
        assert!(refused.reason.contains("managed_only"), "{}", refused.reason);
        let old_receiver = SubmissionReply { reason: Some("no code".into()), ..SubmissionReply::of(ReplyStatus::Refused) };
        let err = reply_outcome("studio", 403, old_receiver).unwrap_err();
        assert_eq!(err.downcast_ref::<SubmitRefused>().unwrap().code, RefusalCode::Unknown);
        assert!(reply_outcome("studio", 200, SubmissionReply::of(ReplyStatus::Checked)).is_err());
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
            current_name: None,
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
            ProfileResolution::Work { profile: "cloud".into(), seat: crate::seats::WorkSeat::Unmanaged { endpoint: "api".into(), model: "gpt-x".into(), concurrent_calls: None } }
        );
        // (#3035) The endpoint's own `limits.concurrent_calls` rides the seat.
        let limited = registry(
            r#"{"profiles":{"cloud":{"models":[{"id":"gpt-x","n_ctx":32000,"endpoint":"api"}]}},
              "endpoints":{"api":{"url":"https://api.example/v1","limits":{"concurrent_calls":3}}},
              "default_profile":"cloud"}"#,
        );
        assert_eq!(
            classify_profile(&limited, &r, Some("cloud"), None, "studio"),
            ProfileResolution::Work { profile: "cloud".into(), seat: crate::seats::WorkSeat::Unmanaged { endpoint: "api".into(), model: "gpt-x".into(), concurrent_calls: Some(3) } }
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
