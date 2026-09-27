//! (4.0) From OUTSIDE darkmux-flow, an unknown or retired action cannot be
//! minted through serde, and one the lenient reader hands back cannot be
//! written by any sink.

use darkmux_flow::{reader, FlowAction, FlowRecord, FlowSinkWrite, LocalFileSink};

#[test]
fn serde_cannot_mint_an_unknown_or_retired_action() {
    assert!(serde_json::from_value::<FlowAction>(serde_json::json!("future.thing")).is_err());
    // flow-action-guard:allow — a retired action serde must refuse
    assert!(serde_json::from_value::<FlowAction>(serde_json::json!("telemetry.process")).is_err());
    let record = serde_json::json!({"ts": "t", "level": "info", "category": "work", "tier": "local",
        "stage": "dispatch", "action": "future.thing", "handle": "h"});
    assert!(serde_json::from_value::<FlowRecord>(record).is_err());
}

#[test]
#[serial_test::serial]
fn no_sink_writes_an_action_the_reader_could_not_name() {
    let tmp = tempfile::TempDir::new().unwrap();
    // SAFETY: serial test; nothing else reads the env concurrently.
    unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path()) };
    // flow-action-guard:allow — a retired action serde must refuse
    for action in ["future.thing", "telemetry.process"] {
        let line = format!(
            r#"{{"ts":"t","level":"info","category":"work","tier":"local","stage":"dispatch","action":"{action}","handle":"h"}}"#
        );
        let record = reader::parse_record(&line).expect("the lenient reader reads it");
        assert!(matches!(record.action, FlowAction::Other(_) | FlowAction::Retired(_)));
        assert!(LocalFileSink::new().write(&record).is_err(), "{action} must be refused");
    }
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0, "nothing reached the day file");
    unsafe { std::env::remove_var("DARKMUX_FLOWS_DIR") };
}
