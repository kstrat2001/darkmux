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
    /// Flow records carrying this mission's `mission_id`, AT THE MOMENT OF
    /// THIS FINALIZE — across every day file scanned. Not the mission's
    /// lifetime total: `finalize_mission_with_payload` computes this block
    /// AFTER driving the phase/mission transitions but BEFORE the launch's
    /// own wrapper closes out (`src/mission_launch.rs`'s outer `dispatch
    /// start`/`dispatch complete` bookend around the whole `launch()` call,
    /// and its post-finalize cmd-audit record) — those records are written
    /// to the day file strictly AFTER this count is taken and are
    /// structurally outside the block. A re-finalize (idempotent re-close)
    /// would see a slightly larger count next time; this field is "records
    /// on disk as of this finalize," not a promise of eventual completeness.
    #[serde(default)]
    pub total_records: u64,
    /// Sum of the on-disk byte length of every JSONL line counted into
    /// `total_records` — the line's raw text (its `serde_json` encoding as
    /// written), EXCLUDING the trailing newline the file separates lines
    /// with, not the record re-serialized by this reader.
    #[serde(default)]
    pub total_bytes: u64,
    /// Sum of (terminal ts − start ts) over the mission's own dispatch
    /// bookend pairs, paired by `session_id`. Excludes the LAUNCH's own
    /// wrapper liveness bookend (`source == "mission"` —
    /// `mission_bookend_record` in `src/mission_launch.rs`, opened around
    /// the whole `launch()` call and closed only after finalize returns):
    /// that bookend is liveness for the launch invocation, not seat work,
    /// and counting it would inflate this field by the mission's entire
    /// wall time every time (it always reads as OPEN at aggregation time,
    /// since finalize runs strictly before the wrapper's own terminal
    /// record is written). An unpaired ("open") seat dispatch — including
    /// one superseded by a second `dispatch start` on the same
    /// `session_id` before ever seeing a terminal — counts to the
    /// finalize-time clock passed to [`aggregate_records_emitted`]; see
    /// `open_dispatches`.
    #[serde(default)]
    pub dispatch_seconds: f64,
    /// Count of dispatch bookend pairs actually matched (a `dispatch
    /// start` paired with a later terminal on the same `session_id`) —
    /// contributes to `dispatch_seconds`. Distinguishes "zero dispatch
    /// seconds because there were no dispatches" (`dispatch_pairs == 0 &&
    /// open_dispatches == 0`) from "zero because every one is still open"
    /// (`open_dispatches > 0`).
    #[serde(default)]
    pub dispatch_pairs: u64,
    /// Count of dispatch starts that never saw a matching terminal — each
    /// credited to `dispatch_seconds` via the finalize-time clock rather
    /// than dropped. Includes both a start still open when the scan ends
    /// AND an earlier start superseded by a second `dispatch start` on the
    /// same `session_id` (flushed to finalize time at the moment of the
    /// second start, since no later terminal can retroactively close it).
    /// The launch's own wrapper bookend is EXCLUDED from this count too
    /// (see `dispatch_seconds`'s doc) — it never reaches `open_dispatches`
    /// even though it always reads as unterminated at aggregation time.
    #[serde(default)]
    pub open_dispatches: u64,
    /// Last record ts − first record ts, over this mission's own records
    /// only. `0.0` when fewer than one timestamp was parseable (including
    /// the MISS case: no records at all).
    #[serde(default)]
    pub wall_seconds: f64,
    /// The `machine_uid` used for the `host_samples_in_window` join — the
    /// first non-`None` `machine_uid` carried by any of this mission's own
    /// matched records. `None` when NONE of them carry one (pre-#640
    /// records, or a non-macOS emitter), in which case
    /// `host_samples_in_window` is honestly `0` (nothing to join against)
    /// and the caller surfaces a `warnings` entry rather than letting that
    /// zero look identical to "the host sampler simply wasn't running." A
    /// mission whose dispatches span MORE than one machine still counts
    /// only THIS one's samples — cross-machine host-sample attribution is
    /// a future enhancement if a real multi-machine mission needs it.
    #[serde(default)]
    pub machine_uid: Option<String>,
    /// Count of `machine.telemetry` host-sample records (#2413 — machine-
    /// scoped, carrying no `mission_id`) whose `machine_uid` matches
    /// `machine_uid` above and whose `ts` falls inside `[first_ts,
    /// last_ts]` (the same window `wall_seconds` measures).
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
/// **The launch's own wrapper bookend is excluded from pairing (#2426 round
/// 2 MF1).** `mission_bookend_record` (`src/mission_launch.rs`) opens a
/// `source == "mission"` `dispatch start`/`dispatch complete` pair around
/// the WHOLE `launch()` call, and `finalize_mission_with_payload` runs
/// strictly INSIDE that pair — so at aggregation time the wrapper's own
/// start is always still open, and treating it like a seat dispatch would
/// credit the mission's entire wall time into `dispatch_seconds` on every
/// finalize. A record with `source == Some("mission")` still counts toward
/// `total_records`/`by_action`/the wall window/the machine_uid resolution —
/// it's a real record this mission emitted — it just never opens or closes
/// a bookend pair.
///
/// **A repeated `dispatch start` on the same `session_id` with no terminal
/// in between** does not silently overwrite the earlier one: the earlier
/// segment is flushed to `finalize_secs` (same treatment as a genuinely
/// open dispatch — see `RecordsEmitted::open_dispatches`'s doc) before the
/// new start is tracked, so neither segment's time is lost to the
/// overwrite.
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
    let mut dispatch_pairs: u64 = 0;
    let mut open_dispatches: u64 = 0;

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

        // The launch's own liveness wrapper never opens/closes a bookend
        // pair — see this function's doc.
        let is_wrapper_bookend = rec.source.as_deref() == Some("mission");
        if is_wrapper_bookend {
            continue;
        }

        if is_dispatch_start(&rec.action) {
            if let (Some(sid), Some(ts)) = (rec.session_id.clone(), ts) {
                if let Some(prev_start) = open_starts.insert(sid, ts) {
                    // A repeat start for this session with no terminal in
                    // between — flush the EARLIER segment to finalize time
                    // rather than losing it to the overwrite.
                    dispatch_seconds += (finalize_secs - prev_start).max(0) as f64;
                    open_dispatches += 1;
                }
            }
        } else if is_dispatch_terminal(&rec.action) {
            if let Some(sid) = &rec.session_id {
                if let Some(start_ts) = open_starts.remove(sid) {
                    dispatch_pairs += 1;
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
        open_dispatches += 1;
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

    RecordsEmitted {
        by_action,
        total_records,
        total_bytes,
        dispatch_seconds,
        dispatch_pairs,
        open_dispatches,
        wall_seconds,
        machine_uid,
        host_samples_in_window,
    }
}

/// Disk-scanning wrapper around [`aggregate_records_emitted`] — the ONLY
/// non-pure entry point in this module. Scans `.jsonl` day files under
/// `darkmux_flow::flows_dir()` whose `YYYY-MM-DD` stem is on/after
/// `date(mission_created_ts) - DAY_MARGIN_DAYS` and on/before
/// `date(finalize_secs)` (mirrors the day-file windowing
/// `darkmux-serve::mission_graph::backfill_step_finals` already established
/// one layer up; the upper bound is new in #2426 round 2 — a mission cannot
/// have records in a day file dated after the moment finalize runs). A
/// malformed/unreadable directory or file, or a line that doesn't parse as
/// a `FlowRecord`, is skipped — best-effort, same discipline as
/// `darkmux-crew::index::derive_cautions`.
///
/// **Streams, never loads a whole day file (#2426 round 2 MF/(5)).** Every
/// bare `darkmux dispatch` pays this path (`dispatch_as_crew_of_one.rs`),
/// and a day file can be tens of megabytes carrying many missions'
/// interleaved records — a `read_to_string` + parse-every-line pass over
/// the whole thing was measured at ~370ms on a 44 MB / 60k-line synthetic
/// file (reviewer figure: ~380ms). This reads with a `BufReader` line by
/// line and pre-filters on a cheap substring test — `line.contains(
/// mission_id) || line.contains("machine.telemetry")` — BEFORE paying for
/// `serde_json::from_str`; the substring test is only a pre-filter (a false
/// positive just means one wasted parse, never a wrong answer — the exact
/// field checks below and inside [`aggregate_records_emitted`] are the
/// actual authority), and after parsing, a record is pushed into `lines`
/// only if it's genuinely this mission's own (`mission_id` matches) or a
/// machine-scoped host sample (`action == "machine.telemetry"`, needed for
/// the host-sample join) — every other mission's records are read,
/// filtered, and dropped without ever being retained in memory.
///
/// Returns the aggregated block plus the list of day-file stems actually
/// searched, so a MISS warning at the call site can name exactly what was
/// looked at.
pub fn records_emitted_for_mission(mission_id: &str, mission_created_ts: u64, finalize_secs: i64) -> (RecordsEmitted, Vec<String>) {
    use std::io::BufRead;

    let dir = darkmux_flow::flows_dir();
    let mut day_files_searched: Vec<String> = Vec::new();
    let mut lines: Vec<(FlowRecord, u64)> = Vec::new();

    if let Ok(rd) = std::fs::read_dir(&dir) {
        let min_day = (mission_created_ts as i64) / 86_400 - DAY_MARGIN_DAYS;
        let max_day = finalize_secs.div_euclid(86_400);
        let mut day_paths: Vec<(String, std::path::PathBuf)> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter_map(|p| {
                if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    return None;
                }
                let stem = p.file_stem()?.to_str()?.to_string();
                let days = day_stem_to_epoch_days(&stem)?;
                if days >= min_day && days <= max_day {
                    Some((stem, p))
                } else {
                    None
                }
            })
            .collect();
        day_paths.sort();

        for (stem, path) in day_paths {
            day_files_searched.push(stem);
            let Ok(file) = std::fs::File::open(&path) else { continue };
            for line in std::io::BufReader::new(file).lines() {
                let Ok(raw) = line else { continue };
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    continue;
                }
                // Cheap pre-filter — see this function's doc. Neither
                // substring needs to appear for this line to be
                // structurally irrelevant to this call.
                if !trimmed.contains(mission_id) && !trimmed.contains("machine.telemetry") {
                    continue;
                }
                let Ok(rec) = serde_json::from_str::<FlowRecord>(trimmed) else { continue };
                if rec.mission_id.as_deref() == Some(mission_id) || rec.action == "machine.telemetry" {
                    lines.push((rec, trimmed.len() as u64));
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
    /// fields the assertion actually cares about. `source` defaults to
    /// `None` (a seat dispatch); use [`rec_src`] for a test that needs to
    /// name the launch wrapper's `source == "mission"`.
    fn rec(
        ts: &str,
        action: &str,
        mission_id: Option<&str>,
        session_id: Option<&str>,
        machine_uid: Option<&str>,
    ) -> FlowRecord {
        rec_src(ts, action, mission_id, session_id, machine_uid, None)
    }

    /// [`rec`] with an explicit `source` — for the wrapper-bookend-exclusion
    /// tests (#2426 round 2 MF1), which need `source == Some("mission")`.
    fn rec_src(
        ts: &str,
        action: &str,
        mission_id: Option<&str>,
        session_id: Option<&str>,
        machine_uid: Option<&str>,
        source: Option<&str>,
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
            source: source.map(String::from),
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

    // ── wrapper-bookend exclusion + repeated starts (#2426 round 2 MF1/(3)) ──

    #[test]
    fn an_open_wrapper_bookend_is_excluded_from_pairing_a_seat_dispatch_left_open_is_not() {
        let finalize_secs = parse_ts_secs_pub("2023-11-14T11:00:00Z");
        let lines = vec![
            // The launch's OWN liveness wrapper — session_id == mission_id
            // in production (`mission_bookend_record`), open the whole
            // time finalize runs. Must contribute NOTHING to
            // dispatch_seconds/open_dispatches.
            line(rec_src("2023-11-14T10:00:00Z", "dispatch start", Some("m1"), Some("m1"), None, Some("mission"))),
            // A real seat dispatch, also left open (no terminal).
            line(rec("2023-11-14T10:30:00Z", "dispatch start", Some("m1"), Some("s-seat"), None)),
        ];
        let got = aggregate_records_emitted(&lines, "m1", finalize_secs);
        // Only the seat dispatch's 30 minutes counts — NOT the wrapper's
        // full 60 minutes on top of it.
        assert_eq!(got.dispatch_seconds, 1800.0, "wrapper excluded, only the seat dispatch's open segment counts");
        assert_eq!(got.open_dispatches, 1, "the wrapper must not appear here at all");
        assert_eq!(got.dispatch_pairs, 0);
        // The wrapper record still counts as a real record of this mission.
        assert_eq!(got.total_records, 2);
        assert_eq!(got.by_action.get("dispatch start"), Some(&2));
    }

    #[test]
    fn a_wrapper_bookend_pair_never_becomes_a_dispatch_pair_either() {
        // Even when the wrapper's terminal DOES land in the same scan (a
        // re-finalize reading a day file written after the launch fully
        // returned), it must not be counted as a dispatch pair — it is
        // liveness, not seat work.
        let lines = vec![
            line(rec_src("2023-11-14T10:00:00Z", "dispatch start", Some("m1"), Some("m1"), None, Some("mission"))),
            line(rec_src("2023-11-14T11:00:00Z", "dispatch complete", Some("m1"), Some("m1"), None, Some("mission"))),
        ];
        let got = aggregate_records_emitted(&lines, "m1", 0);
        assert_eq!(got.dispatch_pairs, 0);
        assert_eq!(got.dispatch_seconds, 0.0);
        assert_eq!(got.open_dispatches, 0);
        assert_eq!(got.total_records, 2, "still real records of this mission");
    }

    #[test]
    fn a_repeated_dispatch_start_on_one_session_flushes_the_earlier_segment_to_finalize_time() {
        let finalize_secs = parse_ts_secs_pub("2023-11-14T10:10:00Z");
        let lines = vec![
            // First start on s1, never terminated...
            line(rec("2023-11-14T10:00:00Z", "dispatch start", Some("m1"), Some("s1"), None)),
            // ...then a SECOND start on the same session_id, also never
            // terminated. The first segment must not be silently dropped.
            line(rec("2023-11-14T10:05:00Z", "dispatch start", Some("m1"), Some("s1"), None)),
        ];
        let got = aggregate_records_emitted(&lines, "m1", finalize_secs);
        // Both segments are credited to the FINALIZE clock, not to the
        // second start's own ts (#2426 round 2 (3): "pair the earlier one
        // to finalize" — conservative, since nothing on disk proves the
        // first segment actually ended when the second one began; it
        // might genuinely have still been running). First: 10:00 ->
        // finalize (10:10) = 600s. Second: 10:05 -> finalize (10:10) = 300s.
        assert_eq!(got.dispatch_seconds, 600.0 + 300.0);
        assert_eq!(got.open_dispatches, 2, "both the flushed segment and the still-open final one");
        assert_eq!(got.dispatch_pairs, 0, "neither segment ever saw a real terminal");
    }

    #[test]
    fn a_repeated_start_that_later_terminates_pairs_against_the_second_start_only() {
        let lines = vec![
            line(rec("2023-11-14T10:00:00Z", "dispatch start", Some("m1"), Some("s1"), None)),
            line(rec("2023-11-14T10:05:00Z", "dispatch start", Some("m1"), Some("s1"), None)),
            line(rec("2023-11-14T10:07:00Z", "dispatch.complete", Some("m1"), Some("s1"), None)),
        ];
        let got = aggregate_records_emitted(&lines, "m1", 0);
        // First segment flushed to finalize_secs=0 at the second start —
        // `(0 - start_ts).max(0)` clamps to 0.0 (finalize "before" the
        // fixture's own timestamps is a test-harness artifact, not a real
        // scenario; the point here is the SECOND segment's pairing).
        assert_eq!(got.open_dispatches, 1, "the flushed first segment");
        assert_eq!(got.dispatch_pairs, 1, "the second start paired with the real terminal");
        assert_eq!(got.dispatch_seconds, 120.0, "only the paired segment's 2 minutes, clamped floor on the flushed one");
    }

    #[test]
    fn machine_uid_field_names_which_machines_samples_were_joined() {
        let lines = vec![line(rec(
            "2023-11-14T10:00:00Z",
            "dispatch start",
            Some("m1"),
            Some("s1"),
            Some("mac-1"),
        ))];
        let got = aggregate_records_emitted(&lines, "m1", 0);
        assert_eq!(got.machine_uid.as_deref(), Some("mac-1"));
    }

    #[test]
    fn no_machine_uid_on_any_matched_record_leaves_the_field_none() {
        let lines = vec![line(rec("2023-11-14T10:00:00Z", "dispatch start", Some("m1"), Some("s1"), None))];
        let got = aggregate_records_emitted(&lines, "m1", 0);
        assert_eq!(got.machine_uid, None);
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

    // ── records_emitted_for_mission — the disk-scanning wrapper (#2426 round 2 (6)) ──

    fn write_day_jsonl(dir: &std::path::Path, stem: &str, lines: &[serde_json::Value]) {
        std::fs::create_dir_all(dir).unwrap();
        let mut text = String::new();
        for v in lines {
            text.push_str(&serde_json::to_string(v).unwrap());
            text.push('\n');
        }
        std::fs::write(dir.join(format!("{stem}.jsonl")), text).unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn the_day_file_window_never_scans_past_the_finalize_day() {
        let tmp = tempfile::TempDir::new().unwrap();
        // In-window day.
        write_day_jsonl(
            tmp.path(),
            "2023-11-14",
            &[serde_json::json!({
                "ts": "2023-11-14T10:00:00Z", "level": "info", "category": "work",
                "tier": "local", "stage": "dispatch", "action": "dispatch.turn",
                "handle": "coder", "mission_id": "m1"
            })],
        );
        // A day file dated AFTER the finalize clock — must never be
        // scanned, even though it names the SAME mission.
        write_day_jsonl(
            tmp.path(),
            "2023-11-20",
            &[serde_json::json!({
                "ts": "2023-11-20T10:00:00Z", "level": "info", "category": "work",
                "tier": "local", "stage": "dispatch", "action": "dispatch.turn",
                "handle": "coder", "mission_id": "m1"
            })],
        );

        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        // SAFETY: serialized via #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path()) };
        // created_ts on 2023-11-14; finalize_secs also on 2023-11-14 — the
        // 2023-11-20 file is 6 days in the future relative to finalize.
        let created_ts = parse_ts_secs("2023-11-14T00:00:00Z").unwrap() as u64;
        let finalize_secs = parse_ts_secs("2023-11-14T23:00:00Z").unwrap();
        let (emitted, searched) = records_emitted_for_mission("m1", created_ts, finalize_secs);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }

        assert_eq!(searched, vec!["2023-11-14".to_string()], "the future day file must not even be OPENED");
        assert_eq!(emitted.total_records, 1, "only the in-window day file's record counts");
    }

    #[test]
    #[serial_test::serial]
    fn foreign_missions_never_leak_into_the_disk_scan_result() {
        // Correctness half of the round-2 retention fix — `aggregate_
        // records_emitted`'s own `mission_id` filter is belt-and-suspenders
        // with this, so this test alone does not prove nothing foreign was
        // ever RETAINED in memory (that half is evidenced by the cost
        // benchmark: `disk_cost_check` shows real wall-clock savings from
        // skipping the JSON parse on non-matching lines, which corroborates
        // that they are not carried forward either). This test proves the
        // OUTPUT is never polluted regardless of which layer is doing the
        // filtering.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut lines = vec![serde_json::json!({
            "ts": "2023-11-14T10:00:00Z", "level": "info", "category": "work",
            "tier": "local", "stage": "dispatch", "action": "dispatch start",
            "handle": "coder", "mission_id": "m1", "session_id": "s1"
        })];
        for i in 0..50 {
            lines.push(serde_json::json!({
                "ts": "2023-11-14T10:00:01Z", "level": "info", "category": "work",
                "tier": "local", "stage": "dispatch", "action": "dispatch.turn",
                "handle": "coder", "mission_id": format!("sibling-{i}"), "session_id": format!("s{i}")
            }));
        }
        write_day_jsonl(tmp.path(), "2023-11-14", &lines);

        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        // SAFETY: serialized via #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path()) };
        let (emitted, _searched) = records_emitted_for_mission("m1", 1_700_000_000, 1_800_000_000);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
        assert_eq!(emitted.total_records, 1, "only m1's own record, none of the 50 siblings");
    }
}


#[cfg(test)]
mod disk_cost_check {
    use super::*;
    use std::io::Write;

    /// (#2421 round 2 self-QA cost check — BEFORE/AFTER) Measures
    /// `records_emitted_for_mission`'s DISK path (not just the in-memory
    /// aggregation `cost_check` above already covers) against a synthetic
    /// ~44 MB / 60k-line day file — the reviewer-measured shape (380ms
    /// pre-fix on a comparable file, hit by every bare `darkmux dispatch`
    /// via `dispatch_as_crew_of_one.rs`). Run with `--nocapture`.
    #[test]
    #[serial_test::serial]
    fn records_emitted_for_mission_disk_cost_on_a_60k_line_day_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let day_file = tmp.path().join("2023-11-14.jsonl");
        {
            let mut f = std::fs::File::create(&day_file).unwrap();
            for i in 0..60_000u32 {
                let sec = i % 60;
                let ts = format!("2023-11-14T10:{:02}:{:02}Z", (i / 3600) % 60, sec);
                // ~90% belongs to a handful of OTHER missions (realistic
                // interleaving — this mission is a small share of a busy
                // day file), a long free-form payload per line (padding
                // toward the reviewer's ~733 bytes/line average), and a
                // sprinkling of `machine.telemetry` samples.
                let (mission, action, session): (String, &str, String) = match i % 100 {
                    0 => ("m-cost".to_string(), "dispatch start", "s-cost".to_string()),
                    1 => ("m-cost".to_string(), "dispatch.complete", "s-cost".to_string()),
                    2 => (String::new(), "machine.telemetry", String::new()),
                    n => (format!("other-mission-{}", n % 37), "dispatch.turn", format!("s-{}", n)),
                };
                let padding = "x".repeat(500);
                let line = if action == "machine.telemetry" {
                    format!(
                        r#"{{"ts":"{ts}","level":"info","category":"telemetry","tier":"local","stage":"dispatch","action":"machine.telemetry","handle":"host","machine_uid":"mac-cost","payload":{{"pad":"{padding}"}}}}"#
                    )
                } else {
                    format!(
                        r#"{{"ts":"{ts}","level":"info","category":"work","tier":"local","stage":"dispatch","action":"{action}","handle":"coder","mission_id":"{mission}","session_id":"{session}","machine_uid":"mac-cost","payload":{{"pad":"{padding}"}}}}"#
                    )
                };
                writeln!(f, "{line}").unwrap();
            }
        }
        let bytes = std::fs::metadata(&day_file).unwrap().len();

        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        // SAFETY: serialized via #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path()) };

        let start = std::time::Instant::now();
        let (got, _searched) = records_emitted_for_mission("m-cost", 1_700_000_000, 1_800_000_000);
        let elapsed = start.elapsed();

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }

        println!(
            "#2421 round-2 cost check: records_emitted_for_mission over a {} byte / 60000 line day file              ({} records matched `m-cost`) took {:?}",
            bytes, got.total_records, elapsed
        );
        assert_eq!(got.total_records, 1200, "600 start + 600 complete for m-cost across the 60k lines");
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
