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
/// and DELetes the key on a clean [`stop`](Self::stop) — or, per (#2344), on
/// [`Drop`] when `stop` was never reached (an early `?`-return or a caught
/// panic unwinding through the dispatch) — so the session drops from the
/// live set immediately; the TTL remains the backstop for the one exit this
/// type structurally cannot observe: the whole host process dying (killed,
/// OOM, `panic = "abort"`), which takes this Drop with it before it can run.
pub struct SessionEmitter {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    client: redis::Client,
    session_id: String,
    /// Set once [`remove_presence_key`](Self::remove_presence_key) has run,
    /// so an explicit [`stop`](Self::stop) — which consumes `self` and so
    /// always runs `Drop` immediately afterward — never pays for the
    /// teardown twice.
    beat_removed: bool,
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
        self.remove_presence_key();
    }

    /// Pre-claim the reconciler edge, then DEL the presence key — the
    /// teardown shared by the CLEAN [`stop`](Self::stop) path and the
    /// [`Drop`] backstop below (#2344). Idempotent via `beat_removed`, and
    /// bounded the same way [`stop`](Self::stop) already was: every call
    /// here goes through `bound_redis_response`'s deadline (#2227), which
    /// caps each individual socket READ at ~1s. That is NOT the same thing
    /// as "bounded against any peer" — `bound_redis_response` sets a
    /// per-read deadline, not a per-command or per-call one, so a peer that
    /// keeps emitting any byte more often than that deadline (never fully
    /// silent, just slow) can hold the read open indefinitely; measured
    /// against such a peer, this can run far longer than the ~3.1s figure
    /// below (#2344 review CONSIDER 5). This class of hang is pre-existing
    /// — [`stop`](Self::stop) already had it before this change — so
    /// running it from `Drop` during a panic unwind is not a NEW hazard
    /// this type introduced by no longer skipping the network round-trip;
    /// it is the same hazard `stop()` already accepted, now also reachable
    /// from the backstop.
    ///
    /// **Considered and deferred (#2344 review CONSIDER 5):** every
    /// `spawn_session_emitter` call site declares the dispatch's bookend
    /// guard BEFORE the `SessionEmitter`, so on scope exit Rust's reverse-
    /// drop order runs the emitter's (now-slow-peer-hangable) `Drop`
    /// FIRST — ahead of the bookend guard's own `Drop`, which is what
    /// emits the dispatch's terminal record for an unclosed unit. A hung
    /// emitter `Drop` there delays that terminal, which didn't used to
    /// depend on this call at all. Swapping the declaration order (emitter
    /// first, bookend second) would let the terminal win regardless — but
    /// that touches five call sites across two crates (`dispatch_internal.
    /// rs`'s hosted/container arms, `builtins.rs`'s single-shot/map arms),
    /// each needing its OWN red-prove against a byte-dribbling peer (the
    /// existing silent-peer fixtures don't exercise this — a silent peer
    /// never reaches the ordering question at all, since IT the READ
    /// itself bounds in ~1s either way). Left as-is for this fix pass; the
    /// pre-existing class (`stop()` already has it) is not made materially
    /// worse by this change, and the reorder is real follow-up work, not
    /// a fold-in.
    fn remove_presence_key(&mut self) {
        if self.beat_removed {
            return;
        }
        self.beat_removed = true;
        // (#647) This is the CLEAN-stop path's framing: the dispatch is about
        // to emit its `dispatch.complete`/`dispatch.error`, which is this
        // session's authoritative close-edge. Pre-claim `session-end:<sid>`
        // BEFORE removing the key so the presence reconciler, when it
        // observes the key gone, LOSES the claim and skips its `session.end`
        // edge (which would be redundant with the terminal record). Read
        // from `Drop` too (#2344): a caught panic still reaches its
        // `dispatch.error` via the dispatch's own bookend guard, so the same
        // "a terminal record already covers this" reasoning applies there.
        // Only a killed host process (no Drop at all) never pre-claims — and
        // the reconciler then wins + records the close, which is exactly the
        // interval bracket playback would otherwise lack.
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
            // blocks instead, and this runs microseconds before the terminal
            // record is emitted.
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
        // and the explicit stop), halt the refresh thread AND remove the
        // presence key (#2344) — a caught panic already emits a
        // `dispatch.error` terminal via the dispatch's own bookend guard
        // (see `DispatchBookendGuard`), so leaving the beat to age out via
        // its up-to-15s TTL made a just-failed session render as "running"
        // for up to 15s past its own terminal record. `remove_presence_key`
        // is bounded (see its doc) — this is the same already-bounded
        // network call `stop()` always made, just also reached here.
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.remove_presence_key();
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
        beat_removed: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (#2344) A minimal, in-process, RESP-speaking fake Redis peer that
    /// actually stores and answers `SET`/`GET`/`DEL` — unlike
    /// [`crate::spawn_silent_redis_peer`] (#2227), which exists purely to
    /// prove a bound and deliberately never answers a real command, so none
    /// of the (#2227) tests in this file can tell "the key still exists"
    /// from "the key never existed". That is exactly the question the
    /// (#2344) tests below need answered.
    ///
    /// Deliberately has **no expiry clock**: a key set here lives until an
    /// explicit `DEL`, which is what lets a test below prove Drop issues
    /// the DEL itself rather than merely waiting out a TTL — against a real
    /// Redis the same assertion would need to sleep past
    /// [`DEFAULT_TTL_SECS`] to even distinguish the two, which is both slow
    /// and, worse, passes for the wrong reason if the TTL alone did the
    /// work. Lives here (a unit test, not `tests/e2e_*`) so it runs on
    /// every `cargo test -p darkmux-flow` — the `fleet-e2e` CI job that
    /// installs a real `redis-server` only runs `tests/e2e_*` binaries, so
    /// a test needing this level of proof would otherwise never execute in
    /// CI at all (the fate of this file's own `#[ignore]`d
    /// `session_roundtrip_against_live_redis`).
    ///
    /// Protocol coverage is the minimum this module's production code
    /// issues: the two `CLIENT SETINFO` calls redis-rs's connection setup
    /// sends (answered `+OK`, same as `spawn_silent_redis_peer`), then RESP
    /// arrays for `PING`/`SET`/`GET`/`DEL`. `NX` is honored (a second
    /// `SET ... NX` against an existing key replies nil), matching
    /// `claim_edge`'s real dependency on that semantics; `EX <ttl>` is
    /// parsed (to stay a valid command) and otherwise ignored.
    mod fake_redis {
        use std::collections::HashMap;
        use std::io::{BufRead, BufReader, Write};
        use std::net::{TcpListener, TcpStream};
        use std::sync::{Arc, Mutex};

        pub struct FakeRedis {
            port: u16,
            store: Arc<Mutex<HashMap<String, String>>>,
            /// (#2344 review, MUST FIX 3) Log of every `(cmd, key)` this peer
            /// has answered, in arrival order. State alone (`contains`) can't
            /// distinguish "`remove_presence_key` ran once" from "it ran
            /// twice and both times were idempotent no-ops" — a `SET ... NX`
            /// against an already-claimed key and a `DEL` of an
            /// already-absent key both leave the store looking identical
            /// either way. Counting calls is what actually proves the
            /// `beat_removed` guard is doing something: without it,
            /// `SessionEmitter::stop()` (which always triggers its own
            /// subsequent `Drop`) issues the claim `SET` and the beat `DEL`
            /// TWICE instead of once.
            calls: Arc<Mutex<Vec<(String, String)>>>,
        }

        impl FakeRedis {
            pub fn spawn() -> Self {
                let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
                let port = listener.local_addr().unwrap().port();
                let store: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
                let calls: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
                let accept_store = Arc::clone(&store);
                let accept_calls = Arc::clone(&calls);
                std::thread::spawn(move || {
                    for stream in listener.incoming() {
                        let Ok(stream) = stream else { continue };
                        let conn_store = Arc::clone(&accept_store);
                        let conn_calls = Arc::clone(&accept_calls);
                        std::thread::spawn(move || handle_conn(stream, conn_store, conn_calls));
                    }
                });
                Self { port, store, calls }
            }

            pub fn url(&self) -> String {
                format!("redis://127.0.0.1:{}", self.port)
            }

            pub fn contains(&self, key: &str) -> bool {
                self.store.lock().unwrap().contains_key(key)
            }

            /// How many times this peer has answered `cmd` (e.g. `"SET"` /
            /// `"DEL"`) addressed at `key`. See the `calls` field doc for why
            /// this — not `contains` — is the assertion that actually proves
            /// teardown ran exactly once.
            pub fn call_count(&self, cmd: &str, key: &str) -> usize {
                self.calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(c, k)| c.eq_ignore_ascii_case(cmd) && k == key)
                    .count()
            }
        }

        fn handle_conn(
            stream: TcpStream,
            store: Arc<Mutex<HashMap<String, String>>>,
            calls: Arc<Mutex<Vec<(String, String)>>>,
        ) {
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut writer = stream;
            loop {
                let Some(args) = read_command(&mut reader) else { return };
                if args.is_empty() {
                    continue;
                }
                let cmd = args[0].to_ascii_uppercase();
                // (#2344 review, MUST FIX 3) Log every SET/DEL call BY KEY,
                // regardless of outcome (an NX-miss SET and a no-op DEL of an
                // already-absent key still count as a call) — this is what
                // lets a test tell "teardown ran once" from "it ran twice
                // and both times happened to be idempotent no-ops".
                if cmd == "SET" || cmd == "DEL" {
                    if let Some(key) = args.get(1) {
                        calls.lock().unwrap().push((cmd.clone(), key.clone()));
                    }
                }
                let reply = match cmd.as_str() {
                    "CLIENT" | "HELLO" => "+OK\r\n".to_string(),
                    "PING" => "+PONG\r\n".to_string(),
                    "SET" => {
                        let key = args.get(1).cloned().unwrap_or_default();
                        let value = args.get(2).cloned().unwrap_or_default();
                        let nx = args[3..].iter().any(|a| a.eq_ignore_ascii_case("NX"));
                        let mut store = store.lock().unwrap();
                        if nx && store.contains_key(&key) {
                            "$-1\r\n".to_string()
                        } else {
                            store.insert(key, value);
                            "+OK\r\n".to_string()
                        }
                    }
                    "GET" => {
                        let key = args.get(1).cloned().unwrap_or_default();
                        match store.lock().unwrap().get(&key) {
                            Some(v) => format!("${}\r\n{}\r\n", v.len(), v),
                            None => "$-1\r\n".to_string(),
                        }
                    }
                    "DEL" => {
                        let mut removed = 0i64;
                        let mut store = store.lock().unwrap();
                        for key in &args[1..] {
                            if store.remove(key).is_some() {
                                removed += 1;
                            }
                        }
                        format!(":{removed}\r\n")
                    }
                    _ => "-ERR unsupported by the (#2344) fake_redis test peer\r\n".to_string(),
                };
                if writer.write_all(reply.as_bytes()).is_err() {
                    return;
                }
            }
        }

        /// Parse one RESP array-of-bulk-strings command off `reader`.
        /// `None` on EOF or a malformed frame (the redis-rs client this
        /// module's production code drives never sends anything else).
        fn read_command(reader: &mut impl BufRead) -> Option<Vec<String>> {
            let mut line = String::new();
            if reader.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let line = line.trim_end();
            let n: usize = line.strip_prefix('*')?.parse().ok()?;
            let mut args = Vec::with_capacity(n);
            for _ in 0..n {
                let mut len_line = String::new();
                reader.read_line(&mut len_line).ok()?;
                let len: usize = len_line.trim_end().strip_prefix('$')?.parse().ok()?;
                let mut buf = vec![0u8; len + 2]; // payload + trailing \r\n
                reader.read_exact(&mut buf).ok()?;
                args.push(String::from_utf8_lossy(&buf[..len]).into_owned());
            }
            Some(args)
        }
    }

    /// Poll `cond` until it's true or 2s elapse (well past one beat interval
    /// at test cadence — beats write immediately on thread start, not after
    /// the first sleep). Panics naming `what` on timeout so a broken
    /// assumption fails loudly instead of degenerating into "and then the
    /// key was never there, so the negative assertion vacuously passed".
    fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !cond() {
            if std::time::Instant::now() > deadline {
                panic!("timed out waiting for {what}");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// The reconciler's edge-claim key for a session's `session-end`
    /// transition — mirrors `presence_reconciler::EDGE_CLAIM_PREFIX` +
    /// `claim_edge`'s `"{kind}:{id}"` suffix, duplicated here (rather than
    /// exposed cross-module) because it's private to that module and this
    /// is test-only plumbing. (#2344 review, MUST FIX 2)
    fn edge_claim_key(sid: &str) -> String {
        format!("darkmux:edge-claim:session-end:{sid}")
    }

    /// (#2344) THE clean-path proof: `stop()` must remove the presence key
    /// immediately, not merely halt the beat thread and leave the key for
    /// the fake peer's (nonexistent) TTL to never actually clean up.
    ///
    /// Also covers two review findings (MUST FIX 2 / MUST FIX 3) that a bare
    /// `!fake.contains(&key)` can't distinguish: (a) the pre-claim actually
    /// ran — deleting `remove_presence_key`'s `claim_edge` call would still
    /// leave the beat key removed by the DEL alone, so the edge-claim key's
    /// presence is the only thing that proves the pre-claim happened; (b)
    /// teardown ran exactly ONCE — `stop(mut self)` always triggers its own
    /// subsequent `Drop` when `self` goes out of scope at the end of the
    /// function, so without the `beat_removed` guard `remove_presence_key`
    /// runs twice. Both the SET NX and the DEL are idempotent, so the final
    /// STATE (key gone, claim present) looks identical either way — only a
    /// call count can tell them apart.
    #[test]
    fn stop_deletes_the_presence_key_immediately() {
        let fake = fake_redis::FakeRedis::spawn();
        let client = redis::Client::open(fake.url().as_str()).expect("open fake client");
        let sid = "sid-2344-stop".to_string();
        let key = session_key(&sid);
        let claim_key = edge_claim_key(&sid);

        let emitter = spawn_with_client(client, sid, Some("coder".into()), None)
            .expect("spawn emitter");
        wait_until(|| fake.contains(&key), "the first beat to land");

        emitter.stop();

        assert!(
            !fake.contains(&key),
            "stop() must remove the presence key immediately, not leave it for a TTL that \
             this fake peer doesn't even implement"
        );
        assert!(
            fake.contains(&claim_key),
            "stop() must pre-claim the reconciler's session-end edge BEFORE removing the \
             presence key, so the reconciler loses the claim and skips its own redundant \
             session.end when it later observes the key gone (MUST FIX 2)"
        );
        assert_eq!(
            fake.call_count("DEL", &key),
            1,
            "remove_presence_key must run exactly once per stop() — the beat_removed guard \
             exists because stop(mut self) always triggers its own Drop when self goes out \
             of scope, and without the guard the DEL (and the claim SET) would fire twice \
             (MUST FIX 3)"
        );
    }

    /// (#2344) THE regression this issue is about, at the emitter level: an
    /// early `?`-return between spawn and the explicit `stop()` simply lets
    /// the emitter go out of scope. Before the fix, `Drop` only halted the
    /// beat thread — no DEL — so against this TTL-less fake peer the key
    /// would never be removed at all, which is exactly the bug: a session
    /// that ended without reaching `stop()` kept reading "running" with
    /// nothing left in the process that would ever say otherwise.
    #[test]
    fn drop_without_stop_removes_the_presence_key() {
        let fake = fake_redis::FakeRedis::spawn();
        let client = redis::Client::open(fake.url().as_str()).expect("open fake client");
        let sid = "sid-2344-early-return".to_string();
        let key = session_key(&sid);
        let claim_key = edge_claim_key(&sid);
        {
            let _emitter = spawn_with_client(client, sid, Some("coder".into()), None)
                .expect("spawn emitter");
            wait_until(|| fake.contains(&key), "the first beat to land");
            // Scope ends here with NO explicit `.stop()` call — simulating
            // a `?`-return between spawn and the dispatch's clean stop.
        }

        assert!(
            !fake.contains(&key),
            "Drop (reached via an early return, no explicit stop()) must remove the presence \
             key. Before #2344's fix, Drop only halted the beat thread, so a session that \
             ended this way kept a beat with nothing left in the process to ever remove it — \
             this fake peer has no TTL clock, so a key still present here can ONLY mean Drop \
             never issued the DEL."
        );
        assert!(
            fake.contains(&claim_key),
            "Drop must also pre-claim the reconciler's session-end edge, same as stop() — \
             otherwise the reconciler wins the (now-unclaimed) edge and records a redundant \
             session.end alongside this early-returning dispatch's own dispatch.error \
             terminal (MUST FIX 2)"
        );
        assert_eq!(
            fake.call_count("DEL", &key),
            1,
            "only Drop ran here (no explicit stop()), so remove_presence_key must have run \
             exactly once (MUST FIX 3)"
        );
    }

    /// (#2344) The panic path, covered explicitly per the RAII precedent:
    /// a panic unwinding through `SessionEmitter`'s scope must still remove
    /// the presence key via `Drop`. This is the scenario #2567's own PR
    /// description named as "known and not fixed" — a caught panic already
    /// gets a terminal `dispatch.error` via the dispatch's bookend guard,
    /// so leaving the beat behind made a just-failed dispatch read
    /// "running" and "errored" at once for however long the (real) TTL
    /// takes to lapse.
    #[test]
    fn drop_via_panic_removes_the_presence_key() {
        let fake = fake_redis::FakeRedis::spawn();
        let client = redis::Client::open(fake.url().as_str()).expect("open fake client");
        let sid = "sid-2344-panic".to_string();
        let key = session_key(&sid);
        let claim_key = edge_claim_key(&sid);

        // Silence the expected panic's backtrace so test output stays
        // clean — same convention as `dispatch_internal_tests`'s
        // `bookend_guard_fires_on_panic_unwind`.
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _emitter = spawn_with_client(client, sid, Some("coder".into()), None)
                .expect("spawn emitter");
            wait_until(|| fake.contains(&key), "the first beat to land");
            panic!("simulated mid-dispatch panic while the session emitter is in scope (#2344)");
        }));
        std::panic::set_hook(prev_hook);
        assert!(result.is_err(), "the closure should have panicked");

        assert!(
            !fake.contains(&key),
            "a panic unwinding through SessionEmitter's scope must remove the presence key via \
             Drop, matching the RAII precedent this module already uses for the beat thread \
             itself — leaving it behind means a just-panicked dispatch (whose bookend guard \
             already emitted its dispatch.error terminal) keeps reading \"running\" with \
             nothing left in the process to say otherwise."
        );
        assert!(
            fake.contains(&claim_key),
            "Drop-via-panic must also pre-claim the reconciler's session-end edge — the \
             dispatch's bookend guard already emitted a dispatch.error for this panic, so an \
             unclaimed edge would let the reconciler record a redundant session.end on top of \
             it once the (real) TTL lapsed (MUST FIX 2)"
        );
        assert_eq!(
            fake.call_count("DEL", &key),
            1,
            "only Drop ran here (the panic unwound before any explicit stop()), so \
             remove_presence_key must have run exactly once (MUST FIX 3)"
        );
    }

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

    /// (#2344 review, MUST FIX 3) The Drop-path twin of the test above: the
    /// (#2227) bounded-time proof existed for `stop()` only — but since
    /// #2344 `Drop` runs the exact same `remove_presence_key` teardown
    /// (pre-claim `SET NX` then `DEL`), reached from an early `?`-return or
    /// a caught panic instead of an explicit call. An unbounded Drop there
    /// would stall a panic unwind indefinitely; this proves it doesn't.
    #[test]
    fn drop_without_stop_against_silent_peer_returns_within_bounded_time() {
        let port = crate::spawn_silent_redis_peer(8);
        crate::assert_silent_peer_reaches_command_phase(port);
        let client = redis::Client::open(format!("redis://127.0.0.1:{port}").as_str())
            .expect("open client against the fake peer");

        let start = std::time::Instant::now();
        {
            let _emitter = spawn_with_client(
                client,
                "sid-2227-drop-teardown".to_string(),
                Some("coder".to_string()),
                None,
            )
            .expect("spawn emitter");

            // Let the beat thread get INSIDE the SET before tearing down —
            // same setup as the stop() sibling above, and the state an early
            // `?`-return or a caught panic would actually unwind from.
            std::thread::sleep(std::time::Duration::from_millis(250));
            // Scope ends here with NO explicit `.stop()` — Drop alone must
            // run the same bounded teardown `stop()` does.
        }
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "SessionEmitter::drop() (no explicit stop()) took {elapsed:?} against a \
             command-silent peer; expected bounded the same way stop() is (~3.1s measured), \
             since remove_presence_key issues the same three bounded commands from either \
             path. An unbounded Drop would stall a panic unwind or an early ?-return \
             indefinitely instead of letting the dispatch's own terminal record land."
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
