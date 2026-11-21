//! (#3466) Availability: each engine's answer, judged per model, and the
//! listing that asks each engine once within its bound.

use super::*;
use crate::engine::tensorfold::{parse_capabilities, parse_version, Version};
use darkmux_types::engine_record::{self, EngineRecord, ENGINE_RECORD_SCHEMA};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const TF_MODEL: &str = "TensorFold/Qwen3.8-27B-MLX-4bit";

/// The profile models of a registry, endpoints materialized as the loader
/// does, in profile-name order.
fn models(json: &str) -> Vec<ProfileModel> {
    let mut r: darkmux_types::ProfileRegistry = serde_json::from_str(json).unwrap();
    r.materialize_endpoints();
    r.profiles.into_values().flat_map(|p| p.models).collect()
}

/// One Tensorfold model on endpoint `tf` at `port`.
fn tf_model(port: u16, engine_args: &str) -> ProfileModel {
    models(&format!(
        r#"{{"profiles":{{"q":{{"models":[{{"id":"{TF_MODEL}","n_ctx":8192,"endpoint":"tf","engine_args":{engine_args}}}]}}}},
            "endpoints":{{"tf":{{"managed":"tensorfold","port":{port}}}}}}}"#
    ))
    .remove(0)
}

// A port nothing listens on, held for this test alone (#3488).
use darkmux_types::test_ports::fixture_port as free_port;

fn facts(version: &str, models: &[&str]) -> TensorfoldProbe {
    TensorfoldProbe {
        version: parse_version(version).map_err(|e| e.to_string()),
        flags: Ok(
            parse_capabilities(r#"{"schema":1,"serve":{"flags":{"--drafter":{}}}}"#).unwrap(),
        ),
        models: Ok(models.iter().map(|s| s.to_string()).collect()),
    }
}

fn tf_answer(facts: TensorfoldProbe, dirs: &tempfile::TempDir) -> TensorfoldAnswer {
    TensorfoldAnswer {
        tf: Tensorfold::at("/opt/tf/tensorfold-native"),
        facts,
        dirs: Dirs {
            records: dirs.path().join("run"),
            logs: dirs.path().join("logs"),
        },
    }
}

fn state(a: &Availability) -> AvailabilityState {
    a.state
}

/// A stopped Tensorfold server reads `startable` (the engine is verified,
/// has the model, takes the args, the port is free), so an app keeps it
/// among its choices.
#[test]
fn a_stopped_tensorfold_server_is_startable() {
    let dir = tempfile::tempdir().unwrap();
    let a = tf_answer(facts("tensorfold-native 1.2.4", &[TF_MODEL]), &dir)
        .judge(&tf_model(free_port(), r#"["--drafter","z/d"]"#));
    assert_eq!(a, Availability::startable());
}

/// An unverified engine version is `unavailable`, naming both versions.
#[test]
fn an_unverified_tensorfold_is_unavailable_with_its_version() {
    let dir = tempfile::tempdir().unwrap();
    let a = tf_answer(facts("tensorfold-native 1.3.0", &[TF_MODEL]), &dir)
        .judge(&tf_model(free_port(), "[]"));
    assert_eq!(state(&a), AvailabilityState::Unavailable, "{a:?}");
    let why = a.reason.unwrap();
    assert!(
        why.contains("1.3.0") && why.contains(VERIFIED_RELEASES),
        "{why}"
    );
}

/// A missing binary is `unavailable`; a probe that failed after the binary
/// answered is `unknown`, never `unavailable`.
#[test]
fn a_missing_binary_is_unavailable_and_a_failed_listing_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let mut gone = facts("tensorfold-native 1.2.4", &[TF_MODEL]);
    gone.version =
        Err("`tensorfold-native --version` failed: `tensorfold-native` was not found".into());
    let a = tf_answer(gone, &dir).judge(&tf_model(free_port(), "[]"));
    assert_eq!(state(&a), AvailabilityState::Unavailable, "{a:?}");
    assert!(a.reason.unwrap().contains("was not found"));

    let mut no_list = facts("tensorfold-native 1.2.4", &[]);
    no_list.models = Err("`tensorfold-native models --json` exited with 1: boom".into());
    let a = tf_answer(no_list, &dir).judge(&tf_model(free_port(), "[]"));
    assert_eq!(state(&a), AvailabilityState::Unknown, "{a:?}");
    assert!(a.reason.unwrap().contains("could not list its models"));

    let mut no_flags = facts("tensorfold-native 1.2.4", &[TF_MODEL]);
    no_flags.flags = Err("`tensorfold-native capabilities --json` timed out".into());
    let a =
        tf_answer(no_flags.clone(), &dir).judge(&tf_model(free_port(), r#"["--drafter","z/d"]"#));
    assert_eq!(state(&a), AvailabilityState::Unknown, "{a:?}");
    let a = tf_answer(no_flags, &dir).judge(&tf_model(free_port(), "[]"));
    assert_eq!(
        a,
        Availability::startable(),
        "no engine_args, so the flags are not needed"
    );
}

/// A model the engine does not have, args it does not take, or a port held
/// by a server darkmux did not start: `unavailable`, each saying which.
#[test]
fn a_missing_model_bad_args_or_a_foreign_port_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let a = tf_answer(facts("tensorfold-native 1.2.4", &["other/model"]), &dir)
        .judge(&tf_model(free_port(), "[]"));
    assert_eq!(state(&a), AvailabilityState::Unavailable);
    assert!(a.reason.unwrap().contains("does not list"));

    let a = tf_answer(facts("tensorfold-native 1.2.4", &[TF_MODEL]), &dir)
        .judge(&tf_model(free_port(), r#"["--turbo"]"#));
    assert_eq!(state(&a), AvailabilityState::Unavailable);
    assert!(a.reason.unwrap().contains("--turbo"));

    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let a =
        tf_answer(facts("tensorfold-native 1.2.4", &[TF_MODEL]), &dir).judge(&tf_model(port, "[]"));
    assert_eq!(state(&a), AvailabilityState::Unavailable, "{a:?}");
    assert!(a.reason.unwrap().contains(&format!("127.0.0.1:{port}")));
}

/// darkmux's server up and serving this model's command is not `resident`
/// when a dispatch would refuse it anyway, `engine_unavailable`: the binary
/// is now a release darkmux was not verified against (after an upgrade), is
/// missing, or no longer takes the model's `engine_args`. A listing reads
/// what a dispatch would do.
#[test]
fn darkmuxs_server_up_is_not_resident_when_a_dispatch_would_refuse_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let port = serving(&format!("darkmux:{TF_MODEL}"));
    let model = tf_model(port, r#"["--turbo"]"#);
    let plain = tf_model(port, "[]");
    for (version, m, says) in [
        ("tensorfold-native 1.3.0", &plain, "1.3.0"),
        ("", &plain, "did not answer"),
        ("tensorfold-native 1.2.4", &model, "--turbo"),
    ] {
        let answer = tf_answer(facts(version, &[TF_MODEL]), &dir);
        let (argv, _) = answer.tf.serve_argv(port, m).unwrap();
        let _ = std::fs::remove_dir_all(&answer.dirs.records);
        record(&answer.dirs, port, argv);
        let a = answer.judge(m);
        assert_eq!(a.state, AvailabilityState::Unavailable, "{version:?}: {a:?}");
        assert!(a.reason.as_deref().unwrap_or_default().contains(says), "{version:?}: {a:?}");
    }
}

/// darkmux's own Tensorfold server whose leader exited while a worker in
/// its group still serves the port is darkmux's, not "something darkmux did
/// not start": a dispatch clears or reuses it, so the model is not
/// `unavailable` for its port.
#[test]
fn darkmuxs_server_with_a_gone_leader_is_not_read_as_someone_elses() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let script = format!(
        "import os, socket, time\nif os.fork() == 0:\n    s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)\n    s.bind(('127.0.0.1', {port})); s.listen(8)\n    time.sleep(30); os._exit(0)\ntime.sleep(1)\n"
    );
    let mut cmd = std::process::Command::new("python3");
    cmd.args(["-c", &script]);
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut leader = cmd.spawn().unwrap();
    let pid = leader.id();
    let stamp = darkmux_platform::current().process_start_stamp(pid).expect("the leader's start stamp");
    leader.wait().unwrap();
    struct KillGroup(u32);
    impl Drop for KillGroup {
        fn drop(&mut self) {
            let _ = darkmux_types::child_registry::kill_group(self.0, darkmux_types::child_registry::SIGKILL);
        }
    }
    let _guard = KillGroup(pid);
    let until = std::time::Instant::now() + Duration::from_secs(10);
    while !server::port_taken(port) && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
    let model = tf_model(port, "[]");
    let answer = tf_answer(facts("tensorfold-native 1.2.4", &[TF_MODEL]), &dir);
    let (argv, _) = answer.tf.serve_argv(port, &model).unwrap();
    let rec = EngineRecord {
        schema: ENGINE_RECORD_SCHEMA,
        endpoint: "tf".into(),
        engine: "tensorfold".into(),
        model: TF_MODEL.into(),
        n_ctx: 8192,
        port,
        pid,
        start_stamp: stamp,
        owner_pid: std::process::id(),
        argv,
        log: answer.dirs.logs.join("tf.log"),
        started_at_ms: 0,
        engine_version: "tensorfold-native 1.2.4".into(),
    };
    engine_record::write_in(&answer.dirs.records, &rec).unwrap();
    let a = answer.judge(&model);
    assert_ne!(a.state, AvailabilityState::Unavailable, "{a:?}");
    assert!(!a.reason.as_deref().unwrap_or_default().contains("did not start"), "{a:?}");
}

/// A server that answers `GET /v1/models` with `name`, forever.
fn serving(name: &str) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let body = format!(r#"{{"object":"list","data":[{{"id":"{name}"}}]}}"#);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            use std::io::{Read, Write};
            let Ok(mut s) = stream else { continue };
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let _ = write!(s, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
        }
    });
    port
}

/// darkmux's record for a server on `port`, naming this test process (so it
/// is alive and matches its start stamp).
fn record(dirs: &Dirs, port: u16, argv: Vec<String>) {
    record_for(dirs, "tf", "tensorfold", port, argv);
}

/// darkmux's record for an `engine` server on `endpoint` at `port`, naming
/// this test process (so it is alive, matches its start stamp, and is what
/// listens on a port the test itself bound).
fn record_for(dirs: &Dirs, endpoint: &str, engine: &str, port: u16, argv: Vec<String>) {
    let pid = std::process::id();
    let rec = EngineRecord {
        schema: ENGINE_RECORD_SCHEMA,
        endpoint: endpoint.into(),
        engine: engine.into(),
        model: TF_MODEL.into(),
        n_ctx: 8192,
        port,
        pid,
        start_stamp: darkmux_platform::current()
            .process_start_stamp(pid)
            .unwrap(),
        owner_pid: pid,
        argv,
        log: dirs.logs.join("tf.log"),
        started_at_ms: 0,
        engine_version: "tensorfold-native 1.2.4".into(),
    };
    engine_record::write_in(&dirs.records, &rec).unwrap();
}

/// darkmux's server running this model's exact command and answering is
/// `resident`, even when the engine no longer lists the model; one running
/// another command holds the port as darkmux's own, so the model is
/// `startable`.
#[test]
fn darkmuxs_running_server_is_resident_and_its_port_is_no_obstacle() {
    let dir = tempfile::tempdir().unwrap();
    let port = serving(&format!("darkmux:{TF_MODEL}"));
    let model = tf_model(port, "[]");
    let mut answer = tf_answer(facts("tensorfold-native 1.2.4", &[]), &dir);
    let (argv, _) = answer.tf.serve_argv(port, &model).unwrap();
    record(&answer.dirs, port, argv.clone());
    assert_eq!(answer.judge(&model), Availability::resident());

    let mut other = argv;
    other.push("--keep-warm".into());
    std::fs::remove_dir_all(&answer.dirs.records).unwrap();
    record(&answer.dirs, port, other);
    answer.facts = facts("tensorfold-native 1.2.4", &[TF_MODEL]);
    assert_eq!(
        answer.judge(&model),
        Availability::startable(),
        "darkmux's own server holds the port: it replaces it"
    );
    assert!(
        engine_record::read_in(&answer.dirs.records, "tf")
            .unwrap()
            .is_some(),
        "a listing removes no record"
    );
}

fn lm(identifier: &str, model: &str, context: u64) -> LoadedModel {
    LoadedModel {
        identifier: identifier.into(),
        model: model.into(),
        status: "idle".into(),
        size: "1 GB".into(),
        context,
        queued: None,
    }
}

/// A catalog row LM Studio knows only by its key.
fn meta(key: &str) -> crate::engine::ModelMeta {
    crate::engine::ModelMeta {
        model_key: key.into(),
        display_name: String::new(),
        publisher: String::new(),
        size_bytes: 0,
        params_string: None,
        architecture: None,
        max_context_length: None,
        trained_for_tool_use: false,
        model_type: "llm".into(),
        load_names: Vec::new(),
    }
}

fn lms_model() -> ProfileModel {
    models(r#"{"profiles":{"p":{"models":[{"id":"qwen","n_ctx":32768}]}}}"#).remove(0)
}

/// LM Studio: darkmux's own instance at the profile's context is
/// `resident`; an undersized one, or a copy someone else loaded, leaves the
/// model `startable` when it is downloaded and `unavailable` when not.
#[test]
fn lm_studio_judges_by_darkmuxs_own_instance_and_its_catalog() {
    let answer = |loaded: Vec<LoadedModel>, catalog: &[&str]| LmStudioAnswer {
        server_up: true,
        server_startable: false,
        loaded: Ok(loaded),
        catalog: Ok(catalog.iter().map(|s| meta(s)).collect()),
    };
    let m = lms_model();
    assert_eq!(
        answer(vec![lm("darkmux:qwen", "qwen", 65536)], &[]).judge(&m),
        Availability::resident()
    );
    assert_eq!(
        answer(vec![lm("darkmux:qwen", "qwen", 4096)], &["qwen"]).judge(&m),
        Availability::startable()
    );
    assert_eq!(
        answer(vec![lm("qwen", "qwen", 65536)], &["qwen"]).judge(&m),
        Availability::startable(),
        "a user's copy is not darkmux's"
    );
    let none = answer(vec![], &["other"]).judge(&m);
    assert_eq!(state(&none), AvailabilityState::Unavailable);
    assert!(none.reason.unwrap().contains("`qwen`"));
}

/// LM Studio's app answering with its local server off (models loaded, a
/// dispatch refused at connect) is `unavailable`, saying the server is off,
/// even for darkmux's own resident instance (#3578).
#[test]
fn lm_studio_with_its_server_off_is_unavailable() {
    let off = LmStudioAnswer {
        server_up: false,
        server_startable: false,
        loaded: Ok(vec![lm("darkmux:qwen", "qwen", 65536)]),
        catalog: Ok(vec![meta("qwen")]),
    };
    let a = off.judge(&lms_model());
    assert_eq!(state(&a), AvailabilityState::Unavailable, "{a:?}");
    assert!(a.reason.unwrap().starts_with("LM Studio's local server is off"));
    // On this machine darkmux starts it for the dispatch: `startable`.
    let here = LmStudioAnswer { server_startable: true, ..off };
    assert_eq!(here.judge(&lms_model()), Availability::startable());
}

/// Only a server on this machine is started.
#[test]
fn only_a_loopback_lm_studio_is_started() {
    assert!(crate::lms::is_loopback_url("http://localhost:1234"));
    assert!(crate::lms::is_loopback_url("localhost:1234"));
    assert!(crate::lms::is_loopback_url("http://127.0.0.1:1234/v1"));
    assert!(crate::lms::is_loopback_url("http://[::1]:1234"));
    assert!(!crate::lms::is_loopback_url("http://192.0.2.10:1234"));
    assert!(!crate::lms::is_loopback_url("http://example-host:1234"));
}

/// A base URL's server is found by host and port, with or without a scheme.
#[test]
fn server_accepts_reads_the_base_url() {
    let open = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = open.local_addr().unwrap().port();
    assert!(server_accepts(&format!("http://127.0.0.1:{port}")));
    assert!(server_accepts(&format!("localhost:{port}/v1")));
    drop(open);
    assert!(!server_accepts(&format!("http://127.0.0.1:{port}")));
    assert!(!server_accepts("http://"));
}

/// LM Studio not answering is `unknown`, never `unavailable`.
#[test]
fn lm_studio_down_is_unknown() {
    let down = LmStudioAnswer {
        server_up: true,
        server_startable: false,
        loaded: Err("running `lms ps --json`: exited 1".into()),
        catalog: Ok(vec![meta("qwen")]),
    };
    let a = down.judge(&lms_model());
    assert_eq!(state(&a), AvailabilityState::Unknown, "{a:?}");
    assert!(a.reason.unwrap().contains("LM Studio did not answer"));
    let no_catalog = LmStudioAnswer {
        server_up: true,
        server_startable: false,
        loaded: Ok(vec![]),
        catalog: Err("`lms ls --json` exited with status 1".into()),
    };
    assert_eq!(
        state(&no_catalog.judge(&lms_model())),
        AvailabilityState::Unknown
    );
}

/// An engine that answers each model the same way, counting its asks.
#[derive(Clone)]
struct Fixed(Availability);
impl EngineAnswer for Fixed {
    fn judge(&self, _: &ProfileModel) -> Availability {
        self.0.clone()
    }
}

/// Each engine is asked once however many models it holds, an unmanaged
/// endpoint is `external` (asked nothing), an unresolvable one
/// `unavailable`, and the answers come back in the models' order.
#[test]
fn a_listing_asks_each_engine_once_and_keeps_the_order() {
    let ms = models(
        r#"{"profiles":{
            "a":{"models":[{"id":"l1","n_ctx":8}]},
            "b":{"models":[{"id":"t1","n_ctx":8,"endpoint":"tf"}]},
            "c":{"models":[{"id":"h","n_ctx":8,"endpoint":"api"}]},
            "d":{"models":[{"id":"l2","n_ctx":8},{"id":"x","n_ctx":8,"endpoint":"nope"}]}
        },"endpoints":{"tf":{"managed":"tensorfold","port":9},"api":{"url":"https://api.example.com/v1"}}}"#,
    );
    let asks = Arc::new(AtomicUsize::new(0));
    let counted = asks.clone();
    let listing = list_with(
        &ms,
        None,
        move |b| {
            counted.fetch_add(1, Ordering::SeqCst);
            Box::new(Fixed(match b {
                ManagedBackend::Lmstudio => Availability::resident(),
                ManagedBackend::Tensorfold | ManagedBackend::Flint => Availability::startable(),
            }))
        },
        |_| Duration::from_secs(5),
    );
    let states: Vec<AvailabilityState> = listing.each.iter().map(state).collect();
    use AvailabilityState::*;
    assert_eq!(
        states,
        [Resident, Startable, External, Resident, Unavailable]
    );
    assert_eq!(
        asks.load(Ordering::SeqCst),
        2,
        "LM Studio and Tensorfold, once each"
    );
    assert!(listing.each[4].reason.as_deref().unwrap().contains("nope"));
}

/// An engine that does not answer within the bound is `unknown`, naming it,
/// and the listing does not wait for it; the others still answer.
#[test]
fn an_engine_that_does_not_answer_in_time_is_unknown() {
    let ms = models(
        r#"{"profiles":{"a":{"models":[{"id":"l1","n_ctx":8}]},"b":{"models":[{"id":"t1","n_ctx":8,"endpoint":"tf"}]}},
            "endpoints":{"tf":{"managed":"tensorfold","port":9}}}"#,
    );
    let began = std::time::Instant::now();
    let listing = list_with(
        &ms,
        None,
        |b| {
            // Well past the 300 ms bound and its 1 s reap grace.
            if b == ManagedBackend::Tensorfold {
                std::thread::sleep(Duration::from_millis(3000));
            }
            Box::new(Fixed(Availability::startable()))
        },
        |_| Duration::from_millis(300),
    );
    assert!(
        began.elapsed() < Duration::from_secs(2),
        "it waited {:?}",
        began.elapsed()
    );
    assert_eq!(listing.each[0], Availability::startable());
    assert_eq!(state(&listing.each[1]), AvailabilityState::Unknown);
    assert!(
        listing.each[1]
            .reason
            .as_deref()
            .unwrap()
            .contains("Tensorfold did not answer within"),
        "{:?}",
        listing.each[1]
    );
}

/// An engine one of whose probes ran to the bound (killed there, so its
/// answer arrives just past it) keeps what its other probes got: a slow
/// `lms ls` must not turn a model LM Studio lists as loaded `unknown`.
#[test]
fn an_answer_whose_probe_was_cut_at_the_bound_keeps_what_it_got() {
    let ms = models(r#"{"profiles":{"a":{"models":[{"id":"l1","n_ctx":8}]}}}"#);
    let listing = list_with(
        &ms,
        None,
        |_| {
            // A probe killed at the 300 ms bound: the answer comes just after.
            std::thread::sleep(Duration::from_millis(400));
            Box::new(Fixed(Availability::resident()))
        },
        |_| Duration::from_millis(300),
    );
    assert_eq!(listing.each[0], Availability::resident());
}

/// The production Tensorfold answer asks the binary only its read-only
/// questions, once each, and starts nothing.
#[test]
fn asking_tensorfold_runs_only_its_read_only_commands() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("calls.log");
    let fake = dir.path().join("tensorfold-native");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$1\" in\n  --version) echo 'tensorfold-native 1.2.4';;\n  \
             capabilities) echo '{{\"schema\":1,\"serve\":{{\"flags\":{{}}}}}}';;\n  \
             models) echo '{{\"schema\":1,\"models\":[{{\"repo\":\"{TF_MODEL}\"}}]}}';;\n  *) exit 9;;\nesac\n",
            log.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A freshly written script's first exec can take seconds on a loaded
    // Mac; run it once so the bounded probes measure the probe, not that.
    let _ = std::process::Command::new(&fake).arg("--version").output();
    let _ = std::fs::remove_file(&log);
    let dirs = Dirs {
        records: dir.path().join("run"),
        logs: dir.path().join("logs"),
    };
    let answer = TensorfoldAnswer::ask(Tensorfold::at(fake.to_string_lossy().into_owned()), dirs);
    assert_eq!(
        answer.facts.version.as_ref().ok(),
        Some(&Version {
            major: 1,
            minor: 2,
            patch: 4
        })
    );
    let m = tf_model(free_port(), "[]");
    assert_eq!(answer.judge(&m), Availability::startable());
    assert_eq!(answer.judge(&m), Availability::startable());
    let mut calls: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    calls.sort();
    assert_eq!(
        calls,
        ["--version", "capabilities --json", "models --json", "serve --help"],
        "judging asks the binary nothing more"
    );
    assert!(
        !dir.path().join("run").exists(),
        "no server record was written"
    );
}

/// A model darkmux could not start as declared (no `n_ctx`, or a Tensorfold
/// endpoint with no `port`) is `unavailable`, saying which, on either engine.
#[test]
fn a_model_with_no_context_or_port_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let mut no_ctx = lms_model();
    no_ctx.n_ctx = None;
    let lms = LmStudioAnswer {
        server_up: true,
        server_startable: false,
        loaded: Ok(vec![]),
        catalog: Ok(vec![meta("qwen")]),
    }
    .judge(&no_ctx);
    assert_eq!(state(&lms), AvailabilityState::Unavailable, "{lms:?}");
    assert!(lms.reason.unwrap().contains("n_ctx"));

    let tf = tf_answer(facts("tensorfold-native 1.2.4", &[TF_MODEL]), &dir);
    let mut tf_no_ctx = tf_model(free_port(), "[]");
    tf_no_ctx.n_ctx = None;
    let a = tf.judge(&tf_no_ctx);
    assert_eq!(state(&a), AvailabilityState::Unavailable, "{a:?}");
    assert!(a.reason.unwrap().contains("n_ctx"));

    let mut no_port = tf_model(free_port(), "[]");
    no_port.endpoint.as_mut().unwrap().port = None;
    let a = tf.judge(&no_port);
    assert_eq!(state(&a), AvailabilityState::Unavailable, "{a:?}");
    assert!(a.reason.unwrap().contains("declares no `port`"));
}

/// Sets env vars for one closure, restoring them after (serial tests only).
fn with_env<T>(vars: &[(&str, &str)], f: impl FnOnce() -> T) -> T {
    let saved: Vec<_> = vars
        .iter()
        .map(|(k, _)| (*k, std::env::var_os(k)))
        .collect();
    for (k, v) in vars {
        unsafe { std::env::set_var(k, v) };
    }
    let out = f();
    for (k, v) in saved {
        match v {
            Some(v) => unsafe { std::env::set_var(k, v) },
            None => unsafe { std::env::remove_var(k) },
        }
    }
    out
}

/// (#3466 review) A refusal from the dispatch preflight (the caller runs
/// it: `darkmux_crew::user_files::dispatch_refusal`) refuses every dispatch,
/// so every model reads `unavailable` with it, and no engine is asked.
#[test]
fn a_dispatch_refusal_makes_every_model_unavailable_and_asks_nothing() {
    let ms = models(
        r#"{"profiles":{"a":{"models":[{"id":"l1","n_ctx":8}]},"b":{"models":[{"id":"h","endpoint":"api"}]}},
            "endpoints":{"api":{"url":"https://api.example.com/v1"}}}"#,
    );
    let listing = list_with(
        &ms,
        Some("dispatch: refusing to start: bad config".into()),
        |_| panic!("no engine is asked"),
        |_| Duration::from_secs(5),
    );
    for a in &listing.each {
        assert_eq!(
            a,
            &Availability::unavailable("dispatch: refusing to start: bad config")
        );
    }
}

/// (#3466 review) LM Studio is given longer than a binary darkmux runs: a
/// cold `lms` takes seconds, and a timeout reads `unknown`, which an app
/// treats as not reachable.
#[test]
fn lm_studio_gets_a_longer_bound_than_a_server_binary() {
    assert_eq!(
        listing_bound(ManagedBackend::Lmstudio),
        Duration::from_secs(10)
    );
    assert_eq!(
        listing_bound(ManagedBackend::Tensorfold),
        Duration::from_secs(3)
    );
}

/// (#3466 review) A profile model id LM Studio resolves is judged by the
/// names LM Studio's catalog gives each model (its key, its path, its
/// variants), as `lms load` resolves them; a name it does not give is not
/// downloaded.
#[test]
#[serial_test::serial]
fn lm_studio_resolves_a_variant_and_a_path() {
    let dir = tempfile::tempdir().unwrap();
    let lms = dir.path().join("lms");
    std::fs::write(
        &lms,
        r#"#!/bin/sh
case "$1" in
  ps) echo '[]';;
  ls) echo '[{"modelKey":"qwen/qwen3.8-27b","path":"qwen/qwen3.8-27b","indexedModelIdentifier":"qwen/qwen3.8-27b","variants":["qwen/qwen3.8-27b@4bit","qwen/qwen3.8-27b@8bit"],"type":"llm"},{"modelKey":"phi-4","path":"lmstudio-community/phi-4-GGUF/phi-4-Q4_K_M.gguf","type":"llm"}]';;
  *) exit 1;;
esac
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&lms, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = std::process::Command::new(&lms).arg("--version").output();
    // LM Studio's local server, up (#3578).
    let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", server.local_addr().unwrap().port());
    let answer = with_env(
        &[("DARKMUX_LMS_BIN", lms.to_str().unwrap()), ("DARKMUX_LMSTUDIO_URL", &url)],
        LmStudioAnswer::ask,
    );
    let judge = |id: &str| {
        let mut m = lms_model();
        m.id = id.into();
        answer.judge(&m).state
    };
    assert_eq!(
        judge("qwen/qwen3.8-27b@4bit"),
        AvailabilityState::Startable,
        "a variant"
    );
    assert_eq!(
        judge("lmstudio-community/phi-4-GGUF/phi-4-Q4_K_M.gguf"),
        AvailabilityState::Startable,
        "a path"
    );
    assert_eq!(judge("phi-4"), AvailabilityState::Startable, "a key");
    assert_eq!(
        judge("qwen/qwen3.8-27b@2bit"),
        AvailabilityState::Unavailable,
        "a variant it does not have"
    );
}

/// (#3466 review) A probe still running when the listing's bound passes is
/// killed then, not left to run out its own longer bound: the listing
/// leaves no child behind.
#[test]
#[serial_test::serial]
fn a_probe_running_past_the_bound_is_killed() {
    let dir = tempfile::tempdir().unwrap();
    let pids = dir.path().join("pids");
    let lms = dir.path().join("lms");
    // It forks a child of its own first: the kill must reach its group.
    std::fs::write(
        &lms,
        format!(
            "#!/bin/sh\necho $$ >> '{0}'\nsleep 30 &\necho $! >> '{0}'\nexec sleep 30\n",
            pids.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&lms, std::fs::Permissions::from_mode(0o755)).unwrap();
    let tf = dir.path().join("tensorfold-native");
    std::fs::write(
        &tf,
        format!(
            "#!/bin/sh\necho $$ >> '{}'\nexec sleep 30\n",
            pids.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&tf, std::fs::Permissions::from_mode(0o755)).unwrap();
    let ms = models(&format!(
        r#"{{"profiles":{{"p":{{"models":[{{"id":"qwen","n_ctx":8}}]}},"t":{{"models":[{{"id":"TF/m","n_ctx":8,"endpoint":"tf"}}]}}}},
            "endpoints":{{"tf":{{"managed":"tensorfold","port":{}}}}}}}"#,
        free_port()
    ));
    let listing = with_env(
        &[
            ("DARKMUX_LMS_BIN", lms.to_str().unwrap()),
            ("DARKMUX_TENSORFOLD_BIN", tf.to_str().unwrap()),
        ],
        // 3 s: a freshly written script's first exec can take seconds on a
        // loaded Mac, and the fakes must have started to be killed.
        || list_with(&ms, None, ask, |_| Duration::from_secs(3)),
    );
    for a in &listing.each {
        assert_eq!(a.state, AvailabilityState::Unknown, "{a:?}");
    }
    // No wait here: the listing must not return with a probe still alive.
    let started: Vec<i32> = std::fs::read_to_string(&pids)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    assert!(
        started.len() >= 2,
        "the fakes ran (a probe and the child it forked, at least): {started:?}"
    );
    for pid in started {
        assert!(gone_soon(pid), "lms probe {pid} outlived the listing");
    }
}

/// Whether `pid` is gone, or a zombie, within a second. The listing kills
/// the probe's whole group before it returns, but the child the fake forked
/// is not the listing's to reap: orphaned by the kill, it waits for init to
/// reap it, and until then `kill(pid, 0)` still finds it. A probe left
/// running is a `sleep 30`, so a second tells the two apart.
fn gone_soon(pid: i32) -> bool {
    let until = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        let zombie = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim_start().starts_with('Z'))
            .unwrap_or(false);
        if unsafe { libc_kill(pid, 0) } != 0 || zombie {
            return true;
        }
        if std::time::Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

// ─── Flint (#3453) ───────────────────────────────────────────────────────

/// A rec 0.1.0 install under `data`: `runtimes` runtimes and, for each of
/// `models`, a model directory holding `<revision>.json`.
fn rec_install(data: &std::path::Path, runtimes: usize, models: &[&str]) {
    for i in 0..runtimes {
        let dir = data.join("runtimes").join(format!("sha{i}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("rec-mlx"), "#!/bin/sh\n").unwrap();
    }
    for (i, rev) in models.iter().enumerate() {
        let dir = data.join("models").join(format!("h{i}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{rev}.json")), "{}").unwrap();
    }
}

fn flint_model(port: u16) -> ProfileModel {
    models(&format!(
        r#"{{"profiles":{{"f":{{"models":[{{"id":"flint-int4","n_ctx":8192,"endpoint":"fl"}}]}}}},
            "endpoints":{{"fl":{{"managed":"flint","port":{port}}}}}}}"#
    ))
    .remove(0)
}

fn flint_answer(
    dir: &tempfile::TempDir,
    version: Result<&str, &str>,
    own: Result<Option<&str>, &str>,
) -> FlintAnswer {
    FlintAnswer {
        rec: crate::engine::flint::Rec::at("/opt/rec/rec", Some(dir.path().join("rec"))),
        version: version.map(str::to_string).map_err(str::to_string),
        own_server: own.map(|o| o.map(serving_nothing)).map_err(str::to_string),
        key: Some(Ok(())),
        dirs: Dirs {
            records: dir.path().join("run"),
            logs: dir.path().join("logs"),
        },
    }
}

/// rec's own server at `address`, ready, as `rec status` says when it names
/// no revision or context: it serves nothing a profile could reuse.
fn serving_nothing(address: &str) -> crate::engine::flint::OwnServer {
    crate::engine::flint::OwnServer {
        address: address.to_string(),
        endpoint: Some(address.to_string()),
        ready: true,
        revision: None,
        context: None,
    }
}

/// (#3574, contract 4) rec's own server serving the profile's revision at
/// its `n_ctx` or more reads `external`, as a dispatch reuses it: usable,
/// and not darkmux's to manage. Another revision, or a smaller context,
/// stays `unavailable` with `engine_in_use`, naming what does not match.
#[test]
fn recs_own_server_serving_the_model_is_external_and_otherwise_engine_in_use() {
    let dir = tempfile::tempdir().unwrap();
    rec_install(&dir.path().join("rec"), 1, &["flint-int4"]);
    let rec_serving = |revision: &str, context: u64| {
        let mut answer = flint_answer(&dir, Ok("0.1.0"), Ok(None));
        answer.own_server = Ok(Some(crate::engine::flint::OwnServer {
            revision: Some(revision.to_string()),
            context: Some(context),
            ..serving_nothing("http://127.0.0.1:11500/v1")
        }));
        answer.judge(&flint_model(free_port()))
    };
    assert_eq!(rec_serving("flint-int4", 8192), Availability::external());
    assert_eq!(rec_serving("flint-int4", 262_144), Availability::external());
    // A key `rec auth token` will not give refuses what a dispatch refuses.
    let mut keyless = flint_answer(&dir, Ok("0.1.0"), Ok(None));
    keyless.own_server = Ok(Some(crate::engine::flint::OwnServer {
        revision: Some("flint-int4".into()),
        context: Some(8192),
        ..serving_nothing("http://127.0.0.1:11500/v1")
    }));
    keyless.key = Some(Err("`rec auth token` printed no key (exit 1)".into()));
    let a = keyless.judge(&flint_model(free_port()));
    assert_eq!((a.state, a.code.as_deref()), (AvailabilityState::Unavailable, Some("engine_unavailable")), "{a:?}");
    assert!(a.reason.as_deref().unwrap().contains("auth token"), "{a:?}");
    for (revision, context, says) in [("flint-int8", 8192, "`flint-int8`"), ("flint-int4", 4096, "4096")] {
        let a = rec_serving(revision, context);
        assert_eq!(a.state, AvailabilityState::Unavailable, "{a:?}");
        assert_eq!(a.code.as_deref(), Some("engine_in_use"), "{a:?}");
        assert!(a.reason.as_deref().unwrap().contains(says), "{a:?}");
    }
}

/// (#3574) A listing asks `rec auth token` only while rec's own server is
/// up, and keeps whether it gave a key, never the key.
#[test]
fn a_listing_asks_recs_key_only_while_its_own_server_is_up() {
    use std::os::unix::fs::PermissionsExt;
    let rec_with = |dir: &std::path::Path, status: &str, auth: &str| {
        let bin = dir.join("rec-bin");
        std::fs::write(
            &bin,
            format!("#!/bin/sh\ncase \"$1\" in\n  --version) echo 'rec 0.1.0';;\n  status) {status};;\n  auth) {auth};;\nesac\n"),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        crate::engine::flint::Rec::at(bin.to_string_lossy().into_owned(), Some(dir.join("rec")))
    };
    let dirs = |dir: &tempfile::TempDir| Dirs { records: dir.path().join("run"), logs: dir.path().join("logs") };
    let up = r#"echo '{"ready":true,"endpoint":"http://127.0.0.1:11500/v1","revision":"flint-int4","context":8192}'"#;
    let dir = tempfile::tempdir().unwrap();
    let failing = FlintAnswer::ask(rec_with(dir.path(), up, "exit 1"), dirs(&dir));
    assert!(matches!(&failing.key, Some(Err(e)) if e.contains("auth token")), "{:?}", failing.key);
    let dir = tempfile::tempdir().unwrap();
    let keyed = FlintAnswer::ask(rec_with(dir.path(), up, "echo placeholder-key-for-tests"), dirs(&dir));
    assert_eq!(keyed.key, Some(Ok(())));
    let dir = tempfile::tempdir().unwrap();
    let down = FlintAnswer::ask(rec_with(dir.path(), "exit 1", "exit 1"), dirs(&dir));
    assert_eq!(down.key, None, "not asked while rec's own server is down");
}

/// A stopped Flint server is `startable`: rec 0.1.0, one runtime, one model
/// directory for the revision, the port free, rec's own server not running.
#[test]
fn a_stopped_flint_server_is_startable() {
    let dir = tempfile::tempdir().unwrap();
    rec_install(&dir.path().join("rec"), 1, &["flint-int4"]);
    let a = flint_answer(&dir, Ok("0.1.0"), Ok(None)).judge(&flint_model(free_port()));
    assert_eq!(a, Availability::startable());
}

/// rec's own server running is `unavailable` (dispatch's `engine_in_use`),
/// saying to run `rec stop`: darkmux never stops or takes over a server it
/// did not start.
#[test]
fn recs_own_server_running_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    rec_install(&dir.path().join("rec"), 1, &["flint-int4"]);
    let a = flint_answer(&dir, Ok("0.1.0"), Ok(Some("http://127.0.0.1:8090/v1")))
        .judge(&flint_model(free_port()));
    assert_eq!(a.state, AvailabilityState::Unavailable, "{a:?}");
    assert!(a.reason.unwrap().contains("rec stop"));
}

/// An unverified rec, no runtime or several, no model directory or several,
/// a port held by a server darkmux did not start: `unavailable`, each
/// naming why. `rec status` that could not run is `unknown`.
#[test]
fn flint_is_unavailable_naming_why_or_unknown_when_rec_cannot_say() {
    let case = |runtimes: usize,
                models: &[&str],
                version: Result<&str, &str>,
                own: Result<Option<&str>, &str>,
                port: u16| {
        let dir = tempfile::tempdir().unwrap();
        rec_install(&dir.path().join("rec"), runtimes, models);
        flint_answer(&dir, version, own).judge(&flint_model(port))
    };
    let unverified = case(
        1,
        &["flint-int4"],
        Err("rec 0.2.0 is installed; this darkmux supports rec 0.1.0"),
        Ok(None),
        free_port(),
    );
    assert_eq!(unverified.state, AvailabilityState::Unavailable);
    assert!(unverified.reason.unwrap().contains("0.2.0"));
    for (runtimes, models, names) in [
        (0, &["flint-int4"][..], "no runtime"),
        (2, &["flint-int4"][..], "2 runtimes"),
        (1, &[][..], "no model"),
        (1, &["flint-int4", "flint-int4"][..], "2 model directories"),
    ] {
        let a = case(runtimes, models, Ok("0.1.0"), Ok(None), free_port());
        assert_eq!(a.state, AvailabilityState::Unavailable, "{names}: {a:?}");
        assert!(
            a.reason.as_deref().unwrap().contains(names),
            "{names}: {a:?}"
        );
    }
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let foreign = case(1, &["flint-int4"], Ok("0.1.0"), Ok(None), port);
    assert_eq!(foreign.state, AvailabilityState::Unavailable, "{foreign:?}");
    assert!(foreign
        .reason
        .unwrap()
        .contains(&format!("127.0.0.1:{port}")));
    let silent = case(
        1,
        &["flint-int4"],
        Ok("0.1.0"),
        Err("`rec status` failed: timed out"),
        free_port(),
    );
    assert_eq!(silent.state, AvailabilityState::Unknown, "{silent:?}");
}

/// darkmux's own Flint server running this model's command, answering
/// `flint` and listening on the port is `resident`, even though `rec status`
/// reports a server there: it is darkmux's, not rec's own.
#[test]
fn darkmuxs_running_flint_server_is_resident() {
    let dir = tempfile::tempdir().unwrap();
    rec_install(&dir.path().join("rec"), 1, &["flint-int4"]);
    let port = serving("flint");
    let model = flint_model(port);
    let address = format!("http://127.0.0.1:{port}/v1");
    let answer = flint_answer(&dir, Ok("0.1.0"), Ok(Some(&address)));
    let argv = answer
        .rec
        .serve_spec("fl", port, &model, "0.1.0")
        .unwrap()
        .argv;
    record_for(&answer.dirs, "fl", "flint", port, argv);
    assert_eq!(answer.judge(&model), Availability::resident());
}

/// darkmux's own Flint server resident on its endpoint, while `rec status`
/// names rec's own server on another port: a dispatch refuses
/// `engine_in_use` (rec runs one server per machine), so the listing reads
/// `unavailable` with that refusal, never `resident`.
#[test]
fn darkmuxs_flint_server_beside_recs_own_is_unavailable_not_resident() {
    let dir = tempfile::tempdir().unwrap();
    rec_install(&dir.path().join("rec"), 1, &["flint-int4"]);
    let port = serving("flint");
    let model = flint_model(port);
    let answer = flint_answer(&dir, Ok("0.1.0"), Ok(Some("http://127.0.0.1:11500/v1")));
    let argv = answer.rec.serve_spec("fl", port, &model, "0.1.0").unwrap().argv;
    record_for(&answer.dirs, "fl", "flint", port, argv);
    let a = answer.judge(&model);
    assert_eq!(a.state, AvailabilityState::Unavailable, "{a:?}");
    assert!(a.reason.unwrap().contains("rec stop"));
}

/// darkmux's own Flint server on the endpoint running another command (a
/// context change, say) is what `rec status` reports, and it is darkmux's,
/// not rec's: the model is `startable` (darkmux replaces its own server),
/// never `engine_in_use`.
#[test]
fn darkmuxs_own_flint_server_is_not_recs() {
    let dir = tempfile::tempdir().unwrap();
    rec_install(&dir.path().join("rec"), 1, &["flint-int4"]);
    let port = serving("flint");
    let model = flint_model(port);
    let address = format!("http://127.0.0.1:{port}/v1");
    let answer = flint_answer(&dir, Ok("0.1.0"), Ok(Some(&address)));
    let mut argv = answer
        .rec
        .serve_spec("fl", port, &model, "0.1.0")
        .unwrap()
        .argv;
    argv.push("--fast-sync".into());
    record_for(&answer.dirs, "fl", "flint", port, argv);
    assert_eq!(answer.judge(&model), Availability::startable());
}

/// The production Flint answer ([`ask`]) asks the installed `rec` its
/// version and status and reads its install from the XDG data home, as
/// dispatch's adapter does.
#[test]
#[serial_test::serial]
fn asking_flint_reads_recs_version_status_and_install() {
    let dir = tempfile::tempdir().unwrap();
    rec_install(&dir.path().join("data/rec"), 1, &["flint-int4"]);
    let rec = dir.path().join("rec");
    std::fs::write(
        &rec,
        "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'rec 0.1.0';;\n  status) echo '{\"ready\": false}';;\n  *) exit 9;;\nesac\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&rec, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = std::process::Command::new(&rec).arg("--version").output();
    let data = dir.path().join("data");
    let home = dir.path().join("home");
    let vars = [
        ("DARKMUX_FLINT_BIN", rec.to_str().unwrap()),
        ("XDG_DATA_HOME", data.to_str().unwrap()),
        ("DARKMUX_HOME", home.to_str().unwrap()),
    ];
    let state = with_env(&vars, || {
        ask(ManagedBackend::Flint).judge(&flint_model(free_port()))
    });
    assert_eq!(state, Availability::startable());
}

// ─── The code (#3574) ────────────────────────────────────────────────────

/// The code an `unavailable` carries, by the engine's own case.
fn code(a: &Availability) -> Option<&str> {
    a.code.as_deref()
}

/// (#3574) Every `unavailable` an engine gives carries the code a dispatch
/// refusing for the same reason carries (`engine_dispatch`'s `refuse`, and
/// the wave planner's `block_code` for LM Studio), so an app branches on it
/// and never on the wording: one vocabulary.
#[test]
fn each_engines_unavailable_carries_the_code_its_dispatch_refusal_carries() {
    let dir = tempfile::tempdir().unwrap();
    // LM Studio: a model it has not downloaded is the planner's
    // `UnknownModelKey`, `engine_unavailable`.
    let lms = LmStudioAnswer { server_up: true, server_startable: false, loaded: Ok(vec![]), catalog: Ok(vec![meta("other")]) }.judge(&lms_model());
    assert_eq!((state(&lms), code(&lms)), (AvailabilityState::Unavailable, Some("engine_unavailable")), "{lms:?}");

    // Tensorfold: `plan`'s `verified_version` is `engine_unavailable`, its
    // `engine_args` check `model_start_failed`, a model it does not list
    // fails the start (`model_start_failed`), and a port someone else holds
    // is the server lifecycle's `port_in_use`.
    let tf = |facts: TensorfoldProbe, port: u16, args: &str| tf_answer(facts, &dir).judge(&tf_model(port, args));
    let mut gone = facts("tensorfold-native 1.2.4", &[TF_MODEL]);
    gone.version = Err("`tensorfold-native --version` failed: `tensorfold-native` was not found".into());
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let held_port = held.local_addr().unwrap().port();
    for (a, want) in [
        (tf(facts("tensorfold-native 1.3.0", &[TF_MODEL]), free_port(), "[]"), "engine_unavailable"),
        (tf(gone, free_port(), "[]"), "engine_unavailable"),
        (tf(facts("tensorfold-native 1.2.4", &[TF_MODEL]), free_port(), r#"["--turbo"]"#), "model_start_failed"),
        (tf(facts("tensorfold-native 1.2.4", &["other/model"]), free_port(), "[]"), "model_start_failed"),
        (tf(facts("tensorfold-native 1.2.4", &[TF_MODEL]), held_port, "[]"), "port_in_use"),
    ] {
        assert_eq!((state(&a), code(&a)), (AvailabilityState::Unavailable, Some(want)), "{a:?}");
    }

    // Flint: `Rec::plan` refuses an unverified rec and an install it cannot
    // read `engine_unavailable`; rec's own server up is `engine_in_use`; a
    // port someone else holds is `port_in_use`.
    let flint = |runtimes: usize, revs: &[&str], version: Result<&str, &str>, own: Option<&str>, port: u16| {
        let dir = tempfile::tempdir().unwrap();
        rec_install(&dir.path().join("rec"), runtimes, revs);
        flint_answer(&dir, version, Ok(own)).judge(&flint_model(port))
    };
    for (a, want) in [
        (flint(1, &["flint-int4"], Err("rec 0.2.0 is installed"), None, free_port()), "engine_unavailable"),
        (flint(0, &["flint-int4"], Ok("0.1.0"), None, free_port()), "engine_unavailable"),
        (flint(1, &[], Ok("0.1.0"), None, free_port()), "engine_unavailable"),
        (flint(1, &["flint-int4"], Ok("0.1.0"), Some("http://127.0.0.1:11500/v1"), free_port()), "engine_in_use"),
        (flint(1, &["flint-int4"], Ok("0.1.0"), None, held_port), "port_in_use"),
    ] {
        assert_eq!((state(&a), code(&a)), (AvailabilityState::Unavailable, Some(want)), "{a:?}");
    }
}

/// (#3574) darkmux's own Flint server resident beside rec's own server is
/// refused `engine_in_use` by a dispatch, so the listing carries that code.
#[test]
fn darkmuxs_flint_server_beside_recs_own_carries_engine_in_use() {
    let dir = tempfile::tempdir().unwrap();
    rec_install(&dir.path().join("rec"), 1, &["flint-int4"]);
    let port = serving("flint");
    let model = flint_model(port);
    let answer = flint_answer(&dir, Ok("0.1.0"), Ok(Some("http://127.0.0.1:11500/v1")));
    let argv = answer.rec.serve_spec("fl", port, &model, "0.1.0").unwrap().argv;
    record_for(&answer.dirs, "fl", "flint", port, argv);
    assert_eq!(code(&answer.judge(&model)), Some("engine_in_use"));
}

/// (#3574) No code where no dispatch refusal fits: a registry problem a
/// dispatch refuses before any engine is asked (no `n_ctx`, no `port`, an
/// endpoint that does not resolve, a refused user file), and every state
/// that is not `unavailable`.
#[test]
fn no_code_where_no_dispatch_refusal_fits() {
    let dir = tempfile::tempdir().unwrap();
    let mut no_ctx = lms_model();
    no_ctx.n_ctx = None;
    let lms = LmStudioAnswer { server_up: true, server_startable: false, loaded: Ok(vec![]), catalog: Ok(vec![meta("qwen")]) }.judge(&no_ctx);
    assert_eq!((state(&lms), code(&lms)), (AvailabilityState::Unavailable, None));
    let mut no_port = tf_model(free_port(), "[]");
    no_port.endpoint.as_mut().unwrap().port = None;
    let tf = tf_answer(facts("tensorfold-native 1.2.4", &[TF_MODEL]), &dir).judge(&no_port);
    assert_eq!((state(&tf), code(&tf)), (AvailabilityState::Unavailable, None));
    let mut flint_no_port = flint_model(free_port());
    flint_no_port.endpoint.as_mut().unwrap().port = None;
    let fl = flint_answer(&dir, Ok("0.1.0"), Ok(None)).judge(&flint_no_port);
    assert_eq!((state(&fl), code(&fl)), (AvailabilityState::Unavailable, None));

    let ms = models(
        r#"{"profiles":{"a":{"models":[{"id":"l1","n_ctx":8},{"id":"x","n_ctx":8,"endpoint":"nope"}]}}}"#,
    );
    let refused = list_with(&ms, Some("bad config".into()), |_| panic!("no engine is asked"), |_| Duration::from_secs(5));
    assert!(refused.each.iter().all(|a| a.code.is_none()), "{refused:?}");
    let listing = list_with(&ms, None, |_| Box::new(Fixed(Availability::resident())), |_| Duration::from_secs(5));
    assert_eq!((state(&listing.each[1]), code(&listing.each[1])), (AvailabilityState::Unavailable, None));

    for a in [Availability::resident(), Availability::startable(), Availability::external(), Availability::unknown("x")] {
        assert_eq!(a.code, None, "{a:?}");
    }
}

/// (#3574) The code is additive on the wire: absent when there is none (a
/// reader of card schema 1.4 sees the shape it knew), carried as its text
/// when there is, and a code a newer darkmux sends that this one does not
/// know reads back as that text, never failing the availability.
#[test]
fn the_code_is_absent_when_none_and_kept_as_text_when_unknown() {
    let plain = serde_json::to_value(Availability::unavailable("why")).unwrap();
    assert_eq!(plain, serde_json::json!({"state": "unavailable", "reason": "why"}));
    let coded = serde_json::to_value(Availability::refused(ClientCode::EngineInUse, "rec's own server")).unwrap();
    assert_eq!(coded["code"], "engine_in_use");
    let old: Availability = serde_json::from_value(serde_json::json!({"state": "unavailable", "reason": "why"})).unwrap();
    assert_eq!(old.code, None);
    let newer: Availability =
        serde_json::from_value(serde_json::json!({"state": "unavailable", "reason": "why", "code": "from_a_newer_darkmux"}))
            .unwrap();
    assert_eq!(newer.code.as_deref(), Some("from_a_newer_darkmux"));
}
