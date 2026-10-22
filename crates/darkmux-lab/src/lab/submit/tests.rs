//! `lab submit` against the site's own reading. `golden/site_bundle_v3.json`
//! is what darklyenergized's `lab_run.ts` (`bundleRun`, `assembleBundle`,
//! `weightsHashOf`, `looksLikePath`) produced for the inputs below, and what
//! its `parseBundle` accepted (`ok`). A change on either side that drifts
//! from the other fails here.

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
    let run = bundle_run("pepper-grinder-p-1761120000-1", &manifest(), &stats(), &ran_by, &stack());
    let bundle = assemble(
        "kstrat2001/pepper-grinder",
        &"a".repeat(40),
        &json!({ "name": "pepper-grinder", "version": "2.0", "verify_command": "pytest" }),
        vec![run],
    )
    .unwrap();
    same(&bundle, &g["bundle"], "bundle");
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
    let run = bundle_run("r", &manifest(), &s, &RanBy::default(), &stack());
    assert_eq!(run["tokensPerSecond"], Value::Null);
    assert_eq!(run["darkmuxVersion"], Value::Null);
}

#[test]
fn host_figures_need_busy_samples() {
    let mut s = stats();
    s["samples_busy"] = json!(0);
    let d = detail_from(&manifest(), &s);
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
    assert_eq!(commit_at_time(&reflog, 2_100_000, 2_100_000), Ok(b));
    assert!(commit_at_time(&reflog, 1_500_000, 2_100_000).unwrap_err().contains("moved between"));
    assert!(commit_at_time(&reflog, 500_000, 600_000).unwrap_err().contains("--commit"));
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
    let cut = shareable(Some(&json!(long)), 500);
    assert_eq!(cut.as_str().unwrap().chars().count(), 500);
    assert!(cut.as_str().unwrap().ends_with('\u{2026}'));
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

#[test]
fn the_fixture_is_read_at_the_commit_not_as_it_is_now() {
    let tmp = tempfile::TempDir::new().unwrap();
    let d = tmp.path();
    git(d, &["init", "-q"]);
    std::fs::write(d.join(".fixture.json"), r#"{"name":"pepper-grinder","version":"2.0"}"#).unwrap();
    git(d, &["add", "."]);
    git(d, &["commit", "-qm", "one"]);
    let first = String::from_utf8(Command::new("git").arg("-C").arg(d).args(["rev-parse", "HEAD"]).output().unwrap().stdout).unwrap();
    std::fs::write(d.join(".fixture.json"), r#"{"name":"pepper-grinder","version":"3.0"}"#).unwrap();
    git(d, &["commit", "-qam", "two"]);
    git(d, &["remote", "add", "origin", "git@github.com:kstrat2001/pepper-grinder.git"]);

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
    let good = bundle_run("r1", &manifest(), &stats(), &ran_by, &stack());
    let fixture = json!({ "name": "pepper-grinder", "version": 2.0 });
    let ok = assemble("kstrat2001/pepper-grinder", &"a".repeat(40), &fixture, vec![good.clone()]).unwrap();
    assert_eq!(site_refusals(&ok), Vec::<String>::new());
    assert_eq!(ok["fixture"]["version"], "2", "a number version reads as the site's JavaScript writes it");

    let mut vm = stack();
    vm.chip = "Apple M2 (Virtual)".into();
    vm.quant = Some("q".repeat(41));
    let bad_ran = RanBy { version: Some("5.4.0 beta".into()), ..Default::default() };
    let bad = bundle_run("r2", &manifest(), &stats(), &bad_ran, &vm);
    let both = assemble("kstrat2001/pepper-grinder", &"a".repeat(40), &fixture, vec![bad, good.clone(), good]).unwrap();
    let refused = site_refusals(&both);
    for want in ["runs[0].stack.chip", "runs[0].stack.quant", "runs[0].darkmuxVersion", "runs[2]: the same run twice"] {
        assert!(refused.iter().any(|r| r.starts_with(want)), "{want} in {refused:?}");
    }
}

#[test]
fn a_word_boundary_is_ascii_as_in_javascript() {
    assert!(looks_like_path("see /var\u{e9}"), "the site reads é as a boundary");
}
