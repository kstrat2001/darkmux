//! Tests for `init_engine`: the decision, the endpoint `init` adds, and the
//! addresses it probes. The probe is a fake; `init_tests.rs` drives a real
//! server end to end.

use super::*;
use std::cell::RefCell;

struct FakeProbe {
    lms: bool,
    servers: Vec<(&'static str, Vec<&'static str>)>,
    well_known: Vec<&'static str>,
    asked: RefCell<Vec<String>>,
}

impl FakeProbe {
    fn new(lms: bool) -> Self {
        Self { lms, servers: Vec::new(), well_known: vec![MLX_DEFAULT_URL], asked: RefCell::new(Vec::new()) }
    }
    fn serving(mut self, url: &'static str, models: &[&'static str]) -> Self {
        self.servers.push((url, models.to_vec()));
        self
    }
}

impl Probe for FakeProbe {
    fn lmstudio_answers(&self) -> bool {
        self.lms
    }
    fn served(&self, url: &str) -> Result<Vec<String>> {
        self.asked.borrow_mut().push(url.to_string());
        self.servers
            .iter()
            .find(|(u, _)| same_url(u, url))
            .map(|(_, m)| m.iter().map(|s| s.to_string()).collect())
            .ok_or_else(|| anyhow::anyhow!("connection refused"))
    }
    fn well_known(&self) -> Vec<String> {
        self.well_known.iter().map(|s| s.to_string()).collect()
    }
}

struct Answer(Option<usize>, Vec<Vec<Found>>);

impl Ask for Answer {
    fn choose(&mut self, options: &[Found]) -> Option<usize> {
        self.1.push(options.to_vec());
        self.0
    }
}

const NO_ENDPOINTS: &str = r#"{ "profiles": {} }"#;

fn run(probe: &FakeProbe, registry: &str, explicit: Option<&str>, ask: Option<&mut dyn Ask>) -> Outcome {
    let (answering, probed, err) = detect(probe, registry, explicit);
    decide(answering, probed, explicit, err, ask)
}

fn server(url: &str, models: &[&str]) -> Found {
    Found::Server { id: None, url: url.to_string(), models: models.iter().map(|s| s.to_string()).collect() }
}

#[test]
fn one_answering_engine_is_chosen_without_asking() {
    let probe = FakeProbe::new(false).serving(MLX_DEFAULT_URL, &["m"]);
    let mut ask = Answer(Some(0), Vec::new());
    let out = run(&probe, NO_ENDPOINTS, None, Some(&mut ask));
    assert_eq!(
        out,
        Outcome::Chosen { found: server(MLX_DEFAULT_URL, &["m"]), endpoint: None, added: false, others: vec![], how: How::Only }
    );
    assert!(ask.1.is_empty(), "one engine is never asked about");
}

#[test]
fn several_answering_ask_with_lm_studio_first_and_take_the_answer() {
    let probe = FakeProbe::new(true).serving(MLX_DEFAULT_URL, &["m"]);
    let mut ask = Answer(Some(1), Vec::new());
    let out = run(&probe, NO_ENDPOINTS, None, Some(&mut ask));
    assert_eq!(ask.1, vec![vec![Found::Lmstudio, server(MLX_DEFAULT_URL, &["m"])]]);
    let Outcome::Chosen { found, others, how, .. } = out else { panic!("{out:?}") };
    assert_eq!((found, others, how), (server(MLX_DEFAULT_URL, &["m"]), vec![Found::Lmstudio], How::Asked));
}

#[test]
fn several_answering_with_no_one_to_ask_take_lm_studio_and_name_the_others() {
    let probe = FakeProbe::new(true).serving(MLX_DEFAULT_URL, &["m"]);
    let mut nobody = Answer(None, Vec::new());
    let out = run(&probe, NO_ENDPOINTS, None, Some(&mut nobody));
    let Outcome::Chosen { found, others, how, .. } = &out else { panic!("{out:?}") };
    assert_eq!((found, others, *how), (&Found::Lmstudio, &vec![server(MLX_DEFAULT_URL, &["m"])], How::Default));
    let lines = out.lines(true).join("\n");
    assert!(lines.contains("also answering:") && lines.contains(MLX_DEFAULT_URL), "{lines}");
    // (#316 review) Not "the one answering" when others answered too.
    assert!(lines.contains("no choice was made") && !lines.contains("the one answering"), "{lines}");
}

#[test]
fn several_servers_and_no_lm_studio_with_no_one_to_ask_record_nothing() {
    let probe = FakeProbe::new(false).serving(MLX_DEFAULT_URL, &["m"]).serving("http://127.0.0.1:9000/v1", &["n"]);
    let mut probe = probe;
    probe.well_known.push("http://127.0.0.1:9000/v1");
    let out = run(&probe, NO_ENDPOINTS, None, None);
    assert!(matches!(&out, Outcome::Undecided { answering } if answering.len() == 2), "{out:?}");
    assert!(out.server().is_none());
}

#[test]
fn none_answering_names_every_address_probed() {
    let reg = r#"{ "profiles": {}, "endpoints": { "vllm": { "url": "http://localhost:8000/v1" } } }"#;
    let out = run(&FakeProbe::new(false), reg, None, None);
    let Outcome::NoneAnswering { probed } = &out else { panic!("{out:?}") };
    assert_eq!(probed.len(), 3, "{probed:?}");
    assert!(probed[0].starts_with("LM Studio (`"), "{probed:?}");
    assert_eq!(&probed[1..], &["http://localhost:8000/v1".to_string(), MLX_DEFAULT_URL.to_string()]);
    let lines = out.lines(true).join("\n");
    assert!(lines.contains("http://localhost:8000/v1") && lines.contains("mlx_lm.server --model"), "{lines}");
}

#[test]
fn a_registry_endpoint_on_this_machine_is_probed_once_and_keeps_its_id() {
    let reg = r#"{ "profiles": {}, "endpoints": { "mlx": { "url": "http://127.0.0.1:8080/v1/" } } }"#;
    let probe = FakeProbe::new(false).serving(MLX_DEFAULT_URL, &["m"]);
    let out = run(&probe, reg, None, None);
    assert_eq!(probe.asked.borrow().len(), 1, "the well-known default is the same address: {:?}", probe.asked.borrow());
    let Outcome::Chosen { found: Found::Server { id, .. }, .. } = out else { panic!("{out:?}") };
    assert_eq!(id.as_deref(), Some("mlx"));
}

#[test]
fn remote_hosted_and_managed_endpoints_are_never_probed() {
    let reg = r#"{ "profiles": {}, "endpoints": {
        "cloud": { "url": "https://api.example.com/v1" },
        "anthropic": { "url": "http://127.0.0.1:9999/v1", "dialect": "messages" },
        "lms": { "managed": "lmstudio" },
        "lan": { "url": "http://192.168.1.20:8080/v1" }
    } }"#;
    assert!(local_endpoints(reg).is_empty(), "{:?}", local_endpoints(reg));
    let local = r#"{ "profiles": {}, "endpoints": { "here": { "url": "http://[::1]:8000/v1" } } }"#;
    assert_eq!(local_endpoints(local), vec![("here".to_string(), "http://[::1]:8000/v1".to_string())]);
}

#[test]
fn an_explicit_url_is_the_only_address_probed_and_is_chosen_without_asking() {
    let probe = FakeProbe::new(true).serving("http://10.0.0.5:8080/v1", &["m"]).serving(MLX_DEFAULT_URL, &["x"]);
    let mut ask = Answer(Some(0), Vec::new());
    let out = run(&probe, NO_ENDPOINTS, Some("http://10.0.0.5:8080/v1"), Some(&mut ask));
    assert_eq!(*probe.asked.borrow(), vec!["http://10.0.0.5:8080/v1".to_string()]);
    assert!(ask.1.is_empty());
    assert!(matches!(out, Outcome::Chosen { found: Found::Server { .. }, how: How::Only, .. }), "{out:?}");
}

#[test]
fn an_explicit_url_that_does_not_answer_changes_nothing_and_says_why() {
    let out = run(&FakeProbe::new(true), NO_ENDPOINTS, Some("http://127.0.0.1:8081/v1"), None);
    assert_eq!(
        out,
        Outcome::NotAnswering { url: "http://127.0.0.1:8081/v1".to_string(), error: "connection refused".to_string() }
    );
}

#[test]
fn engine_url_adds_v1_only_to_a_bare_host() {
    assert_eq!(engine_url("http://127.0.0.1:8080").unwrap(), "http://127.0.0.1:8080/v1");
    assert_eq!(engine_url("http://127.0.0.1:8080/").unwrap(), "http://127.0.0.1:8080/v1");
    assert_eq!(engine_url("https://box:9/api/v1/").unwrap(), "https://box:9/api/v1");
    assert!(engine_url("127.0.0.1:8080").is_err());
    assert!(engine_url("ftp://box/v1").is_err());
    assert!(engine_url("http://").is_err());
}

#[test]
fn parse_choice_reads_a_number_in_range_or_the_default() {
    assert_eq!(parse_choice("\n", 2), Some(0));
    assert_eq!(parse_choice(" 2 \n", 2), Some(1));
    assert_eq!(parse_choice("3", 2), None);
    assert_eq!(parse_choice("0", 2), None);
    assert_eq!(parse_choice("mlx", 2), None);
}

#[test]
fn new_endpoint_id_is_named_for_the_port_and_never_reuses_an_id() {
    assert_eq!(new_endpoint_id("http://127.0.0.1:8080/v1", &[]), "local-8080");
    assert_eq!(new_endpoint_id("http://[::1]:8000/v1", &[]), "local-8000");
    assert_eq!(new_endpoint_id("http://localhost/v1", &[]), "local-engine");
    let taken = vec!["local-8080".to_string(), "local-8080-2".to_string()];
    assert_eq!(new_endpoint_id("http://127.0.0.1:8080/v1", &taken), "local-8080-3");
}

#[test]
fn add_endpoint_inserts_into_an_existing_map_and_keeps_the_rest_byte_identical() {
    let reg = "{\n  \"profiles\": {},\n\n  \"endpoints\": {\n    \"cloud\": { \"url\": \"https://x/v1\" }\n  }\n}\n";
    let out = add_endpoint(reg, "local-8080", MLX_DEFAULT_URL).unwrap();
    assert_eq!(
        out,
        "{\n  \"profiles\": {},\n\n  \"endpoints\": {\n    \"local-8080\": { \"url\": \"http://127.0.0.1:8080/v1\", \
         \"dialect\": \"chat-completions-max-tokens\" },\n    \"cloud\": { \"url\": \"https://x/v1\" }\n  }\n}\n"
    );
    let reg: darkmux_types::ProfileRegistry = serde_json::from_str(&out).unwrap();
    assert_eq!(reg.endpoints["local-8080"].resolved_dialect().unwrap(), Dialect::ChatCompletionsMaxTokens);
}

#[test]
fn add_endpoint_fills_an_empty_map_and_creates_a_missing_one() {
    for reg in ["{ \"profiles\": {}, \"endpoints\": {} }", "{\n  \"profiles\": {}\n}\n"] {
        let out = add_endpoint(reg, "local-8080", MLX_DEFAULT_URL).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["endpoints"]["local-8080"]["url"], MLX_DEFAULT_URL, "{out}");
        assert!(v.get("profiles").is_some(), "{out}");
    }
}

#[test]
fn add_endpoint_refuses_text_it_cannot_edit_safely() {
    assert!(add_endpoint("not json", "local-8080", MLX_DEFAULT_URL).is_err());
    // A nested map that happens to be named `endpoints` comes first: the
    // edit would land there, so the read-back refuses it.
    let reg = r#"{ "profiles": { "p": { "endpoints": {} } }, "endpoints": {} }"#;
    let out = add_endpoint(reg, "local-8080", MLX_DEFAULT_URL);
    assert!(out.is_err(), "{out:?}");
}

#[test]
fn plan_record_adds_a_new_server_and_reuses_a_registry_one() {
    let mut fresh = Outcome::Chosen {
        found: server(MLX_DEFAULT_URL, &["m"]),
        endpoint: None,
        added: false,
        others: vec![],
        how: How::Only,
    };
    let text = plan_record(&mut fresh, NO_ENDPOINTS).unwrap().expect("a new endpoint is written");
    assert!(text.contains("\"local-8080\""), "{text}");
    assert!(matches!(&fresh, Outcome::Chosen { endpoint: Some(id), added: true, .. } if id == "local-8080"));

    let mut known = Outcome::Chosen {
        found: Found::Server { id: Some("mlx".into()), url: MLX_DEFAULT_URL.into(), models: vec![] },
        endpoint: None,
        added: false,
        others: vec![],
        how: How::Only,
    };
    assert_eq!(plan_record(&mut known, NO_ENDPOINTS).unwrap(), None);
    assert!(matches!(&known, Outcome::Chosen { endpoint: Some(id), added: false, .. } if id == "mlx"));
}

#[test]
fn name_models_caps_the_list() {
    let many: Vec<String> = (0..10).map(|i| format!("m{i}")).collect();
    assert_eq!(name_models(&many[..2]), "`m0`, `m1`");
    assert!(name_models(&many).ends_with("`m7` and 2 more"), "{}", name_models(&many));
}

#[test]
fn the_undecided_dry_run_says_a_terminal_run_would_ask() {
    let out = Outcome::Undecided { answering: vec![server(MLX_DEFAULT_URL, &["m"]), server("http://127.0.0.1:9/v1", &["n"])] };
    assert!(out.lines(false).join("\n").contains("A run on a terminal asks which."));
    assert!(!out.lines(true).join("\n").contains("asks which"));
}

#[test]
fn the_kept_line_names_where_the_engine_came_from() {
    let out = Outcome::Kept { engine: "lmstudio".into(), from: FROM_ENV };
    assert!(out.lines(true)[0].contains("from DARKMUX_DEFAULT_ENGINE"), "{:?}", out.lines(true));
}

/// (#316 review finding 3) Only a machine where nothing chose an engine yet
/// is detected on: the variable unset, the config at its `lmstudio`
/// default, and the registry still holding the worker placeholder.
#[test]
fn may_detect_only_while_no_engine_or_model_was_chosen() {
    let fresh = format!("{{ \"profiles\": {{ \"p\": {{ \"models\": [{{ \"id\": \"{}\" }}] }} }} }}", crate::init::PLACEHOLDER_MODEL_ID);
    assert!(may_detect(&fresh, "lmstudio", "config.json"));
    assert!(may_detect(&fresh, " LMStudio ", "config.json"));
    assert!(!may_detect(&fresh, "lmstudio", FROM_ENV), "the variable chose it");
    assert!(!may_detect(&fresh, "mlx", "config.json"), "an engine was recorded");
    let tuned = fresh.replace(crate::init::PLACEHOLDER_MODEL_ID, "qwen/qwen3-coder-30b");
    assert!(!may_detect(&tuned, "lmstudio", "config.json"), "a tuned registry is never moved");
}

/// (#316 re-review A) The example's `_comment` names the placeholder; a
/// registry whose slots the operator filled by hand has no placeholder slot,
/// so nothing chose-nothing about it.
#[test]
fn a_hand_filled_example_is_not_detected_on_despite_its_comment() {
    let example = include_str!("../profiles.example.json");
    let filled = example.replace("\"id\": \"<your-worker-model-id>\"", "\"id\": \"operator/tuned-model\"");
    assert!(filled.contains(crate::init::PLACEHOLDER_MODEL_ID), "the comment still names it");
    assert!(may_detect(example, "lmstudio", "config.json"));
    assert!(!may_detect(&filled, "lmstudio", "config.json"));
}

/// (#316 re-review B, C) The remedies name commands that work: `init` alone
/// does not re-detect once a model is set, and `--engine-url` takes no
/// `lmstudio`, so the way back is `config set`.
#[test]
fn the_remedies_name_commands_that_work() {
    let none = Outcome::NoneAnswering { probed: vec![MLX_DEFAULT_URL.into()] }.lines(true).join("\n");
    assert!(none.contains("darkmux init --engine-url <url>") && !none.contains("then re-run `darkmux init`:"), "{none}");
    let kept = Outcome::Kept { engine: "local-8080".into(), from: "config.json" }.lines(true).join("\n");
    assert!(kept.contains("`darkmux config set default_engine lmstudio` goes back to LM Studio"), "{kept}");
    let lms = Outcome::Kept { engine: "lmstudio".into(), from: "config.json" }.lines(true).join("\n");
    assert!(!lms.contains("goes back"), "{lms}");
}

#[test]
fn same_file_resolves_links_and_falls_back_to_the_spelling() {
    let dir = tempfile::TempDir::new().unwrap();
    let real = dir.path().join("profiles.json");
    std::fs::write(&real, "{}").unwrap();
    let link = dir.path().join("link.json");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(same_file(&link, &real));
    assert!(!same_file(&dir.path().join("other.json"), &real));
    assert!(same_file(&dir.path().join("missing.json"), &dir.path().join("missing.json")));
}
