//! The fleet work job: what one machine asks another to run (#2916).
//!
//! Until 4.0 this was the payload of the Redis work queue (`darkmux:work`),
//! which any node that could write the hub's Redis could fill and any runner
//! would claim. That queue is retired (#2916): a job now travels in one HTTP
//! request, straight to the target machine's work-submission listener, which
//! checks the fleet token and the connecting node before it runs anything
//! (`submission.rs`). The job's SHAPE checks below are unchanged and still run
//! on both sides.

use anyhow::{anyhow, Result};
use darkmux_types::session_id::{SessionId, SessionKind};
use serde::{Deserialize, Serialize};

/// One unit of work one machine asks another to run: the body of a
/// work-submission request (`POST /fleet/work`, see `submission.rs`),
/// inside a [`crate::WorkSubmission`] envelope that carries the wire
/// version.
///
/// `#[serde(deny_unknown_fields)]` (PR-C.2): a sender cannot smuggle extra
/// fields that later receiver code might start interpreting. A shape change
/// is a deliberate [`WORK_JOB_SCHEMA_VERSION`] bump plus a struct edit.
///
/// Nothing in a job is trusted for authorization. `published_by_machine` is
/// the sender's own claim, kept for logs; WHO sent the job is answered by
/// the network (the identity provider) and the fleet token, never by a field
/// the sender writes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkJob {
    /// The machine the sender addressed (its `machine_id`, #2924). The
    /// receiver refuses a job addressed to another name, so a roster entry
    /// pointing at the wrong host fails loudly instead of running the work
    /// on the wrong machine. (It was an advisory hint while the queue let
    /// any runner claim any job; it is now a check.)
    pub target_machine: String,

    /// Role to dispatch against — resolved to a role manifest on the
    /// receiver.
    pub role_id: String,

    /// The operator's dispatch message — handed verbatim to the runtime.
    pub message: String,

    /// The sender's session. On the wire it is the session's wire string,
    /// read back strictly: a string outside the session grammar refuses
    /// the whole job. The receiver runs the job under a relay of it
    /// (`SessionId::relay`), never under the sender's session itself.
    pub session_id: SessionId,

    /// The receiver's profile to run on (`--profile`). `None` = the
    /// receiver resolves it as its own `dispatch` would (the role's
    /// `role_profiles` binding, else `default_profile`). Either way the
    /// RESOLVED profile is what the allow-list scope is checked against,
    /// and what runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,

    /// Optional `--workdir`, a path on the RECEIVER. Refused unless the
    /// sender's allow-list entry grants `workspace` (#755), and then only
    /// under the receiver's worktrees base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,

    /// (#703 Slice 4) Docker image the receiver dispatches into. `None` →
    /// the receiver's default runtime image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,

    /// Timeout (seconds) passed through to the dispatch.
    pub timeout_seconds: u32,

    /// Unix-millis when the sender built the job.
    pub published_at_unix_ms: u64,

    /// The sender's own `machine_id`, as it claims it. Logged only; never
    /// used to decide anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_by_machine: Option<String>,

    /// Run this job as ONE tool-less exchange under the radio answering
    /// persona (`radio-host`), not as an agent dispatch. See
    /// [`SingleShotJob`]. `None` = the receiver runs the role as an
    /// ordinary dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub single_shot: Option<SingleShotJob>,
}

/// The execution mode of a tool-less single exchange, the radio answering
/// seat's. It carries the PARAMETERS of the persona, never its text: the
/// receiver builds the system prompt from its own `radio-host` template
/// (`darkmux_crew::radio_persona::answering_system_prompt`), because the
/// role is the unit of trust in a receiver's allow-list and a sender must
/// not push an arbitrary system prompt. It adds no authority: the receiver's
/// allow-list still decides the role and the profile, and a `single_shot`
/// job for any role but `radio-host` is refused ([`WorkJob::validate`]).
///
/// The receiver runs the job through the same single-shot primitive the
/// sender uses for a local seat (no container, no agent loop, no autonomous
/// dispatch preamble), so the answer does not depend on which machine
/// served it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SingleShotJob {
    /// The persona's `{{humor}}` value (0..=100).
    pub humor: u8,
    /// The surface the answer is for; picks the persona's command wording.
    pub surface: darkmux_flow::payload::RadioSurface,
    /// The completion budget the sender asked for. The receiver runs under
    /// the smaller of this and its own cap
    /// (`darkmux_crew::radio_persona::peer_token_cap`).
    pub max_completion_tokens: u32,
}

/// Wire version of a work submission. History: "1"-"4" were the Redis
/// queue's `schema` tag (#590 single stream, #703 `image`, #1426 retired
/// `deliver`/`runtime`). "5" (#2916) is the first direct-submission shape:
/// `target_machine` became required, `profile` was added, and the dead
/// `attempt` / `published_by_orchestrator` fields were removed. The receiver
/// reads the envelope's `schema` BEFORE parsing the job, so a sender on
/// another version gets a reply naming the version, not a field error.
/// "6" (#2916 stage 2): the reply body became newline-delimited (a queued
/// job's `queued` lines before its answer) and `profile` never carries a
/// `profile@machine` address (the sender splits it off). 3.x senders speak
/// it. "7" (4.0): `session_id` (the job's and the reply's) is a session in
/// the 4.0 grammar (`darkmux_types::session_id`), read back strictly, so a
/// v6 sender's free-form session gets the version remedy, not a field
/// error. "8" (#2954): `phase_id` was removed with the hand-built mission
/// phase verbs; a v7 job that still carries it is refused with the version
/// remedy. "8" also gained the optional `single_shot` mode (unreleased, so
/// it rides the version): a sender that does not write it is unchanged.
pub const WORK_JOB_SCHEMA_VERSION: &str = "8";

/// Max byte size of a `WorkJob.message`. 256 KiB matches the
/// reasoning-text cap in `dispatch_internal.rs` (#231 / S6). (#246 PR-C.2)
pub(crate) const MAX_WORK_MESSAGE_BYTES: usize = 256 * 1024;

/// Max byte size of `WorkJob.workdir`. (#246 PR-C.2)
pub(crate) const MAX_WORK_WORKDIR_BYTES: usize = 4 * 1024;

/// Max length for identifier fields (`target_machine`, `role_id`,
/// `profile`). (#246 PR-C.2)
pub(crate) const MAX_WORK_IDENTIFIER_LEN: usize = 64;

/// Max allowed `timeout_seconds`. 1 hour bounds how long one sender can
/// hold a receiver's single work slot. (#246 PR-C.3)
pub(crate) const MAX_WORK_TIMEOUT_SECONDS: u32 = 60 * 60;

/// Max byte size of `WorkJob.image`. (#838 PR-C.2)
pub(crate) const MAX_WORK_IMAGE_BYTES: usize = 256;

impl WorkJob {
    /// Validate a job's SHAPE — called by the sender before the request
    /// leaves and by the receiver before anything runs. Charset and size
    /// only; who may run what is `submission.rs`'s job.
    ///
    /// - `target_machine`, `role_id`: `[a-z0-9_-]{1,64}`.
    /// - `profile`: 1..=64 printable ASCII, no whitespace (profile names
    ///   are the operator's own and may carry dots or capitals; the value
    ///   is only ever compared against names, never used as a path).
    /// - `message` ≤ 256 KiB, `workdir` ≤ 4 KiB, `timeout_seconds` in
    ///   1..=3600, `image` a conservative image reference.
    pub fn validate(&self) -> Result<()> {
        validate_machine_name("WorkJob.target_machine", &self.target_machine)?;
        validate_work_identifier("role_id", &self.role_id)?;
        validate_session_id(&self.session_id)?;
        if let Some(p) = &self.profile {
            if p.is_empty() || p.len() > MAX_WORK_IDENTIFIER_LEN || !p.chars().all(|c| c.is_ascii_graphic()) {
                return Err(anyhow!(
                    "WorkJob.profile must be 1..={MAX_WORK_IDENTIFIER_LEN} printable ASCII characters \
                     with no spaces: {p:?}"
                ));
            }
            // (#2916 stage 2) The sender splits `profile@machine`; the
            // machine rides in `target_machine`, never in `profile`.
            if p.contains('@') {
                return Err(anyhow!(
                    "WorkJob.profile is the receiver's own profile name and never contains `@` (the \
                     sender splits a `profile@machine` address): {p:?}"
                ));
            }
        }
        if self.message.len() > MAX_WORK_MESSAGE_BYTES {
            return Err(anyhow!(
                "WorkJob.message exceeds {}-byte cap (was {} bytes)",
                MAX_WORK_MESSAGE_BYTES,
                self.message.len()
            ));
        }
        if let Some(w) = &self.workdir {
            if w.len() > MAX_WORK_WORKDIR_BYTES {
                return Err(anyhow!(
                    "WorkJob.workdir exceeds {}-byte cap (was {} bytes)",
                    MAX_WORK_WORKDIR_BYTES,
                    w.len()
                ));
            }
        }
        if self.timeout_seconds == 0 {
            return Err(anyhow!(
                "WorkJob.timeout_seconds must be non-zero (0 would never complete)"
            ));
        }
        if self.timeout_seconds > MAX_WORK_TIMEOUT_SECONDS {
            return Err(anyhow!(
                "WorkJob.timeout_seconds exceeds {}-second cap (was {})",
                MAX_WORK_TIMEOUT_SECONDS,
                self.timeout_seconds
            ));
        }
        if let Some(img) = &self.image {
            validate_work_image(img)?;
        }
        if let Some(single_shot) = &self.single_shot {
            self.validate_single_shot(single_shot)?;
        }
        Ok(())
    }

    /// A `single_shot` job is one tool-less exchange under the radio
    /// persona: only the `radio-host` role has one, the persona's humor is a
    /// percentage, the budget is real, and there is no container or
    /// workspace for an `image` or `workdir` to apply to (refused rather
    /// than silently ignored).
    fn validate_single_shot(&self, single_shot: &SingleShotJob) -> Result<()> {
        if self.role_id != darkmux_crew::loader::RADIO_HOST_ROLE_ID {
            return Err(anyhow!(
                "WorkJob.single_shot is the radio answering seat's mode and only role `{}` has it \
                 (the job names role `{}`)",
                darkmux_crew::loader::RADIO_HOST_ROLE_ID,
                self.role_id
            ));
        }
        if single_shot.humor > 100 {
            return Err(anyhow!("WorkJob.single_shot.humor is a percentage, 0..=100 (was {})", single_shot.humor));
        }
        if single_shot.surface == darkmux_flow::payload::RadioSurface::Unknown {
            return Err(anyhow!(
                "WorkJob.single_shot.surface names no surface this darkmux knows (it answers for `cli` or `panel`)"
            ));
        }
        if single_shot.max_completion_tokens == 0 {
            return Err(anyhow!("WorkJob.single_shot.max_completion_tokens must be non-zero"));
        }
        if self.image.is_some() || self.workdir.is_some() {
            return Err(anyhow!(
                "WorkJob.single_shot runs one exchange with no container and no workspace, so it \
                 takes neither `image` nor `workdir`"
            ));
        }
        Ok(())
    }
}



/// Charset+length check for an identifier-shaped field — the canonical
/// validator used both at the submission boundary (`WorkJob::validate`) and
/// at the CLI boundary (Wave-E.5 #255).
///
/// Allowlist: `[a-z0-9_-]` (ASCII lowercase + digits + underscore +
/// hyphen), length 1..=MAX_WORK_IDENTIFIER_LEN. The full `label`
/// parameter lets callers name the offending field as the operator
/// thinks of it (`"mission_id"`, `"WorkJob.target_machine"`, etc.) so
/// errors are operator-actionable rather than internal-shape-leaky.
pub fn validate_identifier(label: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(anyhow!("{label} must be non-empty"));
    }
    if value.len() > MAX_WORK_IDENTIFIER_LEN {
        return Err(anyhow!(
            "{label} exceeds {}-char limit (was {} chars): {value:?}",
            MAX_WORK_IDENTIFIER_LEN,
            value.len()
        ));
    }
    let bad = value
        .chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '_'));
    if let Some(c) = bad {
        return Err(anyhow!(
            "{label} contains invalid char {c:?} (allowlist [a-z0-9_-]): {value:?}"
        ));
    }
    Ok(())
}

/// (#2916) A machine name (`machine_id`): `[A-Za-z0-9_-]`, 1..=64. Machine
/// names are ASCII case-INSENSITIVE everywhere (`host@MacBook-Pro`
/// and `host@macbook-pro` name the same machine; compare with
/// [`same_machine`]); the displayed name keeps its case. Capitals are
/// allowed because a hostname-derived `machine_id` has them.
pub fn validate_machine_name(label: &str, value: &str) -> Result<()> {
    match darkmux_types::profile_address::machine_name_problem(value) {
        Some(problem) => Err(anyhow!("{label}: {problem}")),
        None => Ok(()),
    }
}

/// (#2916) Whether two machine names name the same machine.
pub fn same_machine(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Max length of a submitted `session_id`.
pub(crate) const MAX_SESSION_ID_LEN: usize = 128;

/// (#2916) A submitted session id is part of file names and flow-record
/// join keys on the receiver. Its charset is the session grammar's (the
/// type cannot hold anything else); its length is bounded here. The bound
/// is on what the SENDER sent: the receiver executes under its relay of
/// that session, which escapes and wraps it and so is longer, and is the
/// receiver's own mint.
fn validate_session_id(value: &SessionId) -> Result<()> {
    let sent = match value.kind() {
        SessionKind::Relay { sender, .. } => sender.as_ref(),
        _ => value,
    };
    if sent.wire().len() > MAX_SESSION_ID_LEN {
        return Err(anyhow!("WorkJob.session_id must be at most {MAX_SESSION_ID_LEN} characters"));
    }
    Ok(())
}

/// Wraps `validate_identifier` with the `"WorkJob.{field}"` label
/// prefix used throughout `WorkJob::validate`. Kept as a thin shim so
/// the existing internal call-sites read tightly.
fn validate_work_identifier(field: &str, value: &str) -> Result<()> {
    validate_identifier(&format!("WorkJob.{field}"), value)
}

/// Validate a Docker image reference at the queue boundary.
/// Rejects empty strings, values starting with `-` (docker-flag injection,
/// #838), and any char outside the conservative image-ref charset
/// `[A-Za-z0-9._/:@-]`. Also enforces a byte-size cap.
///
/// The allowlist covers the full Docker reference grammar:
/// - registry host (`myregistry.io`)
/// - slash-separated path segments (`org/repo`, `a/b/c`)
/// - colon tag (`:latest`, `:v1.2.3`)
/// - at digest (`@sha256:...`)
/// - dots, underscores, hyphens in names
pub fn validate_image_ref(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(anyhow!("image must be non-empty"));
    }
    if value.len() > MAX_WORK_IMAGE_BYTES {
        return Err(anyhow!(
            "image exceeds {}-byte cap (was {} bytes): {value:?}",
            MAX_WORK_IMAGE_BYTES,
            value.len()
        ));
    }
    if value.starts_with('-') {
        return Err(anyhow!(
            "image must not start with '-' (prevents docker-flag injection, #838): {value:?}"
        ));
    }
    let bad = value
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '.' || *c == '_' || *c == '/' || *c == '-' || *c == ':' || *c == '@'));
    if let Some(c) = bad {
        return Err(anyhow!(
            "image contains invalid char {c:?} (allowlist [A-Za-z0-9._/:@-]): {value:?}"
        ));
    }
    Ok(())
}

/// Wraps `validate_image_ref` with the `"WorkJob.image"` label prefix.
fn validate_work_image(value: &str) -> Result<()> {
    validate_image_ref(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid WorkJob with all optional fields None.
    fn make_valid_job() -> WorkJob {
        WorkJob {
            target_machine: "studio".to_string(),
            role_id: "test-role".to_string(),
            message: "hello".to_string(),
            session_id: SessionId::adhoc(darkmux_types::session_id::RunId::mission("m-1").unwrap(), "test-role", "sess-1"),
            profile: None,
            workdir: None,
            image: None,
            timeout_seconds: 60,
            published_at_unix_ms: 1_700_000_000_000,
            published_by_machine: None,
            single_shot: None,
        }
    }

    /// (#2916) `target_machine` is required and checked like every
    /// identifier; `profile` allows the operator's own spellings but no
    /// whitespace or control characters.
    #[test]
    fn validate_checks_target_machine_and_profile() {
        let mut job = make_valid_job();
        job.target_machine = "MacBook-Pro".into();
        assert!(job.validate().is_ok(), "machine names may carry capitals (#2916)");
        job.target_machine = "studio.lan".into();
        assert!(job.validate().unwrap_err().to_string().contains("target_machine"));
        assert!(same_machine("MacBook-Pro", "macbook-pro"));
        assert!(!same_machine("macbook-pro", "macbook-pr0"));
        let mut job = make_valid_job();
        job.profile = Some("Coder.Studio-v2".into());
        assert!(job.validate().is_ok());
        for bad in ["", "has space", "tab\t", "x".repeat(65).as_str()] {
            job.profile = Some(bad.to_string());
            assert!(job.validate().unwrap_err().to_string().contains("profile"), "{bad:?}");
        }
    }

    fn single_shot() -> SingleShotJob {
        SingleShotJob { humor: 40, surface: darkmux_flow::payload::RadioSurface::Cli, max_completion_tokens: 16_384 }
    }

    /// The answering seat's mode is the radio-host role's alone: it adds no
    /// authority, so any other role, an out-of-range persona value, a zero
    /// budget, or a container/workspace field (nothing to apply them to) is
    /// refused rather than ignored.
    #[test]
    fn validate_single_shot_is_the_radio_host_roles_alone() {
        let mut job = make_valid_job();
        job.role_id = "radio-host".into();
        job.single_shot = Some(single_shot());
        assert!(job.validate().is_ok());

        let mut other_role = job.clone();
        other_role.role_id = "coder".into();
        let err = other_role.validate().unwrap_err().to_string();
        assert!(err.contains("only role `radio-host` has it") && err.contains("`coder`"), "{err}");

        let mut humor = job.clone();
        humor.single_shot = Some(SingleShotJob { humor: 101, ..single_shot() });
        assert!(humor.validate().unwrap_err().to_string().contains("humor"));

        let mut budget = job.clone();
        budget.single_shot = Some(SingleShotJob { max_completion_tokens: 0, ..single_shot() });
        assert!(budget.validate().unwrap_err().to_string().contains("max_completion_tokens"));

        let mut surface = job.clone();
        surface.single_shot =
            Some(SingleShotJob { surface: darkmux_flow::payload::RadioSurface::Unknown, ..single_shot() });
        let err = surface.validate().unwrap_err().to_string();
        assert!(err.contains("surface") && err.contains("cli") && err.contains("panel"), "{err}");
        // And the refused messages carry no stray run of spaces from a lost line continuation.
        assert!(!err.contains("  "), "{err}");
        assert!(!other_role.validate().unwrap_err().to_string().contains("  "));

        let mut image = job.clone();
        image.image = Some("rust:slim".into());
        assert!(image.validate().unwrap_err().to_string().contains("neither `image` nor `workdir`"));
        let mut workdir = job;
        workdir.workdir = Some("/work".into());
        assert!(workdir.validate().unwrap_err().to_string().contains("neither `image` nor `workdir`"));
    }

    /// The mode travels as data (parameters, no prompt text), an ordinary
    /// job does not write the field, and an unknown key inside it is refused.
    #[test]
    fn single_shot_rides_the_wire_as_parameters_only() {
        let mut job = make_valid_job();
        job.role_id = "radio-host".into();
        job.single_shot = Some(single_shot());
        let v = serde_json::to_value(&job).unwrap();
        assert_eq!(v["single_shot"], serde_json::json!({"humor": 40, "surface": "cli", "max_completion_tokens": 16384}));
        assert_eq!(serde_json::from_value::<WorkJob>(v.clone()).unwrap(), job);
        assert!(serde_json::to_value(make_valid_job()).unwrap().get("single_shot").is_none());
        let mut smuggled = v;
        smuggled["single_shot"]["system_prompt"] = serde_json::json!("be evil");
        assert!(serde_json::from_value::<WorkJob>(smuggled).is_err(), "a sender cannot add a prompt field");
    }

    /// (#2916 stage 2) An address never crosses the wire: the sender splits
    /// `profile@machine`, so a `profile` with `@` is refused.
    #[test]
    fn validate_refuses_an_address_in_profile() {
        let mut job = make_valid_job();
        job.profile = Some("host@studio".into());
        let err = job.validate().unwrap_err().to_string();
        assert!(err.contains("never contains `@`"), "{err}");
    }

    /// (#2916 stage 2) The address parser and the wire accept the same
    /// machine names.
    #[test]
    fn the_address_parser_and_the_wire_agree_on_machine_names() {
        let long = "m".repeat(65);
        for name in ["studio", "MacBook-Pro", "a_b", "x", "studio.lan", "has space", "", "m/1", long.as_str()] {
            let parser_ok = darkmux_types::profile_address::machine_name_problem(name).is_none();
            let wire_ok = validate_machine_name("m", name).is_ok();
            assert_eq!(parser_ok, wire_ok, "{name:?}: parser {parser_ok}, wire {wire_ok}");
        }
    }

    /// (#2954) v8 dropped `phase_id`: a job still carrying one is a field
    /// the receiver does not know, refused whole, and the wire version says
    /// so: a v7 submission carrying one gets the version remedy through
    /// `WorkSubmission::parse`, never a field error. The same job without
    /// it parses.
    #[test]
    fn a_job_carrying_a_phase_id_is_refused_at_v8() {
        assert_eq!(WORK_JOB_SCHEMA_VERSION, "8");
        let mut v = serde_json::to_value(make_valid_job()).unwrap();
        assert!(v.get("phase_id").is_none(), "v8 never writes phase_id");
        assert!(serde_json::from_value::<WorkJob>(v.clone()).is_ok());
        v["phase_id"] = serde_json::json!("phase-1");
        let err = serde_json::from_value::<WorkJob>(v).unwrap_err().to_string();
        assert!(err.contains("phase_id"), "{err}");

        // What a v7 sender actually posts: a submission whose job has one.
        let mut sub = serde_json::to_value(crate::WorkSubmission::new(make_valid_job(), true)).unwrap();
        sub["schema"] = "7".into();
        sub["job"]["phase_id"] = serde_json::json!("phase-1");
        assert_eq!(
            crate::WorkSubmission::parse(&serde_json::to_vec(&sub).unwrap()).unwrap_err(),
            crate::Refusal::SchemaMismatch { got: "7".into() }
        );
    }

    /// (#2916) A session id is a join key and part of file names on the
    /// receiver: the wire refuses a string outside the session grammar
    /// (a traversal, a space, a control character, a pre-4.0 free-form id),
    /// and `validate` bounds its length.
    #[test]
    fn the_session_id_is_read_strictly_and_bounded() {
        let mut v = serde_json::to_value(make_valid_job()).unwrap();
        for bad in ["", "../x", "a b", "a/b", "x\u{1b}[2J", "crew-dispatch-coder-1-0"] {
            v["session_id"] = serde_json::json!(bad);
            assert!(serde_json::from_value::<WorkJob>(v.clone()).is_err(), "{bad:?}");
        }
        let mut job = make_valid_job();
        let run = darkmux_types::session_id::RunId::mission("m").unwrap();
        job.session_id = SessionId::adhoc(run, "coder", "n".repeat(MAX_SESSION_ID_LEN));
        assert!(job.validate().unwrap_err().to_string().contains("session_id"));
    }

    /// The receiver runs a job under its relay of the sender's session,
    /// which escapes and wraps it, so the relay is longer than what the
    /// sender sent. The bound is on the sender's session: a relay of one
    /// that passed must still pass when the receiver executes it.
    #[test]
    fn the_bound_is_on_the_senders_session_not_the_receivers_relay() {
        let mut job = make_valid_job();
        let run = darkmux_types::session_id::RunId::standalone("dispatch-radio-host-1790546608-d324-0").unwrap();
        let sent = SessionId::adhoc(run, "radio-host", "1790546608989285-0");
        let room = MAX_SESSION_ID_LEN - sent.wire().len();
        let sent = SessionId::adhoc(sent.run_id().clone(), "radio-host", format!("1790546608989285-0{}", "n".repeat(room)));
        assert_eq!(sent.wire().len(), MAX_SESSION_ID_LEN);
        job.session_id = sent.clone();
        job.validate().expect("the sender's session is within the bound");
        job.session_id = SessionId::relay(sent, "macbook-pro");
        assert!(job.session_id.wire().len() > MAX_SESSION_ID_LEN);
        job.validate().expect("the receiver's relay of a bounded session is admitted");
    }

    /// (#2916) The v5 wire shape refuses the fields v4 carried.
    #[test]
    fn v4_only_fields_are_refused() {
        let v = serde_json::to_value(make_valid_job()).unwrap();
        for extra in ["attempt", "published_by_orchestrator"] {
            let mut obj = v.as_object().unwrap().clone();
            obj.insert(extra.into(), serde_json::json!(1));
            let err = serde_json::from_value::<WorkJob>(serde_json::Value::Object(obj)).unwrap_err();
            assert!(err.to_string().contains(extra), "{err}");
        }
    }

    // ---- validate_identifier tests ----

    #[test]
    fn validate_identifier_positive() {
        // lowercase + digits + underscore + hyphen
        assert!(validate_identifier("field", "abc-123_xyz").is_ok());
    }

    #[test]
    fn validate_identifier_empty() {
        let err = validate_identifier("field", "").unwrap_err();
        assert!(err.to_string().contains("must be non-empty"));
    }

    #[test]
    fn validate_identifier_over_length() {
        let long = "a".repeat(MAX_WORK_IDENTIFIER_LEN + 1);
        let err = validate_identifier("field", &long).unwrap_err();
        assert!(err.to_string().contains("exceeds"));
    }

    #[test]
    fn validate_identifier_dot() {
        let err = validate_identifier("field", "a.b").unwrap_err();
        assert!(err.to_string().contains("invalid char"));
    }

    #[test]
    fn validate_identifier_slash() {
        let err = validate_identifier("field", "a/b").unwrap_err();
        assert!(err.to_string().contains("invalid char"));
    }

    #[test]
    fn validate_identifier_double_dot() {
        let err = validate_identifier("field", "..").unwrap_err();
        assert!(err.to_string().contains("invalid char"));
    }

    #[test]
    fn validate_identifier_space() {
        let err = validate_identifier("field", "a b").unwrap_err();
        assert!(err.to_string().contains("invalid char"));
    }

    #[test]
    fn validate_identifier_uppercase() {
        let err = validate_identifier("field", "Abc").unwrap_err();
        assert!(err.to_string().contains("invalid char"));
    }

    #[test]
    fn validate_identifier_embedded_null() {
        let err = validate_identifier("field", "a\0b").unwrap_err();
        assert!(err.to_string().contains("invalid char"));
    }

    // ---- validate_image_ref tests ----

    #[test]
    fn validate_image_ref_valid() {
        // A typical registry/path:tag reference
        assert!(validate_image_ref("rust:slim").is_ok());
        // (#838 regression guard) registry/org-path + digest refs — the charset
        // originally omitted '/' and rejected every real ref.
        assert!(validate_image_ref("ghcr.io/org/repo:tag").is_ok());
        assert!(validate_image_ref("docker.io/library/rust@sha256:abc123").is_ok());
        // image byte-cap boundary (was untested):
        assert!(validate_image_ref(&"a".repeat(MAX_WORK_IMAGE_BYTES)).is_ok());
        assert!(validate_image_ref(&"a".repeat(MAX_WORK_IMAGE_BYTES + 1)).is_err());
    }

    #[test]
    fn validate_image_ref_empty() {
        let err = validate_image_ref("").unwrap_err();
        assert!(err.to_string().contains("non-empty"));
    }

    #[test]
    fn validate_image_ref_leading_dash() {
        let err = validate_image_ref("--privileged").unwrap_err();
        assert!(err.to_string().contains("must not start with '-'"));
    }

    #[test]
    fn validate_image_ref_bad_char() {
        let err = validate_image_ref("rust:slim\0").unwrap_err();
        assert!(err.to_string().contains("invalid char"));
    }

    // ---- WorkJob::validate boundary tests ----

    #[test]
    fn validate_job_zero_timeout() {
        let mut job = make_valid_job();
        job.timeout_seconds = 0;
        let err = job.validate().unwrap_err();
        assert!(err.to_string().contains("non-zero"));
    }

    #[test]
    fn validate_job_over_cap_message() {
        let mut job = make_valid_job();
        job.message = "x".repeat(MAX_WORK_MESSAGE_BYTES + 1);
        let err = job.validate().unwrap_err();
        assert!(err.to_string().contains("exceeds"));
    }

    #[test]
    fn validate_job_over_cap_workdir() {
        let mut job = make_valid_job();
        job.workdir = Some("x".repeat(MAX_WORK_WORKDIR_BYTES + 1));
        let err = job.validate().unwrap_err();
        assert!(err.to_string().contains("exceeds"));
    }

    #[test]
    fn validate_job_invalid_image_leading_dash() {
        let mut job = make_valid_job();
        job.image = Some("--privileged".to_string());
        let err = job.validate().unwrap_err();
        assert!(err.to_string().contains("must not start with '-'"));
    }

    #[test]
    fn validate_job_invalid_image_empty() {
        let mut job = make_valid_job();
        job.image = Some("".to_string());
        let err = job.validate().unwrap_err();
        assert!(err.to_string().contains("non-empty"));
    }

    #[test]
    fn validate_job_invalid_image_bad_char() {
        let mut job = make_valid_job();
        job.image = Some("rust:slim\0".to_string());
        let err = job.validate().unwrap_err();
        assert!(err.to_string().contains("invalid char"));
    }

    #[test]
    fn validate_job_valid_image_accepted() {
        let mut job = make_valid_job();
        job.image = Some("rust:slim".to_string());
        assert!(job.validate().is_ok());
    }
}
