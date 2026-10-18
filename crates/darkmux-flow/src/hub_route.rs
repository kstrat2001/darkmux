//! Which hub stream a record rides, if any: the one rule the live write
//! ([`crate::RedisSink`]'s `persist`) and the outage backfill
//! (`hub_link::Backfill::keep`) both apply, so the two can never disagree
//! about what reaches the fleet hub.
//!
//! By action, [`crate::FlowAction::hub_stream`]: `machine.telemetry` rides the
//! telemetry stream, every other action the work stream, and
//! `dispatch.turn.heartbeat` stays off the hub (#2101: at a 2 s cadence the
//! heartbeats flushed other machines' work out of the capped stream). Two
//! kinds of heartbeat are the exception, decided by the payload
//! ([`is_prompt_phase_heartbeat`], #3210): a turn's opener, or a prefill
//! report written before the reply's first chunk, the only records that say
//! how far a machine has read its prompt; and, while the reply streams, the
//! sparse generation beats their writer marked `hub_relay` (one per
//! execution every [`HUB_GENERATION_BEAT_INTERVAL_MS`], [`HubRelayGate`]), so
//! a peer's card keeps a rate and never reads a false STALL. Any other beat
//! that read a chunk stays local. Without these another machine's card can
//! show neither the prompt bar nor a rate. And a lab run's records never reach the hub, whatever their action (#3074,
//! contract 3).

use crate::payload::DispatchHeartbeatPayload;
use crate::{FlowAction, FlowRecord, HubStream, Payload};

/// (#3210) Whether a heartbeat was written while the turn was still reading
/// its prompt (one of the two kinds that ride the hub, with the marked
/// generation beats, [`heartbeat_rides_hub`]). The
/// rule is structural: the beat read no chunk of the reply (no
/// `partial_index`), nothing was generated (`generated_chars` is 0), and it
/// is either:
///
/// - the turn's opener, the one heartbeat that carries `prompt_chars` (the
///   request's size; one per stream, written before the request is sent), or
/// - an engine's prefill progress report (`prompt_processed` of
///   `prompt_total`) written before the first chunk.
///
/// A beat that read a chunk never travels, whatever its `generated_chars`
/// says: every beat after the prefill carries the turn's final count, and a
/// count of 0 generated is not proof the reply has not started (#3293
/// review: a reasoning field or tool-call arguments a reader did not count
/// left it at 0, and every 2 s beat matched). Every writer marks a beat that
/// read a chunk with `partial_index`: the tailer's chunk and writing-tick
/// beats (`dispatch_internal.rs` `Chunk::of_partial` / `of_writing`), and
/// the direct path's `chunk_beat` once a chunk was read. The opener and the
/// tailer's progress beat (`Chunk::of_progress`) carry none. A beat that does
/// not say how much was generated (`generated_chars` null) stays local.
pub fn is_prompt_phase_heartbeat(p: &DispatchHeartbeatPayload) -> bool {
    let opener = p.prompt_chars.is_some();
    let progress = p.prompt_processed.is_some() && p.prompt_total.is_some();
    p.partial_index.is_none() && p.generated_chars == Some(0) && (opener || progress)
}

/// (#3210) The most often a generation heartbeat of one execution rides the
/// hub: 20 s. The gate marks the first beat at least this long after its
/// last mark, so the hub's gap is this plus the writer's own longest silence
/// between durable beats (the PR #3293 re-review measured 43 s after a 25 s
/// local silence). The viewer therefore allows a relayed beat
/// `STALL_AFTER_MS` (30 s) plus this interval before STALL (`ui/src/lib/
/// tokenRate.ts`, `HUB_RELAY_INTERVAL_MS`, mirrored and pinned by
/// `the_viewer_mirrors_the_relay_interval`); a peer that falls silent longer
/// than that reads STALL as a local card would. Two relayed beats pair into
/// a rate like any other. Clock skew between machines eats into that
/// margin. About 3 records a minute per generating execution against the 30
/// a minute the local file keeps.
/// Without these, a peer card went from PROMPT to a false STALL the moment
/// its prefill ended (live check, PR #3293).
pub const HUB_GENERATION_BEAT_INTERVAL_MS: u64 = 20_000;

/// Whether a heartbeat rides the hub: a prompt-phase one, or a generation
/// beat its writer marked `hub_relay` ([`HubRelayGate`]).
pub fn heartbeat_rides_hub(p: &DispatchHeartbeatPayload) -> bool {
    is_prompt_phase_heartbeat(p) || p.hub_relay == Some(true)
}

/// The writer's side of the sparse generation beats: one per execution,
/// held by whatever writes its durable heartbeats, marking at most one beat
/// every [`HUB_GENERATION_BEAT_INTERVAL_MS`] on the beat's own clock
/// (`sampled_at_ms`), so the choice is fixed by the record, not by when the
/// writer ran. Prompt-phase beats ride the hub anyway and neither consume nor
/// wait on it.
#[derive(Debug, Default)]
pub struct HubRelayGate {
    last_ms: Option<u64>,
}

impl HubRelayGate {
    /// A new stream opened (its opener): the first generation beat of every
    /// stream is marked, so a turn shorter than the interval still gives a
    /// peer a beat to read GEN from. At most one more record per turn.
    pub fn reset(&mut self) {
        self.last_ms = None;
    }

    /// Mark `p` to ride the hub when it is a generation beat and one is due.
    /// A beat with no clock of its own (`sampled_at_ms` absent; every writer
    /// sets it) is not marked. A clock that stepped back marks at once.
    pub fn mark(&mut self, p: &mut DispatchHeartbeatPayload) {
        if is_prompt_phase_heartbeat(p) {
            return;
        }
        let Some(at) = p.sampled_at_ms else { return };
        let due = self.last_ms.is_none_or(|last| at < last || at - last >= HUB_GENERATION_BEAT_INTERVAL_MS);
        if due {
            self.last_ms = Some(at);
            p.hub_relay = Some(true);
        }
    }
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
            Some(Payload::DispatchTurnHeartbeat(p)) if heartbeat_rides_hub(p) => Some(HubStream::Work),
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
        assert!(
            !is_prompt_phase_heartbeat(&typed(json!({"turn_seq": 2, "partial_index": 1, "cumulative_chars": 0, "sampled_at_ms": 5,
                "generated_chars": 0, "prompt_processed": 2516, "prompt_total": 2516}))),
            "a beat that read a chunk, whatever it says was generated (#3293 review: reasoning the runtime did not count)"
        );
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

    /// (#3210, live check) The sparse generation beats: on the beat's own
    /// clock, a 2 s stream of generation beats sends one every 20 s,
    /// starting with the first; prompt-phase beats neither consume nor wait
    /// on the gate.
    #[test]
    fn the_gate_marks_one_generation_beat_per_interval() {
        let mut gate = HubRelayGate::default();
        let mut relayed = Vec::new();
        let mut opener = typed(opener());
        gate.mark(&mut opener);
        assert_eq!(opener.hub_relay, None, "a prompt-phase beat is not marked");
        for i in 0..=35u64 {
            let at = 10_000 + i * 2_000;
            let mut beat = typed(json!({"turn_seq": 2, "partial_index": i + 1, "cumulative_chars": i * 10, "sampled_at_ms": at, "generated_chars": i * 40 + 1}));
            gate.mark(&mut beat);
            if beat.hub_relay == Some(true) {
                relayed.push(at - 10_000);
            }
            assert!(beat.hub_relay != Some(false), "never written false");
        }
        assert_eq!(relayed, [0, 20_000, 40_000, 60_000]);
        let mut late_report = typed(progress_before_generation());
        gate.mark(&mut late_report);
        assert_eq!(late_report.hub_relay, None, "a prompt-phase beat never consumes the gate");
    }

    /// (#3210, PR #3293 re-review) A new stream resets the gate: the first
    /// generation beat after it is marked even inside the interval.
    #[test]
    fn a_new_stream_marks_its_first_generation_beat() {
        let mut gate = HubRelayGate::default();
        let gen = |at: u64, part: u64| typed(json!({"turn_seq": 2, "partial_index": part, "cumulative_chars": 1, "sampled_at_ms": at, "generated_chars": 40}));
        let mut first = gen(1_000, 1);
        gate.mark(&mut first);
        gate.reset();
        let mut next_turn = gen(6_000, 1);
        gate.mark(&mut next_turn);
        assert_eq!((first.hub_relay, next_turn.hub_relay), (Some(true), Some(true)), "5 s apart, across a new stream");
        let mut again = gen(8_000, 2);
        gate.mark(&mut again);
        assert_eq!(again.hub_relay, None, "inside the interval within one stream");
    }

    /// The viewer's mirror of [`HUB_GENERATION_BEAT_INTERVAL_MS`] (its stall
    /// bound for a relayed beat) must say the same number.
    #[test]
    fn the_viewer_mirrors_the_relay_interval() {
        let ts = include_str!("../../../ui/src/lib/tokenRate.ts");
        let digits = HUB_GENERATION_BEAT_INTERVAL_MS.to_string();
        let mut grouped = String::new();
        for (i, c) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i) % 3 == 0 {
                grouped.push('_');
            }
            grouped.push(c);
        }
        let want = format!("export const HUB_RELAY_INTERVAL_MS = {grouped};");
        assert!(ts.contains(&want), "ui/src/lib/tokenRate.ts must declare `{want}`");
    }

    /// (#3210, live check) A marked generation beat rides the work stream; an
    /// unmarked one stays local, and a lab run's marked beat stays local.
    #[test]
    fn a_marked_generation_beat_rides_the_hub() {
        let mut marked = plain_beat();
        marked["hub_relay"] = json!(true);
        assert_eq!(hub_stream_of_value(&line(marked.clone(), solo())), Some(HubStream::Work));
        assert_eq!(hub_stream_of_value(&line(plain_beat(), solo())), None);
        assert_eq!(hub_stream_of_value(&line(marked, lab())), None, "contract 3");
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
