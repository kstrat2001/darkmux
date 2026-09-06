//! Records-emitted aggregation for the mission envelope (#2421).
//!
//! At mission finalize, [`RecordsEmitted`] — counts of the mission's own flow
//! records by `action`, total bytes on disk, aggregate dispatch time, wall
//! time, and host-telemetry sample coverage — rides
//! `MissionEnvelope::records_emitted` (`MISSION_ENVELOPE_SCHEMA` 1.2 -> 1.3,
//! additive) so a run is self-describing about its own stream cost the same
//! way it already carries its staffing snapshot (`FunnelEnvelope::staffing`,
//! CLAUDE.md's `DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION` doctrine — a
//! resolved run-shape snapshot lives ON the envelope rather than requiring a
//! reader to re-derive it). `darkmux mission debrief` renders the persisted
//! block.
//!
//! **Why this lives in `darkmux-crew`, not `darkmux-serve`.**
//! `envelope::finalize_mission_with_payload` — the ONE call-through point
//! every mission driver's finalize goes through — lives in THIS crate, and
//! `darkmux-crew` does not (and per the workspace `Cargo.toml`, structurally
//! cannot) depend on `darkmux-serve` — the dependency runs the other way
//! (`darkmux-serve` depends on `darkmux-crew`). `darkmux-serve`'s own
//! day-file readers (`mission_graph::backfill_step_finals`,
//! `runs::is_host_sample_in_window`) are therefore unreachable from finalize.
//! This module follows the SAME day-file-scanning shape
//! `darkmux-crew::index::derive_cautions` already established in this crate
//! (walk `flows_dir()`, parse each line as a `FlowRecord`, skip anything that
//! doesn't parse) — duplicated rather than shared, matching how
//! `days_from_civil`/`parse_flow_ts` are already independently re-derived in
//! both `darkmux-serve::mission_graph` and `darkmux-serve::runs` (see that
//! module's own doc on why: no shared cross-crate day-file/timestamp API
//! exists to call instead).
//!
//! The counting logic itself ([`aggregate_records_emitted`]) is pure and
//! disk-free — unit-testable without touching a filesystem — with a thin
//! disk-scanning wrapper ([`records_emitted_for_mission`]) around it.

use darkmux_flow::{is_dispatch_start, is_dispatch_terminal, FlowRecord};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One mission's stream-cost summary — see the module doc. Always present
/// (never skipped) on an envelope finalized by a binary carrying this
/// module; `total_records: 0` on a MISS (see
/// [`records_emitted_for_mission`]'s doc) rather than an absent block, per
/// the operator's 2026-09-06 rule that a wrong-or-missing key must leave a
/// warning to follow, not a silent gap.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RecordsEmitted {
    /// Count of this mission's own flow records, keyed by their `action`
    /// string. A `BTreeMap` (not a `HashMap`) so both JSON output and the
    /// debrief's "top actions" rendering are deterministically ordered.
    #[serde(default)]
    pub by_action: BTreeMap<String, u64>,
    /// Total flow records carrying this mission's `mission_id`, across every
    /// day file scanned.
    #[serde(default)]
    pub total_records: u64,
    /// Sum of the on-disk byte length of every JSONL line counted into
    /// `total_records` (the line's raw text, not the record re-serialized).
    #[serde(default)]
    pub total_bytes: u64,
    /// Sum of (terminal ts − start ts) over the mission's dispatch bookend
    /// pairs, paired by `session_id`. An unpaired ("open") start counts to
    /// the finalize-time clock passed to [`aggregate_records_emitted`].
    #[serde(default)]
    pub dispatch_seconds: f64,
    /// Last record ts − first record ts, over this mission's own records
    /// only. `0.0` when fewer than one timestamp was parseable (including
    /// the MISS case: no records at all).
    #[serde(default)]
    pub wall_seconds: f64,
    /// Count of `machine.telemetry` host-sample records (#2413 — machine-
    /// scoped, carrying no `mission_id`) whose `machine_uid` matches this
    /// mission's own (the machine_uid of its first record that carries one)
    /// and whose `ts` falls inside `[first_ts, last_ts]` (the same window
    /// `wall_seconds` measures).
    #[serde(default)]
    pub host_samples_in_window: u64,
}

/// Current wall-clock as epoch seconds. `std`-only (no new dependency) —
/// used both as the default "finalize time" for an open dispatch bookend and
/// to bound the day-file scan window.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Days since the Unix epoch for a UTC civil date — Howard Hinnant's
/// algorithm (public domain). Independently re-derived here rather than
/// shared cross-crate; see the module doc.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y / 400 } else { (y - 399) / 400 };
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Parse a `FlowRecord.ts` string (`YYYY-MM-DDTHH:MM:SSZ`, fixed-width) into
/// epoch seconds. `None` on anything that doesn't match the exact shape — a
/// malformed/absent ts degrades to "no flow-derived timestamp", never a
/// panic. Mirrors `darkmux-serve::runs::parse_flow_ts`.
fn parse_ts_secs(ts: &str) -> Option<i64> {
    let b = ts.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' || b[19] != b'Z'
    {
        return None;
    }
    let y: i64 = ts.get(0..4)?.parse().ok()?;
    let mo: i64 = ts.get(5..7)?.parse().ok()?;
    let d: i64 = ts.get(8..10)?.parse().ok()?;
    let h: i64 = ts.get(11..13)?.parse().ok()?;
    let mi: i64 = ts.get(14..16)?.parse().ok()?;
    let s: i64 = ts.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + s)
}

/// Parse a day-file stem (`YYYY-MM-DD`) into epoch days. `None` on anything
/// that doesn't match the exact shape.
fn day_stem_to_epoch_days(stem: &str) -> Option<i64> {
    let b = stem.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = stem.get(0..4)?.parse().ok()?;
    let m: i64 = stem.get(5..7)?.parse().ok()?;
    let d: i64 = stem.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// One day of margin below `date(mission_created_ts)` on the day-file scan
/// window — absorbs timezone/rollover skew between the clock that stamped
/// `created_ts` and the UTC day the flow sink named its file after. Mirrors
/// `darkmux-serve::mission_graph::BACKFILL_DAY_MARGIN_DAYS`.
const DAY_MARGIN_DAYS: i64 = 1;

/// Pure aggregation core (#2421). `lines` is every flow record parsed from
/// the day file(s) scanned, paired with that line's raw on-disk byte length,
/// in scan order (day files sorted, then file order — i.e. non-decreasing
/// `ts` in the common case, though nothing here assumes strict ordering).
/// `mission_id` selects this mission's own records out of a day file that
/// may hold many missions' interleaved records; `finalize_secs` is the
/// finalize-time clock, used to close an OPEN dispatch bookend (a `dispatch
/// start` with no matching terminal) at "now" rather than dropping it.
///
/// Bookends pair by `session_id` and recognize BOTH action spellings via
/// [`darkmux_flow::is_dispatch_start`]/[`darkmux_flow::is_dispatch_terminal`]
/// — never a literal `"dispatch start"` comparison (#2425: two producer
/// lineages spell the bookends differently).
///
/// The host-telemetry join is two-pass over the already-in-memory `lines`
/// (no second disk read): pass 1 (the loop below) resolves the mission's own
/// wall window (`first_ts..=last_ts`) and its `machine_uid` (the first value
/// any of its own records carries); pass 2 then counts `machine.telemetry`
/// samples matching that machine_uid inside that window (#2413 made those
/// records machine-scoped — carrying no `mission_id` — so time+machine is
/// the only available join key).
pub fn aggregate_records_emitted(lines: &[(FlowRecord, u64)], mission_id: &str, finalize_secs: i64) -> RecordsEmitted {
    let mut by_action: BTreeMap<String, u64> = BTreeMap::new();
    let mut total_records: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut first_ts: Option<i64> = None;
    let mut last_ts: Option<i64> = None;
    let mut machine_uid: Option<String> = None;
    let mut open_starts: BTreeMap<String, i64> = BTreeMap::new();
    let mut dispatch_seconds: f64 = 0.0;

    for (rec, line_bytes) in lines {
        if rec.mission_id.as_deref() != Some(mission_id) {
            continue;
        }
        total_records += 1;
        total_bytes += line_bytes;
        *by_action.entry(rec.action.clone()).or_insert(0) += 1;

        let ts = parse_ts_secs(&rec.ts);
        if let Some(ts) = ts {
            first_ts = Some(first_ts.map_or(ts, |f| f.min(ts)));
            last_ts = Some(last_ts.map_or(ts, |l| l.max(ts)));
        }
        if machine_uid.is_none() {
            machine_uid = rec.machine_uid.clone();
        }

        if is_dispatch_start(&rec.action) {
            if let (Some(sid), Some(ts)) = (rec.session_id.clone(), ts) {
                open_starts.insert(sid, ts);
            }
        } else if is_dispatch_terminal(&rec.action) {
            if let Some(sid) = &rec.session_id {
                if let Some(start_ts) = open_starts.remove(sid) {
                    if let Some(ts) = ts {
                        dispatch_seconds += (ts - start_ts).max(0) as f64;
                    }
                }
            }
        }
    }

    // Anything left in `open_starts` never saw a terminal bookend — an OPEN
    // dispatch, counted to the finalize-time clock (never dropped).
    for start_ts in open_starts.values() {
        dispatch_seconds += (finalize_secs - start_ts).max(0) as f64;
    }

    let wall_seconds = match (first_ts, last_ts) {
        (Some(f), Some(l)) => (l - f).max(0) as f64,
        _ => 0.0,
    };

    let mut host_samples_in_window: u64 = 0;
    if let (Some(mu), Some(f), Some(l)) = (machine_uid.as_deref(), first_ts, last_ts) {
        for (rec, _bytes) in lines {
            if rec.action != "machine.telemetry" {
                continue;
            }
            if rec.machine_uid.as_deref() != Some(mu) {
                continue;
            }
            if let Some(ts) = parse_ts_secs(&rec.ts) {
                if ts >= f && ts <= l {
                    host_samples_in_window += 1;
                }
            }
        }
    }

    RecordsEmitted { by_action, total_records, total_bytes, dispatch_seconds, wall_seconds, host_samples_in_window }
}

/// Disk-scanning wrapper around [`aggregate_records_emitted`] — the ONLY
/// non-pure entry point in this module. Scans `.jsonl` day files under
/// `darkmux_flow::flows_dir()` whose `YYYY-MM-DD` stem is on/after
/// `date(mission_created_ts) - DAY_MARGIN_DAYS` (mirrors the day-file
/// windowing `darkmux-serve::mission_graph::backfill_step_finals` already
/// established one layer up). A malformed/unreadable directory or file, or a
/// line that doesn't parse as a `FlowRecord`, is skipped — best-effort, same
/// discipline as `darkmux-crew::index::derive_cautions`.
///
/// Returns the aggregated block plus the list of day-file stems actually
/// searched, so a MISS warning at the call site can name exactly what was
/// looked at.
pub fn records_emitted_for_mission(mission_id: &str, mission_created_ts: u64, finalize_secs: i64) -> (RecordsEmitted, Vec<String>) {
    let dir = darkmux_flow::flows_dir();
    let mut day_files_searched: Vec<String> = Vec::new();
    let mut lines: Vec<(FlowRecord, u64)> = Vec::new();

    if let Ok(rd) = std::fs::read_dir(&dir) {
        let min_day = (mission_created_ts as i64) / 86_400 - DAY_MARGIN_DAYS;
        let mut day_paths: Vec<(String, std::path::PathBuf)> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter_map(|p| {
                if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    return None;
                }
                let stem = p.file_stem()?.to_str()?.to_string();
                let days = day_stem_to_epoch_days(&stem)?;
                if days >= min_day {
                    Some((stem, p))
                } else {
                    None
                }
            })
            .collect();
        day_paths.sort();

        for (stem, path) in day_paths {
            day_files_searched.push(stem);
            if let Ok(text) = std::fs::read_to_string(&path) {
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    if let Ok(rec) = serde_json::from_str::<FlowRecord>(line) {
                        lines.push((rec, line.len() as u64));
                    }
                }
            }
        }
    }

    let emitted = aggregate_records_emitted(&lines, mission_id, finalize_secs);
    (emitted, day_files_searched)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `FlowRecord` builder — every field the aggregation ignores
    /// gets a neutral default, keeping each test's literal focused on the
    /// fields the assertion actually cares about.
    #[allow(clippy::too_many_arguments)]
    fn rec(
        ts: &str,
        action: &str,
        mission_id: Option<&str>,
        session_id: Option<&str>,
        machine_uid: Option<&str>,
    ) -> FlowRecord {
        FlowRecord {
            ts: ts.to_string(),
            level: darkmux_flow::Level::Info,
            category: darkmux_flow::Category::Work,
            tier: darkmux_flow::Tier::Local,
            stage: darkmux_flow::Stage::Dispatch,
            action: action.to_string(),
            handle: "role".to_string(),
            phase_id: None,
            session_id: session_id.map(String::from),
            source: None,
            model: None,
            reasoning: None,
            mission_id: mission_id.map(String::from),
            machine_id: None,
            machine_uid: machine_uid.map(String::from),
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        }
    }

    fn line(r: FlowRecord) -> (FlowRecord, u64) {
        let bytes = serde_json::to_string(&r).unwrap().len() as u64;
        (r, bytes)
    }

    // ── aggregate_records_emitted — pure core ───────────────────────────

    #[test]
    fn counts_only_the_target_missions_records_from_an_interleaved_file() {
        let lines = vec![
            line(rec("2023-11-14T10:00:00Z", "dispatch start", Some("m1"), Some("s1"), None)),
            line(rec("2023-11-14T10:00:00Z", "dispatch start", Some("m2"), Some("s2"), None)),
            line(rec("2023-11-14T10:00:05Z", "dispatch complete", Some("m1"), Some("s1"), None)),
            line(rec("2023-11-14T10:00:07Z", "dispatch complete", Some("m2"), Some("s2"), None)),
            line(rec("2023-11-14T10:00:08Z", "dispatch.turn", Some("m1"), Some("s1"), None)),
        ];
        let got = aggregate_records_emitted(&lines, "m1", 0);
        assert_eq!(got.total_records, 3, "only m1's 3 records, not all 5");
        assert_eq!(got.by_action.get("dispatch start"), Some(&1));
        assert_eq!(got.by_action.get("dispatch complete"), Some(&1));
        assert_eq!(got.by_action.get("dispatch.turn"), Some(&1));
        assert_eq!(got.by_action.get("dispatch.turn").copied(), Some(1));
        // m2's records must not leak into m1's counts at all.
        assert_eq!(got.by_action.values().sum::<u64>(), 3);
    }

    #[test]
    fn bookends_pair_by_session_id_across_both_spellings() {
        // Start uses the SPACED form, terminal uses the DOTTED form —
        // #2425's exact "a literal is a bug" scenario.
        let lines = vec![
            line(rec("2023-11-14T10:00:00Z", "dispatch start", Some("m1"), Some("s1"), None)),
            line(rec("2023-11-14T10:00:10Z", "dispatch.complete", Some("m1"), Some("s1"), None)),
            // A second pair, spellings reversed the other way.
            line(rec("2023-11-14T10:01:00Z", "dispatch.start", Some("m1"), Some("s2"), None)),
            line(rec("2023-11-14T10:01:20Z", "dispatch error", Some("m1"), Some("s2"), None)),
        ];
        let got = aggregate_records_emitted(&lines, "m1", 0);
        assert_eq!(got.dispatch_seconds, 10.0 + 20.0);
    }

    #[test]
    fn an_open_dispatch_counts_to_the_finalize_time() {
        let lines = vec![line(rec("2023-11-14T10:00:00Z", "dispatch start", Some("m1"), Some("s1"), None))];
        // Finalize happens 42s after the (never-terminated) start.
        let finalize_secs = parse_ts_secs_pub("2023-11-14T10:00:42Z");
        let got = aggregate_records_emitted(&lines, "m1", finalize_secs);
        assert_eq!(got.dispatch_seconds, 42.0);
    }

    fn parse_ts_secs_pub(ts: &str) -> i64 {
        parse_ts_secs(ts).unwrap()
    }

    #[test]
    fn host_samples_counted_only_inside_window_and_only_for_the_missions_machine() {
        let lines = vec![
            line(rec("2023-11-14T10:00:00Z", "dispatch start", Some("m1"), Some("s1"), Some("mac-1"))),
            line(rec("2023-11-14T10:01:00Z", "dispatch complete", Some("m1"), Some("s1"), Some("mac-1"))),
            // Inside the window, matching machine — counted.
            line(rec("2023-11-14T10:00:30Z", "machine.telemetry", None, None, Some("mac-1"))),
            // Inside the window, WRONG machine — not counted.
            line(rec("2023-11-14T10:00:31Z", "machine.telemetry", None, None, Some("mac-2"))),
            // Outside the window (before first_ts) — not counted.
            line(rec("2023-11-14T09:59:00Z", "machine.telemetry", None, None, Some("mac-1"))),
            // Outside the window (after last_ts) — not counted.
            line(rec("2023-11-14T10:02:00Z", "machine.telemetry", None, None, Some("mac-1"))),
        ];
        let got = aggregate_records_emitted(&lines, "m1", 0);
        assert_eq!(got.host_samples_in_window, 1);
    }

    #[test]
    fn wall_seconds_is_last_minus_first_ts_of_the_missions_own_records() {
        let lines = vec![
            line(rec("2023-11-14T10:00:00Z", "dispatch start", Some("m1"), Some("s1"), None)),
            line(rec("2023-11-14T10:05:00Z", "dispatch.turn", Some("m1"), Some("s1"), None)),
            line(rec("2023-11-14T10:10:00Z", "dispatch complete", Some("m1"), Some("s1"), None)),
        ];
        let got = aggregate_records_emitted(&lines, "m1", 0);
        assert_eq!(got.wall_seconds, 600.0);
    }

    #[test]
    fn no_records_for_the_mission_is_an_honest_all_zero_block() {
        let lines = vec![line(rec("2023-11-14T10:00:00Z", "dispatch start", Some("other"), Some("s1"), None))];
        let got = aggregate_records_emitted(&lines, "m1", 0);
        assert_eq!(got, RecordsEmitted::default());
    }

    // ── parse_ts_secs / day_stem_to_epoch_days — small parsers ──────────

    #[test]
    fn parse_ts_secs_epoch_zero() {
        assert_eq!(parse_ts_secs("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn parse_ts_secs_rejects_malformed() {
        assert_eq!(parse_ts_secs("not-a-timestamp"), None);
        assert_eq!(parse_ts_secs(""), None);
    }

    #[test]
    fn day_stem_round_trips_a_known_date() {
        // 1_700_000_000 -> 2023-11-14 (verified via `date -u -r`).
        assert_eq!(day_stem_to_epoch_days("2023-11-14"), Some(1_700_000_000 / 86_400));
    }
}

#[cfg(test)]
mod cost_check {
    use super::*;

    /// (#2421 self-QA cost check) Not a correctness assertion — prints the
    /// wall-clock cost of `aggregate_records_emitted` over a 60k-line
    /// synthetic day file, the size named in the ticket's own #2413 baseline
    /// (26,823 records / 15.9 MB). `finalize` runs this once per mission
    /// close, never on a hot path, but the number is worth having on
    /// record. Run with `--nocapture` to see the printed line; the disk-
    /// scanning wrapper's own I/O is deliberately NOT measured here (a
    /// separate, environment-dependent cost) — this isolates the pure
    /// in-memory aggregation.
    #[test]
    fn aggregate_records_emitted_cost_on_60k_lines() {
        let mut lines: Vec<(FlowRecord, u64)> = Vec::with_capacity(60_000);
        // ~90% belongs to the target mission (like the #2413 baseline's
        // dominant telemetry.process share), interleaved with a sibling
        // mission's records and a spread of dispatch bookends + host samples.
        for i in 0..60_000u32 {
            let sec = i % 60;
            let ts = format!("2023-11-14T10:{:02}:{:02}Z", (i / 3600) % 60, sec);
            let mission = if i % 10 == 0 { "sibling" } else { "m-cost" };
            let (action, session) = match i % 50 {
                0 => ("dispatch start", Some(format!("s{}", i / 50))),
                1 => ("dispatch.complete", Some(format!("s{}", i / 50))),
                _ => ("dispatch.turn", Some(format!("s{}", i / 50))),
            };
            lines.push((
                FlowRecord {
                    ts,
                    level: darkmux_flow::Level::Info,
                    category: darkmux_flow::Category::Work,
                    tier: darkmux_flow::Tier::Local,
                    stage: darkmux_flow::Stage::Dispatch,
                    action: action.to_string(),
                    handle: "coder".to_string(),
                    phase_id: None,
                    session_id: session,
                    source: None,
                    model: None,
                    reasoning: None,
                    mission_id: Some(mission.to_string()),
                    machine_id: None,
                    machine_uid: Some("mac-cost".to_string()),
                    prev_hash: None,
                    hash: None,
                    payload: None,
                    work_id: None,
                    attempt: None,
                },
                80,
            ));
        }
        let start = std::time::Instant::now();
        let got = aggregate_records_emitted(&lines, "m-cost", 0);
        let elapsed = start.elapsed();
        println!(
            "#2421 cost check: aggregate_records_emitted over {} lines ({} matched `m-cost`) took {:?}",
            lines.len(),
            got.total_records,
            elapsed
        );
        assert!(got.total_records > 0);
    }
}
