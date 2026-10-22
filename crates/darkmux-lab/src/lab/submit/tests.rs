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
        cpu_cores: vec![("Super".into(), 6), ("Performance".into(), 12)],
        gpu_cores: Some(40),
        model_id: Some("Mac17,7".into()),
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

fn bench() -> Bench {
    let root = tempfile::tempdir().unwrap();
    let fixture = root.path().join("fixture");
    std::fs::create_dir(&fixture).unwrap();
    git(&fixture, &["init", "-q"]);
    std::fs::write(fixture.join(".fixture.json"), r#"{"name":"pepper-grinder","version":2.0}"#).unwrap();
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
            "fixture": {"source_path": self.fixture.display().to_string(), "baseline_hash": "b"},
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
    assert_eq!(Stack::default().unreported().len(), 13);
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
