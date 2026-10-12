//! A realistic fleet work stream, for the fleet-window tests and the memory
//! and latency measurements of #3217. Shared by `src/lib_tests.rs` (through
//! `#[path]`) and `tests/fleet_window_memory.rs`, so the equality tests and the
//! measurements read the same shape.
//!
//! The shape follows real records (field names and sizes, no content): the
//! sixteen top-level fields a dispatch-grain record carries, and each
//! action's payload keys. Missions of three role executions, each a
//! `dispatch.start` with its prompt, then tool calls with their results,
//! reasoning, heartbeats, turns, usage, context, compaction and step
//! records, then a terminal. About 2.2 KB of record text on average, the
//! hub's measured mean (22.2 MB over 10,000 entries). Every
//! payload a `/runs` reader acts on is present with a value that changes the
//! row: `beats`, `endpoint`, `stop_reason`, `wait_ms`, `later_step_planned`, a
//! degraded `run.complete`, a `mission.start` `config_id`, token counts.
//!
//! Timestamps are relative to `now_secs` (the caller passes the real clock),
//! so the default 14-day scan window always holds every record.

#![allow(dead_code)]

/// `YYYY-MM-DDTHH:MM:SSZ` for an epoch second (the flow schema's spelling).
pub fn iso(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Deterministic filler text of `len` bytes, varied by `seed` so no two
/// records share a payload.
fn text(seed: usize, len: usize) -> String {
    let base =
        format!("line {seed}: the agent read the file, ran the tests, and noted what failed. ");
    base.repeat(len / base.len() + 1)[..len].to_string()
}

/// `n` work-stream records, oldest first, the newest `now_secs - 1`.
pub fn realistic_records(n: usize, now_secs: u64) -> Vec<serde_json::Value> {
    use serde_json::json;
    let mut out = Vec::with_capacity(n);
    // One record every 15 seconds: 10,000 records span about 42 hours.
    let start = now_secs.saturating_sub(15 * n as u64 + 1);
    let mut mission = 0usize;
    while out.len() < n {
        let mid = format!("mis-{mission:04}-review-the-fleet-window-change");
        let machine = if mission % 3 == 0 {
            ("example-laptop", "00000000-0000-4000-8000-ABCDEF000001")
        } else {
            ("example-studio", "00000000-0000-4000-8000-ABCDEF000002")
        };
        let mut recs: Vec<serde_json::Value> = Vec::new();
        // The sixteen top-level fields a dispatch-grain record carries.
        let base = |action: &str,
                    category: &str,
                    source: &str,
                    sid: &str,
                    exec: Option<&str>,
                    payload: serde_json::Value| {
            let mut r = json!({
                "ts": "", "level": "info", "category": category, "tier": "routine", "stage": "dispatch",
                "action": action, "handle": "coder", "phase_id": format!("phase-{mission:04}-implement-and-verify"),
                "session_id": sid, "source": source, "model": "qwen3-coder-30b",
                "mission_id": mid, "machine_id": machine.0, "machine_uid": machine.1, "payload": payload,
            });
            if let Some(e) = exec {
                r.as_object_mut()
                    .unwrap()
                    .insert("execution_id".into(), json!(e));
            }
            r
        };
        let run_sid = format!("run-{mission:04}-fleet-window");
        recs.push(base("mission.start", "work", "mission_lifecycle", &run_sid, None, json!({ "config_id": if mission % 4 == 3 { "dispatch".to_string() } else { format!("cfg-{}", mission % 4) } })));
        recs.push(base(
            "run.start",
            "work",
            "mission_lifecycle",
            &run_sid,
            None,
            json!({ "beats": true }),
        ));
        for k in 0..3 {
            let sid = format!("task-{mission:04}-{k}-implement-the-incremental-reader-and-tests");
            let exec = format!("exec-{mission:x}{k:x}a1b2c3-{k:x}-{mission:x}");
            let endpoint = if k == 1 {
                "azure:myorg.cognitiveservices.azure.com/gpt-4o"
            } else {
                "lmstudio:127.0.0.1:1234/qwen3-coder"
            };
            let step_id = format!("step-{mission:04}-{k}-implement-and-verify-the-change");
            let ctx = |seed: usize| json!({ "workspace": "/work/darkmux", "source": "mission", "sha": format!("{seed:040x}"), "rule": "verify-the-change", "rules": ["verify-the-change"], "confirm": "always", "unit": "turn", "model": "qwen3-coder-30b", "locality": "local", "profile": "coder-long-context" });
            recs.push(base(
                "dispatch.start",
                "work",
                "crew_dispatch",
                &sid,
                Some(&exec),
                json!({
                    "image": "darkmux-runtime:5.1.1-arm64", "tools_requested": ["read", "edit", "bash", "grep"], "brief_refs": [],
                    "prompt_chars": 4_000, "prompt": text(mission * 10 + k, 4_000), "system_chars": 2_400, "workspace": "/work/darkmux",
                    "turn_delay_ms": 0, "beats": mission % 2 == 0, "endpoint": endpoint,
                    "bounds": { "max_tokens_per_call": { "value": null, "source": "default" }, "inactivity_timeout_seconds": { "value": 600, "source": "config" }, "max_turns": { "value": null, "source": "default" } },
                }),
            ));
            for i in 0..30 {
                let seed = mission * 1_000 + k * 100 + i;
                let mut r = match i % 10 {
                    0 | 4 | 7 => base(
                        "dispatch.tool",
                        "work",
                        "crew_dispatch",
                        &sid,
                        Some(&exec),
                        json!({
                            "tool_seq": i, "tool_calls_so_far": i, "tool_name": "read", "args": text(seed, 40), "args_chars": 40,
                            "emitted": null, "emit_seq": null, "result_chars": 2_600, "result": text(seed, 2_600), "ok": true,
                            "outcome": "ok", "exit_code": null, "failure_reason": null, "step_id": step_id,
                        }),
                    ),
                    1 => base(
                        "dispatch.reasoning",
                        "work",
                        "crew_dispatch",
                        &sid,
                        Some(&exec),
                        json!({
                            "turn_seq": i, "reasoning_chars": 3_000, "reasoning_text": text(seed, 3_000), "reasoning_format": "think_tags", "step_id": step_id,
                        }),
                    ),
                    2 | 5 => base(
                        "dispatch.turn.heartbeat",
                        "work",
                        "crew_dispatch",
                        &sid,
                        Some(&exec),
                        json!({
                            "turn_seq": i, "cumulative_chars": seed, "sampled_at_ms": 1_791_000_000_000u64 + seed as u64, "generated_chars": 512, "prompt_chars": 9_000, "step_id": step_id,
                        }),
                    ),
                    3 => base(
                        "dispatch.turn",
                        "work",
                        "crew_dispatch",
                        &sid,
                        Some(&exec),
                        json!({
                            "turn_seq": i, "finish_reason": "tool_calls", "tool_calls_count": 1,
                            "usage": { "prompt_tokens": 900 + i, "completion_tokens": 40 + i, "total_tokens": 940 + 2 * i, "reasoning_tokens": null, "cached_tokens": 600 },
                            "turns_so_far": i, "generation_ms": 2_100, "tool_paths": [null], "tool_names": ["read"], "step_id": step_id,
                        }),
                    ),
                    6 => base(
                        "telemetry.tokens",
                        "telemetry",
                        "tokens",
                        &sid,
                        Some(&exec),
                        json!({
                            "call_kind": "turn", "purpose": "work", "requested_model": "qwen3-coder-30b", "endpoint": endpoint,
                            "reported_model": "qwen3-coder-30b", "endpoint_id": format!("ep-{k}"), "token_source": "provider",
                            "prompt_tokens": 1_000 + seed % 97, "completion_tokens": 50 + i, "total_tokens": 1_050 + seed % 97 + i,
                            "cached_tokens": 600, "turn_seq": i, "step_id": step_id,
                        }),
                    ),
                    8 => base(
                        "telemetry.context",
                        "telemetry",
                        "context",
                        &sid,
                        Some(&exec),
                        json!({ "used": 21_000 + i, "max": 131_072, "threshold": 98_304, "step_id": step_id }),
                    ),
                    _ if i == 9 => base(
                        "budget.wait",
                        "telemetry",
                        "budget",
                        &sid,
                        Some(&exec),
                        json!({ "scope": "endpoint", "message": "waiting for the endpoint's window", "wait_ms": 1_500, "endpoint_id": format!("ep-{k}") }),
                    ),
                    _ if i == 19 => base(
                        "step.complete",
                        "work",
                        "scheduler",
                        &sid,
                        Some(&exec),
                        json!({ "later_step_planned": true }),
                    ),
                    _ => base(
                        "dispatch.compaction",
                        "work",
                        "crew_dispatch",
                        &sid,
                        Some(&exec),
                        json!({
                            "generation": i, "before_messages": 40, "after_messages": 12, "summary_chars": 1_800,
                            "compactor_model": "darkmux:util-4b", "parent_model": "qwen3-coder-30b", "step_id": step_id, "context": ctx(seed),
                        }),
                    ),
                };
                if i == 0 {
                    // The first read of an execution is a whole file.
                    r["payload"]["result"] = json!(text(seed, 20_000));
                }
                recs.push(r);
            }
            // Every fifth mission's middle execution is stopped by the operator.
            let end = json!({
                "wall_ms": 610_000, "rest_ms": 0, "rests": 0, "paced_rest_ms": 0, "turn_delay_effective_ms": 0, "stdout_chars": 12_000,
                "stderr_chars": 0, "exit_code": 0, "result_class": "ok", "total_turns": 30, "total_tools": 9, "tool_calls_failed": 0,
                "tool_calls_invalid_name": 0, "tool_calls_ungranted": 0, "total_compactions": 1, "endpoint": endpoint,
                "live": { "enabled": true, "cadence_ms": 2_000, "samples_sent": 300 },
            });
            if mission % 5 == 4 && k == 1 {
                let mut err = end.clone();
                err["stop_reason"] = json!("interrupt");
                err["result_class"] = json!("error");
                recs.push(base(
                    "dispatch.error",
                    "work",
                    "crew_dispatch",
                    &sid,
                    Some(&exec),
                    err,
                ));
            } else {
                recs.push(base(
                    "dispatch.complete",
                    "work",
                    "crew_dispatch",
                    &sid,
                    Some(&exec),
                    end,
                ));
            }
        }
        // A usage record written under a spelling this binary does not know:
        // the usage fold still counts it by its category and source.
        if mission % 5 == 0 {
            // flow-action-guard:allow — a deliberately unknown action: the usage fold counts it by category and source
            recs.push(base("telemetry.usage_legacy", "telemetry", "tokens", &run_sid, None, json!({ "prompt_tokens": 7_000, "completion_tokens": 300, "total_tokens": 7_300, "endpoint": "lmstudio:127.0.0.1:1234/qwen3-coder" })));
        }
        let status = if mission % 3 == 1 {
            "Degraded"
        } else {
            "Complete"
        };
        recs.push(base(
            "run.complete",
            "work",
            "mission_lifecycle",
            &run_sid,
            None,
            json!({ "result_class": "ok", "status": status }),
        ));
        recs.push(base(
            "mission.close",
            "work",
            "mission_lifecycle",
            &run_sid,
            None,
            json!({ "outcome": "done" }),
        ));
        // An ad-hoc dispatch beside every other mission (no mission: its own
        // row), ending three ways a payload decides: stopped by the operator,
        // a step that says a later one is planned (so not an ending), done.
        if mission % 2 == 0 {
            let sid = format!("adhoc-{mission:04}-dispatch");
            let exec = format!("exec-adhoc-{mission:x}");
            let mut adhoc = vec![base(
                "dispatch.start",
                "work",
                "crew_dispatch",
                &sid,
                Some(&exec),
                json!({ "beats": false, "endpoint": "lmstudio:127.0.0.1:1234/qwen3-coder" }),
            )];
            adhoc.push(match mission % 6 {
                0 => base(
                    "dispatch.error",
                    "work",
                    "crew_dispatch",
                    &sid,
                    Some(&exec),
                    json!({ "result_class": "error", "stop_reason": "interrupt" }),
                ),
                2 => base(
                    "step.complete",
                    "work",
                    "scheduler",
                    &sid,
                    Some(&exec),
                    json!({ "later_step_planned": true }),
                ),
                _ => base(
                    "dispatch.complete",
                    "work",
                    "crew_dispatch",
                    &sid,
                    Some(&exec),
                    json!({ "result_class": "ok" }),
                ),
            });
            for mut r in adhoc {
                r.as_object_mut().unwrap().shift_remove("mission_id");
                recs.push(r);
            }
        }
        for r in recs {
            if out.len() == n {
                break;
            }
            out.push(r);
        }
        mission += 1;
    }
    for (i, r) in out.iter_mut().enumerate() {
        r["ts"] = serde_json::json!(iso(start + 15 * i as u64));
    }
    // The last two: an ad-hoc dispatch that went quiet half an hour ago,
    // past the staleness budget but inside the hour-long budget wait it
    // announced, so it still reads as running.
    if n >= 2 {
        let quiet = iso(now_secs.saturating_sub(1_800));
        let common = json!({ "ts": quiet, "level": "info", "category": "work", "tier": "routine", "stage": "dispatch", "handle": "coder",
            "session_id": "adhoc-waiting-dispatch", "execution_id": "exec-adhoc-waiting", "source": "crew_dispatch", "model": "qwen3-coder-30b",
            "machine_id": "example-studio", "machine_uid": "00000000-0000-4000-8000-ABCDEF000002" });
        let with = |action: &str, payload: serde_json::Value| {
            let mut r = common.clone();
            r["action"] = json!(action);
            r["payload"] = payload;
            r
        };
        out[n - 2] = with("dispatch.start", json!({ "beats": false }));
        out[n - 1] = with(
            "budget.wait",
            json!({ "scope": "endpoint", "message": "waiting", "wait_ms": 3_600_000 }),
        );
    }
    out
}
