//! Flow observability — structured JSONL records for darkmux run tracking.
//!
//! # Storage model
//!
//! Records are appended to a per-day JSONL file (`YYYY-MM-DD.jsonl`) under
//! `~/.darkmux/flows/` (overridable via `DARKMUX_FLOWS_DIR`). The first write
//! atomically prepends a schema header so partial-file recovery is possible.

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const FLOW_SCHEMA_VERSION: &str = "1.1.0";

#[derive(Debug, Clone, Copy, Deserialize, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Work,
    Machinery,
    Audit,
    Review,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Operator,
    Frontier,
    Local,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    Scope,
    Estimate,
    Dispatch,
    Review,
    Ship,
    Retrospect,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FlowRecord {
    pub ts: String,
    pub level: Level,
    pub category: Category,
    pub tier: Tier,
    pub stage: Stage,
    pub action: String,
    pub handle: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sprint_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Resolve the flows directory from env override (`DARKMUX_FLOWS_DIR`) or
/// default (`~/.darkmux/flows/`). Falls back to `/tmp/darkmux/flows/` if
/// neither is resolvable (CI / sandboxed environments without HOME).
pub(crate) fn flows_dir() -> PathBuf {
    std::env::var("DARKMUX_FLOWS_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".darkmux").join("flows")))
        .unwrap_or_else(|| PathBuf::from("/tmp/darkmux/flows"))
}

/// ISO 8601 UTC date string from current time — `YYYY-MM-DD`. Used for
/// per-day file naming (one JSONL file per UTC day), NOT for record `ts`.
pub(crate) fn day_utc_now() -> String {
    let secs = current_epoch_secs();
    let (y, m, d) = epoch_to_yyyymmdd(secs);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// ISO 8601 UTC datetime string from current time — `YYYY-MM-DDTHH:MM:SSZ`.
/// Used for `FlowRecord.ts`. Seconds precision is sufficient for the
/// dispatch / sprint timing surfaces; finer precision is a future bump.
pub(crate) fn ts_utc_now() -> String {
    let secs = current_epoch_secs();
    let (y, mo, d) = epoch_to_yyyymmdd(secs);
    let (h, mi, s) = epoch_to_hhmmss(secs);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, mi, s)
}

fn current_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Convert unix epoch seconds to (year, month, day) in UTC.
/// Civil calendar algorithm from Howard Hinnant (public-domain).
fn epoch_to_yyyymmdd(epochs: i64) -> (i32, u8, u8) {
    let days = epochs.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z / 146_097 } else { (z - 146_096) / 146_097 };
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp as i32 + 3 } else { mp as i32 - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u8, d as u8)
}

/// Convert unix epoch seconds to (hour, minute, second) in UTC.
fn epoch_to_hhmmss(epochs: i64) -> (u8, u8, u8) {
    let secs_of_day = epochs.rem_euclid(86_400);
    let h = (secs_of_day / 3600) as u8;
    let mi = ((secs_of_day % 3600) / 60) as u8;
    let s = (secs_of_day % 60) as u8;
    (h, mi, s)
}

/// Append `record` to today's per-day JSONL file. Creates the file with a
/// schema header as line 1 if it doesn't exist (written atomically with the
/// first record so a partial file never ends up header-only).
///
/// Concurrent writes: append-on-Unix is atomic up to PIPE_BUF (~4 KB on
/// macOS). Single-line JSONL records are well under this limit, so no
/// explicit locking is needed.
pub fn record(record: FlowRecord) -> Result<()> {
    let dir = flows_dir();
    let day = day_utc_now();
    let path = dir.join(format!("{day}.jsonl"));
    record_at(&record, &path)
}

/// Internal entry point writing to an explicit path. Used by tests and the
/// public `record()` wrapper delegates here after resolving the path.
///
/// Atomic-first-write semantics: when the file doesn't exist yet, the
/// schema header AND the first record are written in a single `write_all`
/// call against an exclusively-created handle (`create_new(true)`). This
/// closes two race classes the prior naive open+metadata-check pattern had:
///
///   1. **TOCTOU on header-needed check** — two concurrent processes both
///      seeing `len()==0` and both writing headers. Fixed: `create_new` is
///      atomic at the syscall level; only one process wins the create.
///   2. **Crash between header and record** — header-only files when the
///      process dies after writing line 1 but before line 2. Fixed: both
///      lines join into one buffer, one `write_all` syscall.
///
/// Concurrent appenders after the file exists: append-on-Unix is atomic
/// up to PIPE_BUF (~4 KB on macOS); a single-line JSONL record is well
/// under that, so no explicit locking is needed for the append case.
///
/// `sync_all()` is called after both write paths so audit-log durability
/// survives power loss / crash between record emission and consumer read.
pub(crate) fn record_at(record: &FlowRecord, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating flows dir {}", parent.display()))?;
        }
    }

    let darkmux_version = env!("CARGO_PKG_VERSION");
    let schema_header = serde_json::json!({
        "_type": "schema",
        "version": FLOW_SCHEMA_VERSION,
        "darkmux_version": darkmux_version,
    });
    let header_line = serde_json::to_string(&schema_header)?;
    let record_line = serde_json::to_string(record)?;

    // Try the atomic-create path: we win the create race → write header +
    // record together. If file already exists (other process or earlier
    // call), fall through to append-only.
    match fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
    {
        Ok(mut file) => {
            file.write_all(format!("{header_line}\n{record_line}\n").as_bytes())
                .with_context(|| format!("writing initial flow log {}", path.display()))?;
            file.sync_all()
                .with_context(|| format!("syncing flow log {}", path.display()))?;
            Ok(())
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(path)
                .with_context(|| format!("opening flow log for append {}", path.display()))?;
            writeln!(file, "{record_line}")
                .with_context(|| format!("appending to flow log {}", path.display()))?;
            file.sync_all()
                .with_context(|| format!("syncing flow log {}", path.display()))?;
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("creating flow log {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::env;
    use tempfile::TempDir;

    #[serial_test::serial]
    #[test]
    fn creates_file_with_schema_header_on_first_record() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("2025-01-15.jsonl");

        let record = FlowRecord {
            ts: "2025-01-15T12:34:56Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Dispatch,
            action: "ran".to_string(),
            handle: "test-1".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
        };

        record_at(&record, &path).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "expected 2 lines: header + record");

        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["_type"], "schema");

        let second: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["level"], "info");
    }

    #[serial_test::serial]
    #[test]
    fn appends_to_existing_file_without_re_emitting_header() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("2025-01-15.jsonl");

        let r = |action: &str| FlowRecord {
            ts: "2025-01-15T12:34:56Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Dispatch,
            action: action.to_string(),
            handle: "test".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
        };

        record_at(&r("first"), &path).unwrap();
        record_at(&r("second"), &path).unwrap();
        record_at(&r("third"), &path).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 4, "expected 4 lines: header + 3 records");

        // Header should be exactly once.
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["_type"], "schema");

        // Records 2 and 3 are plain flow records, no `_type: schema`.
        for line in &lines[1..] {
            let line: Value = serde_json::from_str(line).unwrap();
            assert!(
                line.get("_type").is_none(),
                "record line should not contain _type"
            );
        }
    }

    #[test]
    fn record_serializes_with_expected_shape() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("2025-01-15.jsonl");

        let record = FlowRecord {
            ts: "2025-06-01T08:00:00Z".to_string(),
            level: Level::Warn,
            category: Category::Audit,
            tier: Tier::Local,
            stage: Stage::Estimate,
            action: "budget_check".to_string(),
            handle: "handle-42".to_string(),
            sprint_id: Some("sp-100".to_string()),
            session_id: Some("sess-abc".to_string()),
            source: Some("estimator".to_string()),
        };

        record_at(&record, &path).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        // Second line is the record (first is schema header).
        let lines: Vec<&str> = contents.lines().collect();
        let rec_line = lines[1];

        // Parse as JSON and check fields.
        let parsed: Value = serde_json::from_str(rec_line).unwrap();

        assert_eq!(parsed["ts"], "2025-06-01T08:00:00Z");
        assert_eq!(parsed["level"], "warn");
        assert_eq!(parsed["category"], "audit");
        assert_eq!(parsed["tier"], "local");
        assert_eq!(parsed["stage"], "estimate");
        assert_eq!(parsed["action"], "budget_check");
        assert_eq!(parsed["handle"], "handle-42");

        // Optional fields should be present (not omitted) when set.
        let sprint_id = parsed.get("sprint_id").expect("expected sprint_id");
        assert_eq!(sprint_id, "sp-100");

        let session_id = parsed.get("session_id").expect("expected session_id");
        assert_eq!(session_id, "sess-abc");

        let source = parsed.get("source").expect("expected source");
        assert_eq!(source, "estimator");

        // Round-trip: parse back into FlowRecord.
        let roundtrip: FlowRecord = serde_json::from_str(rec_line).unwrap();
        assert_eq!(roundtrip.action, "budget_check");
        assert_eq!(roundtrip.handle, "handle-42");
    }

    #[serial_test::serial]
    #[test]
    fn record_at_uses_explicit_path() {
        let tmp = TempDir::new().unwrap();

        // Ensure DARKMUX_FLOWS_DIR is NOT set (or cleared) so we don't
        // accidentally write to an unexpected location.
        let prev = env::var("DARKMUX_FLOWS_DIR").ok();

        record_at(
            &FlowRecord {
                ts: "2025-03-21T14:00:00Z".to_string(),
                level: Level::Trace,
                category: Category::Review,
                tier: Tier::Frontier,
                stage: Stage::Scope,
                action: "scope_review".to_string(),
                handle: "ex-path-1".to_string(),
                sprint_id: None,
                session_id: None,
                source: Some("reviewer".to_string()),
            },
            &tmp.path().join("custom.jsonl"),
        )
        .unwrap();

        // Restore env var.
        match prev {
            Some(v) => env::set_var("DARKMUX_FLOWS_DIR", v),
            None => env::remove_var("DARKMUX_FLOWS_DIR"),
        }

        let contents = fs::read_to_string(tmp.path().join("custom.jsonl")).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2); // header + record

        let parsed: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(parsed["action"], "scope_review");
    }

    #[serial_test::serial]
    #[test]
    fn optional_fields_omit_when_none() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("2025-01-01.jsonl");

        let record = FlowRecord {
            ts: "2025-01-01T00:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Ship,
            action: "deploy".to_string(),
            handle: "ship-1".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
        };

        record_at(&record, &path).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        let rec_line = lines[1];

        // Optional fields should NOT appear when None.
        let parsed: Value = serde_json::from_str(rec_line).unwrap();

        // Verify keys don't exist (not null, absent entirely).
        assert!(parsed.get("sprint_id").is_none());
        assert!(parsed.get("session_id").is_none());
        assert!(parsed.get("source").is_none());

        // Required fields must be present.
        assert!(parsed.get("ts").is_some());
        assert!(parsed.get("level").is_some());
        assert!(parsed.get("action").is_some());
    }

    #[serial_test::serial]
    #[test]
    fn flows_dir_respects_env_override() {
        let tmp = TempDir::new().unwrap();

        // SAFETY: serialized via `#[serial_test::serial]` on every test that
        // mutates this env var. Outside that lock, `set_var` is unsafe in
        // 2024 edition (race with other readers); serial-tests serializes it.
        let prev = env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { env::set_var("DARKMUX_FLOWS_DIR", tmp.path()); }

        let rec = FlowRecord {
            ts: "2025-04-10T10:00:00Z".to_string(),
            level: Level::Info,
            category: Category::Work,
            tier: Tier::Operator,
            stage: Stage::Review,
            action: "env_test".to_string(),
            handle: "ev-1".to_string(),
            sprint_id: None,
            session_id: None,
            source: None,
        };

        // Capture the day-key BEFORE calling record() so a midnight-UTC
        // crossing between record() and the assertion doesn't make the
        // file appear at a different name than we check. (record() reads
        // day_utc_now() once for the file path; we read it once too; both
        // reads land in the same wall-clock window typically <1ms apart.)
        let day_before = day_utc_now();
        super::record(rec).unwrap();
        let day_after = day_utc_now();

        // SAFETY: same — serialized via the test attribute.
        unsafe {
            match prev {
                Some(v) => env::set_var("DARKMUX_FLOWS_DIR", v),
                None => env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }

        // Accept either day's file (handles the rare midnight crossing).
        let candidates = if day_before == day_after {
            vec![tmp.path().join(format!("{day_before}.jsonl"))]
        } else {
            vec![
                tmp.path().join(format!("{day_before}.jsonl")),
                tmp.path().join(format!("{day_after}.jsonl")),
            ]
        };
        let found = candidates.iter().find(|p| p.exists()).cloned();
        assert!(
            found.is_some(),
            "file should exist in env-override dir under {} or {}",
            day_before,
            day_after
        );

        let contents = fs::read_to_string(found.unwrap()).unwrap();
        assert!(contents.contains("env_test"));
    }

    #[test]
    fn epoch_to_yyyymmdd_known_dates() {
        // Unix epoch start
        let (y, m, d) = epoch_to_yyyymmdd(0);
        assert_eq!((y, m, d), (1970, 1, 1));

        // Leap year: 2024-02-29
        let (y, m, d) = epoch_to_yyyymmdd(1_709_164_800);
        assert_eq!((y, m, d), (2024, 2, 29));

        // Year boundary: 2025-01-01 = epoch 1735689600
        let (y, m, d) = epoch_to_yyyymmdd(1_735_689_600);
        assert_eq!((y, m, d), (2025, 1, 1));

        // Mid-year: 2024-07-04 = epoch 1_720_051_200
        let (y, m, d) = epoch_to_yyyymmdd(1_720_051_200);
        assert_eq!((y, m, d), (2024, 7, 4));
    }

    #[test]
    fn epoch_to_hhmmss_known_times() {
        // Midnight
        assert_eq!(epoch_to_hhmmss(0), (0, 0, 0));
        // 2024-01-01 00:00:00 UTC
        assert_eq!(epoch_to_hhmmss(1_704_067_200), (0, 0, 0));
        // 2024-01-01 12:34:56 UTC = epoch start + 12*3600 + 34*60 + 56 = 1_704_067_200 + 45_296
        assert_eq!(epoch_to_hhmmss(1_704_067_200 + 45_296), (12, 34, 56));
        // 23:59:59 boundary: midnight - 1 second
        assert_eq!(epoch_to_hhmmss(86_400 - 1), (23, 59, 59));
        // Mid-day check: epoch 1_720_094_400 = 2024-07-04 12:00:00 UTC
        // (epoch 1_720_051_200 is 2024-07-04 00:00:00 UTC; +43_200s = noon)
        assert_eq!(epoch_to_hhmmss(1_720_051_200 + 43_200), (12, 0, 0));
    }

    #[test]
    fn ts_utc_now_returns_iso8601_datetime() {
        // Schema 1.1: ts must be full datetime with time-of-day, not just a date.
        let ts = ts_utc_now();
        let bytes = ts.as_bytes();
        assert_eq!(ts.len(), 20, "expected YYYY-MM-DDTHH:MM:SSZ (20 chars), got {ts:?}");
        assert_eq!(bytes[4], b'-');
        assert_eq!(bytes[7], b'-');
        assert_eq!(bytes[10], b'T');
        assert_eq!(bytes[13], b':');
        assert_eq!(bytes[16], b':');
        assert_eq!(bytes[19], b'Z');
        // Digits in the expected positions
        for &i in &[0usize, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18] {
            assert!(
                bytes[i].is_ascii_digit(),
                "expected digit at index {i} in {ts:?}",
            );
        }
    }

    #[test]
    fn day_utc_now_returns_date_only() {
        // day_utc_now() is for file naming — must stay YYYY-MM-DD regardless
        // of the schema bump on ts_utc_now().
        let day = day_utc_now();
        let bytes = day.as_bytes();
        assert_eq!(day.len(), 10, "expected YYYY-MM-DD (10 chars), got {day:?}");
        assert_eq!(bytes[4], b'-');
        assert_eq!(bytes[7], b'-');
        for &i in &[0usize, 1, 2, 3, 5, 6, 8, 9] {
            assert!(
                bytes[i].is_ascii_digit(),
                "expected digit at index {i} in {day:?}",
            );
        }
    }

    #[test]
    fn flow_schema_version_is_1_1_0() {
        // Pin the schema version so an accidental rename can't ship silently;
        // any bump beyond this should be a deliberate code change paired with
        // an update to this assertion (and corresponding viewer EXPECTED_*
        // bump if the change is breaking).
        assert_eq!(FLOW_SCHEMA_VERSION, "1.1.0");
    }

    #[test]
    fn schema_header_contains_version_and_darkmux_version() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("2025-01-01.jsonl");

        record_at(
            &FlowRecord {
                ts: "2025-01-01T00:00:00Z".to_string(),
                level: Level::Info,
                category: Category::Work,
                tier: Tier::Operator,
                stage: Stage::Dispatch,
                action: "init".to_string(),
                handle: "schema-check".to_string(),
                sprint_id: None,
                session_id: None,
                source: None,
            },
            &path,
        )
        .unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        let header: Value = serde_json::from_str(lines[0]).unwrap();

        assert_eq!(header["version"], FLOW_SCHEMA_VERSION);
        // CARGO_PKG_VERSION is set by cargo; check it's a non-empty string.
        let ver: &str = header["darkmux_version"].as_str().unwrap();
        assert!(!ver.is_empty());
    }
}
