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
//! execution every [`HUB_GENERATION_BEAT_INTERVAL_MS`], plus a stream's first
//! real output and each change into or out of a tool call, [`HubRelayGate`]), so
//! a peer's card keeps a rate and never reads a false STALL. Any other beat
//! that read a chunk stays local. Without these another machine's card can
//! show neither the prompt bar nor a rate. And a lab run's records never reach the hub, whatever their action (#3074,
//! contract 3).

use crate::payload::{DispatchHeartbeatPayload, StreamPhase};
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

/// (Matrix review) The most chars a stream's first generation beat may hold
/// and still be only the stream-open chunk (every stream opens with 1-5
/// chars before any real output). The viewer's `STREAM_OPEN_MAX_CHARS`
/// (`ui/src/lib/tokenRate.ts`, `openerPairTrusted`), mirrored and pinned by
/// `the_viewer_mirrors_the_stream_open_chunk`: a pair from the opener to a
/// beat at or under it is not a rate there, so the gate also relays the
/// first beat past it.
pub const STREAM_OPEN_MAX_CHARS: u64 = 16;

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
///
/// (Matrix review) Inside the interval, a beat is also marked when it says
/// something a peer cannot read from the beats already relayed: the
/// stream's first beat past the stream-open chunk ([`STREAM_OPEN_MAX_CHARS`];
/// without it a turn under 20 s gave a peer no rate), and a beat whose state
/// differs from the last marked one (entering a tool call, a different
/// call, or back to text; without it a peer never showed tool gen). The
/// cadence continues from the latest mark.
#[derive(Debug, Default)]
pub struct HubRelayGate {
    last_ms: Option<u64>,
    /// Whether this stream already relayed a beat past the stream-open chunk.
    output_relayed: bool,
    /// The state of the last marked beat: the phase and the tool written.
    last_state: Option<(Option<StreamPhase>, Option<String>)>,
}

impl HubRelayGate {
    /// A new stream opened (its opener): the first generation beat of every
    /// stream is marked, so a turn shorter than the interval still gives a
    /// peer a beat to read GEN from, and so is its first beat past the
    /// stream-open chunk.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Mark `p` to ride the hub when it is a generation beat and one is due.
    /// A beat with no clock of its own (`sampled_at_ms` absent; every writer
    /// sets it) is not marked. A clock that stepped back marks at once.
    pub fn mark(&mut self, p: &mut DispatchHeartbeatPayload) {
        if is_prompt_phase_heartbeat(p) {
            return;
        }
        let Some(at) = p.sampled_at_ms else { return };
        let past_chunk = p.generated_chars.is_some_and(|c| c > STREAM_OPEN_MAX_CHARS);
        let state = (p.phase, p.tool_name.clone());
        let interval_due = self.last_ms.is_none_or(|last| at < last || at - last >= HUB_GENERATION_BEAT_INTERVAL_MS);
        let first_output = past_chunk && !self.output_relayed;
        let state_changed = self.last_state.as_ref().is_some_and(|last| *last != state);
        if interval_due || first_output || state_changed {
            self.last_ms = Some(at);
            self.output_relayed |= past_chunk;
            self.last_state = Some(state);
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

/// Whether a record read back from this machine's day file is one the hub
/// never carries ([`hub_stream_of_value`] is `None`): a lab run's records,
/// and the heartbeats the rule keeps local (every unmarked generation beat).
/// The daemon's flow stream on the Redis path sends its own viewers exactly
/// these from the local file, so a lab run and the 2 s beats still reach
/// this machine's viewers as they are written, and a beat the hub carries
/// (prompt-phase, or marked `hub_relay`) reaches them once, from Redis.
pub fn stays_off_hub(record: &serde_json::Value) -> bool {
    hub_stream_of_value(record).is_none()
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
    /// starting with the first (the stream-open chunk) and the first past it
    /// (matrix review), the cadence counted from that; prompt-phase beats
    /// neither consume nor wait on the gate.
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
        assert_eq!(relayed, [0, 2_000, 22_000, 42_000, 62_000]);
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

    /// A generation beat at `at` with `chars` generated, writing `tool` when named.
    fn beat_at(at: u64, part: u64, chars: u64, tool: Option<&str>) -> DispatchHeartbeatPayload {
        let mut v = json!({"turn_seq": 10, "partial_index": part, "cumulative_chars": 0, "sampled_at_ms": at, "generated_chars": chars});
        if let Some(t) = tool {
            v["phase"] = json!("writing_tool_call");
            v["tool_name"] = json!(t);
        }
        typed(v)
    }

    fn marked_of(gate: &mut HubRelayGate, beats: Vec<DispatchHeartbeatPayload>) -> Vec<u64> {
        let mut out = Vec::new();
        for mut b in beats {
            gate.mark(&mut b);
            if b.hub_relay == Some(true) {
                out.push(b.sampled_at_ms.unwrap());
            }
        }
        out
    }

    /// (Matrix review) A short turn relayed only its stream-open chunk (2-4
    /// chars), which a peer cannot pair into a rate (the viewer's
    /// `openerPairTrusted`). The first beat past that chunk rides too, so the
    /// pair stream-open -> first real output is an ordinary pair. Real
    /// shape: PG2's turn 10 (2026-10-18, 11.8 s of visible text).
    #[test]
    fn the_first_beat_past_the_stream_open_chunk_rides_too() {
        let mut gate = HubRelayGate::default();
        gate.reset();
        let turn10 = vec![
            beat_at(1_792_325_921_976, 1, 3, None),
            beat_at(1_792_325_923_953, 153, 573, None),
            beat_at(1_792_325_925_983, 321, 1_263, None),
            beat_at(1_792_325_928_013, 450, 1_800, None),
            beat_at(1_792_325_930_072, 561, 2_321, None),
            beat_at(1_792_325_932_072, 689, 2_831, None),
        ];
        assert_eq!(marked_of(&mut gate, turn10), [1_792_325_921_976, 1_792_325_923_953], "the chunk, then the first real output");
        // A stream whose first beat already is real output marks one, as before.
        gate.reset();
        let big_first = vec![beat_at(1_000, 1, 40, None), beat_at(3_000, 2, 400, None)];
        assert_eq!(marked_of(&mut gate, big_first), [1_000]);
        // Exactly the chunk bound is still only a chunk; one past it is output.
        gate.reset();
        let edge = vec![beat_at(1_000, 1, 3, None), beat_at(2_000, 2, STREAM_OPEN_MAX_CHARS, None), beat_at(3_000, 3, STREAM_OPEN_MAX_CHARS + 1, None), beat_at(5_000, 4, 90, None)];
        assert_eq!(marked_of(&mut gate, edge), [1_000, 3_000]);
    }

    /// (Matrix review) Writing a tool call is a state a peer otherwise never
    /// sees inside 20 s: the gate marks the beat that enters a writing
    /// stretch (and the one that leaves it), so a peer card shows tool gen.
    /// Real shape: PG2's turn 8 (an `edit` written for 15 s).
    #[test]
    fn entering_and_leaving_a_tool_call_ride() {
        let mut gate = HubRelayGate::default();
        gate.reset();
        let mut turn8 = vec![beat_at(1_792_325_898_984, 1, 3, None), beat_at(1_792_325_900_856, 163, 656, None)];
        for at in [903_460u64, 905_469, 907_476, 909_483, 911_493, 914_501, 916_510] {
            turn8.push(beat_at(1_792_325_000_000 + at, 293, 1_238, Some("edit")));
        }
        turn8.push(beat_at(1_792_325_918_070, 294, 5_236, None));
        assert_eq!(
            marked_of(&mut gate, turn8),
            [1_792_325_898_984, 1_792_325_900_856, 1_792_325_903_460, 1_792_325_918_070],
            "the chunk, the first output, the call's first writing beat and the beat after it"
        );
        // A second call, named differently, is a new state too.
        gate.reset();
        let two = vec![beat_at(1_000, 1, 40, Some("read")), beat_at(3_000, 1, 40, Some("read")), beat_at(5_000, 2, 90, Some("edit"))];
        assert_eq!(marked_of(&mut gate, two), [1_000, 5_000]);
    }

    /// The viewer's mirror of [`STREAM_OPEN_MAX_CHARS`] (`openerPairTrusted`'s
    /// bound) must say the same number.
    #[test]
    fn the_viewer_mirrors_the_stream_open_chunk() {
        let ts = include_str!("../../../ui/src/lib/tokenRate.ts");
        let want = format!("const STREAM_OPEN_MAX_CHARS = {STREAM_OPEN_MAX_CHARS};");
        assert!(ts.contains(&want), "ui/src/lib/tokenRate.ts must declare `{want}`");
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

    /// The daemon's local-file fill is exactly the rule's complement: what the
    /// hub carries never comes from the file too (the viewer would see it
    /// twice), and what it never carries does (or this machine's viewer would
    /// not see it at all until a reconcile).
    #[test]
    fn stays_off_hub_is_exactly_what_persist_skips() {
        let mut marked = plain_beat();
        marked["hub_relay"] = json!(true);
        let rec = |action: &str, session: String| json!({"ts": "2026-10-18T00:00:00Z", "action": action, "handle": "h", "session_id": session});
        let cases = [
            (line(opener(), solo()), false, "a prompt-phase opener comes from Redis"),
            (line(progress_before_generation(), solo()), false, "a prefill report comes from Redis"),
            (line(marked.clone(), solo()), false, "a relayed generation beat comes from Redis"),
            (line(plain_beat(), solo()), true, "an unmarked 2 s generation beat comes from the file"),
            (line(progress_after_generation(), solo()), true, "a beat after generation started comes from the file"),
            (line(opener(), lab()), true, "a lab run's opener comes from the file"),
            (line(marked, lab()), true, "a lab run's marked beat comes from the file"),
            (rec("dispatch.turn", lab()), true, "a lab run's record comes from the file"),
            (rec("dispatch.turn", solo()), false, "work comes from Redis"),
            (rec("machine.telemetry", solo()), false, "a host sample comes from Redis"),
            (json!({"ts": "2026-10-18T00:00:00Z", "handle": "h"}), false, "a record naming no action rides the hub"),
        ];
        for (v, local, why) in cases {
            assert_eq!(stays_off_hub(&v), local, "{why}: {v}");
            let r = crate::reader::parse_record(&v.to_string());
            if let Some(r) = r {
                assert_eq!(stays_off_hub(&v), hub_stream_of_record(&r).is_none(), "persist routes it the same way: {v}");
            }
        }
    }
}
