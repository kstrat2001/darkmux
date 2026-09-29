//! (4.0) A flow record's payload is its action's own type: a record built from
//! a payload takes its action from it, no sink writes a payload that is not its
//! action's or that did not parse, and an archive reads into the type through
//! the legacy upgrade, leniently.

use darkmux_flow::payload::{HookDryRunPayload, HookFailedPayload, HookNoticePayload};
use darkmux_flow::{reader, Category, FlowAction, FlowRecord, FlowSinkWrite, Level, LocalFileSink, Payload, Stage};
use darkmux_types::session_id::{RunId, SessionId};

fn dry_run() -> Payload {
    Payload::HookDryRun(HookDryRunPayload {
        rule_index: 2,
        delivered_action: Some("operator.note".to_string()),
        delivery_id: "d-1".to_string(),
        dump_path: "/tmp/dump".to_string(),
    })
}

fn build(payload: Payload) -> FlowRecord {
    FlowRecord::for_session_with(&SessionId::run(RunId::mission("m-1").unwrap()), Level::Info, Category::Machinery, Stage::Ship, payload, "h")
}

#[test]
fn a_record_built_from_a_payload_takes_the_payloads_action() {
    let record = build(dry_run());
    assert_eq!(record.action, FlowAction::HookDryRun);
    assert_eq!(record.payload.as_ref().map(Payload::action), Some(FlowAction::HookDryRun));
}

/// Every write goes through the flow write check, so a payload that is not its
/// action's cannot reach a file, however the record was assembled.
#[test]
#[serial_test::serial]
fn no_sink_writes_another_actions_payload_or_an_unread_one() {
    let tmp = tempfile::TempDir::new().unwrap();
    // SAFETY: serial test; nothing else reads the env concurrently.
    unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path()) };

    let mut mismatched = build(dry_run());
    mismatched.action = FlowAction::HookFired;
    assert!(LocalFileSink::new().write(&mismatched).is_err(), "a hook.fired carrying a hook.dry_run payload");

    let line = r#"{"ts":"t","level":"info","category":"machinery","tier":"darkmux","stage":"ship","action":"hook.dry_run","handle":"h","payload":{"rule_index":"not a number"}}"#;
    let unread = reader::parse_record(line).expect("the lenient reader reads it");
    assert!(matches!(unread.payload, Some(Payload::Unread(_))));
    assert!(LocalFileSink::new().write(&unread).is_err(), "an unread payload is never written");

    let mut none_takes_payload = build(dry_run());
    none_takes_payload.action = FlowAction::SessionEnd;
    assert!(LocalFileSink::new().write(&none_takes_payload).is_err(), "session.end carries no payload");

    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0, "nothing reached the day file");
    assert!(LocalFileSink::new().write(&build(dry_run())).is_ok(), "the payload of its own action is written");
    unsafe { std::env::remove_var("DARKMUX_FLOWS_DIR") };
}

#[test]
fn an_archived_payload_reads_into_its_actions_type_and_ignores_unknown_fields() {
    let line = r#"{"ts":"t","level":"info","category":"machinery","tier":"darkmux","stage":"ship","action":"hook.dry_run","handle":"h","payload":{"rule_index":2,"delivered_action":null,"delivery_id":"d","dump_path":"/x","a_key_no_reader_knows":true}}"#;
    let record = reader::parse_record(line).expect("read");
    match record.payload {
        Some(Payload::HookDryRun(p)) => {
            assert_eq!(p.rule_index, 2);
            assert_eq!(p.delivered_action, None);
        }
        other => panic!("a typed hook.dry_run payload, got {other:?}"),
    }
}

#[test]
fn an_unread_payload_keeps_its_json_and_serializes_back_to_it() {
    let raw = serde_json::json!({"rule_index": "not a number", "kept": [1, 2]});
    let line = serde_json::json!({"ts":"t","level":"info","category":"machinery","tier":"darkmux","stage":"ship",
        "action":"hook.dry_run","handle":"h","payload": raw})
    .to_string();
    let record = reader::parse_record(&line).expect("read");
    let Some(unread @ Payload::Unread(_)) = record.payload else { panic!("a payload that is not the type is unread") };
    assert_eq!(serde_json::to_value(&unread).unwrap(), raw);
}

/// One action, two shapes: a failed delivery and a rule-level notice both read
/// as `hook.failed`, each as its own form.
#[test]
fn hook_failed_reads_a_notice_and_a_delivery_as_their_own_forms() {
    let read = |payload: &str| {
        let line = format!(
            r#"{{"ts":"t","level":"error","category":"machinery","tier":"darkmux","stage":"ship","action":"hook.failed","handle":"h","payload":{payload}}}"#
        );
        reader::parse_record(&line).expect("read").payload
    };
    let notice = read(r#"{"rule_index":0,"target_host":"127.0.0.1:1","error":"outbox over cap","dropped_count":3}"#);
    assert_eq!(
        notice,
        Some(Payload::HookFailed(HookFailedPayload::Notice(HookNoticePayload {
            rule_index: 0,
            target_host: "127.0.0.1:1".to_string(),
            error: "outbox over cap".to_string(),
            dropped_count: Some(3),
            orphaned_transforms: None,
        })))
    );
    let delivery = read(r#"{"rule_index":0,"target_host":"h","delivered_action":null,"attempt":2,"delivery_id":"d","error":"boom"}"#);
    assert!(matches!(delivery, Some(Payload::HookFailed(HookFailedPayload::Delivery(_)))), "{delivery:?}");
}

/// A producer never builds a payload from JSON: `Payload::settle` is the
/// reader's step (an archived line into its action's type), and a production
/// call anywhere else would be the untyped write path this type exists to end.
#[test]
fn only_the_reader_settles_a_payload_from_json() {
    fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if p.is_dir() {
                if !matches!(name.as_str(), "target" | "node_modules" | ".git" | "tests") {
                    rust_files(&p, out);
                }
            } else if name.ends_with(".rs") && !name.ends_with("_tests.rs") {
                out.push(p);
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    rust_files(&root.join("src"), &mut files);
    assert!(files.len() > 100, "the walk found the workspace sources");
    let allowed = ["crates/darkmux-flow/src/reader.rs", "crates/darkmux-flow/src/schema.rs", "crates/darkmux-flow/src/payload/mod.rs"];
    let mut offenders = Vec::new();
    for f in files {
        let rel = f.strip_prefix(root).unwrap().to_string_lossy().to_string();
        if allowed.contains(&rel.as_str()) {
            continue;
        }
        let text = std::fs::read_to_string(&f).unwrap();
        let production = text.split("\n#[cfg(test)]").next().unwrap();
        if production.contains("Payload::settle(") {
            offenders.push(rel);
        }
    }
    assert!(offenders.is_empty(), "production code settling a payload from JSON: {offenders:?}");
}
