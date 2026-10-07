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
    /// (#1665 review MUST FIX 1) `low_power_mode` reads `None` for TWO
    /// different reasons that `describe`/`checks_power.rs` must not
    /// conflate: the `pmset -g` spawn itself failing (absent, non-zero
    /// exit — genuinely unreadable), versus `pmset -g` succeeding but
    /// [`parse_low_power_mode`] finding neither the `lowpowermode` nor
    /// `powermode` key at all (a real, healthy Mac that just doesn't have
    /// the feature — Intel desktops, pre-Monterey releases; see that
    /// function's own doc). `true` only for the former. A caller that
    /// warns on every `None` regardless of this flag reads a healthy
    /// machine as broken — the exact bug this field exists to prevent.
    pub low_power_mode_unreadable: bool,
    /// Reused from [`thermal::sample`] rather than re-read — same source,
    /// same semantics (`cpu_speed_limit_pct: 100` means "no cap recorded").
    pub thermal: Option<ThermalSample>,
    pub recent_thermal_emergency: Option<ThermalEmergency>,
}

/// Thermal severity, exactly once (#2112 review, second pass finding F —
/// `preflight.rs` and `checks_power.rs` each had their own copy of this
/// position lookup). Position in [`thermal::THERMAL_STATES`] (nominal=0,
/// fair=1, serious=2, critical=3); an unrecognized state (a future macOS
/// addition) sorts PAST `critical` rather than reading as nominal — the
/// conservative default every caller wants.
///
/// **The `None` policy, stated once so no caller has to re-derive it:**
/// `None` means there is no thermal sample at all (unreadable — e.g. an
/// Intel Mac, or a spawn failure), which is a DIFFERENT case from
/// "recognized but past critical." No signal is not evidence of severity.
/// A caller gating a REFUSAL on severity therefore reads `None` as "don't
/// refuse" (`severity(p).unwrap_or(0) >= floor`); a caller deciding
/// whether it's worth the ~0.8s `pmset -g log` scan reads `None` as "no
/// known severity to scan for" (`severity(p).map(|s| s >= 1).unwrap_or
/// (false)`). Both read the SAME `Option`, just with an opposite-sign
/// default for an opposite-sign question.
pub fn severity(p: &PowerPosture) -> Option<usize> {
    p.thermal
        .as_ref()
        .map(|t| thermal::THERMAL_STATES.iter().position(|s| *s == t.state).unwrap_or(thermal::THERMAL_STATES.len()))
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
/// newest-first, before giving up, AND (#2112 review findings 2/6) the
/// `tail -n` bound applied AT THE SPAWN itself (`imp::thermal_log_bounded`
/// pipes `pmset -g log` through `tail -n THERMAL_LOG_LINE_CAP` via `sh
/// -c`), not just in the in-memory scan below. `pmset -g log` generating
/// its output is what's slow (~0.8s / tens of thousands of lines on a
/// machine with more than a few days' uptime) — piping through `tail`
/// doesn't shorten that generation — but it does stop an unbounded,
/// multi-MB `String` from crossing back into this process every time. The
/// log is chronological (oldest first); on a machine with weeks of uptime
/// it can run past 100k lines, and the pre-flight question is only ever
/// "did this happen recently" — far short of a full walk answers that.
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
    // (#2112 review finding 7) `.find(...)` + a single `timestamp_prefix`
    // call on just that one line meant a matching line with NO parseable
    // timestamp (a continuation/wrapped line, still containing the
    // needle) silently swallowed the search — an older REAL emergency a
    // few lines further back would never be reached. Walk every matching
    // line, newest-first, and take the first one whose timestamp actually
    // parses.
    let at = log_text
        .lines()
        .rev()
        .take(THERMAL_LOG_LINE_CAP)
        .filter(|l| l.to_ascii_lowercase().contains(THERMAL_EMERGENCY_NEEDLE))
        .filter_map(timestamp_prefix)
        .next()?;
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

/// Seconds between a `pmset -g log` timestamp and `now`.
fn seconds_since(timestamp: &str, now: SystemTime) -> Option<u64> {
    let epoch = u64::try_from(pmset_timestamp_epoch(timestamp)?).ok()?;
    let event = UNIX_EPOCH + Duration::from_secs(epoch);
    now.duration_since(event).ok().map(|d| d.as_secs())
}

/// Unix seconds for a `pmset -g log` timestamp, `YYYY-MM-DD HH:MM:SS ±HHMM`,
/// or `None` for anything else (including an impossible date).
///
/// Parsed in-process. This used to shell out to BSD `date -j -f`, which
/// made [`find_recent_thermal_emergency`] do I/O despite its contract, and
/// made it macOS-only in practice: GNU `date` has no `-j`, so on Linux every
/// timestamp read as unparseable and nothing was ever "recent". The civil
/// date arithmetic is Howard Hinnant's `days_from_civil`, a few lines, so
/// still no date dependency (CLAUDE.md: don't add dependencies casually).
fn pmset_timestamp_epoch(timestamp: &str) -> Option<i64> {
    let mut parts = timestamp.split_whitespace();
    let (date, time, offset) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let [year, month, day] = three_fields(date, '-')?;
    let [hour, minute, second] = three_fields(time, ':')?;
    let offset_seconds = utc_offset_seconds(offset)?;
    if !is_real_date(year, month, day) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let local = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second;
    Some(local - offset_seconds)
}

/// An unsigned run of ASCII digits. `str::parse::<i64>` alone would also
/// take a sign.
fn digits(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Exactly three `sep`-separated digit fields (`2026-07-10`, `23:16:05`).
fn three_fields(s: &str, sep: char) -> Option<[i64; 3]> {
    let mut it = s.split(sep).map(digits);
    let v = [it.next()??, it.next()??, it.next()??];
    it.next().is_none().then_some(v)
}

/// A `±HHMM` UTC offset, in seconds east of UTC.
fn utc_offset_seconds(offset: &str) -> Option<i64> {
    // Five ASCII characters, checked before any byte slicing (a multi-byte
    // character would otherwise be split and panic).
    if !offset.is_ascii() || offset.len() != 5 {
        return None;
    }
    let sign = match offset.as_bytes()[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (hours, minutes) = (digits(&offset[1..3])?, digits(&offset[3..5])?);
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(sign * (hours * 3_600 + minutes * 60))
}

/// Whether `year-month-day` is a day the Gregorian calendar has.
fn is_real_date(year: i64, month: i64, day: i64) -> bool {
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    (1..=12).contains(&month) && (1..=month_days[(month - 1) as usize]).contains(&day)
}

/// Days from 1970-01-01 to `year-month-day` (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::process::Command;
    use std::sync::OnceLock;

    fn run(cmd: &str, args: &[&str]) -> Option<String> {
        let out = Command::new(cmd).args(args).output().ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// The `pmset -g log` read, bounded AT THE SPAWN (`tail -n
    /// THERMAL_LOG_LINE_CAP`, piped via `sh -c` — `pmset` itself takes no
    /// line-count flag) and cached for the lifetime of the process
    /// (#2112 review finding 2): nothing about the log changes between two
    /// reads seconds apart, so a second caller in the same process reuses
    /// the first read rather than paying the ~0.8s spawn again.
    fn thermal_log_bounded() -> Option<String> {
        static CACHE: OnceLock<Option<String>> = OnceLock::new();
        CACHE
            .get_or_init(|| {
                // (#2112 review, second pass finding B) Without `set -o
                // pipefail`, `sh -c "pmset -g log | tail -n N"` reports
                // `tail`'s exit status, not `pmset`'s — a missing/broken
                // `pmset` still lets the pipeline exit 0 (`tail` on empty
                // stdin succeeds), so a real spawn FAILURE would silently
                // read as "no log, no emergency" rather than "unknown."
                // `pipefail` makes the pipeline fail if EITHER stage
                // fails. Empty-but-successful output is ALSO treated as
                // `None`: a real `pmset -g log` always prints at least its
                // "PM ASL data store" header line, so empty stdout means
                // the read didn't really happen, not that the log is
                // genuinely empty.
                let out = Command::new("sh")
                    .arg("-c")
                    .arg(format!("set -o pipefail; pmset -g log | tail -n {THERMAL_LOG_LINE_CAP}"))
                    .output()
                    .ok()?;
                if !out.status.success() {
                    return None;
                }
                let text = String::from_utf8_lossy(&out.stdout).into_owned();
                if text.trim().is_empty() {
                    None
                } else {
                    Some(text)
                }
            })
            .clone()
    }

    /// One full power-posture reading — every field independently `None`
    /// on a spawn failure or unparsed output. Costs roughly two fast
    /// `pmset` process spawns (~10-20ms each) plus the in-process thermal
    /// read, PLUS the `pmset -g log` scan (~0.8s) only when
    /// `always_scan_thermal_log` is true, the just-read thermal state is
    /// already `fair` or worse, or (#2112 review, second pass finding D)
    /// thermal reads nominal/unknown but the machine is ALREADY
    /// compromised another way (on battery, or Low Power Mode) — cheap
    /// (two pmset spawns already happened either way) and rare (most
    /// launches are neither), and it's exactly the case where an
    /// unnoticed recent thermal emergency is worth surfacing even though
    /// the CURRENT thermal reading alone wouldn't have triggered the scan.
    fn sample_impl(always_scan_thermal_log: bool) -> PowerPosture {
        let ps_text = run("pmset", &["-g", "ps"]);
        let source = ps_text.as_deref().and_then(parse_power_source);
        let battery_pct = ps_text.as_deref().and_then(parse_battery_pct);
        // (#1665 review MUST FIX 1) Captured once so both the parsed value
        // AND whether the underlying spawn itself failed are available —
        // `pmset_g_text.is_none()` means the process failed/exited
        // non-zero (genuinely unreadable); `Some(text)` with
        // `parse_low_power_mode` still returning `None` means the read
        // worked and neither key was present (a healthy Mac without the
        // feature).
        let pmset_g_text = run("pmset", &["-g"]);
        let low_power_mode = pmset_g_text.as_deref().and_then(parse_low_power_mode);
        let low_power_mode_unreadable = pmset_g_text.is_none();
        // (#2779) Through the resolved source, `read` only — a posture
        // reading is one-shot and must not advance a scenario's cursor.
        // See `host_source`'s module doc for the read/advance split.
        let thermal = crate::host_source::current().read().thermal;
        let mut posture = PowerPosture {
            source,
            battery_pct,
            low_power_mode,
            low_power_mode_unreadable,
            thermal,
            recent_thermal_emergency: None,
        };
        let should_scan_log = always_scan_thermal_log
            || severity(&posture).map(|s| s >= 1).unwrap_or(false)
            || matches!(posture.source, Some(PowerSource::Battery))
            || posture.low_power_mode == Some(true);
        posture.recent_thermal_emergency = if should_scan_log {
            thermal_log_bounded().and_then(|log| find_recent_thermal_emergency(&log, SystemTime::now()))
        } else {
            None
        };
        posture
    }

    /// `darkmux doctor`'s reading — always scans `pmset -g log`, since
    /// doctor is a read-only report meant to name a recent thermal
    /// emergency regardless of the CURRENT thermal state.
    pub fn sample() -> PowerPosture {
        sample_impl(true)
    }

    /// The mission/crawl pre-flight's reading (#2112 review finding 2) —
    /// skips the ~0.8s `pmset -g log` scan unless the thermal state this
    /// same call just read is already `fair` or worse, so a nominal
    /// machine's pre-flight costs two fast pmset spawns, not three
    /// (`darkmux doctor` keeps the always-on scan via [`sample`]).
    pub fn sample_for_preflight() -> PowerPosture {
        sample_impl(false)
    }
}

#[cfg(target_os = "macos")]
pub use imp::{sample, sample_for_preflight};

#[cfg(not(target_os = "macos"))]
pub fn sample() -> PowerPosture {
    PowerPosture {
        source: None,
        battery_pct: None,
        low_power_mode: None,
        low_power_mode_unreadable: false,
        thermal: None,
        recent_thermal_emergency: None,
    }
}

#[cfg(not(target_os = "macos"))]
pub fn sample_for_preflight() -> PowerPosture {
    sample()
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

    #[test]
    fn a_continuation_line_with_no_timestamp_does_not_hide_an_older_real_emergency() {
        // (#2112 review finding 7) The NEWEST matching line has no
        // parseable timestamp (a wrapped continuation line); the scan must
        // fall through to the next-newest match rather than returning
        // `None` for the whole search.
        let log = "\
2026-07-10 23:16:05 +0800 Sleep               \tEntering Sleep state due to 'Dark Wake Thermal Emergency': ...\n\
                            continued thermal emergency detail with no leading timestamp\n";
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(seconds_since_epoch_for_test("2026-07-10 23:16:05 +0800") + 60);
        let found = find_recent_thermal_emergency(log, now).expect("must fall through to the timestamped match");
        assert_eq!(found.at, "2026-07-10 23:16:05 +0800");
    }

    /// The fixtures' timestamps in Unix seconds, computed OUTSIDE this
    /// crate (BSD `date -j -f '%Y-%m-%d %H:%M:%S %z' <ts> +%s` and Python's
    /// `datetime.strptime(ts, '%Y-%m-%d %H:%M:%S %z').timestamp()` agree), so
    /// the oracle is neither the parser under test nor a platform's `date`.
    /// It used to shell out to BSD `date -j` here, which does not exist on
    /// Linux.
    fn seconds_since_epoch_for_test(ts: &str) -> u64 {
        match ts {
            "2026-07-11 07:09:50 +0800" => 1_783_724_990,
            "2026-07-10 23:16:05 +0800" => 1_783_696_565,
            other => panic!("no precomputed epoch for fixture timestamp {other:?}"),
        }
    }

    #[test]
    fn pmset_timestamps_parse_to_unix_seconds() {
        assert_eq!(pmset_timestamp_epoch("2026-07-11 07:09:50 +0800"), Some(1_783_724_990));
        assert_eq!(pmset_timestamp_epoch("2026-07-10 23:16:05 +0800"), Some(1_783_696_565));
        // Same instant as the first, written in another offset (and a
        // negative one): 2026-07-10 23:09:50 UTC.
        assert_eq!(pmset_timestamp_epoch("2026-07-10 23:09:50 +0000"), Some(1_783_724_990));
        assert_eq!(pmset_timestamp_epoch("2026-07-10 18:39:50 -0430"), Some(1_783_724_990));
        assert_eq!(pmset_timestamp_epoch("1970-01-01 00:00:00 +0000"), Some(0));
        // 2024-02-29 00:00:00 UTC is a real day (leap year); 2026 has none.
        assert_eq!(pmset_timestamp_epoch("2024-02-29 00:00:00 +0000"), Some(1_709_164_800));
        assert_eq!(pmset_timestamp_epoch("2026-02-29 00:00:00 +0000"), None);
        assert_eq!(pmset_timestamp_epoch("2026-13-01 00:00:00 +0000"), None);
        assert_eq!(pmset_timestamp_epoch("2026-07-10 24:00:00 +0000"), None);
        assert_eq!(pmset_timestamp_epoch("2026-07-10 23:16:05 0800"), None);
        assert_eq!(pmset_timestamp_epoch("2026-07-10 23:16:05"), None);
        assert_eq!(pmset_timestamp_epoch("2026-07-10 23:16 +0800"), None);
        assert_eq!(pmset_timestamp_epoch("2026-07-10 23:16:05 +0800 trailing"), None);
        assert_eq!(pmset_timestamp_epoch("2026-07-10 23:16:05 +08000"), None);
        // Five bytes but not five ASCII characters: refused before any byte
        // slicing, which would otherwise split the `é` and panic.
        assert_eq!(pmset_timestamp_epoch("2026-07-10 23:16:05 +0é0"), None);
        // A sign is not a digit, though `str::parse::<i64>` accepts one.
        assert_eq!(pmset_timestamp_epoch("2026-+7-10 23:16:05 +0800"), None);
        // Century years are leap years only when divisible by 400.
        assert_eq!(pmset_timestamp_epoch("2000-02-29 00:00:00 +0000"), Some(951_782_400));
        assert_eq!(pmset_timestamp_epoch("2100-02-29 00:00:00 +0000"), None);
    }

    #[test]
    fn severity_ranks_every_named_state_and_treats_unrecognized_as_past_critical() {
        let at = |state: &str| {
            severity(&PowerPosture {
                source: None,
                battery_pct: None,
                low_power_mode: None,
                low_power_mode_unreadable: false,
                thermal: Some(ThermalSample { state: state.into(), cpu_speed_limit_pct: 100 }),
                recent_thermal_emergency: None,
            })
        };
        assert_eq!(at("nominal"), Some(0));
        assert_eq!(at("fair"), Some(1));
        assert_eq!(at("serious"), Some(2));
        assert_eq!(at("critical"), Some(3));
        assert_eq!(at("apocalyptic"), Some(4), "unrecognized state must sort PAST critical, not be dropped");
    }

    #[test]
    fn severity_is_none_when_there_is_no_thermal_sample_at_all() {
        let p = PowerPosture {
            source: None,
            battery_pct: None,
            low_power_mode: None,
            low_power_mode_unreadable: false,
            thermal: None,
            recent_thermal_emergency: None,
        };
        assert_eq!(severity(&p), None, "unreadable thermal (e.g. Intel Mac) must not read as any known severity");
    }

    // (#2112 review, second pass finding B) Direct proof of the shell
    // construct `imp::thermal_log_bounded` relies on — exercised here
    // rather than against the private function itself, since this is
    // characterizing `sh`'s own pipefail semantics, not this crate's
    // logic.
    #[cfg(target_os = "macos")]
    #[test]
    fn pipefail_makes_a_missing_upstream_command_fail_the_whole_pipeline() {
        let missing_cmd = "definitely-not-a-real-command-darkmux-2112-review";

        let without_pipefail = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{missing_cmd} | tail -n 1"))
            .output()
            .expect("sh must run");
        assert!(
            without_pipefail.status.success(),
            "sanity: without pipefail, `tail` on empty stdin succeeds even though the upstream command doesn't exist"
        );

        let with_pipefail = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("set -o pipefail; {missing_cmd} | tail -n 1"))
            .output()
            .expect("sh must run");
        assert!(
            !with_pipefail.status.success(),
            "pipefail must propagate the missing upstream command's failure through the pipeline"
        );
    }
}
