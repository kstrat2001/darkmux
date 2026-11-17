//! (#3569) Tests for [`super`]: planning an add from what detection found,
//! the text edit and its checks, and the one write.

use super::*;
use crate::engine::detect::{
    judge_flint, judge_tensorfold, DetectedModel, FlintFacts, TensorfoldFacts,
};
use crate::engine::flint::Pulled;
use crate::engine::tensorfold::Version;

/// A registry laid out the way `profiles.example.json` is, with a field
/// order and spacing an edit must keep.
const REGISTRY: &str = r#"{
  "default_profile": "worker",
  "profiles": {
    "worker": { "description": "the worker", "models": [ { "id": "qwen", "n_ctx": 32768 } ] }
  },
  "endpoints": {
    "api": { "url": "https://api.example.com/v1" }
  }
}
"#;

fn flint(pulled: Vec<Pulled>, endpoints: Vec<String>) -> DetectedEngine {
    judge_flint(
        FlintFacts {
            version: Ok("0.1.0".into()),
            runtime: Ok(()),
            pulled: Ok(pulled),
        },
        endpoints,
    )
}

fn int4() -> Pulled {
    Pulled {
        revision: "flint-int4".into(),
        max_context: Some(262_144),
    }
}

fn tensorfold(models: &[&str]) -> DetectedEngine {
    let facts = TensorfoldFacts {
        version: Ok(Version {
            major: 1,
            minor: 2,
            patch: 5,
        }),
        models: Ok(models.iter().map(|m| m.to_string()).collect()),
    };
    judge_tensorfold(facts, Vec::new())
}

fn ask(engine: DetectedKind, model: Option<&str>, n_ctx: Option<u32>) -> AddRequest {
    AddRequest {
        engine,
        model: model.map(str::to_string),
        n_ctx,
    }
}

fn path() -> &'static Path {
    Path::new("/nowhere/profiles.json")
}

fn all_free(_: u16) -> bool {
    true
}

/// Flint's only revision is added at the context its manifest states: an
/// endpoint `flint` on the first free port and a profile `flint` running
/// it, the rest of the file kept byte for byte, and the result loads with
/// the endpoint read as Flint and the profile on it.
#[test]
fn flints_only_revision_is_added_at_the_context_rec_states() {
    let planned = plan(
        REGISTRY,
        path(),
        &[flint(vec![int4()], Vec::new())],
        &ask(DetectedKind::Flint, None, None),
        &all_free,
    )
    .unwrap();
    assert_eq!(
        (
            planned.endpoint.as_str(),
            planned.port,
            planned.profile.as_str()
        ),
        ("flint", FIRST_PORT, "flint")
    );
    assert_eq!(
        (planned.model.as_str(), planned.n_ctx, planned.n_ctx_from),
        ("flint-int4", 262_144, ContextFrom::EngineStated)
    );
    let doc: serde_json::Value = serde_json::from_str(&planned.text).unwrap();
    assert_eq!(
        doc["endpoints"]["flint"],
        serde_json::json!({"managed": "flint", "port": FIRST_PORT})
    );
    assert_eq!(
        doc["profiles"]["flint"]["models"][0],
        serde_json::json!({"id": "flint-int4", "n_ctx": 262144, "endpoint": "flint"})
    );
    assert_eq!(
        doc["default_profile"], "worker",
        "the default is not changed"
    );
    for kept in [
        r#""worker": { "description": "the worker", "models": [ { "id": "qwen", "n_ctx": 32768 } ] }"#,
        r#""api": { "url": "https://api.example.com/v1" }"#,
    ] {
        assert!(
            planned.text.contains(kept),
            "kept as written: {kept}\n{}",
            planned.text
        );
    }
    let mut reg = crate::profiles::parse_registry_lenient(&planned.text).unwrap();
    reg.materialize_endpoints();
    assert!(reg.quarantined.is_empty(), "{:?}", reg.quarantined);
    assert!(
        planned
            .summary()
            .contains("the largest the engine states for it"),
        "{}",
        planned.summary()
    );
}

/// A named model and context are used as named; a context over what the
/// engine states, or 0, is refused.
#[test]
fn a_named_model_and_context_are_used_and_one_over_the_engines_is_refused() {
    let found = [flint(
        vec![
            int4(),
            Pulled {
                revision: "flint-int8".into(),
                max_context: Some(65_536),
            },
        ],
        Vec::new(),
    )];
    let planned = plan(
        REGISTRY,
        path(),
        &found,
        &ask(DetectedKind::Flint, Some("flint-int8"), Some(32_768)),
        &all_free,
    )
    .unwrap();
    assert_eq!(
        (planned.model.as_str(), planned.n_ctx, planned.n_ctx_from),
        ("flint-int8", 32_768, ContextFrom::Requested)
    );
    let over = plan(
        REGISTRY,
        path(),
        &found,
        &ask(DetectedKind::Flint, Some("flint-int8"), Some(65_537)),
        &all_free,
    )
    .unwrap_err();
    assert!(
        matches!(&over, Refusal::BadRequest(m) if m.contains("at most 65536")),
        "{over:?}"
    );
    let zero = plan(
        REGISTRY,
        path(),
        &found,
        &ask(DetectedKind::Flint, Some("flint-int8"), Some(0)),
        &all_free,
    )
    .unwrap_err();
    assert!(matches!(zero, Refusal::BadRequest(_)));
}

/// darkmux chooses no model and no context: several models with none named,
/// a model the engine does not hold, or an engine that states no context
/// with none named, is refused naming what to send.
#[test]
fn darkmux_chooses_no_model_and_no_context() {
    let two = [flint(
        vec![
            int4(),
            Pulled {
                revision: "flint-int8".into(),
                max_context: None,
            },
        ],
        Vec::new(),
    )];
    let several = plan(
        REGISTRY,
        path(),
        &two,
        &ask(DetectedKind::Flint, None, None),
        &all_free,
    )
    .unwrap_err();
    assert!(
        matches!(&several, Refusal::BadRequest(m) if m.contains("`flint-int4`, `flint-int8`") && m.contains("name the one")),
        "{several:?}"
    );
    let unknown = plan(
        REGISTRY,
        path(),
        &two,
        &ask(DetectedKind::Flint, Some("flint-fp16"), None),
        &all_free,
    )
    .unwrap_err();
    assert!(
        matches!(&unknown, Refusal::BadRequest(m) if m.contains("no model `flint-fp16`")),
        "{unknown:?}"
    );
    let tf = [tensorfold(&["TensorFold/Qwen3.8-27B-MLX-4bit"])];
    let no_ctx = plan(
        REGISTRY,
        path(),
        &tf,
        &ask(DetectedKind::Tensorfold, None, None),
        &all_free,
    )
    .unwrap_err();
    assert!(
        matches!(&no_ctx, Refusal::BadRequest(m) if m.contains("states no context") && m.contains("`n_ctx`")),
        "{no_ctx:?}"
    );
    let named = plan(
        REGISTRY,
        path(),
        &tf,
        &ask(DetectedKind::Tensorfold, None, Some(65_536)),
        &all_free,
    )
    .unwrap();
    assert_eq!(
        (planned_pair(&named), named.n_ctx),
        (("tensorfold", "TensorFold/Qwen3.8-27B-MLX-4bit"), 65_536)
    );
}

fn planned_pair(p: &Planned) -> (&str, &str) {
    (p.endpoint.as_str(), p.model.as_str())
}

/// Only an installed, usable, unconfigured engine darkmux starts servers
/// for is added; LM Studio and the default engine need nothing added.
#[test]
fn only_an_installed_usable_unconfigured_server_engine_is_added() {
    let req = ask(DetectedKind::Flint, None, None);
    let missing = plan(REGISTRY, path(), &[], &req, &all_free).unwrap_err();
    assert!(
        matches!(&missing, Refusal::Unavailable(m) if m.contains("not installed")),
        "{missing:?}"
    );
    let empty = plan(
        REGISTRY,
        path(),
        &[flint(Vec::new(), Vec::new())],
        &req,
        &all_free,
    )
    .unwrap_err();
    assert!(
        matches!(&empty, Refusal::Unavailable(m) if m.contains("rec pull")),
        "{empty:?}"
    );
    // Configured is what the registry being edited declares, whatever
    // detection read before.
    let declares = REGISTRY.replace(
        r#""api": {"#,
        r#""fl": { "managed": "flint", "port": 8092 }, "api": {"#,
    );
    let configured = plan(
        &declares,
        path(),
        &[flint(vec![int4()], Vec::new())],
        &req,
        &all_free,
    )
    .unwrap_err();
    assert!(
        matches!(&configured, Refusal::Configured(m) if m.contains("`fl`")),
        "{configured:?}"
    );
    for kind in [DetectedKind::Lmstudio, DetectedKind::Url] {
        let r = plan(REGISTRY, path(), &[], &ask(kind, None, None), &all_free).unwrap_err();
        assert!(
            matches!(&r, Refusal::BadRequest(m) if m.contains("needs nothing added")),
            "{r:?}"
        );
    }
}

/// The ids and port avoid what is taken: an endpoint or profile named after
/// the engine gets `-2`, and a port an endpoint declares or something
/// listens on is skipped.
#[test]
fn the_ids_and_port_avoid_what_is_taken() {
    let taken = REGISTRY
        .replace(
            r#""api": {"#,
            &format!(r#""flint": {{ "managed": "tensorfold", "port": {FIRST_PORT} }}, "api": {{"#),
        )
        .replace(
            r#""worker": {"#,
            r#""flint": { "models": [] }, "worker": {"#,
        );
    let busy = |p: u16| p != FIRST_PORT + 1;
    let planned = plan(
        &taken,
        path(),
        &[flint(vec![int4()], Vec::new())],
        &ask(DetectedKind::Flint, None, None),
        &busy,
    )
    .unwrap();
    assert_eq!(
        (
            planned.endpoint.as_str(),
            planned.profile.as_str(),
            planned.port
        ),
        ("flint-2", "flint-2", FIRST_PORT + 2)
    );
    let none = plan(
        REGISTRY,
        path(),
        &[flint(vec![int4()], Vec::new())],
        &ask(DetectedKind::Flint, None, None),
        &|_| false,
    )
    .unwrap_err();
    assert!(matches!(none, Refusal::Registry(_)));
    assert_eq!(
        free_name("flint", &["flint".into(), "flint-2".into()]),
        "flint-3"
    );
}

/// A registry the unknown-key check refuses (contract 7) is never edited,
/// and the refusal names the key.
#[test]
fn a_registry_with_an_unknown_key_is_never_edited() {
    let typo = REGISTRY.replace("\"default_profile\"", "\"default_profle\"");
    let refused = plan(
        &typo,
        path(),
        &[flint(vec![int4()], Vec::new())],
        &ask(DetectedKind::Flint, None, None),
        &all_free,
    )
    .unwrap_err();
    assert!(
        matches!(&refused, Refusal::Registry(m) if m.contains("default_profle") && m.contains("fix it before adding")),
        "{refused:?}"
    );
}

/// The text edit: a member added first in an existing map, into an empty
/// map, or as a new map; the rest kept byte for byte; a key that only looks
/// like the map's (inside a string, or nested) is not it.
#[test]
fn a_member_is_added_to_the_top_level_map_and_nothing_else_moves() {
    let v = serde_json::json!({"managed": "flint", "port": 8091});
    let added = insert_member(REGISTRY, "endpoints", "fl", &v).unwrap();
    assert!(
        added.contains(
            "\"endpoints\": {\n    \"fl\": {\"managed\":\"flint\",\"port\":8091},\n    \"api\""
        ),
        "{added}"
    );
    let empty = insert_member(
        r#"{"endpoints": {}, "profiles": {}}"#,
        "endpoints",
        "fl",
        &v,
    )
    .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&empty).unwrap()["endpoints"]["fl"],
        v
    );
    let absent = insert_member(r#"{ "profiles": {} }"#, "endpoints", "fl", &v).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&absent).unwrap()["endpoints"]["fl"],
        v,
        "{absent}"
    );
    let decoy = r#"{"note": "\"endpoints\": {", "profiles": {"p": {"endpoints": {}}}, "endpoints": {"a": 1}}"#;
    let back: serde_json::Value =
        serde_json::from_str(&insert_member(decoy, "endpoints", "fl", &v).unwrap()).unwrap();
    assert_eq!(back["endpoints"]["fl"], v);
    assert_eq!(
        back["profiles"]["p"]["endpoints"],
        serde_json::json!({}),
        "the nested one is untouched"
    );
    assert_eq!(back["note"], "\"endpoints\": {");
    assert!(insert_member(r#"{"endpoints": []}"#, "endpoints", "fl", &v).is_err());
    assert!(insert_member("[]", "endpoints", "fl", &v).is_err());
}

/// The write lands only on the file the plan was made from; a file changed
/// since is left as it is, and nothing is left beside it.
#[test]
fn the_write_lands_only_on_the_file_the_plan_was_read_from() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("profiles.json");
    std::fs::write(&file, REGISTRY).unwrap();
    let planned = plan(
        REGISTRY,
        &file,
        &[flint(vec![int4()], Vec::new())],
        &ask(DetectedKind::Flint, None, None),
        &all_free,
    )
    .unwrap();
    std::fs::write(&file, format!("{REGISTRY} ")).unwrap();
    let changed = write(&file, REGISTRY, &planned).unwrap_err();
    assert!(matches!(changed, Refusal::Changed(_)), "{changed:?}");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        format!("{REGISTRY} "),
        "left as it is"
    );
    std::fs::write(&file, REGISTRY).unwrap();
    write(&file, REGISTRY, &planned).unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), planned.text);
    let mut left: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    left.sort();
    assert_eq!(
        left,
        [
            std::ffi::OsString::from("profiles.json"),
            std::ffi::OsString::from("profiles.json.lock")
        ],
        "nothing beside it but the writers' lock"
    );
    let loaded = crate::profiles::load_registry_quiet(Some(&file.to_string_lossy())).unwrap();
    assert!(loaded.registry.profiles.contains_key("flint"));
}

/// The detected models carry what the engine states, so an app can show
/// the context an add would use before it asks.
#[test]
fn the_context_an_add_would_use_is_the_one_detection_shows() {
    let found = flint(vec![int4()], Vec::new());
    assert_eq!(
        found.models,
        vec![DetectedModel {
            id: "flint-int4".into(),
            max_context: Some(262_144)
        }]
    );
    let planned = plan(
        REGISTRY,
        path(),
        std::slice::from_ref(&found),
        &ask(DetectedKind::Flint, None, None),
        &all_free,
    )
    .unwrap();
    assert_eq!(Some(planned.n_ctx), found.models[0].max_context);
}

/// The checks an edit passes before it is written: a text that does not
/// read back as the registry plus exactly the two entries, or whose
/// endpoint does not classify as the engine, is refused.
#[test]
fn an_edit_that_is_not_exactly_the_two_entries_is_refused() {
    let engine = flint(vec![int4()], Vec::new());
    let entries = Entries::new(
        ManagedBackend::Flint,
        "flint",
        FIRST_PORT,
        "flint",
        "flint-int4",
        8192,
        &engine,
    );
    let original: serde_json::Value = serde_json::from_str(REGISTRY).unwrap();
    let good = entries.insert(REGISTRY).unwrap();
    assert_eq!(entries.verify(&original, &good, path()), Ok(()));
    let extra = good.replace(
        "\"default_profile\": \"worker\"",
        "\"default_profile\": \"flint\"",
    );
    let refused = entries.verify(&original, &extra, path()).unwrap_err();
    assert!(
        matches!(&refused, Refusal::Registry(m) if m.contains("did not read back")),
        "{refused:?}"
    );
    let other = Entries::new(
        ManagedBackend::Tensorfold,
        "flint",
        FIRST_PORT,
        "flint",
        "flint-int4",
        8192,
        &engine,
    );
    let wrong_kind = good.replace(r#"{"managed":"flint""#, r#"{"managed":"tensorfold""#);
    assert_eq!(
        other.classifies(&wrong_kind),
        Ok(()),
        "a tensorfold endpoint whose profile runs on it"
    );
    assert!(entries
        .classifies(&wrong_kind)
        .unwrap_err()
        .contains("would not read as `flint`"));
}

/// (#3569 review) Two adds racing on one file: exactly one lands, the
/// file is always whole JSON holding one add, and the other is told the
/// file changed (an app reads that as `conflict`).
#[test]
fn racing_adds_never_corrupt_the_registry_and_the_loser_is_told() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("profiles.json");
    let found = [flint(vec![int4()], Vec::new())];
    let planned = plan(
        REGISTRY,
        &file,
        &found,
        &ask(DetectedKind::Flint, None, None),
        &all_free,
    )
    .unwrap();
    for round in 0..300 {
        std::fs::write(&file, REGISTRY).unwrap();
        let outcomes: Vec<Result<(), Refusal>> = std::thread::scope(|s| {
            let racers: Vec<_> = (0..2)
                .map(|_| s.spawn(|| write(&file, REGISTRY, &planned)))
                .collect();
            racers.into_iter().map(|r| r.join().unwrap()).collect()
        });
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            serde_json::from_str::<serde_json::Value>(&text).is_ok(),
            "round {round}: corrupt registry:\n{text}"
        );
        assert_eq!(
            outcomes.iter().filter(|o| o.is_ok()).count(),
            1,
            "round {round}: {outcomes:?}"
        );
        assert!(
            outcomes
                .iter()
                .any(|o| matches!(o, Err(Refusal::Changed(_)))),
            "round {round}: {outcomes:?}"
        );
    }
}

/// (#3569 review) `apply` plans and writes under one hold of the lock: of
/// two adds racing from scratch, one lands and the other is refused as
/// configured, which an app reads as `conflict`.
#[test]
fn of_two_racing_applies_the_loser_is_told_it_is_configured() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("profiles.json");
    let found = [flint(vec![int4()], Vec::new())];
    for round in 0..100 {
        std::fs::write(&file, REGISTRY).unwrap();
        let outcomes: Vec<Result<Planned, Refusal>> = std::thread::scope(|s| {
            let racers: Vec<_> = (0..2)
                .map(|_| {
                    s.spawn(|| {
                        apply(
                            &file,
                            &found,
                            &ask(DetectedKind::Flint, None, None),
                            &all_free,
                        )
                    })
                })
                .collect();
            racers.into_iter().map(|r| r.join().unwrap()).collect()
        });
        assert_eq!(
            outcomes.iter().filter(|o| o.is_ok()).count(),
            1,
            "round {round}: {outcomes:?}"
        );
        assert!(
            outcomes
                .iter()
                .any(|o| matches!(o, Err(Refusal::Configured(_)))),
            "round {round}: {outcomes:?}"
        );
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(
            doc["endpoints"].as_object().unwrap().len(),
            2,
            "round {round}: one add, not two"
        );
    }
    let missing = apply(
        &dir.path().join("none.json"),
        &found,
        &ask(DetectedKind::Flint, None, None),
        &all_free,
    )
    .unwrap_err();
    assert!(
        matches!(&missing, Refusal::Registry(m) if m.contains("run `darkmux init`")),
        "{missing:?}"
    );
}

/// (#3569 review) A port a `url` endpoint on this machine's loopback uses
/// is skipped like a declared `port`; a remote one is not.
#[test]
fn a_loopback_url_endpoints_port_is_not_offered() {
    let with_url = REGISTRY.replace(
        r#""api": {"#,
        &format!(r#""mlx": {{ "url": "http://127.0.0.1:{FIRST_PORT}/v1" }}, "far": {{ "url": "http://10.0.0.9:{}/v1" }}, "api": {{"#, FIRST_PORT + 1),
    );
    let planned = plan(
        &with_url,
        path(),
        &[flint(vec![int4()], Vec::new())],
        &ask(DetectedKind::Flint, None, None),
        &all_free,
    )
    .unwrap();
    assert_eq!(planned.port, FIRST_PORT + 1);
}

/// (#3569 review) A manifest that states a context of 0 is refused like a
/// requested 0, never written as `n_ctx: 0`.
#[test]
fn a_stated_context_of_zero_is_refused() {
    let zero = flint(
        vec![Pulled {
            revision: "flint-int4".into(),
            max_context: Some(0),
        }],
        Vec::new(),
    );
    let refused = plan(
        REGISTRY,
        path(),
        &[zero],
        &ask(DetectedKind::Flint, None, None),
        &all_free,
    )
    .unwrap_err();
    assert!(
        matches!(&refused, Refusal::BadRequest(m) if m.contains("context of 0")),
        "{refused:?}"
    );
}

/// (#3569 review) The edit is refused when its profile would not run on
/// the endpoint it adds, though it reads back as written.
#[test]
fn an_edit_whose_profile_runs_elsewhere_is_refused() {
    let engine = flint(vec![int4()], Vec::new());
    let mut entries = Entries::new(
        ManagedBackend::Flint,
        "flint",
        FIRST_PORT,
        "flint",
        "flint-int4",
        8192,
        &engine,
    );
    entries.profile.1["models"][0]["endpoint"] = serde_json::json!("api");
    let original: serde_json::Value = serde_json::from_str(REGISTRY).unwrap();
    let edited = entries.insert(REGISTRY).unwrap();
    let refused = entries.verify(&original, &edited, path()).unwrap_err();
    assert!(
        matches!(&refused, Refusal::Registry(m) if m.contains("would not run on endpoint `flint`")),
        "{refused:?}"
    );
}
