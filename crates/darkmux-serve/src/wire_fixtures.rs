//! (#1637) Browser-test fixtures GENERATED from the wire types.
//!
//! Two of the six bad tests written on 2026-08-04 came from the same hole:
//! nothing connects the Rust structs that emit `/runs` and `graph.json` to the
//! JSON the Playwright specs hand-feed them. A type-safety gap sitting between
//! two type-safe halves. The graph node shape was hand-written wrong twice in
//! one session, and each time the only symptom was a page that rendered
//! nothing — which reads as "my selector is wrong", not "my fixture is wrong",
//! so the wrong lesson gets learned.
//!
//! These tests serialize an exemplar of each wire type into
//! `tests/fixtures/generated/` and assert the checked-in file still matches.
//! So:
//!
//!   - a spec building on a generated fixture cannot feed a shape the server
//!     could never produce;
//!   - adding, renaming or retyping a field FAILS here until the fixture is
//!     regenerated, at which point every spec sees the new shape;
//!   - the diff in review shows the wire change explicitly, rather than it
//!     landing invisibly because no consumer happened to break.
//!
//! Regenerate with `DARKMUX_REGENERATE_FIXTURES=1 cargo test -p darkmux-serve
//! wire_fixtures`. Deliberately NOT automatic: a wire shape changing silently
//! because a test rewrote its own expectation is the failure this exists to
//! prevent.
//!
//! Golden files rather than a schema crate — no new dependency (the dep set is
//! deliberately small), and a concrete example is more useful to a spec author
//! than a schema is.

#[cfg(test)]
mod tests {
    use crate::mission_graph::{
        EdgeKind, GraphEdge, GraphNode, GraphNodeStatus, MissionGraph, NodeKind, PhaseDisplayStatus, StepRow,
        TaskDisplayStatus,
    };
    use darkmux_crew::types::{MissionStatus, NodeStatus};
    use crate::runs::{Run, RunKind, RunStatus};
    use std::path::PathBuf;

    fn fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/generated")
    }

    /// Write-or-assert. The whole point is that a shape change is LOUD, so the
    /// default path only ever compares; rewriting takes an explicit env var.
    fn golden(name: &str, value: &impl serde::Serialize) {
        let dir = fixture_dir();
        let path = dir.join(name);

        if std::env::var("DARKMUX_REGENERATE_FIXTURES").is_ok() {
            let json = serde_json::to_string_pretty(value).expect("wire type must serialize");
            std::fs::create_dir_all(&dir).expect("creating the fixture dir");
            std::fs::write(&path, format!("{json}\n")).expect("writing the fixture");
            return;
        }

        let on_disk = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "missing generated fixture `{}` ({e}).\n\
                 Regenerate: DARKMUX_REGENERATE_FIXTURES=1 cargo test -p darkmux-serve wire_fixtures",
                path.display()
            )
        });

        // Compared as parsed JSON, not as text: two objects with the same keys
        // and values are the same wire shape whatever order the keys print in.
        let on_disk: serde_json::Value = serde_json::from_str(&on_disk)
            .unwrap_or_else(|e| panic!("generated fixture `{}` is not JSON ({e})", path.display()));
        let served = serde_json::to_value(value).expect("wire type must serialize");
        assert_eq!(
            on_disk,
            served,
            "\n\nThe wire shape of `{name}` changed and its generated fixture did not.\n\
             Every Playwright spec built on this fixture is now feeding a shape the server\n\
             no longer produces — which renders as an empty page, not as a failed assertion.\n\n\
             Regenerate: DARKMUX_REGENERATE_FIXTURES=1 cargo test -p darkmux-serve wire_fixtures\n"
        );
    }

    /// (4.0) A pre-4.0 mission archive, as `/flow-mission/<id>` serves it.
    ///
    /// The day file holds raw 3.x records: step sessions spelled
    /// `step-<id>` and, from FLOW 1.43.0, `step-<id>-<mission>`, with no
    /// `payload.step_id` on the records that carry a step's tokens and turns.
    /// The viewer never parses a session id, so the daemon names each step as
    /// `payload.step_id` on the way out. `mission-lens-legacy-archive.spec.js`
    /// feeds this served body to the lens and asserts the steps still show
    /// their tokens and turns, so the pair pins the archive end to end.
    #[tokio::test]
    async fn flow_mission_legacy_archive_wire_shape() {
        let served: serde_json::Value = serde_json::from_slice(&serve_legacy_archive().await).unwrap();
        golden("flow-mission-legacy-archive.json", &served["records"]);
    }

    /// The daemon serves an archived record's keys in the order the archive
    /// holds them, in every build of this crate. `serde_json` sorts object
    /// keys unless its `preserve_order` feature is on, and a dependency of the
    /// binary turns it on, so before the workspace declared it once the key
    /// order on the wire depended on which crates were linked.
    #[tokio::test]
    async fn served_records_keep_the_archives_key_order() {
        let body = String::from_utf8(serve_legacy_archive().await).unwrap();
        let first = &body[body.find("\"records\"").expect("the body carries records")..];
        let ts = first.find("\"ts\"").unwrap();
        let level = first.find("\"level\"").unwrap();
        let action = first.find("\"action\"").unwrap();
        assert!(ts < level && level < action, "keys left the archive's order: {first}");
    }

    /// `/flow-mission/<id>` over a one-day archive of raw 3.x records, as
    /// the served bytes.
    async fn serve_legacy_archive() -> Vec<u8> {
        use tower::ServiceExt;
        let m = "review-1785400940-legacy";
        let day = [
            serde_json::json!({ "ts": "2026-08-20T09:00:00Z", "level": "info", "category": "work", "tier": "local", "stage": "dispatch", "action": "dispatch.start", "handle": "judge", "session_id": "step-judge-1", "mission_id": m, "source": "crew_dispatch", "payload": {} }),
            serde_json::json!({ "ts": "2026-08-20T09:00:05Z", "level": "info", "category": "work", "tier": "local", "stage": "dispatch", "action": "dispatch.turn", "handle": "judge", "session_id": "step-judge-1", "mission_id": m, "source": "crew_dispatch", "payload": { "turn_seq": 4, "turns_so_far": 4 } }),
            serde_json::json!({ "ts": "2026-08-20T09:00:06Z", "level": "info", "category": "telemetry", "tier": "local", "stage": "dispatch", "action": "telemetry.tokens", "handle": "judge", "session_id": "step-judge-1", "mission_id": m, "source": "tokens", "payload": { "total_tokens": 5000 } }),
            serde_json::json!({ "ts": "2026-08-20T09:00:07Z", "level": "info", "category": "work", "tier": "local", "stage": "dispatch", "action": "dispatch.complete", "handle": "judge", "session_id": "step-judge-1", "mission_id": m, "source": "crew_dispatch" }),
            serde_json::json!({ "ts": "2026-08-20T09:01:00Z", "level": "info", "category": "work", "tier": "local", "stage": "dispatch", "action": "dispatch.start", "handle": "verifier", "session_id": format!("step-verify-1-{m}"), "mission_id": m, "source": "crew_dispatch", "payload": {} }),
            serde_json::json!({ "ts": "2026-08-20T09:01:04Z", "level": "info", "category": "work", "tier": "local", "stage": "dispatch", "action": "dispatch.turn", "handle": "verifier", "session_id": format!("step-verify-1-{m}"), "mission_id": m, "source": "crew_dispatch", "payload": { "turn_seq": 2, "turns_so_far": 2 } }),
            serde_json::json!({ "ts": "2026-08-20T09:01:05Z", "level": "info", "category": "telemetry", "tier": "local", "stage": "dispatch", "action": "telemetry.tokens", "handle": "verifier", "session_id": format!("step-verify-1-{m}"), "mission_id": m, "source": "tokens", "payload": { "total_tokens": 18000 } }),
            serde_json::json!({ "ts": "2026-08-20T09:01:06Z", "level": "info", "category": "work", "tier": "local", "stage": "dispatch", "action": "dispatch.complete", "handle": "verifier", "session_id": format!("step-verify-1-{m}"), "mission_id": m, "source": "crew_dispatch" }),
        ];
        let flows = tempfile::TempDir::new().unwrap();
        let body: String = day.iter().map(|r| format!("{r}\n")).collect();
        std::fs::write(flows.path().join("2026-08-20.jsonl"), body).unwrap();
        let response = crate::build_router_local(flows.path().to_path_buf())
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/flow-mission/{m}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap().to_vec()
    }

    /// A `/runs` row with every optional field POPULATED.
    ///
    /// Populated rather than minimal on purpose: a spec author copying this
    /// sees the full vocabulary available to them. The DEGRADED shapes (fields
    /// absent, unknown values) stay hand-written in `runs-degraded-fixture.json`
    /// — those are deliberately things the server would not emit, so generating
    /// them from the type would be a contradiction.
    #[test]
    fn runs_row_wire_shape() {
        let run = Run {
            id: "review-1785400940-136e76".to_string(),
            kind: RunKind::Mission,
            status: RunStatus::Running,
            machine: Some("macbook-pro".to_string()),
            route: Some("https://example.cognitiveservices.azure.com".to_string()),
            role: Some("pr-reviewer".to_string()),
            model: Some("gpt-oss-120b".to_string()),
            started_ts: Some(1_785_400_940),
            completed_ts: None,
            updated_ts: Some(1_785_401_200),
            tracked: true,
            // (#1915) Populated even though this exemplar is `tracked:
            // true` (which opens via `#mission=<id>`, not this field) —
            // "every optional field populated" is the whole point of this
            // fixture, and a spec author copying it should see the full
            // wire vocabulary, not infer that `session_id` only ever
            // shows up on an untracked row.
            dispatch_id: Some("crew-dispatch-pr-reviewer-1785400940-136e76-0".to_string()),
            // (#1907) This exemplar's `status` is `Running`, so `None` here
            // is the honest value — `abandoned_reason` is only ever `Some`
            // alongside `RunStatus::Abandoned` (see that field's own doc).
            // The degraded-shape sibling `runs-degraded-fixture.json` (hand-
            // written, not generated from this type) already covers an
            // `abandoned` row with the field entirely ABSENT, which is the
            // more useful exemplar for this field: it proves an old server
            // response with no `abandoned_reason` at all still deserializes
            // leniently, the same "lenient-on-read WIRE shape" this module's
            // own header doc names for every other optional field.
            abandoned_reason: None,
            // (#2902 step 2b) Populated, like every other optional field here.
            tokens: Some(48_120),
            // Lab rows only (a mission has no workload or verify); the
            // lab shape is covered by the `/runs` lab-row tests.
            workload: None,
            verify_passed: None,
        };
        golden("runs-row.json", &run);
    }

    /// One `graph.json` snapshot with a phase, a task, and steps in three
    /// different states.
    ///
    /// The step states are not decoration. A planned step must show no tokens
    /// (#1481's phantom-token gate) and a running one must show a live clock,
    /// so an exemplar that only carried a happy step would let a spec build a
    /// graph that cannot exercise the thing it means to test. (#2902 step 2a:
    /// the `cloud`/`localOk` fields are gone from the wire.)
    #[test]
    fn mission_graph_wire_shape() {
        let graph = MissionGraph {
            mission_id: "review-1785400940-136e76".to_string(),
            mission_status: MissionStatus::Active,
            nodes: vec![
                GraphNode {
                    id: "phase-investigate".to_string(),
                    label: "Investigate".to_string(),
                    kind: NodeKind::Phase,
                    status: GraphNodeStatus::Phase(PhaseDisplayStatus::Complete),
                    parent_id: None,
                    started_ts: Some(1_785_400_940),
                    completed_ts: Some(1_785_404_428),
                    depth: 0,
                    description: Some("Bundle, probe, dedup".to_string()),
                    steps: None,
                    status_note: None,
                },
                GraphNode {
                    // (#1637) The adjudicate phase MUST exist: the contract
                    // spec caught the first draft naming it as a parent
                    // without including it, and the page rendered one node
                    // instead of two. The server never emits a task whose
                    // parent phase is absent, so an exemplar that did was
                    // teaching specs a shape that cannot occur — precisely
                    // the failure this file exists to prevent, in the file
                    // itself.
                    id: "phase-adjudicate".to_string(),
                    label: "Adjudicate".to_string(),
                    kind: NodeKind::Phase,
                    status: GraphNodeStatus::Phase(PhaseDisplayStatus::Running),
                    parent_id: None,
                    started_ts: Some(1_785_404_428),
                    completed_ts: None,
                    depth: 1,
                    description: None,
                    steps: None,
                    status_note: None,
                },
                GraphNode {
                    id: "task-judge".to_string(),
                    label: "Judge".to_string(),
                    kind: NodeKind::Task,
                    status: GraphNodeStatus::Task(TaskDisplayStatus::Running),
                    // The field a hand-written fixture omitted twice, which is
                    // why the page rendered nothing: steps hang off a TASK.
                    parent_id: Some("phase-adjudicate".to_string()),
                    started_ts: Some(1_785_404_428),
                    completed_ts: None,
                    depth: 0,
                    description: None,
                    steps: Some(vec![
                        StepRow {
                            id: "judge-cloud".to_string(),
                            label: "Judge".to_string(),
                            kind: "dispatch.map".to_string(),
                            status: NodeStatus::Complete,
                            started_ts: Some(1_785_404_428),
                            completed_ts: Some(1_785_404_600),
                            tokens_final: Some(5_000),
                            turns_final: Some(1),
                            model: Some("gpt-oss-120b".to_string()),
                        },
                        StepRow {
                            id: "judge-local".to_string(),
                            label: "Judge".to_string(),
                            kind: "dispatch.map".to_string(),
                            status: NodeStatus::Complete,
                            started_ts: Some(1_785_404_428),
                            completed_ts: Some(1_785_404_610),
                            tokens_final: Some(3_000),
                            turns_final: Some(1),
                            model: Some("qwen3.6-35b-a3b".to_string()),
                        },
                        StepRow {
                            // Neither flag: an errored hosted seat is
                            // indistinguishable from a local one on `cloud`
                            // alone, and must read as UNKNOWN (#1626).
                            id: "judge-unknown".to_string(),
                            label: "Judge".to_string(),
                            kind: "dispatch.map".to_string(),
                            status: NodeStatus::Error,
                            started_ts: Some(1_785_404_428),
                            completed_ts: None,
                            tokens_final: Some(7_000),
                            turns_final: None,
                            model: None,
                        },
                        StepRow {
                            // Never started: no tokens, no clock (#1481).
                            id: "verify-planned".to_string(),
                            label: "Verify".to_string(),
                            kind: "dispatch.map".to_string(),
                            status: NodeStatus::Planned,
                            started_ts: None,
                            completed_ts: None,
                            tokens_final: None,
                            turns_final: None,
                            model: None,
                        },
                    ]),
                    status_note: None,
                },
            ],
            edges: vec![GraphEdge {
                id: "phase-adjudicate->task-judge".to_string(),
                source: "phase-adjudicate".to_string(),
                target: "task-judge".to_string(),
                kind: EdgeKind::Contains,
            }],
            legacy: false,
            note: None,
            generated_at_ms: 1_785_404_700_000,
        };
        golden("mission-graph.json", &graph);
    }
}
