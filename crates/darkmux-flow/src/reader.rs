//! The one lenient flow-record reader.
//!
//! Every consumer that reads flow records back (day files, the Redis stream,
//! a peer's records, the audit substrate's JSON bodies) goes through this
//! module, so a record reads with exactly one spelling per event whichever
//! archive it came from. Two shapes, one upgrade:
//!
//! * [`parse_record`] for a typed [`FlowRecord`]. [`FlowAction`]'s own
//!   deserializer applies [`crate::legacy::read_action`], so a retired
//!   spelling arrives as its current variant.
//! * [`parse_value`] / [`upgrade`] for a consumer that keeps the record as
//!   JSON (the daemon, which serves records on to the viewer with every
//!   field intact). [`upgrade`] rewrites the `action` field in place, so the
//!   JSON a route serves carries the current spelling too. [`action_of`] is
//!   the typed read of that field.
//!
//! Lenient on read (contract 5): an action this binary does not know is kept
//! verbatim, as [`FlowAction::Other`]; one darkmux retired reads as
//! [`FlowAction::Retired`], known and not counted. Neither is upgraded into
//! vocabulary;
//! [`UnknownActions`] counts them so `darkmux doctor` can name them. A file
//! is never rewritten.

use crate::{FlowAction, FlowRecord};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What [`upgrade`] found in a record's `action` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionRead {
    /// A current spelling, left as it was.
    Current,
    /// A retired spelling, rewritten to its current one.
    Upgraded,
    /// An action darkmux retired with no current equivalent, left verbatim.
    Retired,
    /// An action this binary does not know, left verbatim.
    Unknown,
    /// No string `action` field at all (a schema header, a foreign line).
    Absent,
}

/// Rewrite a record's retired action spelling to its current one, in place.
pub fn upgrade(record: &mut Value) -> ActionRead {
    let Some(wire) = record.get("action").and_then(Value::as_str) else {
        return ActionRead::Absent;
    };
    let read = crate::legacy::read_action(wire);
    match read {
        FlowAction::Other(_) => return ActionRead::Unknown,
        FlowAction::Retired(_) => return ActionRead::Retired,
        _ => {}
    }
    if read.as_str() == wire {
        return ActionRead::Current;
    }
    let old = wire.to_string();
    record["action"] = Value::String(read.as_str().to_string());
    if let Some((key, detail)) = crate::legacy::detail_in_action(&old) {
        move_into_payload(record, key, detail);
    }
    ActionRead::Upgraded
}

/// Put a value a retired action string carried into `payload.<key>`, unless
/// the payload already has one. A payload that is not an object (never
/// written that way, but an archive is not ours to assume) is kept, nested
/// under `payload.legacy_payload`, rather than overwritten.
fn move_into_payload(record: &mut Value, key: &str, detail: &str) {
    match record.get("payload") {
        None | Some(Value::Null) => record["payload"] = Value::Object(Default::default()),
        Some(Value::Object(_)) => {}
        Some(other) => {
            let kept = other.clone();
            record["payload"] = serde_json::json!({ "legacy_payload": kept });
        }
    }
    if record["payload"].get(key).is_none() {
        record["payload"][key] = Value::String(detail.to_string());
    }
}

/// Parse one JSONL line to JSON and [`upgrade`] it. `None` for a line that is
/// not a JSON object.
pub fn parse_value(line: &str) -> Option<Value> {
    let mut v: Value = serde_json::from_str(line).ok()?;
    if !v.is_object() {
        return None;
    }
    upgrade(&mut v);
    Some(v)
}

/// One raw JSONL line as a consumer that forwards lines should send it: the
/// line itself, byte for byte, unless its action is a retired spelling, in
/// which case the upgraded record re-serialized. `None` for a line that is
/// not a JSON object.
pub fn upgrade_line(line: &str) -> Option<std::borrow::Cow<'_, str>> {
    let mut v: Value = serde_json::from_str(line).ok()?;
    if !v.is_object() {
        return None;
    }
    Some(match upgrade(&mut v) {
        ActionRead::Upgraded => std::borrow::Cow::Owned(v.to_string()),
        ActionRead::Current | ActionRead::Retired | ActionRead::Unknown | ActionRead::Absent => {
            std::borrow::Cow::Borrowed(line)
        }
    })
}

/// Parse one JSONL line to a typed [`FlowRecord`], upgraded the same way
/// [`parse_value`] upgrades it. `None` for a line that is not a flow record
/// (a schema header, a torn write).
pub fn parse_record(line: &str) -> Option<FlowRecord> {
    let mut v = parse_value(line)?;
    // `FlowAction`'s public deserializer refuses an unknown or retired
    // action; this is the one lenient path, so the action is read here and
    // put back after the rest of the record deserializes.
    let action = action_of(&v)?;
    v["action"] = Value::String(FlowAction::OperatorNote.as_str().to_string());
    let mut record: FlowRecord = serde_json::from_value(v).ok()?;
    record.action = action;
    Some(record)
}

/// The typed action of a JSON record, upgraded; `None` when the record has
/// no string `action`.
pub fn action_of(record: &Value) -> Option<FlowAction> {
    record.get("action")?.as_str().map(crate::legacy::read_action)
}

/// A tally of the unknown actions a read met, by name. Filled by
/// [`UnknownActions::observe`]; `darkmux doctor` reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnknownActions(BTreeMap<String, u64>);

impl UnknownActions {
    /// Count `record`'s action if this binary does not know it, current or
    /// retired.
    pub fn observe(&mut self, record: &Value) {
        if let Some(wire) = record.get("action").and_then(Value::as_str) {
            self.observe_wire(wire);
        }
    }

    /// Count one action string if this binary does not know it.
    pub fn observe_wire(&mut self, wire: &str) {
        if let FlowAction::Other(unknown) = crate::legacy::read_action(wire) {
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

    #[test]
    fn a_spaced_bookend_is_upgraded_in_place() {
        // flow-action-guard:allow — an old spelling is this test's input
        let mut v = json!({"action": "dispatch start", "handle": "coder"});
        assert_eq!(upgrade(&mut v), ActionRead::Upgraded);
        assert_eq!(v["action"], "dispatch.start");
        assert_eq!(v["handle"], "coder", "no other field is touched");
    }

    #[test]
    fn a_current_spelling_is_left_alone() {
        let mut v = json!({"action": "dispatch.start"});
        assert_eq!(upgrade(&mut v), ActionRead::Current);
        assert_eq!(v["action"], "dispatch.start");
    }

    #[test]
    fn an_unknown_action_is_kept_verbatim() {
        let mut v = json!({"action": "future.thing"});
        assert_eq!(upgrade(&mut v), ActionRead::Unknown);
        assert_eq!(v["action"], "future.thing");
        assert!(matches!(action_of(&v), Some(FlowAction::Other(_))));
    }

    #[test]
    fn a_record_without_an_action_is_absent_not_unknown() {
        let mut header = json!({"_type": "schema", "version": "1.0.0"});
        assert_eq!(upgrade(&mut header), ActionRead::Absent);
        assert_eq!(action_of(&header), None);
    }

    #[test]
    fn the_typed_and_json_reads_agree_on_a_retired_spelling() {
        // flow-action-guard:allow — an old spelling is this test's input
        let line = r#"{"ts":"t","level":"info","category":"work","tier":"local","stage":"dispatch","action":"step result","handle":"h"}"#;
        assert_eq!(parse_record(line).unwrap().action, FlowAction::StepResult);
        let v = parse_value(line).unwrap();
        assert_eq!(v["action"], "step.result");
        assert_eq!(action_of(&v), Some(FlowAction::StepResult));
    }

    #[test]
    fn a_forwarded_line_is_untouched_unless_its_spelling_is_retired() {
        let current = r#"{"b":1,"action":"dispatch.start","a":2}"#;
        assert!(matches!(upgrade_line(current), Some(std::borrow::Cow::Borrowed(l)) if l == current));
        let unknown = r#"{"action":"future.thing"}"#;
        assert_eq!(upgrade_line(unknown).unwrap(), unknown);
        // flow-action-guard:allow — an old spelling is this test's input
        let old = r#"{"action":"dispatch start","handle":"h"}"#;
        let up: Value = serde_json::from_str(&upgrade_line(old).unwrap()).unwrap();
        assert_eq!(up["action"], "dispatch.start");
        assert_eq!(up["handle"], "h");
        assert!(upgrade_line("not json").is_none());
    }

    #[test]
    fn a_verdict_carried_in_the_action_moves_into_the_payload() {
        let mut v = json!({"action": "verdict: blockers", "handle": "2B / 0F / 0N"});
        assert_eq!(upgrade(&mut v), ActionRead::Upgraded);
        assert_eq!(v["action"], "phase.review.verdict");
        assert_eq!(v["payload"]["verdict"], "blockers");
        let line = r#"{"ts":"t","level":"info","category":"review","tier":"frontier","stage":"review","action":"verdict: clean","handle":"h"}"#;
        let rec = parse_record(line).unwrap();
        assert_eq!(rec.action, FlowAction::PhaseReviewVerdict);
        assert_eq!(rec.payload.unwrap()["verdict"], "clean");
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
        // flow-action-guard:allow — an old spelling is this test's input
        write("2026-01-02", &["future.thing", "dispatch start"]);
        write("2026-01-03", &["future.thing", "dispatch.turn"]);
        std::fs::write(tmp.path().join("notes.jsonl"), "{\"action\":\"not.a.day\"}\n").unwrap();
        let tally = unknown_actions_in(tmp.path(), 2);
        assert_eq!(tally.by_name().get("future.thing"), Some(&2));
        assert!(!tally.by_name().contains_key("ancient.thing"), "only the newest two days are read");
        assert!(!tally.by_name().contains_key("not.a.day"), "a file that is not a day file is not read");
        assert_eq!(tally.total(), 2);
    }

    /// A retired action reads as known (`Retired`), keeps its spelling, and
    /// is not counted as unknown; an old spelling of a current action
    /// upgrades instead.
    #[test]
    fn a_retired_action_is_known_and_not_unknown() {
        for wire in crate::legacy::RetiredAction::KNOWN_WIRE {
            let mut v = json!({ "action": wire });
            assert_eq!(upgrade(&mut v), ActionRead::Retired, "{wire}");
            assert_eq!(v["action"], *wire, "a retired action keeps its spelling");
            assert!(matches!(action_of(&v), Some(FlowAction::Retired(_))), "{wire}");
            let mut tally = UnknownActions::default();
            tally.observe(&v);
            assert_eq!(tally.total(), 0, "{wire} is retired, not unknown");
        }
        // flow-action-guard:allow — an old spelling is this test's input
        let mut sprint = json!({ "action": "sprint start" });
        assert_eq!(upgrade(&mut sprint), ActionRead::Upgraded);
        assert_eq!(sprint["action"], "phase.start");
    }

    /// No wire string is both current and retired, and no retired string is
    /// also an upgradable old spelling.
    #[test]
    fn retired_actions_are_disjoint_from_current_and_upgraded_ones() {
        for wire in crate::legacy::RetiredAction::KNOWN_WIRE {
            assert!(!FlowAction::KNOWN_WIRE.contains(wire), "{wire} is current");
            assert!(crate::legacy::upgrade_action(wire).is_none(), "{wire} upgrades");
        }
    }

    /// A non-object payload on a `verdict: <v>` record is kept, not
    /// overwritten; an existing `payload.verdict` is never replaced.
    #[test]
    fn moving_a_verdict_never_clobbers_the_payload() {
        let mut v = json!({"action": "verdict: clean", "payload": "a string"});
        upgrade(&mut v);
        assert_eq!(v["payload"], json!({"legacy_payload": "a string", "verdict": "clean"}));
        let mut v = json!({"action": "verdict: clean", "payload": {"verdict": "kept", "n": 1}});
        upgrade(&mut v);
        assert_eq!(v["payload"], json!({"verdict": "kept", "n": 1}));
    }

    #[test]
    fn unknown_actions_are_counted_by_name_and_known_ones_are_not() {
        let mut tally = UnknownActions::default();
        // flow-action-guard:allow — an old spelling is this test's input
        for a in ["future.thing", "future.thing", "dispatch start", "dispatch.turn", "other.x"] {
            tally.observe(&json!({ "action": a }));
        }
        assert_eq!(tally.total(), 3);
        assert_eq!(tally.by_name().get("future.thing"), Some(&2));
        assert_eq!(tally.by_name().get("other.x"), Some(&1));
        // flow-action-guard:allow — an old spelling is this test's input
        assert!(!tally.by_name().contains_key("dispatch start"), "a retired spelling is upgraded, not unknown");
    }
}
