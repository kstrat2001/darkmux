//! (#2265) The FINDING record: what was observed, stored once, never rewritten.
//!
//! A finding is an EVENT. It happened at a moment, from a dispatch, and it is
//! never edited afterwards. Its key is `<dispatch>/<seq>` — the dispatch's
//! session id plus the ordinal of the acceptance within that dispatch — which
//! every finding has, crawl or not. A crawl adds `context` (mission, unit,
//! rule, source, sha) when it launches the dispatch; nothing about a finding
//! requires a crawl.
//!
//! **darkmux does not interpret the emission.** The record is metadata plus the
//! model's arguments verbatim (`emitted`); a hook's transform composes whatever
//! a destination needs from that. A finding's location is domain-specific — a
//! line for text, a page for a PDF, a rect for an image — so no field for it
//! exists on darkmux's side.
//!
//! Two producers write the same record, and they must not be able to disagree:
//! the dispatch tailer materializes it live as the accepted call streams past,
//! and `darkmux finding sync` replays the flow stream for anything the tailer
//! missed (an older binary, a killed process). Both go through
//! [`materialize`], which is **write-once**: an existing file is left exactly
//! as it is and reported as already present.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The record's own schema version. Bumped only when the shape below changes;
/// readers stay lenient (unknown fields ride in `extras`).
pub const FINDING_SCHEMA_VERSION: &str = "1";

/// The runtime tool names whose accepted calls become findings.
///
/// `report_finding` is the pre-2026-09-03 name (the tool *creates* a record; a
/// hook is what *reports* it). Historical records carry it, and the flow stream
/// is append-only and never rewritten, so the old name stays supported for
/// reading forever — the same read-side-fallback discipline `handle` gets.
pub const FINDING_TOOL_NAMES: [&str; 2] = ["create_finding", "report_finding"];

/// Whether a dispatch id is safe to use as a PATH SEGMENT under the store.
///
/// The id comes off the flow stream — untrusted input that ends up joined onto
/// a filesystem path — so anything that could resolve outside the store is
/// refused rather than sanitized: a separator of either kind, `.`/`..`, a
/// leading dot, an empty string. A refusal is loud (an error, a counted skip);
/// nothing is silently rewritten, because a rewritten id would no longer
/// address the finding it names.
pub fn is_safe_dispatch_segment(dispatch: &str) -> bool {
    !dispatch.is_empty()
        && !dispatch.starts_with('.')
        && !dispatch.contains('/')
        && !dispatch.contains('\\')
        && !dispatch.contains('\0')
}

/// Whether a tool name produces a finding record.
pub fn is_finding_tool(tool_name: &str) -> bool {
    FINDING_TOOL_NAMES.contains(&tool_name)
}

/// Who proposed a finding. Named at write time from the dispatch's own
/// identity — the role handle, the model that ran it, and the machine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Proposer {
    /// The role handle (the flow record's `handle`).
    pub handle: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
}

/// The mission scope a dispatch ran under. Every field is `None` for a plain
/// `darkmux dispatch`, which belongs to no mission — grouped into one struct so
/// the two producers cannot pass the three ids in different orders.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    pub mission_id: Option<String>,
    pub phase_id: Option<String>,
    pub step_id: Option<String>,
}

/// One finding, as stored at `<findings dir>/<dispatch>/<seq>/finding.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindingRecord {
    /// `<dispatch>/<seq>` — the address every other surface uses.
    pub key: String,
    /// The dispatch's session id.
    pub dispatch: String,
    /// The accepted call's `emit_seq` within that dispatch.
    pub seq: u64,
    /// When the RECORD was written (not when the dispatch started).
    pub ts: String,
    pub tool_name: String,
    pub proposer: Proposer,
    /// The mission / phase / step this dispatch ran under, when it ran under
    /// one — `null` for a plain `darkmux dispatch`, which belongs to no
    /// mission. These are the dispatch's OWN scope and are top-level fields on
    /// the flow record, NOT part of `context`; a crawl's context blob carries
    /// workspace / source / sha / rule / unit and no mission id at all. They
    /// live here for the same reason: `context` is the launcher's, verbatim,
    /// and darkmux does not write into it.
    ///
    /// Additive (a record written before them simply lacks the keys, and
    /// `Option` reads that as `None`), so the schema version does not move.
    #[serde(default)]
    pub mission_id: Option<String>,
    #[serde(default)]
    pub phase_id: Option<String>,
    #[serde(default)]
    pub step_id: Option<String>,
    /// The dispatch's `record_context` verbatim when it had one (a crawl's
    /// workspace / source / sha / rule / unit), else `null`. darkmux does not
    /// read inside it, and never adds to it.
    pub context: serde_json::Value,
    /// The model's arguments, verbatim. Opaque: never parsed, never validated,
    /// never reshaped.
    pub emitted: serde_json::Value,
    pub schema_version: String,
    /// Lenient-on-read overflow, so a newer writer's fields survive a round
    /// trip through an older reader.
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

/// What [`materialize`] did. Write-once means "already there" is a normal
/// outcome, not an error — both producers race by design.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Materialized {
    Created,
    AlreadyPresent,
}

/// The store root: `env(DARKMUX_FINDINGS_DIR) > config.dirs.findings >
/// <darkmux root>/findings`.
pub fn findings_dir() -> PathBuf {
    darkmux_types::config_access::findings_dir()
}

/// The directory one finding lives in.
pub fn record_dir_at(root: &Path, dispatch: &str, seq: u64) -> PathBuf {
    root.join(dispatch).join(seq.to_string())
}

/// The file one finding lives at.
pub fn record_path_at(root: &Path, dispatch: &str, seq: u64) -> PathBuf {
    record_dir_at(root, dispatch, seq).join("finding.json")
}

/// Build a record from the pieces both producers have.
#[allow(clippy::too_many_arguments)]
pub fn build_record(
    dispatch: &str,
    seq: u64,
    ts: String,
    tool_name: &str,
    proposer: Proposer,
    scope: Scope,
    context: Option<serde_json::Value>,
    emitted: serde_json::Value,
) -> FindingRecord {
    FindingRecord {
        key: format!("{dispatch}/{seq}"),
        dispatch: dispatch.to_string(),
        seq,
        ts,
        tool_name: tool_name.to_string(),
        proposer,
        mission_id: scope.mission_id,
        phase_id: scope.phase_id,
        step_id: scope.step_id,
        context: context.unwrap_or(serde_json::Value::Null),
        emitted,
        schema_version: FINDING_SCHEMA_VERSION.to_string(),
        extras: serde_json::Map::new(),
    }
}

/// Write a finding **once**. An existing file is never overwritten — a finding
/// is an event, and the first writer's version is the one that happened. The
/// second producer to arrive reports [`Materialized::AlreadyPresent`] and
/// leaves the bytes on disk untouched.
pub fn materialize(root: &Path, record: &FindingRecord) -> Result<Materialized> {
    // The backstop for `parse_key`'s check: a record's `dispatch` comes from
    // the stream, so a producer that never parsed a key still cannot write
    // outside the store.
    anyhow::ensure!(
        is_safe_dispatch_segment(&record.dispatch),
        "refusing to write a finding under an unsafe dispatch id {:?}",
        record.dispatch
    );
    let path = record_path_at(root, &record.dispatch, record.seq);
    if path.exists() {
        return Ok(Materialized::AlreadyPresent);
    }
    let dir = path.parent().expect("record path always has a parent");
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating finding dir {}", dir.display()))?;
    let body = serde_json::to_string_pretty(record)? + "\n";
    // `create_new` closes the last of the race: two producers that both saw an
    // absent file still cannot double-write, and the loser reports the same
    // already-present outcome the `exists()` fast path does.
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut f) => {
            use std::io::Write;
            f.write_all(body.as_bytes())
                .with_context(|| format!("writing finding {}", path.display()))?;
            Ok(Materialized::Created)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(Materialized::AlreadyPresent),
        Err(e) => Err(e).with_context(|| format!("creating finding {}", path.display())),
    }
}

/// Read one finding by `<dispatch>/<seq>`.
pub fn load_at(root: &Path, dispatch: &str, seq: u64) -> Result<Option<FindingRecord>> {
    let path = record_path_at(root, dispatch, seq);
    if !path.exists() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(&path)
        .with_context(|| format!("reading finding {}", path.display()))?;
    let rec = serde_json::from_str(&body)
        .with_context(|| format!("parsing finding {}", path.display()))?;
    Ok(Some(rec))
}

/// Split a `<dispatch>/<seq>` key. The dispatch half may itself contain no
/// slash (session ids never do), so the split is on the LAST separator.
pub fn parse_key(key: &str) -> Option<(String, u64)> {
    let (dispatch, seq) = key.rsplit_once('/')?;
    // The dispatch half becomes a path segment, so it is validated HERE rather
    // than at the join: `finding show '../x/1'` must not read outside the store.
    if !is_safe_dispatch_segment(dispatch) {
        return None;
    }
    Some((dispatch.to_string(), seq.parse().ok()?))
}

/// Every finding in the store, ts-ascending. Unreadable or unparseable files
/// are skipped rather than failing the read — the same casual-reader contract
/// the flow day files get.
pub fn load_all_at(root: &Path) -> Result<Vec<FindingRecord>> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    let dispatch_dirs = match std::fs::read_dir(root) {
        Ok(d) => d,
        Err(_) => return Ok(out),
    };
    for dispatch_entry in dispatch_dirs.flatten() {
        let seq_dirs = match std::fs::read_dir(dispatch_entry.path()) {
            Ok(d) => d,
            Err(_) => continue,
        };
        for seq_entry in seq_dirs.flatten() {
            let path = seq_entry.path().join("finding.json");
            let Ok(body) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Ok(rec) = serde_json::from_str::<FindingRecord>(&body) {
                out.push(rec);
            }
        }
    }
    out.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.key.cmp(&b.key)));
    Ok(out)
}

/// A top-level string field off a flow record, when it is present and a string.
fn str_field(v: &serde_json::Value, field: &str) -> Option<String> {
    v.get(field).and_then(|x| x.as_str()).map(String::from)
}

/// What one `finding sync` pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SyncReport {
    /// Flow records inspected that named a finding tool and succeeded.
    pub scanned: usize,
    pub created: usize,
    pub present: usize,
    /// Accepted finding calls whose record predates FLOW 1.33.0 and therefore
    /// carries no `emitted`. They exist in the stream and cannot become
    /// records — there is nothing to store.
    pub skipped_no_emission: usize,
}

/// Replay the flow stream into the store: every `dispatch.tool` record naming
/// a finding tool with `ok == true` and a non-null `emitted` becomes a record
/// if one is not already there. Idempotent by construction (materialize is
/// write-once), so running it twice reports the second pass as all-present.
///
/// `since` filters on the day file's own name (`YYYY-MM-DD.jsonl`), which is
/// how the LocalFileSink partitions — no line-level date parsing needed.
pub fn sync_at(flows_dir: &Path, store_root: &Path, since: Option<&str>) -> Result<SyncReport> {
    // A malformed `--since` would match no day file, scan nothing and exit
    // clean — indistinguishable from "there are no findings". Refuse it.
    if let Some(since) = since {
        let shaped = since.len() == 10
            && since.as_bytes().iter().enumerate().all(|(i, b)| match i {
                4 | 7 => *b == b'-',
                _ => b.is_ascii_digit(),
            });
        anyhow::ensure!(shaped, "--since must be a date of the form YYYY-MM-DD, got {since:?}");
    }
    let mut report = SyncReport::default();
    if !flows_dir.exists() {
        return Ok(report);
    }
    let mut day_files: Vec<PathBuf> = std::fs::read_dir(flows_dir)
        .with_context(|| format!("reading flows dir {}", flows_dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .filter(|p| match since {
            None => true,
            Some(since) => p
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|stem| stem >= since)
                .unwrap_or(false),
        })
        .collect();
    day_files.sort();

    for path in day_files {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines() {
            // Lines that don't parse as a record (the schema header, a partial
            // tail write) are skipped — the casual LocalFileSink read contract.
            let Ok(rec) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if rec.get("action").and_then(|v| v.as_str()) != Some("dispatch.tool") {
                continue;
            }
            let payload = rec.get("payload").unwrap_or(&serde_json::Value::Null);
            let tool_name = payload.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
            if !is_finding_tool(tool_name) {
                continue;
            }
            if payload.get("ok").and_then(|v| v.as_bool()) != Some(true) {
                continue;
            }
            report.scanned += 1;
            let emitted = payload.get("emitted");
            if emitted.is_none() || emitted == Some(&serde_json::Value::Null) {
                // Pre-FLOW-1.33.0: the emission was never carried, so there is
                // no record to make. Counted, and named in the human output.
                report.skipped_no_emission += 1;
                continue;
            }
            let (Some(dispatch), Some(seq)) = (
                rec.get("session_id")
                    .and_then(|v| v.as_str())
                    // An id that could escape the store is never joined onto a
                    // path — it is dropped here, before `materialize` has to
                    // refuse it, and counted like any other unaddressable call.
                    .filter(|d| is_safe_dispatch_segment(d)),
                payload.get("emit_seq").and_then(|v| v.as_u64()),
            ) else {
                // No `session_id` (or an unsafe one), or an `emitted` with no
                // `emit_seq` beside it (the tailer writes the pair together, so
                // this is malformed). Either way there is no usable
                // `<dispatch>/<seq>` to address the finding by, so it cannot
                // become a record — the same bucket, for the same reason, as a
                // pre-1.33.0 call: counted and named, never silently dropped.
                report.skipped_no_emission += 1;
                continue;
            };
            let record = build_record(
                dispatch,
                seq,
                rec.get("ts").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                tool_name,
                Proposer {
                    handle: rec.get("handle").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                    model: rec.get("model").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                    machine_id: rec
                        .get("machine_id")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                },
                Scope {
                    // TOP-LEVEL on the flow record, never inside `payload.context`
                    // — the gap #2288's live proof found. `step_id` is the one
                    // exception: it is stamped into the PAYLOAD (#1483), so it
                    // is read from there, with the top level as a fallback.
                    mission_id: str_field(&rec, "mission_id"),
                    phase_id: str_field(&rec, "phase_id"),
                    step_id: str_field(payload, "step_id").or_else(|| str_field(&rec, "step_id")),
                },
                payload.get("context").cloned(),
                emitted.cloned().unwrap_or(serde_json::Value::Null),
            );
            match materialize(store_root, &record)? {
                Materialized::Created => report.created += 1,
                Materialized::AlreadyPresent => report.present += 1,
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn proposer() -> Proposer {
        Proposer {
            handle: "crawler".into(),
            model: "darkmux:qwen3.6".into(),
            machine_id: Some("studio".into()),
        }
    }

    fn rec_at(dispatch: &str, seq: u64, ts: &str) -> FindingRecord {
        build_record(
            dispatch,
            seq,
            ts.to_string(),
            "create_finding",
            proposer(),
            Scope::default(),
            None,
            serde_json::json!({"file": "x.ts"}),
        )
    }

    #[test]
    fn both_tool_names_are_finding_tools_and_nothing_else_is() {
        assert!(is_finding_tool("create_finding"));
        // The pre-2026-09-03 name: the stream is append-only and historical
        // records carry it, so it stays readable forever.
        assert!(is_finding_tool("report_finding"));
        assert!(!is_finding_tool("read"));
        assert!(!is_finding_tool("create_mod"));
        assert!(!is_finding_tool(""));
    }

    #[test]
    fn parse_key_takes_the_last_separator_and_refuses_traversal() {
        assert_eq!(parse_key("sess-a/1"), Some(("sess-a".into(), 1)));
        assert_eq!(parse_key("sess-a/0"), Some(("sess-a".into(), 0)));
        assert_eq!(parse_key("no-slash"), None);
        assert_eq!(parse_key("sess-a/notanumber"), None);
        assert_eq!(parse_key("/1"), None, "an empty dispatch is not a key");
        // The dispatch segment comes off the STREAM, so it is untrusted input
        // that ends up in a path. Anything that could escape the store is
        // refused rather than joined.
        assert_eq!(parse_key("../x/1"), None, "traversal must not resolve");
        assert_eq!(parse_key("a/b/1"), None, "a nested dispatch is not a key");
        assert_eq!(parse_key("../1"), None);
        assert_eq!(parse_key("./1"), None);
        assert_eq!(parse_key(".hidden/1"), None, "a leading dot is refused");
    }

    #[test]
    fn materialize_writes_once_and_leaves_an_existing_record_alone() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let rec = rec_at("sess-a", 1, "2026-09-03T01:00:00Z");

        assert_eq!(materialize(root, &rec).unwrap(), Materialized::Created);
        let path = record_path_at(root, "sess-a", 1);
        assert!(path.exists());

        // The SECOND writer never overwrites — a finding is an event, and the
        // first writer's version is the one that happened.
        std::fs::write(&path, "{\"key\":\"sentinel\"}").unwrap();
        assert_eq!(materialize(root, &rec).unwrap(), Materialized::AlreadyPresent);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"key\":\"sentinel\"}");
    }

    #[test]
    fn materialize_refuses_a_dispatch_id_that_could_escape_the_store() {
        let tmp = TempDir::new().unwrap();
        // The backstop for the path above: `session_id` comes off the stream,
        // so materialize validates it even when no one called `parse_key`.
        for bad in ["../escape", "a/b", ".hidden", "", "."] {
            let rec = rec_at(bad, 1, "2026-09-03T01:00:00Z");
            assert!(
                materialize(tmp.path(), &rec).is_err(),
                "an unsafe dispatch id must be refused, not joined: {bad:?}"
            );
        }
        // Nothing was created anywhere: a refusal happens BEFORE any path is
        // joined, so the store root is still empty.
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn materialize_surfaces_a_real_io_error_rather_than_swallowing_it() {
        let tmp = TempDir::new().unwrap();
        // A FILE where the dispatch DIRECTORY needs to be: `create_dir_all`
        // fails. Only `AlreadyExists` on the record itself is a normal
        // outcome; every other error is a real failure and must surface.
        std::fs::write(tmp.path().join("sess-a"), b"not a directory").unwrap();
        let err = materialize(tmp.path(), &rec_at("sess-a", 1, "2026-09-03T01:00:00Z"));
        assert!(err.is_err(), "a genuine IO failure must not read as AlreadyPresent");
    }

    #[test]
    fn load_at_round_trips_and_reports_absence_as_none() {
        let tmp = TempDir::new().unwrap();
        let rec = rec_at("sess-a", 7, "2026-09-03T01:00:00Z");
        materialize(tmp.path(), &rec).unwrap();

        let back = load_at(tmp.path(), "sess-a", 7).unwrap().expect("round trips");
        assert_eq!(back.key, "sess-a/7");
        assert_eq!(back.emitted, serde_json::json!({"file": "x.ts"}));
        assert!(back.context.is_null());
        assert!(back.mission_id.is_none());
        assert_eq!(back.schema_version, FINDING_SCHEMA_VERSION);

        assert!(load_at(tmp.path(), "sess-a", 8).unwrap().is_none());
        assert!(load_at(tmp.path(), "nope", 7).unwrap().is_none());
    }

    #[test]
    fn load_all_at_sorts_by_ts_and_skips_what_it_cannot_read() {
        let tmp = TempDir::new().unwrap();
        assert!(load_all_at(tmp.path()).unwrap().is_empty(), "an absent store is empty, not an error");

        materialize(tmp.path(), &rec_at("sess-c", 1, "2026-09-03T03:00:00Z")).unwrap();
        materialize(tmp.path(), &rec_at("sess-a", 1, "2026-09-03T01:00:00Z")).unwrap();
        materialize(tmp.path(), &rec_at("sess-b", 1, "2026-09-03T02:00:00Z")).unwrap();
        // A corrupt record is skipped, not fatal — the same casual-reader
        // contract the flow day files get.
        std::fs::create_dir_all(tmp.path().join("sess-x").join("1")).unwrap();
        std::fs::write(tmp.path().join("sess-x").join("1").join("finding.json"), "{ not json").unwrap();

        let keys: Vec<String> = load_all_at(tmp.path()).unwrap().into_iter().map(|r| r.key).collect();
        assert_eq!(keys, vec!["sess-a/1", "sess-b/1", "sess-c/1"], "ts-ascending");
    }

    /// One day file holding every shape `sync_at` has to tell apart.
    fn write_day(dir: &std::path::Path, name: &str, lines: &[String]) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), lines.join("\n") + "\n").unwrap();
    }

    fn flow_line(sess: &str, tool: &str, ok: bool, emitted: Option<serde_json::Value>, seq: Option<u64>) -> String {
        let mut payload = serde_json::json!({"tool_name": tool, "ok": ok});
        if let Some(e) = emitted {
            payload["emitted"] = e;
        }
        if let Some(s) = seq {
            payload["emit_seq"] = serde_json::json!(s);
        }
        serde_json::json!({
            "ts": "2026-09-03T01:00:00Z", "action": "dispatch.tool",
            "handle": "crawler", "session_id": sess, "model": "m",
            "mission_id": "crawl-1", "payload": payload,
        })
        .to_string()
    }

    #[test]
    fn sync_at_buckets_every_shape_and_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let flows = tmp.path().join("flows");
        let store = tmp.path().join("findings");
        let emitted = || Some(serde_json::json!({"file": "a.ts"}));
        write_day(
            &flows,
            "2026-09-03.jsonl",
            &[
                "# a schema header line, not a record".to_string(),
                "{ not json at all".to_string(),
                flow_line("sess-a", "create_finding", true, emitted(), Some(1)),
                flow_line("sess-b", "report_finding", true, emitted(), Some(2)),
                // Accepted, but pre-FLOW-1.33.0: no emission to store.
                flow_line("sess-c", "create_finding", true, None, None),
                // Rejected citation: a FAILED call is not a finding.
                flow_line("sess-d", "create_finding", false, emitted(), Some(4)),
                // An emission with no ordinal: unaddressable, so unrecordable.
                flow_line("sess-e", "create_finding", true, emitted(), None),
                // A different tool entirely.
                flow_line("sess-f", "read", true, emitted(), Some(6)),
                // An explicit null emission from a current runtime.
                flow_line("sess-g", "create_finding", true, Some(serde_json::Value::Null), Some(7)),
                // A dispatch id that would escape the store: never joined.
                flow_line("../escape", "create_finding", true, emitted(), Some(8)),
            ],
        );

        let r = sync_at(&flows, &store, None).unwrap();
        assert_eq!(r.created, 2, "only the two accepted, emitting, addressable calls: {r:?}");
        assert_eq!(r.present, 0, "{r:?}");
        assert_eq!(r.scanned, 6, "accepted finding calls only — not `read`, not the rejected one: {r:?}");
        assert_eq!(r.skipped_no_emission, 4, "no-emission, null, no-seq, unsafe-id: {r:?}");
        assert!(store.join("sess-a").join("1").exists());
        assert!(store.join("sess-b").join("2").exists());
        assert!(!store.join("sess-d").exists(), "a rejected call makes no record");
        assert!(!tmp.path().join("escape").exists(), "traversal never resolves");

        // The mission the dispatch ran under is a TOP-LEVEL flow field.
        let back = load_at(&store, "sess-a", 1).unwrap().unwrap();
        assert_eq!(back.mission_id.as_deref(), Some("crawl-1"));

        // Idempotent: the store is write-once, so a replay creates nothing.
        let again = sync_at(&flows, &store, None).unwrap();
        assert_eq!((again.created, again.present), (0, 2), "{again:?}");
    }

    #[test]
    fn sync_at_filters_day_files_by_since_and_refuses_a_non_date() {
        let tmp = TempDir::new().unwrap();
        let flows = tmp.path().join("flows");
        let store = tmp.path().join("findings");
        let emitted = || Some(serde_json::json!({"file": "a.ts"}));
        write_day(&flows, "2026-09-01.jsonl", &[flow_line("sess-old", "create_finding", true, emitted(), Some(1))]);
        write_day(&flows, "2026-09-03.jsonl", &[flow_line("sess-new", "create_finding", true, emitted(), Some(1))]);

        let r = sync_at(&flows, &store, Some("2026-09-02")).unwrap();
        assert_eq!(r.created, 1, "only the day files at or after --since: {r:?}");
        assert!(store.join("sess-new").exists() && !store.join("sess-old").exists());

        // A malformed --since would silently scan NOTHING and exit clean,
        // which reads exactly like "no findings". Refuse it loudly instead.
        let err = sync_at(&flows, &store, Some("last tuesday")).unwrap_err();
        assert!(format!("{err:#}").contains("YYYY-MM-DD"), "the error names the shape: {err:#}");

        // An absent flows dir is empty, not an error.
        assert_eq!(sync_at(&tmp.path().join("nope"), &store, None).unwrap(), SyncReport::default());
    }
}
