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
    /// The dispatch's `record_context` verbatim when it had one (a crawl's
    /// mission / unit / rule / source / sha), else `null`. darkmux does not
    /// read inside it.
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
    if dispatch.is_empty() {
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
                rec.get("session_id").and_then(|v| v.as_str()),
                payload.get("emit_seq").and_then(|v| v.as_u64()),
            ) else {
                // No `session_id`, or an `emitted` with no `emit_seq` beside it
                // (the tailer writes the pair together, so this is malformed).
                // Either way there is no `<dispatch>/<seq>` to address the
                // finding by, so it cannot become a record — the same bucket,
                // for the same reason, as a pre-1.33.0 call: counted and
                // named, never silently dropped.
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
