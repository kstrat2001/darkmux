//! (#2112) Mission/crawl pre-flight — power-posture warnings + the
//! thermal-state refusal, called once from each long-mission entry point
//! (`mission_launch::launch`, `crawl_launch::launch`) before any real work
//! starts.
//!
//! Reads through the SAME probe `darkmux doctor`'s "power posture" check
//! reads (`crates/darkmux-doctor/src/checks_power.rs` →
//! `darkmux_crew::host_probe::power_posture`), so the two surfaces report
//! identical facts — never two independently-drifting readings of the
//! same machine.
//!
//! **Battery power and Low Power Mode: warn only** — a long mission still
//! starts, just slower. **Thermal `serious`/`critical`: refuses to start**
//! unless `--force` was passed (a synthetic `force=true` appended to the
//! launch params — see `main.rs`'s `MissionCmd::Launch` handling, which is
//! where the CLI's `--force` flag reaches this function the same way
//! `--dry-run` already reaches every launcher as `dry_run=true`). An
//! unrecognized future thermal state is treated as at least as severe as
//! `critical`, the same conservative posture `checks_power.rs` takes.
//!
//! Does not (yet) thread a `sleep_assertion` field onto the `mission
//! start` flow record's payload — `crew::lifecycle::
//! mission_start_with_reasoning_and_payload` already accepts a payload,
//! so that wiring is a small, well-scoped follow-up once the mission-
//! minting call sites are free of concurrent work. Until then, holding the
//! assertion (see `sleep_assertion::SleepAssertion::hold` at each launch
//! entry point) is the operative behavior; only the record annotation is
//! deferred.

use anyhow::{bail, Result};
use darkmux_crew::host_probe::power_posture;
use darkmux_types::style;

/// Read a `force=true` entry out of a launcher's raw `--param key=value`
/// list. Deliberately standalone rather than reusing `mission_launch`'s
/// private `bool_param` (which operates on the post-`collect_inputs`
/// map, not the raw list this function receives before that map exists).
fn force_requested(params: &[String]) -> bool {
    params.iter().any(|p| matches!(p.split_once('='), Some(("force", v)) if v.eq_ignore_ascii_case("true")))
}

/// Print the power-posture warnings and refuse to proceed when thermal
/// state is `serious`/`critical` and `--force` wasn't passed. Read-only
/// otherwise — battery and Low Power Mode never block a launch. Thin
/// entry point: samples via [`power_posture::sample_for_preflight`]
/// (skips the ~0.8s `pmset -g log` scan unless thermal is already fair or
/// worse — #2112 review finding 2) and hands the reading to [`evaluate`].
pub fn check_power_posture(params: &[String]) -> Result<()> {
    let force = force_requested(params);
    let p = power_posture::sample_for_preflight();
    evaluate(&p, force)
}

/// The pure decision: print the warnings, and refuse (unless `force`)
/// when thermal state is `serious`/`critical`. Split out from
/// [`check_power_posture`] (#2112 review finding 1 — mirrors
/// `checks_power.rs`'s own `describe` split) so every severity/force
/// combination is table-tested without a real `pmset` on the test host,
/// and so deleting the refusal block itself is provably red rather than
/// leaving every suite green.
fn evaluate(p: &power_posture::PowerPosture, force: bool) -> Result<()> {
    if let Some(power_posture::PowerSource::Battery) = p.source {
        let pct = p.battery_pct.map(|n| format!(" ({n}%)")).unwrap_or_default();
        eprintln!(
            "{}",
            style::warn(&format!(
                "on battery power{pct} — sustained local dispatch throttles and drains the battery"
            ))
        );
    }
    if p.low_power_mode == Some(true) {
        eprintln!("{}", style::warn("Low Power Mode is on — local inference throughput is roughly halved"));
    }

    // (#2112 review, second pass finding F) Severity computed ONCE, in
    // `power_posture::severity` — see that function's doc for the `None`
    // policy this call relies on (`None` reads as "don't refuse" below,
    // via `.unwrap_or(0)`).
    let severity = power_posture::severity(p);
    if let (Some(t), Some(sev)) = (&p.thermal, severity) {
        if sev >= 1 {
            eprintln!(
                "{}",
                style::warn(&format!("thermal state: {} (CPU speed capped at {}%)", t.state, t.cpu_speed_limit_pct))
            );
        }
    }

    // (#2112 review, second pass finding D) `p.recent_thermal_emergency`
    // is only ever populated when `power_posture::sample_for_preflight`
    // decided to scan (#2112 review finding 2: always for `darkmux
    // doctor`, or when this SAME reading's thermal is already `fair` or
    // worse, or the machine is on battery/Low Power Mode — see that
    // function's own doc). Below `fair`, on AC, with Low Power Mode off,
    // it is NOT scanned here — the stated trade-off: a thermal emergency
    // that happened recently on a machine that has since cooled and is
    // otherwise healthy is reported by `darkmux doctor` (which always
    // scans), not by this launch pre-flight. That's deliberate: the
    // pre-flight's job is "is starting a long mission safe RIGHT NOW",
    // and a cooled, plugged-in, full-power machine answers "yes" without
    // needing the ~0.8s scan to confirm it.
    if let Some(e) = &p.recent_thermal_emergency {
        if e.within_24h {
            eprintln!(
                "{}",
                style::warn(&format!(
                    "a thermal-emergency forced sleep occurred at {} — within the last 24h",
                    e.at
                ))
            );
        }
    }

    if severity.unwrap_or(0) >= 2 {
        let state_name = p.thermal.as_ref().map(|t| t.state.as_str()).unwrap_or("unknown");
        if force {
            eprintln!(
                "{}",
                style::warn(&format!("--force set: starting anyway despite thermal state \"{state_name}\""))
            );
            return Ok(());
        }
        // (#2112 review CONSIDER 4) Named as the actual command, not just
        // "pass --force" — `mission propose --start` reaches this same
        // refusal by calling `mission_launch::launch` with an EMPTY params
        // slice (see `src/mission_propose.rs::persist_and_maybe_start`),
        // so there is no `--force` for that command to accept. Naming
        // `mission launch <id>`/`mission crawl` explicitly means the
        // recovery instruction is always real, regardless of which
        // surface hit this refusal.
        bail!(
            "refusing to start: thermal state is \"{state_name}\" — this machine is already thermally \
             stressed and a sustained mission would make it worse. Run `darkmux mission launch <config-id> \
             --force` (or `darkmux mission launch crawl --force`) to start anyway — `mission propose \
             --start` does not itself accept `--force` — or let the machine cool first."
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn force_requested_reads_the_synthetic_param() {
        assert!(force_requested(&["force=true".to_string()]));
        assert!(!force_requested(&["force=false".to_string()]));
        assert!(!force_requested(&["dry_run=true".to_string()]));
        assert!(!force_requested(&[]));
    }

    #[test]
    fn force_requested_is_case_insensitive_on_the_value() {
        assert!(force_requested(&["force=TRUE".to_string()]));
    }

    // ── #2112 review finding 1: table-test `evaluate` across every
    //    severity × force combination, so the refusal block going away
    //    (or `>=` becoming `>`, or the force short-circuit ever refusing)
    //    is provably red rather than leaving every suite green. ──

    use darkmux_crew::host_probe::power_posture::PowerPosture;
    use darkmux_crew::host_probe::thermal::ThermalSample;

    fn posture_at(state: Option<&str>) -> PowerPosture {
        PowerPosture {
            source: None,
            battery_pct: None,
            low_power_mode: None,
            thermal: state.map(|s| ThermalSample { state: s.into(), cpu_speed_limit_pct: 100 }),
            recent_thermal_emergency: None,
        }
    }

    #[test]
    fn nominal_and_fair_never_refuse_regardless_of_force() {
        for state in ["nominal", "fair"] {
            for force in [false, true] {
                assert!(
                    evaluate(&posture_at(Some(state)), force).is_ok(),
                    "state={state} force={force} must not refuse"
                );
            }
        }
    }

    #[test]
    fn serious_and_critical_refuse_without_force() {
        for state in ["serious", "critical"] {
            let err = evaluate(&posture_at(Some(state)), false)
                .expect_err(&format!("state={state} without --force must refuse"));
            assert!(err.to_string().contains(state), "{err}");
        }
    }

    #[test]
    fn serious_and_critical_proceed_with_force() {
        for state in ["serious", "critical"] {
            assert!(evaluate(&posture_at(Some(state)), true).is_ok(), "state={state} with --force must proceed");
        }
    }

    #[test]
    fn an_unrecognized_future_thermal_state_refuses_like_critical() {
        assert!(evaluate(&posture_at(Some("apocalyptic")), false).is_err());
        assert!(evaluate(&posture_at(Some("apocalyptic")), true).is_ok());
    }

    #[test]
    fn no_thermal_sample_at_all_never_refuses() {
        // `p.thermal: None` (unreadable — e.g. an Intel Mac): `severity`
        // resolves to `None`, and `severity.unwrap_or(0)` must stay below
        // the refusal floor rather than defaulting into a refusal.
        assert!(evaluate(&posture_at(None), false).is_ok());
    }

    #[test]
    fn the_refusal_message_names_the_real_recovery_commands() {
        // (#2112 review CONSIDER 4) `mission propose --start` calls
        // `mission_launch::launch` with an empty params slice, so it can
        // never satisfy `force_requested` — the message must name the
        // commands that actually accept `--force`, not just say "pass
        // --force" as if the current invocation could.
        let err = evaluate(&posture_at(Some("critical")), false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mission launch <config-id> --force"), "{msg}");
        assert!(msg.contains("mission launch crawl --force"), "{msg}");
        assert!(msg.contains("mission propose --start"), "{msg}");
    }

    // ── #2112 review finding 1 (+ second-pass finding A): the three
    //    long-running entry points (`mission_launch::launch`,
    //    `crawl_launch::launch`, `mission_launch_review::launch` — the
    //    LAST one is reached via `mission_launch::launch`'s OWN
    //    dedicated-launcher routing for `config_uses_review_kinds`,
    //    BEFORE `mission_launch::launch`'s own #2112 call site, so review
    //    needs its own call the same way crawl does) must each still
    //    call `check_power_posture` AND hold a `SleepAssertion` — wiring
    //    a behavioral test can't reach without spawning a real
    //    dispatch/crawl/review (forbidden for this pass), so this is a
    //    physical source check instead (same posture as CLAUDE.md's
    //    StepKind-tiering enforcement: a fact a fresh reader — human OR
    //    test — can verify directly, not just a comment that can
    //    silently drift). Deleting either call at any entry point makes
    //    THIS go red. ──
    #[test]
    fn all_long_running_entry_points_call_the_preflight_and_hold_a_sleep_assertion() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for path in ["src/mission_launch.rs", "src/crawl_launch.rs", "src/mission_launch_review.rs"] {
            let text = std::fs::read_to_string(root.join(path)).expect("read source");
            assert!(
                text.contains("preflight::check_power_posture(params)"),
                "{path} must call the power-posture pre-flight"
            );
            assert!(text.contains("SleepAssertion::hold("), "{path} must hold a sleep assertion");
        }
    }
}
