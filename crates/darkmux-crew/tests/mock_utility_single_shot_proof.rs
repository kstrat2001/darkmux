//! (#2914) Real, container-free proof of the LEAN utility path: darkmux's
//! own jobs (radio routing here; compaction takes the same shape inside
//! the runtime) run on the machine's ONE utility model and leave exactly
//! two records behind: a `utility.start` marker (#2915) and their
//! `telemetry.tokens` usage record with `purpose: utility`. No session, no `dispatch start`/`dispatch complete`
//! bookends, no run. That is the amended contract 2 (CLAUDE.md, "Dispatch
//! liveness"): bookends are for WORK executions; utility jobs are
//! accounted by usage records and made visible by #2915's utility state.
//!
//! Same harness as `mock_single_shot_proof.rs` (a real in-process
//! `httpmock` server reached over the crate's own curl path): zero
//! LMStudio, zero Docker, zero real AI. Not `#[ignore]`d.
use httpmock::prelude::*;
use serde_json::Value;
use std::path::Path;

use darkmux_crew::utility::{run_utility_single_shot, UtilityJob};

/// A registry with ONE work profile and the machine utility binding in the
/// object form. `mock-util` is the utility model; the work profile never
/// lists it, so the proof also shows the utility path reads the BINDING,
/// not a profile (the work profile's model is a different id on purpose).
fn write_registry(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("profiles.json");
    let body = serde_json::json!({
        "schema_version": "2.0",
        "default_profile": "work",
        "profiles": {
            "work": { "models": [ { "id": "mock-worker", "n_ctx": 8192 } ] }
        },
        "internal": { "utility": { "id": "mock-util", "n_ctx": 4096 } }
    });
    std::fs::write(&path, serde_json::to_string_pretty(&body).unwrap()).expect("writing the mock registry");
    path
}

/// Every flow record on disk under the isolated flows dir, in file order.
/// The day file's own schema header line (`_type: schema`, no `action`) is
/// not a record and is skipped.
fn all_flow_records(flows_dir: &Path) -> Vec<Value> {
    let mut records = Vec::new();
    for entry in std::fs::read_dir(flows_dir).expect("reading the isolated flows dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        for line in std::fs::read_to_string(&path).expect("reading a flow file").lines() {
            if let Ok(record) = serde_json::from_str::<Value>(line) {
                if record.get("action").is_some() {
                    records.push(record);
                }
            }
        }
    }
    records
}

fn with_isolated_flows<T>(flows_dir: &Path, f: impl FnOnce() -> T) -> T {
    // SAFETY (matches `mock_single_shot_proof.rs`): each `tests/*.rs` file is
    // its own process and every test here is `#[serial]`, so no other test
    // races this env var.
    let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
    unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", flows_dir) };
    let out = f();
    unsafe {
        match prev {
            Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
            None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
        }
    }
    out
}

#[test]
#[serial_test::serial]
fn a_utility_job_runs_on_the_binding_and_leaves_only_its_usage_record() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            // The BINDING's id is on the wire (bare here: a non-LMStudio
            // base URL skips residency, exactly like the work primitive),
            // never the work profile's model.
            .json_body_partial(r#"{ "model": "mock-util" }"#)
            // The system prompt on the wire is the router role's own frozen
            // prompt (contract 6), loaded from the role library like any
            // other role's; `mock.assert()` below fails if it is not.
            .body_contains("# Radio Router");
        then.status(200).header("content-type", "application/json").json_body(serde_json::json!({
            "id": "mock-1",
            "object": "chat.completion",
            "created": 0,
            "model": "mock-util-served",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "```json\n{\"command\": \"review\"}\n```" },
                "finish_reason": "stop",
            }],
            "usage": { "prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10 },
        }));
    });

    let registry_dir = tempfile::tempdir().unwrap();
    let profiles_path = write_registry(registry_dir.path());
    let flows_dir = tempfile::tempdir().unwrap();

    let reply = with_isolated_flows(flows_dir.path(), || {
        run_utility_single_shot(&UtilityJob {
            role_id: darkmux_crew::loader::RADIO_ROUTER_ROLE_ID,
            message: "review this when you get a sec",
            timeout_seconds: 30,
            max_tokens: 256,
            config_path: Some(profiles_path.to_str().unwrap()),
            base_url_override: Some(&server.base_url()),
        })
    })
    .expect("the utility call round-trips through the mock server");

    assert!(reply.content.contains("\"command\": \"review\""), "the mock's own content came back: {:?}", reply.content);
    mock.assert();

    // LEAN: exactly one record, the usage record; no bookends, no session.
    let records = all_flow_records(flows_dir.path());
    let usage: Vec<&Value> = records
        .iter()
        .filter(|r| r["category"] == "telemetry" && r["source"] == "tokens")
        .collect();
    assert_eq!(usage.len(), 1, "one model call, one usage record: {records:#?}");
    let rec = usage[0];
    assert_eq!(rec["action"], "telemetry.tokens");
    assert_eq!(rec["payload"]["purpose"], "utility", "{rec}");
    assert_eq!(rec["payload"]["call_kind"], "single_shot");
    assert_eq!(rec["payload"]["requested_model"], "mock-util");
    assert_eq!(rec["payload"]["reported_model"], "mock-util-served");
    assert_eq!(rec["payload"]["total_tokens"], 10);
    assert_eq!(rec["handle"], darkmux_crew::loader::RADIO_ROUTER_ROLE_ID, "the record names the JOB");
    assert_eq!(rec["model"], "mock-util");
    assert!(rec["session_id"].is_null(), "a utility job mints no session: {rec}");
    let bookends: Vec<&Value> = records
        .iter()
        .filter(|r| r["action"].as_str().is_some_and(|a| darkmux_flow::is_dispatch_start(a) || darkmux_flow::is_dispatch_terminal(a)))
        .collect();
    assert!(bookends.is_empty(), "a utility job emits no dispatch bookends (contract 2, #2914): {bookends:#?}");
    assert_eq!(rec["payload"]["job"], "radio_routing", "the usage record names the job kind (#2915): {rec}");
    // (#2915) VISIBLE: the job's `utility.start` precedes its usage record,
    // and is just as lean (no session).
    assert_eq!(records.len(), 2, "the start marker and the usage record, nothing else: {records:#?}");
    let start = &records[0];
    assert_eq!(start["action"], darkmux_crew::usage::UTILITY_START_ACTION, "the start comes first: {records:#?}");
    assert_eq!(start["payload"]["job"], "radio_routing", "{start}");
    assert_eq!(start["payload"]["model"], "mock-util", "{start}");
    assert!(start["payload"].get("serves").is_none(), "routing serves no execution: {start}");
    assert_eq!(start["payload"]["stall_after_seconds"], 30, "the job's own bound: {start}");
    assert_eq!(start["handle"], darkmux_crew::loader::RADIO_ROUTER_ROLE_ID);
    assert!(start["session_id"].is_null(), "a utility job mints no session: {start}");
    // (#2915 review, MUST 1 / C4) The start and its end share a job id, and
    // carry ms-precision times: a flow `ts` is whole-second.
    let id = start["payload"]["job_id"].as_str().expect("a start mints a job id");
    assert!(!id.is_empty());
    assert_eq!(rec["payload"]["job_id"], id, "the usage record echoes the start's job id: {rec}");
    let started = start["payload"]["started_at_ms"].as_u64().expect("started_at_ms");
    let ended = rec["payload"]["ended_at_ms"].as_u64().expect("ended_at_ms");
    assert!(ended >= started, "{started} <= {ended}");
    assert_eq!(rec["payload"]["duration_ms"].as_u64(), Some(ended - started));
}

/// (#2915) A utility job whose model call fails after it started says so:
/// `utility.start` then `utility.error`, and no usage record (no reply, no
/// countable tokens). Without the error record a failed routing call would
/// leave a start with no end, and the fleet card would read it as busy, then
/// stalled, until the next job.
#[test]
#[serial_test::serial]
fn a_utility_job_whose_call_fails_ends_with_utility_error() {
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(500).body("boom");
    });
    let registry_dir = tempfile::tempdir().unwrap();
    let profiles_path = write_registry(registry_dir.path());
    let flows_dir = tempfile::tempdir().unwrap();
    let res = with_isolated_flows(flows_dir.path(), || {
        run_utility_single_shot(&UtilityJob {
            role_id: darkmux_crew::loader::RADIO_ROUTER_ROLE_ID,
            message: "anything",
            timeout_seconds: 5,
            max_tokens: 16,
            config_path: Some(profiles_path.to_str().unwrap()),
            base_url_override: Some(&server.base_url()),
        })
    });
    assert!(res.is_err(), "a 500 is an error");
    let records = all_flow_records(flows_dir.path());
    let actions: Vec<&str> = records.iter().filter_map(|r| r["action"].as_str()).collect();
    assert_eq!(
        actions,
        vec![darkmux_crew::usage::UTILITY_START_ACTION, darkmux_crew::usage::UTILITY_ERROR_ACTION],
        "{records:#?}"
    );
    let end = &records[1];
    assert_eq!(end["payload"]["job"], "radio_routing", "{end}");
    assert_eq!(end["payload"]["job_id"], records[0]["payload"]["job_id"], "the error echoes the start's job id");
    assert!(end["payload"]["ended_at_ms"].as_u64().is_some(), "{end}");
    assert!(end["session_id"].is_null(), "{end}");
}

/// (#2915 review, MUST 1) Two jobs never share an id.
#[test]
#[serial_test::serial]
fn each_utility_job_mints_its_own_id() {
    let server = MockServer::start();
    let _m = server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200).header("content-type", "application/json").json_body(serde_json::json!({
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": "ok" }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
        }));
    });
    let registry_dir = tempfile::tempdir().unwrap();
    let profiles_path = write_registry(registry_dir.path());
    let flows_dir = tempfile::tempdir().unwrap();
    with_isolated_flows(flows_dir.path(), || {
        for _ in 0..2 {
            run_utility_single_shot(&UtilityJob {
                role_id: darkmux_crew::loader::RADIO_ROUTER_ROLE_ID,
                message: "x",
                timeout_seconds: 5,
                max_tokens: 8,
                config_path: Some(profiles_path.to_str().unwrap()),
                base_url_override: Some(&server.base_url()),
            })
            .unwrap();
        }
    });
    let records = all_flow_records(flows_dir.path());
    let ids: std::collections::BTreeSet<String> = records
        .iter()
        .filter(|r| r["action"] == darkmux_crew::usage::UTILITY_START_ACTION)
        .map(|r| r["payload"]["job_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 2, "{records:#?}");
}

#[test]
#[serial_test::serial]
fn a_utility_job_with_no_binding_is_a_loud_error_naming_the_fix() {
    let registry_dir = tempfile::tempdir().unwrap();
    let path = registry_dir.path().join("profiles.json");
    std::fs::write(
        &path,
        r#"{"schema_version":"2.0","default_profile":"work","profiles":{"work":{"models":[{"id":"mock-worker","n_ctx":8192}]}}}"#,
    )
    .unwrap();
    let flows_dir = tempfile::tempdir().unwrap();
    let err = with_isolated_flows(flows_dir.path(), || {
        run_utility_single_shot(&UtilityJob {
            role_id: darkmux_crew::loader::RADIO_ROUTER_ROLE_ID,
            message: "anything",
            timeout_seconds: 5,
            max_tokens: 16,
            config_path: Some(path.to_str().unwrap()),
            base_url_override: Some("http://127.0.0.1:9"),
        })
    })
    .expect_err("no utility model, no utility job");
    let msg = format!("{err:#}");
    assert!(msg.contains("internal.utility"), "names the fix: {msg}");
    assert!(all_flow_records(flows_dir.path()).is_empty(), "nothing recorded for a job that never ran");
}

/// (#2914 review, C7) The REAL arm: no base-URL override, so the binding is
/// ensured resident through `lms` and the darkmux-NAMESPACED identifier goes
/// on the wire (#2240), exactly as the compactor's own residency does. The
/// mock server stands in as LMStudio (`DARKMUX_LMSTUDIO_URL`) and a fake
/// `lms` (`DARKMUX_LMS_BIN`) reports the binding already resident at its
/// window, so the preflight reuses it and never loads anything.
#[test]
#[serial_test::serial]
fn the_residency_arm_puts_the_namespaced_binding_on_the_wire() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .json_body_partial(r#"{ "model": "darkmux:mock-util" }"#);
        then.status(200).header("content-type", "application/json").json_body(serde_json::json!({
            "id": "mock-2",
            "object": "chat.completion",
            "created": 0,
            "model": "mock-util",
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": "ok" }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
        }));
    });

    let registry_dir = tempfile::tempdir().unwrap();
    let profiles_path = write_registry(registry_dir.path());
    let flows_dir = tempfile::tempdir().unwrap();
    // A fake `lms`: `ps --json` says the binding is resident under darkmux's
    // own namespaced identifier at its declared window; anything else is a
    // no-op. Reaching `load`/`unload` would mean the preflight did not
    // recognize its own instance.
    let bin_dir = tempfile::tempdir().unwrap();
    let fake_lms = bin_dir.path().join("lms");
    std::fs::write(
        &fake_lms,
        "#!/bin/sh\n\
         if [ \"$1\" = \"ps\" ]; then\n\
         echo '[{\"identifier\":\"darkmux:mock-util\",\"modelKey\":\"mock-util\",\"status\":\"loaded\",\"contextLength\":4096}]'\n\
         exit 0\n\
         fi\n\
         echo \"unexpected lms call: $*\" >&2\n\
         exit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_lms, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let prev_url = std::env::var("DARKMUX_LMSTUDIO_URL").ok();
    let prev_lms = std::env::var("DARKMUX_LMS_BIN").ok();
    unsafe {
        std::env::set_var("DARKMUX_LMSTUDIO_URL", server.base_url());
        std::env::set_var("DARKMUX_LMS_BIN", &fake_lms);
    }
    let reply = with_isolated_flows(flows_dir.path(), || {
        run_utility_single_shot(&UtilityJob {
            role_id: darkmux_crew::loader::RADIO_ROUTER_ROLE_ID,
            message: "anything",
            timeout_seconds: 30,
            max_tokens: 64,
            config_path: Some(profiles_path.to_str().unwrap()),
            base_url_override: None,
        })
    });
    unsafe {
        match prev_url {
            Some(v) => std::env::set_var("DARKMUX_LMSTUDIO_URL", v),
            None => std::env::remove_var("DARKMUX_LMSTUDIO_URL"),
        }
        match prev_lms {
            Some(v) => std::env::set_var("DARKMUX_LMS_BIN", v),
            None => std::env::remove_var("DARKMUX_LMS_BIN"),
        }
    }
    let reply = reply.expect("the residency arm round-trips through the mock LMStudio");
    assert_eq!(reply.content, "ok");
    // The request carried `darkmux:mock-util`, or this never matched.
    mock.assert();

    let records = all_flow_records(flows_dir.path());
    assert_eq!(records.len(), 2, "the start marker and the usage record: {records:#?}");
    assert_eq!(records[0]["action"], darkmux_crew::usage::UTILITY_START_ACTION);
    assert_eq!(records[0]["payload"]["model"], "darkmux:mock-util", "the start names the wire id too");
    assert_eq!(records[1]["payload"]["requested_model"], "darkmux:mock-util", "the usage record names the wire id");
    assert_eq!(records[1]["model"], "darkmux:mock-util");
}
