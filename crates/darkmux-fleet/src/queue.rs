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

    /// Role to dispatch against — `darkmux/<role-id>` resolves to the
    /// openclaw agent on the runner side.
    pub role_id: String,

    /// The operator's dispatch message — handed verbatim to the runtime.
    pub message: String,

    /// Stable session id used as the join key for `--wait` polling.
    /// Generated client-side; threaded to the dispatched `crew::dispatch::dispatch`
    /// via DispatchOpts so the emitted `dispatch.start` / `dispatch.complete`
    /// records carry the same value the publisher's poll loop is watching.
    pub session_id: String,

    /// Optional delivery target (`<channel>:<target>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliver: Option<String>,

    /// Optional `--workdir` override (resolved to a string for transport;
    /// re-parsed to PathBuf on the runner side).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,

    /// Optional sprint-id binding — same semantics as DispatchOpts.sprint_id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sprint_id: Option<String>,

    /// Which runtime the runner should use — `Openclaw` (default) or
    /// `Internal`. Wave-E.14 lifted this from `String` to the
    /// `Runtime` enum: a mistyped runtime is rejected at JSON-parse
    /// time by serde rather than at `validate()`, eliminating a class
    /// of "publisher snuck through validate, runner crashed" bugs.
    #[serde(default)]
    pub runtime: darkmux_crew::dispatch::Runtime,

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
    /// < sum-of-sprint-wall-clocks (parallel-dispatch proof point).
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
    ///
    /// `runtime` is not checked here — the field is the `Runtime` enum,
    /// so an unknown variant is rejected at JSON deserialization
    /// (Wave-E.14 #255). A new runtime requires a deliberate variant
    /// add to `Runtime`.
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

/// Result of a successful `claim_job` — the runner now owns the job.
/// `work_id` is the Redis stream entry ID assigned at publish time
/// (canonical form: `<ms>-<seq>`); `ack_job` uses it to acknowledge
/// completion.
#[derive(Debug, Clone)]
pub(crate) struct ClaimedJob {
    pub work_id: String,
    pub job: WorkJob,
}

/// Publish a job onto the single global Redis Stream (`WORK_STREAM`).
/// Returns the auto-assigned entry ID (the canonical `work_id`).
///
/// XADD fields:
/// - `schema`: `WORK_JOB_SCHEMA_VERSION` ("3") — wire-version tag so
///   future schema bumps can be detected by older runners
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

/// Wire-schema version tag carried alongside each job. Bumped when
/// `WorkJob` shape changes in a way old runners can't safely parse.
/// Bumped "1" → "2" in #590 (single-stream collapse: `target_tier`
/// removed from `WorkJob`). Bumped "2" → "3" in #703 Slice 4 (added the
/// optional `image` field so cross-machine dispatch carries `--image`).
/// `deny_unknown_fields` means a job carrying `image` from a "3"-era
/// publisher is rejected by a "2"-era runner — a clean pre-1.0 wire break;
/// restart all fleet daemons together after upgrading.
#[allow(dead_code)] // PR-C.1 substrate; consumed by PR-C.2 (runner loop) + PR-C.3 (client push)
pub(crate) const WORK_JOB_SCHEMA_VERSION: &str = "3";

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
) -> Result<Option<ClaimedJob>> {
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

    let Some(value) = raw else { return Ok(None) };
    let claimed = parse_xreadgroup_response(&value)?;
    Ok(claimed)
}

/// Parse XREADGROUP's nested-array response into an optional ClaimedJob.
/// Returns `Ok(None)` when the response is empty (timeout / no work);
/// extracted as a pure function so it's unit-testable without Redis.
pub(crate) fn parse_xreadgroup_response(value: &redis::Value) -> Result<Option<ClaimedJob>> {
    use redis::Value as V;

    // Bulk(nil) or Nil → timeout, no work.
    if matches!(value, V::Nil) {
        return Ok(None);
    }

    // Expected shape: Bulk([Bulk([stream_name, Bulk([Bulk([id, Bulk([k,v,k,v...])])])])])
    let outer = match value {
        V::Array(b) => b,
        _ => return Err(anyhow!("XREADGROUP: unexpected outer shape: {value:?}")),
    };
    if outer.is_empty() {
        return Ok(None);
    }
    let stream_block = match &outer[0] {
        V::Array(b) => b,
        _ => return Err(anyhow!("XREADGROUP: expected stream block")),
    };
    if stream_block.len() < 2 {
        return Ok(None);
    }
    let entries = match &stream_block[1] {
        V::Array(b) => b,
        _ => return Err(anyhow!("XREADGROUP: expected entries list")),
    };
    if entries.is_empty() {
        return Ok(None);
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
        _ => return Err(anyhow!("XREADGROUP: expected fields list")),
    };
    let record_json = extract_field(fields, "record")
        .ok_or_else(|| anyhow!("XREADGROUP entry missing `record` field"))?;
    let job: WorkJob = serde_json::from_str(&record_json)
        .with_context(|| format!("deserializing WorkJob from entry {work_id}"))?;
    Ok(Some(ClaimedJob { work_id, job }))
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
