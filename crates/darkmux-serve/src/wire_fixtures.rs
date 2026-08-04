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
    use crate::mission_graph::{GraphEdge, GraphNode, MissionGraph, StepRow};
    use crate::runs::{Run, RunKind, RunStatus};
    use std::path::PathBuf;

    fn fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/generated")
    }

    /// Write-or-assert. The whole point is that a shape change is LOUD, so the
    /// default path only ever compares; rewriting takes an explicit env var.
    fn golden(name: &str, value: &impl serde::Serialize) {
        let json = serde_json::to_string_pretty(value).expect("wire type must serialize");
        let dir = fixture_dir();
        let path = dir.join(name);

        if std::env::var("DARKMUX_REGENERATE_FIXTURES").is_ok() {
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

        assert_eq!(
            on_disk.trim(),
            json.trim(),
            "\n\nThe wire shape of `{name}` changed and its generated fixture did not.\n\
             Every Playwright spec built on this fixture is now feeding a shape the server\n\
             no longer produces — which renders as an empty page, not as a failed assertion.\n\n\
             Regenerate: DARKMUX_REGENERATE_FIXTURES=1 cargo test -p darkmux-serve wire_fixtures\n"
        );
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
        };
        golden("runs-row.json", &run);
    }

    /// One `graph.json` snapshot with a phase, a task, and steps in three
    /// different states.
    ///
    /// The step states are not decoration. A planned step must show no tokens
    /// (#1481's phantom-token gate), a running one must show a live clock, and
    /// the local/cloud/unknown attribution (#1626) needs all three of `cloud:
    /// Some(true)`, `local_ok: Some(true)`, and neither — so an exemplar that
    /// only carried a happy step would let a spec build a graph that cannot
    /// exercise the thing it means to test.
    #[test]
    fn mission_graph_wire_shape() {
        let graph = MissionGraph {
            mission_id: "review-1785400940-136e76".to_string(),
            mission_status: "active",
            nodes: vec![
                GraphNode {
                    id: "phase-investigate".to_string(),
                    label: "Investigate".to_string(),
                    kind: "phase",
                    status: "complete",
                    parent_id: None,
                    started_ts: Some(1_785_400_940),
                    completed_ts: Some(1_785_404_428),
                    depth: 0,
                    description: Some("Bundle, probe, dedup".to_string()),
                    steps: vec![],
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
                    kind: "phase",
                    status: "running",
                    parent_id: None,
                    started_ts: Some(1_785_404_428),
                    completed_ts: None,
                    depth: 1,
                    description: None,
                    steps: vec![],
                },
                GraphNode {
                    id: "task-judge".to_string(),
                    label: "Judge".to_string(),
                    kind: "task",
                    status: "running",
                    // The field a hand-written fixture omitted twice, which is
                    // why the page rendered nothing: steps hang off a TASK.
                    parent_id: Some("phase-adjudicate".to_string()),
                    started_ts: Some(1_785_404_428),
                    completed_ts: None,
                    depth: 0,
                    description: None,
                    steps: vec![
                        StepRow {
                            id: "judge-cloud".to_string(),
                            label: "Judge".to_string(),
                            kind: "review.judge".to_string(),
                            status: "complete",
                            started_ts: Some(1_785_404_428),
                            completed_ts: Some(1_785_404_600),
                            tokens_final: Some(5_000),
                            turns_final: Some(1),
                            cloud: Some(true),
                            local_ok: None,
                            model: Some("gpt-oss-120b".to_string()),
                        },
                        StepRow {
                            id: "judge-local".to_string(),
                            label: "Judge".to_string(),
                            kind: "review.judge".to_string(),
                            status: "complete",
                            started_ts: Some(1_785_404_428),
                            completed_ts: Some(1_785_404_610),
                            tokens_final: Some(3_000),
                            turns_final: Some(1),
                            cloud: None,
                            local_ok: Some(true),
                            model: Some("qwen3.6-35b-a3b".to_string()),
                        },
                        StepRow {
                            // Neither flag: an errored hosted seat is
                            // indistinguishable from a local one on `cloud`
                            // alone, and must read as UNKNOWN (#1626).
                            id: "judge-unknown".to_string(),
                            label: "Judge".to_string(),
                            kind: "review.judge".to_string(),
                            status: "error",
                            started_ts: Some(1_785_404_428),
                            completed_ts: None,
                            tokens_final: Some(7_000),
                            turns_final: None,
                            cloud: None,
                            local_ok: None,
                            model: None,
                        },
                        StepRow {
                            // Never started: no tokens, no clock (#1481).
                            id: "verify-planned".to_string(),
                            label: "Verify".to_string(),
                            kind: "review.verify".to_string(),
                            status: "planned",
                            started_ts: None,
                            completed_ts: None,
                            tokens_final: None,
                            turns_final: None,
                            cloud: None,
                            local_ok: None,
                            model: None,
                        },
                    ],
                },
            ],
            edges: vec![GraphEdge {
                id: "phase-investigate->phase-adjudicate".to_string(),
                source: "phase-investigate".to_string(),
                target: "phase-adjudicate".to_string(),
                kind: "phase_order",
            }],
            legacy: false,
            note: None,
            generated_at_ms: 1_785_404_700_000,
        };
        golden("mission-graph.json", &graph);
    }
}
