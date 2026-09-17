//! (#2779) The scenario library's regression suite — one test per shipped
//! scenario, each pinning a defect that ACTUALLY shipped in #2774 and was
//! proven exactly once, by a hand-written probe that was then deleted.
//!
//! Every test constructs its governor config LITERALLY rather than through
//! `from_env()`. That is not fastidiousness: `from_env` reads process-wide
//! env, so a suite built on it passes or fails on whoever exported
//! `DARKMUX_THERMAL_*`, and the scenarios would then be pinning the
//! developer's shell rather than the shipped behavior. [`shipped_defaults`]
//! is the literal restatement of what `config_access`'s defaults resolve
//! to, and [`the_shipped_defaults_match_config_access`] is the drift guard
//! that keeps the restatement honest.

use super::*;
use crate::governor_tick::GovernorPair;
use crate::host_source::{parse_scenario, ScriptedSource};
use crate::power_policy::{BatteryEvent, BatteryGovernor, PowerPolicyConfig};
use crate::thermal_governor::{ThermalEvent, ThermalGovernor, ThermalGovernorConfig};

/// The knob values `config_access`'s `thermal_*` accessors resolve to with
/// no env and no `config.json` — i.e. what an operator who has tuned
/// nothing actually runs. Restated literally so the scenarios are immune
/// to the ambient environment; kept honest by the drift guard below.
fn shipped_defaults() -> ThermalGovernorConfig {
    ThermalGovernorConfig {
        enabled: true,
        pause_at: "serious".to_string(),
        resume_at: "fair".to_string(),
        resume_hold_ms: 60_000,
        max_pause_ms: 900_000,
        min_cpu_speed_limit_pct: 50,
        speed_limit_hold_samples: 3,
        duty_delay_ms: 15_000,
        ratchet_factor: 2,
        episode_threshold: 2,
        tier4_enabled: true,
    }
}

/// Every scenario writes into a temp out-dir. The pace file lives at
/// `<host_out>/pace.json`, exactly as a real dispatch's does.
fn out_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn source(text: &str) -> ScriptedSource {
    ScriptedSource::new(
        parse_scenario(text).expect("a shipped scenario must parse"),
        std::path::PathBuf::from("<library>"),
    )
}

/// A governor pair with a battery governor that is INERT — its floor is 0,
/// so no charge reading can reach it. Every thermal-only scenario uses
/// this, so a thermal assertion can never be quietly satisfied by a battery
/// decision.
fn thermal_only(cfg: ThermalGovernorConfig) -> GovernorPair {
    GovernorPair::new(
        ThermalGovernor::new(cfg),
        BatteryGovernor::new(PowerPolicyConfig {
            min_battery_pct: 0,
            refuse_start_below_min: false,
            pause_running_below_min: false,
        }),
    )
}

fn reasons(driver: &ScenarioDriver) -> Vec<String> {
    driver.pace_instructions().into_iter().map(|(_, r, _)| r).collect()
}

fn thermal_event_names(driver: &ScenarioDriver) -> Vec<&'static str> {
    driver
        .thermal_events()
        .iter()
        .map(|(_, e)| match e {
            ThermalEvent::Paused { .. } => "Paused",
            ThermalEvent::Resumed { .. } => "Resumed",
            ThermalEvent::Breaker { .. } => "Breaker",
            ThermalEvent::DutyCycleEntered { .. } => "DutyCycleEntered",
            ThermalEvent::DutyCycleExited { .. } => "DutyCycleExited",
            ThermalEvent::OperatorHold { .. } => "OperatorHold",
        })
        .collect()
}

// ── the drift guard on the literal config ────────────────────────────────

#[test]
#[serial_test::serial]
fn the_shipped_defaults_match_config_access() {
    // Reads the process env, so it is `#[serial]` — and it is the ONE test
    // in this file that does. Every scenario below is env-independent by
    // construction, which is the whole point of the literal config.
    //
    // A `DARKMUX_THERMAL_*` export in the ambient environment would make
    // this compare the operator's tuning against the built-in defaults and
    // fail for the wrong reason, so each is cleared for the duration.
    struct Cleared(Vec<(&'static str, Option<String>)>);
    impl Drop for Cleared {
        fn drop(&mut self) {
            for (k, v) in &self.0 {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
    let keys = [
        "DARKMUX_THERMAL_ENABLED",
        "DARKMUX_THERMAL_PAUSE_AT",
        "DARKMUX_THERMAL_RESUME_AT",
        "DARKMUX_THERMAL_RESUME_HOLD_MS",
        "DARKMUX_THERMAL_MAX_PAUSE_MS",
        "DARKMUX_THERMAL_MIN_CPU_SPEED_LIMIT_PCT",
        "DARKMUX_THERMAL_SPEED_LIMIT_HOLD_SAMPLES",
        "DARKMUX_THERMAL_DUTY_DELAY_MS",
        "DARKMUX_THERMAL_RATCHET_FACTOR",
        "DARKMUX_THERMAL_EPISODE_THRESHOLD",
        "DARKMUX_THERMAL_TIER4_ENABLED",
    ];
    let _restore = Cleared(keys.iter().map(|k| (*k, std::env::var(k).ok())).collect());
    for k in keys {
        std::env::remove_var(k);
    }

    let d = shipped_defaults();
    let a = ThermalGovernorConfig::from_env();
    assert_eq!(d.pause_at, a.pause_at);
    assert_eq!(d.resume_at, a.resume_at);
    assert_eq!(d.resume_hold_ms, a.resume_hold_ms);
    assert_eq!(d.max_pause_ms, a.max_pause_ms);
    assert_eq!(d.min_cpu_speed_limit_pct, a.min_cpu_speed_limit_pct);
    assert_eq!(d.speed_limit_hold_samples, a.speed_limit_hold_samples);
    assert_eq!(d.duty_delay_ms, a.duty_delay_ms);
    assert_eq!(d.ratchet_factor, a.ratchet_factor);
    assert_eq!(d.episode_threshold, a.episode_threshold);
    assert_eq!(d.tier4_enabled, a.tier4_enabled);
    assert_eq!(d.enabled, a.enabled);
}

#[test]
fn every_shipped_scenario_parses_and_none_is_silently_missing() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios");
    let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
        .expect("scenarios dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".jsonl"))
        .collect();
    on_disk.sort();
    let mut embedded: Vec<String> = library::all().iter().map(|(n, _)| (*n).to_string()).collect();
    embedded.sort();
    assert_eq!(
        embedded, on_disk,
        "a scenario file added to the directory but never embedded is a fixture no test runs"
    );
    for (name, text) in library::all() {
        let frames = parse_scenario(text).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(!frames.is_empty(), "{name}");
    }
}

// ── scenario 1: nominal throughout ───────────────────────────────────────

#[test]
fn nominal_throughout_never_engages_the_governor_at_all() {
    let out = out_dir();
    let mut d = ScenarioDriver::new(
        source(library::NOMINAL_THROUGHOUT),
        thermal_only(shipped_defaults()),
        out.path(),
    );
    d.run_to_end();

    assert!(d.ticks().len() > 250, "ten minutes at a 2s tick is ~300 samples");
    assert_eq!(thermal_event_names(&d), Vec::<&str>::new(), "tier 1 fires nothing");
    assert_eq!(d.pace_instructions(), Vec::new(), "an unengaged governor writes no instruction");
    assert!(
        !crate::pace_file::path(out.path()).exists(),
        "a cool machine must leave NO pace file — the runtime's own reader treats a present \
         file as an instruction"
    );
    let summary = d.pair().thermal.ladder_summary();
    assert_eq!(summary.serious_episodes, 0);
    assert_eq!(
        summary.current_duty_delay_ms,
        shipped_defaults().duty_delay_ms,
        "the delay must still be at base — nothing ratcheted"
    );
}

// ── scenario 2: sustained fair ───────────────────────────────────────────

#[test]
fn sustained_fair_engages_the_duty_cycle_and_writes_a_real_turn_delay() {
    let out = out_dir();
    let mut d = ScenarioDriver::new(
        source(library::SUSTAINED_FAIR),
        thermal_only(shipped_defaults()),
        out.path(),
    );
    d.run_to_end();

    assert_eq!(
        thermal_event_names(&d),
        vec!["DutyCycleEntered"],
        "ten minutes at `fair` is ONE entry, not one event per sample, and never a pause"
    );
    let (at_ms, event) = d.thermal_events()[0].clone();
    assert_eq!(
        event,
        ThermalEvent::DutyCycleEntered { state: "fair".into(), delay_ms: 15_000 },
        "the delay written is the base one — nothing has ratcheted yet"
    );
    // 20s of `nominal`, then the 30th consecutive `fair` sample (2s apart)
    // is the first at which the accumulated hold reaches 60s:
    // 20_000 + 29*2_000 = 78_000. Asserted as a tight window rather than the
    // exact number so a tick-cadence change reads as a shift, not a break.
    assert!(
        (76_000..=82_000).contains(&at_ms),
        "entry needs a CONTINUOUS resume_hold_ms (60s) inside the band, starting from the \
         first `fair` sample at 20s: {at_ms}ms"
    );

    assert_eq!(
        d.pace_instructions(),
        vec![(false, "thermal-duty-cycle".to_string(), Some(15_000))],
        "the pace file's THIRD state: not a pause, a per-turn delay"
    );
    // The F3 gap this closes on the host side: the number the runtime rests
    // on has to actually REACH the file. A governor that decided correctly
    // and wrote nothing is the same outage as one that decided wrong.
    let pace = d.pace().expect("a duty cycle owns a pace instruction");
    assert_eq!(pace["turn_delay_ms"], serde_json::json!(15_000));
    assert_eq!(pace["pause"], serde_json::json!(false));
    assert_eq!(d.pair().thermal.ladder_summary().serious_episodes, 0);
}

// ── scenario 3: the ratchet, across a dispatch boundary ──────────────────

#[test]
fn the_ratchet_doubles_never_resets_and_carries_across_a_dispatch_boundary() {
    let out = out_dir();
    let ladder = out.path().join("thermal-ladder.json");
    // `episode_threshold: 0` — unbounded — so this scenario isolates the
    // RATCHET. At the shipped default of 2 the second episode would escalate
    // to tier 4's terminal hold before it could ratchet, which is what
    // `nth_serious_episode_...` below pins instead.
    let cfg = ThermalGovernorConfig { episode_threshold: 0, ..shipped_defaults() };

    // Dispatch 1.
    let gov1 = ThermalGovernor::new(cfg.clone())
        .owned_by(Some("mission-a"))
        .seeded_from_mission(Some(&ladder));
    let mut d1 = ScenarioDriver::new(
        source(library::SERIOUS_RECOVER_SERIOUS),
        GovernorPair::new(gov1, BatteryGovernor::new(PowerPolicyConfig {
            min_battery_pct: 0,
            refuse_start_below_min: false,
            pause_running_below_min: false,
        })),
        out.path(),
    );
    // Stop one tick SHORT of the frame boundary at 380_000, so dispatch 1's
    // last sample is still the `fair` recovery and dispatch 2 gets the
    // `serious` transition — which is the boundary this scenario is about.
    d1.run_for(378_000);
    assert_eq!(
        thermal_event_names(&d1),
        vec!["DutyCycleEntered", "Paused", "Resumed"],
        "dispatch 1: base duty cycle, one `serious` episode, one recovery"
    );
    assert_eq!(d1.turn_delays(), vec![15_000, 30_000], "one recovery, one doubling");
    assert_eq!(d1.pair().thermal.ladder_summary().serious_episodes, 1);

    // Dispatch 2 — a FRESH governor, exactly as a crawl's next unit gets,
    // seeded from the mission's ladder-state file. The machine's condition
    // continues: the same source, at the position dispatch 1 left it.
    let src = d1.into_source();
    let gov2 = ThermalGovernor::new(cfg).owned_by(Some("mission-a")).seeded_from_mission(Some(&ladder));
    assert_eq!(
        gov2.current_duty_delay_ms(),
        30_000,
        "a fresh governor must SEED from the mission's ratcheted delay, not restart at base"
    );
    assert_eq!(gov2.serious_episodes(), 1, "and from the mission's episode count");

    let mut d2 = ScenarioDriver::new(
        src,
        GovernorPair::new(gov2, BatteryGovernor::new(PowerPolicyConfig {
            min_battery_pct: 0,
            refuse_start_below_min: false,
            pause_running_below_min: false,
        })),
        out.path(),
    );
    d2.run_to_end();

    assert_eq!(
        thermal_event_names(&d2),
        vec!["Paused", "Resumed", "DutyCycleExited"],
        "dispatch 2: the second episode, its recovery, and the cool-down exit"
    );
    assert_eq!(
        d2.turn_delays(),
        vec![60_000],
        "the second recovery doubles the CARRIED 30s, not the base 15s — without the mission \
         seeding a 40-unit crawl would reset to 15s every unit and never compound"
    );
    assert_eq!(d2.pair().thermal.ladder_summary().serious_episodes, 2);
    assert_eq!(
        d2.pair().thermal.ladder_summary().current_duty_delay_ms,
        60_000,
        "the ratchet is one-way: cooling to nominal exits the duty cycle, it does not restore \
         the pre-doubling delay"
    );
}

// ── scenario 4: the Nth serious episode ──────────────────────────────────

#[test]
fn the_nth_serious_episode_holds_for_the_operator_and_nothing_cool_releases_it() {
    let out = out_dir();
    let stop = out.path().join("STOP");
    let mut d = ScenarioDriver::new(
        source(library::NTH_SERIOUS_OPERATOR_HOLD),
        thermal_only(shipped_defaults()),
        out.path(),
    )
    .with_stop_file(&stop);
    d.run_to_end();

    assert_eq!(
        thermal_event_names(&d),
        vec!["Paused", "Resumed", "OperatorHold"],
        "episode 1 is an ordinary pause; episode 2 — the shipped threshold — is terminal"
    );
    let hold = d.thermal_events().last().expect("three events above").1.clone();
    assert_eq!(hold, ThermalEvent::OperatorHold { state: "serious".into(), episode: 2 });

    assert_eq!(
        reasons(&d).last().map(String::as_str),
        Some("thermal-episode-limit"),
        "tier 4 has its OWN reason word: a flow reader must be able to tell `the count \
         escalated` from `the machine got critical`"
    );
    let pace = d.pace().expect("a hold owns the pace file");
    assert_eq!(pace["pause"], serde_json::json!(true));

    let stop_body = std::fs::read_to_string(&stop).expect("tier 4 drops the crawl STOP file");
    assert!(
        stop_body.contains("thermal-episode-limit"),
        "the STOP file is the artifact an operator opens FIRST — it must not say \
         `thermal-critical` for an event that was not critical: {stop_body}"
    );

    // The scenario ends with FIVE MINUTES of a cool machine after the hold.
    let after: Vec<_> = d
        .thermal_events()
        .into_iter()
        .filter(|(at, _)| *at > d.thermal_events().last().unwrap().0)
        .collect();
    assert!(after.is_empty(), "nothing releases a tier-4 hold automatically: {after:?}");
    assert_eq!(
        d.pace().unwrap()["pause"],
        serde_json::json!(true),
        "and the pace file is STILL a pause after the machine cooled — resume is the \
         operator's own --resume-from call"
    );
}

#[test]
fn the_tier_four_resume_hint_is_accepted_by_the_resume_gate_it_names() {
    // (#2774 review F2) The hold's whole value is that the operator can act
    // on it. The hint the sampler prints is built by
    // `dispatch_internal::resume_hint_from_origin` from the dispatch's own
    // `resume_origin.json`, and the gate that has to ACCEPT it is
    // `validate_resume_checkpoint`. Both are private to `dispatch_internal`
    // and both are exercised by that module's own tests
    // (`resume_hint_*` / `validate_resume_checkpoint_*`), which is where
    // the round trip is pinned; this test exists to record the LINK so a
    // reader of the scenario suite is not left thinking the hint is
    // unchecked. See `dispatch_internal_tests.rs`.
    let src = include_str!("dispatch_internal_tests.rs");
    assert!(
        src.contains("validate_resume_checkpoint"),
        "the resume gate must stay exercised: tier 4's hint is only useful if the gate \
         accepts it"
    );
    assert!(
        src.contains("resume_hint_from_origin"),
        "the hint builder must stay exercised — a bare --resume-from <dir> was refused for \
         every dispatch, which is the defect that made this a review finding"
    );
}

// ── scenario 5: one sustained serious stretch ────────────────────────────

#[test]
fn one_sustained_serious_stretch_is_exactly_one_episode() {
    let out = out_dir();
    // Unbounded threshold so this test measures the COUNT rather than being
    // cut short by tier 4 — which is precisely what a per-sample count would
    // have triggered on the second tick.
    let mut d = ScenarioDriver::new(
        source(library::ONE_SUSTAINED_SERIOUS),
        thermal_only(ThermalGovernorConfig { episode_threshold: 0, ..shipped_defaults() }),
        out.path(),
    );
    d.run_to_end();

    let serious_samples =
        d.ticks().iter().filter(|t| t.reading.thermal.as_ref().is_some_and(|s| s.state == "serious")).count();
    assert!(serious_samples > 250, "the scenario must really spend hundreds of samples at `serious`: {serious_samples}");

    assert_eq!(
        d.pair().thermal.ladder_summary().serious_episodes,
        1,
        "an episode is a TRANSITION INTO a level, never a sample AT it — {serious_samples} \
         samples, one episode"
    );
    assert_eq!(
        thermal_event_names(&d),
        vec!["Paused", "Resumed"],
        "one entry, one exit: the hundreds of ticks in between are heartbeats, not events"
    );
}

// ── scenario 6: critical ─────────────────────────────────────────────────

#[test]
fn critical_trips_the_breaker_and_it_never_releases_itself() {
    let out = out_dir();
    let stop = out.path().join("STOP");
    let mut d = ScenarioDriver::new(
        source(library::CRITICAL_BREAKER),
        thermal_only(shipped_defaults()),
        out.path(),
    )
    .with_stop_file(&stop);
    d.run_to_end();

    assert_eq!(
        thermal_event_names(&d),
        vec!["Breaker"],
        "the breaker owns `critical`: it trips on the FIRST sample, with no hold, and the \
         two minutes of cool readings after it release nothing"
    );
    assert_eq!(
        d.thermal_events()[0].1,
        ThermalEvent::Breaker { state: "critical".into() }
    );
    assert_eq!(reasons(&d), vec!["thermal-critical".to_string()]);
    assert_eq!(d.pace().unwrap()["pause"], serde_json::json!(true));
    assert!(
        std::fs::read_to_string(&stop).expect("the breaker drops the STOP file").contains("thermal"),
        "a tripped breaker must stop the crawl dispatching further units"
    );
    assert_eq!(
        d.pair().thermal.ladder_summary().serious_episodes,
        0,
        "a breaker trip is not a `serious` EPISODE and must not be counted as one — the \
         episode ladder and the breaker are different mechanisms with different remedies"
    );
}

#[test]
fn a_critical_breaker_event_is_what_tier_five_s_eject_keys_on() {
    // The eject sweep itself is a SHELL-OUT (`lms unload` per resident), so
    // no in-process scenario can execute it, and this test does not pretend
    // to. The claim is split into the two halves that ARE checkable, and
    // stated so a reader can see the seam rather than assume coverage:
    //
    // 1. **The sweep's own semantics** — every managed model attempted, a
    //    failed unload recorded and the loop CONTINUED, user state never
    //    touched — are pinned by `darkmux_profiles::swap::eject_each`'s
    //    injected-unloader tests (#2774 review C1).
    // 2. **The trigger** is a `Breaker` event whose state string is
    //    literally `critical` — deliberately NOT the speed-limit trigger,
    //    which also reaches the same arm. The scenario above proves the
    //    governor produces exactly that value; this proves the sampler's
    //    arm still keys on it, which is a wiring fact no test can reach
    //    (the sampler runs on its own thread inside a live dispatch).
    //
    // Same posture as `preflight`'s own `the_pre_flight_calls_the_battery_
    // gate_at_all`: a physical source check for wiring, not a comment that
    // can drift.
    let src = include_str!("dispatch_internal.rs");
    assert!(
        src.contains("if state == \"critical\" {")
            && src.contains("tier5_eject_on_critical(&host_out, trip_wall,"),
        "the Breaker arm must still gate the tier-5 eject on the LITERAL `critical` state — \
         widening it to every breaker trigger would eject the machine's models on a \
         speed-limit proxy signal"
    );
}

// ── scenario 7: the battery/thermal interaction ──────────────────────────

#[test]
fn a_live_battery_pause_survives_a_thermal_duty_cycle() {
    let out = out_dir();
    // The battery governor's heartbeat is PINNED rather than derived from
    // `thermal_max_pause_ms()`, which reads process env — see
    // `with_restamp_interval_ms`'s own doc.
    let pair = GovernorPair::new(
        ThermalGovernor::new(shipped_defaults()),
        BatteryGovernor::new(PowerPolicyConfig {
            min_battery_pct: 50,
            refuse_start_below_min: true,
            pause_running_below_min: true,
        })
        .with_restamp_interval_ms(20_000),
    );
    let mut d = ScenarioDriver::new(source(library::BATTERY_CRITICAL_THEN_FAIR), pair, out.path());
    d.run_to_end();

    // Both halves must actually fire, or the interaction this test is about
    // was never exercised and every assertion below is vacuous.
    let battery = d.battery_events();
    assert_eq!(
        battery.iter().map(|(_, e)| e.clone()).collect::<Vec<_>>(),
        vec![BatteryEvent::Paused { charge_pct: 45, floor_pct: 50 }],
        "the charge drops below the floor exactly once and never comes back up"
    );
    let battery_paused_at = battery[0].0;
    let duty_at = d
        .thermal_events()
        .into_iter()
        .find(|(_, e)| matches!(e, ThermalEvent::DutyCycleEntered { .. }))
        .map(|(at, _)| at)
        .expect("the thermal duty cycle must engage, or the interaction is untested");
    assert!(duty_at > battery_paused_at, "the duty cycle must arrive ON TOP of a live battery pause");

    // The inversion, stated as the safety property it is.
    let final_pace = d.pace().expect("something owns the pace file");
    assert_eq!(
        final_pace["pause"],
        serde_json::json!(true),
        "a run below the battery floor must END paused. Standing the battery governor down \
         for a duty cycle (gating on `is_pacing` rather than `is_pausing`) leaves this at \
         `false` and the machine drains toward 0% while the run keeps working."
    );
    assert_eq!(
        final_pace["reason"],
        serde_json::json!(crate::power_policy::PACE_REASON),
        "and the pause in force must be the BATTERY's, not a stale thermal instruction"
    );

    // The duty cycle does transiently take the file; what matters is that
    // the battery governor takes it back, within its own heartbeat.
    let reasserted = d
        .ticks()
        .iter()
        .find(|t| {
            t.at_ms > duty_at
                && t.pace.as_ref().is_some_and(|p| {
                    p["pause"] == serde_json::json!(true)
                        && p["reason"] == serde_json::json!(crate::power_policy::PACE_REASON)
                })
        })
        .map(|t| t.at_ms)
        .expect("the battery pause must be re-asserted after the duty cycle overwrote it");
    assert!(
        reasserted - duty_at <= 2 * 20_000 + 2 * DEFAULT_TICK_MS,
        "the re-assert must land inside a heartbeat interval, not eventually: {}ms",
        reasserted - duty_at
    );
}

// ── scenario 8: degenerate threshold pairs ───────────────────────────────

#[test]
fn pause_at_equal_to_resume_at_disarms_every_soft_tier_with_a_stated_reason() {
    let out = out_dir();
    let cfg = ThermalGovernorConfig { resume_at: "serious".to_string(), ..shipped_defaults() };
    let gov = ThermalGovernor::new(cfg.clone());

    let notes = gov.disarm_notes().to_vec();
    assert_eq!(notes.len(), 1, "one refusal, covering the whole soft ladder");
    assert_eq!(notes[0].tiers, "tiers 2, 3 and 4");
    assert!(
        notes[0].why.contains("not strictly more severe"),
        "the reason must name the CONFIG, not just say `disarmed`: {}",
        notes[0].why
    );
    assert!(!notes[0].remedy.is_empty(), "a disarm note with no remedy leaves the operator stuck");
    assert!(!gov.soft_tiers_armed());

    let mut d = ScenarioDriver::new(source(library::MIXED_READINGS), thermal_only(cfg), out.path());
    d.run_to_end();

    assert_eq!(
        thermal_event_names(&d),
        Vec::<&str>::new(),
        "a disarmed ladder must fire NOTHING — the failure this replaces manufactured an \
         episode per sample and reached a terminal hold in ~62s from a reading that never \
         changed"
    );
    assert_eq!(d.pair().thermal.ladder_summary().serious_episodes, 0);
    assert!(
        !crate::pace_file::path(out.path()).exists(),
        "and it must wedge no pace instruction at all"
    );
}

#[test]
fn resume_at_nominal_disarms_tier_two_alone_and_leaves_the_pause_ladder_running() {
    let out = out_dir();
    let cfg = ThermalGovernorConfig {
        resume_at: "nominal".to_string(),
        // Unbounded, so the `serious` stretch produces an observable
        // pause/resume rather than being cut short by tier 4.
        episode_threshold: 0,
        ..shipped_defaults()
    };
    let gov = ThermalGovernor::new(cfg.clone());
    let notes = gov.disarm_notes().to_vec();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].tiers, "tier 2", "the disarm is SCOPED — tiers 3 and 4 still run");
    assert!(
        notes[0].why.contains("never leave"),
        "the reason must name the measured failure: a permanent, ratcheting delay on a cold \
         machine: {}",
        notes[0].why
    );

    let mut d = ScenarioDriver::new(source(library::MIXED_READINGS), thermal_only(cfg), out.path());
    d.run_to_end();

    assert!(
        !thermal_event_names(&d).contains(&"DutyCycleEntered"),
        "tier 2 must never engage: every reading below pause_at is inside the band and none \
         is outside it, so the duty cycle could be entered and never exited"
    );
    assert_eq!(
        d.turn_delays(),
        Vec::<u64>::new(),
        "and no turn delay may ever reach the pace file — 900 samples of `nominal` yielding a \
         ratcheting 15s -> 300s per-turn delay is the measured defect"
    );
    assert!(
        thermal_event_names(&d).contains(&"Paused"),
        "tiers 3 and 4 are still armed: the `serious` stretch must still pause the run. A \
         test that only checked tier 2 was silent would pass on a config that disarmed \
         EVERYTHING."
    );
}

// ── scenario 9: a gap in the OS thermal reading ──────────────────────────

#[test]
fn a_gap_in_the_thermal_reading_is_neither_recovery_nor_a_new_episode() {
    let out = out_dir();
    let stop = out.path().join("STOP");
    let mut d = ScenarioDriver::new(
        source(library::READING_GAPS),
        // Unbounded episodes AND an unbounded pause, so the only thing that
        // can end the pause in this scenario is a genuine recovery — which
        // is exactly the thing under test.
        thermal_only(ThermalGovernorConfig {
            episode_threshold: 0,
            max_pause_ms: 0,
            ..shipped_defaults()
        }),
        out.path(),
    )
    .with_stop_file(&stop);
    d.run_to_end();

    let gap_ticks = d.ticks().iter().filter(|t| t.reading.thermal.is_none()).count();
    assert!(gap_ticks > 50, "the scenario must really spend minutes with no reading: {gap_ticks}");

    assert_eq!(
        thermal_event_names(&d),
        vec!["Paused", "Resumed"],
        "two minutes of empty readings mid-pause must produce NO event: a gap is not \
         recovery (which would resume a run on a machine nobody can see), and not a new \
         episode (which would double-count one stretch of heat)"
    );
    assert_eq!(d.pair().thermal.ladder_summary().serious_episodes, 1);
    assert!(!d.stop_file_written(), "no breaker, no hold, so no crawl may be stopped");

    // The resume must come from the CONTINUOUS cool stretch at the end, not
    // from the gap — the gap resets the hold rather than accumulating it.
    let (resumed_at, _) = d
        .thermal_events()
        .into_iter()
        .find(|(_, e)| matches!(e, ThermalEvent::Resumed { .. }))
        .expect("the final cool stretch resumes");
    // The final cool frame starts at 320s. A resume must be a full
    // resume_hold_ms (60s) of CONTINUOUS readings INSIDE it — the two
    // minutes of gap before it count for nothing.
    assert!(
        resumed_at.saturating_sub(320_000) >= 56_000,
        "resume must wait for a full resume_hold_ms of CONTINUOUS cool readings inside the \
         last frame (which starts at 320s), not credit the gap: {resumed_at}ms"
    );
}

// ── scenario 10: flapping right at the threshold ─────────────────────────

#[test]
fn a_state_flapping_at_the_threshold_never_resumes_on_accumulated_good_ticks() {
    let out = out_dir();
    let mut d = ScenarioDriver::new(
        source(library::FLAPPING_AT_THE_THRESHOLD),
        thermal_only(ThermalGovernorConfig {
            episode_threshold: 0,
            max_pause_ms: 0,
            ..shipped_defaults()
        }),
        out.path(),
    );
    d.run_to_end();

    let fair_samples =
        d.ticks().iter().filter(|t| t.reading.thermal.as_ref().is_some_and(|s| s.state == "fair")).count();
    assert!(
        fair_samples > 15,
        "the scenario must supply plenty of recovery-band readings, or there is nothing for \
         a broken hold to accumulate: {fair_samples}"
    );

    assert_eq!(
        thermal_event_names(&d),
        vec!["Paused", "Resumed"],
        "ONE pause, and exactly one resume — from the sustained cool tail. An accumulating \
         (rather than continuous) hold would have crossed resume_hold_ms on the flapping \
         `fair` samples alone and cleared the pause on a still-hot machine, and each \
         phantom recovery would have ratcheted the delay and counted an episode"
    );
    assert_eq!(
        d.pair().thermal.ladder_summary().serious_episodes,
        1,
        "a machine that crossed the threshold twenty times, once"
    );
    assert_eq!(
        d.pair().thermal.ladder_summary().current_duty_delay_ms,
        30_000,
        "exactly ONE ratchet — from the single genuine recovery, not from each flap. Twenty \
         phantom recoveries would have multiplied the base 15s by 2^20."
    );
    assert_eq!(
        d.turn_delays(),
        Vec::<u64>::new(),
        "and no turn delay reaches the pace file: the recovery lands on `nominal`, which is \
         Idle, not the duty cycle. The ratchet is carried, not applied."
    );
}

// ── scenario 11: the CPU speed-limit floor ───────────────────────────────

#[test]
fn a_lone_low_speed_sample_is_noise_and_a_sustained_one_trips_the_breaker() {
    let out = out_dir();
    let stop = out.path().join("STOP");
    let mut d = ScenarioDriver::new(
        source(library::SPEED_LIMIT_FLOOR),
        thermal_only(shipped_defaults()),
        out.path(),
    )
    .with_stop_file(&stop);
    d.run_to_end();

    assert!(
        d.ticks().iter().all(|t| t.reading.thermal.as_ref().is_some_and(|s| s.state == "nominal")),
        "the whole scenario reads `nominal`, so anything that fires here fired on the \
         SPEED-LIMIT signal and nothing else"
    );

    // WHEN the breaker trips is the whole assertion, not just that it
    // eventually does. The scenario's frames end at 20000 / 22000 / 62000 /
    // 102000 and the driver ticks every 2000ms, so exactly one sample
    // (at 20_000) reads low before the forty-second recovery, and the
    // sustained stretch's samples land at 62_000, 64_000, 66_000, …
    // `speed_limit_hold_samples` defaults to 3, so:
    //
    // | streak reset on a healthy sample | trips on |
    // |---|---|
    // | yes (the contract, frame 3's `note`) | the THIRD sustained sample, 66_000 |
    // | no (the reset line deleted)          | the SECOND, 64_000 — the lone noise sample carried across forty seconds |
    //
    // A `> 60_000` bound is satisfied by BOTH, which is how deleting
    // `else { self.speed_limit_low_streak = 0; }` left this whole scenario
    // suite green. The window below excludes 64_000 on purpose: it is the
    // only thing here that pins frame 3's stated invariant.
    let first_event_at = d.thermal_events().first().map(|(at, _)| *at);
    assert!(
        matches!(first_event_at, Some(at) if (65_000..=67_000).contains(&at)),
        "a single sample below the floor is ordinary DVFS noise and must trip nothing, AND \
         the streak it left behind must RESET on the healthy samples that follow — otherwise \
         low samples scattered across an hour add up to a breaker trip. Trip on the third \
         CONSECUTIVE low sample (~66s), never the second (~64s): first event at \
         {first_event_at:?}"
    );
    assert_eq!(
        thermal_event_names(&d),
        vec!["Breaker"],
        "and the sustained stretch DOES trip it — a test that only proved the lone sample \
         was ignored would pass on a floor that was broken in the other direction"
    );
    assert_eq!(reasons(&d), vec!["thermal-critical".to_string()]);
    assert!(d.stop_file_written(), "a tripped breaker stops the crawl regardless of trigger");
}

// ── scenario 12: a pause that never recovers ─────────────────────────────

#[test]
fn a_pause_that_outlasts_max_pause_ms_hands_off_to_the_breaker() {
    let out = out_dir();
    let stop = out.path().join("STOP");
    let mut d = ScenarioDriver::new(
        source(library::MAX_PAUSE_EXHAUSTED),
        thermal_only(ThermalGovernorConfig {
            max_pause_ms: 120_000,
            episode_threshold: 0,
            ..shipped_defaults()
        }),
        out.path(),
    )
    .with_stop_file(&stop);
    d.run_to_end();

    assert_eq!(
        thermal_event_names(&d),
        vec!["Paused", "Breaker"],
        "tier 3 rests, and past max_pause_ms it stops resting — a run must not park \
         indefinitely on a machine that is not recovering"
    );
    let (breaker_at, _) = d.thermal_events()[1].clone();
    let (paused_at, _) = d.thermal_events()[0].clone();
    assert!(
        (120_000..=126_000).contains(&(breaker_at - paused_at)),
        "the hand-off happens AT the configured bound, not early and not eventually: \
         {}ms after the pause",
        breaker_at - paused_at
    );
    assert!(d.stop_file_written());
}

#[test]
fn max_pause_ms_of_zero_means_unbounded_and_never_hands_off() {
    let out = out_dir();
    let stop = out.path().join("STOP");
    let mut d = ScenarioDriver::new(
        source(library::MAX_PAUSE_EXHAUSTED),
        // `0` is darkmux's UNBOUNDED convention at every bound. The bare
        // comparison this replaced read it as the opposite — `elapsed >= 0`
        // is a tautology on the first sample — so an operator who set `0`
        // to mean "never escalate" got the breaker one tick after the
        // pause, plus a STOP file whose reason named a state the machine
        // had never reported.
        thermal_only(ThermalGovernorConfig {
            max_pause_ms: 0,
            episode_threshold: 0,
            ..shipped_defaults()
        }),
        out.path(),
    )
    .with_stop_file(&stop);
    d.run_to_end();

    assert_eq!(
        thermal_event_names(&d),
        vec!["Paused"],
        "ten minutes of unbroken `serious` under an unbounded cap is one pause and nothing \
         else"
    );
    assert!(
        !d.stop_file_written(),
        "and NOTHING may stop the crawl: a STOP file nothing removes, written because a \
         bound read backwards, is the worst outcome of this knob"
    );
}

// ── scenario 13: the Nth episode, entered from Idle ──────────────────────

#[test]
fn the_terminal_hold_is_reachable_from_idle_as_well_as_from_the_duty_cycle() {
    let out = out_dir();
    let mut d = ScenarioDriver::new(
        source(library::NTH_SERIOUS_FROM_IDLE),
        thermal_only(shipped_defaults()),
        out.path(),
    );
    d.run_to_end();

    assert_eq!(thermal_event_names(&d), vec!["Paused", "Resumed", "OperatorHold"]);
    assert_eq!(
        d.turn_delays(),
        Vec::<u64>::new(),
        "recovering to `nominal` lands in IDLE, which owns no turn delay — the sibling \
         scenario recovers to `fair` and lands in the duty cycle instead. Both edges into \
         the terminal hold exist and both are walked."
    );
    assert_eq!(
        d.thermal_events().last().unwrap().1,
        ThermalEvent::OperatorHold { state: "serious".into(), episode: 2 }
    );
}

// ── scenario 14: an unrecognized reading ─────────────────────────────────

#[test]
fn a_state_name_this_build_does_not_know_is_breaker_class_not_nominal() {
    let out = out_dir();
    let mut d = ScenarioDriver::new(
        source(library::UNKNOWN_READING_STATE),
        thermal_only(shipped_defaults()),
        out.path(),
    );
    d.run_to_end();

    assert_eq!(
        thermal_event_names(&d),
        vec!["Breaker"],
        "a state the code cannot rank is not evidence the machine is cool. macOS can add \
         one at any release; reading it as mild would hide real thermal pressure exactly \
         when nobody is looking for it."
    );
    assert_eq!(
        d.thermal_events()[0].1,
        ThermalEvent::Breaker { state: "unknown-7".into() },
        "and the event carries the name VERBATIM, so the artifact says what was actually \
         reported rather than what darkmux guessed it meant"
    );
}

// ── the annotated template is a real scenario, not a comment block ───────

#[test]
fn the_authoring_template_is_itself_a_runnable_scenario() {
    let out = out_dir();
    let mut d = ScenarioDriver::new(
        source(library::TEMPLATE_ANNOTATED),
        GovernorPair::new(
            ThermalGovernor::new(ThermalGovernorConfig {
                resume_hold_ms: 20_000,
                ..shipped_defaults()
            }),
            BatteryGovernor::new(PowerPolicyConfig {
                min_battery_pct: 50,
                refuse_start_below_min: true,
                pause_running_below_min: true,
            })
            .with_restamp_interval_ms(20_000),
        ),
        out.path(),
    );
    d.run_to_end();

    // The template is the artifact an operator copies. If it stops being a
    // scenario that actually drives the ladder, the first thing they write
    // is built on something broken — so it gets a test like any other.
    let names = thermal_event_names(&d);
    assert!(names.contains(&"DutyCycleEntered"), "the template must reach tier 2: {names:?}");
    assert!(names.contains(&"Paused"), "and tier 3: {names:?}");
    assert!(
        !d.battery_events().is_empty(),
        "and must exercise the battery half too — it is documented as independent of the \
         thermal half, which is only true if something reads it"
    );
    assert!(
        d.ticks().iter().any(|t| t.reading.thermal.is_none()),
        "and must contain the omitted-`thermal` frame the README describes as the probe-gap \
         reading, or the documentation describes a feature the template does not show"
    );
}

// ── degenerate configs the ladder must refuse ────────────────────────────

#[test]
fn an_unrecognized_state_token_in_the_config_disarms_the_whole_soft_ladder() {
    let out = out_dir();
    // A typo. Round 3 of #2774 ranked an unknown token ABOVE `critical`,
    // which armed the ladder while tiers 3 and 4 were unreachable — and
    // silenced the very warning that would have said so.
    let cfg = ThermalGovernorConfig { pause_at: "seroius".to_string(), ..shipped_defaults() };
    let gov = ThermalGovernor::new(cfg.clone());

    let notes = gov.disarm_notes().to_vec();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].tiers, "tiers 2, 3 and 4");
    assert!(notes[0].why.contains("seroius"), "the note must quote the typo back: {}", notes[0].why);
    assert!(
        notes[0].why.contains("nominal|fair|serious|critical"),
        "and list the valid values, or the operator cannot fix it: {}",
        notes[0].why
    );

    let mut d = ScenarioDriver::new(source(library::MIXED_READINGS), thermal_only(cfg), out.path());
    d.run_to_end();
    assert_eq!(thermal_event_names(&d), Vec::<&str>::new());
    assert!(!d.ever_wrote_a_pace_instruction(), "a typo must not pace the run either way");
    assert_eq!(d.pair().thermal.ladder_summary().serious_episodes, 0);
}

#[test]
fn pause_at_critical_disarms_the_pause_tiers_and_leaves_the_duty_cycle_running() {
    let out = out_dir();
    // `critical` is the breaker's own reading, so no soft-tier reading can
    // ever reach a pause threshold set there. Tier 2's band is unaffected.
    let cfg = ThermalGovernorConfig { pause_at: "critical".to_string(), ..shipped_defaults() };
    let gov = ThermalGovernor::new(cfg.clone());
    let notes = gov.disarm_notes().to_vec();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].tiers, "tiers 3 and 4", "scoped: tier 2 still runs");

    let mut d = ScenarioDriver::new(source(library::MIXED_READINGS), thermal_only(cfg), out.path());
    d.run_to_end();
    let names = thermal_event_names(&d);
    assert!(names.contains(&"DutyCycleEntered"), "tier 2 is armed and must engage: {names:?}");
    assert!(
        !names.contains(&"Paused") && !names.contains(&"OperatorHold"),
        "and the `serious` stretch must NOT pause — the tier that would have is disarmed, \
         which is precisely what the note says: {names:?}"
    );
    assert_eq!(d.pair().thermal.ladder_summary().serious_episodes, 0);
}

#[test]
fn a_disabled_governor_decides_nothing_at_all() {
    let out = out_dir();
    let stop = out.path().join("STOP");
    let mut d = ScenarioDriver::new(
        source(library::MIXED_READINGS),
        thermal_only(ThermalGovernorConfig { enabled: false, ..shipped_defaults() }),
        out.path(),
    )
    .with_stop_file(&stop);
    d.run_to_end();

    assert_eq!(thermal_event_names(&d), Vec::<&str>::new());
    assert!(
        !d.ever_wrote_a_pace_instruction() && !d.pace_file_exists(),
        "`enabled: false` is the operator switching the feature OFF — it must write no \
         instruction at any point, not merely end with the file cleared"
    );
    assert!(!d.stop_file_written(), "and must never stop a crawl");
}
