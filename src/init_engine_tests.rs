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
        Outcome::Chosen { found: server(MLX_DEFAULT_URL, &["m"]), endpoint: None, added: false, others: vec![], asked: false }
    );
    assert!(ask.1.is_empty(), "one engine is never asked about");
}

#[test]
fn several_answering_ask_with_lm_studio_first_and_take_the_answer() {
    let probe = FakeProbe::new(true).serving(MLX_DEFAULT_URL, &["m"]);
    let mut ask = Answer(Some(1), Vec::new());
    let out = run(&probe, NO_ENDPOINTS, None, Some(&mut ask));
    assert_eq!(ask.1, vec![vec![Found::Lmstudio, server(MLX_DEFAULT_URL, &["m"])]]);
    let Outcome::Chosen { found, others, asked, .. } = out else { panic!("{out:?}") };
    assert_eq!((found, others, asked), (server(MLX_DEFAULT_URL, &["m"]), vec![Found::Lmstudio], true));
}

#[test]
fn several_answering_with_no_one_to_ask_take_lm_studio_and_name_the_others() {
    let probe = FakeProbe::new(true).serving(MLX_DEFAULT_URL, &["m"]);
    let mut nobody = Answer(None, Vec::new());
    let out = run(&probe, NO_ENDPOINTS, None, Some(&mut nobody));
    let Outcome::Chosen { found, others, asked, .. } = &out else { panic!("{out:?}") };
    assert_eq!((found, others, *asked), (&Found::Lmstudio, &vec![server(MLX_DEFAULT_URL, &["m"])], false));
    let lines = out.lines(false).join("\n");
    assert!(lines.contains("also answering:") && lines.contains(MLX_DEFAULT_URL), "{lines}");
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
    let lines = out.lines(false).join("\n");
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
    assert!(matches!(out, Outcome::Chosen { found: Found::Server { .. }, asked: false, .. }), "{out:?}");
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
        asked: false,
    };
    let text = plan_record(&mut fresh, NO_ENDPOINTS).unwrap().expect("a new endpoint is written");
    assert!(text.contains("\"local-8080\""), "{text}");
    assert!(matches!(&fresh, Outcome::Chosen { endpoint: Some(id), added: true, .. } if id == "local-8080"));

    let mut known = Outcome::Chosen {
        found: Found::Server { id: Some("mlx".into()), url: MLX_DEFAULT_URL.into(), models: vec![] },
        endpoint: None,
        added: false,
        others: vec![],
        asked: false,
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
