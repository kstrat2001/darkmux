//! (#2112) Power posture: AC vs battery (+ percent), Low Power Mode,
//! thermal state + CPU speed cap, and the most recent thermal-emergency
//! forced sleep from `pmset -g log`, if one happened recently.
//!
//! Every read here is a **no-sudo** `pmset` process spawn — this is the
//! pre-flight/doctor surface named in #1292 item 1 and #2112, distinct from
//! [`super::thermal`]'s in-process kernel/ObjC reads (which this module
//! reuses for the thermal half rather than re-implementing it). Each field
//! degrades to `None` independently, same posture as `thermal.rs`: no
//! source here is worth a panic, and a machine that answers none of them
//! still gets a `PowerPosture` back with everything `None`.
//!
//! `pmset -g log` can run to tens of thousands of lines on a machine that's
//! been up a long time; the thermal-emergency scan caps how much of it it
//! walks (see [`THERMAL_LOG_LINE_CAP`]) rather than reading the whole
//! thing.

use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::thermal::{self, ThermalSample};

/// AC vs battery, from `pmset -g ps`'s first line (`"Now drawing from 'AC
/// Power'"` / `"...'Battery Power'"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerSource {
    Ac,
    Battery,
}

/// A parsed "Dark Wake Thermal Emergency" sleep from `pmset -g log`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThermalEmergency {
    /// The log line's own timestamp text, preserved verbatim (e.g.
    /// `"2026-07-10 23:16:05 +0800"`) so the operator can grep the same
    /// line back out of `pmset -g log`.
    pub at: String,
    /// Whether `at` parsed to within the last 24h of the sample's "now".
    /// `false` when the emergency is real but old; also `false` (rather
    /// than the timestamp being dropped) when the OS's own `date` can't
    /// parse it — the raw `at` text stays available either way.
    pub within_24h: bool,
}

/// One power-posture reading. Every field is independently `None` when its
/// source didn't resolve.
#[derive(Debug, Clone, PartialEq)]
pub struct PowerPosture {
    pub source: Option<PowerSource>,
    /// 0-100, from `pmset -g ps`'s battery percent (present even on AC
    /// power, reflecting the last known charge level).
    pub battery_pct: Option<u8>,
    pub low_power_mode: Option<bool>,
    /// Reused from [`thermal::sample`] rather than re-read — same source,
    /// same semantics (`cpu_speed_limit_pct: 100` means "no cap recorded").
    pub thermal: Option<ThermalSample>,
    pub recent_thermal_emergency: Option<ThermalEmergency>,
}

/// `pmset -g ps`'s first line names the source in single quotes.
pub fn parse_power_source(ps_text: &str) -> Option<PowerSource> {
    let first = ps_text.lines().next()?;
    if first.contains("'AC Power'") {
        Some(PowerSource::Ac)
    } else if first.contains("'Battery Power'") {
        Some(PowerSource::Battery)
    } else {
        None
    }
}

/// The battery percent off the `-InternalBattery-0 ... N%; ...` line
/// `pmset -g ps` prints under the source line. Absent entirely on a
/// machine with no battery (a Mac Studio/mini/Pro).
pub fn parse_battery_pct(ps_text: &str) -> Option<u8> {
    let line = ps_text.lines().find(|l| l.contains("InternalBattery"))?;
    let digits: String = line
        .split_once(')')
        .map(|(_, rest)| rest)
        .unwrap_or(line)
        .chars()
        .take_while(|c| c.is_ascii_digit() || c.is_whitespace())
        .collect();
    digits.trim().parse().ok()
}

/// Low Power Mode from `pmset -g`'s system-wide settings block. Two key
/// names cover two macOS eras: the older boolean `lowpowermode` (the name
/// #2112 itself was written against), and the unified Energy Mode dial
/// `powermode` that later macOS versions render it as, where `1` means
/// "Low Power" (`0` = Automatic, `2` = High Power). Whichever key is
/// present decides; `None` when neither line appears at all.
pub fn parse_low_power_mode(pmset_g_text: &str) -> Option<bool> {
    for line in pmset_g_text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("lowpowermode") {
            return rest.trim().chars().next().map(|c| c == '1');
        }
    }
    for line in pmset_g_text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("powermode") {
            return rest.trim().chars().next().map(|c| c == '1');
        }
    }
    None
}

/// How many lines of `pmset -g log` the thermal-emergency scan walks,
/// newest-first, before giving up. The log is chronological (oldest
/// first); on a machine with weeks of uptime it can run past 100k lines,
/// and the pre-flight question is only ever "did this happen recently" —
/// far short of a full walk answers that.
pub const THERMAL_LOG_LINE_CAP: usize = 20_000;

/// The needle `pmset -g log` prints for a thermal-emergency forced sleep
/// (`"Entering Sleep state due to 'Dark Wake Thermal Emergency'"`, #1292).
/// Matched case-insensitively against the substring alone, since the
/// surrounding sentence has shifted wording across macOS releases but the
/// two words together have not.
const THERMAL_EMERGENCY_NEEDLE: &str = "thermal emergency";

/// Scan `pmset -g log`'s text (already read; this function does no I/O)
/// for the most recent thermal-emergency line, capped at
/// [`THERMAL_LOG_LINE_CAP`] lines from the end. Pure — the caller supplies
/// `now` so this is exercised in tests without a real clock.
pub fn find_recent_thermal_emergency(log_text: &str, now: SystemTime) -> Option<ThermalEmergency> {
    let hit = log_text.lines().rev().take(THERMAL_LOG_LINE_CAP).find(|l| {
        let lower = l.to_ascii_lowercase();
        lower.contains(THERMAL_EMERGENCY_NEEDLE)
    })?;
    let at = timestamp_prefix(hit)?;
    let within_24h = seconds_since(&at, now).map(|secs| secs <= 24 * 3600).unwrap_or(false);
    Some(ThermalEmergency { at, within_24h })
}

/// `pmset -g log` lines start `"YYYY-MM-DD HH:MM:SS +ZZZZ <rest>"` — pull
/// just the timestamp (date + time + offset, three whitespace-separated
/// tokens).
fn timestamp_prefix(line: &str) -> Option<String> {
    let mut parts = line.split_whitespace();
    let date = parts.next()?;
    let time = parts.next()?;
    let offset = parts.next()?;
    if date.len() == 10 && time.len() == 8 && (offset.starts_with('+') || offset.starts_with('-')) {
        Some(format!("{date} {time} {offset}"))
    } else {
        None
    }
}

/// Seconds between a `pmset -g log` timestamp and `now`. Shells out to the
/// system `date` to parse the `%z`-offset format rather than adding a date
/// dependency to the workspace for one field (CLAUDE.md: don't add
/// dependencies casually) — `darkmux-doctor` already shells out to `pmset`/
/// `vm_stat`/`pagesize` for the same reason.
fn seconds_since(timestamp: &str, now: SystemTime) -> Option<u64> {
    let out = Command::new("date")
        .args(["-j", "-f", "%Y-%m-%d %H:%M:%S %z", timestamp, "+%s"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let epoch: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    let event = UNIX_EPOCH + Duration::from_secs(epoch);
    now.duration_since(event).ok().map(|d| d.as_secs())
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    fn run(cmd: &str, args: &[&str]) -> Option<String> {
        let out = Command::new(cmd).args(args).output().ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// One full power-posture reading — every field independently `None`
    /// on a spawn failure or unparsed output. Costs roughly three `pmset`
    /// process spawns (~10-20ms each) plus the in-process thermal read;
    /// pre-flight/doctor-grade, not a hot path.
    pub fn sample() -> PowerPosture {
        let ps_text = run("pmset", &["-g", "ps"]);
        let source = ps_text.as_deref().and_then(parse_power_source);
        let battery_pct = ps_text.as_deref().and_then(parse_battery_pct);
        let low_power_mode = run("pmset", &["-g"]).as_deref().and_then(parse_low_power_mode);
        let thermal = thermal::sample();
        let recent_thermal_emergency =
            run("pmset", &["-g", "log"]).and_then(|log| find_recent_thermal_emergency(&log, SystemTime::now()));
        PowerPosture { source, battery_pct, low_power_mode, thermal, recent_thermal_emergency }
    }
}

#[cfg(target_os = "macos")]
pub use imp::sample;

#[cfg(not(target_os = "macos"))]
pub fn sample() -> PowerPosture {
    PowerPosture { source: None, battery_pct: None, low_power_mode: None, thermal: None, recent_thermal_emergency: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PS_AC: &str = "Now drawing from 'AC Power'\n -InternalBattery-0 (id=35258467)\t100%; charged; 0:00 remaining present: true\n";
    const PS_BATTERY: &str =
        "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=35258467)\t42%; discharging; 3:10 remaining present: true\n";

    #[test]
    fn parses_ac_and_battery_source() {
        assert_eq!(parse_power_source(PS_AC), Some(PowerSource::Ac));
        assert_eq!(parse_power_source(PS_BATTERY), Some(PowerSource::Battery));
        assert_eq!(parse_power_source("garbage"), None);
    }

    #[test]
    fn parses_battery_percent_from_either_state() {
        assert_eq!(parse_battery_pct(PS_AC), Some(100));
        assert_eq!(parse_battery_pct(PS_BATTERY), Some(42));
        assert_eq!(parse_battery_pct("Now drawing from 'AC Power'\n"), None, "no battery line at all");
    }

    #[test]
    fn low_power_mode_reads_the_legacy_boolean_key() {
        let text = " standby              1\n lowpowermode         1\n womp                 1\n";
        assert_eq!(parse_low_power_mode(text), Some(true));
        let off = " standby              1\n lowpowermode         0\n";
        assert_eq!(parse_low_power_mode(off), Some(false));
    }

    #[test]
    fn low_power_mode_falls_back_to_the_unified_powermode_dial() {
        // Newer macOS: no `lowpowermode` line at all, `powermode 1` means
        // the operator picked "Low Power" in the unified Energy Mode UI.
        let text = " standby              1\n powermode            1\n womp                 1\n";
        assert_eq!(parse_low_power_mode(text), Some(true));
        // powermode 2 == "High Power", not low power mode.
        let high = " standby              1\n powermode            2\n";
        assert_eq!(parse_low_power_mode(high), Some(false));
    }

    #[test]
    fn low_power_mode_absent_when_neither_key_appears() {
        let text = " standby              1\n hibernatemode        3\n";
        assert_eq!(parse_low_power_mode(text), None);
    }

    // Captured shape of `pmset -g log`'s thermal-emergency line (#1292):
    // `Entering Sleep state due to 'Dark Wake Thermal Emergency'`.
    const LOG_FIXTURE: &str = "\
2026-07-10 22:35:32 +0800 Assertions          \tSummary- ...\n\
2026-07-10 22:55:40 +0800 ThermalEvent        \tIgnored DarkWake thermal emergency signal TCPKeepAlive=active\n\
2026-07-10 23:16:05 +0800 Sleep               \tEntering Sleep state due to 'Dark Wake Thermal Emergency': ...\n\
2026-07-11 07:09:50 +0800 Sleep               \tEntering Sleep state due to 'Dark Wake Thermal Emergency': ...\n\
2026-07-11 07:25:00 +0800 Wake                \tDarkWake from Deep Idle\n";

    #[test]
    fn finds_the_most_recent_thermal_emergency_line() {
        // "now" one hour after the last emergency → within 24h.
        let last_at = SystemTime::UNIX_EPOCH
            + Duration::from_secs(seconds_since_epoch_for_test("2026-07-11 07:09:50 +0800") + 3600);
        let found = find_recent_thermal_emergency(LOG_FIXTURE, last_at).expect("a thermal emergency line exists");
        assert_eq!(found.at, "2026-07-11 07:09:50 +0800", "must pick the LAST match, not the first");
        assert!(found.within_24h);
    }

    #[test]
    fn a_thermal_emergency_older_than_24h_is_named_but_not_flagged_recent() {
        let far_future = SystemTime::UNIX_EPOCH
            + Duration::from_secs(seconds_since_epoch_for_test("2026-07-11 07:09:50 +0800") + 48 * 3600);
        let found = find_recent_thermal_emergency(LOG_FIXTURE, far_future).expect("still finds it");
        assert!(!found.within_24h, "48h later must not read as recent");
    }

    #[test]
    fn no_thermal_emergency_line_is_a_clean_none() {
        let clean = "2026-07-11 07:25:00 +0800 Wake                \tDarkWake from Deep Idle\n";
        assert!(find_recent_thermal_emergency(clean, SystemTime::now()).is_none());
    }

    #[test]
    fn ignored_signal_lines_also_match_the_case_insensitive_needle() {
        // "Ignored ... thermal emergency signal" lines are real events too
        // (a near-miss the breaker chose not to act on) — #1292's evidence
        // table lists both kinds, so the scan intentionally doesn't
        // require "Entering Sleep" specifically.
        let only_ignored = "2026-08-28 02:36:45 +0800 ThermalEvent        \tIgnored DarkWake thermal emergency signal TCPKeepAlive=active\n";
        assert!(find_recent_thermal_emergency(only_ignored, SystemTime::now()).is_some());
    }

    fn seconds_since_epoch_for_test(ts: &str) -> u64 {
        let out = std::process::Command::new("date")
            .args(["-j", "-f", "%Y-%m-%d %H:%M:%S %z", ts, "+%s"])
            .output()
            .expect("date must parse the fixture timestamp");
        String::from_utf8_lossy(&out.stdout).trim().parse().expect("date printed a number")
    }
}
