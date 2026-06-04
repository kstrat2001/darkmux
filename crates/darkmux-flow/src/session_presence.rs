//! (#638) Session liveness substrate — the session-level twin of machine
//! presence ([`crate::presence`]).
//!
//! A *running dispatch* refreshes a short-TTL Redis key
//! `darkmux:session-presence:<session_id>` every few seconds for as long as
//! the dispatch process lives. The live set is *"which
//! `darkmux:session-presence:*` keys currently exist"* — Redis TTL does the
//! stale-removal for free, with no timeout logic and no cross-machine
//! clock-skew problem (Redis's own clock governs expiry).
//!
//! This makes `"running"` a **positive liveness signal** instead of an
//! inference from a *missing* `dispatch.complete` record. The old viewer
//! marked any session without a complete record as "running" forever — so a
//! crashed, killed, or watchdog-timed-out dispatch (which never emits a
//! clean complete) lied as "running" indefinitely, and a *past date*
//! (playback of a finished day) showed day-old sessions as "running". With
//! a heartbeat, a dispatch that stops refreshing simply ages out of the
//! live set; the viewer keys "running" on key existence.
//!
//! Emitted by the **dispatch process** (which is alive exactly as long as
//! the session runs), NOT by the daemon — the daemon doesn't know about
//! interactively-launched dispatches. Read by the daemon's
//! `/fleet/sessions/live` endpoint, which the live viewer polls.
//!
//! Like machine presence, session presence is **ephemeral** and separate
//! from the durable flow stream: heartbeats are NOT flow records.

use crate::{open_redis_connection_bounded, REDIS_CONNECT_TIMEOUT};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Redis key namespace for session-liveness beats — one key per running
/// dispatch, `darkmux:session-presence:<session_id>`. Deliberately distinct
/// from machine presence's `darkmux:presence:` prefix so the machine-level
/// `SCAN darkmux:presence:*` never matches a session key (and vice versa).
const SESSION_KEY_PREFIX: &str = "darkmux:session-presence:";

/// Heartbeat cadence default: refresh every 5s. A live dispatch survives a
/// dropped beat or two before its key expires (see [`DEFAULT_TTL_SECS`]).
pub const DEFAULT_BEAT_INTERVAL_SECS: u64 = 5;

/// Session-key TTL default: 15s (≈ 3 missed beats at the 5s cadence). Redis
/// `EX` governs expiry, so the reader never times anything out itself.
pub const DEFAULT_TTL_SECS: u64 = 15;

/// What a running dispatch publishes each heartbeat. The load-bearing field
/// is `session_id` (the live-set membership the viewer gates "running" on);
/// the rest is best-effort enrichment for grouping/labelling the live
/// indicator, omitted from the wire when empty.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionBeat {
    /// The dispatch's session id — the key suffix and the join key against
    /// this session's flow records. Globally unique per dispatch.
    pub session_id: String,
    /// Stable hardware identity of the machine running the dispatch
    /// (`darkmux_hardware::machine_uid`), best-effort. Lets a reader group
    /// the live session under the right machine card. `None` off-Mac.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_uid: Option<String>,
    /// Mutable machine label (`resolve_machine_id`). Display-only.
    pub display_name: String,
    /// The dispatched role id (e.g. `coder`), best-effort enrichment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The model the dispatch is running, best-effort enrichment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Unix-ms at beat-write time. Diagnostic / "last beat" only — liveness
    /// is governed by Redis key existence (TTL), not by clock comparison.
    pub beat_ts_ms: u64,
}

/// The Redis key for a session's liveness beat (keyed on session id).
pub fn session_key(session_id: &str) -> String {
    format!("{SESSION_KEY_PREFIX}{session_id}")
}

/// Publish/refresh a session's liveness beat with `ttl_secs` expiry
/// (`SET darkmux:session-presence:<sid> <json> EX <ttl>`). Best-effort — a
/// Redis blip must never crash the dispatch, so errors propagate for the
/// emitter to swallow.
pub fn write_session_beat(client: &redis::Client, beat: &SessionBeat, ttl_secs: u64) -> Result<()> {
    let payload = serde_json::to_string(beat).context("serializing session beat")?;
    let mut conn = open_redis_connection_bounded(client, REDIS_CONNECT_TIMEOUT)
        .context("getting Redis connection for session-beat write")?;
    let _: redis::Value = redis::cmd("SET")
        .arg(session_key(&beat.session_id))
        .arg(payload)
        .arg("EX")
        .arg(ttl_secs)
        .query(&mut conn)
        .context("SET session beat")?;
    Ok(())
}

/// Read the currently-live sessions — every unexpired
/// `darkmux:session-presence:*` key, parsed back into [`SessionBeat`]. Order
/// is unspecified; malformed payloads are skipped (best-effort). Uses
/// cursor-based `SCAN` (non-blocking) rather than `KEYS`.
pub fn read_live_sessions(client: &redis::Client) -> Result<Vec<SessionBeat>> {
    let mut conn = open_redis_connection_bounded(client, REDIS_CONNECT_TIMEOUT)
        .context("getting Redis connection for session-presence read")?;
    let pattern = format!("{SESSION_KEY_PREFIX}*");
    let mut cursor = "0".to_string();
    let mut keys: Vec<String> = Vec::new();
    loop {
        let (next, batch): (String, Vec<String>) = redis::cmd("SCAN")
            .arg(&cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg(200)
            .query(&mut conn)
            .context("SCAN session-presence keys")?;
        keys.extend(batch);
        if next == "0" {
            break;
        }
        cursor = next;
    }
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        let val: Option<String> = redis::cmd("GET")
            .arg(&key)
            .query(&mut conn)
            .context("GET session-presence key")?;
        if let Some(json) = val {
            if let Ok(beat) = serde_json::from_str::<SessionBeat>(&json) {
                out.push(beat);
            }
        }
    }
    Ok(out)
}

/// A running session's heartbeat emitter. Owns the background refresh thread
/// and DELetes the key on a clean [`stop`](Self::stop) so the session drops
/// from the live set immediately; the TTL is the backstop for crashes that
/// skip `stop` entirely.
pub struct SessionEmitter {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    client: redis::Client,
    session_id: String,
}

impl SessionEmitter {
    /// Stop the heartbeat, join the refresh thread, and DELete the key so
    /// the live view drops the session immediately (rather than waiting out
    /// the TTL). Best-effort: a Redis blip on the final DEL just means the
    /// key ages out via TTL instead.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        if let Ok(mut conn) = open_redis_connection_bounded(&self.client, REDIS_CONNECT_TIMEOUT) {
            let _: std::result::Result<redis::Value, _> = redis::cmd("DEL")
                .arg(session_key(&self.session_id))
                .query(&mut conn);
        }
    }
}

impl Drop for SessionEmitter {
    fn drop(&mut self) {
        // If `stop` wasn't called (early `?`-return / panic between spawn
        // and the explicit stop), at least halt the refresh thread; the key
        // then ages out via TTL. No Redis DEL here — Drop must not block on
        // a network round-trip.
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Spawn a session-liveness heartbeat for the duration of a dispatch.
/// Refreshes `darkmux:session-presence:<session_id>` every
/// [`DEFAULT_BEAT_INTERVAL_SECS`] with a [`DEFAULT_TTL_SECS`] TTL until the
/// returned [`SessionEmitter`] is stopped or dropped.
///
/// **Self-disables** (returns `None`) when `DARKMUX_REDIS_URL` is unset —
/// single-machine, file-only fleets have no shared substrate to be live in,
/// and the viewer then shows terminal status only. The machine identity
/// (`machine_uid` + `display_name`) is stamped from the same source as flow
/// records, so the caller passes only the session-shaped fields.
pub fn spawn_session_emitter(
    session_id: String,
    role: Option<String>,
    model: Option<String>,
) -> Option<SessionEmitter> {
    let url = std::env::var("DARKMUX_REDIS_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    let client = redis::Client::open(url.as_str()).ok()?;

    let machine_uid = darkmux_hardware::machine_uid().map(str::to_string);
    let display_name = crate::resolve_machine_id().unwrap_or_else(|| "unknown".to_string());

    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread_client = client.clone();
    let beat_session_id = session_id.clone();

    let handle = std::thread::Builder::new()
        .name("darkmux-session-presence".to_string())
        .spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                let beat = SessionBeat {
                    session_id: beat_session_id.clone(),
                    machine_uid: machine_uid.clone(),
                    display_name: display_name.clone(),
                    role: role.clone(),
                    model: model.clone(),
                    beat_ts_ms: crate::presence::now_ms(),
                };
                // Best-effort: a failed write just means the key may lapse;
                // the next beat re-establishes it. Never crash the dispatch.
                let _ = write_session_beat(&thread_client, &beat, DEFAULT_TTL_SECS);
                // Interruptible sleep: check the stop flag every 250ms so
                // teardown joins promptly instead of waiting a full interval.
                for _ in 0..(DEFAULT_BEAT_INTERVAL_SECS * 4) {
                    if thread_stop.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            }
        })
        .ok()?;

    Some(SessionEmitter {
        stop,
        handle: Some(handle),
        client,
        session_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_beat() -> SessionBeat {
        SessionBeat {
            session_id: "crew-dispatch-coder-1780493601894484-internal".into(),
            machine_uid: Some("564D1234-ABCD-5678-9EF0-1234567890AB".into()),
            display_name: "laptop".into(),
            role: Some("coder".into()),
            model: Some("qwen3.6-35b".into()),
            beat_ts_ms: 1_780_000_000_000,
        }
    }

    #[test]
    fn session_key_is_namespaced_and_distinct_from_machine_presence() {
        assert_eq!(
            session_key("crew-dispatch-coder-123-internal"),
            "darkmux:session-presence:crew-dispatch-coder-123-internal"
        );
        // Must NOT collide with the machine-presence prefix — else the
        // machine `SCAN darkmux:presence:*` would scoop up session keys.
        assert!(!session_key("x").starts_with("darkmux:presence:"));
    }

    #[test]
    fn beat_round_trips_through_json() {
        let beat = sample_beat();
        let json = serde_json::to_string(&beat).unwrap();
        let back: SessionBeat = serde_json::from_str(&json).unwrap();
        assert_eq!(beat, back);
    }

    #[test]
    fn optional_fields_omitted_when_empty_and_default_back() {
        let beat = SessionBeat {
            session_id: "sid".into(),
            machine_uid: None,
            display_name: "mini".into(),
            role: None,
            model: None,
            beat_ts_ms: 1,
        };
        let json = serde_json::to_string(&beat).unwrap();
        assert!(!json.contains("machine_uid"), "None machine_uid omitted: {json}");
        assert!(!json.contains("role"), "None role omitted: {json}");
        assert!(!json.contains("model"), "None model omitted: {json}");
        let back: SessionBeat = serde_json::from_str(&json).unwrap();
        assert_eq!(beat, back);
    }

    #[test]
    fn minimal_wire_payload_parses() {
        // Only the load-bearing fields — proving enrichment is optional.
        let json = r#"{"session_id":"sid-9","display_name":"studio","beat_ts_ms":42}"#;
        let beat: SessionBeat = serde_json::from_str(json).unwrap();
        assert_eq!(beat.session_id, "sid-9");
        assert_eq!(beat.display_name, "studio");
        assert_eq!(beat.machine_uid, None);
        assert_eq!(beat.role, None);
    }

    /// On-demand integration check against a live Redis. `#[ignore]` so CI
    /// without Redis skips it; run with
    /// `cargo test -p darkmux-flow session_roundtrip -- --ignored` while
    /// `DARKMUX_REDIS_URL` points at a reachable Redis. Writes a uniquely-
    /// named beat, confirms `read_live_sessions` surfaces it, then DELetes
    /// it so it never lingers as a phantom live session.
    #[test]
    #[ignore]
    fn session_roundtrip_against_live_redis() {
        let Some(url) = std::env::var("DARKMUX_REDIS_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
        else {
            eprintln!("DARKMUX_REDIS_URL unset — skipping live session round-trip");
            return;
        };
        let client = redis::Client::open(url.as_str()).expect("open redis client");
        let sid = format!("session-selftest-{}", std::process::id());
        let beat = SessionBeat {
            session_id: sid.clone(),
            machine_uid: None,
            display_name: "selftest".into(),
            role: Some("coder".into()),
            model: None,
            beat_ts_ms: crate::presence::now_ms(),
        };
        write_session_beat(&client, &beat, DEFAULT_TTL_SECS).expect("write_session_beat");
        let live = read_live_sessions(&client).expect("read_live_sessions");
        let found = live.iter().find(|b| b.session_id == sid).cloned();
        // Clean up BEFORE asserting so a failure can't leak the key.
        let mut conn = open_redis_connection_bounded(&client, REDIS_CONNECT_TIMEOUT).unwrap();
        let _: redis::Value = redis::cmd("DEL")
            .arg(session_key(&sid))
            .query(&mut conn)
            .unwrap();
        assert_eq!(
            found.as_ref().map(|b| b.session_id.as_str()),
            Some(sid.as_str()),
            "self-test beat should appear in read_live_sessions"
        );
        assert_eq!(found.unwrap(), beat, "round-tripped beat should match");
    }
}
