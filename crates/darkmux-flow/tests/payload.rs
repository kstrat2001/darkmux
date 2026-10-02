//! (4.0) A flow record's payload is its action's own type: a record built from
//! a payload takes its action from it, no sink writes a payload that is not its
//! action's or that did not parse, and an archive reads into the type through
//! the legacy upgrade, leniently.

use darkmux_flow::payload::{HookDryRunPayload, HookFailedPayload, HookNoticePayload, KnobValue};
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

/// One sanitized line per shape the operator's real archive holds that an earlier
/// reader of these payloads would have dropped (a field a version never wrote, a
/// `null` where a count belongs, a role or class spelled by a version this one
/// does not know). Each must read as its action's typed payload.
#[test]
fn every_historical_archive_shape_reads_typed() {
    let path = format!("{}/tests/fixtures/archive_shapes.jsonl", env!("CARGO_MANIFEST_DIR"));
    let corpus = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut unread = Vec::new();
    let mut lines = 0;
    for line in corpus.lines() {
        lines += 1;
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        let shape = v["archive_shape"].as_str().unwrap_or("?").to_string();
        let record = darkmux_flow::reader::parse_record(line).unwrap_or_else(|| panic!("{shape}: not a flow record"));
        match record.payload {
            Some(darkmux_flow::Payload::Unread(_)) | None => unread.push(shape),
            Some(_) => {}
        }
    }
    assert!(lines >= 15, "the corpus is not empty");
    assert!(unread.is_empty(), "shapes that read as Unread: {unread:?}");
}

/// A word no build names reads as `Unknown` and keeps its record: the old `compactor`
/// role included (#3036), which is no longer read as the utility seat.
#[test]
fn the_old_compactor_role_and_an_unknown_word_are_unknown() {
    use darkmux_flow::payload::{DetectorKind, LmsRole, ResultClass, SeatClass};
    use darkmux_flow::Payload;
    let path = format!("{}/tests/fixtures/archive_shapes.jsonl", env!("CARGO_MANIFEST_DIR"));
    let corpus = std::fs::read_to_string(&path).unwrap();
    let mut seen = 0;
    for line in corpus.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        let shape = v["archive_shape"].as_str().unwrap();
        let payload = darkmux_flow::reader::parse_record(line).unwrap().payload.unwrap();
        match (shape, payload) {
            ("telemetry.lms with the compactor role", Payload::TelemetryLms(p)) => {
                assert_eq!(p.role, Some(LmsRole::Unknown));
                seen += 1;
            }
            ("dispatch.complete with an unknown result_class", Payload::DispatchComplete(p)) => {
                assert_eq!(p.result_class, Some(ResultClass::Unknown));
                seen += 1;
            }
            ("telemetry.detector with an unknown kind", Payload::TelemetryDetector(p)) => {
                assert_eq!(p.kind, DetectorKind::Unknown);
                seen += 1;
            }
            ("step.start with an unknown seat_class", Payload::StepStart(p)) => {
                assert_eq!(p.seat_class, Some(SeatClass::Unknown));
                seen += 1;
            }
            _ => {}
        }
    }
    assert_eq!(seen, 4);
}

/// Every terminal states its turn count on the wire: an execution that ended before any model
/// call says 0. An archived terminal written before the field existed reads it as absent, never
/// as a fabricated 0.
#[test]
fn a_dispatch_terminal_always_carries_its_turn_count() {
    use darkmux_flow::payload::DispatchEndPayload;
    let aborted = serde_json::to_value(DispatchEndPayload::aborted(None)).unwrap();
    assert_eq!(aborted["total_turns"], 0);
    let archived: DispatchEndPayload = serde_json::from_str(r#"{"wall_ms": 5}"#).unwrap();
    assert_eq!(archived.total_turns, None);
}

/// (#3035) The typed `context` and knob values read every shape the archive
/// holds and write back the JSON they were read from: a `rule` that is a list,
/// a context key this build does not name, a knob that is a number, a bool, a
/// string or `null`.
#[test]
fn a_record_context_and_a_knob_value_reserialize_to_the_json_they_were_read_from() {
    let path = format!("{}/tests/fixtures/archive_shapes.jsonl", env!("CARGO_MANIFEST_DIR"));
    let corpus = std::fs::read_to_string(&path).unwrap();
    let mut checked = 0;
    for line in corpus.lines().filter(|l| l.contains("\"archive_shape\": \"roundtrip:")) {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        let record = darkmux_flow::reader::parse_record(line).expect("a flow record");
        let payload = record.payload.expect("a payload");
        assert!(!matches!(payload, Payload::Unread(_)), "{}: read as unread", v["archive_shape"]);
        // The two fields this change typed; the rest of a payload is pinned elsewhere (a field
        // a payload type does not name, like `runtime`, is not written back).
        let back = serde_json::to_value(&payload).unwrap();
        for field in ["context", "bounds"] {
            assert_eq!(back.get(field), v["payload"].get(field), "{}: {field}", v["archive_shape"]);
        }
        checked += 1;
    }
    assert_eq!(checked, 3, "the three round-trip shapes are in the corpus");
}

/// A typed context names what the launcher writes, so an old spelling of one
/// of its keys must not cost the whole record its type.
#[test]
fn a_context_with_a_rule_list_reads_as_a_list_and_keeps_an_unknown_key() {
    use darkmux_flow::payload::{RecordContext, RuleRef};
    let ctx: RecordContext =
        serde_json::from_str(r#"{"workspace":"w","rule":["a","b"],"unit":"u","later":true}"#).unwrap();
    assert_eq!(ctx.rule, Some(RuleRef::Many(vec!["a".into(), "b".into()])));
    assert_eq!(ctx.extras.get("later"), Some(&serde_json::json!(true)));
    assert_eq!(ctx.rules, None);
}

/// Operator-run evidence, not CI: `DARKMUX_ROUNDTRIP_FLOWS=~/.darkmux/flows cargo nextest run -p
/// darkmux-flow --run-ignored only real_archive` reads every record of a real archive and checks
/// that each typed `context` and `bounds` writes back the JSON it was read from.
#[test]
#[ignore = "reads an operator's own archive: set DARKMUX_ROUNDTRIP_FLOWS"]
fn real_archive_context_and_bounds_round_trip() {
    let dir = std::env::var("DARKMUX_ROUNDTRIP_FLOWS").expect("DARKMUX_ROUNDTRIP_FLOWS names a flows directory");
    let (mut records, mut typed_fields, mut bad, mut lost_to_typing) = (0usize, 0usize, Vec::new(), Vec::new());
    for entry in std::fs::read_dir(&dir).unwrap().flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path()) else { continue };
        for line in text.lines() {
            let Some(raw) = darkmux_flow::reader::parse_value(line) else { continue };
            let Some(record) = darkmux_flow::reader::parse_record(line) else { continue };
            let (Some(payload), Some(raw_payload)) = (record.payload, raw.get("payload")) else { continue };
            if matches!(payload, Payload::Unread(_)) {
                // Unread is only acceptable when `context` and `bounds` are not why: strip them,
                // re-read, and a record that becomes readable was lost to a field this change typed.
                let mut stripped = raw.clone();
                if let Some(p) = stripped.get_mut("payload").and_then(|p| p.as_object_mut()) {
                    p.remove("context");
                    p.remove("bounds");
                }
                let readable = darkmux_flow::reader::parse_record(&stripped.to_string())
                    .and_then(|r| r.payload)
                    .is_some_and(|p| !matches!(p, Payload::Unread(_)));
                if readable {
                    lost_to_typing.push(raw["action"].to_string());
                }
                continue;
            }
            records += 1;
            let back = serde_json::to_value(&payload).unwrap();
            for field in ["context", "bounds"] {
                let Some(was) = raw_payload.get(field).filter(|v| !v.is_null()) else { continue };
                // A payload type that does not name the field (a machine-telemetry record
                // stamped with a context) never carried it: that is not this field's concern.
                let host_sample = raw["action"].as_str().is_some_and(|a| a.starts_with("machine"));
                if host_sample && field == "context" {
                    continue;
                }
                typed_fields += 1;
                if back.get(field) != Some(was) {
                    bad.push(format!("{} {field}: {was} != {:?}", raw["action"], back.get(field)));
                }
            }
        }
    }
    println!(
        "records {records}, typed context/bounds fields {typed_fields}, mismatches {}, lost to typing {}",
        bad.len(),
        lost_to_typing.len()
    );
    assert!(lost_to_typing.is_empty(), "records unread because of `context`/`bounds`: {:?}", &lost_to_typing[..lost_to_typing.len().min(5)]);
    assert!(bad.is_empty(), "{:#?}", &bad[..bad.len().min(5)]);
    assert!(typed_fields > 0);
}

/// (#3035, contract 7) Leniency is per FIELD: a wrong-typed `context` key, a wrong-typed
/// `context`, or a knob value no darkmux wrote costs that field, never the record's type.
#[test]
fn a_wrong_typed_context_or_knob_costs_that_field_not_the_payload() {
    use darkmux_flow::payload::RecordContext;
    let read = |action: &str, payload: serde_json::Value| {
        let line = serde_json::json!({"ts":"t","level":"info","category":"work","tier":"local","stage":"dispatch",
            "action": action, "handle": "h", "payload": payload})
        .to_string();
        darkmux_flow::reader::parse_record(&line).expect("a record").payload.expect("a payload")
    };
    for context in [
        serde_json::json!({"unit": 7, "source": "app"}),
        serde_json::json!({"site": {"file": "a.ts", "line": 3}, "source": "app"}),
        serde_json::json!({"rule": 5, "source": "app"}),
        serde_json::json!("not an object"),
    ] {
        match read("dispatch.complete", serde_json::json!({"result_class": "stop", "wall_ms": 5, "context": context})) {
            Payload::DispatchComplete(p) => {
                assert_eq!(p.wall_ms, Some(5), "{context}");
                let ctx = p.context.unwrap_or_default();
                assert!(ctx.unit.is_none() && ctx.site.is_none() && ctx.rule.is_none(), "{context}");
                assert_eq!(ctx.source.is_some(), context.get("source").is_some(), "the good keys survive: {context}");
            }
            other => panic!("a typed dispatch.complete, got {other:?} for {context}"),
        }
    }
    let ctx: RecordContext = serde_json::from_value(serde_json::json!({"unit": 7, "model": "m"})).unwrap();
    assert_eq!((ctx.unit, ctx.model.as_deref()), (None, Some("m")));
    let start = read("dispatch.start", serde_json::json!({"prompt_chars": 4,
        "bounds": {"max_tokens_per_call": {"value": null, "source": "built-in"},
            "inactivity_timeout_seconds": {"value": 600, "source": "config"},
            "max_turns": {"value": [1], "source": "config"}, "max_tokens": {"value": 9, "source": "env"}}}));
    match start {
        Payload::DispatchStart(p) => {
            assert_eq!(p.prompt_chars, Some(4));
            let b = p.bounds.expect("bounds");
            // A value no darkmux wrote is UNKNOWN, kept verbatim: it never reads as `null`,
            // which means uncapped.
            assert_eq!(b.max_turns.value, Some(KnobValue::Unrecognized(serde_json::json!([1]))));
            assert_ne!(b.max_turns.value, None, "a wrong-typed knob must not read as uncapped");
            assert_eq!(b.max_tokens.value, Some(9_u64.into()));
            assert_eq!(b.max_tokens_per_call.value, None, "an explicit null is uncapped");
        }
        other => panic!("a typed dispatch.start, got {other:?}"),
    }
}

/// (#3035, contract 7) A malformed `bounds` block costs the bounds, never the whole
/// `dispatch.start` payload; a wrong-typed OPTIONAL knob costs that knob, never its siblings.
#[test]
fn a_malformed_bounds_block_costs_the_bounds_not_the_dispatch_start() {
    let read = |payload: serde_json::Value| {
        let line = serde_json::json!({"ts":"t","level":"info","category":"work","tier":"local","stage":"dispatch",
            "action": "dispatch.start", "handle": "h", "payload": payload})
        .to_string();
        darkmux_flow::reader::parse_record(&line).expect("a record").payload.expect("a payload")
    };
    match read(serde_json::json!({"prompt_chars": 4, "bounds": "not an object"})) {
        Payload::DispatchStart(p) => assert_eq!((p.prompt_chars, p.bounds), (Some(4), None)),
        other => panic!("a typed dispatch.start, got {other:?}"),
    }
    match read(serde_json::json!({"prompt_chars": 4, "bounds": {"max_tokens_per_call": {"value": 1, "source": "env"}}})) {
        Payload::DispatchStart(p) => assert_eq!((p.prompt_chars, p.bounds), (Some(4), None), "a missing required knob costs the bounds"),
        other => panic!("a typed dispatch.start, got {other:?}"),
    }
    let knob = |v: serde_json::Value| serde_json::json!({"value": v, "source": "config"});
    let bounds = serde_json::json!({"max_tokens_per_call": knob(1.into()), "inactivity_timeout_seconds": knob(2.into()),
        "max_turns": knob(3.into()), "max_tokens": knob(4.into()),
        "turn_delay_ms": "not a knob", "feedback_injection": knob(true.into())});
    match read(serde_json::json!({"prompt_chars": 4, "bounds": bounds})) {
        Payload::DispatchStart(p) => {
            let b = p.bounds.expect("the good knobs survive");
            assert!(b.turn_delay_ms.is_none(), "the malformed optional knob is dropped");
            assert_eq!(b.feedback_injection.map(|k| k.value), Some(Some(true.into())));
            assert_eq!(b.max_tokens.value, Some(4_u64.into()));
        }
        other => panic!("a typed dispatch.start, got {other:?}"),
    }
}

/// (#3035) The unmanaged-endpoint flag and spend were spelled `remote` and
/// `remote_tokens` before 5.0: an archived record still reads, and nothing
/// writes the old key.
#[test]
fn an_archived_remote_key_reads_as_unmanaged_and_is_never_written() {
    use darkmux_flow::payload::{DispatchEndPayload, StepResultPayload, UsagePayload};
    let usage: UsagePayload = serde_json::from_str(r#"{"remote": true}"#).unwrap();
    assert_eq!(usage.unmanaged, Some(true));
    let step: StepResultPayload = serde_json::from_str(r#"{"step_id": "s", "kind": "dispatch.map", "remote": false}"#).unwrap();
    assert_eq!(step.unmanaged, Some(false));
    let end: DispatchEndPayload = serde_json::from_str(r#"{"wall_ms": 5, "remote_tokens": 9}"#).unwrap();
    assert_eq!(end.unmanaged_tokens, Some(9));
    let written = serde_json::to_string(&(usage, step, end)).unwrap();
    assert!(written.contains("\"unmanaged\"") && written.contains("\"unmanaged_tokens\""), "{written}");
    assert!(!written.contains("remote"), "{written}");
}
