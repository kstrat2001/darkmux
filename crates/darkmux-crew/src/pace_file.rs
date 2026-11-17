//! (#2706) The pace file — `<host_out>/pace.json`, mounted into the
//! container at `/darkmux-out/pace.json`, read by `runtime/src/pace.rs`.
//!
//! **Why this is its own module.** Until #2706 the only writer was
//! [`crate::thermal_governor`], so the path formula, the wire shape and the
//! atomic write lived there as private items. #2706 adds a SECOND writer
//! (the battery governor), and the one thing worse than two governors is
//! two spellings of one file — a divergent `written_at_ms`, a second
//! `rename` dance that isn't atomic, a `reason` the runtime doesn't parse.
//! Promoting the writer here is the same "building blocks over bespoke
//! paths" move the StepKind tiering enforces one layer up: a third writer
//! reuses this or states why it can't.
//!
//! **The heartbeat contract (#2114 cf1b1993) is inherited, not optional.**
//! The runtime honors a pause only while `written_at_ms` is fresher than
//! its own ceiling — there is NO per-reason opt-out, only an ACTIVE WRITER.
//! "Indefinite" is expressed as "someone keeps renewing it", never as a
//! flag. Any governor that holds a pause must therefore keep re-stamping
//! for as long as it holds it; a writer that stamps once and goes quiet has
//! written an expiring pause, whatever it meant.
//!
//! **Who may write what: a live pause is never written over (2026-11-17
//! layer review).** Three writers share this file: the thermal governor
//! (its tiers, including tier 2's duty cycle), the battery governor, and the
//! budget pacer. Each decides on its own, and each yields by reading the
//! others' LIVE state rather than the file, through [`OtherPacing`]:
//!
//! - **A pause may always be written.** `pause: true` over another
//!   writer's pause stops nothing that was running. Precedence for which
//!   reason is shown is thermal, then battery, then budget: the battery
//!   governor writes nothing while thermal pauses, and the pacer writes
//!   nothing while either governor pauses.
//! - **A release is written only when no other writer holds a live
//!   pause.** A release is anything that lets the run take turns:
//!   `pause: false`, or the duty cycle's `pause: false` with a turn delay.
//!   A writer whose release would land on another's live pause writes
//!   nothing, and that pause stays in force.
//! - **Whoever lets go last writes the release, through [`release`].** It
//!   restores the thermal duty cycle's instruction when one is in force,
//!   because a writer that yielded its own write never gets to repeat it.
//!
//! The governors decide in one synchronous tick (thermal, then battery, then
//! the pacer), but the runtime polls this file on its own cadence, so the
//! rule holds for every write, not just for whatever the file says when the
//! tick ends.
//!
//! **Not under `/workspace`** (operator correction during the #2110/#2109
//! review): crawl units mount that read-only and a coder run's workspace IS
//! the operator's own repo tree — the wrong place for darkmux's own
//! bookkeeping. The out-dir is the right home, beside `.prompt.txt` and the
//! trajectory.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// `<host_out>/pace.json` — mounted into the container at
/// `/darkmux-out/pace.json`. MUST match the runtime-side join in
/// `runtime/src/pace.rs`'s `pace_file_path` (`out_dir.join("pace.json")`);
/// `thermal_governor`'s `pace_file_path_matches_runtime_out_base` is the
/// conformance test that keeps the two literals in sync.
pub fn path(host_out: &Path) -> PathBuf {
    host_out.join("pace.json")
}

/// The shape written to the pace file. Mirrors `runtime/src/pace.rs`'s
/// `PaceFile` fields (`pause`/`reason`/`state`/`written_at_ms`) exactly —
/// there is deliberately no `expires` field: #2114's cf1b1993 replaced that
/// flag with the pure heartbeat contract in this module's doc, so writing
/// an `expires` key here would be dead data the runtime no longer reads.
/// The runtime-side reader tolerates unknown/extra fields on deserialize
/// (no `deny_unknown_fields`), so this stays forward-compatible regardless.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct PaceFile<'a> {
    pause: bool,
    reason: &'a str,
    state: String,
    written_at_ms: u64,
    /// (#2774 tier 2) The pace file's THIRD state, between plain "run" and
    /// "pause": a host-set turn delay the runtime applies at the next turn
    /// boundary (through its own budget clamp) instead of blocking
    /// entirely. Absent on every write that carries no duty-cycle
    /// instruction — a plain pause, a plain resume, or a governor build
    /// that predates this field — which the runtime-side reader treats
    /// identically to `None` (see `runtime/src/pace.rs`'s own
    /// `turn_delay_ms` field doc).
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_delay_ms: Option<u64>,
}

/// What the OTHER writers of the pace file hold on the tick a writer
/// decides, read from their live state (see this module's doc for the rule
/// it carries).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OtherPacing {
    /// Another writer holds a genuine pause: a release from this writer
    /// writes nothing.
    pub pausing: bool,
    /// The thermal governor is duty-cycling (`pause: false` with a turn
    /// delay): `(turn_delay_ms, state)`. A writer that lets go of the file
    /// writes this instruction back rather than a bare `pause: false`.
    pub duty_cycle: Option<(u64, String)>,
}

/// The `reason` the thermal duty cycle writes, restored verbatim when
/// another writer lets go of the file during a duty-cycle episode.
pub const THERMAL_DUTY_REASON: &str = "thermal-duty-cycle";

/// Let go of the pace file on behalf of a writer whose pause ended. Writes
/// nothing while another writer holds a live pause (theirs stays in force,
/// and they write the release when they let go); otherwise writes the
/// thermal duty cycle's instruction when one is in force, else
/// `pause: false` with this writer's `reason` and `state`. Returns whether
/// it wrote.
pub(crate) fn release(host_out: &Path, others: &OtherPacing, reason: &str, state: &str) -> bool {
    if others.pausing {
        return false;
    }
    match &others.duty_cycle {
        Some((delay, thermal_state)) => {
            write_with_turn_delay(host_out, false, THERMAL_DUTY_REASON, thermal_state, Some(*delay))
        }
        None => write(host_out, false, reason, state),
    }
    true
}

/// UNIX epoch milliseconds. `0` on the vanishingly rare
/// clock-before-`UNIX_EPOCH` failure, matching every other clock read in
/// this crate.
fn now_epoch_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Write the pace file atomically: a temp file in the SAME directory (so
/// the rename is same-filesystem, hence atomic on both APFS and common
/// Linux filesystems) followed by `rename` onto the real path.
///
/// Without this, the runtime's poll (`PaceReader::read`, on its own ~2s
/// cadence, fully independent of any writer's cadence) could observe a
/// truncated file mid-write and treat it as malformed — logged once,
/// harmless, but a needless false alarm every time the two cadences race.
///
/// Best-effort by design: a failed write is pacing/observability, never
/// fatal to the dispatch itself, matching every other sampler-adjacent
/// write in `dispatch_internal.rs`.
///
/// `reason` is the vocabulary the runtime and the flow stream share —
/// `"thermal"` / `"thermal-critical"` (#2110/#2109) and `"battery"`
/// (#2706). It is a plain `&str` rather than a closed enum on purpose: the
/// runtime treats it as opaque text it echoes back, so a new governor adds
/// a word without a cross-crate type change.
pub(crate) fn write(host_out: &Path, pause: bool, reason: &str, state: &str) {
    write_with_turn_delay(host_out, pause, reason, state, None)
}

/// (#2774 tier 2) Same atomic write as [`write`], plus the pace file's
/// third state: a host-set turn delay (`turn_delay_ms`), for a writer that
/// wants the runtime to keep working but pace itself between turns rather
/// than stop outright. `write` is a thin `None` alias over this so its nine
/// existing call sites (pure pause/resume) never had to change shape.
pub(crate) fn write_with_turn_delay(
    host_out: &Path,
    pause: bool,
    reason: &str,
    state: &str,
    turn_delay_ms: Option<u64>,
) {
    let pace =
        PaceFile { pause, reason, state: state.to_string(), written_at_ms: now_epoch_ms(), turn_delay_ms };
    let _ = std::fs::create_dir_all(host_out);
    let Ok(json) = serde_json::to_string(&pace) else { return };
    // Unique per-write tmp name (pid + epoch-ns) — with two governors now
    // able to write the same file, a name shared between them would be a
    // real collision rather than the theoretical one it was under a single
    // writer.
    let tmp_path = host_out.join(format!(
        ".pace.json.tmp.{}.{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
    ));
    if std::fs::write(&tmp_path, &json).is_ok() {
        let _ = std::fs::rename(&tmp_path, path(host_out));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_written_shape_is_exactly_what_the_runtime_reads() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), true, "battery", "38%");
        let raw = std::fs::read_to_string(path(dir.path())).expect("pace file written");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        assert_eq!(v["pause"], true);
        assert_eq!(v["reason"], "battery");
        assert_eq!(v["state"], "38%");
        assert!(v["written_at_ms"].as_u64().unwrap_or(0) > 0, "the heartbeat stamp is load-bearing: {raw}");
        assert!(
            v.get("expires").is_none(),
            "#2114 cf1b1993 replaced the expires flag with a pure heartbeat contract — writing \
             one would be dead data the runtime no longer reads"
        );
    }

    #[test]
    fn a_second_write_replaces_the_first_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), true, "battery", "38%");
        write(dir.path(), false, "battery", "62%");
        let raw = std::fs::read_to_string(path(dir.path())).expect("pace file");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        assert_eq!(v["pause"], false, "the newer write wins");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("readdir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".pace.json.tmp."))
            .collect();
        assert!(leftovers.is_empty(), "the rename must consume the temp file: {leftovers:?}");
    }

    #[test]
    fn write_with_turn_delay_carries_the_third_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_with_turn_delay(dir.path(), false, "thermal-duty-cycle", "fair", Some(15_000));
        let raw = std::fs::read_to_string(path(dir.path())).expect("pace file written");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        assert_eq!(v["pause"], false, "duty-cycle is NOT a pause");
        assert_eq!(v["turn_delay_ms"], 15_000);
    }

    #[test]
    fn plain_write_omits_turn_delay_ms_entirely() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), true, "thermal", "serious");
        let raw = std::fs::read_to_string(path(dir.path())).expect("pace file written");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        assert!(
            v.get("turn_delay_ms").is_none(),
            "an ordinary pause/resume write must not carry a stale or spurious turn_delay_ms key: {raw}"
        );
    }

    #[test]
    fn a_release_writes_nothing_over_another_writers_live_pause() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), true, "battery", "38%");
        let held = OtherPacing { pausing: true, duty_cycle: Some((15_000, "fair".into())) };
        assert!(!release(dir.path(), &held, "budget", "ep"), "a live pause is never written over");
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path(dir.path())).unwrap()).unwrap();
        assert_eq!((v["pause"].as_bool(), v["reason"].as_str()), (Some(true), Some("battery")));
    }

    #[test]
    fn a_release_restores_the_duty_cycle_or_writes_a_plain_resume() {
        let dir = tempfile::tempdir().expect("tempdir");
        let duty = OtherPacing { pausing: false, duty_cycle: Some((15_000, "fair".into())) };
        assert!(release(dir.path(), &duty, "battery", "60%"));
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path(dir.path())).unwrap()).unwrap();
        assert_eq!(
            (v["pause"].as_bool(), v["reason"].as_str(), v["state"].as_str(), v["turn_delay_ms"].as_u64()),
            (Some(false), Some(THERMAL_DUTY_REASON), Some("fair"), Some(15_000))
        );
        assert!(release(dir.path(), &OtherPacing::default(), "battery", "60%"));
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path(dir.path())).unwrap()).unwrap();
        assert_eq!((v["pause"].as_bool(), v["reason"].as_str(), v.get("turn_delay_ms")), (Some(false), Some("battery"), None));
    }

    #[test]
    fn the_path_is_the_out_dir_join_the_runtime_performs() {
        assert_eq!(path(Path::new("/darkmux-out")), PathBuf::from("/darkmux-out/pace.json"));
    }
}
