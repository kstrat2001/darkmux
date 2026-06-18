//! (#647) Presence edge-recording — durable bookends for playback.
//!
//! Heartbeats ([`crate::presence`], [`crate::session_presence`]) stay
//! ephemeral and OUT of the durable flow stream (the deliberate no-noise
//! decision). But **playback** can't replay running/present *intervals* from
//! heartbeats that have since expired. So this records the two EDGES of each
//! presence episode as durable flow records — ~2 per episode, no heartbeat
//! noise:
//!
//! - **OPEN-edges are self-emitted** by the live entity, which is alive to
//!   speak: a machine emits `machine.online` at daemon startup
//!   ([`emit_machine_online_edge`]); a session's open-edge is its existing
//!   `dispatch.start` record.
//! - **CLOSE-edges are reconciler-emitted** — a daemon-side reconciler
//!   ([`spawn_reconciler_thread`]) observes a presence key *disappearing*
//!   (TTL expiry on crash, or DEL on clean stop) and records the close-edge.
//!   The crash case is *why* a peer must observe: a dispatch process killed
//!   mid-run (or a crashed daemon) can't emit its own close-edge — it's dead.
//!
//! **Dedup across a symmetric fleet** (#647): every live daemon observes the
//! same disappearance, so the first to win an atomic `SET NX` claim
//! ([`claim_edge`]) records the edge; the others skip. No leader/hub, correct
//! for any fleet size. The claim also disambiguates clean-vs-crash later
//! (a clean-stop path can pre-claim to suppress a redundant edge) — but this
//! Slice 1 covers machine edges only; sessions follow.

use crate::presence::{read_live, PresenceBeat};
use crate::session_presence::{read_live_sessions, SessionBeat};
use crate::{open_redis_connection_bounded, FlowRecord, REDIS_CONNECT_TIMEOUT};
use std::collections::{HashMap, HashSet};

/// Redis key namespace for edge-record claims — one short-lived key per
/// (transition-kind, id), e.g. `darkmux:edge-claim:machine-offline:<uid>`.
const EDGE_CLAIM_PREFIX: &str = "darkmux:edge-claim:";

/// Claim TTL: long enough that all peers observing the same transition in the
/// same reconcile window see the claim (so exactly one records the edge),
/// short enough to self-clear well before a realistic re-transition of the
/// same id (a machine going offline→online→offline).
///
/// Known Slice-1 tradeoff: a machine that flaps offline→online→offline *within*
/// this window has its second (legitimate) offline edge suppressed by the still-
/// held claim. Favors no-duplicate over completeness — acceptable here; the
/// clean-vs-crash disambiguation slice can revisit (e.g. generation-stamped
/// claim ids) if fast crash-restart-crash loops prove real.
const EDGE_CLAIM_TTL_SECS: u64 = 60;

/// Reconcile cadence: between the 5s presence beat and the 15s presence TTL,
/// so a disappearance is detected within ~one TTL window of the last beat.
pub const RECONCILE_INTERVAL_SECS: u64 = 7;

/// Source tag on edge records — lets the viewer/operator tell reconciler-
/// emitted lifecycle edges from work records at a glance.
const EDGE_SOURCE: &str = "presence-reconciler";

/// Try to claim the right to record one transition. Atomic `SET <key> 1 NX EX
/// <ttl>` — returns `true` iff THIS caller set the key (won the claim), `false`
/// if it already existed (a peer claimed it first). Best-effort: a Redis error
/// returns `false` (don't emit on a failed claim — better a missed edge than a
/// duplicate or a crash).
pub fn claim_edge(client: &redis::Client, kind: &str, id: &str) -> bool {
    let key = format!("{EDGE_CLAIM_PREFIX}{kind}:{id}");
    let mut conn = match open_redis_connection_bounded(client, REDIS_CONNECT_TIMEOUT) {
        Ok(c) => c,
        Err(_) => return false,
    };
    // `SET key 1 NX EX ttl` replies `+OK` when set, `nil` when the key existed.
    // Bind the reply to Option<String>: Some("OK") = won, None = lost.
    let res: redis::RedisResult<Option<String>> = redis::cmd("SET")
        .arg(&key)
        .arg("1")
        .arg("NX")
        .arg("EX")
        .arg(EDGE_CLAIM_TTL_SECS)
        .query(&mut conn);
    matches!(res, Ok(Some(_)))
}

/// Pure: the beats present in `last` whose uid is absent from `now_uids` —
/// i.e. the machines that disappeared since the previous reconcile tick.
/// Extracted from the loop so the diff is unit-testable without Redis.
fn disappeared_machines<'a>(
    last: &'a HashMap<String, PresenceBeat>,
    now_uids: &HashSet<String>,
) -> Vec<&'a PresenceBeat> {
    last.iter()
        .filter(|(uid, _)| !now_uids.contains(*uid))
        .map(|(_, beat)| beat)
        .collect()
}

/// Build a machine lifecycle edge record (`machine.online` / `machine.offline`)
/// for the given uid + display label. `machine_uid` and `machine_id` are set
/// EXPLICITLY to the *subject* machine — for an offline edge that's the
/// DISAPPEARED peer, not the local observer — which suppresses the write-time
/// auto-stamp (`record_to` stamps only when the field is `None`). Category is
/// `Machinery` (lifecycle, not work) and the event type rides in `action`
/// (free-form string — no enum variant, so no schema bump / cross-version
/// deser break).
fn build_machine_edge_record(action: &str, machine_uid: &str, display_name: &str) -> FlowRecord {
    FlowRecord {
        ts: crate::ts_utc_now(),
        level: crate::Level::Info,
        category: crate::Category::Machinery,
        tier: crate::Tier::Local,
        stage: crate::Stage::Dispatch,
        action: action.to_string(),
        handle: display_name.to_string(),
        sprint_id: None,
        session_id: None,
        source: Some(EDGE_SOURCE.to_string()),
        model: None,
        reasoning: None,
        mission_id: None,
        machine_id: Some(display_name.to_string()),
        machine_uid: Some(machine_uid.to_string()),
        orchestrator: None,
        prev_hash: None,
        hash: None,
        payload: None,
        work_id: None,
        attempt: None,
    }
}

/// Pure: the session beats present in `last` whose id is absent from
/// `now_sids` — the sessions that disappeared since the previous tick.
/// Unit-testable without Redis (sibling of [`disappeared_machines`]).
fn disappeared_sessions<'a>(
    last: &'a HashMap<String, SessionBeat>,
    now_sids: &HashSet<String>,
) -> Vec<&'a SessionBeat> {
    last.iter()
        .filter(|(sid, _)| !now_sids.contains(*sid))
        .map(|(_, beat)| beat)
        .collect()
}

/// Build a `session.end` close-edge from a disappeared session's last beat.
/// `session_id` is the subject; `machine_uid`/`machine_id` are set explicitly
/// from the beat (the session's machine, not the local observer) to suppress
/// the write-time auto-stamp. `handle` carries the role for the viewer.
///
/// A `session.end` is the close-edge for the **abandoned** case (host process
/// killed mid-run — no clean `dispatch.complete`). A cleanly-completed session
/// pre-claims `session-end:<sid>` in `SessionEmitter::stop` to SUPPRESS this
/// edge, keeping its `dispatch.complete` as the sole close. So in practice
/// `session.end` marks the crash/kill/timeout interval-close that playback
/// would otherwise have no bracket for.
fn build_session_end_record(beat: &SessionBeat) -> FlowRecord {
    FlowRecord {
        ts: crate::ts_utc_now(),
        level: crate::Level::Info,
        category: crate::Category::Machinery,
        tier: crate::Tier::Local,
        stage: crate::Stage::Dispatch,
        action: "session.end".to_string(),
        handle: beat.role.clone().unwrap_or_else(|| beat.session_id.clone()),
        sprint_id: None,
        session_id: Some(beat.session_id.clone()),
        source: Some(EDGE_SOURCE.to_string()),
        model: beat.model.clone(),
        reasoning: None,
        mission_id: None,
        machine_id: Some(beat.display_name.clone()),
        machine_uid: beat.machine_uid.clone(),
        orchestrator: None,
        prev_hash: None,
        hash: None,
        payload: None,
        work_id: None,
        attempt: None,
    }
}

/// Self-emit this machine's `machine.online` open-edge — call once at daemon
/// startup, after the presence emitter is up. Gated on a stable hardware uid
/// (no uid → the machine can't be identified, so it must not bookend under an
/// unprovable name, #640). Best-effort: a write failure is non-fatal.
pub fn emit_machine_online_edge() {
    let Some(uid) = darkmux_hardware::machine_uid() else {
        return;
    };
    let display_name = crate::resolve_machine_id().unwrap_or_else(|| "unknown".to_string());
    let _ = crate::record(build_machine_edge_record("machine.online", uid, &display_name));
}

/// Spawn the presence reconciler on a dedicated OS thread (sync redis client;
/// same shape as the presence emitter). **Self-disables** (returns `None`)
/// when `DARKMUX_REDIS_URL` is unset — no shared substrate to reconcile.
///
/// (#647): close-edges for both machines (`machine.offline`) and sessions
/// (`session.end`). Each tick reads the live machine + session sets; the FIRST
/// tick just establishes a baseline (entities already present when this daemon
/// started are not "appearances" — machine `online` is self-emitted by the
/// machine's own daemon, and a session's open-edge is its `dispatch.start`, so
/// we never emit appearances here). From the next tick on, any id that vanished
/// is a close-edge, recorded once across the fleet via [`claim_edge`]. A
/// cleanly-completed session pre-claims its `session-end` in
/// `SessionEmitter::stop`, so only abandoned (crash/kill/timeout) sessions —
/// the ones playback has no close bracket for otherwise — get a `session.end`.
pub fn spawn_reconciler_thread() -> Option<std::thread::JoinHandle<()>> {
    // env(DARKMUX_REDIS_URL) > config-assembled (#661 Slice 5). `RawRedisUrl`
    // moves into the thread; `expose_for_probe()` at the client open below.
    let url = crate::redis_url()?;

    let spawned = std::thread::Builder::new()
        .name("darkmux-presence-reconciler".to_string())
        .spawn(move || {
            let client = match redis::Client::open(url.expose_for_probe()) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!(
                        "{}",
                        darkmux_types::style::warn(&format!(
                            "presence-reconciler: could not open Redis client ({e}); disabled"
                        ))
                    );
                    return;
                }
            };
            eprintln!(
                "presence-reconciler: started — recording machine + session \
                 lifecycle edges (every {RECONCILE_INTERVAL_SECS}s)"
            );
            // Last-seen live machines + sessions, id → beat (the beat carries
            // the label/role needed to build a faithful close-edge once gone).
            let mut last_machines: HashMap<String, PresenceBeat> = HashMap::new();
            let mut last_sessions: HashMap<String, SessionBeat> = HashMap::new();
            let mut first_tick = true;
            // Edge-triggered health latch — mirrors the presence emitter
            // (presence.rs). If `read_live` starts failing, close-edges quietly
            // stop being recorded; "loud beats quiet" (project doctrine) means
            // the operator must SEE that, so log once on each fail↔recover
            // transition (never per-tick — a persistently-down Redis must not
            // spam the daemon log). Assume-healthy at start so a normal first
            // tick stays quiet. The machine read drives the latch; the session
            // read is a best-effort secondary (a session-read blip just keeps
            // the last session baseline, never fires a false `session.end`).
            let mut healthy = true;
            loop {
                match read_live(&client) {
                    Ok(beats) => {
                        if !healthy {
                            eprintln!("{}", darkmux_types::style::success("presence-reconciler: read_live recovered — recording edges again"));
                            healthy = true;
                        }
                        let now_machines: HashMap<String, PresenceBeat> = beats
                            .into_iter()
                            .map(|b| (b.machine_uid.clone(), b))
                            .collect();
                        let session_read = read_live_sessions(&client);
                        if !first_tick {
                            // Machine offline edges.
                            let now_uids: HashSet<String> =
                                now_machines.keys().cloned().collect();
                            for gone in disappeared_machines(&last_machines, &now_uids) {
                                // Dedup across the fleet: only the claim winner
                                // records the edge.
                                if claim_edge(&client, "machine-offline", &gone.machine_uid) {
                                    let _ = crate::record(build_machine_edge_record(
                                        "machine.offline",
                                        &gone.machine_uid,
                                        &gone.display_name,
                                    ));
                                }
                            }
                            // Session end edges (only when the session read
                            // succeeded — a blip must not read as a disappearance).
                            if let Ok(ref sbeats) = session_read {
                                let now_sids: HashSet<String> =
                                    sbeats.iter().map(|b| b.session_id.clone()).collect();
                                for gone in disappeared_sessions(&last_sessions, &now_sids) {
                                    // (#640 honesty) Skip a session whose machine
                                    // can't be proven (no hardware uid — e.g. off
                                    // Mac): emitting here would let the write-time
                                    // auto-stamp attribute the close-edge to THIS
                                    // observer's machine, not the session's. Better
                                    // no bracket (it shows "ended" in playback) than
                                    // one under an unprovable machine.
                                    if gone.machine_uid.is_none() {
                                        continue;
                                    }
                                    // A cleanly-completed session pre-claimed this
                                    // in SessionEmitter::stop, so the claim loses
                                    // here and no redundant edge is recorded; an
                                    // abandoned (crash/kill) session has no
                                    // pre-claim, so this wins and records the close.
                                    if claim_edge(&client, "session-end", &gone.session_id) {
                                        let _ = crate::record(build_session_end_record(gone));
                                    }
                                }
                            }
                        }
                        last_machines = now_machines;
                        // Keep the last session baseline on a session-read error
                        // (no false disappearances); refresh it on success.
                        if let Ok(sbeats) = session_read {
                            last_sessions = sbeats
                                .into_iter()
                                .map(|b| (b.session_id.clone(), b))
                                .collect();
                        }
                        first_tick = false;
                    }
                    Err(e) => {
                        // Best-effort: keep the last baseline, retry next tick —
                        // but surface the transition so a silent edge-recording
                        // stall is operator-visible (it isn't otherwise).
                        if healthy {
                            eprintln!(
                                "{}",
                                darkmux_types::style::warn(&format!(
                                    "presence-reconciler: read_live failing (close-edges \
                                     paused, retrying every {RECONCILE_INTERVAL_SECS}s): {e}"
                                ))
                            );
                            healthy = false;
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(RECONCILE_INTERVAL_SECS));
            }
        });

    match spawned {
        Ok(handle) => Some(handle),
        Err(e) => {
            eprintln!("{}", darkmux_types::style::warn(&format!("presence-reconciler: could not spawn thread ({e}); disabled")));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beat(uid: &str, name: &str) -> PresenceBeat {
        PresenceBeat {
            machine_uid: uid.to_string(),
            display_name: name.to_string(),
            schema_version: "1.11.0".to_string(),
            beat_ts_ms: 1,
            specs: None,
            loaded_models: Vec::new(),
        }
    }

    #[test]
    fn disappeared_is_last_minus_now() {
        let mut last = HashMap::new();
        last.insert("A".to_string(), beat("A", "studio"));
        last.insert("B".to_string(), beat("B", "laptop"));
        last.insert("C".to_string(), beat("C", "mini"));
        // Only A is still present.
        let now: HashSet<String> = ["A".to_string()].into_iter().collect();
        let mut gone: Vec<String> = disappeared_machines(&last, &now)
            .iter()
            .map(|b| b.machine_uid.clone())
            .collect();
        gone.sort();
        assert_eq!(gone, vec!["B".to_string(), "C".to_string()]);
    }

    #[test]
    fn no_disappearance_when_all_present() {
        let mut last = HashMap::new();
        last.insert("A".to_string(), beat("A", "studio"));
        let now: HashSet<String> = ["A".to_string()].into_iter().collect();
        assert!(disappeared_machines(&last, &now).is_empty());
    }

    #[test]
    fn appearance_is_not_a_disappearance() {
        // last is empty, now has a new machine → nothing disappeared (the
        // online edge is self-emitted by that machine, not reconciler-emitted).
        let last = HashMap::new();
        let now: HashSet<String> = ["NEW".to_string()].into_iter().collect();
        assert!(disappeared_machines(&last, &now).is_empty());
    }

    fn sbeat(sid: &str, role: &str) -> SessionBeat {
        SessionBeat {
            session_id: sid.to_string(),
            machine_uid: Some("UID-1".to_string()),
            display_name: "laptop".to_string(),
            role: Some(role.to_string()),
            model: Some("qwen3.6-35b".to_string()),
            beat_ts_ms: 1,
        }
    }

    #[test]
    fn disappeared_sessions_is_last_minus_now() {
        let mut last = HashMap::new();
        last.insert("S1".to_string(), sbeat("S1", "coder"));
        last.insert("S2".to_string(), sbeat("S2", "reviewer"));
        let now: HashSet<String> = ["S1".to_string()].into_iter().collect();
        let gone: Vec<String> = disappeared_sessions(&last, &now)
            .iter()
            .map(|b| b.session_id.clone())
            .collect();
        assert_eq!(gone, vec!["S2".to_string()]);
    }

    #[test]
    fn session_end_edge_carries_subject_session_and_machine() {
        // The close-edge must be attributed to the session's own machine
        // (explicit machine_uid/machine_id suppress the observer auto-stamp),
        // and carry the session id + role for the viewer to bracket it.
        let rec = build_session_end_record(&sbeat("crew-dispatch-coder-1-internal", "coder"));
        assert_eq!(rec.action, "session.end");
        assert_eq!(rec.session_id.as_deref(), Some("crew-dispatch-coder-1-internal"));
        assert_eq!(rec.machine_uid.as_deref(), Some("UID-1"));
        assert_eq!(rec.machine_id.as_deref(), Some("laptop"));
        assert_eq!(rec.handle, "coder");
        assert_eq!(rec.source.as_deref(), Some(EDGE_SOURCE));
    }

    #[test]
    fn offline_edge_subject_is_the_disappeared_machine() {
        // The edge's machine_uid/machine_id must be the DISAPPEARED peer (so
        // it's set explicitly and the write-time auto-stamp — which would put
        // the local observer's uid — is suppressed).
        let rec = build_machine_edge_record("machine.offline", "PEER-UID", "studio");
        assert_eq!(rec.machine_uid.as_deref(), Some("PEER-UID"));
        assert_eq!(rec.machine_id.as_deref(), Some("studio"));
        assert_eq!(rec.action, "machine.offline");
        assert_eq!(rec.source.as_deref(), Some(EDGE_SOURCE));
        assert!(matches!(rec.category, crate::Category::Machinery));
    }

    /// On-demand dedup check against a live Redis. `#[ignore]` so CI skips it;
    /// run with `cargo test -p darkmux-flow claim_edge_is -- --ignored` while
    /// `DARKMUX_REDIS_URL` points at a reachable Redis. The first claim wins,
    /// the second loses; cleans up the claim key.
    #[test]
    #[ignore]
    fn claim_edge_is_exclusive_against_live_redis() {
        let Some(url) = std::env::var("DARKMUX_REDIS_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
        else {
            eprintln!("DARKMUX_REDIS_URL unset — skipping live claim test");
            return;
        };
        let client = redis::Client::open(url.as_str()).expect("open redis client");
        let id = format!("reconciler-selftest-{}", std::process::id());
        let first = claim_edge(&client, "selftest", &id);
        let second = claim_edge(&client, "selftest", &id);
        // Clean up before asserting so a failure can't leak the claim key.
        let mut conn =
            open_redis_connection_bounded(&client, REDIS_CONNECT_TIMEOUT).unwrap();
        let _: redis::Value = redis::cmd("DEL")
            .arg(format!("{EDGE_CLAIM_PREFIX}selftest:{id}"))
            .query(&mut conn)
            .unwrap();
        assert!(first, "first claim should win");
        assert!(!second, "second claim should lose (key already set)");
    }
}
