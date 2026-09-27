use super::*;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize, JsonSchema)]
#[allow(dead_code)]
struct Inner {
    port: Option<u16>,
    host: Option<String>,
    #[serde(flatten)]
    #[schemars(skip)]
    extras: serde_json::Map<String, Value>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(dead_code)]
enum Shape {
    Circle { radius: f64 },
    Square { side: f64 },
}

#[derive(Deserialize, JsonSchema)]
#[allow(dead_code)]
struct Probe {
    #[serde(default)]
    redis: Option<Inner>,
    #[serde(default)]
    list: Vec<Inner>,
    #[serde(default)]
    by_name: BTreeMap<String, Inner>,
    #[serde(default)]
    free: Option<serde_json::Map<String, Value>>,
    #[serde(default)]
    anything: Option<Value>,
    #[serde(default)]
    shape: Option<Shape>,
    #[serde(rename = "type", default)]
    ty: Option<String>,
    #[serde(flatten)]
    #[schemars(skip)]
    extras: serde_json::Map<String, Value>,
}

fn paths(keys: &[UnknownKey]) -> Vec<&str> {
    keys.iter().map(|k| k.path.as_str()).collect()
}

fn closest_of(k: &UnknownKey) -> Option<&str> {
    match &k.hint {
        KeyHint::Closest { closest, .. } => closest.as_deref(),
        KeyHint::Retired(_) => None,
    }
}

#[test]
fn closest_names_the_nearest_candidate_and_the_first_of_a_tie() {
    assert_eq!(closest("hsot", ["host", "port"]), Some("host"));
    assert_eq!(closest("xy", ["ab", "cd"]), Some("ab"), "a tie keeps the first");
    assert_eq!(closest("anything", std::iter::empty()), None);
    assert_eq!(edit_distance("kitten", "sitting"), 3);
}

#[test]
fn known_keys_at_every_level_pass() {
    let doc = json!({
        "redis": {"port": 1, "host": "h"},
        "list": [{"port": 2}],
        "by_name": {"any-name": {"host": "x"}},
        "free": {"whatever": 1, "nested": {"also": true}},
        "anything": {"x": {"y": 1}},
        "shape": {"kind": "circle", "radius": 1.0},
        "type": "t",
    });
    assert_eq!(unknown_keys::<Probe>(&doc, &no_retired), vec![]);
}

#[test]
fn an_unknown_top_level_key_names_the_closest_valid_key() {
    let keys = unknown_keys::<Probe>(&json!({"rediss": {}}), &no_retired);
    assert_eq!(paths(&keys), ["rediss"]);
    assert_eq!(closest_of(&keys[0]), Some("redis"));
    let msg = keys[0].to_string();
    assert!(msg.contains("unknown key `rediss`") && msg.contains("did you mean `redis`?"), "{msg}");
    assert!(msg.contains("valid keys here: anything, by_name, free, list, redis, shape, type"), "{msg}");
}

#[test]
fn an_unknown_nested_key_names_its_full_path_and_the_closest_sibling() {
    let keys = unknown_keys::<Probe>(&json!({"redis": {"hots": "h"}}), &no_retired);
    assert_eq!(paths(&keys), ["redis.hots"]);
    assert_eq!(closest_of(&keys[0]), Some("redis.host"));
}

#[test]
fn keys_inside_arrays_maps_and_enum_variants_are_checked() {
    let doc = json!({
        "list": [{"port": 1}, {"prot": 2}],
        "by_name": {"a": {"hst": "x"}},
        "shape": {"kind": "square", "sid": 2.0},
    });
    let keys = unknown_keys::<Probe>(&doc, &no_retired);
    assert_eq!(paths(&keys), ["by_name.a.hst", "list[1].prot", "shape.sid"]);
    assert_eq!(closest_of(&keys[1]), Some("list[1].port"));
    assert_eq!(closest_of(&keys[2]), Some("shape.side"));
}

#[test]
fn a_comment_key_is_valid_at_every_level() {
    let doc = json!({"_comment": "why", "redis": {"_comment": "and here"}, "list": [{"_comment": 1}]});
    assert_eq!(unknown_keys::<Probe>(&doc, &no_retired), vec![]);
}

#[test]
fn free_maps_and_untyped_values_take_any_key() {
    let doc = json!({"free": {"a": 1}, "anything": {"deep": {"er": 1}}});
    assert_eq!(unknown_keys::<Probe>(&doc, &no_retired), vec![]);
}

#[test]
fn a_retired_key_names_its_replacement_instead_of_a_guess() {
    let retired = |p: &str| (p == "list.old").then(|| "renamed to `list.port`".to_string());
    let keys = unknown_keys::<Probe>(&json!({"list": [{"old": 1}]}), &retired);
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].hint, KeyHint::Retired("renamed to `list.port`".into()));
    assert_eq!(keys[0].to_string(), "unknown key `list[0].old`: renamed to `list.port`");
}

#[test]
fn a_retired_key_under_a_map_is_looked_up_with_the_map_key_as_a_wildcard() {
    let retired = |p: &str| (p == "by_name.*.old").then(|| "removed".to_string());
    let keys = unknown_keys::<Probe>(&json!({"by_name": {"any": {"old": 1}}}), &retired);
    assert_eq!(keys[0].to_string(), "unknown key `by_name.any.old`: removed");
}

/// The key set is DERIVED from the type, never listed. This struct exists
/// only here, with a field name that appears nowhere else in darkmux, so a
/// hand-maintained key list could not know it: the gate passes it because
/// the schema is generated from the type.
#[test]
fn a_new_field_is_valid_without_being_listed_anywhere() {
    #[derive(Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct Fresh {
        zanzibar_quokka_ratio: Option<u8>,
        #[serde(default)]
        nested_quokka: Option<Inner>,
    }
    let doc = json!({"zanzibar_quokka_ratio": 3, "nested_quokka": {"port": 1}});
    assert_eq!(unknown_keys::<Fresh>(&doc, &no_retired), vec![]);
    let keys = unknown_keys::<Fresh>(&json!({"zanzibar_quokka_rati": 3}), &no_retired);
    assert_eq!(closest_of(&keys[0]), Some("zanzibar_quokka_ratio"));
}

#[test]
fn not_json_is_a_problem_and_a_clean_file_is_none() {
    let p = Path::new("x.json");
    let bad = check_text::<Probe>(UserFileKind::Role, p, "{\"redis\": ", &no_retired).unwrap();
    assert!(matches!(bad.problem, Problem::NotJson(_)), "{bad:?}");
    assert!(bad.to_string().starts_with("role manifest x.json: not valid JSON"), "{bad}");
    assert_eq!(check_text::<Probe>(UserFileKind::Role, p, "{\"redis\": {}}", &no_retired), None);
}

#[test]
fn a_file_problem_names_the_kind_the_file_and_every_key() {
    let p = Path::new("/tmp/r.json");
    let fp = check_text::<Probe>(UserFileKind::Role, p, r#"{"rediss": 1, "redis": {"hots": 1}}"#, &no_retired).unwrap();
    let msg = fp.to_string();
    assert!(msg.starts_with("role manifest /tmp/r.json: "), "{msg}");
    assert!(msg.contains("`rediss`: did you mean `redis`?") && msg.contains("`redis.hots`: did you mean `redis.host`?"), "{msg}");
}

#[test]
fn check_dir_reads_every_json_file_and_skips_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.json"), r#"{"redis": {}}"#).unwrap();
    std::fs::write(dir.path().join("b.json"), r#"{"lst": []}"#).unwrap();
    std::fs::write(dir.path().join("c.md"), r#"{"lst": []}"#).unwrap();
    let problems = check_dir::<Probe>(UserFileKind::Workload, dir.path(), &no_retired);
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].path, dir.path().join("b.json"));
    assert_eq!(check_dir::<Probe>(UserFileKind::Workload, &dir.path().join("absent"), &no_retired), vec![]);
}

// ── config.json ──

fn config_keys(doc: Value) -> Vec<UnknownKey> {
    let text = doc.to_string();
    match config_problem(&text) {
        Some(FileProblem { problem: Problem::UnknownKeys(k), .. }) => k,
        other => panic!("expected unknown keys, got {other:?}"),
    }
}

fn config_problem(text: &str) -> Option<FileProblem> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(&path, text).unwrap();
    config_json_problem_at(&path)
}

#[test]
fn config_unknown_top_level_key_names_the_closest() {
    let keys = config_keys(json!({"machine_idd": "m"}));
    assert_eq!(paths(&keys), ["machine_idd"]);
    assert_eq!(closest_of(&keys[0]), Some("machine_id"));
}

#[test]
fn config_unknown_nested_key_names_the_closest() {
    let keys = config_keys(json!({"runtime": {"thermal": {"pase_at": "serious"}}}));
    assert_eq!(paths(&keys), ["runtime.thermal.pase_at"]);
    assert_eq!(closest_of(&keys[0]), Some("runtime.thermal.pause_at"));
    let keys = config_keys(json!({"redis": {"hots": "127.0.0.1"}}));
    assert_eq!(closest_of(&keys[0]), Some("redis.host"));
}

#[test]
fn config_near_miss_in_a_list_item_names_the_closest() {
    let keys = config_keys(json!({"hooks": {"rules": [{"file": "/tmp/x", "matc": {}}]}}));
    assert_eq!(paths(&keys), ["hooks.rules[0].matc"]);
    assert_eq!(closest_of(&keys[0]), Some("hooks.rules[0].match"));
}

#[test]
fn config_retired_keys_name_their_replacement() {
    let keys = config_keys(json!({"remote": {"max_tokens_per_execution": 5}, "dirs": {"notebook": "/x"}}));
    let msgs: Vec<String> = keys.iter().map(ToString::to_string).collect();
    assert!(msgs.iter().any(|m| m.contains("`remote.max_tokens_per_execution`: renamed to `remote.max_tokens_per_step`")), "{msgs:?}");
    assert!(msgs.iter().any(|m| m.contains("`dirs.notebook`: removed in 4.0")), "{msgs:?}");
}

#[test]
fn config_hook_match_payload_keys_and_role_profiles_are_free() {
    let doc = json!({
        "hooks": {"rules": [{"file": "/tmp/x", "match": {"action": "a", "tool": "create_finding", "ok": true}}]},
        "role_profiles": {"any-role": "p"},
    });
    assert_eq!(config_problem(&doc.to_string()), None);
}

/// What `darkmux init` writes must never be refused.
#[test]
fn the_init_written_config_has_no_unknown_keys() {
    let init = serde_json::to_string(&crate::config::DarkmuxConfig::with_defaults()).unwrap();
    assert_eq!(config_problem(&init), None);
}

#[test]
fn config_syntax_error_is_reported_not_json() {
    let p = config_problem("{\"redis\": ").unwrap();
    assert!(matches!(p.problem, Problem::NotJson(_)), "{p:?}");
    assert_eq!(config_json_problem_at(Path::new("/nonexistent/darkmux/config.json")), None);
}

/// Every object in a user file's schema that accepts keys it does not name
/// is listed here, with why. A struct that forgets `#[schemars(skip)]` on
/// its `extras` overflow shows up as a new entry and fails this test,
/// instead of silently accepting every typo.
#[test]
fn open_objects_are_declared() {
    // HookMatch: its extra keys are payload fields matched against records.
    assert_eq!(open_objects::<crate::config::DarkmuxConfig>(), ["HookMatch"]);
    assert_eq!(open_objects::<crate::ProfileRegistry>(), Vec::<String>::new());
}

#[test]
fn every_kind_has_a_label_and_the_scope_table_is_what_the_docs_say() {
    for k in UserFileKind::ALL {
        assert!(!k.label().is_empty());
    }
    assert_eq!(UserFileKind::Config.scopes(), &Scope::ALL);
    assert!(UserFileKind::Role.scopes().contains(&Scope::Dispatch));
    assert!(!UserFileKind::Workload.scopes().contains(&Scope::MissionLaunch));
}

/// Every entry point's preflight refuses a `config.json` with an unknown
/// key (every scope consumes `config.json`), naming the key and the closest.
#[test]
#[serial_test::serial]
fn every_preflight_refuses_an_unknown_config_key() {
    let cfg: crate::config::DarkmuxConfig = serde_json::from_str(r#"{"redis": {"hots": "h"}}"#).unwrap();
    let _guard = crate::config_access::set_config_for_test(cfg);
    for scope in Scope::ALL {
        let refusal = crate::config_enum::preflight(scope).expect_err("refused").to_string();
        assert!(refusal.contains("unknown key `redis.hots`: did you mean `redis.host`?"), "{scope:?}: {refusal}");
    }
    drop(_guard);
    for scope in Scope::ALL {
        assert_eq!(crate::config_enum::preflight(scope), Ok(()), "a clean config passes ({scope:?})");
    }
}

/// A test build never reads the developer's own darkmux state, so no test
/// here passes or fails on what `~/.darkmux` holds.
#[test]
fn a_test_build_skips_the_operators_own_state() {
    let home = dirs::home_dir().unwrap();
    assert!(is_operator_state(&home.join(".darkmux/config.json")));
    assert!(is_operator_state(&home.join(".config/darkmux/profiles.json")));
    assert!(!is_operator_state(&home.join("elsewhere/config.json")));
    assert_eq!(check_path::<Probe>(UserFileKind::Config, &home.join(".darkmux/config.json"), &no_retired), None);
}
