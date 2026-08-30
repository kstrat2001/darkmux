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
//! A pause is honored only while it's FRESH: `written_at_ms` older than
//! `max_pause_ms` (default 900_000 / 15 min, override `DARKMUX_MAX_PAUSE_MS`)
//! is treated as a stale/abandoned pace writer — see `PaceFile::is_expired`
//! — so a killed or hung governor process can never make the container
//! immortal (each rest increment resets the HOST'S inactivity deadline too,
//! so an unbounded honored pause is an unbounded dodge of that watchdog).
//!
//! Shared contract with the thermal governor/breaker (#2110/#2109,
//! `crates/darkmux-crew` — landing on its own branch, not yet on this
//! one): a pace file may set `"expires": false` to mean "hold this pause
//! until I explicitly lift it or remove the file" — the breaker writes
//! this for a genuine `thermal-critical` stop, where an operator wants
//! the dispatch parked indefinitely rather than falling through to a
//! runaway request the moment `max_pause_ms` elapses. Default (field
//! absent, or `true`) keeps the staleness ceiling above. `expires` is a
//! property of THIS pause instruction, so it's read fresh off each poll
//! the same as `pause`/`reason` — a governor can widen or narrow it on a
//! later write without the runtime needing to restart.

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
    /// never knew about.
    #[serde(default)]
    pub written_at_ms: Option<u64>,
    /// `false` disables the `max_pause_ms` staleness ceiling for THIS
    /// pause — the breaker's `thermal-critical` stop sets this so the
    /// dispatch stays parked until the file is explicitly resumed or
    /// removed, not until a timer elapses. `None` (absent — every writer
    /// before this contract, and the ordinary governor pause) behaves the
    /// same as `Some(true)`: the ceiling applies.
    #[serde(default)]
    pub expires: Option<bool>,
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
    /// false "expired". `expires: false` (the breaker's `thermal-critical`
    /// stop) opts THIS pause out of the ceiling entirely — checked first,
    /// so a stale-but-non-expiring pause never even evaluates the
    /// timestamp.
    pub fn is_expired(&self, now_ms: u64, max_pause_ms: u64) -> bool {
        if !self.expires.unwrap_or(true) {
            return false;
        }
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
        let pace =
            PaceFile { pause: true, reason: None, state: None, written_at_ms: None, expires: None };
        assert_eq!(pace.reason_or_default(), "paused");
    }

    #[test]
    fn no_timestamp_never_expires() {
        let pace =
            PaceFile { pause: true, reason: None, state: None, written_at_ms: None, expires: None };
        assert!(!pace.is_expired(1_000_000_000, 900_000));
    }

    #[test]
    fn fresh_timestamp_is_not_expired() {
        let pace = PaceFile {
            pause: true,
            reason: None,
            state: None,
            written_at_ms: Some(1_000_000_000),
            expires: None,
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
            expires: None,
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
            expires: None,
        };
        assert!(!pace.is_expired(1_000_000, 900_000));
    }

    #[test]
    fn stale_stamp_with_expires_false_stays_paused() {
        // (#2140 contract) The breaker's `thermal-critical` stop sets
        // `expires: false` — a stale `written_at_ms` must NOT expire it.
        let pace = PaceFile {
            pause: true,
            reason: Some("thermal-critical".into()),
            state: None,
            written_at_ms: Some(1_000_000_000),
            expires: Some(false),
        };
        assert!(
            !pace.is_expired(1_000_000_000 + 900_001, 900_000),
            "expires:false holds the pause past max_pause_ms"
        );
    }

    #[test]
    fn stale_stamp_with_expires_true_is_expired() {
        // Same staleness as the test above, but `expires: true` (the
        // default-equivalent, spelled out) — the ceiling applies exactly
        // like an absent `expires` field.
        let pace = PaceFile {
            pause: true,
            reason: Some("thermal".into()),
            state: None,
            written_at_ms: Some(1_000_000_000),
            expires: Some(true),
        };
        assert!(pace.is_expired(1_000_000_000 + 900_001, 900_000));
    }

    #[test]
    fn valid_pause_file_with_expires_false_parses() {
        let out_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            pace_file_path(out_dir.path()),
            r#"{"pause": true, "reason": "thermal-critical", "expires": false}"#,
        )
        .unwrap();
        let mut reader = PaceReader::new();
        let pace = reader.read(out_dir.path()).unwrap();
        assert_eq!(pace.expires, Some(false));
    }
}
