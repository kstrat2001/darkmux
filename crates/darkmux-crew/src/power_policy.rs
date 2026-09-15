//! (#2706) The battery-charge policy: refuse to START below the floor, and
//! PAUSE a run already in flight that crosses it.
//!
//! Consumes #2705's charge telemetry. Three operator knobs, resolved
//! through `config_access` in the documented `env > config.json > default`
//! order — see [`PowerPolicyConfig::from_env`].
//!
//! # A machine with no battery is never gated
//!
//! This is the sharpest correctness requirement of the feature, and it is
//! enforced HERE rather than at each call site, because a call site that
//! forgot would fail silently in one of two equally bad directions.
//!
//! An absent reading is [`Option::None`] all the way down — not "treated as
//! 0%", not "treated as 100%", not defaulted either way. Every entry point
//! in this module takes `Option<&BatterySample>` and returns its inert
//! answer on `None` BEFORE reading any config. In this fleet the always-on
//! hub is a desktop and the battery-bearing laptop is the inference peer,
//! so a gate that misread absence would either block the hub permanently
//! (absence read as 0%) or silently disable itself on the one machine it
//! exists to protect (absence read as 100%). Both directions are pinned by
//! tests; see `a_machine_with_no_battery_starts_regardless_of_config` and
//! `a_machine_with_no_battery_never_pauses_regardless_of_config`.
//!
//! # Describing versus adjudicating
//!
//! darkmux describes what it observed and does not adjudicate. A gate is an
//! ACTION, so this stays clean only under a strict reading: darkmux
//! enforces a threshold **the operator wrote**, and never invents a policy
//! of its own. Every refusal names what was OBSERVED, the FLOOR, and the
//! CONFIG FIELD that produced the decision, so the operator never has to
//! wonder where it came from — and names the two ways to change it, both of
//! which are the operator's own settings.
//!
//! It does not say the battery is unhealthy, does not recommend a charge
//! policy, and does not suggest a different threshold.
//!
//! # Pausing means pausing
//!
//! The in-flight half composes with #2114's pace-file contract: the
//! governor writes `<host_out>/pace.json` with `pause: true, reason:
//! "battery"`, the runtime rests at its next turn boundary in bounded ≤2s
//! increments with its full conversation state in memory, and CONTINUES
//! from exactly there once the file flips. It is a rest, not a stop:
//! nothing is killed, no STOP file is written, and no checkpoint round-trip
//! is needed for the common case.
//!
//! **A run type whose pause cannot be resumed refuses to pause and says
//! so** rather than pausing into a state it cannot leave — see
//! [`BatteryEvent::PauseUnsupported`] and
//! [`BatteryGovernor::pause_supported`].

use crate::host_probe::BatterySample;
use std::path::Path;

/// The config field that carries the floor. Named in every refusal so the
/// operator can go straight to the line that produced the decision.
pub const FLOOR_FIELD: &str = "power.min_battery_pct";
/// The config field that turns the START policy on and off.
pub const START_POLICY_FIELD: &str = "power.refuse_start_below_min";
/// The config field that turns the IN-FLIGHT policy on and off.
pub const INFLIGHT_POLICY_FIELD: &str = "power.pause_running_below_min";

/// The `reason` this governor stamps on the pace file and on its flow
/// records. Plain text the runtime echoes back verbatim
/// (`runtime/src/pace.rs`'s `reason_or_default` — the reason is opaque
/// there, never matched against a closed set), so a battery pause is
/// honored exactly as a thermal one is.
pub const PACE_REASON: &str = "battery";

/// Resolved policy. Constructed once per decision point rather than read
/// per sample — the env tier is live-per-access in `config_access`, so a
/// long-lived governor holding a snapshot is the same discipline
/// [`crate::thermal_governor::ThermalGovernorConfig`] already follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerPolicyConfig {
    pub min_battery_pct: u8,
    pub refuse_start_below_min: bool,
    pub pause_running_below_min: bool,
}

impl PowerPolicyConfig {
    /// Resolve from the standard `env > config.json > built-in default`
    /// precedence — `DARKMUX_POWER_MIN_BATTERY_PCT` /
    /// `DARKMUX_POWER_REFUSE_START_BELOW_MIN` /
    /// `DARKMUX_POWER_PAUSE_RUNNING_BELOW_MIN` over `power.*` over
    /// `50`/`true`/`true`.
    pub fn from_env() -> Self {
        Self {
            min_battery_pct: darkmux_types::config_access::power_min_battery_pct(),
            refuse_start_below_min: darkmux_types::config_access::power_refuse_start_below_min(),
            pause_running_below_min: darkmux_types::config_access::power_pause_running_below_min(),
        }
    }
}

/// What a refusal OBSERVED and which operator setting produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerRefusal {
    /// The charge reading this decision was made on.
    pub observed_pct: u8,
    /// The floor that reading was compared against.
    pub floor_pct: u8,
    /// The config field whose `true` enabled this policy.
    pub policy_field: &'static str,
}

impl PowerRefusal {
    /// The operator-facing sentence. Names the observation, the floor and
    /// BOTH config fields involved — the one that set the number and the
    /// one that switched the policy on — plus `--force`, which is the same
    /// escape the thermal refusal on this pre-flight already offers.
    ///
    /// Deliberately carries no advice: no claim about battery health, no
    /// suggested threshold, no charging instruction. It reports the two
    /// numbers and names where the decision came from.
    pub fn message(&self) -> String {
        format!(
            "refusing to start: battery is at {}% and {FLOOR_FIELD} is {}%. This machine's own \
             config asked for this ({} = true). To start anyway: `darkmux config set {} false`, \
             lower `darkmux config set {FLOOR_FIELD} <pct>`, or pass `--force` to this launch.",
            self.observed_pct, self.floor_pct, self.policy_field, self.policy_field
        )
    }
}

/// The START decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartDecision {
    /// Start the run. This is the answer for a machine with NO battery,
    /// for a disabled policy, and for a charge at or above the floor.
    Proceed,
    /// Refuse, with everything the operator needs to know why.
    Refuse(PowerRefusal),
}

/// Should a new run start?
///
/// `battery: None` — a machine with no battery — is `Proceed`, always,
/// before any config is read. See the module doc.
///
/// The comparison is `charge < floor`, so a charge EXACTLY at the floor
/// starts: a floor of 50 means "50 is acceptable", which is how an operator
/// reads a minimum. One percent below refuses.
pub fn start_decision(battery: Option<&BatterySample>, cfg: &PowerPolicyConfig) -> StartDecision {
    let Some(b) = battery else {
        return StartDecision::Proceed;
    };
    if !cfg.refuse_start_below_min {
        return StartDecision::Proceed;
    }
    if b.charge_pct >= cfg.min_battery_pct {
        return StartDecision::Proceed;
    }
    StartDecision::Refuse(PowerRefusal {
        observed_pct: b.charge_pct,
        floor_pct: cfg.min_battery_pct,
        policy_field: START_POLICY_FIELD,
    })
}

/// One state change the in-flight governor made this tick. The caller turns
/// each into a `dispatch.rest`-family flow record so a paused run is
/// attributable, exactly as [`crate::thermal_governor::ThermalEvent`] is.
///
/// The periodic heartbeat re-stamp is NOT an event: it changes nothing
/// observable about the run's pacing, only keeps an existing pause fresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatteryEvent {
    /// Charge crossed below the floor; the pace file now holds
    /// `pause: true, reason: "battery"`.
    Paused { charge_pct: u8, floor_pct: u8 },
    /// Charge came back to at-or-above the floor; the pause is cleared and
    /// the run continues from exactly where it rested.
    Resumed { charge_pct: u8, floor_pct: u8 },
    /// The floor was crossed, the operator asked for a pause, and THIS run
    /// type cannot be resumed from one — so it refuses to pause and says so
    /// rather than parking in a state it cannot leave. Emitted at most
    /// ONCE per governor; the run continues.
    PauseUnsupported { charge_pct: u8, floor_pct: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Paused,
}

/// Per-run state machine for the in-flight half. One instance lives for the
/// run's sampler-thread lifetime, fed one charge reading per tick beside
/// the thermal governor.
///
/// # Why thermal wins
///
/// Both governors write ONE file, so precedence has to be decided rather
/// than raced. Every entry point takes `thermal_pacing`, and while that is
/// true this governor **writes nothing and emits nothing** — it records
/// that the pace file is no longer carrying its pause
/// ([`Self::pace_owned`]) and re-asserts on the first tick after thermal
/// lets go. Thermal is the more severe condition (it protects the hardware,
/// not the charge level) and its breaker is terminal, so deferring to it is
/// the right ordering; re-asserting within one sampler tick (~2s) is what
/// keeps deferring from becoming forgetting.
pub struct BatteryGovernor {
    config: PowerPolicyConfig,
    state: State,
    /// Whether the pace file currently carries THIS governor's pause. Goes
    /// false whenever the thermal governor takes the file over, which is
    /// what makes the re-assert on the next free tick unconditional rather
    /// than dependent on the heartbeat happening to come due.
    pace_owned: bool,
    /// Whether this run type can be resumed from a pace pause. `true` for
    /// the container-backed dispatch path, whose pause is an in-memory rest
    /// at a turn boundary. See [`Self::pause_supported`].
    pause_supported: bool,
    /// [`BatteryEvent::PauseUnsupported`] is emitted at most once — it is a
    /// property of the run type, so repeating it every tick would be noise
    /// on a stream this feature is supposed to keep quiet.
    announced_unsupported: bool,
    /// ms accumulated since the last pace-file write, driving the heartbeat
    /// re-stamp.
    ms_since_stamp: u64,
    /// The last charge reading from a real `Some` sample — keeps the pace
    /// file's `state` field meaningful on a tick where the probe returned
    /// nothing.
    last_known_pct: u8,
    /// How often to re-stamp a held pause. See [`Self::new`].
    restamp_interval_ms: u64,
}

impl BatteryGovernor {
    /// `restamp_interval_ms` is a quarter of the RUNTIME's own staleness
    /// ceiling, which is `config_access::thermal_max_pause_ms()`.
    ///
    /// That knob's name says "thermal" for historical reasons; the VALUE is
    /// the ceiling the runtime applies to every pause regardless of reason
    /// (`dispatch_internal.rs` forwards it as `DARKMUX_MAX_PAUSE_MS`, and
    /// `runtime/src/pace.rs` expires any pause past it — "there is NO
    /// per-reason opt-out, only an ACTIVE WRITER"). A battery pause that
    /// stamped once and went quiet would therefore release itself on a
    /// still-drained laptop, which is why re-stamping is not optional.
    pub fn new(config: PowerPolicyConfig) -> Self {
        Self {
            config,
            state: State::Idle,
            pace_owned: false,
            pause_supported: true,
            announced_unsupported: false,
            ms_since_stamp: 0,
            last_known_pct: 0,
            restamp_interval_ms: (darkmux_types::config_access::thermal_max_pause_ms() / 4).max(1),
        }
    }

    /// Declare whether this run type can be resumed from a pace pause.
    ///
    /// `true` (the default, and what the container-backed dispatch path
    /// passes) means the pause is an in-memory rest at a turn boundary and
    /// the run continues from exactly where it stopped. `false` makes the
    /// governor refuse to pause and say so once
    /// ([`BatteryEvent::PauseUnsupported`]) rather than park a run that
    /// cannot come back.
    #[must_use]
    pub fn pause_supported(mut self, supported: bool) -> Self {
        self.pause_supported = supported;
        self
    }

    /// Test-only override of the heartbeat cadence, so the re-stamp
    /// behavior is driven by an INJECTED interval rather than by whatever
    /// `thermal_max_pause_ms()` resolves to on the machine running the
    /// suite (which would make the assertion depend on process env and on
    /// the operator's own config).
    #[cfg(test)]
    fn with_restamp_interval_ms(mut self, ms: u64) -> Self {
        self.restamp_interval_ms = ms;
        self
    }

    /// Whether this governor is currently holding a pause — the mirror of
    /// [`crate::thermal_governor::ThermalGovernor::is_pacing`], so a caller
    /// can report which condition is parking a run.
    pub fn is_pacing(&self) -> bool {
        self.state == State::Paused
    }

    /// Feed one charge reading.
    ///
    /// - `battery: None` — no battery on this machine, or the probe failed
    ///   this tick: **no-op, unconditionally**, before any config is read.
    ///   The state machine does not move and nothing is written. An absent
    ///   reading is not evidence the charge crossed anything.
    /// - `elapsed_ms` is wall time since the previous tick, injected so the
    ///   heartbeat is testable without real sleeps.
    /// - `thermal_pacing` — the thermal governor holds the pace file this
    ///   tick. See the struct doc for why this governor then stands down.
    ///
    /// Returns the event that fired this tick, if any. At most one fires.
    pub fn on_sample(
        &mut self,
        battery: Option<&BatterySample>,
        elapsed_ms: u64,
        host_out: &Path,
        thermal_pacing: bool,
    ) -> Option<BatteryEvent> {
        // THE INERTNESS, first and unconditional. A machine with no battery
        // never reaches a config read, a threshold comparison, or a write.
        let b = battery?;
        if !self.config.pause_running_below_min {
            return None;
        }
        self.last_known_pct = b.charge_pct;

        if thermal_pacing {
            // Thermal owns the file. Accumulate time so the heartbeat stays
            // honest, but write nothing and claim nothing — and remember
            // that whatever is in the file now is not ours.
            self.ms_since_stamp = self.ms_since_stamp.saturating_add(elapsed_ms);
            self.pace_owned = false;
            return None;
        }

        let below = b.charge_pct < self.config.min_battery_pct;
        match self.state {
            State::Idle if below => {
                if !self.pause_supported {
                    if self.announced_unsupported {
                        return None;
                    }
                    self.announced_unsupported = true;
                    return Some(BatteryEvent::PauseUnsupported {
                        charge_pct: b.charge_pct,
                        floor_pct: self.config.min_battery_pct,
                    });
                }
                self.state = State::Paused;
                self.write_pause(host_out, true);
                Some(BatteryEvent::Paused {
                    charge_pct: b.charge_pct,
                    floor_pct: self.config.min_battery_pct,
                })
            }
            State::Idle => None,
            State::Paused if !below => {
                self.state = State::Idle;
                // Only clear a pause we actually own. If thermal overwrote
                // the file while we were standing down, clearing it here
                // would release THEIR hold on a machine still too hot.
                if self.pace_owned {
                    self.write_pause(host_out, false);
                    self.pace_owned = false;
                }
                Some(BatteryEvent::Resumed {
                    charge_pct: b.charge_pct,
                    floor_pct: self.config.min_battery_pct,
                })
            }
            State::Paused => {
                // Still below the floor. Re-assert immediately if thermal
                // took the file from us, else keep the stamp fresh on the
                // heartbeat cadence — a pause that stops being re-stamped
                // is a pause the runtime releases.
                self.ms_since_stamp = self.ms_since_stamp.saturating_add(elapsed_ms);
                if !self.pace_owned || self.ms_since_stamp >= self.restamp_interval_ms {
                    self.write_pause(host_out, true);
                }
                None
            }
        }
    }

    /// Write the pace file and reset the heartbeat accounting together, so
    /// the counter and the file on disk can never drift apart.
    ///
    /// `state` carries the charge reading the decision was made on, which
    /// is what the runtime echoes into its own `runtime.rest` trajectory
    /// events — so a paused run's own artifact says how much charge was
    /// left, not merely that something paused it.
    fn write_pause(&mut self, host_out: &Path, pause: bool) {
        crate::pace_file::write(
            host_out,
            pause,
            PACE_REASON,
            &format!("{}%", self.last_known_pct),
        );
        self.ms_since_stamp = 0;
        self.pace_owned = pause;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(floor: u8, refuse_start: bool, pause_running: bool) -> PowerPolicyConfig {
        PowerPolicyConfig {
            min_battery_pct: floor,
            refuse_start_below_min: refuse_start,
            pause_running_below_min: pause_running,
        }
    }

    fn at(pct: u8) -> BatterySample {
        BatterySample { charge_pct: pct, on_ac: false, charging: false, minutes_to_empty: Some(90) }
    }

    // ── The no-battery machine, pinned in BOTH directions ──

    #[test]
    fn a_machine_with_no_battery_starts_regardless_of_config() {
        // Absence must not read as 0%. In this fleet the always-on hub is a
        // desktop; reading absence as "drained" would block it permanently.
        for floor in [0, 1, 50, 99, 100] {
            assert_eq!(
                start_decision(None, &cfg(floor, true, true)),
                StartDecision::Proceed,
                "floor={floor}: a desktop must start regardless of the gate"
            );
        }
    }

    #[test]
    fn a_machine_with_no_battery_never_pauses_regardless_of_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut g = BatteryGovernor::new(cfg(50, true, true));
        for _ in 0..5 {
            assert_eq!(g.on_sample(None, 2_000, dir.path(), false), None);
        }
        assert!(!g.is_pacing(), "an absent reading must not move the state machine");
        assert!(
            !crate::pace_file::path(dir.path()).exists(),
            "a machine with no battery must never have a pace file written for it"
        );
    }

    #[test]
    fn a_machine_with_no_battery_is_not_read_as_full_either() {
        // The opposite failure: absence read as 100% would silently DISABLE
        // the gate on the one machine it exists to protect. Proven by
        // contrast — the same config that is inert on `None` does refuse on
        // a real low reading, so `None` is not taking a "100%" path that
        // would also have been inert.
        let c = cfg(50, true, true);
        assert_eq!(start_decision(None, &c), StartDecision::Proceed);
        assert!(
            matches!(start_decision(Some(&at(49)), &c), StartDecision::Refuse(_)),
            "the gate is live on this config — so `Proceed` for `None` is inertness, not a \
             disabled gate"
        );
    }

    // ── The floor boundary: at, one below, one above ──

    #[test]
    fn charge_exactly_at_the_floor_starts() {
        assert_eq!(
            start_decision(Some(&at(50)), &cfg(50, true, true)),
            StartDecision::Proceed,
            "a MINIMUM of 50 means 50 is acceptable"
        );
    }

    #[test]
    fn one_percent_below_the_floor_refuses() {
        let d = start_decision(Some(&at(49)), &cfg(50, true, true));
        let StartDecision::Refuse(r) = d else { panic!("must refuse: {d:?}") };
        assert_eq!(r.observed_pct, 49);
        assert_eq!(r.floor_pct, 50);
        assert_eq!(r.policy_field, START_POLICY_FIELD);
    }

    #[test]
    fn one_percent_above_the_floor_starts() {
        assert_eq!(start_decision(Some(&at(51)), &cfg(50, true, true)), StartDecision::Proceed);
    }

    #[test]
    fn the_start_policy_off_lets_a_drained_machine_start() {
        assert_eq!(
            start_decision(Some(&at(1)), &cfg(50, false, true)),
            StartDecision::Proceed,
            "refuse_start_below_min=false is the operator's documented way to allow runs under \
             the threshold"
        );
    }

    #[test]
    fn the_two_policies_are_independent() {
        // The whole reason these are two config items: an operator may want
        // one without the other.
        assert!(matches!(start_decision(Some(&at(10)), &cfg(50, true, false)), StartDecision::Refuse(_)));
        assert_eq!(start_decision(Some(&at(10)), &cfg(50, false, true)), StartDecision::Proceed);
    }

    // ── The refusal describes; it does not adjudicate ──

    #[test]
    fn the_refusal_names_the_charge_the_floor_and_the_field() {
        let msg = PowerRefusal { observed_pct: 31, floor_pct: 50, policy_field: START_POLICY_FIELD }
            .message();
        assert!(msg.contains("31%"), "the OBSERVATION must be named: {msg}");
        assert!(msg.contains("50%"), "the FLOOR must be named: {msg}");
        assert!(msg.contains(FLOOR_FIELD), "the field carrying the floor must be named: {msg}");
        assert!(msg.contains(START_POLICY_FIELD), "the field that enabled the policy must be named: {msg}");
    }

    #[test]
    fn the_refusal_gives_no_advice_about_the_battery() {
        let msg = PowerRefusal { observed_pct: 31, floor_pct: 50, policy_field: START_POLICY_FIELD }
            .message()
            .to_lowercase();
        for banned in ["unhealthy", "degrad", "plug in", "charge the", "recommend", "should set", "we suggest"] {
            assert!(
                !msg.contains(banned),
                "darkmux reports the numbers and does not advise: {banned:?} in {msg:?}"
            );
        }
    }

    // ── In-flight: cross the floor with the policy on, and with it off ──

    fn pace_json(dir: &Path) -> serde_json::Value {
        let raw = std::fs::read_to_string(crate::pace_file::path(dir)).expect("pace file exists");
        serde_json::from_str(&raw).expect("valid pace JSON")
    }

    #[test]
    fn an_in_flight_run_crossing_the_floor_pauses_when_the_policy_is_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut g = BatteryGovernor::new(cfg(50, true, true));
        assert_eq!(g.on_sample(Some(&at(55)), 2_000, dir.path(), false), None, "above the floor: nothing");
        assert!(!crate::pace_file::path(dir.path()).exists(), "no pace file until a pause fires");

        let ev = g.on_sample(Some(&at(49)), 2_000, dir.path(), false);
        assert_eq!(ev, Some(BatteryEvent::Paused { charge_pct: 49, floor_pct: 50 }));
        let v = pace_json(dir.path());
        assert_eq!(v["pause"], true);
        assert_eq!(v["reason"], PACE_REASON);
        assert_eq!(v["state"], "49%", "the artifact says how much charge was left, not just that something paused");
        assert!(g.is_pacing());
    }

    #[test]
    fn an_in_flight_run_crossing_the_floor_does_nothing_when_the_policy_is_off() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut g = BatteryGovernor::new(cfg(50, true, false));
        for pct in [49, 20, 5, 1] {
            assert_eq!(
                g.on_sample(Some(&at(pct)), 2_000, dir.path(), false),
                None,
                "pause_running_below_min=false lets the run finish"
            );
        }
        assert!(!crate::pace_file::path(dir.path()).exists());
        assert!(!g.is_pacing());
    }

    #[test]
    fn a_paused_run_resumes_when_charge_returns_and_the_pause_is_cleared() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut g = BatteryGovernor::new(cfg(50, true, true));
        g.on_sample(Some(&at(40)), 2_000, dir.path(), false);
        assert_eq!(pace_json(dir.path())["pause"], true);

        // Exactly at the floor is enough to resume — the same boundary the
        // start gate uses, so one number means one thing on both surfaces.
        let ev = g.on_sample(Some(&at(50)), 2_000, dir.path(), false);
        assert_eq!(ev, Some(BatteryEvent::Resumed { charge_pct: 50, floor_pct: 50 }));
        assert_eq!(pace_json(dir.path())["pause"], false, "the hold must actually be released");
        assert!(!g.is_pacing());
    }

    #[test]
    fn a_held_pause_keeps_being_restamped_or_the_runtime_releases_it() {
        // #2114's heartbeat contract: the runtime honors a pause only while
        // `written_at_ms` is fresh. A governor that stamps once and goes
        // quiet has written an EXPIRING pause, whatever it meant.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut g = BatteryGovernor::new(cfg(50, true, true)).with_restamp_interval_ms(10_000);
        g.on_sample(Some(&at(40)), 2_000, dir.path(), false);
        let first = pace_json(dir.path())["written_at_ms"].as_u64().expect("stamped");

        // Four 2s ticks: still inside the 10s interval, no re-stamp yet.
        for _ in 0..4 {
            assert_eq!(g.on_sample(Some(&at(40)), 2_000, dir.path(), false), None, "holding is not an event");
        }
        assert_eq!(
            pace_json(dir.path())["written_at_ms"].as_u64(),
            Some(first),
            "no re-stamp before the interval comes due"
        );

        // The fifth tick crosses 10s.
        std::thread::sleep(std::time::Duration::from_millis(2));
        g.on_sample(Some(&at(40)), 2_000, dir.path(), false);
        let restamped = pace_json(dir.path())["written_at_ms"].as_u64().expect("stamped");
        assert!(restamped >= first, "the stamp must advance, not go backwards");
        assert_eq!(pace_json(dir.path())["pause"], true, "a re-stamp holds the pause, it does not flip it");
    }

    #[test]
    fn a_run_type_that_cannot_resume_refuses_to_pause_and_says_so_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut g = BatteryGovernor::new(cfg(50, true, true)).pause_supported(false);
        assert_eq!(
            g.on_sample(Some(&at(40)), 2_000, dir.path(), false),
            Some(BatteryEvent::PauseUnsupported { charge_pct: 40, floor_pct: 50 })
        );
        assert!(
            !crate::pace_file::path(dir.path()).exists(),
            "it must not park a run in a state it cannot leave"
        );
        assert!(!g.is_pacing());
        for _ in 0..3 {
            assert_eq!(
                g.on_sample(Some(&at(35)), 2_000, dir.path(), false),
                None,
                "said once — repeating it every tick is noise on a stream this feature keeps quiet"
            );
        }
    }

    // ── Coexistence with the thermal governor over one pace file ──

    #[test]
    fn the_battery_governor_stands_down_while_thermal_holds_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut g = BatteryGovernor::new(cfg(50, true, true));
        assert_eq!(
            g.on_sample(Some(&at(40)), 2_000, dir.path(), true),
            None,
            "thermal is the more severe condition and owns the file"
        );
        assert!(
            !crate::pace_file::path(dir.path()).exists(),
            "the battery governor must not write while thermal is pacing"
        );
    }

    #[test]
    fn the_battery_governor_reasserts_on_the_first_tick_after_thermal_lets_go() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut g = BatteryGovernor::new(cfg(50, true, true)).with_restamp_interval_ms(u64::MAX);
        g.on_sample(Some(&at(40)), 2_000, dir.path(), false);
        assert_eq!(pace_json(dir.path())["reason"], PACE_REASON);

        // Thermal takes the file over and then releases it, leaving its own
        // `pause: false` behind.
        g.on_sample(Some(&at(40)), 2_000, dir.path(), true);
        crate::pace_file::write(dir.path(), false, "thermal", "nominal");
        assert_eq!(pace_json(dir.path())["pause"], false);

        // The very next free tick must re-assert, WITHOUT waiting for a
        // heartbeat that (here) never comes due — otherwise a thermal
        // resume silently un-pauses a drained laptop.
        g.on_sample(Some(&at(40)), 2_000, dir.path(), false);
        let v = pace_json(dir.path());
        assert_eq!(v["pause"], true, "still below the floor — the battery hold must come back");
        assert_eq!(v["reason"], PACE_REASON);
    }

    #[test]
    fn a_battery_resume_never_clears_a_pause_it_does_not_own() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut g = BatteryGovernor::new(cfg(50, true, true));
        g.on_sample(Some(&at(40)), 2_000, dir.path(), false);
        // Thermal takes over; we stand down and no longer own the file.
        g.on_sample(Some(&at(40)), 2_000, dir.path(), true);
        crate::pace_file::write(dir.path(), true, "thermal-critical", "critical");
        // Charge recovers while thermal still holds a critical stop. The
        // battery governor goes Idle but must NOT release the machine.
        g.on_sample(Some(&at(90)), 2_000, dir.path(), false);
        let v = pace_json(dir.path());
        assert_eq!(v["pause"], true, "a battery recovery must not release a thermal hold");
        assert_eq!(v["reason"], "thermal-critical");
    }

    #[test]
    fn an_absent_reading_mid_pause_is_not_evidence_the_charge_recovered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut g = BatteryGovernor::new(cfg(50, true, true));
        g.on_sample(Some(&at(40)), 2_000, dir.path(), false);
        assert!(g.is_pacing());
        assert_eq!(g.on_sample(None, 2_000, dir.path(), false), None);
        assert!(g.is_pacing(), "a probe that failed this tick must not resume the run");
        assert_eq!(pace_json(dir.path())["pause"], true);
    }
}
