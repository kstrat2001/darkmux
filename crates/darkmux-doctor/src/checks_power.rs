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
    // (#1665) `is_macos` is threaded in as an argument, not read inside
    // `describe`, so both branches of the OS split stay reachable from a
    // pure unit test on any host — the same reason `describe` itself takes
    // a `PowerPosture` value instead of sampling one.
    describe(power_posture::sample(), cfg!(target_os = "macos"))
}

/// Render a [`PowerPosture`] reading into a `Check`. Split out from
/// [`check_power_posture`] so every warn combination is testable without a
/// real `pmset` on the test host (mirrors `describe_host_probe`'s own
/// split for the same reason).
///
/// (#1665) `is_macos` disambiguates the two reasons every field can come
/// back `None`: a genuinely non-macOS host (nothing to probe — `pmset`
/// doesn't exist there, expected and healthy) versus a macOS host where
/// the probe itself failed (`pmset` absent, a non-zero exit, or output in
/// a format `parse_power_source`/`parse_low_power_mode` don't recognize).
/// Before this split both read as the identical `Pass` — "n/a" — which is
/// the fail-open doctrine violation named in #1665: a check that cannot
/// verify anything must not render as reassurance. Only the macOS-but-
/// unreadable case is a Warn; a real non-macOS host stays a clean Pass,
/// because there genuinely is nothing wrong to report there.
fn describe(p: PowerPosture, is_macos: bool) -> Check {
    let name = "power posture";

    if p.source.is_none() && p.thermal.is_none() && p.low_power_mode.is_none() {
        return if is_macos {
            Check {
                name: name.into(),
                status: Status::Warn,
                message: "power/thermal state could not be read — pmset failed, exited \
                          non-zero, or returned output darkmux doesn't recognize"
                    .into(),
                hint: Some(
                    "darkmux could not verify power or thermal state on this Mac — treat it \
                     as UNKNOWN, not healthy. Try `pmset -g ps` by hand to see what's wrong."
                        .into(),
                ),
            }
        } else {
            Check {
                name: name.into(),
                status: Status::Pass,
                message: "n/a (not macOS)".into(),
                hint: None,
            }
        };
    }

    let mut warnings: Vec<String> = Vec::new();
    let mut facts: Vec<String> = Vec::new();

    match (p.source, p.battery_pct) {
        (Some(PowerSource::Battery), Some(pct)) => warnings.push(format!("on battery ({pct}%)")),
        (Some(PowerSource::Battery), None) => warnings.push("on battery".into()),
        (Some(PowerSource::Ac), Some(pct)) => facts.push(format!("AC power ({pct}%)")),
        (Some(PowerSource::Ac), None) => facts.push("AC power".into()),
        // (#1665) Some OTHER field resolved (or the all-`None` branch above
        // would have already returned), so this is a PARTIAL probe failure
        // on a real macOS host, not "nothing to report" — the same class of
        // silent drop #2112's NIT 8 already fixed for thermal below. Named
        // as a warning, not folded into `facts`: an unread power source is
        // exactly the fact a battery-drain warning depends on, so treating
        // the gap as healthy would be the fail-open bug in miniature.
        (None, _) if is_macos => warnings.push("power source: unreadable (pmset unrecognized)".into()),
        (None, _) => {}
    }

    match p.low_power_mode {
        Some(true) => warnings.push("Low Power Mode on".into()),
        Some(false) => facts.push("Low Power Mode off".into()),
        // (#1665 review MUST FIX 1) `None` means two different things and
        // only one of them is worth a warning: `low_power_mode_unreadable`
        // distinguishes "the `pmset -g` spawn itself failed" (genuinely
        // unreadable — warn) from "`pmset -g` succeeded but neither the
        // `lowpowermode` nor `powermode` key appeared at all" (a healthy
        // Mac without the feature — Intel desktops, pre-Monterey; see
        // `parse_low_power_mode`'s own doc). Warning on the latter reads a
        // healthy machine as broken, which is the bug this split fixes.
        None if is_macos && p.low_power_mode_unreadable => {
            warnings.push("Low Power Mode: unreadable (pmset unrecognized)".into())
        }
        None if is_macos => facts.push("Low Power Mode: not supported on this Mac".into()),
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
            low_power_mode_unreadable: false,
            thermal: Some(ThermalSample { state: "nominal".into(), cpu_speed_limit_pct: 100 }),
            recent_thermal_emergency: None,
        }
    }

    #[test]
    fn a_healthy_reading_passes() {
        let c = describe(base(), true);
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("AC power"), "{}", c.message);
        assert!(c.hint.is_none());
    }

    #[test]
    fn battery_warns_with_percent() {
        let mut p = base();
        p.source = Some(PowerSource::Battery);
        p.battery_pct = Some(42);
        let c = describe(p, true);
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("on battery (42%)"), "{}", c.message);
    }

    #[test]
    fn low_power_mode_warns() {
        let mut p = base();
        p.low_power_mode = Some(true);
        let c = describe(p, true);
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("Low Power Mode on"), "{}", c.message);
    }

    #[test]
    fn thermal_fair_warns_but_does_not_claim_a_mission_would_refuse() {
        let mut p = base();
        p.thermal = Some(ThermalSample { state: "fair".into(), cpu_speed_limit_pct: 80 });
        let c = describe(p, true);
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("thermal fair (cap 80%)"), "{}", c.message);
        let hint = c.hint.expect("warn carries a hint");
        assert!(!hint.contains("refuses to start"), "fair is not refuse-worthy: {hint}");
    }

    #[test]
    fn thermal_nominal_does_not_warn() {
        let p = base();
        let c = describe(p, true);
        assert_eq!(c.status, Status::Pass);
    }

    #[test]
    fn thermal_serious_warns_and_names_the_mission_refusal() {
        let mut p = base();
        p.thermal = Some(ThermalSample { state: "serious".into(), cpu_speed_limit_pct: 40 });
        let c = describe(p, true);
        assert_eq!(c.status, Status::Warn);
        let hint = c.hint.expect("warn carries a hint");
        assert!(hint.contains("refuses to start"), "{hint}");
    }

    #[test]
    fn recent_thermal_emergency_warns_and_names_the_timestamp() {
        let mut p = base();
        p.recent_thermal_emergency =
            Some(ThermalEmergency { at: "2026-08-29 23:16:05 +0800".into(), within_24h: true });
        let c = describe(p, true);
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("2026-08-29 23:16:05 +0800"), "{}", c.message);
    }

    #[test]
    fn stale_thermal_emergency_beyond_24h_does_not_warn() {
        let mut p = base();
        p.recent_thermal_emergency =
            Some(ThermalEmergency { at: "2026-08-01 23:16:05 +0800".into(), within_24h: false });
        let c = describe(p, true);
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains(">24h ago"), "{}", c.message);
    }

    #[test]
    fn thermal_unreadable_is_named_rather_than_silently_omitted() {
        // (#2112 review NIT 8) Intel Mac / unreadable-thermal shape: other
        // fields resolve, thermal specifically does not.
        let mut p = base();
        p.thermal = None;
        let c = describe(p, true);
        assert!(c.message.contains("thermal: unreadable on this Mac"), "{}", c.message);
    }

    #[test]
    fn hint_text_is_imperative_and_names_no_jargon() {
        let mut p = base();
        p.source = Some(PowerSource::Battery);
        p.low_power_mode = Some(true);
        let c = describe(p, true);
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
        let c = describe(p, true);
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
        let c = describe(p, true);
        let hint = c.hint.expect("warn carries a hint");
        assert!(hint.contains("let the machine cool"), "{hint}");
        assert!(hint.contains("airflow"), "{hint}");
    }

    fn all_none() -> PowerPosture {
        PowerPosture {
            source: None,
            battery_pct: None,
            low_power_mode: None,
            // A total probe failure (every field None) is the scenario this
            // fixture represents — the underlying `pmset -g` spawn itself
            // failed, not "succeeded but found no key".
            low_power_mode_unreadable: true,
            thermal: None,
            recent_thermal_emergency: None,
        }
    }

    /// (#1665) A genuinely non-macOS host (Linux CI, a stripped build) has
    /// nothing to probe — `pmset` doesn't exist there. That is the ONE case
    /// where every field reading `None` is healthy, not a failed
    /// verification, so it stays the clean Pass the old (unconditional)
    /// version of this check always gave.
    #[test]
    fn no_sources_on_a_non_macos_host_reads_as_a_clean_not_applicable_pass() {
        let c = describe(all_none(), false);
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("n/a"), "{}", c.message);
        assert!(c.message.contains("not macOS"), "{}", c.message);
    }

    /// (#1665) The fail-open bug this ticket was filed over: on a REAL
    /// macOS host, every field reading `None` means the `pmset` probe
    /// itself failed (absent, non-zero exit, or unrecognized output) — not
    /// that there's nothing to report. The old behavior rendered this
    /// exactly like the non-macOS case above: a clean Pass reading "n/a
    /// (non-macOS, or pmset unreadable)", which mislabeled a broken probe
    /// on an Apple Silicon laptop as "non-Apple Silicon?" and, worse,
    /// degraded to green instead of Warn when the parse rotted. A check
    /// that cannot verify anything must say so loudly, not pass.
    #[test]
    fn no_sources_on_a_macos_host_is_an_unverifiable_warn_not_a_silent_pass() {
        let c = describe(all_none(), true);
        assert_eq!(
            c.status,
            Status::Warn,
            "an unreadable probe on a real Mac must not read as healthy: {c:?}"
        );
        assert!(c.message.contains("could not be read"), "{}", c.message);
        assert!(!c.message.contains("non-macOS"), "must not blame the OS on an actual Mac: {}", c.message);
        let hint = c.hint.expect("an unverifiable reading must carry a hint");
        assert!(hint.to_ascii_uppercase().contains("UNKNOWN"), "{hint}");
    }

    /// (#1665) The narrower fail-open case: `pmset -g ps` fails (or its
    /// output stops matching `parse_power_source`) while `pmset -g` and the
    /// thermal probe both keep working. Before this fix `(None, _) => {}`
    /// silently dropped the power-source field from both `warnings` and
    /// `facts` — the message just never mentioned it, and the overall
    /// status still passed if nothing else warned. The gap must now be
    /// named, not disappear.
    #[test]
    fn a_partial_probe_failure_on_macos_is_named_not_silently_dropped() {
        let mut p = base();
        p.source = None;
        p.battery_pct = None;
        let c = describe(p, true);
        assert_eq!(c.status, Status::Warn, "{c:?}");
        assert!(c.message.contains("power source: unreadable"), "{}", c.message);
    }

    /// Same class of gap, for `low_power_mode` specifically — the GENUINE
    /// probe-failure case (`pmset -g` itself failed to run), not a healthy
    /// Mac lacking the feature. See
    /// `low_power_mode_absent_on_a_mac_without_the_feature_is_healthy_not_a_warn`
    /// below for the other `None` case this field exists to distinguish.
    #[test]
    fn low_power_mode_read_failure_on_macos_is_named_not_silently_dropped() {
        let mut p = base();
        p.low_power_mode = None;
        p.low_power_mode_unreadable = true;
        let c = describe(p, true);
        assert_eq!(c.status, Status::Warn, "{c:?}");
        assert!(c.message.contains("Low Power Mode: unreadable"), "{}", c.message);
    }

    /// (#1665 review MUST FIX 1) The fail-open-in-miniature this fix
    /// closes: `parse_low_power_mode` returns `None` when `pmset -g`
    /// SUCCEEDED but neither the legacy `lowpowermode` key nor the unified
    /// `powermode` dial appeared at all — a genuinely healthy Mac without
    /// the feature (Intel desktops, pre-Monterey releases; see that
    /// function's own doc and its
    /// `low_power_mode_absent_when_neither_key_appears` test). Before this
    /// fix `describe` warned on EVERY `None` regardless of why, so this
    /// exact reading — AC power, nominal thermal, no LPM support — came
    /// back `Warn` on a machine with nothing wrong.
    #[test]
    fn low_power_mode_absent_on_a_mac_without_the_feature_is_healthy_not_a_warn() {
        let mut p = base();
        p.low_power_mode = None;
        p.low_power_mode_unreadable = false;
        let c = describe(p, true);
        assert_eq!(c.status, Status::Pass, "a healthy Mac without LPM support must not warn: {c:?}");
        assert!(
            !c.message.to_ascii_lowercase().contains("unreadable"),
            "must not call a successful-but-key-absent read \"unreadable\": {}",
            c.message
        );
    }

    /// A partial read failure on a non-macOS host must stay silent — there
    /// is no `pmset` there to have failed, so naming an "unreadable" power
    /// source would be describing a probe that was never expected to run.
    #[test]
    fn a_partial_gap_on_non_macos_stays_silent() {
        let mut p = base();
        p.source = None;
        p.battery_pct = None;
        let c = describe(p, false);
        assert!(!c.message.contains("unreadable"), "{}", c.message);
    }

    /// (#1665 review MUST FIX 2) `check_power_posture()`'s single decision
    /// — which `is_macos` a real `darkmux doctor` run hands `describe` —
    /// has ZERO callers anywhere in this test suite; every other test
    /// above calls `describe` directly with a hardcoded bool. That makes
    /// the wiring line itself mutation-dead: hardcoding either `true`
    /// (every non-Mac CI host would warn) or `false` (the fix goes inert
    /// on every real Mac) leaves all the OTHER tests in this file green.
    /// A source-conformance assertion — same posture as
    /// `src/preflight.rs`'s `all_long_running_entry_points_call_the_
    /// preflight_and_hold_a_sleep_assertion` — mutation-proves it: reading
    /// a fact directly out of this crate's own source, rather than
    /// exercising `check_power_posture()` through a real `pmset` on the
    /// test host (which would defeat the reason `describe` was split out
    /// in the first place).
    #[test]
    fn check_power_posture_wires_the_real_os_flag_not_a_literal() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/checks_power.rs");
        let text = std::fs::read_to_string(&path).expect("read source");
        assert!(
            text.contains("describe(power_posture::sample(), cfg!(target_os = \"macos\"))"),
            "check_power_posture() must wire the REAL target_os flag into `describe`, not a \
             hardcoded bool — a hardcoded `true` warns every non-Mac CI host, a hardcoded \
             `false` makes MUST FIX 1's fix inert on every real Mac"
        );
    }
}
