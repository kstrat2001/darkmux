//! Host-driven pause file (#2114).
//!
//! Between turns the loop checks `<out_dir>/pace.json` — `out_dir` is the
//! container's `/darkmux-out` mount (`trajectory::RUNTIME_OUT_BASE`), the
//! SAME always-writable, never-`:ro` bookkeeping dir trajectory/findings
//! already use. NOT the `/workspace` mount: `/workspace` is read-only for
//! crawl-kind dispatches (#1959) and, when writable, is the operator's own
//! repo tree — a `.darkmux/pace.json` landing there would either EROFS on
//! every poll or leave an untracked file inside the operator's checkout.
//! While it holds `pause: true` the loop rests in bounded increments
//! rather than exiting, re-reading the file each increment, so a
//! host-side pause (the thermal governor, #2110) never looks like a stall
//! to the runtime's own inactivity clock — each increment counts as
//! proof-of-work the same way #2094's `turn_delay_ms` rest does.
//!
//! Absent file = the overwhelmingly common case (no pause active) and is
//! NOT an error. A malformed file is ignored — logged once so a broken
//! writer is visible without spamming stderr once per 2s poll.
//!
//! A pause is a HEARTBEAT, not a standing instruction: it is honored only
//! while `written_at_ms` is fresher than `max_pause_ms` (default 900_000 /
//! 15 min, override `DARKMUX_MAX_PAUSE_MS`) — see [`PaceFile::is_expired`].
//! There is no separate "hold indefinitely" flag; a governor that wants
//! the pause to keep holding just keeps RE-STAMPING `written_at_ms` while
//! it wants the dispatch parked (the #2110/#2109 breaker's
//! `thermal-critical` stop does this by re-writing the file on its own
//! polling cadence, well inside `max_pause_ms`). A dead or hung writer
//! stops re-stamping, the stamp ages past the ceiling, and the pause
//! releases itself — "indefinite" is expressed as "someone keeps renewing
//! it," never as a flag that opts out of the ceiling, so there is exactly
//! ONE expiry rule and no way for a stuck writer to hold a dispatch
//! forever by mistake. Each rest increment resets the HOST'S inactivity
//! deadline too, so this ceiling is what stops an honored pause from
//! being an unbounded dodge of that watchdog.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Default ceiling on how old a pace file's `written_at_ms` may be while
/// still being honored — 15 minutes. Override via `DARKMUX_MAX_PAUSE_MS`.
pub const DEFAULT_MAX_PAUSE_MS: u64 = 900_000;

/// Shape of `.darkmux/pace.json`. All fields optional/defaulted so a
/// partial or forward-compat file still parses.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PaceFile {
    #[serde(default)]
    pub pause: bool,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    /// Unix ms the writer last touched this file. `None` (an older or
    /// hand-written pace file) is treated as fresh — the expiry guard only
    /// engages when a timestamp is actually present, so an operator's
    /// manual `{"pause": true}` isn't punished for omitting a field it
    /// never knew about. A writer that wants the pause to keep holding
    /// re-stamps this on each write — see the module doc's heartbeat
    /// contract.
    #[serde(default)]
    pub written_at_ms: Option<u64>,
}

impl PaceFile {
    /// The reason string to stamp on a `runtime.rest` event — the
    /// operator-supplied reason, or a generic fallback when the pace file
    /// set `pause: true` without one.
    pub fn reason_or_default(&self) -> String {
        self.reason.clone().unwrap_or_else(|| "paused".to_string())
    }

    /// Whether this pause is too STALE to honor — `written_at_ms` is more
    /// than `max_pause_ms` behind `now_ms`. A pace file with no timestamp
    /// never expires (see the field's own doc). Saturating on both sides
    /// so a clock skew (writer ahead of reader) can't underflow into a
    /// false "expired". This is the ONLY expiry rule (#2114 finding N3) —
    /// there is no per-reason override; a writer that wants to hold past
    /// one poll interval re-stamps `written_at_ms`, it doesn't opt out of
    /// this check.
    pub fn is_expired(&self, now_ms: u64, max_pause_ms: u64) -> bool {
        match self.written_at_ms {
            Some(written_at) => now_ms.saturating_sub(written_at) > max_pause_ms,
            None => false,
        }
    }
}

/// `env(DARKMUX_MAX_PAUSE_MS) > DEFAULT_MAX_PAUSE_MS`. Read once at
/// dispatch startup, same as `DARKMUX_INACTIVITY_TIMEOUT_SECONDS` and the
/// other host-forwarded env knobs the loop resolves once, not per poll.
pub fn max_pause_ms() -> u64 {
    std::env::var("DARKMUX_MAX_PAUSE_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MAX_PAUSE_MS)
}

/// `<out_dir>/pace.json` — `out_dir` is the container's `/darkmux-out`
/// mount in production (`trajectory::RUNTIME_OUT_BASE`), a tempdir in
/// tests. See the module doc for why this moved off `<workspace>/.darkmux`.
pub fn pace_file_path(out_dir: &Path) -> PathBuf {
    out_dir.join("pace.json")
}

/// Tracks whether we've already warned about a malformed pace file, so the
/// warning fires once per malformed-window rather than once per 2s poll.
#[derive(Default)]
pub struct PaceReader {
    warned_malformed: bool,
}

impl PaceReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read + parse the pace file. `None` covers both "file absent" (the
    /// common no-pause case) and "malformed" (ignored, logged once) —
    /// callers can't and don't need to distinguish the two: either way
    /// there's no pause instruction to act on.
    pub fn read(&mut self, out_dir: &Path) -> Option<PaceFile> {
        let path = pace_file_path(out_dir);
        let contents = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str::<PaceFile>(&contents) {
            Ok(pace) => {
                // A subsequent valid write clears the warn state, so a
                // fixed file after a bad edit gets a fresh chance to warn
                // if it breaks again.
                self.warned_malformed = false;
                Some(pace)
            }
            Err(e) => {
                if !self.warned_malformed {
                    eprintln!(
                        "darkmux-runtime: ⚠ malformed pace file at {}: {e} (ignoring, treating as no pause)",
                        path.display()
                    );
                    self.warned_malformed = true;
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_file_reads_as_none() {
        let ws = tempfile::tempdir().unwrap();
        let mut reader = PaceReader::new();
        assert_eq!(reader.read(ws.path()), None);
    }

    #[test]
    fn valid_pause_file_parses() {
        let out_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            pace_file_path(out_dir.path()),
            r#"{"pause": true, "reason": "thermal", "state": "hot"}"#,
        )
        .unwrap();
        let mut reader = PaceReader::new();
        let pace = reader.read(out_dir.path()).unwrap();
        assert!(pace.pause);
        assert_eq!(pace.reason.as_deref(), Some("thermal"));
        assert_eq!(pace.state.as_deref(), Some("hot"));
    }

    #[test]
    fn malformed_file_is_ignored_not_fatal() {
        let out_dir = tempfile::tempdir().unwrap();
        std::fs::write(pace_file_path(out_dir.path()), "{not json").unwrap();
        let mut reader = PaceReader::new();
        assert_eq!(reader.read(out_dir.path()), None);
    }

    #[test]
    fn reason_or_default_falls_back() {
        let pace = PaceFile { pause: true, reason: None, state: None, written_at_ms: None };
        assert_eq!(pace.reason_or_default(), "paused");
    }

    #[test]
    fn no_timestamp_never_expires() {
        let pace = PaceFile { pause: true, reason: None, state: None, written_at_ms: None };
        assert!(!pace.is_expired(1_000_000_000, 900_000));
    }

    #[test]
    fn fresh_timestamp_is_not_expired() {
        let pace = PaceFile {
            pause: true,
            reason: None,
            state: None,
            written_at_ms: Some(1_000_000_000),
        };
        assert!(!pace.is_expired(1_000_000_000 + 899_999, 900_000));
    }

    #[test]
    fn stale_timestamp_past_max_pause_is_expired() {
        let pace = PaceFile {
            pause: true,
            reason: None,
            state: None,
            written_at_ms: Some(1_000_000_000),
        };
        assert!(pace.is_expired(1_000_000_000 + 900_001, 900_000));
    }

    #[test]
    fn clock_skew_does_not_underflow_into_false_expiry() {
        // Writer's clock is AHEAD of the reader's — `now_ms < written_at_ms`.
        // `saturating_sub` must not wrap this into a huge "elapsed".
        let pace = PaceFile {
            pause: true,
            reason: None,
            state: None,
            written_at_ms: Some(2_000_000),
        };
        assert!(!pace.is_expired(1_000_000, 900_000));
    }

    #[test]
    fn a_fresh_re_stamp_keeps_it_paused_regardless_of_reason() {
        // (#2114 finding N3) The heartbeat contract: there's no
        // "hold indefinitely" flag — a governor that wants to keep the
        // dispatch parked past one poll interval just re-writes
        // `written_at_ms`. Simulate that: the SAME pause instruction,
        // re-stamped just now, stays unexpired even though its ORIGINAL
        // timestamp (not modeled here — only the latest stamp matters) may
        // have been written long ago.
        let pace = PaceFile {
            pause: true,
            reason: Some("thermal-critical".into()),
            state: None,
            written_at_ms: Some(5_000_000),
        };
        assert!(
            !pace.is_expired(5_000_000 + 1_000, 900_000),
            "a stamp re-written well inside max_pause_ms stays fresh, whatever the reason"
        );
    }

    #[test]
    fn a_stale_stamp_expires_regardless_of_reason() {
        // Mirror of the test above: once nobody has re-stamped the file
        // for longer than max_pause_ms, it expires — a
        // `thermal-critical`-labeled pause gets NO special exemption, on
        // purpose. Holding past the ceiling requires an active writer,
        // not a reason string.
        let pace = PaceFile {
            pause: true,
            reason: Some("thermal-critical".into()),
            state: None,
            written_at_ms: Some(1_000_000_000),
        };
        assert!(pace.is_expired(1_000_000_000 + 900_001, 900_000));
    }
}
