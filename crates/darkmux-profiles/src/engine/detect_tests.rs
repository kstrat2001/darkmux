//! (#3569) Tests for [`super`]: each engine's verdict from what it said, and
//! the bounded asking.

use super::*;
use std::os::unix::fs::PermissionsExt;

fn pulled(revision: &str, max_context: Option<u32>) -> Pulled {
    Pulled {
        revision: revision.into(),
        max_context,
    }
}

fn rec(version: &str, runtime: Said<()>, pulled: Said<Vec<Pulled>>) -> FlintFacts {
    FlintFacts {
        version: Ok(version.into()),
        runtime,
        pulled,
    }
}

fn v(major: u32, minor: u32, patch: u32) -> Version {
    Version {
        major,
        minor,
        patch,
    }
}

/// rec 0.1.0 with a runtime and a pulled revision is usable, names the
/// revision with the context its manifest states, and can be added while no
/// endpoint declares it; once one does, it is configured and cannot.
#[test]
fn rec_with_a_runtime_and_a_pulled_revision_is_usable_and_addable_until_configured() {
    let facts = || {
        rec(
            "0.1.0",
            Ok(()),
            Ok(vec![pulled("flint-int4", Some(262_144))]),
        )
    };
    let found = judge_flint(facts(), Vec::new());
    assert_eq!(found.engine, DetectedKind::Flint);
    assert_eq!(
        (found.state, found.reason.as_deref()),
        (DetectedState::Usable, None)
    );
    assert_eq!(found.version.as_deref(), Some("0.1.0"));
    assert_eq!(
        found.models,
        vec![DetectedModel {
            id: "flint-int4".into(),
            max_context: Some(262_144)
        }]
    );
    assert!(!found.configured && found.can_add, "{found:?}");

    let configured = judge_flint(facts(), vec!["flint".into()]);
    assert!(
        configured.configured && !configured.can_add,
        "{configured:?}"
    );
    assert_eq!(configured.endpoints, ["flint"]);
}

/// rec that is not the verified release, has no runtime, holds no revision,
/// or did not answer is not usable, says why, and is never addable.
#[test]
fn rec_that_cannot_run_a_model_says_why_and_is_not_addable() {
    let cases = [
        (
            rec("0.2.0", Ok(()), Ok(vec![pulled("flint-int4", None)])),
            "rec 0.2.0 is installed; this darkmux supports rec 0.1.0",
        ),
        (
            rec(
                "0.1.0",
                Err("holds no runtime".into()),
                Ok(vec![pulled("flint-int4", None)]),
            ),
            "holds no runtime",
        ),
        (
            rec("0.1.0", Ok(()), Ok(Vec::new())),
            "`rec pull` downloads one",
        ),
        (
            rec("0.1.0", Ok(()), Err("no home directory".into())),
            "no home directory",
        ),
        (
            FlintFacts {
                version: Err("exited with 1".into()),
                runtime: Ok(()),
                pulled: Ok(Vec::new()),
            },
            "did not report its version",
        ),
    ];
    for (facts, says) in cases {
        let found = judge_flint(facts, Vec::new());
        assert_eq!(found.state, DetectedState::Unusable, "{says}: {found:?}");
        assert!(
            found.reason.as_deref().is_some_and(|r| r.contains(says)),
            "{says}: {found:?}"
        );
        assert!(
            !found.can_add && found.models.is_empty(),
            "{says}: {found:?}"
        );
    }
}

/// A probe killed at its bound is `unknown`, never `unusable`.
#[test]
fn a_probe_that_ran_out_of_time_is_unknown() {
    let timed_out = format!(
        "`rec --version` failed: {} (waited 3s)",
        darkmux_gestalt::HostError::TIMED_OUT
    );
    let found = judge_flint(
        FlintFacts {
            version: Err(timed_out),
            runtime: Ok(()),
            pulled: Ok(Vec::new()),
        },
        Vec::new(),
    );
    assert_eq!(found.state, DetectedState::Unknown, "{found:?}");
    assert!(!found.can_add);
}

/// Tensorfold 1.2.x that lists a model is usable and addable; another
/// release, an empty or failed listing is not, naming why.
#[test]
fn tensorfold_is_usable_only_at_a_verified_release_listing_a_model() {
    let tf = |version, models| TensorfoldFacts { version, models };
    let found = judge_tensorfold(
        tf(
            Ok(v(1, 2, 5)),
            Ok(vec!["TensorFold/Qwen3.8-27B-MLX-4bit".into()]),
        ),
        Vec::new(),
    );
    assert_eq!(
        (found.state, found.version.as_deref()),
        (DetectedState::Usable, Some("1.2.5"))
    );
    assert_eq!(
        found.models,
        vec![DetectedModel {
            id: "TensorFold/Qwen3.8-27B-MLX-4bit".into(),
            max_context: None
        }]
    );
    assert!(found.can_add);
    for (facts, says) in [
        (
            tf(Ok(v(1, 3, 0)), Ok(vec!["m".into()])),
            "Tensorfold 1.3.0 is installed; this darkmux supports Tensorfold 1.2.x",
        ),
        (tf(Ok(v(1, 2, 5)), Ok(Vec::new())), "lists no model"),
        (
            tf(Ok(v(1, 2, 5)), Err("schema 2".into())),
            "could not list its models: schema 2",
        ),
        (
            tf(Err("exited with 2".into()), Ok(vec!["m".into()])),
            "did not report its version",
        ),
    ] {
        let found = judge_tensorfold(facts, Vec::new());
        assert_eq!(found.state, DetectedState::Unusable, "{says}");
        assert!(
            found.reason.as_deref().is_some_and(|r| r.contains(says)),
            "{says}: {found:?}"
        );
        assert!(!found.can_add, "{says}");
    }
}

/// LM Studio needs no endpoint: it is configured as found, usable when it
/// lists a downloaded model, and never addable. A failed listing is
/// `unknown` (it is not answering), an empty one `unusable`.
#[test]
fn lm_studio_is_configured_as_found_and_usable_with_a_downloaded_model() {
    let found = judge_lmstudio(
        Some("CLI commit: abc".into()),
        Ok(vec!["qwen3.8-27b".into()]),
    );
    assert_eq!(found.state, DetectedState::Usable);
    assert!(
        found.configured && !found.can_add && found.endpoints.is_empty(),
        "{found:?}"
    );
    assert_eq!(
        judge_lmstudio(None, Ok(Vec::new())).state,
        DetectedState::Unusable
    );
    let quiet = judge_lmstudio(None, Err("LM Studio is not running".into()));
    assert_eq!(quiet.state, DetectedState::Unknown);
    assert!(quiet.reason.unwrap().contains("not running"));
}

/// The default engine's server is configured (it is an endpoint), usable
/// when it answers with a model, and unusable when it does not answer.
#[test]
fn the_default_engines_server_is_usable_when_it_answers_with_a_model() {
    let up = judge_url("mlx".into(), Ok(vec!["mlx-community/Qwen3.6".into()]));
    assert_eq!(
        (up.state, up.configured, up.can_add),
        (DetectedState::Usable, true, false)
    );
    assert_eq!(up.endpoints, ["mlx"]);
    let down = judge_url("mlx".into(), Err("connection refused".into()));
    assert_eq!(down.state, DetectedState::Unusable);
    assert!(down
        .reason
        .unwrap()
        .contains("`mlx` did not answer `GET /models`: connection refused"));
    assert_eq!(
        judge_url("mlx".into(), Ok(Vec::new())).state,
        DetectedState::Unusable
    );
}

/// `usable` is any engine found being usable: none installed, or only
/// unusable or unknown ones, is `false`.
#[test]
fn usable_means_some_engine_found_is_usable() {
    assert!(!Detected::of(Vec::new()).usable, "nothing installed");
    let unknown = judge_lmstudio(None, Err("x".into()));
    let unusable = judge_flint(rec("0.1.0", Ok(()), Ok(Vec::new())), Vec::new());
    assert!(!Detected::of(vec![unknown.clone(), unusable.clone()]).usable);
    let good = judge_flint(
        rec("0.1.0", Ok(()), Ok(vec![pulled("flint-int4", None)])),
        Vec::new(),
    );
    assert!(Detected::of(vec![unknown, unusable, good]).usable);
}

/// The endpoints that declare an engine are read from the registry, and an
/// endpoint of another kind is not one of them.
#[test]
fn configured_means_an_endpoint_declares_the_engine() {
    let registry: ProfileRegistry = crate::profiles::parse_registry_lenient(
        r#"{"profiles": {}, "endpoints": {
            "fl": {"managed": "flint", "port": 8092},
            "tf": {"managed": "tensorfold", "port": 8091},
            "api": {"url": "https://api.example.com/v1"}
        }}"#,
    )
    .unwrap();
    assert_eq!(declaring(Some(&registry), ManagedBackend::Flint), ["fl"]);
    assert_eq!(
        declaring(Some(&registry), ManagedBackend::Tensorfold),
        ["tf"]
    );
    assert!(
        declaring(None, ManagedBackend::Flint).is_empty(),
        "no registry, nothing configured"
    );
}

/// A bare name is installed when a `PATH` directory holds it; a path, when
/// it is a file.
#[test]
#[serial_test::serial]
fn installed_is_a_file_at_the_path_or_on_path() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("rec-probe-test");
    std::fs::write(&bin, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(installed(&bin.to_string_lossy()));
    assert!(!installed(&dir.path().join("missing").to_string_lossy()));
    let before = std::env::var_os("PATH");
    // SAFETY: serial; restored below.
    unsafe { std::env::set_var("PATH", dir.path()) };
    let (named, missing) = (
        installed("rec-probe-test"),
        installed("no-such-engine-binary"),
    );
    match before {
        Some(p) => unsafe { std::env::set_var("PATH", p) },
        None => unsafe { std::env::remove_var("PATH") },
    }
    assert!(named && !missing);
}

/// Every question is asked at once and each answer lands in its place; one
/// that does not answer by its bound and the grace is `unknown`, naming the
/// bound, and does not hold up the rest.
#[test]
fn an_engine_that_does_not_answer_in_time_is_unknown_and_holds_up_nothing() {
    let quick = |kind| -> Question {
        (
            kind,
            Duration::from_millis(200),
            Box::new(move || judge_lmstudio(None, Ok(vec!["m".into()]))),
        )
    };
    let slow: Question = (
        DetectedKind::Flint,
        Duration::from_millis(100),
        Box::new(|| {
            std::thread::sleep(Duration::from_secs(5));
            judge_flint(rec("0.1.0", Ok(()), Ok(Vec::new())), Vec::new())
        }),
    );
    let start = Instant::now();
    let answers = ask_all(vec![quick(DetectedKind::Lmstudio), slow]);
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "waited {:?}",
        start.elapsed()
    );
    assert_eq!(answers[0].state, DetectedState::Usable);
    assert_eq!(
        (answers[1].engine, answers[1].state),
        (DetectedKind::Flint, DetectedState::Unknown)
    );
    assert!(
        answers[1]
            .reason
            .as_deref()
            .unwrap()
            .contains("did not answer within 0 s"),
        "{:?}",
        answers[1]
    );
}

/// The wire shape: snake-case kinds and states, a model's `max_context`
/// only when stated, and a state a newer darkmux sends read as `unknown`.
#[test]
fn the_wire_shape_is_snake_case_and_reads_a_newer_state_as_unknown() {
    let found = judge_flint(
        rec(
            "0.1.0",
            Ok(()),
            Ok(vec![pulled("flint-int4", Some(8192)), pulled("x", None)]),
        ),
        Vec::new(),
    );
    let json = serde_json::to_value(Detected::of(vec![found])).unwrap();
    assert_eq!(json["usable"], true);
    assert_eq!(json["detected"][0]["engine"], "flint");
    assert_eq!(json["detected"][0]["state"], "usable");
    assert_eq!(
        json["detected"][0]["models"][0],
        serde_json::json!({"id": "flint-int4", "max_context": 8192})
    );
    assert_eq!(
        json["detected"][0]["models"][1],
        serde_json::json!({"id": "x"})
    );
    let newer: DetectedState = serde_json::from_str("\"installing\"").unwrap();
    assert_eq!(newer, DetectedState::Unknown);
}

/// Sets environment variables for one test and puts them back.
struct Env(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl Env {
    fn set(&mut self, var: &'static str, value: impl AsRef<std::ffi::OsStr>) {
        self.0.push((var, std::env::var_os(var)));
        // SAFETY: the test holds #[serial_test::serial].
        unsafe { std::env::set_var(var, value) };
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for (var, before) in self.0.drain(..).rev() {
            // SAFETY: as in `set`.
            unsafe {
                match before {
                    Some(v) => std::env::set_var(var, v),
                    None => std::env::remove_var(var),
                }
            }
        }
    }
}

fn script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The whole detection on a machine of fakes: LM Studio with a downloaded
/// model, Tensorfold 1.2.5 listing one, rec 0.1.0 with a pulled revision
/// and an endpoint declaring it, and a default engine whose server does not
/// answer. Each is asked only read-only questions, and lands in order.
#[test]
#[serial_test::serial]
fn every_installed_engine_is_asked_and_listed_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut env = Env(Vec::new());
    let lms = script(
        dir.path(),
        "lms",
        r#"echo "$*" >> "$(dirname "$0")/asked.log"
case "$1" in
  ls) echo '[{"modelKey":"qwen3.8-27b","sizeBytes":1,"type":"llm"},{"modelKey":"nomic-embed","sizeBytes":1,"type":"embedding"}]' ;;
  --version) echo 'CLI commit: abc123' ;;
  *) exit 3 ;;
esac"#,
    );
    let tf = script(
        dir.path(),
        "tensorfold-native",
        r#"echo "$*" >> "$(dirname "$0")/asked.log"
case "$1" in
  --version) echo 'tensorfold-native 1.2.5' ;;
  models) echo '{"schema":1,"models":[{"repo":"TensorFold/Qwen3.8-27B-MLX-4bit"}]}' ;;
  *) exit 3 ;;
esac"#,
    );
    let rec = script(
        dir.path(),
        "rec-bin",
        r#"echo "$*" >> "$(dirname "$0")/asked.log"; [ "$1" = --version ] && echo 'rec 0.1.0' || exit 9"#,
    );
    let data = dir.path().join("share");
    std::fs::create_dir_all(data.join("rec/runtimes/0906aa")).unwrap();
    std::fs::write(data.join("rec/runtimes/0906aa/rec-mlx"), "").unwrap();
    std::fs::create_dir_all(data.join("rec/models/fdc852")).unwrap();
    std::fs::write(
        data.join("rec/models/fdc852/flint-int4.json"),
        r#"{"model_id":"flint-int4","max_context":262144}"#,
    )
    .unwrap();
    let registry = dir.path().join("profiles.json");
    std::fs::write(
        &registry,
        r#"{"profiles": {}, "endpoints": {"fl": {"managed": "flint", "port": 8092}, "gone": {"url": "http://127.0.0.1:9/v1"}}}"#,
    )
    .unwrap();
    env.set("DARKMUX_LMS_BIN", &lms);
    env.set("DARKMUX_TENSORFOLD_BIN", &tf);
    env.set("DARKMUX_FLINT_BIN", &rec);
    env.set("XDG_DATA_HOME", &data);
    env.set("DARKMUX_PROFILES", &registry);
    env.set("DARKMUX_DEFAULT_ENGINE", "gone");

    let loaded =
        crate::profiles::parse_registry_lenient(&std::fs::read_to_string(&registry).unwrap())
            .unwrap();
    // The bound each engine is waited for is not what this test is about
    // (`an_engine_that_does_not_answer_in_time_is_unknown_...` is), so a
    // loaded machine starting freshly written fakes cannot run it out.
    let unhurried = |_: ManagedBackend| Duration::from_secs(60);
    let ask = |servers_only| ask_all(questions(Some(&loaded), servers_only, &unhurried));
    let found = Detected::of(ask(false));
    let kinds: Vec<DetectedKind> = found.detected.iter().map(|d| d.engine).collect();
    assert_eq!(
        kinds,
        [
            DetectedKind::Lmstudio,
            DetectedKind::Tensorfold,
            DetectedKind::Flint,
            DetectedKind::Url
        ]
    );
    let [lmstudio, tensorfold, flint, url] = &found.detected[..] else {
        unreachable!()
    };
    assert_eq!(
        lmstudio.models,
        plain(vec!["qwen3.8-27b".into()]),
        "language models only"
    );
    assert_eq!(
        (
            tensorfold.state,
            tensorfold.version.as_deref(),
            tensorfold.can_add
        ),
        (DetectedState::Usable, Some("1.2.5"), true)
    );
    assert_eq!(
        (flint.state, flint.configured, flint.can_add),
        (DetectedState::Usable, true, false)
    );
    assert_eq!(
        (url.state, url.endpoints.as_slice()),
        (DetectedState::Unusable, &["gone".to_string()][..]),
        "{url:?}"
    );
    assert!(found.usable);

    let servers: Vec<DetectedKind> = ask(true).iter().map(|d| d.engine).collect();
    assert_eq!(
        servers,
        [DetectedKind::Tensorfold, DetectedKind::Flint],
        "the doctor's question leaves LM Studio and the server out"
    );

    let asked = std::fs::read_to_string(dir.path().join("asked.log")).unwrap();
    for line in asked.lines() {
        let read_only = ["ls", "--version", "models"]
            .iter()
            .any(|q| line.starts_with(q));
        assert!(
            read_only,
            "only read-only questions are asked: {line:?}\n{asked}"
        );
    }

    env.set("DARKMUX_LMS_BIN", dir.path().join("no-such-lms"));
    env.set(
        "DARKMUX_TENSORFOLD_BIN",
        dir.path().join("no-such-tensorfold"),
    );
    env.set("DARKMUX_FLINT_BIN", dir.path().join("no-such-rec"));
    env.set("DARKMUX_DEFAULT_ENGINE", "lmstudio");
    let nothing = Detected::of(ask(false));
    assert!(
        nothing.detected.is_empty() && !nothing.usable,
        "nothing installed: {nothing:?}"
    );
    assert!(detect(Some(&loaded)).detected.is_empty() && detect_servers(None).is_empty());
}

/// Every `managed` backend darkmux starts servers for is an engine
/// detection looks for and an add can declare, and no other is: a new one
/// fails here until detection learns it.
#[test]
fn every_engine_darkmux_starts_servers_for_is_detected_and_addable() {
    use darkmux_types::config_enum::ConfigEnum;
    let kinds = [DetectedKind::Lmstudio, DetectedKind::Tensorfold, DetectedKind::Flint, DetectedKind::Url];
    for token in ManagedBackend::TOKENS {
        let backend = ManagedBackend::from_token(token).unwrap();
        let detected = kinds.iter().find(|k| k.startable() == Some(backend));
        let starts = darkmux_types::EndpointKind::Managed(backend).starts_servers();
        assert_eq!(detected.is_some(), starts, "`{token}`: detected and addable exactly when darkmux starts its servers");
    }
}
