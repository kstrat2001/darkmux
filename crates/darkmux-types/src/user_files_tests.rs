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
    schema_version: Option<String>,
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

fn paths(keys: &[KeyIssue]) -> Vec<&str> {
    keys.iter().map(|k| k.path.as_str()).collect()
}

fn closest_of(k: &KeyIssue) -> Option<&str> {
    match &k.issue {
        Issue::Unknown { closest, .. } => closest.as_deref(),
        Issue::Retired(_) | Issue::Removed(_) | Issue::WrongType { .. } | Issue::Missing { .. } | Issue::Rule(_) => None,
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
    assert_eq!(key_issues::<Probe>(&doc, &no_retired), vec![]);
}

#[test]
fn an_unknown_top_level_key_names_the_closest_valid_key() {
    let keys = key_issues::<Probe>(&json!({"rediss": {}}), &no_retired);
    assert_eq!(paths(&keys), ["rediss"]);
    assert_eq!(closest_of(&keys[0]), Some("redis"));
    let msg = keys[0].to_string();
    assert!(msg.contains("unknown key `rediss`") && msg.contains("did you mean `redis`?"), "{msg}");
    assert!(msg.contains("valid keys here: anything, by_name, free, list, redis, schema_version, shape, type"), "{msg}");
}

#[test]
fn an_unknown_nested_key_names_its_full_path_and_the_closest_sibling() {
    let keys = key_issues::<Probe>(&json!({"redis": {"hots": "h"}}), &no_retired);
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
    let keys = key_issues::<Probe>(&doc, &no_retired);
    // Problems come in the order the document holds them. The `kind` tag
    // picks the `square` variant, so its missing `side` is named too, before
    // the typo that was meant for it.
    assert_eq!(paths(&keys), ["list[1].prot", "by_name.a.hst", "shape.side", "shape.sid"]);
    assert_eq!(closest_of(&keys[0]), Some("list[1].port"));
    assert!(matches!(keys[2].issue, Issue::Missing { .. }), "{keys:?}");
    assert_eq!(closest_of(&keys[3]), Some("shape.side"));
}

#[test]
fn a_comment_key_is_valid_at_every_level() {
    let doc = json!({"_comment": "why", "redis": {"_comment": "and here"}, "list": [{"_comment": 1}]});
    assert_eq!(key_issues::<Probe>(&doc, &no_retired), vec![]);
}

#[test]
fn free_maps_and_untyped_values_take_any_key() {
    let doc = json!({"free": {"a": 1}, "anything": {"deep": {"er": 1}}});
    assert_eq!(key_issues::<Probe>(&doc, &no_retired), vec![]);
}

#[test]
fn a_retired_key_names_its_replacement_instead_of_a_guess() {
    let retired = |p: &str| (p == "list.old").then(|| "renamed to `list.port`".to_string());
    let keys = key_issues::<Probe>(&json!({"list": [{"old": 1}]}), &retired);
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].issue, Issue::Retired("renamed to `list.port`".into()));
    assert_eq!(keys[0].to_string(), "unknown key `list[0].old`: renamed to `list.port`");
}

#[test]
fn a_retired_key_under_a_map_is_looked_up_with_the_map_key_as_a_wildcard() {
    let retired = |p: &str| (p == "by_name.*.old").then(|| "removed".to_string());
    let keys = key_issues::<Probe>(&json!({"by_name": {"any": {"old": 1}}}), &retired);
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
    assert_eq!(key_issues::<Fresh>(&doc, &no_retired), vec![]);
    let keys = key_issues::<Fresh>(&json!({"zanzibar_quokka_rati": 3}), &no_retired);
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

fn config_keys(doc: Value) -> Vec<KeyIssue> {
    let text = doc.to_string();
    match config_problem(&text) {
        Some(FileProblem { problem: Problem::Keys(k), .. }) => k,
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

/// (#3036) `dirs.ack` went with the licensed-adjacent acknowledgment gate:
/// a config still carrying it is an unknown key that names the removal (and
/// the release), never a near-miss guess.
#[test]
fn config_dirs_ack_is_named_as_retired() {
    let keys = config_keys(json!({"dirs": {"ack": "/x"}}));
    assert_eq!(keys.len(), 1, "{keys:#?}");
    assert!(matches!(keys[0].issue, Issue::Retired(_)), "{keys:#?}");
    let msg = keys[0].to_string();
    assert!(msg.contains("`dirs.ack`: removed in 5.0"), "{msg}");
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

// ── wrong-type values ──

fn wrong_type(k: &KeyIssue) -> Option<(&str, &str)> {
    match &k.issue {
        Issue::WrongType { expected, got } => Some((expected.as_str(), got.as_str())),
        _ => None,
    }
}

/// One mistyped value fails the whole typed load, which for `config.json`
/// means every setting falls back to its default (Redis and audit off).
/// The gate names it: the path, the expected type, and what it got.
#[test]
fn a_mistyped_config_value_is_named_with_its_expected_type() {
    let text = r#"{"redis": {"enabled": true, "port": "x"}, "audit": {"enabled": true}}"#;
    assert!(serde_json::from_str::<crate::config::DarkmuxConfig>(text).is_err(), "precondition: the typed load fails");
    let keys = config_keys(serde_json::from_str(text).unwrap());
    assert_eq!(paths(&keys), ["redis.port"]);
    assert_eq!(wrong_type(&keys[0]), Some(("an integer from 0 to 65535", "\"x\"")));
    assert_eq!(keys[0].to_string(), "`redis.port` must be an integer from 0 to 65535, got \"x\"");
}

#[test]
fn wrong_types_are_found_at_every_level_and_in_every_json_type() {
    let doc = json!({
        "redis": {"port": 70000, "enabled": "yes"},
        "hooks": {"rules": [{"file": 5}]},
        "role_profiles": {"coder": ["not", "a", "string"]},
        "machine_id": null,
    });
    let keys = config_keys(doc);
    let found: Vec<(&str, Option<(&str, &str)>)> = keys.iter().map(|k| (k.path.as_str(), wrong_type(k))).collect();
    assert_eq!(
        found,
        [
            ("redis.port", Some(("an integer from 0 to 65535", "70000"))),
            ("redis.enabled", Some(("true or false", "\"yes\""))),
            ("hooks.rules[0].file", Some(("a string", "5"))),
            ("role_profiles.coder", Some(("a string", "[\"not\",\"a\",\"string\"]"))),
        ]
    );
}

#[test]
fn enum_variants_free_values_and_null_options_are_accepted_or_named() {
    let ok = json!({"shape": {"kind": "square", "side": 2}, "anything": [1, {"x": null}], "redis": null, "type": null});
    assert_eq!(key_issues::<Probe>(&ok, &no_retired), vec![]);
    let bad = key_issues::<Probe>(&json!({"shape": {"kind": "hexagon"}, "list": {"port": 1}}), &no_retired);
    let found: Vec<(&str, Option<(&str, &str)>)> = bad.iter().map(|k| (k.path.as_str(), wrong_type(k))).collect();
    assert_eq!(found, [("shape.kind", Some(("`circle` or `square`", "\"hexagon\""))), ("list", Some(("a list", "{\"port\":1}")))]);
}

/// A long value is shortened in the message, never printed whole.
#[test]
fn a_long_wrong_value_is_shortened() {
    let long = "y".repeat(200);
    let keys = key_issues::<Probe>(&json!({"type": [long]}), &no_retired);
    let (_, got) = wrong_type(&keys[0]).unwrap();
    assert!(got.chars().count() <= 61 && got.ends_with('…'), "{got}");
}

/// What `darkmux init` writes and a clean config have no wrong types either.
#[test]
fn the_init_written_config_has_no_wrong_types() {
    let init = serde_json::to_value(crate::config::DarkmuxConfig::with_defaults()).unwrap();
    assert_eq!(key_issues::<crate::config::DarkmuxConfig>(&init, &no_retired), vec![]);
}

/// (review C1) `_comment` is a note only where the schema names its keys.
/// Inside a map it is an ENTRY: `accept_work` would read it as a peer named
/// `_comment` and serde fails the whole load (every setting back to its
/// default), so the gate must walk it like any other entry.
#[test]
fn a_comment_key_inside_a_map_is_an_entry_not_a_note() {
    let text = r#"{"fleet": {"accept_work": {"_comment": "why", "peer": {}}}, "redis": {"enabled": true}}"#;
    assert!(serde_json::from_str::<crate::config::DarkmuxConfig>(text).is_err(), "precondition: serde refuses it");
    let keys = config_keys(serde_json::from_str(text).unwrap());
    assert_eq!(paths(&keys), ["fleet.accept_work._comment"]);
    assert!(matches!(keys[0].issue, Issue::WrongType { .. }), "{keys:?}");
    // In a free map (`role_profiles` is string -> string) it is a checked entry too.
    let keys = config_keys(json!({"role_profiles": {"_comment": 5}}));
    assert_eq!(paths(&keys), ["role_profiles._comment"]);
}


/// (review C1) A capability vector's keys are the `Capability` tokens: a
/// misspelled one fails the load, so the gate names it.
#[test]
fn a_misspelled_capability_is_an_unknown_key() {
    let keys = key_issues::<crate::ProfileModel>(&json!({"id": "m", "capabilities": {"code": 1, "reasonin": 1}}), &no_retired);
    assert_eq!(paths(&keys), ["capabilities.reasonin"]);
    assert_eq!(closest_of(&keys[0]), Some("capabilities.reasoning"));
}

// ── (review C2) nothing from the file can forge an output line ──

/// A key is printed through JSON escaping and a length cap: a newline and
/// indentation inside it can never draw a fake line in preflight or doctor.
#[test]
fn a_key_cannot_forge_an_output_line() {
    let forged = format!("\n  config ok{}", "x".repeat(300));
    let keys = key_issues::<Probe>(&json!({ forged.clone(): 1, "redis": { forged: 1 } }), &no_retired);
    assert_eq!(keys.len(), 2);
    for k in &keys {
        let msg = k.to_string();
        assert!(!msg.contains('\n'), "a raw newline reached the message: {msg:?}");
        assert!(msg.contains("\"\\n  config ok"), "the key is shown escaped and quoted: {msg}");
        assert!(k.path.chars().count() < 100, "the key is capped: {}", k.path);
    }
    assert!(keys.iter().any(|k| k.path.starts_with("redis.\"")), "{keys:?}");
}

/// Bidi and other invisible format characters in a value or a key are shown
/// as escapes, never passed to the terminal to reorder the line.
#[test]
fn bidi_and_format_characters_are_escaped() {
    let keys = key_issues::<Probe>(&json!({"type": ["a\u{202e}b\u{200b}"], "ab\u{2066}c": 1}), &no_retired);
    let text: String = keys.iter().map(ToString::to_string).collect();
    for c in ['\u{202e}', '\u{200b}', '\u{2066}'] {
        assert!(!text.contains(c), "{c:?} reached the message: {text:?}");
    }
    assert!(text.contains("\\u{202e}") && text.contains("\\u{2066}"), "{text}");
}

/// A file path is escaped the same way before it is printed.
#[test]
fn a_file_path_cannot_forge_an_output_line() {
    let p = Path::new("/tmp/a\n  config ok.json");
    let fp = check_text::<Probe>(UserFileKind::Role, p, "{\"rediss\": 1}", &no_retired).unwrap();
    assert!(!fp.to_string().contains('\n'), "{:?}", fp.to_string());
}

// ── (review C3) every key an older darkmux wrote is named as retired ──

/// Every `config.json` key a released darkmux ever had and this one does
/// not, from `git log` of `config.rs`: each is named with what replaced it,
/// never a near-miss guess (`orchestrator` used to read "did you mean
/// `remote`?").
#[test]
fn every_historical_config_key_is_named_as_retired() {
    let doc = json!({
        "orchestrator": "claude",
        "gh": {"enabled": true, "allowed": []},
        "review": {"judge_concurrency": 2, "judge_fail_on_any_skip": true},
        "dirs": {"notebook": "/n", "openclaw_config": "/o", "runtime_agents": "/r", "crew": "/c"},
        "radio": {"router_profile": "p"},
        "remote": {"max_tokens_per_execution": 1, "stage_budget_policy": "warn"},
        "runtime": {"telemetry_record_every_samples": 5, "daemon_auth_enabled": true, "log_level": "debug"},
        "machine_rollup": {"enabled": true, "period_seconds": 60},
    });
    let keys = config_keys(doc);
    let not_retired: Vec<String> =
        keys.iter().filter(|k| !matches!(k.issue, Issue::Retired(_))).map(ToString::to_string).collect();
    assert!(not_retired.is_empty(), "{not_retired:#?}");
    assert_eq!(keys.len(), 14, "{keys:#?}");
    let msg: String = keys.iter().map(|k| format!("{k}\n")).collect();
    for says in ["`gh`: renamed to `cmd`", "`orchestrator`: removed", "`remote.stage_budget_policy`: renamed to `remote.step_budget_policy`", "host_sampler_interval_ms", "`runtime.daemon_auth_enabled`: replaced in 4.0 (#2988) by `serve.token_keychain`", "`dirs.crew`: removed in 4.0", "DARKMUX_HOME", "`runtime.log_level`: removed in 5.0", "`machine_rollup`: removed in 5.0"] {
        assert!(msg.contains(says), "{says}: {msg}");
    }
}

/// (#755) `fleet.accept_work.<name>.repos` is a known key, not an unknown
/// one: a config carrying it passes the user-file gate, and a wrong-typed
/// value is reported.
#[test]
fn the_reserved_repos_grant_passes_the_user_file_gate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(&path, r#"{"fleet":{"accept_work":{"m":{"node_id":"n1","repos":["darkmux"]}}}}"#).unwrap();
    assert!(config_json_problem_at(&path).is_none(), "{:?}", config_json_problem_at(&path));
    std::fs::write(&path, r#"{"fleet":{"accept_work":{"m":{"node_id":"n1","repos":"darkmux"}}}}"#).unwrap();
    let shown = format!("{:?}", config_json_problem_at(&path).expect("a string is not a list of repos"));
    assert!(shown.contains("repos"), "{shown}");
}

/// (#2988 review) A wrong-typed value is a reported problem, not a silent
/// drop to defaults: `serve.read_auth` written as a string used to make the
/// lenient loader discard the whole file, turning read auth off.
#[test]
fn a_wrong_typed_config_value_is_a_reported_problem() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(&path, r#"{"serve":{"read_auth":"true"}}"#).unwrap();
    let found = config_json_problem_at(&path).expect("a string read_auth is a problem");
    let shown = format!("{found:?}");
    assert!(shown.contains("read_auth"), "{shown}");
    std::fs::write(&path, r#"{"serve":{"read_auth":true}}"#).unwrap();
    assert!(config_json_problem_at(&path).is_none(), "the same key with the right type is clean");
}

/// `darkmux serve` is an entry point of its own: its preflight refuses the
/// retired auth switch by name, and points at both replacements.
#[test]
#[serial_test::serial]
fn the_serve_preflight_refuses_the_retired_auth_switch_naming_both_replacements() {
    let cfg: crate::config::DarkmuxConfig =
        serde_json::from_str(r#"{"runtime": {"daemon_auth_enabled": true}}"#).unwrap();
    let _guard = crate::config_access::set_config_for_test(cfg);
    let refusal = crate::config_enum::preflight(Scope::Serve).expect_err("refused").to_string();
    for says in ["runtime.daemon_auth_enabled", "serve.token_keychain", "serve.read_auth"] {
        assert!(refusal.contains(says), "{says}: {refusal}");
    }
}

/// The daemon's fleet listener reads `fleet.busy_policy` and
/// `fleet.identity.provider`, so a bad value must refuse `serve` at start,
/// not surface later on the listener path.
#[test]
#[serial_test::serial]
fn the_serve_preflight_refuses_a_bad_fleet_listener_enum() {
    for (json, key) in [
        (r#"{"fleet": {"busy_policy": "zz-bad"}}"#, "fleet.busy_policy"),
        (r#"{"fleet": {"identity": {"provider": "zz-bad"}}}"#, "fleet.identity.provider"),
    ] {
        let cfg: crate::config::DarkmuxConfig = serde_json::from_str(json).unwrap();
        let _guard = crate::config_access::set_config_for_test(cfg);
        let refusal = crate::config_enum::preflight(Scope::Serve)
            .expect_err(&format!("{key}: serve must refuse a bad value"))
            .to_string();
        assert!(refusal.contains(key) && refusal.contains("zz-bad"), "{key}: {refusal}");
    }
}

/// (review minor) A file larger than the cap is not read into memory: it is
/// reported, the same cap the crew loader holds manifests to.
#[test]
fn an_oversized_file_is_reported_not_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.json");
    let f = std::fs::File::create(&path).unwrap();
    f.set_len(MAX_USER_FILE_BYTES + 1).unwrap();
    let found = check_path::<Probe>(UserFileKind::Role, &path, &no_retired).unwrap();
    assert!(matches!(&found.problem, Problem::Unreadable(e) if e.contains("cap")), "{found:?}");
}

// ---- (#3035) version-aware gate ----

#[test]
fn a_newer_file_gets_the_upgrade_message_and_no_key_errors() {
    let p = Path::new("/x/coder.json");
    // Carries a key this binary does not know AND a newer marker: the key is
    // not judged, the file is refused for its version.
    let text = format!(r#"{{"schema_version": "99.0", "future_key": 1}}"#);
    let fp = check_text::<Probe>(UserFileKind::Role, p, &text, &no_retired).unwrap();
    let known = UserFileKind::Role.schema_version();
    assert_eq!(fp.problem, Problem::Newer { file_version: "99.0".into(), known: known.into() });
    let msg = fp.to_string();
    assert!(msg.contains("written by a newer darkmux (role manifest `99.0`"), "{msg}");
    assert!(msg.contains(&format!("this binary reads `{known}`")), "{msg}");
    assert!(msg.contains("Upgrade darkmux."), "{msg}");
    assert!(!msg.contains("future_key") && !msg.contains("unknown key"), "{msg}");
}

#[test]
fn a_same_version_file_with_an_unknown_key_is_still_a_typo() {
    let p = Path::new("/x/coder.json");
    let known = UserFileKind::Role.schema_version();
    let text = format!(r#"{{"schema_version": "{known}", "rediss": 1}}"#);
    let fp = check_text::<Probe>(UserFileKind::Role, p, &text, &no_retired).unwrap();
    let Problem::Keys(keys) = &fp.problem else { panic!("{fp:?}") };
    assert_eq!(paths(keys), vec!["rediss"]);
    // An older marker is no different.
    let fp = check_text::<Probe>(UserFileKind::Role, p, r#"{"schema_version": "0.1", "rediss": 1}"#, &no_retired).unwrap();
    assert!(matches!(fp.problem, Problem::Keys(_)));
}

#[test]
fn every_user_file_kind_refuses_a_newer_marker() {
    for kind in UserFileKind::ALL {
        let fp = check_text::<Probe>(kind, Path::new("/x/f.json"), r#"{"schema_version": "999.0"}"#, &no_retired)
            .unwrap_or_else(|| panic!("{kind:?} accepted a newer file"));
        assert!(matches!(fp.problem, Problem::Newer { .. }), "{kind:?}");
    }
}

#[test]
fn config_json_with_a_newer_marker_is_refused_for_its_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(&path, r#"{"schema_version": "99.0", "from_the_future": true}"#).unwrap();
    let fp = config_json_problem_at(&path).expect("refused");
    assert!(matches!(fp.problem, Problem::Newer { .. }), "{fp}");
}
