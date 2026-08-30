//! (#2110 governor, #2109 breaker) Host-side thermal pacing.
//!
//! Fed one OS thermal reading (`host_probe::ThermalSample`) per tick from
//! the dispatch's per-dispatch host sampler (`dispatch_internal.rs`'s
//! `run_telemetry_sampler`, which already reads `probe.sample().thermal`
//! every `TELEMETRY_SAMPLE_INTERVAL`), this module decides whether the
//! in-flight dispatch should rest and, if the machine never cools, whether
//! the mission should stop dispatching further units.
//!
//! **Governor (#2110):** at or above `pause_at`, write the pace file
//! (`pause: true, reason: "thermal"`) — the in-flight unit rests at its
//! next turn boundary (`runtime/src/pace.rs`, #2114). Clears the pause once
//! the state has held at or below `resume_at`, continuously, for
//! `resume_hold_ms` (hysteresis — a state bouncing right at the threshold
//! must not flap the pace file). If one continuous pause episode runs past
//! `max_pause_ms` without recovering, hands off to the breaker instead of
//! resting forever.
//!
//! **Breaker (#2109):** at `critical`, or when `cpu_speed_limit_pct` drops
//! below `min_cpu_speed_limit_pct`, write the pace file with
//! `reason: "thermal-critical", expires: false` and — for a crawl mission —
//! drop the crawl's `STOP` file so no further unit gets dispatched. **Never
//! kills the container.** The in-flight unit pauses at its next turn
//! boundary with its checkpoint persisted (#2114); resume is the operator's
//! call (`darkmux dispatch --resume`). Once tripped, the governor goes
//! terminal — it does not un-pause itself on recovery, and (per finding 2
//! of the #2110/#2109 review) it does not need to keep re-stamping
//! `written_at_ms` either: `expires: false` tells the runtime's staleness
//! ceiling to never apply to this pause, so a stale timestamp can't make
//! the unit resume on a still-critical machine.
//!
//! **Pace file location (operator correction during #2110/#2109 review):**
//! NOT under the mounted `/workspace` — crawl units mount that read-only,
//! and a coder run's workspace IS the operator's own repo tree, the wrong
//! place for darkmux's own bookkeeping. It lives in the dispatch's
//! **out-dir** instead (`host_out`, mounted at `/darkmux-out`; see
//! `dispatch_internal.rs`'s `apply_volume_mounts`), same home as
//! `.prompt.txt` and the trajectory. Container path: `/darkmux-out/pace.json`.
//! The runtime-side reader (`runtime/src/pace.rs`) reads from there —
//! `pace_file_path_matches_runtime_out_base` below is the conformance test
//! that keeps the two literal join expressions in sync.
//!
//! Every write stamps `written_at_ms` (epoch millis) so the runtime's
//! staleness guard (`PaceFile::is_expired`, `runtime/src/pace.rs`) can
//! treat a pause older than `max_pause_ms` as expired — a host process
//! that dies mid-pause must not strand a dispatch resting forever on a
//! stale file. The ordinary governor pause (`reason: "thermal"`) is
//! subject to that ceiling; the breaker's `thermal-critical` stop opts out
//! via `expires: false` (above). A gap in OS thermal readings while paused
//! (`thermal_sample` returns `None` mid-episode) is treated as "time
//! passed, no new information" rather than frozen accounting or a stale
//! stamp — see `on_sample`'s `None` arm (finding 3 of the same review).

use crate::host_probe::thermal::THERMAL_STATES;
use crate::host_probe::ThermalSample;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Severity rank of a thermal state name. An unrecognized name (a future
/// macOS state this build doesn't know) ranks WORSE than `critical` —
/// mirrors `host_probe::mod::thermal_severity`'s reasoning: silently
/// treating an unknown state as mild would hide real thermal pressure.
fn severity(state: &str) -> usize {
    THERMAL_STATES.iter().position(|s| *s == state).unwrap_or(THERMAL_STATES.len())
}

/// `<host_out>/pace.json` — mounted into the container at
/// `/darkmux-out/pace.json`. MUST match the runtime-side join in
/// `runtime/src/pace.rs`'s `pace_file_path`
/// (`out_dir.join("pace.json")`) — `pace_file_path_matches_runtime_out_base`
/// below is the conformance test that keeps the two in sync.
pub fn pace_file_path(host_out: &Path) -> PathBuf {
    host_out.join("pace.json")
}

/// Shape written to the pace file. Mirrors `runtime/src/pace.rs`'s
/// `PaceFile` fields (`pause`/`reason`/`state`/`written_at_ms`/`expires`) —
/// the runtime-side reader tolerates unknown/extra fields on deserialize
/// (no `deny_unknown_fields`), so this stays forward-compatible with any
/// future additive field on that side. `expires` is omitted (not `null`)
/// when `None` — the ordinary governor pause doesn't opt out of the
/// staleness ceiling, so it writes the same shape a pre-`expires` reader
/// already tolerated.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct GovernorPaceFile {
    pause: bool,
    reason: &'static str,
    state: String,
    written_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires: Option<bool>,
}

fn now_epoch_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// `expires: Some(false)` for the breaker's `thermal-critical` stop (opts
/// out of the runtime's `max_pause_ms` staleness ceiling — finding 2 of
/// the #2110/#2109 review); `None` for every ordinary governor pause/resume
/// write, which stays subject to that ceiling.
fn write_pace_file(host_out: &Path, pause: bool, reason: &'static str, state: &str, expires: Option<bool>) {
    let pace = GovernorPaceFile {
        pause,
        reason,
        state: state.to_string(),
        written_at_ms: now_epoch_ms(),
        expires,
    };
    // Best-effort — a failed write is observability/pacing, never fatal to
    // the dispatch itself (mirrors every other sampler-adjacent write in
    // `dispatch_internal.rs`).
    let _ = std::fs::create_dir_all(host_out);
    if let Ok(json) = serde_json::to_string(&pace) {
        let _ = std::fs::write(pace_file_path(host_out), json);
    }
}

/// (#2109) Best-effort derivation of a crawl mission's `STOP` file path
/// from the dispatch's `record_context` (`src/crawl_launch.rs`'s per-unit
/// `record_context`, carrying `workspace` = the crawl manifest name, plus
/// `unit`/`rule` as crawl-specific markers). The breaker needs to write the
/// SAME path the crawl launcher's per-unit loop checks
/// (`<crawl_root>/STOP`) for "no further unit dispatches" to actually
/// hold — but this module must not depend on or edit `crawl_launch.rs`
/// (another agent owns that file for #2131 concurrently), so the formula
/// is duplicated here from that launcher's DEFAULT (no `root:` override)
/// resolution: `<darkmux root>/crawl/<manifest_name>/STOP`.
///
/// **Known gap, documented rather than guessed at:** a crawl spec with an
/// explicit `root:` override reuses `materialized.root` instead (see
/// `WorkspaceSpec::resolved_root()`) and is NOT reconstructable from
/// `record_context` alone. Widen if a consumer needs it — same "not yet,
/// but named" shape as #1352's other documented narrowings.
///
/// Only fires when `record_context` carries BOTH `workspace` (the manifest
/// name) and `unit` — the crawl launcher's own vocabulary — so a non-crawl
/// dispatch that happens to set `record_context` for its own reasons never
/// gets a spurious `STOP` file written under it.
pub fn stop_file_path_from_record_context(
    record_context: Option<&serde_json::Value>,
) -> Option<PathBuf> {
    let ctx = record_context?.as_object()?;
    if !ctx.contains_key("unit") {
        return None;
    }
    let manifest_name = ctx.get("workspace")?.as_str()?;
    if manifest_name.trim().is_empty() {
        return None;
    }
    let root = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::Auto).root;
    Some(root.join("crawl").join(manifest_name).join("STOP"))
}

fn write_stop_file(stop_file: &Path) {
    if let Some(parent) = stop_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(stop_file, b"thermal-critical\n");
}

/// One state change the governor made this tick — the caller (the sampler
/// loop in `dispatch_internal.rs`) turns each into a `dispatch.rest`-family
/// flow record so a slowed or stopped run is attributable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThermalEvent {
    /// Paused for thermal pacing (`reason: "thermal"`).
    Paused { state: String },
    /// Resumed after the hysteresis hold (`reason: "thermal"`, `pause: false`).
    Resumed { state: String },
    /// Breaker tripped (`reason: "thermal-critical"`) — max pause exceeded,
    /// `critical` state, or the CPU speed-limit floor.
    Breaker { state: String },
}

/// Resolved thermal-governor tuning (`config_access::thermal_*`).
#[derive(Debug, Clone)]
pub struct ThermalGovernorConfig {
    pub enabled: bool,
    pub pause_at: String,
    pub resume_at: String,
    pub resume_hold_ms: u64,
    pub max_pause_ms: u64,
    pub min_cpu_speed_limit_pct: u64,
}

impl ThermalGovernorConfig {
    /// Resolve from the standard `env > config.json > default` precedence
    /// (`darkmux_types::config_access::thermal_*`).
    pub fn from_env() -> Self {
        Self {
            enabled: darkmux_types::config_access::thermal_enabled(),
            pause_at: darkmux_types::config_access::thermal_pause_at(),
            resume_at: darkmux_types::config_access::thermal_resume_at(),
            resume_hold_ms: darkmux_types::config_access::thermal_resume_hold_ms(),
            max_pause_ms: darkmux_types::config_access::thermal_max_pause_ms(),
            min_cpu_speed_limit_pct: darkmux_types::config_access::thermal_min_cpu_speed_limit_pct(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Paused,
    /// Terminal: the breaker tripped. The governor stops touching the pace
    /// file / STOP file for the rest of this dispatch's lifetime — resume
    /// is out-of-band (the operator's `darkmux dispatch --resume`).
    Broken,
}

/// Per-dispatch state machine. One instance lives for the dispatch's
/// sampler-thread lifetime (constructed alongside `run_telemetry_sampler`,
/// dropped when the sampler thread returns).
pub struct ThermalGovernor {
    config: ThermalGovernorConfig,
    state: State,
    /// ms accumulated in the CURRENT pause episode (resets to 0 on resume).
    pause_episode_ms: u64,
    /// ms the state has continuously held at/below `resume_at` while
    /// paused (resets to 0 the moment it rises back above `resume_at`).
    resume_hold_accum_ms: u64,
    /// (finding 3) Last thermal state name from a real `Some` sample —
    /// only ever set from `on_sample`'s `Some` arm, so it's populated by
    /// the time `Paused`/`Broken` is reachable (both require at least one
    /// prior `Some` to enter). Used to keep the pace file's `state` field
    /// and the breaker's `STOP`/event state meaningful on a tick where the
    /// OS thermal reading itself came back `None`.
    last_known_state: String,
}

impl ThermalGovernor {
    pub fn new(config: ThermalGovernorConfig) -> Self {
        Self {
            config,
            state: State::Idle,
            pause_episode_ms: 0,
            resume_hold_accum_ms: 0,
            last_known_state: String::new(),
        }
    }

    /// Feed one thermal sample. `elapsed_ms` is the wall time since the
    /// previous sample — the sampler's own cadence in production
    /// (`TELEMETRY_SAMPLE_INTERVAL`), injectable here so tests drive a
    /// scripted sequence without real sleeps. `host_out` is the dispatch's
    /// out-dir (pace file lives at `<host_out>/pace.json`); `stop_file`,
    /// when `Some`, is the crawl `STOP` path to drop on breaker (from
    /// [`stop_file_path_from_record_context`]) — `None` for a non-crawl
    /// dispatch, where there is no "further unit" concept to stop.
    ///
    /// Returns the event that fired this tick, if any (pause / resume /
    /// breaker are mutually exclusive per tick — at most one fires).
    pub fn on_sample(
        &mut self,
        thermal: Option<&ThermalSample>,
        elapsed_ms: u64,
        host_out: &Path,
        stop_file: Option<&Path>,
    ) -> Option<ThermalEvent> {
        if !self.config.enabled || self.state == State::Broken {
            // Broken is terminal and its pace file write already carries
            // `expires: false` (see the breaker writes below) — the
            // runtime's staleness ceiling never applies to it, so there's
            // nothing to keep fresh here (finding 2 of the #2110/#2109
            // review; superseded the earlier "re-stamp every N ticks" plan
            // once `expires` landed on the runtime side).
            return None;
        }

        // (finding 3) A missing OS thermal reading is "time passed, no new
        // information" — NOT evidence of recovery, and NOT nothing. Only
        // matters while actively `Paused`: accumulate the elapsed time into
        // the episode (so `max_pause_ms` escalation still fires on a
        // machine that stays hot through a reading gap), reset the resume
        // hold (a gap is not a continuous hold at/below `resume_at`), and
        // re-stamp the pace file with the last known state so a real
        // reading gap can't let `written_at_ms` go stale and trip the
        // runtime's staleness ceiling mid-pause. `Idle` has nothing to
        // accumulate; a `None` there is simply a no-op tick.
        let thermal = match thermal {
            Some(t) => {
                self.last_known_state = t.state.clone();
                t
            }
            None => {
                if self.state == State::Paused {
                    self.pause_episode_ms = self.pause_episode_ms.saturating_add(elapsed_ms);
                    self.resume_hold_accum_ms = 0;
                    write_pace_file(host_out, true, "thermal", &self.last_known_state, None);
                    if self.pause_episode_ms >= self.config.max_pause_ms {
                        self.state = State::Broken;
                        write_pace_file(
                            host_out,
                            true,
                            "thermal-critical",
                            &self.last_known_state,
                            Some(false),
                        );
                        if let Some(stop) = stop_file {
                            write_stop_file(stop);
                        }
                        return Some(ThermalEvent::Breaker {
                            state: self.last_known_state.clone(),
                        });
                    }
                }
                return None;
            }
        };

        let sev = severity(&thermal.state);
        let is_breaker_condition = sev >= severity("critical")
            || thermal.cpu_speed_limit_pct < self.config.min_cpu_speed_limit_pct;

        if is_breaker_condition {
            self.state = State::Broken;
            write_pace_file(host_out, true, "thermal-critical", &thermal.state, Some(false));
            if let Some(stop) = stop_file {
                write_stop_file(stop);
            }
            return Some(ThermalEvent::Breaker { state: thermal.state.clone() });
        }

        match self.state {
            State::Idle => {
                if sev >= severity(&self.config.pause_at) {
                    self.state = State::Paused;
                    self.pause_episode_ms = 0;
                    self.resume_hold_accum_ms = 0;
                    write_pace_file(host_out, true, "thermal", &thermal.state, None);
                    Some(ThermalEvent::Paused { state: thermal.state.clone() })
                } else {
                    None
                }
            }
            State::Paused => {
                self.pause_episode_ms = self.pause_episode_ms.saturating_add(elapsed_ms);
                if sev <= severity(&self.config.resume_at) {
                    self.resume_hold_accum_ms = self.resume_hold_accum_ms.saturating_add(elapsed_ms);
                } else {
                    // Still hot enough to matter — hysteresis resets: only
                    // a CONTINUOUS hold at/below resume_at counts, so a
                    // state that ticks back up must restart the clock,
                    // not just pause it. Without this reset a state
                    // bouncing right at the threshold (fair, serious,
                    // fair, serious, ...) would eventually cross
                    // `resume_hold_ms` on ACCUMULATED good ticks alone and
                    // clear the pause while the machine is still flapping
                    // hot — exactly the flapping this hysteresis exists to
                    // prevent.
                    self.resume_hold_accum_ms = 0;
                }
                if self.resume_hold_accum_ms >= self.config.resume_hold_ms {
                    self.state = State::Idle;
                    self.pause_episode_ms = 0;
                    self.resume_hold_accum_ms = 0;
                    write_pace_file(host_out, false, "thermal", &thermal.state, None);
                    return Some(ThermalEvent::Resumed { state: thermal.state.clone() });
                }
                if self.pause_episode_ms >= self.config.max_pause_ms {
                    self.state = State::Broken;
                    write_pace_file(host_out, true, "thermal-critical", &thermal.state, Some(false));
                    if let Some(stop) = stop_file {
                        write_stop_file(stop);
                    }
                    return Some(ThermalEvent::Breaker { state: thermal.state.clone() });
                }
                None
            }
            State::Broken => unreachable!("returned early above"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(state: &str, cpu_speed_limit_pct: u64) -> ThermalSample {
        ThermalSample { state: state.to_string(), cpu_speed_limit_pct }
    }

    fn cfg() -> ThermalGovernorConfig {
        ThermalGovernorConfig {
            enabled: true,
            pause_at: "serious".to_string(),
            resume_at: "fair".to_string(),
            resume_hold_ms: 60_000,
            max_pause_ms: 900_000,
            min_cpu_speed_limit_pct: 50,
        }
    }

    fn read_pace(host_out: &Path) -> serde_json::Value {
        let raw = std::fs::read_to_string(pace_file_path(host_out)).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn nominal_never_pauses() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        let ev = gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None);
        assert_eq!(ev, None);
        assert!(!pace_file_path(dir.path()).exists(), "no pace file until a pause fires");
    }

    #[test]
    fn serious_pauses_and_writes_pace_file_with_written_at_ms() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        let ev = gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        assert_eq!(ev, Some(ThermalEvent::Paused { state: "serious".to_string() }));
        let pace = read_pace(dir.path());
        assert_eq!(pace["pause"], serde_json::json!(true));
        assert_eq!(pace["reason"], serde_json::json!("thermal"));
        assert_eq!(pace["state"], serde_json::json!("serious"));
        assert!(pace["written_at_ms"].as_u64().unwrap() > 0, "written_at_ms must be stamped");
    }

    #[test]
    fn full_hysteresis_sequence_nominal_serious_fair_hold_nominal() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());

        // nominal: no-op
        assert_eq!(gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None), None);

        // serious: pauses
        assert_eq!(
            gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::Paused { state: "serious".to_string() })
        );

        // fair, held for 60s total in 2s ticks: no resume until the hold
        // completes. 29 ticks * 2000ms = 58000ms < 60000ms hold — still paused.
        for _ in 0..29 {
            assert_eq!(gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None), None);
        }
        let pace = read_pace(dir.path());
        assert_eq!(pace["pause"], serde_json::json!(true), "hold not yet complete");

        // 30th tick crosses 60000ms — resumes.
        assert_eq!(
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::Resumed { state: "fair".to_string() })
        );
        let pace = read_pace(dir.path());
        assert_eq!(pace["pause"], serde_json::json!(false));

        // nominal after resume: no-op, stays idle.
        assert_eq!(gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None), None);
    }

    #[test]
    fn a_tick_back_above_resume_at_resets_the_hold_no_flapping() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);

        // 58s of "fair" (just under the 60s hold)...
        for _ in 0..29 {
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
        }
        // ...then one tick back at "serious" — must reset the hold clock,
        // not just pause it.
        assert_eq!(gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None), None);

        // Even 58 more seconds of "fair" must NOT be enough to resume,
        // because the hold restarted at the "serious" tick above — this is
        // the assertion that fails red if the reset (`= 0`, not skip) is
        // removed, proving the hysteresis is load-bearing.
        for _ in 0..29 {
            assert_eq!(gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None), None);
        }
        assert_eq!(
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::Resumed { state: "fair".to_string() }),
            "resume only after a FRESH continuous 60s hold"
        );
    }

    #[test]
    fn serious_held_past_max_pause_trips_the_breaker() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("crawl-root").join("STOP");
        let mut cfg = cfg();
        cfg.max_pause_ms = 10_000;
        let mut gov = ThermalGovernor::new(cfg);

        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), Some(&stop));
        // 4 more ticks of "serious" = 8000ms more, total 8000ms < 10000ms.
        for _ in 0..4 {
            let ev = gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), Some(&stop));
            assert_ne!(ev, Some(ThermalEvent::Breaker { state: "serious".to_string() }));
        }
        assert!(!stop.exists(), "breaker must not fire before max_pause_ms elapses");

        // One more tick crosses 10000ms.
        let ev = gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), Some(&stop));
        assert_eq!(ev, Some(ThermalEvent::Breaker { state: "serious".to_string() }));
        assert!(stop.exists(), "breaker must drop the crawl STOP file");
        let pace = read_pace(dir.path());
        assert_eq!(pace["reason"], serde_json::json!("thermal-critical"));
        assert_eq!(
            pace["expires"],
            serde_json::json!(false),
            "(finding 2) breaker stop must opt out of the runtime's staleness ceiling — a stale \
             written_at_ms must not resume the unit on a still-critical machine"
        );

        // Terminal: further samples, even nominal, do nothing more.
        assert_eq!(gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None), None);
    }

    #[test]
    fn critical_trips_the_breaker_immediately_no_hold_needed() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let mut gov = ThermalGovernor::new(cfg());
        let ev = gov.on_sample(Some(&sample("critical", 100)), 2000, dir.path(), Some(&stop));
        assert_eq!(ev, Some(ThermalEvent::Breaker { state: "critical".to_string() }));
        assert!(stop.exists());
        let pace = read_pace(dir.path());
        assert_eq!(pace["expires"], serde_json::json!(false));
    }

    #[test]
    fn low_cpu_speed_limit_trips_the_breaker_even_at_nominal_state() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let mut gov = ThermalGovernor::new(cfg());
        // "nominal" state, but the CPU is throttled below the floor —
        // the breaker must fire on the speed-limit signal alone.
        let ev = gov.on_sample(Some(&sample("nominal", 30)), 2000, dir.path(), Some(&stop));
        assert_eq!(ev, Some(ThermalEvent::Breaker { state: "nominal".to_string() }));
        assert!(stop.exists());
    }

    #[test]
    fn ordinary_pause_and_resume_writes_omit_expires() {
        // The ordinary governor pause/resume stays subject to the runtime's
        // default staleness ceiling — `expires` must be ABSENT (not
        // `false`), matching every pre-`expires` pace file.
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        let pace = read_pace(dir.path());
        assert!(pace.get("expires").is_none(), "ordinary pause must not set expires: {pace}");
    }

    // ── finding 3: a missing OS thermal reading mid-pause ──

    #[test]
    fn none_reading_while_paused_accumulates_toward_max_pause_and_trips_the_breaker() {
        // (#2140 review finding 3) `let thermal = thermal?;` used to bail
        // out BEFORE touching any accounting on a `None` sample, freezing
        // `pause_episode_ms` for as long as OS thermal readings kept
        // coming back empty — so a machine that stayed hot through a
        // reading gap could pause forever without ever escalating to the
        // breaker. This proves the breaker fires from None-tick elapsed
        // time ALONE, with no intervening real reading.
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let mut cfg = cfg();
        cfg.max_pause_ms = 10_000;
        let mut gov = ThermalGovernor::new(cfg);

        // Seed with one real "serious" reading — enters Paused.
        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), Some(&stop));

        // 4 ticks of a MISSING reading = 8000ms more elapsed, total 8000ms
        // — not yet at the 10000ms ceiling.
        for _ in 0..4 {
            let ev = gov.on_sample(None, 2000, dir.path(), Some(&stop));
            assert_eq!(ev, None, "not yet at max_pause_ms");
        }
        let pace = read_pace(dir.path());
        assert_eq!(pace["pause"], serde_json::json!(true), "still paused through the reading gap");
        assert_eq!(pace["state"], serde_json::json!("serious"), "state carries the last known reading");
        assert!(
            pace["written_at_ms"].as_u64().unwrap() > 0,
            "stamp still refreshed on a None tick, not frozen"
        );
        assert!(!stop.exists());

        // One more None tick crosses 10000ms — breaker fires without ever
        // seeing another real reading.
        let ev = gov.on_sample(None, 2000, dir.path(), Some(&stop));
        assert_eq!(ev, Some(ThermalEvent::Breaker { state: "serious".to_string() }));
        assert!(stop.exists(), "breaker must fire from accumulated None-tick elapsed time alone");
        let pace = read_pace(dir.path());
        assert_eq!(pace["reason"], serde_json::json!("thermal-critical"));
        assert_eq!(pace["expires"], serde_json::json!(false));
    }

    #[test]
    fn none_reading_while_paused_resets_the_resume_hold() {
        // A reading gap is NOT evidence of recovery — it must not count
        // toward (or preserve) a continuous resume_at hold, matching the
        // "still hot" reset the Some(above-resume_at) branch already
        // applies.
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);

        // 58s of "fair" (just under the 60s hold)...
        for _ in 0..29 {
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
        }
        // ...then one None tick — must reset the hold clock, same as a
        // tick back above resume_at would.
        assert_eq!(gov.on_sample(None, 2000, dir.path(), None), None);

        // Even 58 more seconds of "fair" must NOT be enough to resume,
        // because the hold restarted at the None tick above.
        for _ in 0..29 {
            assert_eq!(gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None), None);
        }
        assert_eq!(
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::Resumed { state: "fair".to_string() }),
            "resume only after a FRESH continuous 60s hold following the None-tick reset"
        );
    }

    #[test]
    fn none_reading_while_idle_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        assert_eq!(gov.on_sample(None, 2000, dir.path(), None), None);
        assert!(!pace_file_path(dir.path()).exists(), "no pace file — Idle has nothing to accumulate");
    }

    // ── finding 1: host/runtime pace-file location conformance ──

    /// (#2140 review finding 1) `thermal_governor.rs`'s `pace_file_path`
    /// and `runtime/src/pace.rs`'s `pace_file_path` must join the SAME
    /// literal file name onto their root — the runtime crate is not a
    /// workspace member and cannot depend on `darkmux-crew` (or vice
    /// versa), so there is no shared type to enforce this at compile time.
    /// This reads the runtime source at test time and asserts the join
    /// literal is still `"pace.json"` — a rename on either side that isn't
    /// mirrored on the other breaks this test instead of silently going
    /// inert (exactly what shipped in the earlier stacked-but-inert state
    /// this finding caught).
    #[test]
    fn pace_file_path_matches_runtime_out_base() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let runtime_pace_rs = manifest_dir.join("../../runtime/src/pace.rs");
        let source = std::fs::read_to_string(&runtime_pace_rs)
            .unwrap_or_else(|e| panic!("reading {}: {e}", runtime_pace_rs.display()));
        assert!(
            source.contains(r#"out_dir.join("pace.json")"#),
            "runtime/src/pace.rs's pace_file_path must join \"pace.json\" onto its out_dir root \
             to match crates/darkmux-crew/src/thermal_governor.rs's host-side pace_file_path \
             (host_out.join(\"pace.json\")) — got:\n{source}"
        );
        // The host side, asserted the same way for symmetry — if this ever
        // drifts from `host_out.join("pace.json")` the two literals no
        // longer describe the same file even though this crate's own
        // `pace_file_path` still compiles fine.
        assert_eq!(
            pace_file_path(std::path::Path::new("/darkmux-out")),
            std::path::PathBuf::from("/darkmux-out/pace.json")
        );
    }

    #[test]
    fn disabled_never_writes_anything() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let mut cfg = cfg();
        cfg.enabled = false;
        let mut gov = ThermalGovernor::new(cfg);
        for state in ["serious", "critical", "nominal"] {
            assert_eq!(gov.on_sample(Some(&sample(state, 10)), 2000, dir.path(), Some(&stop)), None);
        }
        assert!(!pace_file_path(dir.path()).exists());
        assert!(!stop.exists());
    }

    #[test]
    fn no_stop_file_target_never_panics_on_breaker() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        // `stop_file: None` — the non-crawl-dispatch case. Breaker still
        // pauses via the pace file; there's simply nothing crawl-shaped to
        // stop.
        let ev = gov.on_sample(Some(&sample("critical", 100)), 2000, dir.path(), None);
        assert_eq!(ev, Some(ThermalEvent::Breaker { state: "critical".to_string() }));
    }

    // ── stop_file_path_from_record_context ──

    #[test]
    fn stop_file_path_derives_from_crawl_record_context() {
        let ctx = serde_json::json!({
            "workspace": "my-manifest",
            "source": "github",
            "sha": "abc123",
            "rule": "some-rule",
            "rules": ["some-rule"],
            "unit": "unit-1",
        });
        let path = stop_file_path_from_record_context(Some(&ctx)).unwrap();
        assert!(path.ends_with("crawl/my-manifest/STOP"), "{}", path.display());
    }

    #[test]
    fn stop_file_path_none_without_unit_marker() {
        // A record_context that isn't crawl-shaped (no `unit`) must not
        // synthesize a STOP path — avoids a spurious write under an
        // unrelated dispatch's context.
        let ctx = serde_json::json!({ "workspace": "my-manifest" });
        assert_eq!(stop_file_path_from_record_context(Some(&ctx)), None);
    }

    #[test]
    fn stop_file_path_none_when_absent() {
        assert_eq!(stop_file_path_from_record_context(None), None);
    }
}
