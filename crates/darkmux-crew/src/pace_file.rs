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
    let pace = PaceFile { pause, reason, state: state.to_string(), written_at_ms: now_epoch_ms() };
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
    fn the_path_is_the_out_dir_join_the_runtime_performs() {
        assert_eq!(path(Path::new("/darkmux-out")), PathBuf::from("/darkmux-out/pace.json"));
    }
}
