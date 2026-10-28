//! `lab submit` against the site's own reading. `golden/site_bundle_v4.json`
//! is what darklyenergized's `lab_run.ts` (`bundleRun`, `assembleBundle`,
//! `weightsHashOf`, `looksLikePath`) produced for the inputs below, and what
//! its `parseBundle` accepted (`ok`). A change on either side that drifts
//! from the other fails here. A bundle with nothing format 4 added is still
//! format 3, so this golden holds for it unchanged.

use super::*;
use serde_json::json;

fn golden() -> Value {
    serde_json::from_str(include_str!("golden/site_bundle_v3.json")).unwrap()
}

fn manifest() -> Value {
    json!({
        "ok": true, "profile": "p",
        "verify": { "passed": true, "details": "pytest: 14 passed in /Users/kain/x", "work_gate": {
            "baseline_test_count": 10, "tests_total": 14, "tests_passed": 14, "tests_added": 4, "tests_failed": 0,
            "sandbox_changed": true, "coverage_pct": 91.234, "coverage_min_pct": 80, "command_passed": true,
            "forced_failure_reason": null } },
        "fixture": { "source_path": "/x", "baseline_hash": "b" },
    })
}

fn stats() -> Value {
    json!({
        "run": "r", "model": "darkmux:qwen3-coder-30b", "verify": "pass", "ok": true,
        "started_at_unix_ms": 1_761_120_000_123u64, "active_ms": 123_456, "tok_per_s": 41.27, "verify_ungated": false,
        "wall_ms": 150_049, "rest_ms": 26_593, "turns": 12, "tool_calls_total": 30, "tool_calls_failed": 2,
        "completion_tokens": 9000, "reasoning_tokens": 3000, "samples_busy": 40, "pkg_w_busy": 31.26,
        "cpu_w_busy": 5.04, "gpu_w_busy": 24.95, "gpu_duty_pct": 87.5, "pkg_j_per_1k_tokens": 410.33,
        "mem_pct_busy_max": 62, "thermal_states_busy": { "nominal": 38, "fair": 2, "Bad": 1 },
        "cpu_speed_limit_min": 100, "throttled_samples": 0,
        "compactions": 2, "context_peak_tokens": 48_211, "first_token_ms_median": 1_840,
        "first_token_ms_max": 9_120, "decode_tok_per_s": 58.44,
        "checks": { "all_streams_billed": true },
    })
}

fn stack() -> Stack {
    Stack {
        chip: "Apple M5 Max".into(),
        ram_gb: Some(128),
        accelerator: Some("Metal".into()),
        os: Some("macOS 26.1".into()),
        engine: Some("LM Studio".into()),
        engine_backend: Some("MLX".into()),
        engine_version: Some("mlx-llm 1.13.1".into()),
        model: Some("qwen3-coder-30b".into()),
        weights_hash: None,
        quant: Some("4bit".into()),
        context_length: Some(65_536),
        cpu_cores: vec![("Super".into(), 6), ("Performance".into(), 12)],
        gpu_cores: Some(40),
        model_id: Some("Mac17,7".into()),
        hardware: darkmux_types::Hardware::SubmittingMachine,
    }
}

/// JSON equality with numbers compared as numbers: JavaScript writes `150`
/// where serde writes `150.0`, and the site reads them the same.
fn same(a: &Value, b: &Value, at: &str) {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => assert_eq!(x.as_f64(), y.as_f64(), "{at}"),
        (Value::Object(x), Value::Object(y)) => {
            assert_eq!(x.keys().collect::<BTreeSet<_>>(), y.keys().collect::<BTreeSet<_>>(), "{at}: keys");
            for (k, v) in x {
                same(v, &y[k], &format!("{at}.{k}"));
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            assert_eq!(x.len(), y.len(), "{at}: length");
            x.iter().zip(y).enumerate().for_each(|(i, (p, q))| same(p, q, &format!("{at}[{i}]")));
        }
        _ => assert_eq!(a, b, "{at}"),
    }
}

#[test]
fn the_bundle_is_the_one_the_site_writes_and_accepts() {
    let g = golden();
    assert_eq!(g["ok"], true, "the site's parser accepted its own bundle");
    let ran_by = RanBy { version: Some("5.4.0".into()), commit: Some("abc1234".into()), build: Some("release".into()) };
    let run = bundle_run("pepper-grinder-p-1761120000-1", &manifest(), &stats(), &ran_by, &stack(), &[]);
    let bundle = assemble(
        "kstrat2001/pepper-grinder",
        &"a".repeat(40),
        &json!({ "name": "pepper-grinder", "version": "2.0", "verify_command": "pytest" }),
        vec![run],
    )
    .unwrap();
    same(&bundle, &g["bundle"], "bundle");
}

/// (format 4) The site's own bundle for a structured-output run
/// (`golden/site_bundle_v4.json`, written by its `lab_run.ts` for these
/// inputs and accepted by its `parseBundle`): the fields count in the verify
/// detail, the reply cap in the stack.
#[test]
fn a_structured_output_bundle_is_the_one_the_site_writes_and_accepts() {
    let g: Value = serde_json::from_str(include_str!("golden/site_bundle_v4.json")).unwrap();
    assert_eq!(g["ok"], true, "the site's parser accepted its own bundle");
    let mut m = manifest();
    m["verify"] = json!({"passed": false, "details": "7/8 fields in /Users/kain/x",
        "fields_correct": 7, "fields_expected": 8, "fields_missed": ["hours"]});
    m["max_completion_tokens"] = json!(2048);
    m.as_object_mut().unwrap().remove("fixture");
    let mut st = stats();
    st["verify"] = json!("fail");
    let ran_by = RanBy { version: Some("5.4.0".into()), commit: Some("abc1234".into()), build: Some("release".into()) };
    let run = bundle_run("biz-scrape-easy-p-1761120000-1", &m, &st, &ran_by, &stack(), &[]);
    let fixture = json!({ "name": "biz-scrape-fixture", "version": "1.0", "satisfies": "biz-scrape-easy@1.0" });
    let bundle = assemble("kstrat2001/biz-scrape-fixture", &"a".repeat(40), &fixture, vec![run]).unwrap();
    same(&bundle, &g["bundle"], "bundle");
}

/// A bundle mixing a structured run with another is format 4 (the other run
/// says null); a bundle with neither a count nor a cap stays format 3.
#[test]
fn format_4_only_when_a_run_carries_what_it_added() {
    let mut m = manifest();
    m["verify"] = json!({"passed": true, "details": "d", "fields_correct": 2, "fields_expected": 2, "fields_missed": []});
    let structured = bundle_run("r", &m, &stats(), &RanBy::default(), &stack(), &[]);
    let plain = bundle_run("q", &manifest(), &stats(), &RanBy::default(), &stack(), &[]);
    let fixture = json!({ "name": "biz", "version": "1.0" });
    let four = assemble("kstrat2001/biz", &"a".repeat(40), &fixture, vec![structured, plain.clone()]).unwrap();
    assert_eq!(four["formatVersion"], 4);
    assert!(four["runs"][1]["stack"].get("maxCompletionTokens").is_some_and(Value::is_null), "the other run says null");
    let three = assemble("kstrat2001/biz", &"a".repeat(40), &fixture, vec![plain]).unwrap();
    assert_eq!(three["formatVersion"], 3, "nothing format 4 added: still format 3");
    assert!(three["runs"][0]["stack"].get("maxCompletionTokens").is_none());
    assert!(three["runs"][0]["detail"]["verify"].get("fieldsExpected").is_none());
}

/// (#3386) A run whose grader recorded criteria shares them, each id with
/// met or missed, as format 6 (the site's `tests/fixtures/darkmux_evals/criteria`
/// sample). A bundle with none stays format 5 or older, without the key, so a
/// site that reads only 5 keeps taking it.
#[test]
fn format_6_only_when_a_run_carries_criteria() {
    let mut m = manifest();
    m["verify"] = json!({"passed": false, "details": "d", "grade": {"passed": false, "checks": [],
        "criteria": [{"id": "raw-body", "passed": false}, {"id": "timestamp-tolerance", "passed": true}]}});
    let mut graded = bundle_run("r", &m, &stats(), &RanBy::default(), &stack(), &[]);
    graded["workloadPath"] = json!("eval/workloads/w.json");
    graded["workloadSha256"] = json!("b".repeat(64));
    let mut plain = bundle_run("q", &manifest(), &stats(), &RanBy::default(), &stack(), &[]);
    plain["workloadPath"] = json!("eval/workloads/w.json");
    plain["workloadSha256"] = json!("b".repeat(64));
    let fixture = json!({ "name": "webhook-forgery", "version": "1.2" });
    let six = assemble("kstrat2001/webhook-forgery-eval", &"a".repeat(40), &fixture, vec![graded, plain.clone()]).unwrap();
    assert_eq!(six["formatVersion"], 6);
    assert_eq!(
        six["runs"][0]["criteria"],
        json!([{"id": "raw-body", "passed": false}, {"id": "timestamp-tolerance", "passed": true}])
    );
    assert!(six["runs"][1]["criteria"].is_null(), "the other run says null");
    let five = assemble("kstrat2001/webhook-forgery-eval", &"a".repeat(40), &fixture, vec![plain]).unwrap();
    assert_eq!(five["formatVersion"], 5, "no criteria: still format 5");
    assert!(five["runs"][0].get("criteria").is_none());
}

/// Counts that do not add up are sent as nothing, the site's own rule.
#[test]
fn a_fields_count_that_does_not_add_up_is_left_out() {
    let detail = |c: Value, e: Value, m: Value| {
        let v = json!({"passed": false, "fields_correct": c, "fields_expected": e, "fields_missed": m});
        let d = verify_detail_from(&json!({ "verify": v }), &[]);
        (d["fieldsCorrect"].clone(), d["fieldsExpected"].clone(), d["fieldsMissed"].clone())
    };
    assert_eq!(detail(json!(7), json!(8), json!(["hours"])), (json!(7), json!(8), json!(["hours"])));
    let null = (Value::Null, Value::Null, Value::Null);
    for (c, e, m) in [
        (json!(9), json!(8), json!([])),
        (json!(0), json!(0), json!([])),
        (json!(6), json!(8), json!(["hours"])),
        (json!(6), json!(8), json!(["hours", "hours"])),
        (json!(7), json!(8), json!(["/Users/kain/x"])),
        (json!(7), json!(8), Value::Null),
    ] {
        assert_eq!(detail(c.clone(), e.clone(), m.clone()), null, "{c} {e} {m}");
    }
}

#[test]
fn the_weights_hash_is_the_sites() {
    let files = vec![
        ("model-00002.safetensors".to_string(), "b".repeat(64)),
        ("config.json".to_string(), "c".repeat(64)),
        ("Z/\u{e9}.gguf".to_string(), "d".repeat(64)),
    ];
    assert_eq!(weights_hash_of(&files).as_deref(), golden()["weights"].as_str());
    assert_eq!(weights_hash_of(&[]), None);
}

#[test]
fn paths_are_found_as_the_site_finds_them() {
    let cases = ["/Users/kain", "C:\\x", "file:///a", "%2FUsers%2Fk", "~/x", "pytest ok", "50/50 passed", "a\\\\b"];
    let want: Vec<bool> = golden()["paths"].as_array().unwrap().iter().map(|v| v.as_bool().unwrap()).collect();
    assert_eq!(cases.iter().map(|c| looks_like_path(c)).collect::<Vec<_>>(), want);
    let mut found = Vec::new();
    paths_in(&json!({"a": ["ok", "/home/me"], "/tmp/x": 1}), "b", &mut found);
    found.sort();
    assert_eq!(found, vec!["b./tmp/x", "b.a[1]"]);
}

#[test]
fn a_rate_over_part_of_the_generation_is_not_quoted() {
    let mut s = stats();
    s["checks"]["all_streams_billed"] = json!(false);
    let run = bundle_run("r", &manifest(), &s, &RanBy::default(), &stack(), &[]);
    assert_eq!(run["tokensPerSecond"], Value::Null);
    assert_eq!(run["darkmuxVersion"], Value::Null);
    // (Argon review) Decode speed is a rate over the generation too.
    s["decode_tok_per_s"] = json!(80.0);
    assert_eq!(detail_from(&manifest(), &s, &[])["decodeTokensPerSecond"], Value::Null);
    s["checks"]["all_streams_billed"] = json!(true);
    assert_eq!(detail_from(&manifest(), &s, &[])["decodeTokensPerSecond"], json!(80.0));
}

#[test]
fn host_figures_need_busy_samples() {
    let mut s = stats();
    s["samples_busy"] = json!(0);
    let d = detail_from(&manifest(), &s, &[]);
    for k in ["packageWatts", "gpuWatts", "thermalStates", "throttledSamples"] {
        assert_eq!(d[k], Value::Null, "{k}");
    }
    assert_eq!(d["turns"], 12, "work figures stay");
}

#[test]
fn refusals_name_why() {
    let m = manifest();
    let s = stats();
    assert_eq!(refusal_for(&m, &s), None);
    assert_eq!(refusal_for(&m, &json!({})), Some("it never reached a model"));
    let mut harness = m.clone();
    harness["ok"] = json!(false);
    assert_eq!(refusal_for(&harness, &s), Some("the dispatch did not complete cleanly (a harness failure)"));
    let mut no_verdict = m.clone();
    no_verdict["verify"] = json!("pass");
    assert_eq!(refusal_for(&no_verdict, &s), Some("it has no verdict to share"));
    let mut ungated = s.clone();
    ungated["verify_ungated"] = json!(true);
    assert_eq!(refusal_for(&m, &ungated), Some("it passed before the work gate existed"));
    let mut failed = m.clone();
    failed["verify"]["passed"] = json!(false);
    assert_eq!(refusal_for(&failed, &s), None, "a failed run is a result too");
    assert!(!passed_from(&failed));
}

#[test]
fn the_commit_is_the_one_checked_out_through_the_runs() {
    let a = "a".repeat(40);
    let b = "b".repeat(40);
    // Newest first: b checked out at 2000s, a at 1000s.
    let reflog = format!("{b} HEAD@{{2000}}\n{a} HEAD@{{1000}}\n");
    assert_eq!(commit_at_time(&reflog, 1_500_000, 1_900_000), Ok(a.clone()));
    assert_eq!(commit_at_time(&reflog, 2_100_000, 2_100_000), Ok(b.clone()));
    assert!(commit_at_time(&reflog, 1_500_000, 2_100_000).unwrap_err().contains("moved between"));
    assert!(commit_at_time(&reflog, 500_000, 600_000).unwrap_err().contains("--commit"));
    // Checked out b and back to a between the runs: the ends agree, the
    // middle run did not run on a.
    let away_and_back = format!("{a} HEAD@{{3000}}\n{b} HEAD@{{2000}}\n{a} HEAD@{{1000}}\n");
    assert!(commit_at_time(&away_and_back, 1_500_000, 3_500_000).unwrap_err().contains("moved between"));
    // Checking out the same commit again moves nothing.
    let again = format!("{a} HEAD@{{2000}}\n{a} HEAD@{{1000}}\n");
    assert_eq!(commit_at_time(&again, 1_500_000, 2_500_000), Ok(a.clone()));
    // Every run needs a start time to be placed, not just some of them.
    let reflog_a = format!("{a} HEAD@{{1000}}\n");
    assert_eq!(commit_through(&reflog_a, &[Some(1_500_000), Some(1_600_000)]).unwrap(), a);
    let err = commit_through(&reflog_a, &[Some(1_500_000), None]).unwrap_err().to_string();
    assert!(err.contains("--commit"), "{err}");
}

#[test]
fn facts_hold_only_when_they_changed_before_the_run() {
    assert_eq!(as_of_run(Some(1), Some(10), Some(20)), Some(1));
    assert_eq!(as_of_run(Some(1), Some(30), Some(20)), None);
    assert_eq!(as_of_run(Some(1), None, Some(20)), None);
    assert_eq!(as_of_run::<u8>(None, Some(10), Some(20)), None);
}

#[test]
fn small_readings() {
    assert_eq!(model_id_from(Some("darkmux:qwen")).as_deref(), Some("qwen"));
    assert_eq!(model_id_from(Some("darkmux:")), None);
    assert_eq!(repo_from_remote("git@github.com:kstrat2001/pepper-grinder.git").as_deref(), Some("kstrat2001/pepper-grinder"));
    assert_eq!(repo_from_remote("https://github.com/o/r").as_deref(), Some("o/r"));
    assert_eq!(repo_from_remote("https://gitlab.com/o/r"), None);
    assert!(is_model_file("model-00001-of-00004.safetensors") && is_model_file("tokenizer.json"));
    assert!(!is_model_file("verified.json") && !is_model_file("README.md"));
    assert_eq!(iso_utc(1_761_120_000_123), "2025-10-22T08:00:00.123Z");
    assert_eq!(iso_utc(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(iso_utc(951_782_400_000), "2000-02-29T00:00:00.000Z");
}

#[test]
fn a_long_summary_is_cut_to_what_the_site_takes() {
    let long = "x".repeat(600);
    let cut = shareable(Some(&json!(long)), 500, &[]);
    assert_eq!(cut.as_str().unwrap().chars().count(), 500);
    assert!(cut.as_str().unwrap().ends_with('\u{2026}'));
}

#[test]
fn the_fixture_is_read_at_the_commit_not_as_it_is_now() {
    let tmp = tempfile::TempDir::new().unwrap();
    let d = tmp.path();
    let _ = git(d, &["init", "-q"]);
    std::fs::write(d.join(".fixture.json"), r#"{"name":"pepper-grinder","version":"2.0"}"#).unwrap();
    let _ = git(d, &["add", "."]);
    let _ = git(d, &["commit", "-qm", "one"]);
    let first = String::from_utf8(Command::new("git").arg("-C").arg(d).args(["rev-parse", "HEAD"]).output().unwrap().stdout).unwrap();
    std::fs::write(d.join(".fixture.json"), r#"{"name":"pepper-grinder","version":"3.0"}"#).unwrap();
    let _ = git(d, &["commit", "-qam", "two"]);
    let _ = git(d, &["remote", "add", "origin", "git@github.com:kstrat2001/pepper-grinder.git"]);

    let (repo, sha, manifest) = fixture_at(d, &[], Some(&first.trim()[..10])).unwrap();
    assert_eq!(repo, "kstrat2001/pepper-grinder");
    assert_eq!(sha, first.trim(), "a short commit is read as the full one");
    assert_eq!(manifest["version"], "2.0");

    std::fs::write(d.join("stray.txt"), "x").unwrap();
    assert!(fixture_at(d, &[], Some(first.trim())).unwrap_err().to_string().contains("uncommitted"));
    std::fs::remove_file(d.join("stray.txt")).unwrap();
    assert!(fixture_at(d, &[], None).unwrap_err().to_string().contains("no start time"));
    assert!(fixture_at(d, &[], Some("deadbeef")).unwrap_err().to_string().contains("no commit"));
}

#[test]
fn weights_are_the_model_files_under_the_folder() {
    let tmp = tempfile::TempDir::new().unwrap();
    let d = tmp.path();
    std::fs::create_dir(d.join("sub")).unwrap();
    for f in ["config.json", "sub/model.safetensors", ".hidden.safetensors", "verified.json"] {
        std::fs::write(d.join(f), f).unwrap();
    }
    let mut rel: Vec<String> = model_files(d).iter().map(|f| rel_path(d, f)).collect();
    rel.sort();
    assert_eq!(rel, vec!["config.json", "sub/model.safetensors"]);
    let one = d.join("config.json");
    assert_eq!(rel_path(&one, &model_files(&one)[0]), "config.json", "one file is named by itself");
}

#[test]
fn what_the_site_would_refuse_is_refused_here() {
    let ran_by = RanBy { version: Some("5.4.0".into()), commit: Some("abc1234".into()), build: Some("release".into()) };
    let good = bundle_run("r1", &manifest(), &stats(), &ran_by, &stack(), &[]);
    let fixture = json!({ "name": "pepper-grinder", "version": 2.0 });
    let ok = assemble("kstrat2001/pepper-grinder", &"a".repeat(40), &fixture, vec![good.clone()]).unwrap();
    assert_eq!(site_refusals(&ok), Vec::<String>::new());
    assert_eq!(ok["fixture"]["version"], "2", "a number version reads as the site's JavaScript writes it");

    let mut vm = stack();
    vm.chip = "Apple M2 (Virtual)".into();
    vm.quant = Some("q".repeat(41));
    vm.cpu_cores = (0..5).map(|i| (if i == 0 { "super core".to_string() } else { "Performance".to_string() }, 4)).collect();
    vm.gpu_cores = Some(5000);
    vm.model_id = Some("kains-macbook".into());
    let bad_ran = RanBy { version: Some("5.4.0 beta".into()), ..Default::default() };
    let bad = bundle_run("r2", &manifest(), &stats(), &bad_ran, &vm, &[]);
    let both = assemble("kstrat2001/pepper-grinder", &"a".repeat(40), &fixture, vec![bad, good.clone(), good]).unwrap();
    let refused = site_refusals(&both);
    for want in [
        "runs[0].stack.chip",
        "runs[0].stack.quant",
        "runs[0].stack.cpuCores: expected 1 to 4",
        "runs[0].stack.cpuCores[0].cluster",
        "runs[0].stack.gpuCores: outside",
        "runs[0].stack.modelId",
        "runs[0].darkmuxVersion",
        "runs[2]: the same run twice",
    ] {
        assert!(refused.iter().any(|r| r.starts_with(want)), "{want} in {refused:?}");
    }
}

#[test]
fn a_word_boundary_is_ascii_as_in_javascript() {
    assert!(looks_like_path("see /var\u{e9}"), "the site reads é as a boundary");
}

// ---------------------------------------------------------------------------
// gather_on: runs, a fixture checkout and a machine, read end to end
// ---------------------------------------------------------------------------

use darkmux_profiles::engine::ModelProvenance;

/// A fixture checkout, a flows dir, a weights folder and runs, all on disk.
struct Bench {
    root: tempfile::TempDir,
    fixture: PathBuf,
    flows: PathBuf,
    weights: PathBuf,
    /// Run starts: a minute from now, so the fixture commit and the weights are older.
    start: u64,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The eval's own workload document in the bench fixture.
const WORKLOAD_DOC: &[u8] = br#"{"workload":{"id":"pg"}}"#;

fn bench() -> Bench {
    let root = tempfile::tempdir().unwrap();
    let fixture = root.path().join("fixture");
    std::fs::create_dir(&fixture).unwrap();
    git(&fixture, &["init", "-q"]);
    std::fs::write(
        fixture.join(".fixture.json"),
        r#"{"name":"pepper-grinder","version":2.0,"satisfies":"pg@2.0","eval_dir":"eval"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(fixture.join("eval/workloads")).unwrap();
    std::fs::write(fixture.join("eval/workloads/pg.json"), WORKLOAD_DOC).unwrap();
    git(&fixture, &["add", "."]);
    git(&fixture, &["commit", "-qm", "fixture"]);
    git(&fixture, &["remote", "add", "origin", "https://github.com/kstrat2001/pepper-grinder.git"]);
    let flows = root.path().join("flows");
    std::fs::create_dir(&flows).unwrap();
    let weights = root.path().join("weights");
    std::fs::create_dir_all(weights.join("sub")).unwrap();
    std::fs::write(weights.join("model.safetensors"), b"tensor bytes").unwrap();
    std::fs::write(weights.join("sub").join("config.json"), b"{}").unwrap();
    std::fs::write(weights.join(".hidden.safetensors"), b"left out").unwrap();
    std::fs::write(weights.join("README.md"), b"not a model file").unwrap();
    let start = mtime_ms(&weights.join("model.safetensors")).unwrap() + 60_000;
    Bench { root, fixture, flows, weights, start }
}

impl Bench {
    /// A run of `model`, its manifest given `extra` fields; returns its path.
    fn run(&self, name: &str, model: &str, extra: Value) -> String {
        let dir = self.root.path().join("runs").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let trajectory = [
            json!({"type": "dispatch.start", "ts": self.start, "model": model}),
            json!({"type": "dispatch.complete", "ts": self.start + 10_000, "result": "stop", "wall_ms": 10_000}),
        ];
        std::fs::write(dir.join("trajectory.jsonl"), trajectory.iter().map(|v| format!("{v}\n")).collect::<String>()).unwrap();
        std::fs::write(dir.join("lifecycle.json"), json!({"session_id": name}).to_string()).unwrap();
        let mut manifest = json!({
            "ok": true, "session_id": name, "profile": "p",
            "verify": {"passed": true, "details": "ok"},
            "fixture": {"source_path": self.fixture.display().to_string(), "baseline_hash": "b",
                "eval_dir": "eval", "workload_path": "eval/workloads/pg.json", "workload_sha256": sha256::hex(WORKLOAD_DOC)},
        });
        for (k, v) in extra.as_object().unwrap() {
            manifest[k] = v.clone();
        }
        std::fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
        dir.display().to_string()
    }

    fn provenance(&self) -> ModelProvenance {
        ModelProvenance {
            backend: Some("MLX".into()),
            quant: Some("4bit".into()),
            weights_path: Some(self.weights.clone()),
            runtime: Some("mlx-llm 1.13.1".into()),
            runtime_installed_at_ms: Some(0),
        }
    }

    /// An Asahi machine where everything was in place before the runs.
    fn machine(&self, provenance: Option<ModelProvenance>) -> Machine {
        Machine {
            chip: "Apple M2".into(),
            ram_gb: Some(8),
            accelerator: Some("Vulkan".into()),
            config: darkmux_platform::MachineConfig {
                cpu_clusters: vec![("Performance".into(), 4), ("Efficiency".into(), 4)],
                ..Default::default()
            },
            os_release: Some(darkmux_platform::OsRelease { name: "Omarchy (Arch Linux ARM)".into(), installed_at_unix_ms: Some(0) }),
            registry: serde_json::from_value(json!({"profiles": {"p": {"models": [{"id": "m1", "n_ctx": 65_536}]}}})).ok(),
            installed: darkmux_types::BuildInfo { version: "5.4.0".into(), commit: Some("c".repeat(40)), build: Some(darkmux_types::BuildKind::Dev) },
            installed_at: Some(0),
            provenance: Box::new(move |m| (m == "m1").then(|| provenance.clone()).flatten()),
            identifying: Vec::new(),
        }
    }

    fn gather(&self, machine: &Machine, runs: &[String], stated: &Stated) -> Result<(Built, Vec<String>)> {
        let mut said = Vec::new();
        let built = gather_on(machine, runs, stated, &self.flows, &mut |s| said.push(s.to_string()))?;
        Ok((built, said))
    }
}

#[test]
fn gather_reads_the_runs_the_fixture_and_the_machine() {
    let b = bench();
    let runs = vec![
        b.run("r1", "darkmux:m1", json!({})),
        b.run("r2", "darkmux:m1", json!({"verify": {"passed": false, "details": "1 failed"},
            "darkmux": {"version": "5.3.0", "commit": "a".repeat(40), "build": "release"}})),
    ];
    let (built, said) = b.gather(&b.machine(Some(b.provenance())), &runs, &Stated::default()).unwrap();
    let sha = git(&b.fixture, &["rev-parse", "HEAD"]);
    assert_eq!(built.fixture, format!("kstrat2001/pepper-grinder@{} (pepper-grinder 2)", &sha[..7]));
    assert_eq!(built.bundle["fixture"]["commitSha"], sha);
    assert_eq!(said.len(), 1, "one model's weights are hashed once: {said:?}");
    let r1 = &built.bundle["runs"][0];
    let stack = &r1["stack"];
    assert_eq!(stack["chip"], "Apple M2");
    assert_eq!(stack["os"], "Omarchy (Arch Linux ARM)");
    assert_eq!(stack["engine"], "LM Studio");
    assert_eq!(stack["engineBackend"], "MLX");
    assert_eq!(stack["engineVersion"], "mlx-llm 1.13.1");
    assert_eq!(stack["quant"], "4bit");
    assert_eq!(stack["model"], "m1");
    assert_eq!(stack["contextLength"], 65_536, "the window the run's profile declares");
    assert_eq!(stack["cpuCores"], json!([{"cluster": "Performance", "cores": 4}, {"cluster": "Efficiency", "cores": 4}]));
    assert_eq!((&stack["gpuCores"], &stack["modelId"]), (&Value::Null, &Value::Null), "what the machine does not say stays null");
    let files = [
        ("model.safetensors".to_string(), sha256::hex(b"tensor bytes")),
        ("sub/config.json".to_string(), sha256::hex(b"{}")),
    ];
    assert_eq!(stack["weightsHash"].as_str(), weights_hash_of(&files).as_deref(), "hidden and non-model files left out");
    assert_eq!((r1["darkmuxVersion"].as_str(), r1["darkmuxBuild"].as_str()), (Some("5.4.0"), Some("dev")), "installed before the run");
    let r2 = &built.bundle["runs"][1];
    assert_eq!((r2["darkmuxVersion"].as_str(), r2["darkmuxBuild"].as_str()), (Some("5.3.0"), Some("release")), "the manifest's own record wins");
    assert_eq!(r2["passed"], false);
    assert!(built.notes[0].starts_with("r1: passed, m1; not reported: gpuCores, modelId, kvCacheDtype"), "{}", built.notes[0]);
    assert!(built.notes[1].starts_with("r2: failed, m1"), "{}", built.notes[1]);
}

/// (#3367) A rolling release records no install time, so the as-of check
/// drops the OS on every Linux run. The name the run recorded holds anyway,
/// and the window the run recorded beats the registry loaded at submit.
#[test]
fn a_run_that_recorded_its_os_and_window_keeps_them_when_the_machine_cannot_say() {
    let b = bench();
    let runs = vec![b.run("r1", "darkmux:m1", json!({"os": "Omarchy (Arch Linux ARM)", "n_ctx": 16_384}))];
    let mut machine = b.machine(Some(b.provenance()));
    machine.os_release = Some(darkmux_platform::OsRelease { name: "Omarchy (updated since)".into(), installed_at_unix_ms: None });
    let (built, _) = b.gather(&machine, &runs, &Stated::default()).unwrap();
    let stack = &built.bundle["runs"][0]["stack"];
    assert_eq!(stack["os"], "Omarchy (Arch Linux ARM)", "the name recorded with the run");
    assert_eq!(stack["contextLength"], 16_384, "the window the run loaded, not the registry's 65536");
    assert!(!built.notes[0].contains("the OS may have been updated"), "{}", built.notes[0]);
}

/// A name the site's form refuses stays null. Putting it on the bundle would
/// fail the upload, and null is a value the site allows.
#[test]
fn a_recorded_os_the_site_does_not_take_stays_null() {
    let b = bench();
    let runs = vec![b.run("r1", "darkmux:m1", json!({"os": "Arch Linux"}))];
    let mut machine = b.machine(Some(b.provenance()));
    machine.os_release = Some(darkmux_platform::OsRelease { name: "Arch Linux".into(), installed_at_unix_ms: None });
    let (built, _) = b.gather(&machine, &runs, &Stated::default()).unwrap();
    assert_eq!(built.bundle["runs"][0]["stack"]["os"], Value::Null);
    assert!(built.notes[0].contains("os is not in the form the site takes (Arch Linux)"), "{}", built.notes[0]);
    assert!(!built.notes[0].contains("the OS may have been updated"), "{}", built.notes[0]);
}

/// The same machine, with nothing recorded on the run, still reports no OS:
/// an unknown install time is not a license to claim the name read now.
#[test]
fn an_unrecorded_os_with_no_install_time_stays_unreported() {
    let b = bench();
    let runs = vec![b.run("r1", "darkmux:m1", json!({}))];
    let mut machine = b.machine(Some(b.provenance()));
    machine.os_release = Some(darkmux_platform::OsRelease { name: "Omarchy (Arch Linux ARM)".into(), installed_at_unix_ms: None });
    let (built, _) = b.gather(&machine, &runs, &Stated::default()).unwrap();
    assert_eq!(built.bundle["runs"][0]["stack"]["os"], Value::Null);
    assert!(built.notes[0].contains("the OS may have been updated"), "{}", built.notes[0]);
}

#[test]
fn what_changed_after_the_run_is_left_out_and_said() {
    let b = bench();
    let runs = vec![b.run("r1", "darkmux:m1", json!({}))];
    let mut machine = b.machine(Some(ModelProvenance { runtime_installed_at_ms: Some(b.start + 1), ..b.provenance() }));
    machine.installed_at = Some(b.start + 1);
    machine.os_release = Some(darkmux_platform::OsRelease { name: "macOS 26.1".into(), installed_at_unix_ms: Some(b.start + 1) });
    let (built, _) = b.gather(&machine, &runs, &Stated::default()).unwrap();
    let r1 = &built.bundle["runs"][0];
    assert_eq!(r1["darkmuxVersion"], Value::Null);
    assert_eq!(r1["stack"]["os"], Value::Null);
    assert_eq!(r1["stack"]["engineVersion"], Value::Null);
    let note = &built.notes[0];
    for why in ["darkmux was updated after the run", "the OS may have been updated", "mlx-llm 1.13.1 may not be the runtime"] {
        assert!(note.contains(why), "{note}");
    }

    // Weights written after the run: neither their hash nor their quant holds.
    let later = b.root.path().join("later.gguf");
    std::fs::write(&later, b"newer").unwrap();
    let mut machine = b.machine(Some(ModelProvenance { weights_path: Some(later), ..b.provenance() }));
    machine.installed_at = None;
    let runs = vec![b.run("r9", "darkmux:m1", json!({}))];
    std::fs::write(
        b.root.path().join("runs/r9/trajectory.jsonl"),
        format!("{}\n", json!({"type": "dispatch.start", "ts": 1_000, "model": "darkmux:m1"})),
    )
    .unwrap();
    let (built, _) = b.gather(&machine, &runs, &Stated { commit: Some(git(&b.fixture, &["rev-parse", "HEAD"])), ..Default::default() }).unwrap();
    let stack = &built.bundle["runs"][0]["stack"];
    assert_eq!((stack["weightsHash"].clone(), stack["quant"].clone()), (Value::Null, Value::Null));
    assert!(built.notes[0].contains("the weights changed after the run"), "{}", built.notes[0]);
}

#[test]
fn a_model_lm_studio_does_not_list_says_what_to_pass() {
    let b = bench();
    let runs = vec![b.run("r1", "darkmux:elsewhere", json!({}))];
    let (built, said) = b.gather(&b.machine(None), &runs, &Stated::default()).unwrap();
    assert!(said.is_empty(), "nothing to hash");
    let stack = &built.bundle["runs"][0]["stack"];
    assert_eq!((stack["engine"].clone(), stack["weightsHash"].clone(), stack["contextLength"].clone()), (Value::Null, Value::Null, Value::Null));
    for why in ["LM Studio does not list this model (pass --engine)", "weights unknown (pass --weights-dir)"] {
        assert!(built.notes[0].contains(why), "{}", built.notes[0]);
    }
}

#[test]
fn what_the_operator_states_stands_in_for_what_the_machine_cannot_say() {
    let b = bench();
    let runs = vec![b.run("r1", "darkmux:flint", json!({}))];
    let stated = Stated {
        engine: Some("rec".into()),
        engine_backend: Some("rec-metal".into()),
        engine_version: Some("0.9".into()),
        quant: Some("q8".into()),
        weights_dir: Some(b.weights.clone()),
        darkmux_version: Some("5.3.0".into()),
        commit: Some(git(&b.fixture, &["rev-parse", "--short", "HEAD"])),
        hardware: None,
    };
    let (built, _) = b.gather(&b.machine(Some(b.provenance())), &runs, &stated).unwrap();
    let r1 = &built.bundle["runs"][0];
    assert_eq!(r1["stack"]["engine"], "rec");
    assert_eq!(r1["stack"]["engineBackend"], "rec-metal");
    assert_eq!(r1["stack"]["engineVersion"], "0.9");
    assert_eq!(r1["stack"]["quant"], "q8");
    assert!(r1["stack"]["weightsHash"].is_string());
    assert_eq!((r1["darkmuxVersion"].as_str(), r1["darkmuxCommit"].as_str()), (Some("5.3.0"), None), "a stated version says nothing of its build");
    assert_eq!(built.bundle["fixture"]["commitSha"], git(&b.fixture, &["rev-parse", "HEAD"]), "a stated short sha is resolved");
}

#[test]
fn gather_refuses_what_it_cannot_share() {
    let b = bench();
    let machine = b.machine(Some(b.provenance()));
    let err = |runs: &[String], stated: &Stated| b.gather(&machine, runs, stated).unwrap_err().to_string();
    let r1 = b.run("r1", "darkmux:m1", json!({}));
    assert!(err(&[], &Stated::default()).contains("name at least one lab run"));
    assert!(err(&[r1.clone(), r1.clone()], &Stated::default()).contains("named twice"));

    let gone = b.root.path().join("runs/gone").display().to_string();
    std::fs::create_dir_all(&gone).unwrap();
    let harness = b.run("harness", "darkmux:m1", json!({"ok": false}));
    let e = err(&[gone, harness], &Stated::default());
    assert!(e.contains("gone: no manifest.json") && e.contains("harness: the dispatch did not complete cleanly"), "{e}");

    let empty = b.root.path().join("runs/empty");
    std::fs::create_dir_all(&empty).unwrap();
    std::fs::write(empty.join("manifest.json"), "{}").unwrap();
    assert!(err(&[empty.display().to_string()], &Stated::default()).contains("its numbers cannot be read"));

    let other = b.run("other", "darkmux:m1", json!({"fixture": {"source_path": "/elsewhere", "baseline_hash": "b"}}));
    assert!(err(&[r1.clone(), other], &Stated::default()).contains("one fixture checkout at a time"));
    let rebased = b.run("rebased", "darkmux:m1", json!({"fixture": {"source_path": b.fixture.display().to_string(), "baseline_hash": "c"}}));
    assert!(err(&[r1.clone(), rebased], &Stated::default()).contains("different fixture contents"));
    let m2 = b.run("m2", "darkmux:m2", json!({}));
    let e = err(&[r1.clone(), m2], &Stated { quant: Some("4bit".into()), ..Default::default() });
    assert!(e.contains("describe one model; these runs used m1, m2"), "{e}");

    let bare = b.root.path().join("bare");
    std::fs::create_dir(&bare).unwrap();
    assert!(err(std::slice::from_ref(&r1), &Stated { weights_dir: Some(bare), ..Default::default() }).contains("no weights found for m1"));

    std::fs::write(b.fixture.join("dirty.txt"), "x").unwrap();
    assert!(err(&[r1], &Stated::default()).contains("uncommitted changes"));
}

#[test]
fn only_apple_silicon_results_are_taken() {
    assert_eq!(apple_chip(Some("Apple M5 Max".into())).unwrap(), "Apple M5 Max");
    assert!(apple_chip(Some("Intel(R) Xeon(R)".into())).unwrap_err().to_string().contains("reports Intel(R) Xeon(R)"));
    assert!(apple_chip(Some(String::new())).unwrap_err().to_string().contains("no Apple chip"));
    assert!(apple_chip(None).unwrap_err().to_string().contains("no Apple chip"));
}

#[test]
fn small_readers_say_none_rather_than_guess() {
    assert_eq!(build_word(darkmux_types::BuildKind::Release), "release");
    assert_eq!(build_word(darkmux_types::BuildKind::Dev), "dev");
    assert_eq!(fixture_field(&json!({"fixture": {"source_path": "/x"}}), "source_path").as_deref(), Some("/x"));
    assert_eq!(fixture_field(&json!({"fixture": {"source_path": 3}}), "source_path"), None);
    assert_eq!(fixture_field(&json!({}), "source_path"), None);
    assert_eq!(mtime_ms(Path::new("/no/such/file")), None);
    let mut all = stack();
    all.weights_hash = Some("h".into());
    assert_eq!(all.unreported(), vec!["kvCacheDtype"]);
    let none = Stack {
        chip: "Apple M2".into(),
        ram_gb: None,
        accelerator: None,
        os: None,
        engine: None,
        engine_backend: None,
        engine_version: None,
        model: None,
        weights_hash: None,
        quant: None,
        context_length: None,
        cpu_cores: Vec::new(),
        gpu_cores: None,
        model_id: None,
        hardware: darkmux_types::Hardware::Unknown,
    };
    assert_eq!(none.unreported().len(), 13);
}

#[test]
fn a_fixture_version_reads_as_the_site_writes_it() {
    let version = |v: Value| assemble("o/r", &"a".repeat(40), &json!({"name": "f", "version": v}), vec![]).unwrap()["fixture"]["version"].clone();
    assert_eq!(version(Value::Null), "1.0", "no version reads as 1.0");
    assert_eq!(version(json!("2.1")), "2.1");
    assert_eq!(version(json!(3)), "3");
    assert_eq!(version(json!(2.5)), "2.5");
    assert_eq!(version(json!(true)), "true");
    assert_eq!(assemble("o/r", "a", &json!({"name": "f"}), vec![]).unwrap()["fixture"]["version"], "1.0");
    for nameless in [json!({}), json!({"name": "  "}), json!({"name": 3})] {
        assert!(assemble("o/r", "a", &nameless, vec![]).unwrap_err().to_string().contains("has no name"));
    }
}

#[test]
fn a_fixture_checkout_that_cannot_say_what_ran_is_refused() {
    let b = bench();
    let head = git(&b.fixture, &["rev-parse", "HEAD"]);
    let _ = git(&b.fixture, &["remote", "set-url", "origin", "https://gitlab.com/o/r.git"]);
    assert!(fixture_at(&b.fixture, &[], Some(&head)).unwrap_err().to_string().contains("not a GitHub repo"));
    let _ = git(&b.fixture, &["remote", "set-url", "origin", "https://github.com/o/r.git"]);
    std::fs::write(b.fixture.join(".fixture.json"), "not json").unwrap();
    let _ = git(&b.fixture, &["commit", "-qam", "broken"]);
    let broken = git(&b.fixture, &["rev-parse", "HEAD"]);
    assert!(fixture_at(&b.fixture, &[], Some(&broken)).unwrap_err().to_string().contains("reading the fixture's .fixture.json"));
    let _ = git(&b.fixture, &["rm", "-q", ".fixture.json"]);
    let _ = git(&b.fixture, &["commit", "-qm", "gone"]);
    let gone = git(&b.fixture, &["rev-parse", "HEAD"]);
    assert!(fixture_at(&b.fixture, &[], Some(&gone)).unwrap_err().to_string().contains("has no .fixture.json at"));
    assert!(fixture_at(&b.root.path().join("not-a-checkout"), &[], Some(&head)).unwrap_err().to_string().contains("not a git checkout"));
}

#[test]
fn no_weights_where_there_is_no_folder() {
    assert!(model_files(Path::new("/no/such/weights")).is_empty());
}

#[test]
fn gather_here_refuses_before_it_reads_anything() {
    let err = gather(&[], &Stated::default(), &mut |_| {}).unwrap_err().to_string();
    // Off Apple Silicon the machine is refused first; on it, the empty list.
    assert!(err.contains("name at least one lab run") || err.contains("Apple Silicon results only"), "{err}");
}

/// What identifies the machine and its user never leaves it (Kain, 2026-10-23):
/// free text that names them is left out, and anything else that does refuses
/// the bundle, saying where but never what. The fixture is a public repo's.
#[test]
fn nothing_that_identifies_the_machine_or_its_user_is_shared() {
    let b = bench();
    let serial = "C02XYZ123AB";
    let uuid = "00000000-0000-4000-8000-ABCDEF000009";
    let mut machine = b.machine(Some(b.provenance()));
    machine.identifying = vec![serial.into(), uuid.into(), "Kains-MacBook-Pro".into(), "kstrat2001".into(), "ab".into()];
    let runs = vec![
        b.run("r1", "darkmux:m1", json!({"verify": {"passed": true, "details": "ran on Kains-MacBook-Pro.local"}})),
        b.run("r2", "darkmux:m1", json!({"verify": {"passed": false, "details": "mail kain@example.com for the logs",
            "work_gate": {"forced_failure_reason": format!("board {serial} timed out")}}})),
        b.run("r3", "darkmux:m1", json!({"verify": {"passed": true, "details": "lab ab 3 of 5"}})),
    ];
    let (built, _) = b.gather(&machine, &runs, &Stated::default()).unwrap();
    let text = serde_json::to_string(&built.bundle["runs"]).unwrap();
    for gone in [serial, uuid, "Kains-MacBook-Pro", "kain@example.com"] {
        assert!(!text.contains(gone), "{gone} reached the bundle: {text}");
    }
    let verify = |i: usize| built.bundle["runs"][i]["detail"]["verify"].clone();
    assert_eq!((verify(0)["summary"].clone(), verify(1)["summary"].clone()), (Value::Null, Value::Null));
    assert_eq!(verify(1)["forcedFailureReason"], Value::Null);
    assert_eq!(verify(2)["summary"], "lab ab 3 of 5", "a two-letter name is too common to judge by");
    assert_eq!(built.bundle["fixture"]["repo"], "kstrat2001/pepper-grinder", "the fixture's public repo is shared as it is");

    // A name anywhere else is not edited out: the bundle is refused, naming where.
    let named = vec![b.run("r4", "darkmux:Kains-MacBook-Pro-model", json!({}))];
    let err = b.gather(&machine, &named, &Stated { engine: Some("LM Studio".into()), ..Default::default() }).unwrap_err().to_string();
    assert!(err.contains("identifies this machine or you") && err.contains("bundle.runs[0].stack.model"), "{err}");
    assert!(!err.contains("Kains-MacBook-Pro"), "the refusal never repeats the name: {err}");
}

#[test]
fn personal_text_is_an_email_an_address_or_a_uuid() {
    for yes in ["kain@example.com", "from 192.168.1.20", "a4:83:e7:0b:12:9f", "a4-83-e7-0b-12-9f", "fe80:0:0:0:a483:e7ff:fe0b:129f", "id 00000000-0000-4000-8000-ABCDEF000009"] {
        assert!(looks_personal(yes), "{yes}");
    }
    for no in ["mlx-llm 1.13.1", "14 of 14 passed", "qwen/qwen3.8-27b", "@decorator", "v5.4.0", "at 12:30:45", "std::fmt"] {
        assert!(!looks_personal(no), "{no}");
    }
}

#[test]
fn a_name_counts_as_a_whole_word_in_any_case() {
    let names = vec!["kain".to_string(), "macbook-dm-5".to_string(), "ab".to_string()];
    assert!(names_any("built by Kain today", &names));
    assert!(names_any("host MACBOOK-DM-5.local", &names));
    assert!(!names_any("the kainite crystal", &names), "inside a longer word");
    assert!(!names_any("ab ab ab", &names), "too short to judge by");
    assert!(!names_any("macbook-dm-50", &names));
    let mut found = Vec::new();
    names_in(&json!({"a": ["x", "kain"], "kain": 1}), "v", &names, &mut found);
    assert_eq!(found, vec!["v.a[1]".to_string(), "v.kain".to_string()]);
}

#[test]
fn the_user_is_named_by_login_home_folder_and_real_name_and_by_their_words() {
    let got = identifying_here(
        vec!["C02XYZ".into(), "Kain\u{2019}s MacBook Pro".into()],
        Some("kain".into()),
        Some("kain".into()),
        Some(PathBuf::from("/Users/kosterholt")),
        Some("Kain Osterholt".into()),
    );
    assert_eq!(
        got,
        ["C02XYZ", "Kain\u{2019}s MacBook Pro", "Kain", "kain", "kosterholt", "Kain Osterholt", "Osterholt"].map(String::from).to_vec()
    );
    assert!(identifying_here(Vec::new(), Some(" ".into()), None, None, None).is_empty());
    assert_eq!(name_words("lab-server_01.local"), vec!["lab".to_string(), "server".to_string()], "digits-only and product words are not names");
    // Free text names the computer with a plain apostrophe, macOS with a curly one.
    assert!(names_any("built on Kain's MacBook Pro", &got));
}

/// A name is judged on the whole text, before it is cut to the site's limit:
/// a name cut in half no longer reads as one, and must not ship as half.
#[test]
fn a_long_text_naming_the_machine_is_left_out_not_cut() {
    let names = vec!["Kains-MacBook-Pro".to_string()];
    let long = format!("{} Kains-MacBook-Pro", "x".repeat(492));
    assert_eq!(shareable(Some(&json!(long)), 500, &names), Value::Null);
    assert!(shareable(Some(&json!(long)), 500, &[]).as_str().is_some_and(|t| t.ends_with('\u{2026}')));
}

#[test]
fn the_git_user_name_is_what_git_printed_trimmed_or_nothing() {
    assert_eq!(user_name_from(true, b"Kain Osterholt\n"), Some("Kain Osterholt".to_string()));
    assert_eq!(user_name_from(true, b"  \n"), None, "only space");
    assert_eq!(user_name_from(true, b""), None, "no user.name set");
    assert_eq!(user_name_from(false, b"Kain\n"), None, "git failed");
}

/// (#3361) A bundle names the eval's own workload document each run used,
/// as format 5.
#[test]
fn a_bundle_names_the_eval_workload_each_run_used() {
    let b = bench();
    let runs = vec![b.run("r1", "darkmux:m1", json!({}))];
    let (built, _) = b.gather(&b.machine(Some(b.provenance())), &runs, &Stated::default()).unwrap();
    assert_eq!(built.bundle["formatVersion"], 5);
    let r1 = &built.bundle["runs"][0];
    assert_eq!(r1["workloadPath"], "eval/workloads/pg.json");
    assert_eq!(r1["workloadSha256"], sha256::hex(WORKLOAD_DOC));
}

/// (#3361) Only an eval that owns its workload is shared, and only runs of
/// the workload document at the fixture commit.
#[test]
fn a_run_not_of_the_eval_own_workload_is_refused() {
    let b = bench();
    let mut wrong = b.run("changed", "darkmux:m1", json!({}));
    let manifest_path = std::path::Path::new(&wrong).join("manifest.json");
    let mut m: Value = serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    m["fixture"]["workload_sha256"] = json!(sha256::hex(b"edited after the commit"));
    std::fs::write(&manifest_path, m.to_string()).unwrap();
    let elsewhere = b.run("elsewhere", "darkmux:m1", json!({}));
    let manifest_path = std::path::Path::new(&elsewhere).join("manifest.json");
    let mut m: Value = serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    m["fixture"].as_object_mut().unwrap().retain(|k, _| k == "source_path" || k == "baseline_hash");
    std::fs::write(&manifest_path, m.to_string()).unwrap();
    let err = b.gather(&b.machine(Some(b.provenance())), &[wrong.clone(), elsewhere], &Stated::default()).unwrap_err().to_string();
    assert!(err.contains("changed: its workload eval/workloads/pg.json is not the one at"), "{err}");
    assert!(err.contains("elsewhere: its workload did not come from the eval's eval/"), "{err}");

    // A fixture that defines no eval_dir does not own its workload.
    std::fs::write(b.fixture.join(".fixture.json"), r#"{"name":"pepper-grinder","version":2.0}"#).unwrap();
    git(&b.fixture, &["commit", "-qam", "no eval"]);
    wrong = b.run("old-fixture", "darkmux:m1", json!({}));
    let err = b.gather(&b.machine(Some(b.provenance())), &[wrong], &Stated::default()).unwrap_err().to_string();
    assert!(err.contains("defines no eval_dir"), "{err}");
}

#[test]
fn an_eval_document_path_is_plain_names_inside_the_eval_dir() {
    assert!(super::is_eval_document("eval/workloads/pg.json", "eval"));
    for bad in ["eval/../secret.json", "eval/workloads/pg.txt", "other/pg.json", "eval/my doc.json", "/eval/pg.json", "eval/./pg.json", "pg.json"] {
        assert!(!super::is_eval_document(bad, "eval"), "{bad}");
    }
}

/// (#3397) The bundles `lab submit` writes for runs served on the submitting
/// machine (LM Studio), one without criteria (format 5) and one with (format
/// 6), as the text `deliver` writes and uploads.
fn submitting_machine_bundles() -> [(String, &'static str); 2] {
    let ran_by = RanBy { version: Some("5.8.0".into()), commit: Some("abc1234".into()), build: Some("release".into()) };
    let with_workload = |mut run: Value| {
        run["workloadPath"] = json!("eval/workloads/w.json");
        run["workloadSha256"] = json!("b".repeat(64));
        run
    };
    let fixture = json!({ "name": "webhook-forgery", "version": "1.2", "eval_dir": "eval" });
    let plain = with_workload(bundle_run("plain-p-1761120000-1", &manifest(), &stats(), &ran_by, &stack(), &[]));
    let mut m = manifest();
    m["verify"]["grade"] = json!({"passed": true, "checks": [], "criteria": [{"id": "raw-body", "passed": true}]});
    let graded = with_workload(bundle_run("graded-p-1761120000-2", &m, &stats(), &ran_by, &stack(), &[]));
    let text = |runs: Vec<Value>| {
        serde_json::to_string_pretty(&assemble("kstrat2001/webhook-forgery-eval", &"a".repeat(40), &fixture, runs).unwrap()).unwrap() + "\n"
    };
    [
        (text(vec![plain.clone()]), include_str!("golden/submitting_machine_v5.json")),
        (text(vec![graded, plain]), include_str!("golden/submitting_machine_v6.json")),
    ]
}

/// (#3397) A bundle whose runs were all served on the submitting machine is
/// byte for byte what main wrote before `hardware` existed (the goldens were
/// written by 666395b39), so a site that reads formats up to 6 keeps taking it.
#[test]
fn a_submitting_machine_bundle_is_byte_for_byte_what_main_wrote() {
    for (i, (now, then)) in submitting_machine_bundles().iter().enumerate() {
        assert_eq!(now, then, "bundle {i}");
    }
}

// ---------------------------------------------------------------------------
// (#3397) Which machine served a run
// ---------------------------------------------------------------------------

use crate::lab::manifest::{HardwareFrom, ModelHardware};
use darkmux_types::Hardware::{SubmittingMachine, Unknown};

fn snap(model: &str, from: HardwareFrom, hardware: Option<darkmux_types::Hardware>) -> ModelHardware {
    ModelHardware {
        model: model.into(),
        identifier: None,
        from,
        hardware,
        provider: (from == HardwareFrom::Provider).then(|| "Anthropic API".to_string()),
        endpoint: (from != HardwareFrom::Managed).then(|| "studio".to_string()),
    }
}

fn profile_model(json: Value) -> darkmux_types::ProfileModel {
    let mut reg: darkmux_types::ProfileRegistry = serde_json::from_value(json!({
        "profiles": {"p": {"models": [json]}},
        "endpoints": {"claude": {"url": "https://api.anthropic.com/v1"}, "studio": {"url": "http://studio.local:8080/v1", "hardware": "unknown"}}
    }))
    .unwrap();
    reg.materialize_endpoints();
    reg.profiles["p"].models[0].clone()
}

/// The precedence, decided once: what darkmux knows on its own (a managed
/// model, a host on the provider registry) is fixed and a flag that
/// contradicts it is refused; for any other endpoint `--hardware` wins over
/// the run's snapshot (it corrects a wrong one), the snapshot stands in for
/// no flag, and with neither the run is refused. A run recorded before the
/// snapshot falls back to the registry for a managed model or a listed host
/// only, never to an endpoint's declaration as it is now.
#[test]
fn hardware_precedence_is_registry_then_flag_then_snapshot() {
    let flint = [snap("flint", HardwareFrom::Endpoint, Some(SubmittingMachine))];
    let at = |s: Option<&[ModelHardware]>, cur: Option<&darkmux_types::ProfileModel>, flag| served_by("r1", "flint", s, cur, flag);
    assert_eq!(at(Some(&flint), None, None).unwrap().hardware, SubmittingMachine, "the snapshot with no flag");
    let corrected = at(Some(&flint), None, Some(Unknown)).unwrap();
    assert_eq!((corrected.hardware, corrected.corrected), (Unknown, Some(SubmittingMachine)), "the flag corrects the snapshot");
    let undeclared = [snap("flint", HardwareFrom::Endpoint, None)];
    assert_eq!(at(Some(&undeclared), None, Some(SubmittingMachine)).unwrap().hardware, SubmittingMachine, "the flag supplies it");
    let refused = at(Some(&undeclared), None, None).unwrap_err();
    assert!(refused.starts_with("r1: "), "{refused}");
    for says in ["--hardware submitting-machine", "--hardware unknown", r#""hardware""#, "endpoints.studio"] {
        assert!(refused.contains(says), "names what to set ({says}): {refused}");
    }

    // Fixed: a provider host and a managed model, from the snapshot.
    let claude = [snap("flint", HardwareFrom::Provider, Some(Unknown))];
    let by = at(Some(&claude), None, None).unwrap();
    assert_eq!((by.hardware, by.provider.as_deref()), (Unknown, Some("Anthropic API")));
    assert_eq!(at(Some(&claude), None, Some(Unknown)).unwrap().hardware, Unknown, "a flag that agrees is fine");
    let e = at(Some(&claude), None, Some(SubmittingMachine)).unwrap_err();
    assert!(e.contains("Anthropic API") && e.contains("--hardware submitting-machine"), "{e}");
    let managed = [snap("flint", HardwareFrom::Managed, Some(SubmittingMachine))];
    assert!(at(Some(&managed), None, None).unwrap().lmstudio);
    assert!(at(Some(&managed), None, Some(Unknown)).unwrap_err().contains("LM Studio"));

    // Before the snapshot: the registry's provider row or a managed model, never a declaration read now.
    let old_claude = profile_model(json!({"id": "flint", "endpoint": "claude"}));
    assert_eq!(at(None, Some(&old_claude), None).unwrap().provider.as_deref(), Some("Anthropic API"));
    let old_studio = profile_model(json!({"id": "flint", "endpoint": "studio"}));
    assert!(at(None, Some(&old_studio), None).is_err(), "today's `hardware` was not the run's");
    assert_eq!(at(None, Some(&old_studio), Some(Unknown)).unwrap().hardware, Unknown);
    let old_local = profile_model(json!({"id": "flint", "n_ctx": 1}));
    assert!(at(None, Some(&old_local), None).unwrap().lmstudio);
    assert!(at(None, None, None).unwrap().lmstudio, "a model no registry lists reads as LM Studio's, as before");
    // A model matched by its identifier.
    let mut by_identifier = snap("flint-id", HardwareFrom::Endpoint, Some(Unknown));
    by_identifier.identifier = Some("flint".into());
    assert_eq!(at(Some(&[by_identifier]), None, None).unwrap().hardware, Unknown);
}

/// (macOS) The GPU cross-check flags a claim the run's own GPU reading
/// disagrees with, both ways, and says nothing when the reading agrees or
/// when there is none.
#[test]
fn the_gpu_cross_check_flags_a_disagreeing_claim_and_is_silent_without_a_reading() {
    let busy = json!({"samples_busy": 40, "gpu_duty_pct": 87.5});
    let idle = json!({"samples_busy": 0, "samples_idle": 12, "gpu_duty_pct": 0.0});
    let no_gpu = json!({"samples_busy": 3});
    let e = hardware_check(Unknown, &busy).expect("busy while stated elsewhere");
    assert!(e.contains("unknown") && e.contains("87.5%"), "{e}");
    let e = hardware_check(SubmittingMachine, &idle).expect("idle while stated here");
    assert!(e.contains("submitting-machine") && e.contains("idle"), "{e}");
    assert_eq!(hardware_check(SubmittingMachine, &busy), None);
    assert_eq!(hardware_check(Unknown, &idle), None);
    assert_eq!(hardware_check(Unknown, &no_gpu), None, "no GPU reading (Omarchy), no note");
    assert_eq!(hardware_check(SubmittingMachine, &json!({})), None, "no telemetry, no note");
}

/// Format 7 only when a run's hardware is unknown, and then every run says
/// which; otherwise the key is left out and the version is what it was.
/// The URL and host are never in the bundle.
#[test]
fn format_7_only_when_a_runs_hardware_is_unknown() {
    let mut elsewhere = stack();
    elsewhere.hardware = Unknown;
    elsewhere.engine = Some("Anthropic API".into());
    let with_workload = |mut run: Value| {
        run["workloadPath"] = json!("eval/workloads/w.json");
        run["workloadSha256"] = json!("b".repeat(64));
        run
    };
    let far = with_workload(bundle_run("far", &manifest(), &stats(), &RanBy::default(), &elsewhere, &[]));
    let here = with_workload(bundle_run("here", &manifest(), &stats(), &RanBy::default(), &stack(), &[]));
    let fixture = json!({ "name": "webhook-forgery", "version": "1.2" });
    let seven = assemble("kstrat2001/webhook-forgery-eval", &"a".repeat(40), &fixture, vec![far, here.clone()]).unwrap();
    assert_eq!(seven["formatVersion"], 7);
    assert_eq!((&seven["runs"][0]["stack"]["hardware"], &seven["runs"][1]["stack"]["hardware"]), (&json!("unknown"), &json!("submitting-machine")));
    assert!(seven["runs"][1].get("criteria").is_some_and(Value::is_null), "7 carries what 6 does");
    assert_eq!(site_refusals(&seven), Vec::<String>::new());
    let five = assemble("kstrat2001/webhook-forgery-eval", &"a".repeat(40), &fixture, vec![here]).unwrap();
    assert_eq!(five["formatVersion"], 5);
    assert!(five["runs"][0]["stack"].get("hardware").is_none());
}

/// `site_refusals` holds `stack.hardware` to the site's reading: one of the
/// two values in format 7, and no key at all below it.
#[test]
fn the_site_takes_hardware_only_in_format_7_and_only_its_two_values() {
    let mut elsewhere = stack();
    elsewhere.hardware = Unknown;
    let run = bundle_run("far", &manifest(), &stats(), &RanBy::default(), &elsewhere, &[]);
    let fixture = json!({ "name": "pepper-grinder", "version": "2" });
    let mut seven = assemble("kstrat2001/pepper-grinder", &"a".repeat(40), &fixture, vec![run]).unwrap();
    seven["runs"][0]["stack"]["hardware"] = json!("another-machine");
    assert!(site_refusals(&seven).iter().any(|r| r.starts_with("runs[0].stack.hardware")), "{:?}", site_refusals(&seven));
    seven["runs"][0]["stack"].as_object_mut().unwrap().remove("hardware");
    assert!(site_refusals(&seven).iter().any(|r| r.starts_with("runs[0].stack.hardware")), "required in 7");
    seven["formatVersion"] = json!(6);
    seven["runs"][0]["stack"]["hardware"] = json!("unknown");
    assert!(site_refusals(&seven).iter().any(|r| r.starts_with("runs[0].stack.hardware")), "unknown below 7");
    seven["formatVersion"] = json!(8);
    seven["runs"][0]["stack"].as_object_mut().unwrap().remove("hardware");
    assert!(site_refusals(&seven).iter().any(|r| r.starts_with("formatVersion")), "{:?}", site_refusals(&seven));
}

/// End to end: a run on a provider host is sent as format 7 with the
/// provider as its engine and no LM Studio or weights reading; a run on an
/// unlisted endpoint that recorded its hardware keeps it; one that recorded
/// none is refused naming what to set, and submits with --hardware. The
/// host never reaches the bundle.
#[test]
fn a_run_on_an_endpoint_submits_where_it_was_served_and_never_its_host() {
    let b = bench();
    let haiku = b.run("haiku", "claude-haiku-4-5", json!({"model_hardware": [
        {"model": "claude-haiku-4-5", "from": "provider", "hardware": "unknown", "provider": "Anthropic API", "endpoint": "claude"}]}));
    let (built, said) = b.gather(&b.machine(None), std::slice::from_ref(&haiku), &Stated::default()).unwrap();
    assert!(said.is_empty(), "nothing hashed for a provider's model: {said:?}");
    assert_eq!(built.bundle["formatVersion"], 7);
    let stack = &built.bundle["runs"][0]["stack"];
    assert_eq!((stack["hardware"].as_str(), stack["engine"].as_str()), (Some("unknown"), Some("Anthropic API")));
    for k in ["engineBackend", "engineVersion", "quant", "weightsHash"] {
        assert!(stack[k].is_null(), "{k}: {stack}");
    }
    assert_eq!(stack["chip"], "Apple M2", "the submitting machine's fields are still sent");
    assert!(!built.notes[0].contains("pass --engine") && !built.notes[0].contains("--weights-dir"), "{}", built.notes[0]);
    assert!(built.notes.iter().any(|n| n.contains("format 7")), "{:?}", built.notes);
    let text = serde_json::to_string_pretty(&built.bundle).unwrap();
    assert!(!text.contains("anthropic.com") && !text.contains("\"claude\""), "no host and no endpoint id: {text}");

    let mut registry = b.machine(None);
    registry.registry = serde_json::from_value(json!({"profiles": {"p": {"models": [{"id": "flint", "endpoint": "studio"}]}},
        "endpoints": {"studio": {"url": "http://studio.local:8080/v1", "hardware": "submitting-machine"}}})).ok();
    if let Some(r) = registry.registry.as_mut() {
        r.materialize_endpoints();
    }
    let declared = b.run("flint-declared", "flint", json!({"model_hardware": [
        {"model": "flint", "from": "endpoint", "hardware": "unknown", "endpoint": "studio"}]}));
    let (built, _) = b.gather(&registry, std::slice::from_ref(&declared), &Stated::default()).unwrap();
    assert_eq!(built.bundle["runs"][0]["stack"]["hardware"], "unknown", "the snapshot, not today's declaration");
    assert!(!serde_json::to_string(&built.bundle).unwrap().contains("studio"), "no host or endpoint id");

    let old = b.run("flint-old", "flint", json!({}));
    let e = b.gather(&registry, std::slice::from_ref(&old), &Stated::default()).unwrap_err().to_string();
    assert!(e.contains("flint-old: ") && e.contains("--hardware") && e.contains("endpoints.studio"), "{e}");
    let (built, _) = b.gather(&registry, &[old], &Stated { hardware: Some(SubmittingMachine), ..Default::default() }).unwrap();
    assert_eq!(built.bundle["formatVersion"], 5, "all on the submitting machine: no hardware key, the old format");
    assert!(built.bundle["runs"][0]["stack"].get("hardware").is_none());

    let e = b.gather(&b.machine(None), &[haiku], &Stated { engine: Some("rec".into()), ..Default::default() }).unwrap_err().to_string();
    assert!(e.contains("Anthropic API") && e.contains("--engine"), "{e}");
}
