//! (#2779) The scenario driver — a [`crate::host_source::ScriptedSource`]
//! fed through the live [`crate::governor_tick::GovernorPair`] at a fixed
//! simulated interval, so a 40-minute escalation regression-tests in
//! microseconds.
//!
//! # What this proves, and what it does not
//!
//! It proves the LADDER: the tier transitions, the ratchet, the episode
//! count, the two governors' interaction, and what each decision writes to
//! the pace file. It does that by calling the SAME `GovernorPair::tick` the
//! live dispatch sampler calls — not a reimplementation of it, which is the
//! failure mode an in-process driver invites and the reason that seam
//! exists as a module at all.
//!
//! It does NOT prove that an IOKit reading is interpreted correctly.
//! [`crate::host_source::RealSource`] stays untested by construction; the
//! live dogfood run remains the check on the probe itself. Nor does it
//! prove the pace file reaches a TURN — that is the runtime's side of the
//! boundary, reachable by pointing `DARKMUX_HOST_SOURCE_SCRIPT` at a
//! scenario and running a real containered dispatch against
//! `tools/darkmux-mock-model` (fake host + fake model), which the facade
//! makes possible and which this driver deliberately does not simulate.
//!
//! # The tick is the live sampler's own cadence
//!
//! [`DEFAULT_TICK_MS`] is 2000 — `dispatch_internal`'s
//! `TELEMETRY_SAMPLE_INTERVAL_MS`. A scenario's `hold_ms` values are
//! therefore read at the same granularity the real sampler would read them
//! at, so a hold shorter than one tick is invisible in a test for exactly
//! the reason it would be invisible in production.

use crate::governor_tick::{GovernorPair, TickEvents};
use crate::host_source::{HostReading, HostSource, ScriptedSource};
use crate::power_policy::BatteryEvent;
use crate::thermal_governor::ThermalEvent;
use std::path::{Path, PathBuf};

/// The live dispatch sampler's own cadence (`TELEMETRY_SAMPLE_INTERVAL_MS`).
pub const DEFAULT_TICK_MS: u64 = 2000;

/// One tick's full record — what was read, what was decided, and what the
/// pace file said afterwards.
#[derive(Debug)]
pub struct TickRecord {
    /// Simulated ms since the driver's first tick.
    pub at_ms: u64,
    pub reading: HostReading,
    pub events: TickEvents,
    /// The pace file's contents after this tick, when one exists.
    pub pace: Option<serde_json::Value>,
}

/// Drives a scenario through a governor pair and records every tick.
pub struct ScenarioDriver {
    source: ScriptedSource,
    pair: GovernorPair,
    tick_ms: u64,
    host_out: PathBuf,
    stop_file: Option<PathBuf>,
    at_ms: u64,
    ticks: Vec<TickRecord>,
}

impl ScenarioDriver {
    /// Take ownership of a source (possibly one a previous driver already
    /// advanced — see [`Self::into_source`]) and a governor pair.
    pub fn new(source: ScriptedSource, pair: GovernorPair, host_out: &Path) -> Self {
        Self {
            source,
            pair,
            tick_ms: DEFAULT_TICK_MS,
            host_out: host_out.to_path_buf(),
            stop_file: None,
            at_ms: 0,
            ticks: Vec::new(),
        }
    }

    /// Drop a crawl `STOP` file at `path` when the breaker or tier 4 fires.
    pub fn with_stop_file(mut self, path: &Path) -> Self {
        self.stop_file = Some(path.to_path_buf());
        self
    }

    /// Override the simulated interval between ticks.
    pub fn with_tick_ms(mut self, tick_ms: u64) -> Self {
        assert!(tick_ms > 0, "a zero tick would never advance the scenario");
        self.tick_ms = tick_ms;
        self
    }

    /// Hand the (advanced) source back, so a SECOND dispatch's governor can
    /// continue against the same machine condition — which is what makes
    /// the mission-scoped ratchet testable at all.
    pub fn into_source(self) -> ScriptedSource {
        self.source
    }

    /// One tick: advance the simulated clock, read, feed both governors,
    /// snapshot the pace file.
    pub fn tick(&mut self) -> &TickRecord {
        self.source.advance(self.tick_ms);
        self.at_ms = self.at_ms.saturating_add(self.tick_ms);
        let reading = self.source.read();
        let events = self.pair.tick(&reading, self.tick_ms, &self.host_out, self.stop_file.as_deref());
        let pace = read_pace(&self.host_out);
        self.ticks.push(TickRecord { at_ms: self.at_ms, reading, events, pace });
        self.ticks.last().expect("just pushed")
    }

    /// Tick until `ms` of simulated time has elapsed IN THIS DRIVER.
    pub fn run_for(&mut self, ms: u64) -> &mut Self {
        let target = self.at_ms.saturating_add(ms);
        while self.at_ms < target {
            self.tick();
        }
        self
    }

    /// Tick until the scenario's own span is exhausted — i.e. until the
    /// simulated clock is past the last frame's end.
    pub fn run_to_end(&mut self) -> &mut Self {
        let span = self.source.span_ms();
        while self.source.now_ms() < span {
            self.tick();
        }
        self
    }

    pub fn ticks(&self) -> &[TickRecord] {
        &self.ticks
    }

    pub fn pair(&self) -> &GovernorPair {
        &self.pair
    }

    pub fn pair_mut(&mut self) -> &mut GovernorPair {
        &mut self.pair
    }

    /// Every thermal event this driver saw, in order, paired with the
    /// simulated time it fired at.
    pub fn thermal_events(&self) -> Vec<(u64, ThermalEvent)> {
        self.ticks
            .iter()
            .filter_map(|t| t.events.thermal.clone().map(|e| (t.at_ms, e)))
            .collect()
    }

    /// Every battery event this driver saw, in order.
    pub fn battery_events(&self) -> Vec<(u64, BatteryEvent)> {
        self.ticks
            .iter()
            .filter_map(|t| t.events.battery.clone().map(|e| (t.at_ms, e)))
            .collect()
    }

    /// The pace file as of the last tick.
    pub fn pace(&self) -> Option<&serde_json::Value> {
        self.ticks.last().and_then(|t| t.pace.as_ref())
    }

    // ── Absence, as a first-class assertion ──────────────────────────────
    //
    // The inversions #2774 actually shipped both look like NOTHING HAPPENED
    // from the outside: a duty cycle silently releasing a live battery
    // pause, and a cold machine reaching a false `thermal-critical`. A
    // suite that can only assert presence cannot catch either, so the three
    // absences a scenario needs are named methods rather than something
    // each test re-derives from raw ticks.

    /// Did ANY pace instruction ever reach the runtime? `false` is the
    /// assertion a "the governor never engaged" scenario makes — and note
    /// it is stronger than checking the file at the end, because a governor
    /// that paced and then cleared leaves a file saying `pause: false`.
    pub fn ever_wrote_a_pace_instruction(&self) -> bool {
        self.ticks.iter().any(|t| t.pace.is_some())
    }

    /// Does a pace file exist on disk right now? A cool machine must leave
    /// NONE — the runtime's reader treats a present file as an instruction.
    pub fn pace_file_exists(&self) -> bool {
        crate::pace_file::path(&self.host_out).exists()
    }

    /// Was a crawl `STOP` file written? `false` is the assertion every
    /// scenario short of the breaker and tier 4 makes: a STOP file stops a
    /// whole mission dispatching further units, and nothing removes it.
    pub fn stop_file_written(&self) -> bool {
        self.stop_file.as_ref().is_some_and(|p| p.exists())
    }

    /// The `STOP` file's contents, when one was written.
    pub fn stop_file_body(&self) -> Option<String> {
        std::fs::read_to_string(self.stop_file.as_ref()?).ok()
    }

    /// Every DISTINCT pace-file instruction the run produced, in order —
    /// `(pause, reason, turn_delay_ms)`. Consecutive identical instructions
    /// collapse, so a heartbeat re-stamp (which changes only
    /// `written_at_ms`) does not appear as a new entry. This is the shape a
    /// scenario assertion actually wants: "what did the runtime get told,
    /// and in what order".
    pub fn pace_instructions(&self) -> Vec<(bool, String, Option<u64>)> {
        let mut out: Vec<(bool, String, Option<u64>)> = Vec::new();
        for t in &self.ticks {
            let Some(p) = t.pace.as_ref() else { continue };
            let entry = (
                p.get("pause").and_then(|v| v.as_bool()).unwrap_or(false),
                p.get("reason").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                p.get("turn_delay_ms").and_then(|v| v.as_u64()),
            );
            if out.last() != Some(&entry) {
                out.push(entry);
            }
        }
        out
    }

    /// Every `turn_delay_ms` the pace file ever carried, in order of first
    /// appearance — the ratchet's own trace.
    pub fn turn_delays(&self) -> Vec<u64> {
        let mut out: Vec<u64> = Vec::new();
        for (_, _, delay) in self.pace_instructions() {
            if let Some(d) = delay {
                if out.last() != Some(&d) {
                    out.push(d);
                }
            }
        }
        out
    }
}

fn read_pace(host_out: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(crate::pace_file::path(host_out)).ok()?;
    serde_json::from_str(&text).ok()
}

/// (#2779) The shipped scenario library — **each file is a defect that
/// actually shipped**, and each replaced a hand-written probe that was
/// deleted after proving it once.
///
/// Embedded with `include_str!` rather than read from disk at test time so
/// the tests have no relative-path dependency on the repository layout, and
/// so a scenario cannot be silently deleted out from under an assertion.
/// The files themselves live at `crates/darkmux-crew/scenarios/*.jsonl` and
/// are ordinary fixtures: a new scenario costs a file plus one line here,
/// not a test rewrite. They are also what an operator points
/// `DARKMUX_HOST_SOURCE_SCRIPT` at to drive a LIVE dispatch against
/// simulated hardware (see the facade's module doc).
pub mod library {
    /// Ten minutes of a cool machine — tier 1, the governor never engages.
    pub const NOMINAL_THROUGHOUT: &str = include_str!("../scenarios/nominal-throughout.jsonl");
    /// Sustained `fair` — tier 2 engages and a real `turn_delay_ms` is
    /// written.
    pub const SUSTAINED_FAIR: &str = include_str!("../scenarios/sustained-fair.jsonl");
    /// `serious` -> recover -> `serious`, spanning a dispatch boundary —
    /// the ratchet doubles, never resets, and carries across dispatches.
    pub const SERIOUS_RECOVER_SERIOUS: &str = include_str!("../scenarios/serious-recover-serious.jsonl");
    /// The Nth `serious` episode — tier 4's terminal operator-gated hold.
    pub const NTH_SERIOUS_OPERATOR_HOLD: &str =
        include_str!("../scenarios/nth-serious-operator-hold.jsonl");
    /// One sustained `serious` stretch — exactly ONE episode.
    pub const ONE_SUSTAINED_SERIOUS: &str = include_str!("../scenarios/one-sustained-serious.jsonl");
    /// `critical` — the breaker trips and stays tripped.
    pub const CRITICAL_BREAKER: &str = include_str!("../scenarios/critical-breaker.jsonl");
    /// Battery below the floor, then `fair` — the battery pause must
    /// survive the thermal duty cycle.
    pub const BATTERY_CRITICAL_THEN_FAIR: &str =
        include_str!("../scenarios/battery-critical-then-fair.jsonl");
    /// Every soft severity, replayed under degenerate threshold pairs.
    pub const MIXED_READINGS: &str = include_str!("../scenarios/mixed-readings.jsonl");
    /// A GAP in the OS thermal reading mid-episode — "time passed, no new
    /// information", which is neither recovery nor a new episode.
    pub const READING_GAPS: &str = include_str!("../scenarios/reading-gaps.jsonl");
    /// A machine sitting ON the threshold, alternating every sample — the
    /// hysteresis's whole purpose.
    pub const FLAPPING_AT_THE_THRESHOLD: &str =
        include_str!("../scenarios/flapping-at-the-threshold.jsonl");
    /// The breaker's SECOND trigger: a sustained CPU speed cap, on a
    /// machine whose reported thermal state never leaves `nominal`.
    pub const SPEED_LIMIT_FLOOR: &str = include_str!("../scenarios/speed-limit-floor.jsonl");
    /// A pause that never recovers — tier 3 hands off to the breaker at
    /// `max_pause_ms` rather than resting forever.
    pub const MAX_PAUSE_EXHAUSTED: &str = include_str!("../scenarios/max-pause-exhausted.jsonl");
    /// The Nth episode reached from IDLE (its sibling reaches it from the
    /// duty cycle) — both edges into the terminal hold.
    pub const NTH_SERIOUS_FROM_IDLE: &str = include_str!("../scenarios/nth-serious-from-idle.jsonl");
    /// A state name this build does not know — breaker-class, never
    /// silently `nominal`.
    pub const UNKNOWN_READING_STATE: &str = include_str!("../scenarios/unknown-reading-state.jsonl");
    /// The annotated template an operator copies to author a new scenario.
    /// A real, runnable scenario, not a comment block — see this
    /// directory's `README.md`.
    pub const TEMPLATE_ANNOTATED: &str = include_str!("../scenarios/template-annotated.jsonl");

    /// Every shipped scenario, as `(file name, contents)`.
    pub fn all() -> &'static [(&'static str, &'static str)] {
        &[
            ("nominal-throughout.jsonl", NOMINAL_THROUGHOUT),
            ("sustained-fair.jsonl", SUSTAINED_FAIR),
            ("serious-recover-serious.jsonl", SERIOUS_RECOVER_SERIOUS),
            ("nth-serious-operator-hold.jsonl", NTH_SERIOUS_OPERATOR_HOLD),
            ("one-sustained-serious.jsonl", ONE_SUSTAINED_SERIOUS),
            ("critical-breaker.jsonl", CRITICAL_BREAKER),
            ("battery-critical-then-fair.jsonl", BATTERY_CRITICAL_THEN_FAIR),
            ("mixed-readings.jsonl", MIXED_READINGS),
            ("reading-gaps.jsonl", READING_GAPS),
            ("flapping-at-the-threshold.jsonl", FLAPPING_AT_THE_THRESHOLD),
            ("speed-limit-floor.jsonl", SPEED_LIMIT_FLOOR),
            ("max-pause-exhausted.jsonl", MAX_PAUSE_EXHAUSTED),
            ("nth-serious-from-idle.jsonl", NTH_SERIOUS_FROM_IDLE),
            ("unknown-reading-state.jsonl", UNKNOWN_READING_STATE),
            ("template-annotated.jsonl", TEMPLATE_ANNOTATED),
        ]
    }
}

#[cfg(test)]
#[path = "host_scenario_tests.rs"]
mod tests;
