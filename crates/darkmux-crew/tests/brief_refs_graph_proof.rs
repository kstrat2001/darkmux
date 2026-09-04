//! (#2295 review, CRITICAL 1) The GRAPH path's own proof: a `dispatch.internal`
//! step whose `config.brief_refs` was written by hand — no CLI anywhere — must
//! reach the model with the blocks in its brief.
//!
//! This is the case the first cut of #2295 got wrong and no test caught: the
//! CLI appended the blocks and the step config only carried the keys, so a
//! MISSION graph that set `brief_refs` (the thing the issue exists for) got a
//! read-only attachment mount and a provenance stamp on the flow record, with
//! no block in the brief and no refusal for a key addressing nothing. Every
//! test at the time went through the CLI, which appended before the graph ever
//! ran, so all of them stayed green.
//!
//! Runs against a REAL in-process HTTP mock (`httpmock`, a real local TCP port
//! reached over the same hardened `curl` path every hosted single-shot call
//! uses) via a tool-less role on an ENDPOINT profile — the light single-shot
//! path. Zero LMStudio, zero Docker, zero GPU, zero real AI. Same shape and
//! same reasoning as `mock_single_shot_proof.rs`; see that file's module doc.

use httpmock::prelude::*;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

use darkmux_crew::step_kinds::{DispatchInternalStepKind, StepKind};
use darkmux_crew::types::{NodeStatus, Step, Task};

/// One endpoint profile pointing at the mock server, so a tool-less role takes
/// the container-free hosted single-shot path.
fn write_endpoint_profiles(dir: &Path, base_url: &str) -> std::path::PathBuf {
    let path = dir.join("profiles.json");
    let body = serde_json::json!({
        "schema_version": "1.5",
        "default_profile": "stub",
        "profiles": {
            "stub": {
                "models": [
                    { "id": "stub-model", "n_ctx": 8000, "endpoint": { "url": base_url } }
                ]
            }
        }
    });
    std::fs::write(&path, serde_json::to_string_pretty(&body).unwrap())
        .expect("writing temp profiles.json");
    path
}

fn write_finding(root: &Path, dispatch: &str, seq: u64, marker: &str) {
    let dir = root.join(dispatch).join(seq.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("finding.json"),
        serde_json::json!({
            "key": format!("{dispatch}/{seq}"), "dispatch": dispatch, "seq": seq,
            "ts": "2026-09-04T00:00:00Z", "tool_name": "create_finding",
            "proposer": {"handle": "crawler", "model": "m"},
            "context": {"unit": "u7"},
            "emitted": {"why": marker},
            "schema_version": "1"
        })
        .to_string(),
    )
    .unwrap();
}

fn write_mod(root: &Path, key: &str, kit: &str, attachment: Option<&str>) {
    let dir = root.join(key);
    std::fs::create_dir_all(&dir).unwrap();
    let attachments: Vec<&str> = attachment.into_iter().collect();
    if let Some(name) = attachment {
        std::fs::create_dir_all(dir.join("attachments")).unwrap();
        std::fs::write(dir.join("attachments").join(name), b"body").unwrap();
    }
    std::fs::write(
        dir.join("mod.json"),
        serde_json::json!({
            "key": key, "ts": "2026-09-04T00:00:00Z", "by": "sonnet",
            "for": ["sess-graph/1"],
            "kit": kit,
            "kit_looks_json": false,
            "attachments": attachments,
            "context": {"findings": []},
            "schema_version": "1"
        })
        .to_string(),
    )
    .unwrap();
}

fn step_with(config: Value) -> Step {
    Step {
        id: "step-graph-0001".to_string(),
        task_id: "task-graph-001".to_string(),
        gate: None,
        kind: "dispatch.internal".to_string(),
        status: NodeStatus::Planned,
        config,
        started_ts: None,
        completed_ts: None,
        output: None,
    }
}

fn empty_task() -> Task {
    Task {
        id: "task-graph-001".to_string(),
        phase_id: "phase-graph".to_string(),
        description: "graph proof".to_string(),
        display_name: None,
        step_ids: Vec::new(),
        depends_on: Vec::new(),
        reads: Vec::new(),
        role_id: None,
        profile_name: None,
        workdir: None,
        image: None,
    }
}

struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    fn set(pairs: &[(&'static str, &Path)]) -> Self {
        let mut saved = Vec::new();
        for (k, v) in pairs {
            saved.push((*k, std::env::var(k).ok()));
            // SAFETY: same pattern as `mock_single_shot_proof.rs` — each
            // `tests/*.rs` file is its own process and the two tests in this
            // one are serialized by the guard's own restore.
            unsafe { std::env::set_var(k, v) };
        }
        EnvGuard { saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
    }
}

/// The whole point: config-set refs, no CLI, and the blocks are IN the brief
/// the model was actually sent — read back off the `dispatch start` record.
#[test]
#[serial_test::serial]
fn a_step_config_that_names_records_gets_their_blocks_in_the_brief() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        // Any chat-completions path: the hosted dialect composes its own
        // suffix onto the profile's base URL.
        when.method(POST);
        then.status(200).header("content-type", "application/json").json_body(serde_json::json!({
            "id": "mock-1", "object": "chat.completion", "created": 0, "model": "stub-model",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "ok" },
                "finish_reason": "stop",
            }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 5, "total_tokens": 10 },
        }));
    });

    let registry = tempfile::tempdir().unwrap();
    let flows = tempfile::tempdir().unwrap();
    let findings = tempfile::tempdir().unwrap();
    let mods = tempfile::tempdir().unwrap();
    let profiles = write_endpoint_profiles(registry.path(), &server.base_url());
    write_finding(findings.path(), "sess-graph", 1, "MARKER-graph-observation");
    write_mod(mods.path(), "mod-graph-1", "MARKER-graph-kit", Some("g.patch"));

    let _env = EnvGuard::set(&[
        ("DARKMUX_FLOWS_DIR", flows.path()),
        ("DARKMUX_FINDINGS_DIR", findings.path()),
        ("DARKMUX_MODS_DIR", mods.path()),
    ]);

    let session_id = format!("brief-refs-graph-{}", std::process::id());
    let step = step_with(serde_json::json!({
        "role_id": "review-judge",
        "message": "the graph's own message",
        "session_id": session_id,
        "skip_preflight": true,
        "json": false,
        "profile_name": "stub",
        "config_path": profiles.to_string_lossy(),
        // Set BY HAND, the way a mission config's step would — this is the
        // producer the CLI-driven tests never exercised.
        "brief_refs": [
            {"kind": "finding", "key": "sess-graph/1"},
            {"kind": "mod", "key": "mod-graph-1"},
        ],
    }));

    DispatchInternalStepKind
        .run(&step, &empty_task(), &BTreeMap::new())
        .expect("the step must run against the mock endpoint");
    mock.assert();

    let mut start: Option<Value> = None;
    for entry in std::fs::read_dir(flows.path()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        for line in std::fs::read_to_string(&path).unwrap().lines() {
            let Ok(rec) = serde_json::from_str::<Value>(line) else { continue };
            if rec["action"] == "dispatch start" && rec["session_id"] == session_id.as_str() {
                start = Some(rec);
            }
        }
    }
    let start = start.expect("a `dispatch start` flow record for this step");
    let prompt = start["payload"]["prompt"].as_str().unwrap_or_default();

    assert!(prompt.starts_with("the graph's own message"), "{start}");
    assert!(
        prompt.contains("MARKER-graph-observation"),
        "the finding block must be in the brief the GRAPH produced: {start}"
    );
    assert!(
        prompt.contains("MARKER-graph-kit"),
        "and the mod's kit, byte-exact: {start}"
    );
    assert!(
        prompt.contains("/darkmux-mods/mod-graph-1/attachments/g.patch"),
        "named by the container path the mount uses: {start}"
    );
    assert_eq!(
        start["payload"]["brief_refs"],
        serde_json::json!([
            {"kind": "finding", "key": "sess-graph/1"},
            {"kind": "mod", "key": "mod-graph-1"},
        ]),
        "and the provenance stamp still names both: {start}"
    );
}

/// The refusal reaches the graph path too, and it lands before anything is
/// spawned: the step fails, and the message names the kind and the key.
#[test]
#[serial_test::serial]
fn a_step_config_naming_a_missing_record_fails_the_step_before_any_container_work() {
    let flows = tempfile::tempdir().unwrap();
    let findings = tempfile::tempdir().unwrap();
    let mods = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set(&[
        ("DARKMUX_FLOWS_DIR", flows.path()),
        ("DARKMUX_FINDINGS_DIR", findings.path()),
        ("DARKMUX_MODS_DIR", mods.path()),
    ]);

    // No profiles file and no endpoint: if resolution did NOT refuse first,
    // the failure would be about a model or a container, not about a mod. The
    // DISTINCT error text is the proof nothing downstream was reached.
    let step = step_with(serde_json::json!({
        "role_id": "review-judge",
        "message": "hi",
        "session_id": "brief-refs-graph-missing",
        "skip_preflight": true,
        "brief_refs": [{"kind": "mod", "key": "mod-nope-1"}],
    }));

    let err = DispatchInternalStepKind
        .run(&step, &empty_task(), &BTreeMap::new())
        .expect_err("a step naming a record that is not stored must fail");
    let text = format!("{err:#}");
    assert!(text.contains("no mod mod-nope-1"), "{text}");
    assert!(text.contains("step `step-graph-0001`"), "the step names itself: {text}");
    let lower = text.to_lowercase();
    assert!(
        !lower.contains("docker") && !lower.contains("container"),
        "nothing downstream of resolution was reached: {text}"
    );

    // And nothing was dispatched: no start record for this session at all.
    for entry in std::fs::read_dir(flows.path()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            !body.contains("brief-refs-graph-missing"),
            "a refused step must write no dispatch record: {body}"
        );
    }
}

/// A step minted from a mission config carries its phase on the TASK, not in
/// its own config. The generic dispatch kind must stamp that phase on the
/// dispatch it issues, or the run's records never join the mission: no drill
/// link from the mission view, "Events · 0" in the sheet, no token
/// attribution (operator screenshot 2026-09-04, the grown follow-on steps).
#[test]
#[serial_test::serial] // env-scoped flows dir, like its neighbors
fn a_step_whose_task_names_the_phase_stamps_it_on_the_dispatch_record() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST);
        then.status(200).header("content-type", "application/json").json_body(serde_json::json!({
            "id": "mock-2", "object": "chat.completion", "created": 0, "model": "stub-model",
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": "ok" }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 5, "total_tokens": 10 },
        }));
    });
    let registry = tempfile::tempdir().unwrap();
    let flows = tempfile::tempdir().unwrap();
    let profiles = write_endpoint_profiles(registry.path(), &server.base_url());
    let _env = EnvGuard::set(&[("DARKMUX_FLOWS_DIR", flows.path())]);
    let session_id = format!("phase-stamp-graph-{}", std::process::id());
    let step = step_with(serde_json::json!({
        "role_id": "review-judge",
        "message": "phase stamp",
        "session_id": session_id,
        "skip_preflight": true,
        "json": false,
        "profile_name": "stub",
        "config_path": profiles.to_string_lossy(),
    }));
    // No `phase_id` in the step config: the task is the only source.
    DispatchInternalStepKind.run(&step, &empty_task(), &BTreeMap::new()).expect("runs against the mock");
    mock.assert();
    let mut start: Option<Value> = None;
    let mut seen: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(flows.path()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") { continue; }
        for line in std::fs::read_to_string(&path).unwrap().lines() {
            let Ok(rec) = serde_json::from_str::<Value>(line) else { continue };
            seen.push(format!("{} {} phase={}", rec["action"], rec["session_id"], rec["phase_id"]));
            if rec["action"] == "dispatch start" && rec["session_id"] == session_id.as_str() { start = Some(rec); }
        }
    }
    let start = start.unwrap_or_else(|| panic!("a `dispatch start` flow record for this step; saw: {seen:?}"));
    assert_eq!(start["phase_id"], "phase-graph", "the task's phase must ride the dispatch record: {start}");
}
