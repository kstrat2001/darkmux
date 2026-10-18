//! Which hub stream a record rides, if any: the one rule the live write
//! ([`crate::RedisSink`]'s `persist`) and the outage backfill
//! (`hub_link::Backfill::keep`) both apply, so the two can never disagree
//! about what reaches the fleet hub.
//!
//! By action, [`crate::FlowAction::hub_stream`]: `machine.telemetry` rides the
//! telemetry stream, every other action the work stream, and
//! `dispatch.turn.heartbeat` stays off the hub (#2101: at a 2 s cadence the
//! heartbeats flushed other machines' work out of the capped stream). One
//! kind of heartbeat is the exception, decided by its payload
//! ([`is_prompt_phase_heartbeat`], #3210): a heartbeat from a turn's prefill,
//! which is the only record that says how far a machine has read its prompt.
//! Without it another machine's card cannot show the prompt bar. And a lab
//! run's records never reach the hub, whatever their action (#3074,
//! contract 3).

use crate::payload::DispatchHeartbeatPayload;
use crate::{FlowAction, FlowRecord, HubStream, Payload};

/// (#3210) Whether a heartbeat was written while the turn was still reading
/// its prompt, which is the only kind of heartbeat that rides the hub. Nothing
/// generated yet (`generated_chars` is 0), and either:
///
/// - the turn's opener, the one heartbeat that carries `prompt_chars` (the
///   request's size; one per turn, written before the request is sent), or
/// - an engine's prefill progress report (`prompt_processed` of
///   `prompt_total`).
///
/// A beat written after generation starts also carries the turn's latest
/// progress count, so "carries a count" alone would send every 2 s beat of a
/// turn to the hub; the zero-generated condition is what keeps those local.
/// A beat that does not say how much was generated (`generated_chars` null,
/// an older runtime) stays local.
pub(crate) fn is_prompt_phase_heartbeat(p: &DispatchHeartbeatPayload) -> bool {
    let opener = p.prompt_chars.is_some();
    let progress = p.prompt_processed.is_some() && p.prompt_total.is_some();
    p.generated_chars == Some(0) && (opener || progress)
}

/// The hub stream a record rides, `None` when it stays local. The one rule;
/// see the module doc. `action` is `None` for a record that names none (an
/// archive line from before actions), which rides the work stream as it
/// always has.
pub(crate) fn hub_stream_for(action: Option<&FlowAction>, payload: Option<&Payload>, session_id: Option<&str>) -> Option<HubStream> {
    if crate::is_lab_session(session_id) {
        return None;
    }
    let Some(action) = action else {
        return Some(HubStream::Work);
    };
    if *action == FlowAction::DispatchTurnHeartbeat {
        return match payload {
            Some(Payload::DispatchTurnHeartbeat(p)) if is_prompt_phase_heartbeat(p) => Some(HubStream::Work),
            _ => None,
        };
    }
    action.hub_stream()
}

/// [`hub_stream_for`] over a record being written.
pub(crate) fn hub_stream_of_record(record: &FlowRecord) -> Option<HubStream> {
    hub_stream_for(Some(&record.action), record.payload.as_ref(), record.session_id.as_deref())
}

/// [`hub_stream_for`] over a record read back from a day file. Only a
/// heartbeat's payload is read: the backfill walks every line of an outage,
/// and typing a tool result's payload to decide nothing would be waste.
pub(crate) fn hub_stream_of_value(record: &serde_json::Value) -> Option<HubStream> {
    let action = crate::reader::action_of(record);
    let payload = (action == Some(FlowAction::DispatchTurnHeartbeat)).then(|| crate::reader::payload_of(record)).flatten();
    hub_stream_for(action.as_ref(), payload.as_ref(), record.get("session_id").and_then(|s| s.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_types::session_id::{RunId, SessionId};
    use serde_json::json;

    fn solo() -> String {
        SessionId::adhoc(RunId::standalone("s-3210").unwrap(), "coder", "n").wire()
    }

    fn lab() -> String {
        SessionId::adhoc(RunId::lab("l-3210").unwrap(), "coder", "n").wire()
    }

    /// The heartbeat shapes the writers produce (`dispatch_internal.rs`'s
    /// `opening_heartbeat_payload` / `Chunk::of_progress` / `of_partial`, and
    /// `direct_stream.rs`'s `opener` / `chunk_beat`), as JSON payloads.
    fn opener() -> serde_json::Value {
        json!({"turn_seq": 2, "cumulative_chars": 0, "sampled_at_ms": 1, "generated_chars": 0, "prompt_chars": 9000})
    }
    fn progress_before_generation() -> serde_json::Value {
        json!({"turn_seq": 2, "cumulative_chars": 0, "sampled_at_ms": 2, "generated_chars": 0, "prompt_processed": 1024, "prompt_total": 2516})
    }
    fn progress_after_generation() -> serde_json::Value {
        json!({"turn_seq": 2, "partial_index": 4, "cumulative_chars": 30, "sampled_at_ms": 3, "generated_chars": 42, "prompt_processed": 2516, "prompt_total": 2516})
    }
    fn plain_beat() -> serde_json::Value {
        json!({"turn_seq": 2, "partial_index": 9, "cumulative_chars": 80, "sampled_at_ms": 4, "generated_chars": 120})
    }

    fn line(payload: serde_json::Value, session: String) -> serde_json::Value {
        json!({"ts": "2026-10-18T00:00:00Z", "level": "info", "category": "work", "tier": "local", "stage": "dispatch",
            "action": "dispatch.turn.heartbeat", "handle": "h", "session_id": session, "payload": payload})
    }

    fn typed(payload: serde_json::Value) -> DispatchHeartbeatPayload {
        serde_json::from_value(payload).unwrap()
    }

    /// (#3210) The predicate: the opener and a prefill report before anything
    /// is generated ride the hub; a report after generation started, and a
    /// plain beat, do not.
    #[test]
    fn only_prompt_phase_heartbeats_are_prompt_phase() {
        assert!(is_prompt_phase_heartbeat(&typed(opener())), "the opener");
        assert!(is_prompt_phase_heartbeat(&typed(progress_before_generation())), "a prefill report");
        assert!(!is_prompt_phase_heartbeat(&typed(progress_after_generation())), "a beat after generation carries the count too");
        assert!(!is_prompt_phase_heartbeat(&typed(plain_beat())), "a plain beat");
        let mut unsaid = progress_before_generation();
        unsaid["generated_chars"] = serde_json::Value::Null;
        assert!(!is_prompt_phase_heartbeat(&typed(unsaid)), "a beat that does not say what was generated");
        let mut half = progress_before_generation();
        half.as_object_mut().unwrap().remove("prompt_total");
        assert!(!is_prompt_phase_heartbeat(&typed(half)), "a count with no total");
    }

    /// (#3210) Over a record read back from a day file: the prompt-phase
    /// beats ride the work stream, every other beat stays local, and a lab
    /// run's opener stays local too (contract 3).
    #[test]
    fn a_day_file_line_routes_by_the_same_rule() {
        let route = |payload, session| hub_stream_of_value(&line(payload, session));
        assert_eq!(route(opener(), solo()), Some(HubStream::Work));
        assert_eq!(route(progress_before_generation(), solo()), Some(HubStream::Work));
        assert_eq!(route(progress_after_generation(), solo()), None);
        assert_eq!(route(plain_beat(), solo()), None);
        assert_eq!(route(opener(), lab()), None, "a lab run's opener stays local");
        assert_eq!(route(progress_before_generation(), lab()), None, "a lab run's report stays local");
        let no_payload = json!({"ts": "2026-10-18T00:00:00Z", "action": "dispatch.turn.heartbeat", "handle": "h"});
        assert_eq!(hub_stream_of_value(&no_payload), None, "a heartbeat with no payload stays local");
    }

    /// Every other action routes exactly as [`FlowAction::hub_stream`] says,
    /// and a lab run's records of any action stay local.
    #[test]
    fn other_actions_keep_their_action_route() {
        let rec = |action: &str, session: String| json!({"ts": "2026-10-18T00:00:00Z", "action": action, "handle": "h", "session_id": session});
        assert_eq!(hub_stream_of_value(&rec("dispatch.tool", solo())), Some(HubStream::Work));
        assert_eq!(hub_stream_of_value(&rec("machine.telemetry", solo())), Some(HubStream::Telemetry));
        assert_eq!(hub_stream_of_value(&rec("dispatch.tool", lab())), None);
        assert_eq!(hub_stream_of_value(&json!({"ts": "2026-10-18T00:00:00Z", "handle": "h"})), Some(HubStream::Work), "a record naming no action");
    }

    /// The record twin agrees with the JSON one on every shape.
    #[test]
    fn the_record_and_the_value_route_alike() {
        for payload in [opener(), progress_before_generation(), progress_after_generation(), plain_beat()] {
            for session in [solo(), lab()] {
                let v = line(payload.clone(), session);
                let r = crate::reader::parse_record(&v.to_string()).expect("a record");
                assert_eq!(hub_stream_of_record(&r), hub_stream_of_value(&v), "{v}");
            }
        }
    }
}
