//! The work wire at its frozen version: committed fixtures written by this
//! release's writer, and a shape hash tied to `WORK_JOB_SCHEMA_VERSION`.
//!
//! From this release the wire grows only by a minor bump (a receiver takes
//! the same major with a minor at or below its own), so a later build must
//! keep reading what this one wrote, and a change to the shape without a
//! version change must fail here. Regenerate deliberately with
//! `DARKMUX_REGENERATE_FIXTURES=1 cargo nextest run -p darkmux-fleet -E 'binary(work_wire_fixtures)'`.

use darkmux_fleet::{
    Boundary, CheckReport, EndpointClass, RefusalCode, ReplyStatus, SeatOutlook, SingleShotJob, SubmissionMode,
    SubmissionReply, WorkJob, WorkSubmission, WORK_JOB_SCHEMA_VERSION,
};
use darkmux_types::session_id::{RunId, SessionId};
use serde::Serialize;
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn regenerating() -> bool {
    std::env::var_os("DARKMUX_REGENERATE_FIXTURES").is_some()
}

/// A job carrying every field this version has.
fn full_job() -> WorkJob {
    WorkJob {
        target_machine: "studio".into(),
        target_machine_uid: Some("00000000-0000-4000-8000-ABCDEF000001".into()),
        role_id: "radio-host".into(),
        message: "what is running?".into(),
        session_id: SessionId::adhoc(RunId::standalone("radio").unwrap(), "radio-host", "n1"),
        profile: Some("deep".into()),
        workdir: None,
        image: None,
        timeout_seconds: 300,
        published_at_unix_ms: 1_700_000_000_000,
        published_by_machine: Some("laptop".into()),
        single_shot: Some(SingleShotJob {
            humor: 50,
            surface: darkmux_flow::payload::RadioSurface::Panel,
            max_completion_tokens: 3_000,
        }),
        boundary: Some(Boundary::ManagedOnly),
        mode: SubmissionMode::Run,
    }
}

fn check_job() -> WorkJob {
    WorkJob {
        message: String::new(),
        single_shot: None,
        mode: SubmissionMode::Check,
        ..full_job()
    }
}

fn session_wire() -> Option<SessionId> {
    Some(SessionId::relay(full_job().session_id, "laptop"))
}

fn base_reply(status: ReplyStatus) -> SubmissionReply {
    SubmissionReply {
        machine: Some("studio".into()),
        session_id: session_wire(),
        profile: Some("deep".into()),
        ..SubmissionReply::of(status)
    }
}

/// Every wire value this version writes, by fixture name.
fn writers() -> Vec<(&'static str, serde_json::Value)> {
    fn v(x: &impl Serialize) -> serde_json::Value {
        serde_json::to_value(x).unwrap()
    }
    vec![
        ("submission-run-8.1.json", v(&WorkSubmission::new(full_job(), true))),
        ("submission-check-8.1.json", v(&WorkSubmission::new(check_job(), false))),
        (
            "reply-completed-8.1.json",
            v(&SubmissionReply {
                exit_code: Some(0),
                stdout: Some("the answer".into()),
                stderr: Some(String::new()),
                ..base_reply(ReplyStatus::Completed)
            }),
        ),
        (
            "reply-refused-8.1.json",
            v(&SubmissionReply {
                reason: Some("studio's profile cloud runs on a hosted endpoint".into()),
                refusal: Some(RefusalCode::Boundary),
                session_id: None,
                ..base_reply(ReplyStatus::Refused)
            }),
        ),
        (
            "reply-checked-8.1.json",
            v(&SubmissionReply {
                reason: Some("studio would take this job on profile deep; its seat is free".into()),
                check: Some(CheckReport { endpoint: EndpointClass::Managed, seat: SeatOutlook::Free }),
                session_id: None,
                ..base_reply(ReplyStatus::Checked)
            }),
        ),
        (
            "reply-queued-8.1.json",
            v(&SubmissionReply { reason: Some("studio is busy; the job is queued".into()), ..base_reply(ReplyStatus::Queued) }),
        ),
    ]
}

fn committed(name: &str) -> serde_json::Value {
    let text = std::fs::read_to_string(fixtures_dir().join(name))
        .unwrap_or_else(|e| panic!("missing fixture {name} ({e}): regenerate with DARKMUX_REGENERATE_FIXTURES=1"));
    serde_json::from_str(&text).unwrap()
}

/// The promise: this build's writer still produces the committed fixtures. A
/// change to what the wire says is a change to `WORK_JOB_SCHEMA_VERSION`.
#[test]
fn the_writer_produces_the_committed_8_1_fixtures() {
    for (name, written) in writers() {
        if regenerating() {
            std::fs::create_dir_all(fixtures_dir()).unwrap();
            std::fs::write(fixtures_dir().join(name), format!("{}\n", serde_json::to_string_pretty(&written).unwrap())).unwrap();
            continue;
        }
        assert_eq!(
            committed(name),
            written,
            "the writer no longer produces the committed {name}. A change to the work wire is a change to \
             WORK_JOB_SCHEMA_VERSION (a minor for an added optional field, a major for anything else), then \
             regenerate with DARKMUX_REGENERATE_FIXTURES=1"
        );
    }
}

/// Every later build reads what this one wrote: the submissions through the
/// receiver's own parse (version, shape, validation), the replies through the
/// sender's reader.
#[test]
fn the_committed_8_1_fixtures_still_parse() {
    for name in ["submission-run-8.1.json", "submission-check-8.1.json"] {
        let body = std::fs::read(fixtures_dir().join(name)).unwrap();
        let sub = WorkSubmission::parse(&body).unwrap_or_else(|r| panic!("{name}: {r:?}"));
        assert_eq!(sub.schema, "8.1");
    }
    let run = WorkSubmission::parse(&std::fs::read(fixtures_dir().join("submission-run-8.1.json")).unwrap()).unwrap();
    assert_eq!(run.job, full_job());
    for name in ["reply-completed-8.1.json", "reply-refused-8.1.json", "reply-checked-8.1.json", "reply-queued-8.1.json"] {
        let reply: SubmissionReply = serde_json::from_value(committed(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_ne!(reply.status, ReplyStatus::Unknown, "{name}");
    }
}

/// (#3028) The 8.0 fixtures are frozen, written by the 8.0 release's writer
/// and never regenerated: an 8.1 receiver still takes what an 8.0 sender
/// posted (a job with no `target_machine_uid`), and still reads the 8.0
/// replies.
#[test]
fn the_frozen_8_0_fixtures_are_still_accepted_by_an_8_1_receiver() {
    for name in ["submission-run-8.0.json", "submission-check-8.0.json"] {
        let body = std::fs::read(fixtures_dir().join(name)).unwrap();
        let sub = WorkSubmission::parse(&body).unwrap_or_else(|r| panic!("{name}: {r:?}"));
        assert_eq!(sub.schema, "8.0");
        assert_eq!(sub.job.target_machine_uid, None, "{name}: an 8.0 sender writes no uid");
    }
    let run = WorkSubmission::parse(&std::fs::read(fixtures_dir().join("submission-run-8.0.json")).unwrap()).unwrap();
    assert_eq!(run.job, WorkJob { target_machine_uid: None, ..full_job() });
    for name in ["reply-completed-8.0.json", "reply-refused-8.0.json", "reply-checked-8.0.json", "reply-queued-8.0.json"] {
        let reply: SubmissionReply = serde_json::from_value(committed(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_ne!(reply.status, ReplyStatus::Unknown, "{name}");
    }
}

/// The hash of the JSON Schemas of the submission and the reply, keys sorted.
fn shape_hash() -> String {
    fn canonical(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(map) => {
                let mut entries: Vec<_> = std::mem::take(map).into_iter().collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                for (_, child) in &mut entries {
                    canonical(child);
                }
                map.extend(entries);
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(canonical),
            _ => {}
        }
    }
    let mut schema = serde_json::json!({
        "submission": schemars::schema_for!(WorkSubmission),
        "reply": schemars::schema_for!(SubmissionReply),
    });
    canonical(&mut schema);
    blake3::hash(schema.to_string().as_bytes()).to_hex().to_string()
}

/// The promise: the wire's shape and `WORK_JOB_SCHEMA_VERSION` move together.
/// `tests/fixtures/work-shape.golden` holds one `<version> <hash>` line per
/// released version; the line for the current version must match the shape.
#[test]
fn the_work_wire_shape_is_tied_to_its_schema_version() {
    let path = fixtures_dir().join("work-shape.golden");
    let line = format!("{WORK_JOB_SCHEMA_VERSION} {}", shape_hash());
    if regenerating() {
        let mut lines: Vec<String> = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.starts_with(&format!("{WORK_JOB_SCHEMA_VERSION} ")))
            .map(str::to_string)
            .collect();
        lines.push(line);
        std::fs::create_dir_all(fixtures_dir()).unwrap();
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        return;
    }
    let golden = std::fs::read_to_string(&path).expect("work-shape.golden is missing");
    let recorded = golden.lines().find(|l| l.starts_with(&format!("{WORK_JOB_SCHEMA_VERSION} ")));
    assert_eq!(
        recorded,
        Some(line.as_str()),
        "the work wire's shape and WORK_JOB_SCHEMA_VERSION ({WORK_JOB_SCHEMA_VERSION}) disagree. If the shape \
         changed, bump the version (a minor for an added optional field, a major for anything else) and \
         regenerate with DARKMUX_REGENERATE_FIXTURES=1; a line already committed for a released version is \
         never edited."
    );
}
