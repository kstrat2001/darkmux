//! Fleet work queue — Redis work-stream publish / claim / ack + the WorkJob schema.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// The single global work stream. After #590 the fleet routes all work
/// onto one stream; the first available runner claims any job. The former
/// per-tier streams (`darkmux:work:<tier>`) are retired — machine-capacity
/// tier is no longer the work-routing key.
pub(crate) const WORK_STREAM: &str = "darkmux:work";

/// MAXLEN cap for the work streams (approximate; passes `MAXLEN ~ N`
/// to XADD). 1000 in-flight + recently-acked jobs is generous — at
/// 2-machine fleet scale, the steady-state depth is bounded by
/// in-flight count (typically 1-2). The cap exists to prevent a stuck
/// or crashed runner from growing the stream unboundedly.
#[allow(dead_code)] // PR-C.1 substrate; consumed by PR-C.2 (runner loop) + PR-C.3 (client push)
pub(crate) const WORK_STREAM_MAXLEN: usize = 1000;

/// One unit of dispatch work flowing through the queue. The producing
/// orchestrator constructs and publishes; the consuming runner reads,
/// dispatches, and acks. Serialized as the `record` field on a Redis
/// stream entry; the stream entry's auto-assigned ID becomes the
/// canonical `work_id` after claim.
///
/// Backward-compat shape: all fields are explicit (no `#[serde(default)]`
/// trickery) so any change to this struct is a deliberate schema bump.
/// Older runner code seeing a newer-shaped job will fail to deserialize
/// loudly rather than dispatching with missing context.
///
/// `#[serde(deny_unknown_fields)]` (PR-C.2) — a publisher cannot inject
/// extra fields that future-PR consumer code might inadvertently start
/// interpreting. Pairs with the schema-version contract; a real shape
/// change is a deliberate `WORK_JOB_SCHEMA_VERSION` bump + struct edit,
/// not a silent field smuggling.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkJob {
    /// Optional advisory machine hint — when set, the dispatching
    /// orchestrator suggests this specific machine should handle the job.
    /// Advisory only (#590): any runner may claim the job off the single
    /// `WORK_STREAM`; a non-target runner logs a soft warning and
    /// proceeds. There is NO NACK/requeue enforcement. When `None`, the
    /// first available runner claims it (pull semantics).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_machine: Option<String>,

    /// Role to dispatch against — resolved to a role manifest under
    /// `templates/builtin/roles/<role-id>.{json,md}` on the runner side.
    pub role_id: String,

    /// The operator's dispatch message — handed verbatim to the runtime.
    pub message: String,

    /// Stable session id used as the join key for `--wait` polling.
    /// Generated client-side; threaded to the dispatched `crew::dispatch::dispatch`
    /// via DispatchOpts so the emitted `dispatch.start` / `dispatch.complete`
    /// records carry the same value the publisher's poll loop is watching.
    pub session_id: String,

    /// Optional `--workdir` override (resolved to a string for transport;
    /// re-parsed to PathBuf on the runner side).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,

    /// Optional phase-id binding — same semantics as DispatchOpts.phase_id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_id: Option<String>,

    // (#1426 ship-3) The `deliver` (reserved-never-consumed) and `runtime`
    // (single-variant enum after #1405/#1409) fields retired here in the
    // coordinated `WORK_JOB_SCHEMA_VERSION` "3" to "4" bump. Because
    // `#[serde(deny_unknown_fields)]` is set on this struct, dropping the
    // fields is a real WIRE break, not a silent edit. A job carrying either
    // key from a pre-4 peer is rejected at parse time, which is exactly why
    // the removal rides a deliberate schema bump (see the doctrine on
    // `WORK_JOB_SCHEMA_VERSION` below) rather than shipping unversioned.
    /// (#703 Slice 4) Docker image the runner should dispatch into. When
    /// set, the runner injects darkmux's runtime binary into this image so
    /// the coder can compile/test the job in-sandbox. `None` → the runner's
    /// default slim image. Carries `--image` (and a workload's declared
    /// `image`) across the fleet queue so cross-machine dispatch honors it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,

    /// Per-turn timeout (seconds) — passes through to the runtime's
    /// turn timeout.
    pub timeout_seconds: u32,

    /// Unix-millis when the job was published. Used for queue-age
    /// diagnostics + the eureka rule that fires when total wall-clock
    /// < sum-of-phase-wall-clocks (parallel-dispatch proof point).
    pub published_at_unix_ms: u64,

    /// Machine that published the job (the dispatching orchestrator's
    /// `DARKMUX_MACHINE_ID`). Read-only provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_by_machine: Option<String>,

    /// Orchestrator that published the job (the dispatching session's
    /// `DARKMUX_ORCHESTRATOR`). Read-only provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_by_orchestrator: Option<String>,

    /// Attempt counter — 1 on first publish, 2+ after a lease-expiry
    /// re-publish (PR-E semantics). PR-C.1 always publishes with 1.
    pub attempt: u32,
}

/// Max byte size of a `WorkJob.message` accepted by the queue. A
/// publisher cannot XADD a multi-megabyte prompt that would force every
/// runner to allocate it on deserialize. 256 KiB matches the
/// reasoning-text cap in `dispatch_internal.rs` (#231 / S6) — same
/// rationale, same number. (#246 PR-C.2 boundary defense)
pub(crate) const MAX_WORK_MESSAGE_BYTES: usize = 256 * 1024;

/// Max byte size of `WorkJob.workdir` (the operator-supplied path
/// string). Filesystem path limits vary by platform; 4 KiB is generous
/// and prevents a publisher from filling memory with a multi-megabyte
/// path string. (#246 PR-C.2)
pub(crate) const MAX_WORK_WORKDIR_BYTES: usize = 4 * 1024;

/// Max length for identifier fields (`target_machine`, `role_id`). 64
/// chars is plenty for any realistic operator-named machine or role id
/// and forecloses identifier-as-payload attacks (e.g. an `role_id` of
/// 100MB). (#246 PR-C.2)
pub(crate) const MAX_WORK_IDENTIFIER_LEN: usize = 64;

/// Max allowed `timeout_seconds` on a queued `WorkJob`. 1 hour bounds
/// the worst-case "publisher pins this machine's single runner" surface.
/// Legitimate dispatches measured in this codebase top out around 15
/// minutes (long-agentic-shape workloads at large context); 1 hour is
/// 4× that headroom. A publisher specifying `u32::MAX` (136 years) is
/// rejected at the queue boundary. (#246 PR-C.3 / PR-C.2 review carry-over)
pub(crate) const MAX_WORK_TIMEOUT_SECONDS: u32 = 60 * 60;

/// Max byte size of `WorkJob.image` (the operator's Docker image ref).
/// 256 bytes is generous for any realistic image reference (e.g.
/// `ghcr.io/org/repo:tag`) and prevents a publisher from filling memory
/// with a multi-megabyte image string. (#838 PR-C.2)
pub(crate) const MAX_WORK_IMAGE_BYTES: usize = 256;

impl WorkJob {
    /// Validate a `WorkJob` at the queue boundary — called by both the
    /// publisher (in `publish_job`) and the consumer (after claim, before
    /// dispatch). Enforces charset + size invariants that protect the
    /// downstream dispatch path from a hostile or buggy publisher.
    ///
    /// Validated:
    /// - Identifier fields (optional `target_machine`, `role_id`) match
    ///   `[a-z0-9_-]{1,MAX_WORK_IDENTIFIER_LEN}`. Rejects path-traversal
    ///   (`../`), null bytes, command-injection chars, and over-long
    ///   values.
    /// - `message` ≤ `MAX_WORK_MESSAGE_BYTES`. Prevents memory
    ///   exhaustion at deserialize time.
    /// - Optional `workdir` ≤ `MAX_WORK_WORKDIR_BYTES`. The
    ///   symlink-escape check on the resolved path is done by the
    ///   runner (PR-C.2b / follow-up).
    /// - `image` (optional) — non-empty, ≤ `MAX_WORK_IMAGE_BYTES`, no
    ///   leading `-` (prevents docker-flag injection, #838), and every
    ///   char in the conservative image-ref charset.
    pub fn validate(&self) -> Result<()> {
        if let Some(m) = &self.target_machine {
            validate_work_identifier("target_machine", m)?;
        }
        validate_work_identifier("role_id", &self.role_id)?;
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
        Ok(())
    }
}

/// Charset+length check for an identifier-shaped field — the canonical
/// validator used both at the queue boundary (`WorkJob::validate`) and
/// at the CLI boundary (`darkmux mission dispatch <mission_id>` etc.,
/// Wave-E.5 #255).
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

/// Result of a successful `claim_job` — the runner now owns the job.
/// `work_id` is the Redis stream entry ID assigned at publish time
/// (canonical form: `<ms>-<seq>`); `ack_job` uses it to acknowledge
/// completion.
#[derive(Debug, Clone)]
pub(crate) struct ClaimedJob {
    pub work_id: String,
    pub job: WorkJob,
}

/// (#903) Outcome of a single `claim_job` / `parse_xreadgroup_response`.
/// Distinguishes a poison entry (claimed into this consumer's PEL but
/// unparseable — the caller should `XACK` it so it doesn't sit pending
/// forever) from an empty read and a genuine connection/protocol error
/// (which stays `Err` and warrants a backoff, not an ACK).
#[derive(Debug)]
pub(crate) enum ClaimOutcome {
    /// BLOCK timeout / no entries.
    Empty,
    /// A valid claimed job ready to dispatch. Boxed because `ClaimedJob`
    /// carries a full `WorkJob` — far larger than the other variants, so an
    /// unboxed `Job` would bloat every `ClaimOutcome` to that size.
    Job(Box<ClaimedJob>),
    /// An entry was claimed (it's in this consumer's PEL) but can't be parsed
    /// into a `WorkJob` — missing `record` field or invalid JSON. It can
    /// NEVER be processed, so the runner `XACK`s it to drop the poison from
    /// the PEL. `work_id` is known because it's extracted before the
    /// record/deser step.
    Malformed { work_id: String, reason: String },
}

/// Publish a job onto the single global Redis Stream (`WORK_STREAM`).
/// Returns the auto-assigned entry ID (the canonical `work_id`).
///
/// XADD fields:
/// - `schema`: `WORK_JOB_SCHEMA_VERSION` ("4"), a wire-version gate. Since
///   #1426 ship-3 the consumer READS this tag on claim (see
///   [`parse_xreadgroup_response`]) and rejects a mismatched version FIRST,
///   before touching `record`, so the operator gets a version-naming reason
///   rather than a generic `deny_unknown_fields` parse error. Serde SHAPE
///   (`#[serde(deny_unknown_fields)]` + required-field deserialization) is
///   the second line of defense for a same-version-tag-but-wrong-shape job.
/// - `record`: the JSON-serialized WorkJob
///
/// Capped at `WORK_STREAM_MAXLEN` via `MAXLEN ~ N` so a stuck runner
/// can't grow the stream unboundedly.
#[allow(dead_code)] // PR-C.1 substrate; consumed by PR-C.2 (runner loop) + PR-C.3 (client push)
pub fn publish_job(client: &redis::Client, job: &WorkJob) -> Result<String> {
    // Fail-fast at the queue boundary — better to reject a malformed
    // job at publish than to ship it across the network and trip the
    // consumer-side validator after one or more runners waste their
    // claim budget on it. (#246 PR-C.2)
    job.validate()
        .context("validating WorkJob before publish")?;
    // Ensure the consumer group exists BEFORE the XADD. The group is created
    // at `$` (new-messages-only); if this job were XADD'd before any runner
    // had ever created the group, the group's cursor would start AFTER it and
    // the job would never be delivered — silent lost work (publish reports
    // success, but `--wait` times out and no runner ever sees it). Creating
    // the group here first guarantees its cursor precedes this message.
    // Idempotent: an already-existing group returns BUSYGROUP, treated as ok.
    init_consumer_group(client, crate::runner::RUNNER_CONSUMER_GROUP)
        .context("ensuring the consumer group exists before publish")?;
    let mut conn = client
        .get_connection()
        .context("opening Redis connection to publish work job")?;
    let payload = serde_json::to_string(job).context("serializing WorkJob")?;
    let mut cmd = redis::cmd("XADD");
    cmd.arg(WORK_STREAM)
        .arg("MAXLEN")
        .arg("~")
        .arg(WORK_STREAM_MAXLEN)
        .arg("*")
        .arg("schema")
        .arg(WORK_JOB_SCHEMA_VERSION)
        .arg("record")
        .arg(&payload);
    let id: String = cmd
        .query(&mut conn)
        .with_context(|| format!("XADD to {WORK_STREAM}"))?;
    Ok(id)
}

/// Wire-schema version tag carried alongside each job (the XADD `schema`
/// field). Bumped when `WorkJob` shape changes in a way old runners can't
/// safely parse.
///
/// History:
/// - "1" to "2" in #590 (single-stream collapse: `target_tier` removed).
/// - "2" to "3" in #703 Slice 4 (added the optional `image` field).
/// - "3" to "4" in #1426 ship-3 (coordinated wire break: the dead `deliver`
///   and single-variant `runtime` fields retired from `WorkJob`). Because
///   the struct carries `#[serde(deny_unknown_fields)]`, removing either
///   field is a genuine wire break: a pre-4 peer's job carries those keys
///   and a v4 runner's `serde_json::from_str` rejects them. The removal
///   rides this deliberate bump rather than shipping unversioned (see the
///   #1426 comment: field removal is a schema-version bump, never silent).
///
/// **Version-first mismatch semantics (#1426 ship-3).** [`claim_job`] /
/// [`parse_xreadgroup_response`] read this `schema` tag on the claimed
/// entry BEFORE deserializing the `record`. A tag that isn't
/// `WORK_JOB_SCHEMA_VERSION` routes the entry to [`ClaimOutcome::Malformed`]
/// with a reason that NAMES the version cause ("job schema v<old> from an
/// older or newer peer; upgrade + restart all fleet daemons"), so the
/// operator sees the real cause instead of a generic unknown-field parse
/// error. The runner XACKs the poison out of its PEL and keeps looping.
///
/// **The v3 -> v4 wire break is ASYMMETRIC.** The two retired fields were
/// BOTH `#[serde(default)]` on the v3 struct (`deliver` optional-skipped,
/// `runtime` a defaulted enum), and a v4 job is a strict SUBSET of v3's
/// keys. So the two mismatch directions do NOT behave the same:
/// - **New runner (v4), old job (v3 tag) in the stream:** the version-first
///   gate reads the `schema` tag, sees v3, and routes to `Malformed` with
///   the version-naming reason; the runner XACKs it. It is NOT retried (a
///   v3 record also carries the retired keys, which a v4 runner's
///   `deny_unknown_fields` rejects anyway; the gate just reports the real
///   cause first). This direction breaks loudly, by design.
/// - **Old runner (v3), new job (v4 tag) in the stream:** the old runner
///   predates this gate, so it ignores the `schema` tag and parses the
///   record directly. A v4 job OMITS `deliver`/`runtime`, and their absence
///   is fine because v3 declared both `#[serde(default)]`; nothing unknown
///   is present for `deny_unknown_fields` to reject. So the old runner
///   parses and RUNS the v4 job transparently, harmlessly (both removed
///   fields were dead: `deliver` was never consumed, `runtime` had one
///   variant). This direction does NOT break for THIS bump.
///
/// **Operational contract: restart all fleet daemons together on upgrade.**
/// Both machines are the operator's; a version bump is a fleet-wide event.
/// The restart-together rule is the safe general discipline (a FUTURE bump
/// may not be so lucky about default-valued removals), not a claim that a
/// pre-4 runner chokes on v4 jobs. For this specific bump the only failure
/// direction is new-runner-claims-old-job, and it is caught loudly by the
/// version gate rather than left to a confusing unknown-field parse error.
pub(crate) const WORK_JOB_SCHEMA_VERSION: &str = "4";

/// Ensure the consumer group exists on the single global stream.
/// Idempotent — returns `Ok(())` whether the group was just created OR
/// already existed. The `MKSTREAM` flag creates the stream itself if
/// missing (XGROUP CREATE on a non-existent stream would otherwise
/// error).
///
/// Call once per daemon-startup. Safe to call repeatedly.
pub(crate) fn init_consumer_group(client: &redis::Client, group: &str) -> Result<()> {
    let mut conn = client
        .get_connection()
        .context("opening Redis connection to init consumer group")?;
    let result: redis::RedisResult<String> = redis::cmd("XGROUP")
        .arg("CREATE")
        .arg(WORK_STREAM)
        .arg(group)
        .arg("$")
        .arg("MKSTREAM")
        .query(&mut conn);
    match result {
        Ok(_) => Ok(()),
        Err(e) => {
            // BUSYGROUP → group already exists; treat as success. Use the
            // typed error code (redis-rs 0.27+ `RedisError::code()`) rather
            // than substring-matching the Display string — survives future
            // crate-version reformatting of error messages.
            if matches!(e.code(), Some("BUSYGROUP")) {
                Ok(())
            } else {
                Err(anyhow!("XGROUP CREATE on {WORK_STREAM}: {e}"))
            }
        }
    }
}

/// Claim the next job from the single global stream's consumer group via
/// XREADGROUP. Blocks for up to `block_ms` waiting for a new entry;
/// returns `Ok(None)` on timeout (no work available).
///
/// Returns the entry ID (used for `ack_job`) plus the deserialized
/// `WorkJob`. Malformed entries (deserialize failure) are surfaced as
/// errors so the caller can decide whether to ack-and-skip or bail.
///
/// `consumer` is the per-runner identity (typically `DARKMUX_MACHINE_ID`)
/// — Redis tracks per-consumer pending-entries lists for lease semantics
/// (PR-E will consume these).
pub(crate) fn claim_job(
    client: &redis::Client,
    group: &str,
    consumer: &str,
    block_ms: u64,
) -> Result<ClaimOutcome> {
    let mut conn = client
        .get_connection()
        .context("opening Redis connection to claim work job")?;

    // XREADGROUP returns nested arrays: [[stream, [[id, [k, v, k, v]]]]].
    // We parse via `redis::Value` (the dynamic type) rather than a typed
    // tuple to keep the parser robust across redis-rs versions.
    let raw: Option<redis::Value> = redis::cmd("XREADGROUP")
        .arg("GROUP")
        .arg(group)
        .arg(consumer)
        .arg("COUNT")
        .arg(1usize)
        .arg("BLOCK")
        .arg(block_ms)
        .arg("STREAMS")
        .arg(WORK_STREAM)
        .arg(">")
        .query(&mut conn)
        .with_context(|| format!("XREADGROUP from {WORK_STREAM}"))?;

    let Some(value) = raw else { return Ok(ClaimOutcome::Empty) };
    parse_xreadgroup_response(&value)
}

/// Parse XREADGROUP's nested-array response into a [`ClaimOutcome`].
/// `Empty` on an empty response (timeout / no work); `Malformed` when an
/// entry was claimed but its `record` is missing or unparseable (the caller
/// XACKs it, #903); `Err` only on an unexpected protocol shape. Extracted as
/// a pure function so it's unit-testable without Redis.
pub(crate) fn parse_xreadgroup_response(value: &redis::Value) -> Result<ClaimOutcome> {
    use redis::Value as V;

    // Bulk(nil) or Nil → timeout, no work.
    if matches!(value, V::Nil) {
        return Ok(ClaimOutcome::Empty);
    }

    // Expected shape: Bulk([Bulk([stream_name, Bulk([Bulk([id, Bulk([k,v,k,v...])])])])])
    let outer = match value {
        V::Array(b) => b,
        _ => return Err(anyhow!("XREADGROUP: unexpected outer shape: {value:?}")),
    };
    if outer.is_empty() {
        return Ok(ClaimOutcome::Empty);
    }
    let stream_block = match &outer[0] {
        V::Array(b) => b,
        _ => return Err(anyhow!("XREADGROUP: expected stream block")),
    };
    if stream_block.len() < 2 {
        return Ok(ClaimOutcome::Empty);
    }
    let entries = match &stream_block[1] {
        V::Array(b) => b,
        _ => return Err(anyhow!("XREADGROUP: expected entries list")),
    };
    if entries.is_empty() {
        return Ok(ClaimOutcome::Empty);
    }
    let entry = match &entries[0] {
        V::Array(b) => b,
        _ => return Err(anyhow!("XREADGROUP: expected entry tuple")),
    };
    if entry.len() < 2 {
        return Err(anyhow!("XREADGROUP: entry missing id or fields"));
    }
    let work_id = match &entry[0] {
        V::BulkString(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        V::SimpleString(s) => s.clone(),
        _ => return Err(anyhow!("XREADGROUP: expected entry id")),
    };
    let fields = match &entry[1] {
        V::Array(b) => b,
        // (#903) work_id is known but the fields aren't a list — this entry
        // is claimed-but-unparseable poison, same class as a missing record.
        // Route to Malformed so the runner XACKs it, rather than Err (which
        // would leave it stuck in the PEL forever).
        _ => {
            return Ok(ClaimOutcome::Malformed {
                work_id,
                reason: "fields list is not an array".to_string(),
            })
        }
    };
    // (#1426 ship-3) Version-first gate. Read the `schema` tag BEFORE
    // deserializing `record` so a version mismatch is reported with its
    // REAL cause. Without this ordering a pre-v4 peer's job (carrying the
    // retired `deliver` / `runtime` keys) would fail `deny_unknown_fields`
    // parsing with a generic "unknown field `deliver`" error that hides the
    // actual problem: a fleet running mixed schema versions. A tag that
    // isn't ours routes to Malformed (XACK-able poison), never a runner
    // crash, with a reason the operator can act on. An ABSENT `schema` tag
    // (a hypothetical pre-versioning job) is NOT version-rejected here; it
    // falls through to the record parse, which handles it honestly (parses
    // if shape-compatible, else Malformed via the invalid-JSON path).
    if let Some(schema) = extract_field(fields, "schema") {
        if schema != WORK_JOB_SCHEMA_VERSION {
            return Ok(ClaimOutcome::Malformed {
                work_id,
                reason: format!(
                    "job schema v{schema} from an older or newer fleet peer \
                     (this runner speaks v{WORK_JOB_SCHEMA_VERSION}); \
                     upgrade + restart all fleet daemons together"
                ),
            });
        }
    }
    let Some(record_json) = extract_field(fields, "record") else {
        // (#903) Claimed but no `record` field — poison. Hand the work_id
        // back so the runner can XACK it out of the pending-entries list.
        return Ok(ClaimOutcome::Malformed {
            work_id,
            reason: "missing `record` field".to_string(),
        });
    };
    let job: WorkJob = match serde_json::from_str(&record_json) {
        Ok(j) => j,
        Err(e) => {
            // (#903) Claimed but the record isn't a valid WorkJob — poison.
            return Ok(ClaimOutcome::Malformed {
                work_id,
                reason: format!("invalid WorkJob JSON: {e}"),
            });
        }
    };
    Ok(ClaimOutcome::Job(Box::new(ClaimedJob { work_id, job })))
}

/// Pull a field's value out of a Redis field-list (`[k, v, k, v, ...]`).
/// Returns `None` if the key isn't present.
pub(crate) fn extract_field(fields: &[redis::Value], key: &str) -> Option<String> {
    use redis::Value as V;
    let mut i = 0;
    while i + 1 < fields.len() {
        let k = match &fields[i] {
            V::BulkString(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            V::SimpleString(s) => s.clone(),
            _ => {
                i += 2;
                continue;
            }
        };
        if k == key {
            return match &fields[i + 1] {
                V::BulkString(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
                V::SimpleString(s) => Some(s.clone()),
                _ => None,
            };
        }
        i += 2;
    }
    None
}

/// Acknowledge a claimed job, removing it from the consumer group's
/// pending-entries list (PEL). After ack, the job is fully delivered
/// from the queue's perspective.
///
/// Runner MUST call this after the dispatch completes, regardless of
/// dispatch success — the `dispatch.complete` flow record carries the
/// success/error signal; the ack just releases the queue lease.
pub(crate) fn ack_job(client: &redis::Client, group: &str, work_id: &str) -> Result<()> {
    let mut conn = client
        .get_connection()
        .context("opening Redis connection to ack work job")?;
    let _: i64 = redis::cmd("XACK")
        .arg(WORK_STREAM)
        .arg(group)
        .arg(work_id)
        .query(&mut conn)
        .with_context(|| format!("XACK on {WORK_STREAM}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid WorkJob with all optional fields None.
    fn make_valid_job() -> WorkJob {
        WorkJob {
            target_machine: None,
            role_id: "test-role".to_string(),
            message: "hello".to_string(),
            session_id: "sess-1".to_string(),
            workdir: None,
            phase_id: None,
            image: None,
            timeout_seconds: 60,
            published_at_unix_ms: 1_700_000_000_000,
            published_by_machine: None,
            published_by_orchestrator: None,
            attempt: 1,
        }
    }

    /// Regression: a job published BEFORE any runner ever created the consumer
    /// group must still be delivered. `publish_job` creates the group (at `$`)
    /// before the XADD, so the group's cursor precedes the message. Without
    /// that, a later runner's `XGROUP CREATE ... $` sets the cursor AFTER the
    /// already-published message and it is never delivered (silent lost work).
    ///
    /// Live-Redis test: gated on `DARKMUX_TEST_REDIS_URL` (skips when unset, so
    /// CI without a Redis passes). Run locally with e.g.
    /// `DARKMUX_TEST_REDIS_URL=redis://127.0.0.1:6379 cargo test -p darkmux-fleet`.
    #[test]
    #[serial_test::serial]
    fn publish_ensures_group_so_a_job_published_first_is_delivered() {
        let Ok(url) = std::env::var("DARKMUX_TEST_REDIS_URL") else {
            eprintln!("skipping publish_ensures_group_*: DARKMUX_TEST_REDIS_URL unset");
            return;
        };
        let group = crate::runner::RUNNER_CONSUMER_GROUP;
        let client = redis::Client::open(url).expect("open test redis");
        let mut conn = client.get_connection().expect("test redis connection");

        // Fresh-Redis scenario: no stream, no group yet.
        let _: () = redis::cmd("DEL").arg(WORK_STREAM).query(&mut conn).unwrap();

        // Publish BEFORE any runner exists. With the fix this creates the group
        // first, then XADDs, so the message lands after the group cursor.
        let id = publish_job(&client, &make_valid_job()).expect("publish");

        // A runner starts now: init_consumer_group is idempotent (BUSYGROUP),
        // and reading new messages with `>` must include the earlier publish.
        init_consumer_group(&client, group).expect("runner init group");
        let reply: redis::Value = redis::cmd("XREADGROUP")
            .arg("GROUP")
            .arg(group)
            .arg("test-consumer")
            .arg("COUNT")
            .arg(10)
            .arg("STREAMS")
            .arg(WORK_STREAM)
            .arg(">")
            .query(&mut conn)
            .expect("XREADGROUP");

        // Clean up before asserting so a failure doesn't leak stream state.
        let _: () = redis::cmd("DEL").arg(WORK_STREAM).query(&mut conn).unwrap();

        assert!(
            !matches!(reply, redis::Value::Nil),
            "job {id} published before the group existed was not delivered (lost work)"
        );
    }

    /// (#1426 ship-3) Real-redis publish then claim round-trip of the v4 shape,
    /// end-to-end through `publish_job` (which XADDs the schema="4" tag) and
    /// `claim_job` (which reads it via the version-first gate). A same-version
    /// happy path: the job comes back as `ClaimOutcome::Job`, byte-equal to
    /// what was published. Gated on `DARKMUX_TEST_REDIS_URL`.
    #[test]
    #[serial_test::serial]
    fn e2e_publish_then_claim_round_trips_v4_shape() {
        let Ok(url) = std::env::var("DARKMUX_TEST_REDIS_URL") else {
            eprintln!("skipping e2e_publish_then_claim_*: DARKMUX_TEST_REDIS_URL unset");
            return;
        };
        let group = crate::runner::RUNNER_CONSUMER_GROUP;
        let client = redis::Client::open(url).expect("open test redis");
        let mut conn = client.get_connection().expect("test redis connection");
        let _: () = redis::cmd("DEL").arg(WORK_STREAM).query(&mut conn).unwrap();

        let job = make_valid_job();
        let work_id = publish_job(&client, &job).expect("publish");

        let outcome = claim_job(&client, group, "e2e-consumer", 2000).expect("claim");
        // Clean up before asserting so a failure doesn't leak stream state.
        let _: () = redis::cmd("DEL").arg(WORK_STREAM).query(&mut conn).unwrap();

        let ClaimOutcome::Job(claimed) = outcome else {
            panic!("expected ClaimOutcome::Job for a same-version v4 job, got {outcome:?}");
        };
        assert_eq!(claimed.work_id, work_id);
        assert_eq!(claimed.job, job, "the claimed job must round-trip byte-equal");
    }

    /// (#1426 ship-3) Real-redis version-mismatch e2e: an older peer XADDs a
    /// pre-4 job (schema="3" tag, record still carrying the retired `deliver`
    /// / `runtime` keys). A v4 runner's `claim_job` must route it to
    /// `ClaimOutcome::Malformed` with the version-naming reason (never a
    /// runner crash, never a generic unknown-field parse error). This is the
    /// operational contract exercised over a real socket, not just the pure
    /// parser. Gated on `DARKMUX_TEST_REDIS_URL`.
    #[test]
    #[serial_test::serial]
    fn e2e_claim_of_old_schema_job_is_version_mismatch_malformed() {
        let Ok(url) = std::env::var("DARKMUX_TEST_REDIS_URL") else {
            eprintln!("skipping e2e_claim_of_old_schema_*: DARKMUX_TEST_REDIS_URL unset");
            return;
        };
        let group = crate::runner::RUNNER_CONSUMER_GROUP;
        let client = redis::Client::open(url).expect("open test redis");
        let mut conn = client.get_connection().expect("test redis connection");
        let _: () = redis::cmd("DEL").arg(WORK_STREAM).query(&mut conn).unwrap();

        // The runner must have created the group before it can claim.
        init_consumer_group(&client, group).expect("init group");

        // Simulate an older peer: XADD with a pre-4 schema tag and a record
        // that still carries the retired fields.
        let old_record = r#"{
            "role_id": "coder",
            "message": "hi",
            "session_id": "s-old",
            "deliver": "discord:123",
            "runtime": "openclaw",
            "timeout_seconds": 300,
            "published_at_unix_ms": 0,
            "attempt": 1
        }"#;
        let _: String = redis::cmd("XADD")
            .arg(WORK_STREAM)
            .arg("*")
            .arg("schema")
            .arg("3")
            .arg("record")
            .arg(old_record)
            .query(&mut conn)
            .expect("XADD old-shape job");

        let outcome = claim_job(&client, group, "e2e-consumer", 2000).expect("claim");
        let _: () = redis::cmd("DEL").arg(WORK_STREAM).query(&mut conn).unwrap();

        let ClaimOutcome::Malformed { reason, .. } = outcome else {
            panic!("expected ClaimOutcome::Malformed for a version-mismatched job, got {outcome:?}");
        };
        assert!(reason.contains("schema v3"), "reason must name the version: {reason}");
        assert!(
            reason.contains("restart all fleet daemons"),
            "reason must name the operational fix: {reason}"
        );
    }

    /// (#1426 ship-3) The claim loop SURVIVES a poison/version-mismatched
    /// entry: after the runner XACKs the malformed job out of its PEL (the
    /// #903 pattern), a subsequent well-formed v4 job still claims cleanly.
    /// This proves version skew degrades one entry, never the consumer.
    /// Gated on `DARKMUX_TEST_REDIS_URL`.
    #[test]
    #[serial_test::serial]
    fn e2e_runner_survives_malformed_then_claims_next_good_job() {
        let Ok(url) = std::env::var("DARKMUX_TEST_REDIS_URL") else {
            eprintln!("skipping e2e_runner_survives_malformed_*: DARKMUX_TEST_REDIS_URL unset");
            return;
        };
        let group = crate::runner::RUNNER_CONSUMER_GROUP;
        let client = redis::Client::open(url).expect("open test redis");
        let mut conn = client.get_connection().expect("test redis connection");
        let _: () = redis::cmd("DEL").arg(WORK_STREAM).query(&mut conn).unwrap();
        init_consumer_group(&client, group).expect("init group");

        // Poison first: an old-schema entry.
        let _: String = redis::cmd("XADD")
            .arg(WORK_STREAM)
            .arg("*")
            .arg("schema")
            .arg("3")
            .arg("record")
            .arg(r#"{"role_id":"coder","message":"x","session_id":"s","runtime":"openclaw","timeout_seconds":300,"published_at_unix_ms":0,"attempt":1}"#)
            .query(&mut conn)
            .expect("XADD poison");

        // The runner claims it, sees Malformed, and XACKs it (loop's #903 path).
        let poison = claim_job(&client, group, "e2e-consumer", 2000).expect("claim poison");
        let ClaimOutcome::Malformed { work_id, .. } = poison else {
            let _: () = redis::cmd("DEL").arg(WORK_STREAM).query(&mut conn).unwrap();
            panic!("expected Malformed for the poison entry, got {poison:?}");
        };
        ack_job(&client, group, &work_id).expect("XACK the poison");

        // A well-formed v4 job published next must still be claimable; the
        // consumer group survived the poison.
        let good = make_valid_job();
        publish_job(&client, &good).expect("publish good job");
        let outcome = claim_job(&client, group, "e2e-consumer", 2000).expect("claim good");
        let _: () = redis::cmd("DEL").arg(WORK_STREAM).query(&mut conn).unwrap();

        let ClaimOutcome::Job(claimed) = outcome else {
            panic!("consumer must keep working after poison, got {outcome:?}");
        };
        assert_eq!(claimed.job, good);
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

    // (Timeout-cap boundary — over-cap reject + at-cap accept — is already
    // covered in lib.rs by validate_rejects_oversize_timeout +
    // validate_accepts_max_timeout, so not duplicated here.)

    // ---- parse_xreadgroup_response: protocol-shape Err branches (#842) ----
    //
    // COMPLEMENTS the lib.rs tests, which cover the Empty (nil/empty-bulk),
    // Job (happy round-trip), and Malformed (#903 poison: non-array-fields /
    // missing-record / invalid-json) cases. Those are NOT re-tested here.
    // What lib.rs leaves uncovered is the protocol-shape `Err` arm — when the
    // nested response shape itself is wrong (vs an entry being poison). A
    // regression there mis-buckets a malformed protocol response and sails
    // through green CI. These build the nested `redis::Value` trees by hand and
    // walk each remaining return branch.

    use redis::Value as RV;

    #[test]
    fn parse_xreadgroup_response_non_array_outer_errs() {
        let err = parse_xreadgroup_response(&RV::SimpleString("nope".into())).unwrap_err();
        assert!(err.to_string().contains("unexpected outer shape"), "{err}");
    }

    #[test]
    fn parse_xreadgroup_response_stream_block_not_array_errs() {
        let v = RV::Array(vec![RV::SimpleString("x".into())]);
        let err = parse_xreadgroup_response(&v).unwrap_err();
        assert!(err.to_string().contains("expected stream block"), "{err}");
    }

    #[test]
    fn parse_xreadgroup_response_stream_block_too_short_is_empty() {
        // stream_block has only the name, no entries list (len < 2).
        let v = RV::Array(vec![RV::Array(vec![RV::BulkString(b"darkmux:work".to_vec())])]);
        assert!(matches!(
            parse_xreadgroup_response(&v).unwrap(),
            ClaimOutcome::Empty
        ));
    }

    #[test]
    fn parse_xreadgroup_response_entries_not_array_errs() {
        let v = RV::Array(vec![RV::Array(vec![
            RV::BulkString(b"darkmux:work".to_vec()),
            RV::SimpleString("notalist".into()),
        ])]);
        let err = parse_xreadgroup_response(&v).unwrap_err();
        assert!(err.to_string().contains("expected entries list"), "{err}");
    }

    #[test]
    fn parse_xreadgroup_response_empty_entries_is_empty() {
        let v = RV::Array(vec![RV::Array(vec![
            RV::BulkString(b"darkmux:work".to_vec()),
            RV::Array(vec![]),
        ])]);
        assert!(matches!(
            parse_xreadgroup_response(&v).unwrap(),
            ClaimOutcome::Empty
        ));
    }

    #[test]
    fn parse_xreadgroup_response_entry_not_array_errs() {
        let v = RV::Array(vec![RV::Array(vec![
            RV::BulkString(b"darkmux:work".to_vec()),
            RV::Array(vec![RV::SimpleString("nottuple".into())]),
        ])]);
        let err = parse_xreadgroup_response(&v).unwrap_err();
        assert!(err.to_string().contains("expected entry tuple"), "{err}");
    }

    #[test]
    fn parse_xreadgroup_response_entry_too_short_errs() {
        // entry has an id but no fields (len < 2).
        let v = RV::Array(vec![RV::Array(vec![
            RV::BulkString(b"darkmux:work".to_vec()),
            RV::Array(vec![RV::Array(vec![RV::BulkString(b"1-0".to_vec())])]),
        ])]);
        let err = parse_xreadgroup_response(&v).unwrap_err();
        assert!(err.to_string().contains("entry missing id or fields"), "{err}");
    }

    #[test]
    fn parse_xreadgroup_response_bad_id_type_errs() {
        // entry[0] is neither BulkString nor SimpleString → unrecoverable shape.
        let fields = RV::Array(vec![
            RV::BulkString(b"record".to_vec()),
            RV::BulkString(b"{}".to_vec()),
        ]);
        let v = RV::Array(vec![RV::Array(vec![
            RV::BulkString(b"darkmux:work".to_vec()),
            RV::Array(vec![RV::Array(vec![RV::Int(42), fields])]),
        ])]);
        let err = parse_xreadgroup_response(&v).unwrap_err();
        assert!(err.to_string().contains("expected entry id"), "{err}");
    }

    // ---- extract_field: edge cases (#842) ----
    // lib.rs covers key-found (BulkString + SimpleString) and key-absent.
    // These add the structural edge cases that lib.rs leaves uncovered.

    #[test]
    fn extract_field_odd_length_returns_none() {
        // Incomplete trailing pair (k with no v) — the i+1<len guard.
        let fields = vec![RV::BulkString(b"k".to_vec())];
        assert_eq!(extract_field(&fields, "k"), None);
    }

    #[test]
    fn extract_field_empty_returns_none() {
        assert_eq!(extract_field(&[], "k"), None);
    }

    #[test]
    fn extract_field_skips_non_string_keys() {
        // A non-string key + its value is skipped (i += 2), the later string key found.
        let fields = vec![
            RV::Int(1),
            RV::BulkString(b"ignored".to_vec()),
            RV::BulkString(b"k".to_vec()),
            RV::BulkString(b"v".to_vec()),
        ];
        assert_eq!(extract_field(&fields, "k").as_deref(), Some("v"));
    }

    #[test]
    fn extract_field_non_string_value_returns_none() {
        let fields = vec![RV::BulkString(b"k".to_vec()), RV::Int(42)];
        assert_eq!(extract_field(&fields, "k"), None);
    }
}
