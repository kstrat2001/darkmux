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
//! | `requested_model` | the model id darkmux put on the wire                           |
//! | `reported_model`  | the response's own `model` field; ABSENT when it had none      |
//! | `endpoint`        | the endpoint darkmux called, as a fact (see below)             |
//! | `token_source`    | `"provider"` when the reply carried a usage block, else `"absent"` |
//! | `prompt_tokens` · `completion_tokens` · `total_tokens` · `reasoning_tokens` · `cached_tokens` | the counts, provider total wins |
//!
//! `endpoint` is what darkmux INVOKED, never a classification: for a hosted
//! call it is the same label string the dispatch bookends carry
//! (`remote_endpoint_label`), for an LMStudio call the resolved LMStudio
//! base URL ([`lmstudio_endpoint`]). Nothing here decides whether an
//! endpoint is local, off-machine, metered or free, and no vendor is inferred
//! from a dialect.
//!
//! Counts are NEVER fabricated: a count the provider did not report is
//! omitted, and a reply with no usage block at all carries no count keys and
//! `token_source: "absent"`. `total_tokens` prefers the provider's own total
//! and falls back to `prompt + completion` only when the provider reported a
//! split without a total (arithmetic on reported numbers, the same precedence
//! `turn_tokens_payload` and the map-item payload always used).
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

/// The flow-record action every usage record carries.
pub const USAGE_ACTION: &str = "telemetry.tokens";
/// The flow-record telemetry `source` every usage record carries.
pub const USAGE_SOURCE: &str = "tokens";

/// Which kind of model call a usage record accounts for. Serialized into
/// the payload's `call_kind` through serde (the variant names ARE the wire
/// spelling), and exported to the viewer as a generated TS type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum CallKind {
    /// One agent-loop turn of the container runtime.
    Turn,
    /// One container-free single-shot chat completion.
    SingleShot,
    /// One `dispatch.map` item.
    MapItem,
    /// (#2902 step 1b) One runtime compactor call: a sub-execution of the
    /// utility role, attributed to it, never to the specialist.
    Compaction,
}

/// (#2902, #2914) WHOSE job a model call was: the operator's WORK, or one of
/// darkmux's own UTILITY jobs. Stamped on every usage record as `purpose`
/// by [`usage_payload`], decided by [`call_purpose`] (the single definition
/// of the utility jobs). The viewer's hero shows utility as its own chip, and
/// an execution's own numbers (run page tiles, mission-graph step meter)
/// exclude it (CLAUDE.md contract 8: sub-executions are never blended into
/// the primary).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum UsagePurpose {
    /// The operator's work: every call that is not a utility job.
    Work,
    /// darkmux's own job, run on the machine's utility model.
    Utility,
}

/// (#2902, #2914) THE definition of darkmux's utility jobs: every runtime
/// COMPACTOR call, and every call made by the radio ROUTING role
/// ([`crate::loader::RADIO_ROUTER_ROLE_ID`]). Everything else is work. No
/// other code may hard-code this list: #2914 routes exactly these jobs to
/// the machine's one utility model, and must read the same definition.
///
/// `role_id` is the role the call ran for, when the call site has one (a
/// `dispatch.single_shot`/`dispatch.map` STEP runs no role, so `None`).
pub fn call_purpose(call_kind: CallKind, role_id: Option<&str>) -> UsagePurpose {
    let utility = call_kind == CallKind::Compaction
        || role_id == Some(crate::loader::RADIO_ROUTER_ROLE_ID);
    if utility {
        UsagePurpose::Utility
    } else {
        UsagePurpose::Work
    }
}

/// The token counts a reply reported. Every field is tri-state: `None`
/// means the provider did not say, never zero.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageCounts {
    pub prompt: Option<u64>,
    pub completion: Option<u64>,
    pub total: Option<u64>,
    pub reasoning: Option<u64>,
    pub cached: Option<u64>,
}

impl UsageCounts {
    /// True when the reply carried a usage block this record can count.
    pub fn reported(&self) -> bool {
        self.prompt.is_some() || self.completion.is_some() || self.total.is_some()
    }
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
}

/// THE writer: one call's canonical `telemetry.tokens` payload.
pub fn usage_payload(facts: &CallFacts<'_>, counts: &UsageCounts) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "call_kind": facts.call_kind,
        "purpose": call_purpose(facts.call_kind, facts.role_id),
        "requested_model": facts.requested_model,
        "endpoint": facts.endpoint,
    });
    let obj = payload.as_object_mut().expect("json! built an object");
    if let Some(m) = facts.reported_model {
        obj.insert("reported_model".into(), serde_json::json!(m));
    }
    if !counts.reported() {
        obj.insert("token_source".into(), serde_json::json!("absent"));
        return payload;
    }
    obj.insert("token_source".into(), serde_json::json!("provider"));
    let total = counts
        .total
        .or_else(|| match (counts.prompt, counts.completion) {
            (None, None) => None,
            (p, c) => Some(p.unwrap_or(0).saturating_add(c.unwrap_or(0))),
        });
    for (key, value) in [
        ("prompt_tokens", counts.prompt),
        ("completion_tokens", counts.completion),
        ("total_tokens", total),
        ("reasoning_tokens", counts.reasoning),
        ("cached_tokens", counts.cached),
    ] {
        if let Some(v) = value {
            obj.insert(key.into(), serde_json::json!(v));
        }
    }
    payload
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
    crate::single_shot::lmstudio_v1_base(&base)
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
        .filter(|r| r["category"] == "telemetry" && r["source"] == USAGE_SOURCE)
        .collect();
    assert_eq!(
        usage.len(),
        1,
        "{path}: one model call must emit exactly one `{USAGE_ACTION}` record, got {usage:#?}"
    );
    let rec = usage[0];
    assert_eq!(rec["action"], USAGE_ACTION, "{path}");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(reported: Option<&'static str>) -> CallFacts<'static> {
        CallFacts {
            call_kind: CallKind::SingleShot,
            role_id: None,
            requested_model: "m",
            reported_model: reported,
            endpoint: "http://h:1234/v1",
        }
    }

    #[test]
    fn provider_total_wins_over_the_sum() {
        let p = usage_payload(
            &facts(Some("served")),
            &UsageCounts {
                prompt: Some(10),
                completion: Some(5),
                total: Some(40),
                ..Default::default()
            },
        );
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
                call_kind,
                role_id,
                requested_model: "m",
                reported_model: None,
                endpoint: "http://h:1234/v1",
            };
            usage_payload(&f, &UsageCounts::default())["purpose"].clone()
        };
        let utility = serde_json::json!(UsagePurpose::Utility);
        let work = serde_json::json!(UsagePurpose::Work);
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

    /// The wire spellings the viewer reads, pinned so a serde rename is a
    /// visible change (the generated TS binding carries the same union).
    #[test]
    fn wire_spellings_are_snake_case() {
        assert_eq!(serde_json::json!(UsagePurpose::Work), "work");
        assert_eq!(serde_json::json!(UsagePurpose::Utility), "utility");
        assert_eq!(serde_json::json!(CallKind::SingleShot), "single_shot");
        assert_eq!(serde_json::json!(CallKind::MapItem), "map_item");
        assert_eq!(serde_json::json!(CallKind::Compaction), "compaction");
        assert_eq!(serde_json::json!(CallKind::Turn), "turn");
    }

    #[test]
    fn a_split_without_a_total_sums() {
        let p = usage_payload(
            &facts(None),
            &UsageCounts {
                prompt: Some(10),
                completion: Some(5),
                ..Default::default()
            },
        );
        assert_eq!(p["total_tokens"], 15);
        assert!(p.get("reported_model").is_none(), "absent, not null: {p}");
        assert!(
            p.get("reasoning_tokens").is_none(),
            "unreported count omitted: {p}"
        );
    }

    #[test]
    fn no_usage_block_is_absent_with_no_counts() {
        let p = usage_payload(&facts(None), &UsageCounts::default());
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
