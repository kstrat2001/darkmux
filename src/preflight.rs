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
use darkmux_crew::host_probe::{power_posture, thermal};
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
/// otherwise — battery and Low Power Mode never block a launch.
pub fn check_power_posture(params: &[String]) -> Result<()> {
    let force = force_requested(params);
    let p = power_posture::sample();

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

    // Position in THERMAL_STATES: nominal=0, fair=1, serious=2,
    // critical=3. An unrecognized state (a future macOS addition) sorts
    // past `critical` rather than being read as nominal — same posture
    // `checks_power.rs::describe` takes for the doctor-side reading.
    let severity = p
        .thermal
        .as_ref()
        .map(|t| thermal::THERMAL_STATES.iter().position(|s| *s == t.state).unwrap_or(thermal::THERMAL_STATES.len()));
    if let (Some(t), Some(sev)) = (&p.thermal, severity) {
        if sev >= 1 {
            eprintln!(
                "{}",
                style::warn(&format!("thermal state: {} (CPU speed capped at {}%)", t.state, t.cpu_speed_limit_pct))
            );
        }
    }

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
        bail!(
            "refusing to start: thermal state is \"{state_name}\" — this machine is already thermally \
             stressed and a sustained mission would make it worse. Pass --force to start anyway, or let \
             the machine cool first."
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
}
