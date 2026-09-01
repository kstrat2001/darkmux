//! (#638 + #640) Fleet presence substrate, keyed on stable hardware identity.
//!
//! A live machine's `darkmux serve` daemon refreshes a short-TTL Redis key
//! every few seconds. The live set is *"which `darkmux:presence:*` keys
//! currently exist"* — Redis TTL does stale-removal for free: no timeout
//! logic to write, and no cross-machine clock-skew problem, because Redis's
//! own clock governs expiry rather than each reader comparing timestamps.
//!
//! The key is the machine's stable **hardware identity** (`machine_uid`,
//! #640) — NOT the operator-set name. The mutable label rides in the payload
//! as `display_name`. So a machine is one presence entry regardless of what
//! it's named, and a machine that can't resolve a hardware uid is honestly
//! *unidentifiable* (presence is disabled for it) rather than masquerading
//! under an unprovable name.
//!
//! Presence is **ephemeral** and deliberately separate from the durable flow
//! stream: heartbeats are NOT flow records. High-frequency presence noise on
//! the flow stream would pollute the durable work/audit substrate and
//! re-conflate "is this machine here" with "what did it do".

use crate::{bound_redis_response, open_redis_connection_bounded, REDIS_CONNECT_TIMEOUT};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Redis key namespace for presence beats — one key per live machine,
/// `darkmux:presence:<machine_uid>`. Matches darkmux's `darkmux:` namespace
/// convention for state it writes into shared systems.
const PRESENCE_KEY_PREFIX: &str = "darkmux:presence:";

/// Heartbeat cadence default: publish every 5s. A live machine survives a
/// dropped beat or two before its key expires (see [`DEFAULT_TTL_SECS`]), so
/// a packet-loss blip doesn't flap it offline.
pub const DEFAULT_BEAT_INTERVAL_SECS: u64 = 5;

/// Presence-key TTL default: 15s (≈ 3 missed beats at the 5s cadence). The
/// beat interval and the TTL are independent knobs — Redis `EX` handles
/// expiry, so the reader never times anything out itself.
pub const DEFAULT_TTL_SECS: u64 = 15;

/// What a live machine publishes each heartbeat. Kept small (refreshed every
/// few seconds). The identity key is `machine_uid` (stable hardware id);
/// `display_name` is the mutable operator label. `specs` and `loaded_models`
/// are best-effort enrichment for the machine cards, omitted from the wire
/// when empty (forward-compatible — a later phase populates them without a
/// format change).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PresenceBeat {
    /// Stable hardware identity (`darkmux_hardware::machine_uid`) — the key
    /// suffix and the join key for this machine's flow records. Immutable.
    pub machine_uid: String,
    /// Mutable operator label (`resolve_machine_id`: `DARKMUX_MACHINE_ID` else
    /// hostname). Shown in the UI; never used as identity.
    pub display_name: String,
    /// `FLOW_SCHEMA_VERSION` this daemon's binary writes — the *live* version
    /// signal the skew check keys on, instead of scanning stream contents.
    pub schema_version: String,
    /// Unix-ms at beat-write time. Diagnostic / "last beat" display only —
    /// liveness is governed by Redis key existence (TTL), NOT by comparing
    /// this against the reader's clock.
    pub beat_ts_ms: u64,
    /// One-line machine summary (chip · ram · cores), best-effort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub specs: Option<String>,
    /// LMStudio loaded-model ids, best-effort (may be empty / omitted).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loaded_models: Vec<String>,
    /// (#1580) The darkmux version this daemon's binary is. `None` from a peer
    /// running a build that predates this field.
    ///
    /// It lives HERE, on the heartbeat, and not only on `machine list --deep`,
    /// because presence needs no HTTP reachability — it travels the shared
    /// Redis. The failure that motivated #1580 is exactly the case where the
    /// deep probe cannot answer: a peer whose daemon is up and heartbeating
    /// but whose HTTP surface is unreachable. `schema_version` above already
    /// proved the pattern works; the darkmux version is the other half of the
    /// same question and was the half nobody could get.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub darkmux_version: Option<String>,
}

/// The Redis key for a machine's presence beat (keyed on the stable uid).
pub fn presence_key(machine_uid: &str) -> String {
    format!("{PRESENCE_KEY_PREFIX}{machine_uid}")
}

/// Publish/refresh this machine's presence beat with `ttl_secs` expiry
/// (`SET darkmux:presence:<uid> <json> EX <ttl>`). Errors propagate so the
/// emitter can log + carry on (best-effort — a Redis blip must never crash
/// the daemon).
pub fn write_beat(client: &redis::Client, beat: &PresenceBeat, ttl_secs: u64) -> Result<()> {
    let payload = serde_json::to_string(beat).context("serializing presence beat")?;
    let mut conn = open_redis_connection_bounded(client, REDIS_CONNECT_TIMEOUT)
        .context("getting Redis connection for presence write")?;
    // (#2227) The connect above is bounded; this `SET` was not. Same shape as
    // the session-beat write: an accepts-but-never-answers peer blocks the
    // daemon's presence thread here indefinitely, and "errors propagate so the
    // emitter can log + carry on" only holds if the call can RETURN at all.
    bound_redis_response(&conn);
    // Bind the reply to `redis::Value` (identity `FromRedisValue`) so the
    // `SET` `+OK` status parses regardless of reply shape; errors still
    // propagate.
    let _: redis::Value = redis::cmd("SET")
        .arg(presence_key(&beat.machine_uid))
        .arg(payload)
        .arg("EX")
        .arg(ttl_secs)
        .query(&mut conn)
        .context("SET presence beat")?;
    Ok(())
}

/// Read the currently-live machines — every unexpired `darkmux:presence:*`
/// key, parsed back into [`PresenceBeat`]. Order is unspecified. A malformed
/// payload is skipped (best-effort), not fatal. Uses cursor-based `SCAN`
/// (non-blocking) rather than `KEYS`.
pub fn read_live(client: &redis::Client) -> Result<Vec<PresenceBeat>> {
    let mut conn = open_redis_connection_bounded(client, REDIS_CONNECT_TIMEOUT)
        .context("getting Redis connection for presence read")?;
    // (#2227) Bounds each `SCAN`/`GET` below individually (a per-socket-read
    // deadline, not a budget for the whole loop). `SCAN COUNT 200` is
    // explicitly non-blocking, so no single reply legitimately takes a second.
    // This read backs the machine topology view and the reconciler's tick — an
    // unbounded stall here wedges the reconcile loop, not just one request.
    bound_redis_response(&conn);
    let pattern = format!("{PRESENCE_KEY_PREFIX}*");
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
            .context("SCAN presence keys")?;
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
            .context("GET presence key")?;
        if let Some(json) = val {
            if let Ok(beat) = serde_json::from_str::<PresenceBeat>(&json) {
                out.push(beat);
            }
        }
    }
    Ok(out)
}

/// Unix-ms wall-clock helper for stamping a beat. Saturates to 0 before the
/// epoch (never in practice).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Spawn the presence-emitter loop on a dedicated OS thread (the sync redis
/// client blocks; a `std::thread` keeps it off the tokio executor — same
/// shape as `darkmux_fleet::spawn_runner_thread`). **Self-disables** (returns
/// `None`) when either gate is unmet:
///
/// - `DARKMUX_REDIS_URL` unset → no shared substrate to be present in.
/// - no stable hardware uid (`machine_uid` is `None`) → the machine can't be
///   identified, so it must NOT publish under an unprovable name (#640).
///
/// The thread runs for the process lifetime; a Redis blip is logged
/// once-per-transition and retried — it never crashes the daemon.
pub fn spawn_emitter_thread() -> Option<std::thread::JoinHandle<()>> {
    // env(DARKMUX_REDIS_URL) > config-assembled (#661 Slice 5). `RawRedisUrl`
    // moves into the thread; `expose_for_probe()` at the client open below.
    let url = crate::redis_url()?;
    let machine_uid = match darkmux_hardware::machine_uid() {
        Some(uid) => uid.to_string(),
        None => {
            eprintln!(
                "{}",
                darkmux_types::style::warn(
                    "presence: no stable machine UID (IOPlatformUUID unavailable); \
                     presence disabled — this machine can't be identified, and it \
                     won't masquerade under a name (#640)"
                )
            );
            return None;
        }
    };
    // The display label — mutable, never identity. Defaults to the hostname
    // when DARKMUX_MACHINE_ID is unset.
    let display_name = crate::resolve_machine_id().unwrap_or_else(|| "unknown".to_string());
    let schema_version = crate::FLOW_SCHEMA_VERSION.to_string();

    let spawned = std::thread::Builder::new()
        .name("darkmux-presence".to_string())
        .spawn(move || {
            let client = match redis::Client::open(url.expose_for_probe()) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("{}", darkmux_types::style::warn(&format!("presence: could not open Redis client ({e}); presence disabled")));
                    return;
                }
            };
            eprintln!(
                "presence: heartbeat emitter started — uid={machine_uid} \
                 display={display_name} schema={schema_version} \
                 (every {DEFAULT_BEAT_INTERVAL_SECS}s, TTL {DEFAULT_TTL_SECS}s)"
            );
            // Log only on health TRANSITIONS (publishing↔failing), never
            // per-iteration — a persistently-down Redis must not spam the
            // daemon log every cadence tick.
            let mut healthy: Option<bool> = None;
            loop {
                let beat = PresenceBeat {
                    machine_uid: machine_uid.clone(),
                    display_name: display_name.clone(),
                    schema_version: schema_version.clone(),
                    beat_ts_ms: now_ms(),
                    specs: None,
                    loaded_models: Vec::new(),
                    darkmux_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                };
                match write_beat(&client, &beat, DEFAULT_TTL_SECS) {
                    Ok(()) => {
                        if healthy != Some(true) {
                            eprintln!(
                                "presence: heartbeat publishing as `{}`",
                                presence_key(&machine_uid)
                            );
                            healthy = Some(true);
                        }
                    }
                    Err(e) => {
                        if healthy != Some(false) {
                            eprintln!(
                                "{}",
                                darkmux_types::style::warn(&format!(
                                    "presence: heartbeat write failing (retrying every \
                                     {DEFAULT_BEAT_INTERVAL_SECS}s): {e}"
                                ))
                            );
                            healthy = Some(false);
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(DEFAULT_BEAT_INTERVAL_SECS));
            }
        });

    match spawned {
        Ok(handle) => Some(handle),
        Err(e) => {
            eprintln!("{}", darkmux_types::style::warn(&format!("presence: could not spawn emitter thread ({e}); presence disabled")));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (#2227) Per-site bounds for the machine-presence pair. Both run on the
    /// daemon's presence thread, which the reconcile loop depends on — an
    /// unbounded command there stops machine liveness fleet-wide, not just one
    /// call. Each half is timed separately so removing EITHER bound fails its
    /// own assertion rather than hiding behind the other's slack.
    #[test]
    fn presence_write_and_read_against_silent_peer_err_within_bounded_time() {
        let port = crate::spawn_silent_redis_peer(4);
        let client = redis::Client::open(format!("redis://127.0.0.1:{port}").as_str()).unwrap();

        let start = std::time::Instant::now();
        let write_res = write_beat(&client, &sample_beat(), DEFAULT_TTL_SECS);
        let write_elapsed = start.elapsed();
        let write_err = write_res.expect_err("a command-silent peer must surface as Err");
        let write_msg = format!("{write_err:#}");
        // COMMAND-phase, not connect-phase — otherwise vacuous against #278.
        assert!(
            write_msg.contains("SET presence beat"),
            "expected a SET-phase error (the connect must have SUCCEEDED); got {write_msg}"
        );
        assert!(
            write_elapsed < std::time::Duration::from_secs(3),
            "write_beat took {write_elapsed:?}; expected bounded by \
             REDIS_RESPONSE_TIMEOUT (1s) + connect. Unbounded before #2227."
        );

        let start = std::time::Instant::now();
        let read_res = read_live(&client);
        let read_elapsed = start.elapsed();
        let read_err = read_res.expect_err("a command-silent peer must surface as Err");
        let read_msg = format!("{read_err:#}");
        assert!(
            read_msg.contains("SCAN presence keys"),
            "expected a SCAN-phase error (the connect must have SUCCEEDED); got {read_msg}"
        );
        assert!(
            read_elapsed < std::time::Duration::from_secs(3),
            "read_live took {read_elapsed:?}; expected bounded by \
             REDIS_RESPONSE_TIMEOUT (1s) + connect. Unbounded before #2227."
        );
    }

    fn sample_beat() -> PresenceBeat {
        PresenceBeat {
            machine_uid: "564D1234-ABCD-5678-9EF0-1234567890AB".into(),
            display_name: "laptop".into(),
            schema_version: "1.10.0".into(),
            beat_ts_ms: 1_780_000_000_000,
            specs: Some("Apple Silicon · 128 GB".into()),
            loaded_models: vec!["qwen3.6-35b".into()],
            darkmux_version: Some("2.8.0".into()),
        }
    }

    /// (#1580) The whole point of putting the version HERE: a beat carries it
    /// over Redis, so a peer's darkmux version is readable without its HTTP
    /// daemon being reachable. That is precisely the case `machine list
    /// --deep` cannot serve, and the case that motivated the issue — a hub
    /// that was up and heartbeating while nothing could ask it anything.
    #[test]
    fn a_beat_carries_the_darkmux_version_over_the_wire() {
        let json = serde_json::to_string(&sample_beat()).unwrap();
        assert!(json.contains("darkmux_version"), "field must reach the wire: {json}");
        let back: PresenceBeat = serde_json::from_str(&json).unwrap();
        assert_eq!(back.darkmux_version.as_deref(), Some("2.8.0"));
    }

    /// A peer on a build that predates this field still parses — the reader
    /// must degrade to "unknown", never fail. Same leniency `specs` and
    /// `loaded_models` already have.
    #[test]
    fn a_beat_from_an_older_peer_without_the_field_still_parses() {
        let old = r#"{"machine_uid":"U","display_name":"studio","schema_version":"1.19.0","beat_ts_ms":1}"#;
        let back: PresenceBeat = serde_json::from_str(old).expect("older beat must still parse");
        assert_eq!(back.darkmux_version, None, "absent means unknown, not an error");
        assert_eq!(back.display_name, "studio");
    }

    /// The field is omitted rather than serialized as `null` when unset, so an
    /// older reader sees the same bytes it always did.
    #[test]
    fn an_unset_version_is_omitted_from_the_wire_entirely() {
        let mut b = sample_beat();
        b.darkmux_version = None;
        let json = serde_json::to_string(&b).unwrap();
        assert!(!json.contains("darkmux_version"), "must be omitted, not null: {json}");
    }

    #[test]
    fn presence_key_is_namespaced_on_uid() {
        assert_eq!(
            presence_key("564D1234-ABCD-5678-9EF0-1234567890AB"),
            "darkmux:presence:564D1234-ABCD-5678-9EF0-1234567890AB"
        );
    }

    #[test]
    fn beat_round_trips_through_json() {
        let beat = sample_beat();
        let json = serde_json::to_string(&beat).unwrap();
        let back: PresenceBeat = serde_json::from_str(&json).unwrap();
        assert_eq!(beat, back);
    }

    #[test]
    fn optional_fields_omitted_when_empty_and_default_back() {
        let beat = PresenceBeat {
            machine_uid: "UID-2".into(),
            display_name: "mini".into(),
            schema_version: "1.10.0".into(),
            beat_ts_ms: 1,
            specs: None,
            darkmux_version: None,
            loaded_models: vec![],
        };
        let json = serde_json::to_string(&beat).unwrap();
        assert!(!json.contains("specs"), "None specs should be omitted: {json}");
        assert!(!json.contains("loaded_models"), "empty loaded_models should be omitted: {json}");
        let back: PresenceBeat = serde_json::from_str(&json).unwrap();
        assert_eq!(beat, back);
    }

    #[test]
    fn minimal_wire_payload_parses() {
        // A beat that only carries the load-bearing fields (what the emitter
        // publishes) must parse — proving the enrichment fields are optional.
        let json = r#"{"machine_uid":"UID-3","display_name":"studio","schema_version":"1.10.0","beat_ts_ms":42}"#;
        let beat: PresenceBeat = serde_json::from_str(json).unwrap();
        assert_eq!(beat.machine_uid, "UID-3");
        assert_eq!(beat.display_name, "studio");
        assert_eq!(beat.specs, None);
        assert!(beat.loaded_models.is_empty());
    }

    /// On-demand integration check against a live Redis. `#[ignore]` so CI
    /// without Redis skips it; run with
    /// `cargo test -p darkmux-flow presence_roundtrip -- --ignored` while
    /// `DARKMUX_REDIS_URL` points at a reachable Redis. Writes a uniquely-
    /// named self-test beat, confirms `read_live` surfaces it, then DELetes
    /// the key so it never lingers as a phantom machine.
    #[test]
    #[ignore]
    fn presence_roundtrip_against_live_redis() {
        let Some(url) = std::env::var("DARKMUX_REDIS_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
        else {
            eprintln!("DARKMUX_REDIS_URL unset — skipping live presence round-trip");
            return;
        };
        let client = redis::Client::open(url.as_str()).expect("open redis client");
        let uid = format!("presence-selftest-{}", std::process::id());
        let beat = PresenceBeat {
            machine_uid: uid.clone(),
            display_name: "selftest".into(),
            schema_version: crate::FLOW_SCHEMA_VERSION.to_string(),
            beat_ts_ms: now_ms(),
            specs: None,
            darkmux_version: None,
            loaded_models: Vec::new(),
        };
        write_beat(&client, &beat, DEFAULT_TTL_SECS).expect("write_beat");
        let live = read_live(&client).expect("read_live");
        let found = live.iter().find(|b| b.machine_uid == uid).cloned();
        // Clean up BEFORE asserting so a failure can't leak the key.
        let mut conn = open_redis_connection_bounded(&client, REDIS_CONNECT_TIMEOUT).unwrap();
        let _: redis::Value = redis::cmd("DEL")
            .arg(presence_key(&uid))
            .query(&mut conn)
            .unwrap();
        assert_eq!(
            found.as_ref().map(|b| b.machine_uid.as_str()),
            Some(uid.as_str()),
            "self-test beat should appear in read_live"
        );
        assert_eq!(found.unwrap(), beat, "round-tripped beat should match what was written");
    }
}
