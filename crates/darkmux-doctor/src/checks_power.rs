//! (#2112) The doctor half of the power-posture pre-flight: battery, Low
//! Power Mode, thermal state, and a recent thermal-emergency forced sleep.
//! Reads through [`darkmux_crew::host_probe::power_posture`] — the same
//! probe `src/preflight.rs` calls before a long mission starts — so
//! `darkmux doctor` and the mission pre-flight report the identical
//! reading, never two independently-drifting checks of the same facts.
//!
//! Unlike the pre-flight, this check never refuses anything: `doctor` is a
//! read-only report, so even a `critical` thermal state or an
//! emergency-within-24h renders as `Warn`, not `Fail` — the REFUSAL is the
//! mission launcher's job (`--force` to override), not doctor's.

use crate::{Check, Status};
use darkmux_crew::host_probe::power_posture::{self, PowerPosture, PowerSource};

pub fn check_power_posture() -> Check {
    describe(power_posture::sample())
}

/// Render a [`PowerPosture`] reading into a `Check`. Split out from
/// [`check_power_posture`] so every warn combination is testable without a
/// real `pmset` on the test host (mirrors `describe_host_probe`'s own
/// split for the same reason).
fn describe(p: PowerPosture) -> Check {
    let name = "power posture";

    if p.source.is_none() && p.thermal.is_none() && p.low_power_mode.is_none() {
        return Check {
            name: name.into(),
            status: Status::Pass,
            message: "n/a (non-macOS, or pmset unreadable)".into(),
            hint: None,
        };
    }

    let mut warnings: Vec<String> = Vec::new();
    let mut facts: Vec<String> = Vec::new();

    match (p.source, p.battery_pct) {
        (Some(PowerSource::Battery), Some(pct)) => warnings.push(format!("on battery ({pct}%)")),
        (Some(PowerSource::Battery), None) => warnings.push("on battery".into()),
        (Some(PowerSource::Ac), Some(pct)) => facts.push(format!("AC power ({pct}%)")),
        (Some(PowerSource::Ac), None) => facts.push("AC power".into()),
        (None, _) => {}
    }

    match p.low_power_mode {
        Some(true) => warnings.push("Low Power Mode on".into()),
        Some(false) => facts.push("Low Power Mode off".into()),
        None => {}
    }

    let mut thermal_serious_or_worse = false;
    if let Some(t) = &p.thermal {
        // (#2112 review, second pass finding F) Severity computed ONCE,
        // in `power_posture::severity` — see that function's doc for the
        // `None` policy. `p.thermal` is `Some` in this branch, so `sev`
        // is never actually `None` here; the `unwrap_or(true)` below is
        // the same conservative default the shared function documents.
        let sev = power_posture::severity(&p);
        let fair_or_worse = sev.map(|i| i >= 1).unwrap_or(true);
        thermal_serious_or_worse = sev.map(|i| i >= 2).unwrap_or(true);
        let entry = format!("thermal {} (cap {}%)", t.state, t.cpu_speed_limit_pct);
        if fair_or_worse {
            warnings.push(entry);
        } else {
            facts.push(entry);
        }
    } else if p.source.is_some() || p.low_power_mode.is_some() {
        // (#2112 review NIT 8) Reachable on an Intel Mac (or any host
        // where `thermal::sample` itself resolves nothing): the OVERALL
        // "n/a" early-return above only fires when EVERY field is absent,
        // so a machine with a readable power source but no readable
        // thermal state would otherwise just silently omit thermal from
        // the message — read as "nothing to report" rather than "this
        // field didn't resolve".
        facts.push("thermal: unreadable on this Mac".into());
    }

    if let Some(e) = &p.recent_thermal_emergency {
        if e.within_24h {
            warnings.push(format!("thermal emergency at {}", e.at));
        } else {
            facts.push(format!("last thermal emergency at {} (>24h ago)", e.at));
        }
    } else {
        facts.push("no recent thermal emergency".into());
    }

    let status = if warnings.is_empty() { Status::Pass } else { Status::Warn };
    let message = if warnings.is_empty() {
        facts.join("; ")
    } else {
        format!("{} — {}", warnings.join("; "), facts.join("; "))
    };

    // (#2112 review CONSIDER 5) Imperative remedies, not a description of
    // what's slow — an operator reading `darkmux doctor` wants to know
    // what to DO. No internal jargon ("ANE") in operator-facing text.
    let hint = (!warnings.is_empty()).then(|| {
        let mut remedies: Vec<&str> = Vec::new();
        if matches!(p.source, Some(PowerSource::Battery)) {
            remedies.push("plug in");
        }
        if p.low_power_mode == Some(true) {
            remedies.push("turn Low Power Mode off");
        }
        if thermal_serious_or_worse {
            remedies.push("let the machine cool before starting a long mission");
        } else if p.thermal.as_ref().is_some_and(|t| t.state != "nominal") {
            remedies.push("let the machine cool");
        }
        // (#2112 review, second pass finding E) Independent of the
        // thermal branch above, not an `else if` on it — a recent
        // emergency on a machine that's since cooled to exactly `fair`
        // (or is currently `serious`/`critical` for an UNRELATED reason)
        // must still get the airflow remedy; the old `else if` chain
        // silently dropped it whenever the "let the machine cool" arm had
        // already fired.
        if p.recent_thermal_emergency.as_ref().is_some_and(|e| e.within_24h) {
            remedies.push("improve airflow before a sustained mission — a thermal-emergency forced sleep happened within the last 24h");
        }
        let mut lines = vec![format!("{}.", remedies.join("; "))];
        if thermal_serious_or_worse {
            lines.push(
                "`mission launch`/`crawl` refuses to start at this thermal state unless \
                 `--force` is passed."
                    .to_string(),
            );
        }
        lines.join(" ")
    });

    Check { name: name.into(), status, message, hint }
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_crew::host_probe::power_posture::ThermalEmergency;
    use darkmux_crew::host_probe::thermal::ThermalSample;

    fn base() -> PowerPosture {
        PowerPosture {
            source: Some(PowerSource::Ac),
            battery_pct: Some(100),
            low_power_mode: Some(false),
            thermal: Some(ThermalSample { state: "nominal".into(), cpu_speed_limit_pct: 100 }),
            recent_thermal_emergency: None,
        }
    }

    #[test]
    fn a_healthy_reading_passes() {
        let c = describe(base());
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("AC power"), "{}", c.message);
        assert!(c.hint.is_none());
    }

    #[test]
    fn battery_warns_with_percent() {
        let mut p = base();
        p.source = Some(PowerSource::Battery);
        p.battery_pct = Some(42);
        let c = describe(p);
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("on battery (42%)"), "{}", c.message);
    }

    #[test]
    fn low_power_mode_warns() {
        let mut p = base();
        p.low_power_mode = Some(true);
        let c = describe(p);
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("Low Power Mode on"), "{}", c.message);
    }

    #[test]
    fn thermal_fair_warns_but_does_not_claim_a_mission_would_refuse() {
        let mut p = base();
        p.thermal = Some(ThermalSample { state: "fair".into(), cpu_speed_limit_pct: 80 });
        let c = describe(p);
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("thermal fair (cap 80%)"), "{}", c.message);
        let hint = c.hint.expect("warn carries a hint");
        assert!(!hint.contains("refuses to start"), "fair is not refuse-worthy: {hint}");
    }

    #[test]
    fn thermal_nominal_does_not_warn() {
        let p = base();
        let c = describe(p);
        assert_eq!(c.status, Status::Pass);
    }

    #[test]
    fn thermal_serious_warns_and_names_the_mission_refusal() {
        let mut p = base();
        p.thermal = Some(ThermalSample { state: "serious".into(), cpu_speed_limit_pct: 40 });
        let c = describe(p);
        assert_eq!(c.status, Status::Warn);
        let hint = c.hint.expect("warn carries a hint");
        assert!(hint.contains("refuses to start"), "{hint}");
    }

    #[test]
    fn recent_thermal_emergency_warns_and_names_the_timestamp() {
        let mut p = base();
        p.recent_thermal_emergency =
            Some(ThermalEmergency { at: "2026-08-29 23:16:05 +0800".into(), within_24h: true });
        let c = describe(p);
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("2026-08-29 23:16:05 +0800"), "{}", c.message);
    }

    #[test]
    fn stale_thermal_emergency_beyond_24h_does_not_warn() {
        let mut p = base();
        p.recent_thermal_emergency =
            Some(ThermalEmergency { at: "2026-08-01 23:16:05 +0800".into(), within_24h: false });
        let c = describe(p);
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains(">24h ago"), "{}", c.message);
    }

    #[test]
    fn thermal_unreadable_is_named_rather_than_silently_omitted() {
        // (#2112 review NIT 8) Intel Mac / unreadable-thermal shape: other
        // fields resolve, thermal specifically does not.
        let mut p = base();
        p.thermal = None;
        let c = describe(p);
        assert!(c.message.contains("thermal: unreadable on this Mac"), "{}", c.message);
    }

    #[test]
    fn hint_text_is_imperative_and_names_no_jargon() {
        let mut p = base();
        p.source = Some(PowerSource::Battery);
        p.low_power_mode = Some(true);
        let c = describe(p);
        let hint = c.hint.expect("warn carries a hint");
        assert!(hint.contains("plug in"), "{hint}");
        assert!(hint.contains("turn Low Power Mode off"), "{hint}");
        assert!(!hint.to_ascii_lowercase().contains("ane"), "no ANE jargon: {hint}");
    }

    #[test]
    fn an_emergency_only_warning_still_carries_a_non_empty_remedy() {
        // Regression: current thermal nominal + AC power + LPM off, but a
        // thermal emergency happened within 24h — this is the ONE warning
        // combination where none of the other three remedy branches fire;
        // the hint must not degrade to a bare ".".
        let mut p = base();
        p.recent_thermal_emergency =
            Some(ThermalEmergency { at: "2026-08-29 23:16:05 +0800".into(), within_24h: true });
        let c = describe(p);
        let hint = c.hint.expect("warn carries a hint");
        assert_ne!(hint.trim(), ".", "empty remedy list must not render as a bare period: {hint}");
        assert!(hint.contains("airflow"), "{hint}");
    }

    #[test]
    fn the_airflow_remedy_survives_alongside_the_cool_down_remedy() {
        // (#2112 review, second pass finding E) Regression for the `else
        // if` chain bug: thermal at `fair` (which fires the "let the
        // machine cool" arm) AND a recent thermal emergency must BOTH
        // show up — the airflow remedy must not be dropped just because
        // the cool-down remedy already fired.
        let mut p = base();
        p.thermal = Some(ThermalSample { state: "fair".into(), cpu_speed_limit_pct: 80 });
        p.recent_thermal_emergency =
            Some(ThermalEmergency { at: "2026-08-29 23:16:05 +0800".into(), within_24h: true });
        let c = describe(p);
        let hint = c.hint.expect("warn carries a hint");
        assert!(hint.contains("let the machine cool"), "{hint}");
        assert!(hint.contains("airflow"), "{hint}");
    }

    #[test]
    fn no_sources_at_all_reads_as_a_clean_not_applicable_pass() {
        let p = PowerPosture {
            source: None,
            battery_pct: None,
            low_power_mode: None,
            thermal: None,
            recent_thermal_emergency: None,
        };
        let c = describe(p);
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("n/a"), "{}", c.message);
    }
}
