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

use crate::{bound_redis_response, open_redis_connection_bounded, REDIS_CONNECT_TIMEOUT};
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
    // (#2227) The connect above is bounded; this `SET` was not. This is the
    // command the teardown path wedges on: the beat thread blocks here, so
    // `SessionEmitter::stop`'s `h.join()` never returns and the dispatch
    // strands without its terminal record. With the deadline the write fails,
    // the beat lapses (the TTL covers that), and teardown proceeds.
    bound_redis_response(&conn);
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
    // (#2227) Bounds each `SCAN`/`GET` below individually (the deadline is a
    // per-socket-read one, not a budget for the whole loop) — which is the
    // right shape here: `SCAN COUNT 200` is explicitly non-blocking, so no
    // single reply legitimately takes a second. This read serves the daemon's
    // `/fleet/sessions/live` endpoint, so an unbounded stall here holds an
    // HTTP worker as well as the caller.
    bound_redis_response(&conn);
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
        // (#647) This is the CLEAN-stop path — the dispatch is about to emit its
        // `dispatch.complete`, which is this session's authoritative close-edge.
        // Pre-claim `session-end:<sid>` BEFORE removing the key so the presence
        // reconciler, when it observes the key gone, LOSES the claim and skips
        // its `session.end` edge (which would be redundant with the complete).
        // An abandoned dispatch (host process killed) never reaches here, so it
        // never pre-claims — and the reconciler then wins + records the close,
        // which is exactly the interval bracket playback would otherwise lack.
        // (Benign edge: a Redis outage spanning longer than the claim's TTL can
        // let the pre-claim expire before the reconciler recovers, so a clean
        // session may get a redundant session.end alongside its complete. The
        // viewer's `closeTs=min(...)` + cleanClose still render it "complete".)
        let _ = crate::presence_reconciler::claim_edge(
            &self.client,
            "session-end",
            &self.session_id,
        );
        if let Ok(mut conn) = open_redis_connection_bounded(&self.client, REDIS_CONNECT_TIMEOUT) {
            // (#2227) The last of the three commands on the teardown path.
            // Already best-effort (the TTL is the backstop if the DEL is lost)
            // — but "best-effort" only holds if it can FAIL; unbounded, it
            // blocks instead, and this runs microseconds before
            // `dispatch.complete` is emitted.
            bound_redis_response(&conn);
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
    // env(DARKMUX_REDIS_URL) > config-assembled (#661 Slice 5).
    let url = crate::redis_url()?;
    let client = redis::Client::open(url.expose_for_probe()).ok()?;
    spawn_with_client(client, session_id, role, model)
}

/// (#2227) The emitter body, taking an explicit client. Split out of
/// [`spawn_session_emitter`] purely so the TEARDOWN path — a beat thread
/// blocked inside `write_session_beat`, joined by [`SessionEmitter::stop`] —
/// is reachable from a test pointed at a fake peer, without mutating
/// `DARKMUX_REDIS_URL` process-wide (which `isolate_test_env_once` scrubs, and
/// which would force every such test to be `#[serial]`). Production behavior
/// is unchanged: `spawn_session_emitter` resolves the URL and delegates here.
fn spawn_with_client(
    client: redis::Client,
    session_id: String,
    role: Option<String>,
    model: Option<String>,
) -> Option<SessionEmitter> {
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

    /// (#2227) THE lifecycle regression: `SessionEmitter::stop()` against a
    /// peer that completes the Redis handshake and then answers nothing must
    /// return within a bounded time.
    ///
    /// Why this is the bug's headline symptom and not just lost observability:
    /// `darkmux-crew`'s `dispatch_internal` calls `em.stop()` IMMEDIATELY
    /// before emitting `dispatch.complete`. `stop()` joins the beat thread —
    /// which is blocked inside `write_session_beat`'s unbounded `SET` and so
    /// cannot reach its stop-flag check — and then issues two MORE unbounded
    /// commands (`claim_edge`'s `SET NX`, then the `DEL`). Against a silent
    /// peer a dispatch therefore strands with a `dispatch.start` record and no
    /// terminal record at all. Measured before this fix: 89.42s against a
    /// fake peer that eventually closed; unbounded against a genuinely silent
    /// one. `spawn_session_emitter` gates on the same `crate::redis_url()` the
    /// flow sink does, so any operator who can hit #2227's sink hang has this
    /// heartbeat running in the same dispatch.
    ///
    /// Three separate `bound_redis_response` sites are load-bearing here, and
    /// the ceiling is deliberately tight (measured ~3.1s: three 1s socket
    /// deadlines plus connects) so that removing ANY ONE of them fails this
    /// test — an unbounded command blocks until the fake peer closes the
    /// socket, which puts the total at ~6.8s. A generous ceiling would let a
    /// missing bound hide behind the other two's slack.
    #[test]
    fn session_emitter_stop_against_silent_peer_returns_within_bounded_time() {
        // Budget: the phase guard, the first beat's SET, claim_edge's SET NX,
        // stop's DEL, plus slack for a second beat if the thread laps.
        let port = crate::spawn_silent_redis_peer(8);
        // `stop()` returns `()`, so wall-clock is the only thing this test can
        // assert — and a wall-clock bound passes just as well when the CONNECT
        // fails. Pin the phase first so this can't go vacuous the way round 1's
        // test did.
        crate::assert_silent_peer_reaches_command_phase(port);
        let client = redis::Client::open(format!("redis://127.0.0.1:{port}").as_str())
            .expect("open client against the fake peer");

        let emitter = spawn_with_client(
            client,
            "sid-2227-teardown".to_string(),
            Some("coder".to_string()),
            None,
        )
        .expect("spawn emitter");

        // Let the beat thread get INSIDE the SET before tearing down — that is
        // the state `dispatch_internal` tears down from, and the state whose
        // `h.join()` wedged.
        std::thread::sleep(std::time::Duration::from_millis(250));

        let start = std::time::Instant::now();
        emitter.stop();
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "SessionEmitter::stop() took {elapsed:?} against a command-silent \
             peer; expected bounded by 3 x REDIS_RESPONSE_TIMEOUT + connects \
             (~3.1s measured). Before #2227 this wedged (measured 89.42s), \
             stranding the dispatch with no terminal record."
        );
    }

    /// (#2227) Per-site bound: `write_session_beat`'s `SET`. The beat thread
    /// blocks here, which is what makes `stop()`'s `h.join()` unbounded — so
    /// this is the narrowest red-provable assertion for that site.
    #[test]
    fn write_session_beat_against_silent_peer_errs_within_bounded_time() {
        let port = crate::spawn_silent_redis_peer(2);
        let client = redis::Client::open(format!("redis://127.0.0.1:{port}").as_str()).unwrap();
        let beat = SessionBeat {
            session_id: "sid-2227-beat".into(),
            machine_uid: None,
            display_name: "test".into(),
            role: None,
            model: None,
            beat_ts_ms: 1,
        };

        let start = std::time::Instant::now();
        let res = write_session_beat(&client, &beat, DEFAULT_TTL_SECS);
        let elapsed = start.elapsed();

        let err = res.expect_err("a command-silent peer must surface as Err, not block");
        // Prove the failure came from the COMMAND phase, not the connect —
        // otherwise this degenerates into a duplicate of the #278 connect test.
        let msg = format!("{err:#}");
        assert!(
            msg.contains("SET session beat"),
            "expected a SET-phase error (the connect must have SUCCEEDED); got {msg}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "write_session_beat took {elapsed:?}; expected bounded by \
             REDIS_RESPONSE_TIMEOUT (1s) + connect. Unbounded before #2227."
        );
    }

    /// (#2227) Per-site bound: `read_live_sessions`'s `SCAN`. Backs the
    /// daemon's `/fleet/sessions/live` endpoint.
    #[test]
    fn read_live_sessions_against_silent_peer_errs_within_bounded_time() {
        let port = crate::spawn_silent_redis_peer(2);
        let client = redis::Client::open(format!("redis://127.0.0.1:{port}").as_str()).unwrap();

        let start = std::time::Instant::now();
        let res = read_live_sessions(&client);
        let elapsed = start.elapsed();

        let err = res.expect_err("a command-silent peer must surface as Err, not block");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("SCAN session-presence keys"),
            "expected a SCAN-phase error (the connect must have SUCCEEDED); got {msg}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "read_live_sessions took {elapsed:?}; expected bounded by \
             REDIS_RESPONSE_TIMEOUT (1s) + connect. Unbounded before #2227."
        );
    }
}
