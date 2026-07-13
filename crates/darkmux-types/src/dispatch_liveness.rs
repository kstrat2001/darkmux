//! Dependency-free dispatch liveness FLOOR (#1311, part of #1278).
//!
//! `finhub-adonisjs#563` hung 19 minutes on a tiny Azure-only review, machine
//! awake (no thermal), and emitted ZERO flow records — it froze BEFORE
//! flow-sink init (before Redis/audit setup), leaving no trace of what phase
//! it was in. The dispatch bookends (#1272) ride the flow machinery, so they
//! cannot help a hang that dies before flow exists. This module is the level
//! BELOW them: a liveness marker with NO dependency on config resolution,
//! Redis, the audit sink, or the flow stream.
//!
//! [`liveness`] appends `<ts> +<elapsed>ms <phase> pid=<pid> case=<case> | <detail>`
//! to BOTH stderr (an `[darkmux-liveness]`-prefixed line) AND a per-dispatch
//! heartbeat file at `<darkmux-home>/liveness/<pid>.log`. Every line stamps
//! the elapsed-since-first-marker so the heartbeat shows WHERE the wall-clock
//! went; [`liveness_detail`] carries a resolved, NON-SECRET detail (a host, an
//! item name, counts) so the trail is genuinely debuggable from the host, not
//! just "reached phase X". It is INFALLIBLE by construction:
//!
//! - A failed file write (dir missing, permission denied, disk full) is
//!   swallowed — it NEVER panics and NEVER blocks a dispatch.
//! - stderr is attempted FIRST (it is the most reliable surface — #563 left us
//!   with only the run-log stderr, and even that showed nothing).
//! - It touches ONLY `std` + `dirs` (home resolution). No `config_access`, no
//!   `darkmux-flow`, no Redis, no audit — so it works at the very first instant
//!   a dispatch process starts, long before any of those are initialized.
//!
//! The heartbeat file is keyed by the process id (stable for the whole dispatch
//! and known from the first instant), so every marker for one process lands in
//! the same file even when the early phases don't yet know a case id. Files
//! accumulate (tiny text, one per dispatch); post-hoc inspection reads the
//! newest `<pid>.log`. Rotation is intentionally not built here — the floor
//! stays dead simple so it cannot itself become a source of hangs.

use crate::paths::expand_tilde;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Wall-clock origin for the elapsed stamp — the first marker of this process.
/// Purely for the debuggability stamp; still zero external dependency.
fn start_instant() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

/// Emit a liveness marker for the current dispatch `phase`, keyed by the
/// process id as the case field, with no detail.
///
/// Use this at the earliest phases, before a real case id (`repo@sha`, a
/// worktree path) is known — the case field defaults to the pid. Once the
/// case id is known, prefer [`liveness_case`] / [`liveness_detail`] so the
/// trail is self-describing. Infallible: see the module docs.
pub fn liveness(phase: &str) {
    let pid = std::process::id();
    emit(phase, &pid.to_string(), "");
}

/// [`liveness`] with an explicit `case` id threaded in (e.g. `repo@sha` or the
/// worktree path — the same handle the dispatch bookends and review-pipeline records
/// carry, so the floor trail lines up with the flow records once flow exists).
pub fn liveness_case(phase: &str, case: &str) {
    emit(phase, case, "");
}

/// [`liveness_case`] plus a resolved, NON-SECRET `detail` — the debuggable half
/// (a resolved home, enabled sinks, an endpoint HOST, a Keychain item name,
/// bundle counts, a seat/model). HARD RULE: `detail` must NEVER carry a secret
/// — no Keychain values, no api keys, no full URL that carries a key/token in
/// its query. Host-only + item-name-only for anything credential-adjacent.
pub fn liveness_detail(phase: &str, case: &str, detail: &str) {
    emit(phase, case, detail);
}

/// The shared emit path: stderr first (always attempted), then a best-effort
/// heartbeat-file append whose every failure is swallowed.
fn emit(phase: &str, case: &str, detail: &str) {
    let pid = std::process::id();
    let ms = start_instant().elapsed().as_millis();
    let tail = if detail.is_empty() { String::new() } else { format!(" | {detail}") };
    let line = format!("{ts} +{ms}ms {phase} pid={pid} case={case}{tail}", ts = ts_utc_now());
    // stderr FIRST — the most reliable surface. The `[darkmux-liveness]` prefix
    // makes the markers greppable in a workflow run log (where #563 showed
    // nothing at all).
    eprintln!("[darkmux-liveness] {line}");
    // Best-effort heartbeat-file append. EVERY failure is swallowed: a liveness
    // marker must NEVER panic or block a dispatch (#1311).
    let _ = append_heartbeat(pid, &line);
}

/// Append `line` to `<darkmux-home>/liveness/<pid>.log`, creating the dir.
/// Returns the error to [`emit`], which swallows it — the `Result` exists only
/// so the `?` operator keeps this body tidy.
fn append_heartbeat(pid: u32, line: &str) -> std::io::Result<()> {
    let dir = liveness_dir();
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{pid}.log"));
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{line}")
}

/// The heartbeat directory: `<darkmux-home>/liveness/`.
///
/// Resolves the darkmux home WITHOUT touching config resolution — the whole
/// point of the floor is zero dependency on config/Redis/audit/flow: honor
/// `DARKMUX_HOME` (the #661 bootstrap pointer, tilde-expanded) if set, else
/// `~/.darkmux`. This mirrors the `DARKMUX_HOME` + user-root branches of
/// `paths::resolve`, minus the project-local `.darkmux` auto-detect and all
/// config reads — the floor can't afford a cwd stat or a config load at the
/// first instant of a possibly-already-hung process.
fn liveness_dir() -> PathBuf {
    if let Ok(root) = std::env::var("DARKMUX_HOME") {
        let root = root.trim();
        if !root.is_empty() {
            return expand_tilde(root).join("liveness");
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".darkmux")
        .join("liveness")
}

/// RFC3339-ish UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`), pure `std` — no chrono,
/// no `darkmux-flow` dependency. Uses the same public-domain civil-calendar
/// algorithm (Howard Hinnant) that `darkmux_flow::schema::ts_utc_now` uses,
/// inlined here because the liveness floor must not depend on `darkmux-flow`
/// (its records are exactly what a pre-flow hang can't produce). A clock error
/// degrades to the epoch rather than panicking (infallibility, #1311).
fn ts_utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, mo, d) = epoch_to_yyyymmdd(secs);
    let secs_of_day = secs.rem_euclid(86_400);
    let (h, mi, s) = (secs_of_day / 3600, (secs_of_day % 3600) / 60, secs_of_day % 60);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Unix epoch seconds -> (year, month, day) in UTC (Howard Hinnant, public
/// domain). Same algorithm as `darkmux_flow::schema::epoch_to_yyyymmdd`.
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Read the single `<pid>.log` heartbeat file under a liveness dir. The
    /// floor writes exactly one file per process, so a test that sets
    /// `DARKMUX_HOME` to a fresh tempdir finds its own process's file here.
    fn read_only_heartbeat(liveness_dir: &std::path::Path) -> String {
        let mut logs: Vec<_> = fs::read_dir(liveness_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "log"))
            .collect();
        assert_eq!(logs.len(), 1, "expected exactly one heartbeat file, got {logs:?}");
        fs::read_to_string(logs.pop().unwrap()).unwrap()
    }

    #[serial_test::serial]
    #[test]
    fn liveness_writes_markers_to_the_heartbeat_file_in_order() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()); }

        liveness("process-start");
        liveness_case("config-resolved", "acme/repo@abc123");
        liveness_detail("bundling-done", "acme/repo@abc123", "bundles=3 files=2");
        liveness("done");

        let body = read_only_heartbeat(&tmp.path().join("liveness"));
        // Line shape: `<ts> +<ms>ms <phase> pid=.. case=.. [| detail]` — phase
        // is the THIRD whitespace token (ts, +Nms, phase).
        let phases: Vec<&str> = body
            .lines()
            .filter_map(|l| l.split_whitespace().nth(2))
            .collect();
        assert_eq!(
            phases,
            ["process-start", "config-resolved", "bundling-done", "done"],
            "body was:\n{body}"
        );
        // The explicit case id is carried verbatim; the default is the pid.
        assert!(body.contains("case=acme/repo@abc123"), "body was:\n{body}");
        assert!(body.contains(&format!("case={}", std::process::id())), "body was:\n{body}");
        // Every line stamps elapsed; the detail rides after the `|` separator.
        assert!(body.lines().all(|l| l.contains("ms ")), "elapsed stamp missing:\n{body}");
        assert!(body.contains("| bundles=3 files=2"), "detail missing:\n{body}");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn liveness_is_infallible_when_the_dir_cannot_be_created() {
        // Point DARKMUX_HOME at a path UNDER a regular file, so `create_dir_all`
        // for `<home>/liveness/` is guaranteed to fail (a file is not a dir).
        // The marker must still return normally — stderr is emitted, the file
        // write is swallowed, nothing panics or blocks (#1311).
        let tmp = TempDir::new().unwrap();
        let a_file = tmp.path().join("not-a-dir");
        fs::write(&a_file, b"x").unwrap();
        let unwritable_home = a_file.join("home"); // a child path of a plain file
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", &unwritable_home); }

        // No panic, returns — that IS the assertion.
        liveness("process-start");
        liveness_case("credential-read:darkmux-azure", "acme/repo@abc123");

        // And nothing was created under the bogus home.
        assert!(!unwritable_home.join("liveness").exists());

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
    }

    #[test]
    fn ts_utc_now_is_rfc3339_ish_20_chars() {
        let ts = ts_utc_now();
        assert_eq!(ts.len(), 20, "expected YYYY-MM-DDTHH:MM:SSZ (20 chars), got {ts:?}");
        assert!(ts.ends_with('Z') && ts.as_bytes()[10] == b'T', "shape wrong: {ts:?}");
    }
}
