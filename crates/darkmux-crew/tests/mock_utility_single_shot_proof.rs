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
use darkmux_types::test_isolation::IsolatedState;

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

/// Pins every darkmux state destination under a root this test alone owns,
/// for as long as the guard lives, with the flows dir created so a job that
/// records nothing still leaves an empty, readable one. The pinned home is
/// also what scopes the LIVE channel: its socket derives from the home, so
/// this test's socket is its own. Pinning only the flows dir left the home
/// ambient, and every test process sharing that home (a suite run with
/// `DARKMUX_HOME` set) sent its utility edges into whichever test had bound
/// the socket.
fn isolated_state() -> IsolatedState {
    // `#[serial]` on every test here keeps the env mutation single-threaded
    // within this process; each `tests/*.rs` file is its own process.
    let state = IsolatedState::new();
    std::fs::create_dir_all(state.join("flows")).expect("creating the isolated flows dir");
    state
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
    let state = isolated_state();
    let flows_dir = state.join("flows");

    let reply = run_utility_single_shot(&UtilityJob {
        role_id: darkmux_crew::loader::RADIO_ROUTER_ROLE_ID,
        message: "review this when you get a sec",
        timeout_seconds: 30,
        max_tokens: 256,
        config_path: Some(profiles_path.to_str().unwrap()),
        base_url_override: Some(&server.base_url()),
    })
    .expect("the utility call round-trips through the mock server");

    assert!(reply.content.contains("\"command\": \"review\""), "the mock's own content came back: {:?}", reply.content);
    mock.assert();

    // LEAN: exactly one record, the usage record; no bookends, no session.
    let records = all_flow_records(&flows_dir);
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
        .filter(|r| {
            matches!(
                darkmux_flow::reader::action_of(r),
                Some(darkmux_flow::FlowAction::DispatchStart | darkmux_flow::FlowAction::DispatchComplete | darkmux_flow::FlowAction::DispatchError)
            )
        })
        .collect();
    assert!(bookends.is_empty(), "a utility job emits no dispatch bookends (contract 2, #2914): {bookends:#?}");
    assert_eq!(rec["payload"]["job"], "radio_routing", "the usage record names the job kind (#2915): {rec}");
    // (#2915) VISIBLE: the job's `utility.start` precedes its usage record,
    // and is just as lean (no session).
    assert_eq!(records.len(), 2, "the start marker and the usage record, nothing else: {records:#?}");
    let start = &records[0];
    assert_eq!(start["action"], "utility.start", "the start comes first: {records:#?}");
    assert_eq!(start["payload"]["job"], "radio_routing", "{start}");
    assert_eq!(start["payload"]["model"], "mock-util", "{start}");
    assert!(start["payload"].get("serves").is_none(), "routing serves no execution: {start}");
    assert_eq!(start["payload"]["stall_after_ms"], 30_000, "the job's own bound: {start}");
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
    let state = isolated_state();
    let flows_dir = state.join("flows");
    let res = run_utility_single_shot(&UtilityJob {
        role_id: darkmux_crew::loader::RADIO_ROUTER_ROLE_ID,
        message: "anything",
        timeout_seconds: 5,
        max_tokens: 16,
        config_path: Some(profiles_path.to_str().unwrap()),
        base_url_override: Some(&server.base_url()),
    });
    assert!(res.is_err(), "a 500 is an error");
    let records = all_flow_records(&flows_dir);
    let actions: Vec<&str> = records.iter().filter_map(|r| r["action"].as_str()).collect();
    assert_eq!(
        actions,
        vec!["utility.start", "utility.error"],
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
    let state = isolated_state();
    let flows_dir = state.join("flows");
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
    let records = all_flow_records(&flows_dir);
    let ids: std::collections::BTreeSet<String> = records
        .iter()
        .filter(|r| r["action"] == "utility.start")
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
    let state = isolated_state();
    let flows_dir = state.join("flows");
    let err = run_utility_single_shot(&UtilityJob {
        role_id: darkmux_crew::loader::RADIO_ROUTER_ROLE_ID,
        message: "anything",
        timeout_seconds: 5,
        max_tokens: 16,
        config_path: Some(path.to_str().unwrap()),
        base_url_override: Some("http://127.0.0.1:9"),
    })
    .expect_err("no utility model, no utility job");
    let msg = format!("{err:#}");
    assert!(msg.contains("internal.utility"), "names the fix: {msg}");
    assert!(all_flow_records(&flows_dir).is_empty(), "nothing recorded for a job that never ran");
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
    let state = isolated_state();
    let flows_dir = state.join("flows");
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
    let reply = run_utility_single_shot(&UtilityJob {
        role_id: darkmux_crew::loader::RADIO_ROUTER_ROLE_ID,
        message: "anything",
        timeout_seconds: 30,
        max_tokens: 64,
        config_path: Some(profiles_path.to_str().unwrap()),
        base_url_override: None,
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

    let records = all_flow_records(&flows_dir);
    assert_eq!(records.len(), 2, "the start marker and the usage record: {records:#?}");
    assert_eq!(records[0]["action"], "utility.start");
    assert_eq!(records[0]["payload"]["model"], "darkmux:mock-util", "the start names the wire id too");
    assert_eq!(records[1]["payload"]["requested_model"], "darkmux:mock-util", "the usage record names the wire id");
    assert_eq!(records[1]["model"], "darkmux:mock-util");
}

/// (#2928) The job's two edges also go out on the LIVE channel, at once and
/// in order, carrying the same job id as its durable records, and the flow
/// log is exactly what it was without the channel: a start and a usage
/// record.
#[test]
#[serial_test::serial]
fn a_utility_job_sends_its_start_and_end_on_the_live_channel() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200).header("content-type", "application/json").json_body(serde_json::json!({
            "id": "mock-1", "object": "chat.completion", "created": 0, "model": "mock-util",
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": "{\"command\": \"review\"}" }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10 },
        }));
    });
    let registry_dir = tempfile::tempdir().unwrap();
    let profiles_path = write_registry(registry_dir.path());
    // Stands in for the home this process inherits. A suite run with
    // `DARKMUX_HOME` exported hands the SAME home to every test process, so
    // a sibling test (or a real dispatch) sends to that home's socket.
    let _inherited = IsolatedState::new();
    let sibling_socket = darkmux_flow::live::local_socket_path().expect("a sibling's socket path");
    let state = isolated_state();
    let flows_dir = state.join("flows");
    // The daemon's ingest socket, where this process's sender resolves it.
    let sock = darkmux_flow::live::local_socket_path().expect("a socket path");
    let rx = darkmux_flow::live::bind_ingest(&sock).expect("binding the ingest socket");
    rx.set_nonblocking(true).unwrap();
    // The sibling's own utility edge must never reach this test's socket.
    let mut sibling_edge = darkmux_flow::live::LiveSample::new(darkmux_flow::live::LiveKind::Utility, 1, 250);
    sibling_edge.fields.insert("event".into(), serde_json::json!("sibling"));
    darkmux_flow::live::LiveSender::to_path(sibling_socket).send(&sibling_edge);

    run_utility_single_shot(&UtilityJob {
        role_id: darkmux_crew::loader::RADIO_ROUTER_ROLE_ID,
        message: "review this",
        timeout_seconds: 30,
        max_tokens: 256,
        config_path: Some(profiles_path.to_str().unwrap()),
        base_url_override: Some(&server.base_url()),
    })
    .expect("the utility call round-trips");

    let mut samples = Vec::new();
    let mut buf = [0u8; darkmux_flow::live::MAX_LIVE_DATAGRAM];
    while let Ok(n) = rx.recv(&mut buf) {
        samples.push(darkmux_flow::live::LiveSample::from_datagram(&buf[..n]).expect("well-formed"));
    }
    let _ = std::fs::remove_file(&sock);
    // (#2928 review, C-7) Leave no empty fallback directory behind.
    if let Some(dir) = sock.parent().filter(|d| d.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("darkmux-live-"))) {
        let _ = std::fs::remove_dir(dir);
    }
    assert_eq!(samples.len(), 2, "one start, one end: {samples:?}");
    assert_eq!(samples[0].fields["event"], "start");
    assert_eq!(samples[1].fields["event"], "end");
    assert!(samples.iter().all(|s| s.kind == darkmux_flow::live::LiveKind::Utility && s.session_id.is_none()));
    let records = all_flow_records(&flows_dir);
    assert_eq!(records.len(), 2, "the live channel added no flow record");
    assert_eq!(samples[0].fields["job_id"], records[0]["payload"]["job_id"], "the live start names the durable job id");
    assert_eq!(samples[1].fields["job_id"], records[0]["payload"]["job_id"]);
    assert!(samples[1].at_ms >= samples[0].at_ms);
}

/// A fake `lms` that records every invocation to `<dir>/lms.log` and fails,
/// so a test can show darkmux never ran it (#3207).
fn recording_fake_lms(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let log = dir.join("lms.log");
    let fake = dir.join("lms");
    std::fs::write(&fake, format!("#!/bin/sh\necho \"$*\" >> '{}'\nexit 1\n", log.display())).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (fake, log)
}

/// (#316) A utility binding on this machine's default engine reached on
/// loopback stays on this machine, like LM Studio: its prompt carries the
/// operator's identity.
#[test]
#[serial_test::serial]
fn a_utility_job_on_the_loopback_default_engine_carries_identity() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions").body_contains("PIN-UTIL-IDENTITY");
        then.status(200).header("content-type", "application/json").json_body(serde_json::json!({
            "id": "mlx-1", "object": "chat.completion", "created": 0, "model": "mlx-util",
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": "routed" }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7 },
        }));
    });
    let registry_dir = tempfile::tempdir().unwrap();
    let profiles_path = registry_dir.path().join("profiles.json");
    let body = serde_json::json!({
        "default_profile": "work",
        "profiles": { "work": { "models": [ { "id": "mock-worker", "n_ctx": 8192 } ] } },
        "endpoints": { "mlx-local": { "url": format!("{}/v1", server.base_url()) } },
        "internal": { "utility": { "id": "mlx-util", "n_ctx": 32768 } }
    });
    std::fs::write(&profiles_path, serde_json::to_string_pretty(&body).unwrap()).unwrap();
    let state = isolated_state();
    std::fs::write(state.join("identity.md"), "PIN-UTIL-IDENTITY").unwrap();
    let prev = std::env::var("DARKMUX_DEFAULT_ENGINE").ok();
    unsafe { std::env::set_var("DARKMUX_DEFAULT_ENGINE", "mlx-local") };
    let reply = run_utility_single_shot(&UtilityJob {
        role_id: darkmux_crew::loader::RADIO_ROUTER_ROLE_ID,
        message: "review this",
        timeout_seconds: 30,
        max_tokens: 128,
        config_path: Some(profiles_path.to_str().unwrap()),
        base_url_override: None,
    });
    unsafe {
        match prev {
            Some(v) => std::env::set_var("DARKMUX_DEFAULT_ENGINE", v),
            None => std::env::remove_var("DARKMUX_DEFAULT_ENGINE"),
        }
    }
    assert_eq!(reply.expect("the utility call reaches the default engine").content, "routed");
    mock.assert();
}

/// (#3207) A utility binding naming an UNMANAGED endpoint sends the job
/// there: the bare binding id on the wire, the endpoint's dialect
/// (`max_tokens`, no `max_completion_tokens`) and its auth header, and the
/// usage record names that endpoint (its route label and its `endpoints`
/// id). darkmux never runs `lms` for it: nothing to load, eject or read.
/// LM Studio's own address points at a dead port, so a request that went
/// there instead would fail.
#[test]
#[serial_test::serial]
fn a_utility_job_on_an_unmanaged_endpoint_runs_there_with_the_bare_id_and_no_lms() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .header("authorization", "Bearer util-secret")
            .json_body_partial(r#"{ "model": "mlx-util", "max_tokens": 128 }"#)
            // The managed arm's low-variance setting, in the dialect that sends one.
            .body_contains("\"temperature\":0.2")
            .body_contains("# Radio Router")
            // (#316 review) The data boundary: this endpoint is not the
            // loopback default engine, so the operator's identity stays here.
            .matches(|req| !String::from_utf8_lossy(req.body.as_deref().unwrap_or_default()).contains("PIN-UTIL-IDENTITY"));
        then.status(200).header("content-type", "application/json").json_body(serde_json::json!({
            "id": "mlx-1",
            "object": "chat.completion",
            "created": 0,
            "model": "mlx-util",
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": "routed" }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7 },
        }));
    });
    let registry_dir = tempfile::tempdir().unwrap();
    let profiles_path = registry_dir.path().join("profiles.json");
    let body = serde_json::json!({
        "schema_version": "2.1",
        "default_profile": "work",
        "profiles": { "work": { "models": [ { "id": "mock-worker", "n_ctx": 8192 } ] } },
        "endpoints": {
            "mlx-local": {
                "url": format!("{}/v1", server.base_url()),
                "dialect": "chat-completions-max-tokens",
                "auth": { "type": "bearer", "key_env": "DARKMUX_TEST_UTIL_KEY" }
            }
        },
        "internal": { "utility": { "id": "mlx-util", "n_ctx": 32768, "endpoint": "mlx-local" } }
    });
    std::fs::write(&profiles_path, serde_json::to_string_pretty(&body).unwrap()).unwrap();
    let state = isolated_state();
    std::fs::write(state.join("identity.md"), "PIN-UTIL-IDENTITY").unwrap();
    let flows_dir = state.join("flows");
    let bin_dir = tempfile::tempdir().unwrap();
    let (fake_lms, lms_log) = recording_fake_lms(bin_dir.path());

    let prev: Vec<(&str, Option<String>)> = ["DARKMUX_LMSTUDIO_URL", "DARKMUX_LMS_BIN", "DARKMUX_TEST_UTIL_KEY"]
        .into_iter()
        .map(|k| (k, std::env::var(k).ok()))
        .collect();
    unsafe {
        std::env::set_var("DARKMUX_LMSTUDIO_URL", "http://127.0.0.1:9");
        std::env::set_var("DARKMUX_LMS_BIN", &fake_lms);
        std::env::set_var("DARKMUX_TEST_UTIL_KEY", "util-secret");
    }
    let reply = run_utility_single_shot(&UtilityJob {
        role_id: darkmux_crew::loader::RADIO_ROUTER_ROLE_ID,
        message: "review this",
        timeout_seconds: 30,
        max_tokens: 128,
        config_path: Some(profiles_path.to_str().unwrap()),
        base_url_override: None,
    });
    unsafe {
        for (k, v) in prev {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
    let reply = reply.expect("the utility call reaches the unmanaged endpoint");
    assert_eq!(reply.content, "routed");
    mock.assert();
    assert!(!lms_log.exists(), "darkmux ran `lms` for an unmanaged utility binding: {:?}", std::fs::read_to_string(&lms_log));

    let records = all_flow_records(&flows_dir);
    assert_eq!(records.len(), 2, "lean: the start marker and the usage record: {records:#?}");
    assert_eq!(records[0]["action"], "utility.start");
    assert_eq!(records[0]["payload"]["model"], "mlx-util", "the start names the bare id: {:#}", records[0]);
    let rec = &records[1];
    assert_eq!(rec["action"], "telemetry.tokens");
    assert_eq!(rec["payload"]["purpose"], "utility", "{rec}");
    assert_eq!(rec["payload"]["requested_model"], "mlx-util", "{rec}");
    assert_eq!(rec["payload"]["endpoint_id"], "mlx-local", "the record names the endpoint it went through: {rec}");
    let label = rec["payload"]["endpoint"].as_str().unwrap_or_default();
    assert!(label.contains("127.0.0.1") && label.contains("mlx-util"), "the real endpoint, not LM Studio's: {rec}");
    assert!(!label.contains(":9/"), "never LM Studio's address: {rec}");
    assert!(rec["session_id"].is_null(), "{rec}");
}

/// (#3207) A utility binding naming a MANAGED endpoint is today's binding:
/// the residency preflight runs and the namespaced id goes on the wire.
#[test]
#[serial_test::serial]
fn a_utility_binding_on_a_managed_endpoint_keeps_the_lm_studio_path() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions").json_body_partial(r#"{ "model": "darkmux:mock-util" }"#);
        then.status(200).header("content-type", "application/json").json_body(serde_json::json!({
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": "ok" }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
        }));
    });
    let registry_dir = tempfile::tempdir().unwrap();
    let profiles_path = registry_dir.path().join("profiles.json");
    std::fs::write(
        &profiles_path,
        r#"{"schema_version":"2.1","default_profile":"work",
            "profiles":{"work":{"models":[{"id":"mock-worker","n_ctx":8192}]}},
            "endpoints":{"lms":{"managed":"lmstudio"}},
            "internal":{"utility":{"id":"mock-util","n_ctx":4096,"endpoint":"lms"}}}"#,
    )
    .unwrap();
    let _state = isolated_state();
    let bin_dir = tempfile::tempdir().unwrap();
    let fake_lms = bin_dir.path().join("lms");
    std::fs::write(
        &fake_lms,
        "#!/bin/sh\n\
         if [ \"$1\" = \"ps\" ]; then\n\
         echo '[{\"identifier\":\"darkmux:mock-util\",\"modelKey\":\"mock-util\",\"status\":\"loaded\",\"contextLength\":4096}]'\n\
         exit 0\n\
         fi\n\
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
    let reply = run_utility_single_shot(&UtilityJob {
        role_id: darkmux_crew::loader::RADIO_ROUTER_ROLE_ID,
        message: "anything",
        timeout_seconds: 30,
        max_tokens: 64,
        config_path: Some(profiles_path.to_str().unwrap()),
        base_url_override: None,
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
    reply.expect("the managed arm round-trips through the mock LM Studio");
    mock.assert();
}
