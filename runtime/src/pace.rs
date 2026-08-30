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
//! ## Writer contract (#2114 finding 2)
//!
//! A writer MUST publish `pace.json` atomically: serialize to a sibling
//! temp file, then `rename` it over the real path (the same pattern
//! `checkpoint::write_checkpoint` uses on this runtime's own writes).
//! `rename` is atomic on POSIX when both paths share a filesystem, so a
//! reader never observes a half-written file from a CONFORMING writer.
//! But the runtime does not control the writer (the #2110/#2109
//! governor/breaker is a separate process, possibly a separate
//! implementation entirely down the line), so a torn read is still
//! possible in principle — a non-conforming writer, a filesystem that
//! doesn't honor POSIX rename atomicity, or a read landing in the
//! microscopic window most `rename` implementations still leave open on
//! some platforms. **A torn read must never release a live pause.** See
//! `PaceReader::read`'s tolerance for how this is enforced: a parse
//! failure while the reader's last known-good state was `pause: true`
//! holds that cached state for one more poll interval (logged once)
//! instead of falling through to "no pause." A dead or genuinely
//! misbehaving writer is still caught, because the CACHED
//! `written_at_ms` stops advancing while reads keep failing, so the
//! `max_pause_ms` ceiling below still fires on schedule.
//!
//! Absent file = the overwhelmingly common case (no pause active) and is
//! NOT an error — and unlike a torn read, a genuinely ABSENT file releases
//! a cached pause immediately (see `PaceReader::read`): deleting the file
//! is the operator's/governor's own explicit "stop pausing" signal, not a
//! read race, and tolerating it the same way as a malformed read would
//! turn a deliberate release into a delayed one.
//!
//! ## The heartbeat + `max_pause_ms` ceiling
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
//! releases itself. Each rest increment resets the HOST'S inactivity
//! deadline too, so this ceiling is what stops an honored pause from
//! being an unbounded dodge of that watchdog.
//!
//! **This bounds a STUCK writer, not a clock fault in either direction —
//! see the next section.** "No way to hold forever" is true only when the
//! writer's and reader's clocks agree closely enough that `max_pause_ms`
//! means the same thing to both; #2114 finding 3 narrows that claim and
//! covers the gap.
//!
//! ## Clock skew, both directions (#2114 finding 3)
//!
//! `written_at_ms` and `now_ms` are read from two DIFFERENT clocks — the
//! writer's (host or governor process) and the reader's (this runtime,
//! inside the Docker VM) — so `is_expired`'s arithmetic is only as
//! trustworthy as those two clocks' agreement:
//!
//! - **Container clock BEHIND the host** (the ordinary skew direction: a
//!   VM clock that's simply slower or was paused during a host suspend) —
//!   `now_ms.saturating_sub(written_at)` reads LARGER than the true
//!   elapsed time, so a live, actively-re-stamped pause reads as staler
//!   than it is. In the worst case this makes a genuinely live hold
//!   expire early — annoying (an extra request goes out mid-pause) but
//!   never dangerous, and it self-corrects: the very next re-stamp
//!   re-establishes freshness against the same (still-behind) clock. The
//!   ceiling's failure mode on this side is "drops a live hold a bit
//!   early," not "holds forever."
//! - **Container clock AHEAD of the host by more than `max_pause_ms`, or
//!   a writer that stamps a genuinely future `written_at_ms`** —
//!   `saturating_sub` clamps the negative difference to 0, which
//!   `is_expired` reads as "0ms elapsed" — i.e. maximally fresh, forever,
//!   as long as the SAME future-dated stamp sits there unchanged. Left
//!   unguarded this is exactly the infinite-hold failure mode the
//!   heartbeat design exists to prevent, just reached via a clock fault
//!   instead of a missing re-stamp.
//!
//! The second case is why `is_expired` alone is not the full story:
//! `PaceReader::pause_is_expired` adds a stamp-in-the-future guard on top
//! of it. A `written_at_ms` more than `max_pause_ms` ahead of `now_ms` is
//! treated as UNKNOWN rather than confirmed-live — held for one more poll
//! interval (the same one-interval tolerance the torn-read case gets),
//! then expired if the identical anomaly is still present on the NEXT
//! poll. A live, correctly-behaving writer never trips this twice in a
//! row: each of its heartbeats stamps a FRESH `written_at_ms` close to
//! ITS OWN clock's "now," so if that clock is merely ahead by a bounded,
//! roughly-constant amount, successive stamps keep moving forward with
//! real time and the skew from `now_ms` stays roughly the same — never
//! silently growing past `max_pause_ms` on its own between one heartbeat
//! and the next unless the writer's clock itself is jumping. A writer
//! whose clock is free-running arbitrarily far ahead, or that stops
//! re-stamping while an old future-dated value lingers, is exactly the
//! stuck/misbehaving case this guard exists to catch.

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
    /// false "expired" — see the module doc's clock-skew section: THIS
    /// function only covers the writer-behind-reader direction cleanly.
    /// The writer-AHEAD-of-reader direction (`written_at_ms` in the
    /// future) reads as "0 elapsed" here, which is why callers use
    /// [`PaceReader::pause_is_expired`] rather than this fn directly —
    /// that wraps this check with the stamp-in-the-future guard. This is
    /// the ONLY staleness rule otherwise (#2114 finding N3) — there is no
    /// per-reason override; a writer that wants to hold past one poll
    /// interval re-stamps `written_at_ms`, it doesn't opt out of this
    /// check.
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

/// Per-dispatch pace-file reader. Tracks enough state across polls to
/// implement two tolerances the module doc describes: (1) a malformed
/// read while previously paused holds the cached pause instead of
/// releasing it (#2114 finding 2), and (2) a stamp-in-the-future gets ONE
/// grace interval before being treated as expired (#2114 finding 3).
#[derive(Default)]
pub struct PaceReader {
    warned_malformed: bool,
    /// Edge-trigger for the "malformed while a cached pause exists"
    /// warning — separate from `warned_malformed` so switching between
    /// the two failure shapes (e.g. file goes malformed, gets fixed,
    /// then goes malformed again while paused) still warns once per
    /// shape rather than being silenced by the other's flag.
    warned_malformed_while_paused: bool,
    /// The last SUCCESSFULLY parsed pace file. `None` once the file goes
    /// missing (a deliberate delete releases immediately, no tolerance)
    /// or before the first successful read this dispatch.
    last_good: Option<PaceFile>,
    /// Stamp-in-the-future grace tracker (finding 3): `Some(written_at)`
    /// once a grace interval has been granted to that EXACT
    /// `written_at_ms` value. A REPEAT of the same value expires (the
    /// writer froze); a DIFFERENT future-dated value (a live writer's
    /// clock-skewed but still-advancing heartbeat) gets its own fresh
    /// grace instead of inheriting the exhausted one.
    future_skew_grace_used: Option<u64>,
}

impl PaceReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read + parse the pace file.
    ///
    /// - **File absent**: clears any cached state and returns `None` —
    ///   the common no-pause case, and also the operator's/governor's
    ///   explicit "stop pausing" signal (a deleted file), which must
    ///   release immediately rather than being smoothed over by the
    ///   malformed-read tolerance below.
    /// - **Malformed, no prior pause cached** (or the cached state wasn't
    ///   paused): returns `None`, logged once — same as before finding 2.
    /// - **Malformed, but the last successful read was `pause: true`**
    ///   (#2114 finding 2): returns the CACHED pace file instead of
    ///   `None`, logged once. This is the torn-read tolerance the module
    ///   doc's writer contract section describes — a one-off read glitch
    ///   must not silently drop a live pause. A genuinely dead writer is
    ///   still caught: the cached `written_at_ms` doesn't advance while
    ///   reads keep failing, so `max_pause_ms` (via
    ///   [`PaceReader::pause_is_expired`]) still expires it on schedule.
    pub fn read(&mut self, out_dir: &Path) -> Option<PaceFile> {
        let path = pace_file_path(out_dir);
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => {
                self.last_good = None;
                self.warned_malformed = false;
                self.warned_malformed_while_paused = false;
                return None;
            }
        };
        match serde_json::from_str::<PaceFile>(&contents) {
            Ok(pace) => {
                // A subsequent valid write clears both warn states, so a
                // fixed file after a bad edit gets a fresh chance to warn
                // if it breaks again.
                self.warned_malformed = false;
                self.warned_malformed_while_paused = false;
                self.last_good = Some(pace.clone());
                Some(pace)
            }
            Err(e) => {
                if let Some(cached) = self.last_good.clone().filter(|p| p.pause) {
                    if !self.warned_malformed_while_paused {
                        eprintln!(
                            "darkmux-runtime: ⚠ malformed pace file at {}: {e} (was paused — \
                             holding the last-known pause state for this interval rather than \
                             releasing; a dead writer still expires via max_pause_ms)",
                            path.display()
                        );
                        self.warned_malformed_while_paused = true;
                    }
                    return Some(cached);
                }
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

    /// (#2114 finding 3) Whether `pace`'s pause should be treated as
    /// expired — [`PaceFile::is_expired`] plus a stamp-in-the-future
    /// guard the pure fn can't express on its own (it has no memory
    /// across polls). See the module doc's clock-skew section for the
    /// full reasoning; summary:
    ///
    /// - `written_at_ms` in the past, past `max_pause_ms`: expired
    ///   (delegates to `is_expired`).
    /// - `written_at_ms` more than `max_pause_ms` in the FUTURE relative
    ///   to `now_ms`: treated as unknown rather than confirmed-live. The
    ///   FIRST time this is seen, it's held for one more poll interval
    ///   (not expired yet); if the SAME anomaly is still present on the
    ///   NEXT call, it expires. A live writer's own heartbeats keep
    ///   moving `written_at_ms` forward each interval, so this can't
    ///   trip twice in a row against a correctly-behaving writer.
    /// - Otherwise: not expired, and the future-skew grace resets so a
    ///   later anomaly gets its own fresh one-interval grace.
    pub fn pause_is_expired(&mut self, pace: &PaceFile, now_ms: u64, max_pause_ms: u64) -> bool {
        if pace.is_expired(now_ms, max_pause_ms) {
            self.future_skew_grace_used = None;
            return true;
        }
        let future_skew_ms = pace
            .written_at_ms
            .map(|written_at| written_at.saturating_sub(now_ms))
            .unwrap_or(0);
        if future_skew_ms > max_pause_ms {
            if self.future_skew_grace_used == pace.written_at_ms {
                // The IDENTICAL future-dated stamp granted grace last
                // time is still sitting there unchanged — the writer
                // froze (or was never live to begin with). Expire.
                true
            } else {
                // Either the first sighting, or a DIFFERENT (moved
                // forward) future-dated stamp — a live writer's own
                // heartbeat, even if clock-skewed. Grant this one its
                // own fresh grace interval.
                self.future_skew_grace_used = pace.written_at_ms;
                false
            }
        } else {
            self.future_skew_grace_used = None;
            false
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

    // ===== (#2114 finding 2) Malformed-while-paused tolerance =====

    #[test]
    fn malformed_read_while_previously_paused_holds_the_cached_pause() {
        let out_dir = tempfile::tempdir().unwrap();
        let mut reader = PaceReader::new();
        std::fs::write(
            pace_file_path(out_dir.path()),
            r#"{"pause": true, "reason": "thermal", "written_at_ms": 1000}"#,
        )
        .unwrap();
        let first = reader.read(out_dir.path()).unwrap();
        assert!(first.pause);

        // Simulate a torn read: the file now holds garbage (e.g. a
        // partial write outside the tmp+rename contract).
        std::fs::write(pace_file_path(out_dir.path()), "{not json").unwrap();
        let held = reader.read(out_dir.path());
        assert_eq!(
            held,
            Some(PaceFile {
                pause: true,
                reason: Some("thermal".into()),
                state: None,
                written_at_ms: Some(1000),
            }),
            "a malformed read while previously paused must return the CACHED pace file, not None"
        );
    }

    #[test]
    fn malformed_read_with_no_prior_pause_still_returns_none() {
        // No cached state at all (first read is malformed) — no pause to
        // hold, so this must behave exactly like the pre-finding-2 path.
        let out_dir = tempfile::tempdir().unwrap();
        let mut reader = PaceReader::new();
        std::fs::write(pace_file_path(out_dir.path()), "{not json").unwrap();
        assert_eq!(reader.read(out_dir.path()), None);
    }

    #[test]
    fn malformed_read_after_a_non_paused_cache_returns_none() {
        // The cache exists but its last known state was pause:false — a
        // malformed read here has nothing live to protect, so it must
        // fall through to None rather than resurrecting a stale
        // NOT-paused snapshot as if it were meaningful.
        let out_dir = tempfile::tempdir().unwrap();
        let mut reader = PaceReader::new();
        std::fs::write(pace_file_path(out_dir.path()), r#"{"pause": false}"#).unwrap();
        reader.read(out_dir.path()).unwrap();
        std::fs::write(pace_file_path(out_dir.path()), "{not json").unwrap();
        assert_eq!(reader.read(out_dir.path()), None);
    }

    #[test]
    fn deleting_the_file_releases_a_cached_pause_immediately() {
        // Deletion is the operator's/governor's deliberate release
        // signal, NOT a read race — it must NOT get the malformed-read
        // tolerance.
        let out_dir = tempfile::tempdir().unwrap();
        let mut reader = PaceReader::new();
        std::fs::write(pace_file_path(out_dir.path()), r#"{"pause": true}"#).unwrap();
        reader.read(out_dir.path()).unwrap();
        std::fs::remove_file(pace_file_path(out_dir.path())).unwrap();
        assert_eq!(reader.read(out_dir.path()), None);
    }

    #[test]
    fn a_valid_write_after_a_held_malformed_read_clears_the_tolerance_state() {
        // Once the writer produces a valid file again, the reader must
        // reflect it immediately — not keep serving the stale cache.
        let out_dir = tempfile::tempdir().unwrap();
        let mut reader = PaceReader::new();
        std::fs::write(pace_file_path(out_dir.path()), r#"{"pause": true}"#).unwrap();
        reader.read(out_dir.path()).unwrap();
        std::fs::write(pace_file_path(out_dir.path()), "{not json").unwrap();
        reader.read(out_dir.path()).unwrap();
        std::fs::write(pace_file_path(out_dir.path()), r#"{"pause": false}"#).unwrap();
        let resumed = reader.read(out_dir.path()).unwrap();
        assert!(!resumed.pause, "a fresh valid write must be reflected, not the stale cache");
    }

    // ===== (#2114 finding 3) Stamp-in-the-future guard =====

    #[test]
    fn a_stamp_far_in_the_future_holds_for_one_interval_then_expires() {
        let mut reader = PaceReader::new();
        let pace = PaceFile {
            pause: true,
            reason: Some("thermal".into()),
            state: None,
            written_at_ms: Some(10_000_000), // way ahead of now_ms below
        };
        let now_ms = 1_000_000;
        let max_pause_ms = 900_000;

        assert!(
            !reader.pause_is_expired(&pace, now_ms, max_pause_ms),
            "first sighting of a future-dated stamp gets one grace interval, not an instant expiry"
        );
        assert!(
            reader.pause_is_expired(&pace, now_ms, max_pause_ms),
            "the SAME anomaly seen again must expire — grace is one-time, not infinite hold"
        );
    }

    #[test]
    fn a_moderately_future_stamp_within_max_pause_ms_is_not_flagged() {
        // Small clock skew (well under max_pause_ms) is normal and must
        // not trip the future-skew guard at all.
        let mut reader = PaceReader::new();
        let pace = PaceFile {
            pause: true,
            reason: None,
            state: None,
            written_at_ms: Some(1_000_100), // 100ms ahead
        };
        assert!(!reader.pause_is_expired(&pace, 1_000_000, 900_000));
        assert!(!reader.pause_is_expired(&pace, 1_000_000, 900_000), "stays fine on repeat polls too");
    }

    #[test]
    fn a_live_writer_advancing_its_future_stamp_never_trips_twice_in_a_row() {
        // A writer whose clock is ahead by a roughly CONSTANT amount, but
        // still re-stamping on its own heartbeat cadence, produces a
        // DIFFERENT (moving forward) written_at_ms each poll — this must
        // never accumulate into an expiry as long as each poll sees a
        // fresh value.
        let mut reader = PaceReader::new();
        let now_ms = 1_000_000;
        let max_pause_ms = 900_000;
        for step in 0..5u64 {
            let pace = PaceFile {
                pause: true,
                reason: None,
                state: None,
                // Deliberately always > max_pause_ms ahead, but a NEW
                // value each time (a live, if clock-skewed, writer).
                written_at_ms: Some(now_ms + max_pause_ms + 1 + step),
            };
            assert!(
                !reader.pause_is_expired(&pace, now_ms, max_pause_ms),
                "step {step}: a fresh future-dated stamp each poll must not expire"
            );
        }
    }

    #[test]
    fn past_expiry_takes_priority_and_resets_future_skew_grace() {
        let mut reader = PaceReader::new();
        let future = PaceFile {
            pause: true,
            reason: None,
            state: None,
            written_at_ms: Some(10_000_000),
        };
        // Use up the future-skew grace.
        assert!(!reader.pause_is_expired(&future, 1_000_000, 900_000));

        // Now a NORMAL stale-in-the-past pace file — must expire via the
        // ordinary rule, independent of the future-skew grace state.
        let stale = PaceFile {
            pause: true,
            reason: None,
            state: None,
            written_at_ms: Some(1_000_000),
        };
        assert!(reader.pause_is_expired(&stale, 1_000_000 + 900_001, 900_000));
    }
}
