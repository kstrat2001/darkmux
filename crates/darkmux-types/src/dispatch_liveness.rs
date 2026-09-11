//! Dependency-free dispatch liveness FLOOR (#1311, part of #1278).
//!
//! a private production review hung 19 minutes on a tiny Azure-only review, machine
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
//! - It touches ONLY `std` + `dirs` (home resolution) + a raw `serde_json::Value`
//!   peek at `config.json` for the retention window (below) — never
//!   `config_access`, `darkmux-flow`, Redis, or audit — so it works at the
//!   very first instant a dispatch process starts, long before any of those
//!   are initialized.
//!
//! The heartbeat file is keyed by the process id (stable for the whole dispatch
//! and known from the first instant), so every marker for one process lands in
//! the same file even when the early phases don't yet know a case id. Files
//! accumulate (tiny text, one per dispatch); post-hoc inspection reads the
//! newest `<pid>.log`.
//!
//! ## Retention (#2653)
//!
//! An operator laptop was found with 10,249 heartbeat files (40 MB, oldest
//! from two months prior) and nothing pruning them — every write grows the
//! directory forever, and (the correctness half, not just the size half) a
//! bare pid filename with no age/generation component means a RECYCLED pid
//! can collide with a stale file from an unrelated, long-dead dispatch:
//! [`append_heartbeat`] opens in append mode, so without pruning, a reused
//! pid would silently interleave a new dispatch's markers into an old
//! dispatch's leftover trail.
//!
//! [`prune_stale_heartbeats`] runs (best-effort, once per process per
//! directory) before every heartbeat-file open, removing `<pid>.log` files
//! whose last-modified time is older than [`retention_hours`]'s window
//! (default 168h / 7 days). This bounds growth AND narrows the pid-collision
//! window to that same retention period: a pid reused after the window has
//! elapsed opens a fresh file rather than appending into a stale one. It does
//! NOT eliminate collision within the window (two dispatches whose pids
//! happen to match inside those 7 days) — closing that fully needs a
//! pid+start-time or generation component in the filename/content, which is
//! deliberately NOT built here, mirroring [`crate::residency_lease`]'s own
//! documented v1 decision on the identical pid-reuse question for its sibling
//! `<darkmux-home>/residency/<pid>.lease` registry: the failure direction of
//! a bare pid without that hardening is a stale record being read a little
//! longer than ideal, never data being wrongfully destroyed. Filed as
//! kstrat2001/darkmux#2654 if that slack ever proves to matter in practice.
//!
//! The retention window is an operator-visible setting
//! (`config.json`'s `runtime.liveness_retention_hours`, doctor-surfaced via
//! `config_access::liveness_retention_hours`) but [`retention_hours`] does
//! its OWN tiny raw peek at the config file rather than calling
//! `config_access` — see that function's doc for why.
//!
//! Pruning is infallible in the same sense as everything else in this
//! module: every per-file error is skipped, never propagated, and a failed
//! prune pass can never block or fail the heartbeat write it rides on.

use crate::paths::expand_tilde;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Default retention window, in hours, for `<darkmux-home>/liveness/<pid>.log`
/// heartbeat files — 7 days. See the module docs' "Retention" section.
const DEFAULT_LIVENESS_RETENTION_HOURS: u64 = 24 * 7;

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
    // (#2653) Best-effort prune BEFORE opening this pid's file: never fails
    // (see `prune_once_per_dir`'s own doc), and running it first means a
    // recycled pid whose old file has aged out is gone before we'd otherwise
    // append into it.
    prune_once_per_dir(&dir);
    let path = dir.join(format!("{pid}.log"));
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{line}")
}

/// The darkmux home root: `DARKMUX_HOME` (the #661 bootstrap pointer,
/// tilde-expanded) if set, else `~/.darkmux`. Resolved WITHOUT touching
/// config resolution — the whole point of the floor is zero dependency on
/// config/Redis/audit/flow: this mirrors the `DARKMUX_HOME` + user-root
/// branches of `paths::resolve`, minus the project-local `.darkmux`
/// auto-detect and all config reads — the floor can't afford a cwd stat or
/// a full config load at the first instant of a possibly-already-hung
/// process. Shared by [`liveness_dir`] and [`retention_hours`]'s raw
/// config-file peek.
fn darkmux_home_dir() -> PathBuf {
    #[cfg(any(test, feature = "test-support"))]
    crate::env_audit::audit_env_read("DARKMUX_HOME");
    if let Ok(root) = std::env::var("DARKMUX_HOME") {
        let root = root.trim();
        if !root.is_empty() {
            return expand_tilde(root);
        }
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".darkmux")
}

/// The heartbeat directory: `<darkmux-home>/liveness/`.
fn liveness_dir() -> PathBuf {
    darkmux_home_dir().join("liveness")
}

/// (#2653) The retention window, in hours, [`prune_stale_heartbeats`] applies
/// at write time. Resolves `env(DARKMUX_LIVENESS_RETENTION_HOURS) >
/// config.json's "runtime"."liveness_retention_hours" (a raw peek) > 168`.
///
/// Deliberately NOT `config_access::liveness_retention_hours()` — which
/// resolves the SAME precedence and is what `darkmux doctor` reads for the
/// operator-visible value — because this module's entire reason for
/// existing is to work before config/Redis/audit/flow are touched (see the
/// module docs' "no config_access" invariant). Calling into
/// `config_access`'s cached `DarkmuxConfig` / `paths::resolve` machinery
/// from here would reintroduce exactly the dependency #1311 built this
/// floor to route around. So this does its own minimal, swallow-everything
/// peek at the raw JSON instead — no shared cache, no `paths::resolve`, just
/// one more `std::fs::read_to_string` alongside the ones this module
/// already does for the heartbeat file itself.
fn retention_hours() -> u64 {
    #[cfg(any(test, feature = "test-support"))]
    crate::env_audit::audit_env_read("DARKMUX_LIVENESS_RETENTION_HOURS");
    if let Ok(v) = std::env::var("DARKMUX_LIVENESS_RETENTION_HOURS") {
        if let Ok(n) = v.trim().parse::<u64>() {
            return n;
        }
    }
    raw_config_liveness_retention_hours().unwrap_or(DEFAULT_LIVENESS_RETENTION_HOURS)
}

/// Best-effort raw peek at `<darkmux-home>/config.json`'s
/// `runtime.liveness_retention_hours` field — `None` on ANY failure
/// (missing file, unreadable, malformed JSON, wrong type, absent field),
/// which falls through to [`DEFAULT_LIVENESS_RETENTION_HOURS`] in
/// [`retention_hours`]. Never panics, never touches `config_access`.
fn raw_config_liveness_retention_hours() -> Option<u64> {
    let text = fs::read_to_string(darkmux_home_dir().join("config.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value.get("runtime")?.get("liveness_retention_hours")?.as_u64()
}

/// Best-effort removal of `<pid>.log` heartbeat files in `dir` whose last
/// modification is older than `retention` (measured against `now`). NEVER
/// fails: every per-file error (can't stat, can't parse the pid, can't
/// remove) is skipped, not propagated — a prune pass running under
/// degraded permissions must never block the heartbeat write it rides on
/// (#2653). Returns the count actually removed (for tests).
///
/// Safety invariants (each has a dedicated test below):
/// - only `.log` files are ever considered — `host-sampler.lock`
///   (`config_access::host_sampler_lock_path`, which shares this directory)
///   and anything else non-`.log` is untouched.
/// - a `.log` file whose stem does not parse as a `u32` pid is untouched —
///   filename-trust is exactly the correctness risk this module's docs
///   name, so pruning never guesses at what a non-pid name might mean.
/// - a directory entry that isn't a regular file is untouched.
/// - a FUTURE mtime (clock skew) makes `now.duration_since(modified)` error,
///   which is treated as age zero — never removed. Fail-safe in the same
///   direction `residency_lease::process_alive` documents: a slack here is a
///   *missed* prune, never a *wrongful* delete.
/// - the boundary is exclusive: age strictly greater than `retention` is
///   removed; age equal to or less than `retention` is kept.
fn prune_stale_heartbeats(dir: &Path, retention: Duration, now: SystemTime) -> usize {
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    let mut removed = 0;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("log") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        if stem.parse::<u32>().is_err() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else { continue };
        let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
        if age > retention && fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Run [`prune_stale_heartbeats`] at most once per `(process, directory)`
/// pair — a real dispatch process calls [`liveness`]/[`liveness_case`]/
/// [`liveness_detail`] many times across its phases, and re-scanning the
/// whole directory on every single marker would be wasted work once the
/// corpus is trimmed down (and needless cost on every call before that).
/// Keyed by directory rather than a bare "ran once" flag so tests using
/// distinct `DARKMUX_HOME` tempdirs each get their own independent prune,
/// same as production (one process resolves one home for its whole life).
fn prune_once_per_dir(dir: &Path) {
    static PRUNED_DIRS: OnceLock<Mutex<std::collections::HashSet<PathBuf>>> = OnceLock::new();
    let set = PRUNED_DIRS.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
    let mut guard = match set.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.insert(dir.to_path_buf()) {
        drop(guard);
        let retention = Duration::from_secs(retention_hours().saturating_mul(3600));
        prune_stale_heartbeats(dir, retention, SystemTime::now());
    }
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

    // ── #2653: retention pruning ──

    /// Restore-on-drop guard for a single env var (so a panicking assertion
    /// mid-test still restores it) — same shape as
    /// `residency_lease::tests::EnvGuard`.
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value) };
            Self { key, prev }
        }
        fn unset(key: &'static str) -> Self {
            let prev = std::env::var(key).ok();
            unsafe { std::env::remove_var(key) };
            Self { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    /// Create a FRESH (empty) file with a specific mtime — for tests where
    /// content doesn't matter, only age. `File::create` truncates, so this
    /// must never be called on a path whose existing content the test
    /// still needs; use [`set_mtime`] for that.
    fn touch_with_mtime(path: &std::path::Path, mtime: SystemTime) {
        let f = fs::File::create(path).unwrap();
        f.set_modified(mtime).unwrap();
    }

    /// Backdate an ALREADY-WRITTEN file's mtime without touching its
    /// content — `OpenOptions::write(true)` with no `truncate`/`create`,
    /// unlike [`touch_with_mtime`]. Use this after `fs::write` when the
    /// test still needs to read that content back later.
    fn set_mtime(path: &std::path::Path, mtime: SystemTime) {
        let f = fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(mtime).unwrap();
    }

    #[test]
    fn prune_stale_heartbeats_boundary_cases() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let now = SystemTime::now();
        let retention = Duration::from_secs(3600);

        // Just outside the window (older than retention) → removed.
        touch_with_mtime(&dir.join("100.log"), now - retention - Duration::from_secs(1));
        // Just inside the window (younger than retention) → kept.
        touch_with_mtime(&dir.join("200.log"), now - retention + Duration::from_secs(1));
        // Exactly at the cutoff → kept (the boundary removes on STRICTLY
        // greater age, never equal).
        touch_with_mtime(&dir.join("300.log"), now - retention);
        // Future mtime (clock skew) → `duration_since` errors, treated as
        // age zero → never removed.
        touch_with_mtime(&dir.join("400.log"), now + Duration::from_secs(3600));
        // A `.log` file whose stem is not a pid → never touched, however old.
        touch_with_mtime(&dir.join("not-a-pid.log"), now - retention - Duration::from_secs(1_000));
        // A non-`.log` file sharing the directory (mirrors host-sampler.lock)
        // → never touched, however old.
        touch_with_mtime(&dir.join("500.lock"), now - retention - Duration::from_secs(1_000));
        // A directory that happens to be named like a pid file → never
        // touched (not a regular file).
        fs::create_dir(dir.join("600.log")).unwrap();

        let removed = prune_stale_heartbeats(dir, retention, now);

        assert_eq!(removed, 1, "only the strictly-aged-out pid file is removed");
        assert!(!dir.join("100.log").exists(), "aged-out pid file must be gone");
        assert!(dir.join("200.log").exists(), "just inside the window is kept");
        assert!(dir.join("300.log").exists(), "exactly at the cutoff is kept (exclusive boundary)");
        assert!(dir.join("400.log").exists(), "a future mtime (clock skew) is never removed");
        assert!(dir.join("not-a-pid.log").exists(), "a non-pid filename is never touched");
        assert!(dir.join("500.lock").exists(), "a non-.log file (e.g. host-sampler.lock) is never touched");
        assert!(dir.join("600.log").exists(), "a directory, not a regular file, is never touched");
    }

    #[test]
    fn prune_stale_heartbeats_on_a_dir_with_nothing_prunable_removes_nothing() {
        let tmp = TempDir::new().unwrap();
        touch_with_mtime(&tmp.path().join("1.log"), SystemTime::now());
        assert_eq!(
            prune_stale_heartbeats(tmp.path(), Duration::from_secs(3600), SystemTime::now()),
            0
        );
        assert!(tmp.path().join("1.log").exists());
    }

    #[test]
    fn prune_stale_heartbeats_on_an_empty_dir_removes_nothing() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            prune_stale_heartbeats(tmp.path(), Duration::from_secs(3600), SystemTime::now()),
            0
        );
    }

    #[test]
    fn prune_stale_heartbeats_on_a_missing_dir_is_a_noop_not_a_panic() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert_eq!(
            prune_stale_heartbeats(&missing, Duration::from_secs(3600), SystemTime::now()),
            0
        );
    }

    #[serial_test::serial]
    #[test]
    fn retention_hours_env_override_wins() {
        let _g = EnvGuard::set("DARKMUX_LIVENESS_RETENTION_HOURS", "42");
        assert_eq!(retention_hours(), 42);
    }

    #[serial_test::serial]
    #[test]
    fn retention_hours_reads_the_config_json_field_when_env_is_unset() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("config.json"), r#"{"runtime":{"liveness_retention_hours":42}}"#)
            .unwrap();
        let _home = EnvGuard::set("DARKMUX_HOME", tmp.path().to_str().unwrap());
        let _ret = EnvGuard::unset("DARKMUX_LIVENESS_RETENTION_HOURS");
        assert_eq!(retention_hours(), 42);
    }

    #[serial_test::serial]
    #[test]
    fn retention_hours_falls_back_to_the_default_on_malformed_config_json() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("config.json"), "{not valid json").unwrap();
        let _home = EnvGuard::set("DARKMUX_HOME", tmp.path().to_str().unwrap());
        let _ret = EnvGuard::unset("DARKMUX_LIVENESS_RETENTION_HOURS");
        assert_eq!(retention_hours(), DEFAULT_LIVENESS_RETENTION_HOURS);
    }

    #[serial_test::serial]
    #[test]
    fn retention_hours_falls_back_to_the_default_with_no_config_json_at_all() {
        let tmp = TempDir::new().unwrap();
        let _home = EnvGuard::set("DARKMUX_HOME", tmp.path().to_str().unwrap());
        let _ret = EnvGuard::unset("DARKMUX_LIVENESS_RETENTION_HOURS");
        assert_eq!(retention_hours(), DEFAULT_LIVENESS_RETENTION_HOURS);
    }

    /// The growth half (#2653): a heartbeat file left behind by some OTHER,
    /// long-dead pid and older than the retention window is pruned the next
    /// time ANY dispatch on this machine writes a marker — not just when
    /// that exact pid comes back.
    #[serial_test::serial]
    #[test]
    fn liveness_prunes_an_unrelated_stale_heartbeat_file_on_write() {
        let tmp = TempDir::new().unwrap();
        let _home = EnvGuard::set("DARKMUX_HOME", tmp.path().to_str().unwrap());
        let _ret = EnvGuard::set("DARKMUX_LIVENESS_RETENTION_HOURS", "1");

        let dir = tmp.path().join("liveness");
        fs::create_dir_all(&dir).unwrap();
        let stale = dir.join("999999.log");
        fs::write(&stale, "an old, unrelated dispatch's leftover marker\n").unwrap();
        set_mtime(&stale, SystemTime::now() - Duration::from_secs(2 * 3600));

        liveness("process-start");

        assert!(!stale.exists(), "a heartbeat file older than the retention window must be pruned on write");
    }

    /// The correctness half (#2653): a stale file that happens to share THIS
    /// process's own pid (the collision case — an earlier, unrelated
    /// dispatch that has long since exited) must be pruned before the new
    /// marker is written, never appended into. Without this, two unrelated
    /// dispatches' heartbeat trails would interleave under one `pid=` field.
    #[serial_test::serial]
    #[test]
    fn liveness_recovers_from_a_same_pid_collision_instead_of_appending_into_the_stale_file() {
        let tmp = TempDir::new().unwrap();
        let _home = EnvGuard::set("DARKMUX_HOME", tmp.path().to_str().unwrap());
        let _ret = EnvGuard::set("DARKMUX_LIVENESS_RETENTION_HOURS", "1");

        let dir = tmp.path().join("liveness");
        fs::create_dir_all(&dir).unwrap();
        let pid = std::process::id();
        let stale = dir.join(format!("{pid}.log"));
        fs::write(&stale, "leftover from an unrelated dispatch that once held this same pid\n").unwrap();
        set_mtime(&stale, SystemTime::now() - Duration::from_secs(2 * 3600));

        liveness("process-start");

        let body = fs::read_to_string(&stale).unwrap();
        assert!(
            !body.contains("leftover from an unrelated dispatch"),
            "a stale same-pid file past the retention window must be replaced, not appended \
             into — body was:\n{body}"
        );
        assert!(body.contains("process-start"), "the new marker must still land: {body}");
    }
}
