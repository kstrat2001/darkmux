//! The one lenient flow-record reader.
//!
//! Every consumer that reads flow records back (day files, the Redis stream,
//! a peer's records, the audit substrate's JSON bodies) goes through this
//! module, so a record reads with exactly one spelling per event whichever
//! archive it came from.
//!
//! * [`parse_record`] for a typed [`FlowRecord`].
//! * [`parse_value`] for a consumer that keeps the record as JSON (the
//!   daemon, which serves records on to the viewer with every field intact).
//!   [`action_of`], [`source_of`] and [`payload_of`] are the typed reads of
//!   its fields.
//!
//! Lenient on read (contract 5), and nothing is rewritten or synthesized: an
//! action this binary does not know is kept verbatim, as
//! [`FlowAction::Other`], and [`UnknownActions`] counts it so `darkmux doctor`
//! can name it. That covers a newer writer's action and one a release retired:
//! a pre-5.0 archive reads, and its retired spellings read as unknown. A file
//! is never rewritten.

use crate::{FlowAction, FlowRecord};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Parse one JSONL line to JSON. `None` for a line that is not a JSON object.
pub fn parse_value(line: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(line).ok()?;
    v.is_object().then_some(v)
}

/// Parse one JSONL line to a typed [`FlowRecord`]. `None` for a line that is not a flow record
/// (a schema header, a torn write).
pub fn parse_record(line: &str) -> Option<FlowRecord> {
    let mut v = parse_value(line)?;
    // `FlowAction`'s public deserializer refuses an unknown action; this is
    // the one lenient path, so the action is read here and
    // put back after the rest of the record deserializes.
    let action = action_of(&v)?;
    v["action"] = Value::String(FlowAction::OperatorNote.as_str().to_string());
    let mut record: FlowRecord = serde_json::from_value(v).ok()?;
    record.action = action;
    Some(record.settled())
}

/// The typed `source` of a JSON record; a spelling this build does not know
/// is [`crate::FlowSource::Unknown`], and `None` when the record names none.
pub fn source_of(record: &Value) -> Option<crate::FlowSource> {
    let wire = record.get("source")?.as_str()?;
    Some(serde_json::from_value(Value::String(wire.to_string())).unwrap_or(crate::FlowSource::Unknown))
}

/// The typed action of a JSON record: a current action, else
/// [`FlowAction::Other`]; `None` when the record has no string `action`.
pub fn action_of(record: &Value) -> Option<FlowAction> {
    Some(FlowAction::from_wire(record.get("action")?.as_str()?))
}

/// The execution a JSON record names: its own `execution_id`. `None` for a
/// record that names none (a record written before 4.0): none is invented.
pub fn execution_id_of(record: &Value) -> Option<darkmux_types::execution_id::ExecutionId> {
    darkmux_types::execution_id::ExecutionId::parse(record.get("execution_id")?.as_str()?).ok()
}

/// The typed payload of a JSON record: read as its action's type,
/// [`Payload::Unread`] when it is not that type, `None` when the record has no
/// payload or no action. For a reader that holds records as JSON; one that
/// parses lines into [`FlowRecord`]s gets the same through
/// [`FlowRecord::payload`].
pub fn payload_of(record: &Value) -> Option<crate::Payload> {
    let action = action_of(record)?;
    let raw = record.get("payload").filter(|p| !p.is_null())?;
    Some(crate::Payload::settle(&action, raw.clone()))
}

/// A tally of the unknown actions a read met, by name. Filled by
/// [`UnknownActions::observe`]; `darkmux doctor` reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnknownActions(BTreeMap<String, u64>);

impl UnknownActions {
    /// Count `record`'s action if this binary does not know it.
    pub fn observe(&mut self, record: &Value) {
        if let Some(wire) = record.get("action").and_then(Value::as_str) {
            self.observe_wire(wire);
        }
    }

    /// Count one action string if this binary does not know it.
    pub fn observe_wire(&mut self, wire: &str) {
        if let FlowAction::Other(unknown) = FlowAction::from_wire(wire) {
            *self.0.entry(unknown.as_str().to_string()).or_insert(0) += 1;
        }
    }

    /// Unknown action names and how many records carried each.
    pub fn by_name(&self) -> &BTreeMap<String, u64> {
        &self.0
    }

    /// Records with an unknown action, in total.
    pub fn total(&self) -> u64 {
        self.0.values().sum()
    }
}

/// The day files (`YYYY-MM-DD.jsonl`) in `dir`, newest first, at most
/// `limit` of them. Empty when `dir` does not exist.
pub fn recent_day_files(dir: &Path, limit: usize) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut days: Vec<(String, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            let stem = name.strip_suffix(".jsonl")?;
            is_day_stem(stem).then(|| (stem.to_string(), e.path()))
        })
        .collect();
    days.sort_by(|a, b| b.0.cmp(&a.0));
    days.into_iter().take(limit).map(|(_, p)| p).collect()
}

fn is_day_stem(stem: &str) -> bool {
    let b = stem.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// Every record in one day file, read through [`parse_value`]. A missing or
/// unreadable file reads as empty.
pub fn day_file_records(path: &Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    text.lines().filter_map(parse_value).collect()
}

/// The session a role execution ran under: the `session_id` its
/// `dispatch.start` record carries.
///
/// The id encodes the moment it was minted, and its `dispatch.start` follows
/// within moments, so the record is in the day file of that UTC day, or in
/// the neighboring days when the mint sat near midnight or clocks differ.
/// Only those three files are read, however old the id is; a line that does
/// not contain the id is skipped without being parsed. `None` when the id
/// carries no mint time or no such record is there.
pub fn session_of_execution(dir: &Path, execution: &darkmux_types::execution_id::ExecutionId) -> Option<String> {
    let minted = execution.minted_at_secs()?;
    [0, 1, -1].into_iter().find_map(|day_offset| {
        let path = dir.join(format!("{}.jsonl", crate::day_utc_at(minted + day_offset * 86_400)));
        let text = std::fs::read_to_string(path).ok()?;
        text.lines().filter(|line| line.contains(execution.as_str())).filter_map(parse_value).find_map(|r| {
            let of_it = r.get("execution_id").and_then(Value::as_str) == Some(execution.as_str());
            (of_it && action_of(&r) == Some(FlowAction::DispatchStart))
                .then(|| r.get("session_id").and_then(Value::as_str).map(str::to_string))
                .flatten()
        })
    })
}

/// Tally the unknown actions in the `days` newest day files under `dir`.
/// Reads only each line's `action` field ([`action_field`]), not the whole
/// record, so a week of busy day files costs a scan, not a parse.
pub fn unknown_actions_in(dir: &Path, days: usize) -> UnknownActions {
    let mut tally = UnknownActions::default();
    for path in recent_day_files(dir, days) {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        for wire in text.lines().filter_map(action_field) {
            tally.observe_wire(&wire);
        }
    }
    tally
}

/// A line's top-level `action` string without parsing the record. darkmux
/// writes `action` before `payload`, so the first `"action"` key is the
/// record's own. Whenever that quick read does not land cleanly (an earlier
/// VALUE spelled `"action"`, an escape in the action, a non-string action),
/// the line is parsed in full instead.
pub fn action_field(line: &str) -> Option<std::borrow::Cow<'_, str>> {
    match quick_action_field(line) {
        Some(wire) => Some(std::borrow::Cow::Borrowed(wire)),
        None => {
            let v: Value = serde_json::from_str(line).ok()?;
            v.get("action")?.as_str().map(|s| std::borrow::Cow::Owned(s.to_string()))
        }
    }
}

fn quick_action_field(line: &str) -> Option<&str> {
    const KEY: &str = "\"action\"";
    let rest = &line[line.find(KEY)? + KEY.len()..];
    let body = rest.trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    let wire = &body[..body.find('"')?];
    (!wire.contains('\\')).then_some(wire)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An id minted at `secs`, in the grammar [`ExecutionId::mint`] writes.
    fn exec_at(secs: i64, tag: u32) -> darkmux_types::execution_id::ExecutionId {
        darkmux_types::execution_id::ExecutionId::parse_minted(&format!("exec-{:x}-1-{tag:x}", secs * 1_000_000)).unwrap()
    }

    fn start_line(exec: &darkmux_types::execution_id::ExecutionId, action: &str, session: &str) -> String {
        json!({"action": action, "execution_id": exec.as_str(), "session_id": session}).to_string()
    }

    #[test]
    fn an_execution_resolves_to_the_session_its_dispatch_start_carries() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_790_564_400; // 2026-09-28T03:00:00Z
        let (mine, other) = (exec_at(now, 1), exec_at(now, 2));
        // The note names the execution too, under a session of its own: only
        // the `dispatch.start` record says which session the execution ran in.
        let body = [
            start_line(&other, "dispatch.start", "run-a.adhoc.coder.other"),
            start_line(&mine, "operator.note", "not-the-session"),
            start_line(&mine, "dispatch.start", "run-b.adhoc.coder.mine"),
        ]
        .join("\n");
        std::fs::write(dir.path().join(format!("{}.jsonl", crate::day_utc_at(now))), body).unwrap();

        assert_eq!(session_of_execution(dir.path(), &mine).as_deref(), Some("run-b.adhoc.coder.mine"));
        assert_eq!(session_of_execution(dir.path(), &exec_at(now, 3)), None, "an unknown execution");
    }

    #[test]
    fn an_old_execution_resolves_however_old_it_is() {
        // No look-back window: the id names its own day.
        let dir = tempfile::tempdir().unwrap();
        let then = 1_790_564_400 - 400 * 86_400;
        let mine = exec_at(then, 1);
        std::fs::write(dir.path().join(format!("{}.jsonl", crate::day_utc_at(then))), start_line(&mine, "dispatch.start", "run-o.adhoc.coder.old")).unwrap();
        for newer in 1..=40 {
            std::fs::write(dir.path().join(format!("{}.jsonl", crate::day_utc_at(then + newer * 86_400 + 43_200))), "{}").unwrap();
        }
        assert_eq!(session_of_execution(dir.path(), &mine).as_deref(), Some("run-o.adhoc.coder.old"));
    }

    #[test]
    fn only_the_days_around_the_mint_are_read() {
        // A record in a far-away day file is not reached: the mint time bounds
        // the search, which is what keeps an unknown id off the whole archive.
        let dir = tempfile::tempdir().unwrap();
        let now = 1_790_564_400;
        let mine = exec_at(now, 1);
        std::fs::write(dir.path().join(format!("{}.jsonl", crate::day_utc_at(now - 10 * 86_400))), start_line(&mine, "dispatch.start", "run-x.adhoc.coder.far")).unwrap();
        assert_eq!(session_of_execution(dir.path(), &mine), None);
    }

    #[test]
    fn an_execution_started_across_midnight_of_its_mint_is_found_in_the_next_day() {
        let dir = tempfile::tempdir().unwrap();
        let just_before_midnight = 1_790_553_599; // 2026-09-27T23:59:59Z
        let mine = exec_at(just_before_midnight, 1);
        let next = crate::day_utc_at(just_before_midnight + 1);
        std::fs::write(dir.path().join(format!("{next}.jsonl")), start_line(&mine, "dispatch.start", "run-n.adhoc.coder.next")).unwrap();
        assert_eq!(session_of_execution(dir.path(), &mine).as_deref(), Some("run-n.adhoc.coder.next"));
    }

    /// (#3036) A record written before 5.0 with a retired spelling still
    /// reads, without a panic: its action is an unknown one, kept verbatim and
    /// counted as unknown, its retired `source` reads as `Unknown`, and the
    /// reader rewrites and synthesizes nothing.
    #[test]
    fn a_pre_5_0_record_reads_as_an_unknown_action() {
        // flow-action-guard:allow-start — retired spellings are this test's input
        let retired = [
            "dispatch start", "step result", "sprint start", "verdict: clean", "telemetry.process",
            "mission.run.start", "mission pause", "phase.added", "crawl.finding", // drift-guard:allow mission pause — the archived spelling is this test's input
        ];
        // flow-action-guard:allow-end
        let mut tally = UnknownActions::default();
        for old in retired {
            let line = format!(
                r#"{{"ts":"t","level":"info","category":"work","tier":"local","stage":"dispatch","action":"{old}","handle":"h","session_id":"s","source":"host-sampler"}}"#
            );
            let record = parse_record(&line).unwrap_or_else(|| panic!("{old}: an old record still parses"));
            assert!(matches!(record.action, FlowAction::Other(_)), "{old}");
            assert_eq!(record.action.as_str(), old);
            assert_eq!(record.source, Some(crate::FlowSource::Unknown), "{old}: a retired source reads as unknown");
            assert!(record.execution_id.is_none(), "{old}: no identity is synthesized");
            let v = parse_value(&line).unwrap();
            assert_eq!((v["action"].as_str(), v["source"].as_str(), v.get("execution_id")), (Some(old), Some("host-sampler"), None), "{old}: nothing is rewritten");
            tally.observe(&v);
        }
        assert_eq!(tally.total(), retired.len() as u64, "every one counts as unknown");
    }

    /// A record written before the queue fields were dropped still reads: the
    /// typed record has no `work_id` / `attempt`, and parsing ignores them.
    #[test]
    fn an_archived_record_carrying_dropped_fields_still_parses() {
        let line = r#"{"ts":"2026-01-01T00:00:00Z","level":"info","category":"work","tier":"local","stage":"estimate","action":"operator.note","handle":"h","work_id":"1-0","attempt":2}"#;
        let record = parse_record(line).expect("archive line parses");
        assert!(matches!(record.stage, crate::Stage::Unknown), "{:?}", record.stage);
        let back = serde_json::to_value(&record).unwrap();
        assert!(back.get("work_id").is_none() && back.get("attempt").is_none());
    }

    #[test]
    fn action_field_reads_the_top_level_action_without_a_full_parse() {
        let cases = [
            (r#"{"ts":"t","action":"dispatch.start","payload":{"action":"x"}}"#, Some("dispatch.start")),
            (r#"{"ts":"t", "action" : "future.thing"}"#, Some("future.thing")),
            (r#"{"payload":{"delivered_action":"x"},"action":"hook.fired"}"#, Some("hook.fired")),
            (r#"{"action":"we\"ird"}"#, Some("we\"ird")),
            (r#"{"_type":"schema"}"#, None),
            (r#"{"action":7}"#, None),
            (r#"{"ts":"t","handle":"action","action":"future.thing"}"#, Some("future.thing")),
        ];
        for (line, want) in cases {
            assert_eq!(action_field(line).as_deref(), want, "{line}");
        }
    }

    #[test]
    fn unknown_actions_are_tallied_across_the_newest_day_files_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let write = |day: &str, actions: &[&str]| {
            let body: String = actions.iter().map(|a| format!("{}\n", json!({ "action": a }))).collect();
            std::fs::write(tmp.path().join(format!("{day}.jsonl")), body).unwrap();
        };
        write("2026-01-01", &["ancient.thing"]);
        write("2026-01-02", &["future.thing", "dispatch.start"]);
        write("2026-01-03", &["future.thing", "dispatch.turn"]);
        std::fs::write(tmp.path().join("notes.jsonl"), "{\"action\":\"not.a.day\"}\n").unwrap();
        let tally = unknown_actions_in(tmp.path(), 2);
        assert_eq!(tally.by_name().get("future.thing"), Some(&2));
        assert!(!tally.by_name().contains_key("ancient.thing"), "only the newest two days are read");
        assert!(!tally.by_name().contains_key("not.a.day"), "a file that is not a day file is not read");
        assert_eq!(tally.total(), 2);
    }

    #[test]
    fn unknown_actions_are_counted_by_name_and_known_ones_are_not() {
        let mut tally = UnknownActions::default();
        for a in ["future.thing", "future.thing", "dispatch.turn", "other.x"] {
            tally.observe(&json!({ "action": a }));
        }
        assert_eq!(tally.total(), 3);
        assert_eq!(tally.by_name().get("future.thing"), Some(&2));
        assert_eq!(tally.by_name().get("other.x"), Some(&1));
        assert!(!tally.by_name().contains_key("dispatch.turn"));
        assert_eq!(action_of(&json!({"_type": "schema", "version": "1.0.0"})), None, "no action: absent, not unknown");
        tally.observe(&json!({"_type": "schema"}));
        assert_eq!(tally.total(), 3);
    }
}
