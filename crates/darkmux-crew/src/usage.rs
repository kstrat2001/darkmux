//! (#2902 step 1a) The accounting writer: ONE usage record per model call.
//!
//! The MODEL CALL is darkmux's unit of accounting. Every call darkmux makes
//! to a model endpoint emits exactly one `telemetry.tokens` flow record
//! (category `telemetry`, source `tokens`) when its reply returns, and a
//! total anywhere is a plain sum of those records. This module owns the
//! record's PAYLOAD, so every producer stamps the same vocabulary:
//!
//! | field             | meaning                                                        |
//! |-------------------|----------------------------------------------------------------|
//! | `call_kind`       | `"turn"` · `"single_shot"` · `"map_item"` · `"compaction"` ([`CallKind`]) |
//! | `purpose`         | `"work"` · `"utility"` ([`UsagePurpose`], decided by [`call_purpose`], #2914) |
//! | `job`             | utility records only: [`UtilityJobKind`] (`"compaction"` · `"radio_routing"`, #2915) |
//! | `requested_model` | the model id darkmux put on the wire                           |
//! | `reported_model`  | the response's own `model` field; ABSENT when it had none      |
//! | `endpoint`        | the endpoint darkmux called, as a fact (see below)             |
//! | `endpoint_id`     | the `endpoints` id the call was made through; ABSENT for an unnamed endpoint (#2902 step 5) |
//! | `token_source`    | `"provider"` when the reply carried a usage block, else `"absent"` |
//! | `prompt_tokens` · `completion_tokens` · `total_tokens` · `reasoning_tokens` · `cached_tokens` | the counts, provider total wins |
//!
//! `endpoint` is what darkmux INVOKED, never a classification: for a hosted
//! call it is the same label string the dispatch bookends carry
//! (`target::endpoint_route_label`), for an LMStudio call the resolved LMStudio
//! base URL ([`lmstudio_endpoint`]). Nothing here decides whether an
//! endpoint is local, off-machine, metered or free, and no vendor is inferred
//! from a dialect.
//!
//! Counts are NEVER fabricated: a count the provider did not report is
//! omitted, and a reply with no usage block at all carries no count keys and
//! `token_source: "absent"`. `total_tokens` prefers the provider's own total
//! and falls back to `prompt + completion` only when the provider reported a
//! split without a total (arithmetic on reported numbers):
//! `darkmux_trajectory::UsageCounts::total_tokens`, the one rule every usage
//! record, every token sum and every budget settle reads.
//!
//! The producers, each calling [`usage_payload`] (directly or through
//! [`crate::single_shot::SingleShotReply::usage_payload`], the shared reply
//! seam both single-shot transports return):
//!
//! - the container path's per-turn tailer (`dispatch_internal`, `"turn"`)
//! - `dispatch_remote` and `dispatch_local_single_shot` (`"single_shot"`)
//! - the `dispatch.single_shot` step kind, both arms (`"single_shot"`)
//! - `dispatch.map`, one record per model call of each item, retries included
//!   (`"map_item"`, via `map_call_token_payload`)
//! - the container path's tailer again, one per runtime COMPACTOR call
//!   (`"compaction"`, #2902 step 1b), from each `compaction.call` trajectory
//!   event. A compactor call is a sub-execution of a utility role, so its
//!   record's `handle`/`model` are the compactor's, never the specialist's
//!   (CLAUDE.md contract 8), and its `endpoint` is the LMStudio base the
//!   compactor client called (it never takes a hosted brain's route).
//!
//! `reported_model` on a turn record comes from the runtime's
//! `model.completed` event (#2902 step 1b), parsed from the reply (the
//! streaming path takes it from the chunks).
//!
//! `usage_conformance` (tests) drives each one and holds the roster.

use darkmux_types::execution_id::ExecutionId;

pub use darkmux_flow::payload::{CallKind, TokenSource, UsagePayload, UsagePurpose, UtilityErrorPayload, UtilityJobKind, UtilityStartPayload};

/// (#2902, #2914, #2915) THE definition of darkmux's utility jobs: every
/// runtime COMPACTOR call, and every call made by the radio ROUTING role
/// ([`crate::loader::RADIO_ROUTER_ROLE_ID`]). Everything else is work
/// (`None`). No other code may hard-code this list: #2914 routes exactly
/// these jobs to the machine's one utility model, [`call_purpose`] derives
/// `purpose` from it, and #2915's `utility.start` names the job it returns.
///
/// `role_id` is the role the call ran for, when the call site has one (a
/// `dispatch.single_shot`/`dispatch.map` STEP runs no role, so `None`).
pub fn utility_job(call_kind: CallKind, role_id: Option<&str>) -> Option<UtilityJobKind> {
    if call_kind == CallKind::Compaction {
        Some(UtilityJobKind::Compaction)
    } else if role_id == Some(crate::loader::RADIO_ROUTER_ROLE_ID) {
        Some(UtilityJobKind::RadioRouting)
    } else {
        None
    }
}

/// (#2902, #2914) WHOSE job a call was, read off [`utility_job`]: utility
/// when it names a job, work otherwise.
pub fn call_purpose(call_kind: CallKind, role_id: Option<&str>) -> UsagePurpose {
    if utility_job(call_kind, role_id).is_some() {
        UsagePurpose::Utility
    } else {
        UsagePurpose::Work
    }
}


/// (#2915 review, MUST 1) A fresh id for one utility job, echoed by its
/// start and its end (usage record or `utility.error`), so the viewer pairs
/// an end with ITS start. Routing has no session to pair on, and a start
/// orphaned by a killed process must never absorb a later job's end.
/// Unique per process (a counter) and across processes (pid, microseconds).
pub fn mint_utility_job_id(job: UtilityJobKind) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    let job = serde_json::to_value(job).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
    format!("{job}-{micros}-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed))
}

/// Milliseconds since the epoch, for the ms-precision times utility markers
/// carry (a flow record's `ts` is whole-second).
pub fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// (#2915 review, C4) Stamp a utility job's END on its usage record: the
/// `job_id` its start minted, `ended_at_ms`, and `duration_ms` from the
/// start's `started_at_ms`. A sub-second job's start and end share a
/// whole-second `ts`; these keep them apart.
pub fn stamp_utility_end(payload: &mut UsagePayload, job_id: &str, started_at_ms: u64, ended_at_ms: u64) {
    payload.job_id = Some(job_id.to_string());
    payload.ended_at_ms = Some(ended_at_ms);
    payload.duration_ms = Some(ended_at_ms.saturating_sub(started_at_ms));
}

/// (#2915) The end of a started job that has no usage record: its
/// `utility.error` payload, with the same end stamps a usage record carries.
pub fn utility_error_payload(
    job: UtilityJobKind,
    model: &str,
    job_id: &str,
    started_at_ms: u64,
    ended_at_ms: u64,
) -> UtilityErrorPayload {
    UtilityErrorPayload {
        job,
        model: model.to_string(),
        job_id: job_id.to_string(),
        ended_at_ms,
        duration_ms: ended_at_ms.saturating_sub(started_at_ms),
    }
}

/// (#2915) The payload of a `utility.start`: the `job`, the `model` it runs
/// on (the wire id), the session id of the execution it `serves` (ABSENT
/// when it serves none, as routing does), and `stall_after_ms`, the
/// job's own bound, after which a start with no end reads as stalled (the
/// inactivity window for a compaction, the call timeout for routing). The
/// bound rides on the record so the viewer never guesses a knob the host
/// already knows (a recorded cadence, not an assumed one).
///
/// (#2915 review) Also `job_id` (echoed by the job's end) and
/// `started_at_ms` (ms precision; the record's `ts` is whole-second).
pub fn utility_start_payload(
    job: UtilityJobKind,
    job_id: &str,
    model: &str,
    serves: Option<&str>,
    stall_after_ms: u64,
    started_at_ms: u64,
) -> UtilityStartPayload {
    UtilityStartPayload {
        job,
        job_id: job_id.to_string(),
        model: model.to_string(),
        stall_after_ms,
        started_at_ms,
        serves: serves.map(str::to_string),
        generation: None,
        step_id: None,
        context: None,
    }
}

/// (#2915) A host-side (sessionless) utility job's lifecycle marker: a
/// `utility.start` or `utility.error` (the payload's own action), `handle`
/// the job's role id, the same attribution its usage record carries.
pub fn utility_marker_record(job_role_id: &str, model: &str, payload: darkmux_flow::Payload) -> darkmux_flow::FlowRecord {
    let mut rec = utility_record(job_role_id, model, payload);
    // A marker is not a record of the execution (its `job_id` pairs it with
    // its job), so it carries no execution id.
    rec.execution_id = None;
    if rec.action == darkmux_flow::FlowAction::UtilityError {
        rec.level = darkmux_flow::Level::Warn;
    }
    rec.source = Some(darkmux_flow::FlowSource::Utility);
    rec
}

/// What darkmux knows about the call itself, independent of what it cost.
#[derive(Clone, Copy, Debug)]
pub struct CallFacts<'a> {
    pub call_kind: CallKind,
    /// (#2914) The role the call ran for, when the call site runs one (the
    /// container path, `dispatch_remote`, `dispatch_local_single_shot`); a
    /// `dispatch.single_shot`/`dispatch.map` STEP runs no role (`None`).
    /// Read only by [`call_purpose`].
    pub role_id: Option<&'a str>,
    pub requested_model: &'a str,
    pub reported_model: Option<&'a str>,
    pub endpoint: &'a str,
    /// (#2902 step 5) The profile registry's `endpoints` id this call was
    /// made through (`ModelEndpoint::named_id`), when it has one. What an
    /// endpoint's rolling-window budget sums by (`crate::budget`); the
    /// `endpoint` label above is a host/model display string, not a key.
    pub endpoint_id: Option<&'a str>,
}

/// THE writer: one call's canonical `telemetry.tokens` payload.
pub fn usage_payload(facts: &CallFacts<'_>, counts: &darkmux_trajectory::UsageCounts) -> UsagePayload {
    let reported = counts.reported();
    let count = |value: Option<u64>| if reported { value } else { None };
    UsagePayload {
        call_kind: Some(facts.call_kind),
        purpose: Some(call_purpose(facts.call_kind, facts.role_id)),
        requested_model: Some(facts.requested_model.to_string()),
        endpoint: Some(facts.endpoint.to_string()),
        // (#2915) A utility call names its job, so the viewer pairs it with
        // the job's `utility.start` and tallies each job's own usage.
        job: utility_job(facts.call_kind, facts.role_id),
        reported_model: facts.reported_model.map(str::to_string),
        endpoint_id: facts.endpoint_id.map(str::to_string),
        token_source: Some(if reported { TokenSource::Provider } else { TokenSource::Absent }),
        prompt_tokens: count(counts.prompt),
        completion_tokens: count(counts.completion),
        total_tokens: count(counts.total_tokens()),
        reasoning_tokens: count(counts.reasoning),
        cached_tokens: count(counts.cached),
        turn_seq: None,
        remote: None,
        index: None,
        generation: None,
        parent_role_id: None,
        parent_model: None,
        job_id: None,
        ended_at_ms: None,
        duration_ms: None,
        step_id: None,
        context: None,
    }
}

/// (#2914) The flow record for a host-side UTILITY job's usage: the same
/// `telemetry.tokens` shape every producer writes, minus the session. A
/// utility job mints no session and writes no bookends (the amended
/// contract 2), so `session_id` is `None` and `handle` is the JOB's role
/// id (`radio-router`), the way a compactor call's record is attributed to
/// `compactor`. The job is one role execution of a utility role, so its
/// record names the `execution` the caller minted for it. Built here,
/// beside the payload writer, so the record and its payload cannot drift
/// apart.
pub fn utility_usage_record(job_role_id: &str, model: &str, execution: &ExecutionId, payload: UsagePayload) -> darkmux_flow::FlowRecord {
    darkmux_flow::FlowRecord {
        execution_id: Some(execution.clone()),
        source: Some(darkmux_flow::FlowSource::Tokens),
        ..utility_record(job_role_id, model, darkmux_flow::Payload::TelemetryTokens(payload))
    }
}

/// The record shape a host-side utility job's records share: telemetry,
/// darkmux's, no session, on the job's role and model.
fn utility_record(job_role_id: &str, model: &str, payload: darkmux_flow::Payload) -> darkmux_flow::FlowRecord {
    darkmux_flow::FlowRecord {
        ts: darkmux_flow::ts_utc_now(),
        level: darkmux_flow::Level::Info,
        category: darkmux_flow::Category::Telemetry,
        tier: darkmux_flow::Tier::Darkmux,
        stage: darkmux_flow::Stage::Dispatch,
        action: payload.action(),
        handle: job_role_id.to_string(),
        phase_id: None,
        session_id: None,
        execution_id: None,
        source: None,
        model: Some(model.to_string()),
        reasoning: None,
        mission_id: None,
        machine_id: None,
        machine_uid: None,
        prev_hash: None,
        hash: None,
        payload: Some(payload),
    }
}

/// The `endpoint` fact for a call to LMStudio: the resolved LMStudio base
/// (`base_override`, else the configured `lmstudio_url`), normalized to its
/// `/v1` root the same way the chat URL is built. The HOST-side address, so
/// every path that reaches the same LMStudio records the same string (the
/// container dials it through `host.docker.internal`; that rewrite is a
/// transport detail, not a different endpoint).
pub fn lmstudio_endpoint(base_override: Option<&str>) -> String {
    let base = base_override
        .map(str::to_string)
        .unwrap_or_else(darkmux_types::config_access::lmstudio_url);
    darkmux_types::endpoint::lmstudio_v1_base(&base)
}

/// (tests) The canonical-record check every conformance test shares:
/// exactly one `telemetry.tokens` among `records` (flow records as JSON),
/// carrying the canonical fields for `kind`. Returns it for further checks.
#[cfg(test)]
pub(crate) fn assert_one_usage_record<'a>(
    records: &'a [serde_json::Value],
    kind: CallKind,
    path: &str,
) -> &'a serde_json::Value {
    let usage: Vec<&serde_json::Value> = records
        .iter()
        .filter(|r| r["category"] == "telemetry" && r["source"] == "tokens")
        .collect();
    assert_eq!(
        usage.len(),
        1,
        "{path}: one model call must emit exactly one `telemetry.tokens` record, got {usage:#?}"
    );
    let rec = usage[0];
    // flow-action-guard:allow — a test-only assertion on the wire form
    assert_eq!(rec["action"], "telemetry.tokens", "{path}");
    let p = &rec["payload"];
    assert_eq!(p["call_kind"], serde_json::json!(kind), "{path}: call_kind: {p}");
    assert!(
        serde_json::from_value::<UsagePurpose>(p["purpose"].clone()).is_ok(),
        "{path}: every usage record carries a `purpose`: {p}"
    );
    assert!(
        p["requested_model"].as_str().is_some_and(|s| !s.is_empty()),
        "{path}: requested_model: {p}"
    );
    assert!(
        p["endpoint"].as_str().is_some_and(|s| !s.is_empty()),
        "{path}: endpoint: {p}"
    );
    assert!(
        p["token_source"] == "provider" || p["token_source"] == "absent",
        "{path}: token_source: {p}"
    );
    rec
}

// ── (#2902 step 5) Reading a usage record back ──────────────────────────
//
// The per-record half of every token sum: what ONE `telemetry.tokens`
// record contributes, in one value domain. Moved here from
// `darkmux_serve::usage_sum` (which re-exports it unchanged, and remains the
// fold over many records: `run list --usage`, `GET /runs`) so the endpoint
// budget's rolling-window sum (`crate::budget`) reads a record exactly the
// way the fold does. The viewer's twin is `ui/src/lib/usageRecords.ts`.

/// One record's contribution: the twin of `usageContribution`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageAmount {
    /// What the record's counts add up to: its total, else whatever halves
    /// it reported. A lower bound when a half is missing; display sums show
    /// it, and the endpoint budget counts it as the call's known spend.
    pub total: u64,
    pub prompt: u64,
    pub completion: u64,
    pub cached: Option<u64>,
    pub purpose: UsagePurpose,
    /// True when the payload carried any token count.
    pub reported: bool,
    /// What the call SPENT, by the one total rule
    /// ([`darkmux_trajectory::UsageCounts::total_tokens`]): `None` when it is
    /// unknown (no usage, or no prompt count). The endpoint budget reads it
    /// to know whether `total` is the full spend or only a floor.
    pub spend: Option<u64>,
}

/// True for a usage record (`telemetry.tokens`), by either mark it carries
/// (category + source, or the action).
pub fn is_usage_record(v: &serde_json::Value) -> bool {
    let category = v.get("category").and_then(|x| x.as_str());
    (category == Some("telemetry") && darkmux_flow::reader::source_of(v) == Some(darkmux_flow::FlowSource::Tokens))
        || darkmux_flow::reader::action_of(v) == Some(darkmux_flow::FlowAction::TelemetryTokens)
}

pub fn payload_of(v: &serde_json::Value) -> &serde_json::Value {
    static EMPTY: serde_json::Value = serde_json::Value::Null;
    v.get("payload").unwrap_or(&EMPTY)
}

/// The largest count either side holds exactly: `2^53`, the edge of a JS
/// number's integer range and comfortably inside `u64`. Every count is
/// clamped to it, so a sum of clamped counts is the same arithmetic in
/// both twins (`usageRecords.ts` spells the same constant).
pub const MAX_COUNT: u64 = 1 << 53;

/// THE value domain, shared with the viewer's `num`: a finite number is
/// floored to an integer and clamped to `[0, MAX_COUNT]`; anything else (a
/// string, a bool, null, a negative) reads as 0. "Reported" is judged by
/// this same reading everywhere, so a negative count is not a count.
fn num(v: Option<&serde_json::Value>) -> u64 {
    let Some(x) = v else { return 0 };
    if let Some(u) = x.as_u64() {
        return u.min(MAX_COUNT);
    }
    match x.as_f64() {
        // `as u64` already saturates and truncates toward zero; the clamp
        // is what keeps the two twins on one edge.
        Some(f) if f.is_finite() && f > 0.0 => (f as u64).min(MAX_COUNT),
        _ => 0,
    }
}

/// True when a value is a finite number at all — the presence test for
/// `cached_tokens` (a reported `-3` is a reported 0, not an absence).
fn is_finite_number(v: &serde_json::Value) -> bool {
    v.as_u64().is_some() || v.as_i64().is_some() || v.as_f64().is_some_and(f64::is_finite)
}

/// A record's `purpose`. Records from before flow schema 1.59.0 carry none;
/// for those (THE LEGACY RULE, the only one) a compactor call is utility and
/// anything else, a legacy `dispatch complete` included, is work. The twin
/// of `usagePurpose`.
pub fn usage_purpose(payload: &serde_json::Value) -> UsagePurpose {
    if let Some(p) = payload.get("purpose") {
        if let Ok(purpose) = serde_json::from_value::<UsagePurpose>(p.clone()) {
            return purpose;
        }
    }
    let compaction = payload
        .get("call_kind")
        .and_then(|k| serde_json::from_value::<CallKind>(k.clone()).ok())
        == Some(CallKind::Compaction);
    if compaction {
        UsagePurpose::Utility
    } else {
        UsagePurpose::Work
    }
}

/// True when a payload carries any token count (the twin of
/// `hasAnyTokenCounts`).
pub fn has_any_token_counts(p: &serde_json::Value) -> bool {
    num(p.get("total_tokens")) > 0
        || num(p.get("prompt_tokens")) > 0
        || num(p.get("completion_tokens")) > 0
        || num(p.get("remote_tokens")) > 0
}

pub fn amount_of(p: &serde_json::Value) -> UsageAmount {
    let prompt = num(p.get("prompt_tokens"));
    let completion = num(p.get("completion_tokens"));
    let mut total = num(p.get("total_tokens"));
    if total == 0 {
        total = prompt + completion;
    }
    if total == 0 {
        // The retired review path's spelling of its own spend, on a legacy
        // `dispatch complete` only.
        total = num(p.get("remote_tokens"));
    }
    let cached = p.get("cached_tokens").filter(|c| is_finite_number(c)).map(|c| num(Some(c)));
    UsageAmount {
        total,
        prompt,
        completion,
        cached,
        purpose: usage_purpose(p),
        reported: has_any_token_counts(p),
        spend: darkmux_trajectory::UsageCounts::from_provider(p).total_tokens(),
    }
}

/// The per-record half of the sum: what one usage record adds, or `None`
/// when `v` is not a usage record.
pub fn usage_contribution(v: &serde_json::Value) -> Option<UsageAmount> {
    if !is_usage_record(v) {
        return None;
    }
    Some(amount_of(payload_of(v)))
}

#[cfg(test)]
mod tests {

    /// (#2902 step 5) `endpoint_id` is written when the call went through a
    /// named endpoint, and absent otherwise (never an empty string): the
    /// endpoint budget sums records by it.
    #[test]
    fn endpoint_id_is_written_only_for_a_named_endpoint() {
        let facts = |endpoint_id| CallFacts {
            call_kind: CallKind::SingleShot,
            role_id: None,
            requested_model: "m",
            reported_model: None,
            endpoint: "h/m",
            endpoint_id,
        };
        let counts = darkmux_trajectory::UsageCounts { total: Some(5), ..Default::default() };
        assert_eq!(usage_payload(&facts(Some("azure")), &counts).endpoint_id.as_deref(), Some("azure"));
        assert!(usage_payload(&facts(None), &counts).endpoint_id.is_none());
        assert_eq!(usage_payload(&facts(Some("azure")), &darkmux_trajectory::UsageCounts::default()).endpoint_id.as_deref(), Some("azure"), "an absent-usage record still names its endpoint");
    }
    /// The sums read a usage record as raw JSON, in the one value domain the
    /// viewer shares (`num`: floor, clamp, hostile values read 0), because a
    /// typed `u64` cannot express that domain. This pins the two halves
    /// together: what the typed writer serializes is what the reader sums.
    #[test]
    fn the_value_domain_reader_reads_what_the_typed_writer_writes() {
        let facts = CallFacts {
            call_kind: CallKind::SingleShot,
            role_id: None,
            requested_model: "m",
            reported_model: None,
            endpoint: "h/m",
            endpoint_id: Some("azure"),
        };
        let counts = darkmux_trajectory::UsageCounts { total: Some(42), prompt: Some(30), completion: Some(12), cached: Some(7), ..Default::default() };
        let record = serde_json::json!({
            "category": "telemetry",
            "action": "telemetry.tokens",
            "payload": serde_json::to_value(usage_payload(&facts, &counts)).unwrap(),
        });
        let amount = usage_contribution(&record).expect("a telemetry.tokens record is a usage record");
        assert_eq!((amount.total, amount.prompt, amount.completion, amount.cached), (42, 30, 12, Some(7)));
        assert!(amount.reported);
        assert_eq!(amount.purpose, call_purpose(CallKind::SingleShot, None));

        let unreported = serde_json::json!({
            "category": "telemetry",
            "action": "telemetry.tokens",
            "payload": serde_json::to_value(usage_payload(&facts, &darkmux_trajectory::UsageCounts::default())).unwrap(),
        });
        let amount = usage_contribution(&unreported).expect("still a usage record");
        assert!(!amount.reported, "a call that reported nothing sums as unreported, not as zero spend");
        assert_eq!((amount.total, amount.cached), (0, None));
    }

    use super::*;

    fn facts(reported: Option<&'static str>) -> CallFacts<'static> {
        CallFacts {
            endpoint_id: None,
            call_kind: CallKind::SingleShot,
            role_id: None,
            requested_model: "m",
            reported_model: reported,
            endpoint: "http://h:1234/v1",
        }
    }

    #[test]
    fn provider_total_wins_over_the_sum() {
        let p = serde_json::to_value(usage_payload(
            &facts(Some("served")),
            &darkmux_trajectory::UsageCounts {
                prompt: Some(10),
                completion: Some(5),
                total: Some(40),
                ..Default::default()
            },
        )).unwrap();
        assert_eq!(p["total_tokens"], 40);
        assert_eq!(p["token_source"], "provider");
        assert_eq!(p["reported_model"], "served");
        assert_eq!(p["call_kind"], "single_shot");
    }

    /// (#2914) The utility-job rule, on the record: a compactor call and
    /// every radio-router call are utility; everything else is work.
    #[test]
    fn purpose_names_darkmux_utility_jobs_and_nothing_else() {
        let purpose = |call_kind, role_id| {
            let f = CallFacts {
                endpoint_id: None,
                call_kind,
                role_id,
                requested_model: "m",
                reported_model: None,
                endpoint: "http://h:1234/v1",
            };
            usage_payload(&f, &darkmux_trajectory::UsageCounts::default()).purpose
        };
        let utility = Some(UsagePurpose::Utility);
        let work = Some(UsagePurpose::Work);
        assert_eq!(purpose(CallKind::Compaction, Some("compactor")), utility, "compactor call");
        assert_eq!(purpose(CallKind::Compaction, None), utility, "compactor call, no role");
        assert_eq!(
            purpose(CallKind::SingleShot, Some(crate::loader::RADIO_ROUTER_ROLE_ID)),
            utility,
            "radio routing (single-shot)"
        );
        assert_eq!(
            purpose(CallKind::Turn, Some(crate::loader::RADIO_ROUTER_ROLE_ID)),
            utility,
            "radio routing (container turn)"
        );
        assert_eq!(purpose(CallKind::Turn, Some("coder")), work, "a coder turn");
        assert_eq!(purpose(CallKind::SingleShot, Some("radio-host")), work, "another role's single-shot");
        assert_eq!(purpose(CallKind::SingleShot, None), work, "a single-shot step");
        assert_eq!(purpose(CallKind::MapItem, None), work, "a map item");
    }

    /// (#2915) Every utility usage record names its job; a work record
    /// carries no `job` key at all.
    #[test]
    fn a_utility_usage_record_names_its_job_and_work_carries_none() {
        let payload = |call_kind, role_id| {
            let f = CallFacts {
                endpoint_id: None,
                call_kind,
                role_id,
                requested_model: "m",
                reported_model: None,
                endpoint: "http://h:1234/v1",
            };
            usage_payload(&f, &darkmux_trajectory::UsageCounts::default())
        };
        assert_eq!(payload(CallKind::Compaction, Some("compactor")).job, Some(UtilityJobKind::Compaction));
        assert_eq!(
            payload(CallKind::SingleShot, Some(crate::loader::RADIO_ROUTER_ROLE_ID)).job,
            Some(UtilityJobKind::RadioRouting)
        );
        let work = serde_json::to_value(payload(CallKind::Turn, Some("coder"))).unwrap();
        assert!(work.get("job").is_none(), "work names no job: {work}");
    }

    /// (#2915) `utility_job` is the one list, and `purpose` follows it.
    #[test]
    fn utility_job_is_the_single_definition_purpose_reads() {
        assert_eq!(utility_job(CallKind::Compaction, None), Some(UtilityJobKind::Compaction));
        assert_eq!(
            utility_job(CallKind::Turn, Some(crate::loader::RADIO_ROUTER_ROLE_ID)),
            Some(UtilityJobKind::RadioRouting)
        );
        assert_eq!(utility_job(CallKind::MapItem, None), None);
        assert_eq!(utility_job(CallKind::SingleShot, Some("radio-host")), None);
    }

    /// (#2915) The start marker's payload: the job, the model, the bound,
    /// and `serves` only when the job serves an execution.
    #[test]
    fn utility_start_payload_names_job_model_bound_and_what_it_serves() {
        let p = serde_json::to_value(utility_start_payload(UtilityJobKind::Compaction, "j-1", "darkmux:u4b", Some("sid-1"), 600_000, 5)).unwrap();
        assert_eq!(p["job_id"], "j-1");
        assert_eq!(p["started_at_ms"], 5);
        assert_eq!(p["job"], "compaction");
        assert_eq!(p["model"], "darkmux:u4b");
        assert_eq!(p["serves"], "sid-1");
        assert_eq!(p["stall_after_ms"], 600_000);
        let r = utility_start_payload(UtilityJobKind::RadioRouting, "j-2", "u4b", None, 30_000, 5);
        assert!(serde_json::to_value(&r).unwrap().get("serves").is_none(), "absent, never null: {r:?}");
        let rec = utility_marker_record("radio-router", "u4b", darkmux_flow::Payload::UtilityStart(r));
        assert_eq!(rec.action, darkmux_flow::FlowAction::UtilityStart);
        assert_eq!(rec.source, Some(darkmux_flow::FlowSource::Utility));
        assert!(rec.session_id.is_none(), "a host-side utility job has no session");
    }

    /// (#2915 review, MUST 1) Ids minted back to back, inside one
    /// microsecond, never collide: the counter, not the clock, guarantees it.
    #[test]
    fn utility_job_ids_are_unique_even_within_one_microsecond() {
        let ids: std::collections::BTreeSet<String> =
            (0..2000).map(|_| mint_utility_job_id(UtilityJobKind::RadioRouting)).collect();
        assert_eq!(ids.len(), 2000);
        assert!(ids.iter().all(|i| i.starts_with("radio_routing-")));
    }

    /// The wire spellings the viewer reads, pinned so a serde rename is a
    /// visible change (the generated TS binding carries the same union).
    #[test]
    fn wire_spellings_are_snake_case() {
        assert_eq!(serde_json::json!(UtilityJobKind::Compaction), "compaction");
        assert_eq!(serde_json::json!(UtilityJobKind::RadioRouting), "radio_routing");
        assert_eq!(serde_json::json!(UsagePurpose::Work), "work");
        assert_eq!(serde_json::json!(UsagePurpose::Utility), "utility");
        assert_eq!(serde_json::json!(CallKind::SingleShot), "single_shot");
        assert_eq!(serde_json::json!(CallKind::MapItem), "map_item");
        assert_eq!(serde_json::json!(CallKind::Compaction), "compaction");
        assert_eq!(serde_json::json!(CallKind::Turn), "turn");
    }

    #[test]
    fn a_split_without_a_total_sums() {
        let p = serde_json::to_value(usage_payload(
            &facts(None),
            &darkmux_trajectory::UsageCounts {
                prompt: Some(10),
                completion: Some(5),
                ..Default::default()
            },
        )).unwrap();
        assert_eq!(p["total_tokens"], 15);
        assert!(p.get("reported_model").is_none(), "absent, not null: {p}");
        assert!(
            p.get("reasoning_tokens").is_none(),
            "unreported count omitted: {p}"
        );
    }

    #[test]
    fn no_usage_block_is_absent_with_no_counts() {
        let p = serde_json::to_value(usage_payload(&facts(None), &darkmux_trajectory::UsageCounts::default())).unwrap();
        assert_eq!(p["token_source"], "absent");
        for k in [
            "prompt_tokens",
            "completion_tokens",
            "total_tokens",
            "reasoning_tokens",
            "cached_tokens",
        ] {
            assert!(p.get(k).is_none(), "{k} must not be fabricated: {p}");
        }
    }

    #[test]
    fn lmstudio_endpoint_normalizes_to_the_v1_root() {
        assert_eq!(
            lmstudio_endpoint(Some("http://h:1234/")),
            "http://h:1234/v1"
        );
        assert_eq!(
            lmstudio_endpoint(Some("http://h:1234/v1")),
            "http://h:1234/v1"
        );
    }
}
