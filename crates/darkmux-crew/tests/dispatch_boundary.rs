//! (#316 stage 2, #3136) Boundary pins for `dispatch_internal::dispatch` and
//! its telemetry sampler (`run_telemetry_sampler`), before either is split by
//! responsibility.
//!
//! Every test drives the REAL `darkmux_crew::dispatch::dispatch` in-process
//! and asserts only what an observer outside the function sees:
//!
//! - the `DispatchResult` it returns, or the error text;
//! - the flow records it writes (contract 2: `dispatch.start` and one
//!   terminal `dispatch.complete`/`dispatch.error` on every exit path after
//!   the start), read back from the isolated flows dir;
//! - every `docker` call it makes, argv byte for byte, and what it piped to
//!   the container's stdin;
//! - every `lms` call it makes (load, unload, ps);
//! - the files it leaves in the out-dir and the workspace.
//!
//! Nothing here names an internal a split would move. These pin behavior AS
//! IT IS, oddities included (each marked "ODDITY"): a test that fails after a
//! refactor means the refactor changed what an observer sees, not that the old
//! behavior was right.
//!
//! # The doubles
//!
//! No real Docker, no real LM Studio, no real `~/.darkmux`, and no production
//! change: every seam below already exists.
//!
//! - `docker` and `lms` are shell fakes in one stable directory (written once,
//!   atomically; a re-created file is a new file to XProtect, the reason
//!   `tests/cli.rs::stable_cli_fakes` does the same). `docker` is found on
//!   `PATH`, `lms` through `DARKMUX_LMS_BIN`. The fake `docker run` plays the
//!   container: it copies a scripted trajectory into the mounted out-dir,
//!   sources an optional run script (to sleep, or to change what `lms ps`
//!   reports mid-run), prints scripted stdout/stderr and exits with a
//!   scripted code. Every call's argv is logged NUL-separated, one file per
//!   call.
//! - State (`DARKMUX_HOME`, flows, mods, ...) is pinned by
//!   `darkmux_types::test_isolation::IsolatedState`; `TMPDIR` is pinned under
//!   the same root, so the auto workspace and out-dir never land in the shared
//!   temp root (#2707). The daemon probe dials a closed loopback port.
//! - Host readings for the sampler's governors come from a scripted scenario
//!   through `DARKMUX_HOST_SOURCE_SCRIPT` (#2779).
//! - Hosted endpoints are `httpmock` (the single-shot path) or an address
//!   nothing answers (the container path, where only the fake container would
//!   call it).
//! - The host's own stderr is read from fd 2 for the length of the call.
//!
//! # What this file cannot see
//!
//! - The sampler's clock and cadence: it ticks every 2s of real time on its
//!   own thread, so the sampler tests hold the fake container open for a few
//!   seconds and assert what happened, never how many ticks.
//! - The session-presence heartbeat (Redis is unset by the isolation guard).
//! - Whether a kill reached a container: the fake `docker kill` always
//!   succeeds, so only that a kill was ISSUED, by name, is pinned.
//! - The `?` arms that need an I/O fault or a race to fire: a workspace-name
//!   collision, the origin stamp, the checkpoint staging and prompt writes,
//!   the container spawn, the secret write to a container that closed its
//!   stdin, `wait_with_output` itself failing.
//! - A sampler heartbeat lost to another process's steal of the sampler lock.
//!
//! Every test runs in a process of its own (`own_process!`) and `#[serial]`:
//! they mutate process env (`PATH`, `TMPDIR`, the state vars), and
//! `dispatch()` resolves some of it once per process.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use darkmux_crew::dispatch::{dispatch, CompactionDispatchArgs, DispatchOpts, DispatchResult};
use darkmux_types::session_id::{RunId, SessionId};
use darkmux_types::test_isolation::IsolatedState;
use serde_json::{json, Value};
use serial_test::serial;

// ─── harness ────────────────────────────────────────────────────────────

/// Every test here runs in a process of its own: `dispatch()` reads
/// process-global state that is resolved once (the host source, the budget
/// ledger's flows dir) and some tests raise the sticky interrupt flag.
/// nextest already runs one test per process; under plain `cargo test` this
/// re-runs the test alone, the way `darkmux_types::run_in_own_process!` does,
/// but with `--nocapture`: these tests read the host's stderr from fd 2,
/// which libtest's capture would otherwise swallow.
macro_rules! own_process {
    () => {{
        fn own_process_marker() {}
        if !darkmux_types::test_isolation::is_own_process() {
            let test = darkmux_types::test_isolation::test_name_from_marker(std::any::type_name_of_val(&own_process_marker));
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([test.as_str(), "--exact", "--test-threads=1", "--nocapture"])
                .env(darkmux_types::test_isolation::OWN_PROCESS_VAR, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success() && stdout.lines().any(|l| l.starts_with("test result: ok. 1 passed; 0 failed;")),
                "`{test}` failed in its own process ({}):\n{stdout}\n{}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
    }};
}

const FAKE_DOCKER: &str = r#"#!/bin/sh
state="${FAKE_DOCKER_STATE_DIR:?the fake docker needs FAKE_DOCKER_STATE_DIR}"
mkdir -p "$state/docker-calls"
n=$(ls "$state/docker-calls" | wc -l | tr -d ' ')
f="$state/docker-calls/$(printf '%04d-%s' "$n" "$$")"
for a in "$@"; do printf '%s\0' "$a"; done > "$f"
case "$1" in
  version)
    if [ -f "$state/docker-version-fail" ]; then cat "$state/docker-version-fail" >&2; exit 1; fi
    echo 27.0.0; exit 0 ;;
  image)
    for last; do :; done
    if [ -f "$state/docker-labels" ]; then
      while IFS= read -r entry; do
        case "$entry" in "$last="*) echo "sha256:id-of-$last|${entry#"$last="}"; exit 0 ;; esac
      done < "$state/docker-labels"
    fi
    echo "Error response from daemon: No such image: $last" >&2; exit 1 ;;
  pull) echo 'pull denied' >&2; exit 1 ;;
  create) echo fakecid; exit 0 ;;
  cp) for last; do :; done; printf '#!/bin/sh\n' > "$last"; exit 0 ;;
  run)
    case " $* " in *" -i "*) cat > "$state/docker.stdin" ;; esac
    out=""
    for a; do case "$a" in *:/darkmux-out) out="${a%:/darkmux-out}" ;; esac; done
    if [ -n "$out" ] && [ -f "$state/trajectory.jsonl" ]; then
      mkdir -p "$out/.darkmux-runtime"
      cp "$state/trajectory.jsonl" "$out/.darkmux-runtime/trajectory.jsonl"
    fi
    if [ -f "$state/run-script" ]; then . "$state/run-script"; fi
    if [ -f "$state/run-stdout" ]; then cat "$state/run-stdout"; fi
    if [ -f "$state/run-stderr" ]; then cat "$state/run-stderr" >&2; fi
    if [ -f "$state/run-exit" ]; then exit "$(cat "$state/run-exit")"; fi
    exit 0 ;;
esac
exit 0
"#;

const FAKE_LMS: &str = r#"#!/bin/sh
state="${FAKE_DOCKER_STATE_DIR:?the fake lms needs FAKE_DOCKER_STATE_DIR}"
echo "$*" >> "$state/lms.log"
case "$1" in
  ps)
    if [ -f "$state/lms-ps.json" ]; then cat "$state/lms-ps.json"; else echo '[]'; fi
    exit 0 ;;
  load)
    if [ -f "$state/lms-load-fail" ]; then cat "$state/lms-load-fail" >&2; exit 1; fi
    if [ -f "$state/lms-ps-after-load.json" ]; then cp "$state/lms-ps-after-load.json" "$state/lms-ps.json"; fi
    exit 0 ;;
  unload)
    if [ -f "$state/lms-ps-after-unload.json" ]; then cp "$state/lms-ps-after-unload.json" "$state/lms-ps.json"; fi
    exit 0 ;;
esac
if [ "$1" = ls ]; then echo '[]'; fi
exit 0
"#;

/// The fakes' directory, written once per target dir (atomic publish: nextest
/// runs each test in its own process, and a half-written fake is an exec
/// failure, not a fake).
fn fake_bin() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("crew-dispatch-boundary-fakes");
    fs::create_dir_all(&dir).unwrap();
    for (name, body) in [("docker", FAKE_DOCKER), ("lms", FAKE_LMS)] {
        let path = dir.join(name);
        if fs::read_to_string(&path).ok().as_deref() == Some(body) {
            continue;
        }
        let tmp = dir.join(format!("{name}.tmp.{}", std::process::id()));
        fs::write(&tmp, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&tmp, &path).unwrap();
    }
    dir
}

/// Sets env vars for the life of the value and restores each on drop.
#[derive(Default)]
struct Env(Vec<(String, Option<OsString>)>);

impl Env {
    fn set(&mut self, key: &str, value: impl AsRef<std::ffi::OsStr>) {
        self.0.push((key.to_string(), std::env::var_os(key)));
        // SAFETY: every caller holds #[serial]; `dispatch()` joins the threads
        // it spawns on every ordinary exit before this guard is dropped.
        unsafe { std::env::set_var(key, value) };
    }
    fn remove(&mut self, key: &str) {
        self.0.push((key.to_string(), std::env::var_os(key)));
        unsafe { std::env::remove_var(key) };
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for (k, v) in self.0.drain(..).rev() {
            unsafe {
                match v {
                    Some(v) => std::env::set_var(&k, v),
                    None => std::env::remove_var(&k),
                }
            }
        }
    }
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// One isolated world for one dispatch.
struct Rig {
    // Declared first so it drops (restoring env) before the state root goes.
    _env: Env,
    state: IsolatedState,
    /// Where the fakes keep their logs and scripts.
    fakes: PathBuf,
    pub profiles: PathBuf,
}

impl Rig {
    fn new() -> Rig {
        let state = IsolatedState::new();
        let fakes = state.join("fakes");
        let tmp = state.join("tmp");
        fs::create_dir_all(&fakes).unwrap();
        fs::create_dir_all(&tmp).unwrap();
        let profiles = state.join("profiles.json");
        fs::write(
            &profiles,
            json!({"profiles":{"fast":{"models":[{"id":"model-a","n_ctx":32000}]}},"default_profile":"fast"})
                .to_string(),
        )
        .unwrap();
        let mut env = Env::default();
        let path = std::env::var("PATH").unwrap_or_default();
        env.set("PATH", format!("{}:{path}", fake_bin().display()));
        env.set("DARKMUX_LMS_BIN", fake_bin().join("lms"));
        env.set("FAKE_DOCKER_STATE_DIR", &fakes);
        env.set("TMPDIR", &tmp);
        env.set("DARKMUX_PROFILES", &profiles);
        // The daemon probe dials a port nothing listens on, never a real
        // `darkmux serve` on this machine.
        env.set("DARKMUX_SERVE_BIND", "127.0.0.1");
        env.set("DARKMUX_SERVE_PORT", closed_port().to_string());
        for k in [
            "DARKMUX_TURN_DELAY_MS",
            "DARKMUX_HOST_SAMPLER_INTERVAL_MS",
            "DARKMUX_HOST_SOURCE_SCRIPT",
            "DARKMUX_INACTIVITY_TIMEOUT_SECONDS",
            "DARKMUX_MAX_TURNS",
            "DARKMUX_MAX_TOKENS",
            "DARKMUX_TEST_PANIC_AFTER_GUARD_SPAWN",
        ] {
            env.remove(k);
        }
        let rig = Rig { _env: env, state, fakes, profiles };
        rig.labels(&[("darkmux-runtime:latest", VERSION)]);
        rig.resident(&[("darkmux:model-a", "model-a", 32000)]);
        rig
    }

    fn env(&mut self) -> &mut Env {
        &mut self._env
    }

    fn labels(&self, labels: &[(&str, &str)]) {
        let lines: String = labels.iter().map(|(i, l)| format!("{i}={l}\n")).collect();
        fs::write(self.fakes.join("docker-labels"), lines).unwrap();
    }

    fn resident(&self, models: &[(&str, &str, u32)]) {
        fs::write(self.fakes.join("lms-ps.json"), lms_ps(models)).unwrap();
    }

    fn fake(&self, name: &str, body: impl AsRef<[u8]>) {
        fs::write(self.fakes.join(name), body).unwrap();
    }

    /// Every `docker` call, in order, as its argv.
    fn docker_calls(&self) -> Vec<Vec<String>> {
        let dir = self.fakes.join("docker-calls");
        let mut names: Vec<PathBuf> = fs::read_dir(&dir)
            .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default();
        names.sort();
        names
            .iter()
            .map(|p| {
                let bytes = fs::read(p).unwrap();
                bytes
                    .split(|b| *b == 0)
                    .filter(|s| !s.is_empty())
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .collect()
            })
            .collect()
    }

    fn docker_run(&self) -> Option<Vec<String>> {
        self.docker_calls().into_iter().find(|c| c.first().map(String::as_str) == Some("run"))
    }

    fn docker_verbs(&self) -> Vec<String> {
        self.docker_calls()
            .iter()
            .map(|c| c.iter().take(2).cloned().collect::<Vec<_>>().join(" "))
            .collect()
    }

    fn lms_log(&self) -> Vec<String> {
        fs::read_to_string(self.fakes.join("lms.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn stdin_blob(&self) -> Option<String> {
        fs::read_to_string(self.fakes.join("docker.stdin")).ok()
    }

    /// Every flow record written under the isolated flows dir.
    fn flows(&self) -> Vec<Value> {
        let mut out = Vec::new();
        let mut stack = vec![self.state.join("flows")];
        while let Some(d) = stack.pop() {
            let Ok(rd) = fs::read_dir(&d) else { continue };
            let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
            entries.sort();
            for p in entries {
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|e| e == "jsonl") {
                    for line in fs::read_to_string(&p).unwrap_or_default().lines() {
                        if let Ok(v) = serde_json::from_str::<Value>(line) {
                            out.push(v);
                        }
                    }
                }
            }
        }
        out
    }

    fn actions(&self) -> Vec<String> {
        self.flows()
            .iter()
            .map(|r| r.get("action").and_then(Value::as_str).unwrap_or("?").to_string())
            .collect()
    }

    fn record(&self, action: &str) -> Value {
        let all = self.flows();
        all.iter()
            .find(|r| r.get("action").and_then(Value::as_str) == Some(action))
            .cloned()
            .unwrap_or_else(|| panic!("no `{action}` record; got {:?}", self.actions()))
    }

    /// The scripted container: trajectory, stdout, stderr, exit code.
    fn container(&self, trajectory: &str, stdout: &str, stderr: &str, exit: i32) {
        self.fake("trajectory.jsonl", trajectory);
        self.fake("run-stdout", stdout);
        self.fake("run-stderr", stderr);
        self.fake("run-exit", exit.to_string());
    }

    /// Forget the docker calls and the scripted run, for a second dispatch.
    fn reset_calls(&self) {
        let _ = fs::remove_dir_all(self.fakes.join("docker-calls"));
        let _ = fs::remove_file(self.fakes.join("run-script"));
        let _ = fs::remove_file(self.fakes.join("lms.log"));
    }

    fn opts(&self) -> DispatchOpts {
        opts_with(&self.profiles)
    }
}

/// A loopback port with no listener: bind, read it, drop it.
fn closed_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn lms_ps(models: &[(&str, &str, u32)]) -> String {
    Value::Array(
        models
            .iter()
            .map(|(id, key, ctx)| {
                json!({"identifier": id, "modelKey": key, "status": "loaded", "sizeBytes": 1_000_000_000u64, "contextLength": ctx})
            })
            .collect(),
    )
    .to_string()
}

fn session() -> SessionId {
    SessionId::adhoc(RunId::standalone("boundary").unwrap(), "coder", "pin")
}

fn opts_with(profiles: &Path) -> DispatchOpts {
    DispatchOpts {
        finding_sites: None,
        allow_utility_model: false,
        remote_origin: None,
        live_channel: false,
        brief_refs: Vec::new(),
        workspace_read_only: false,
        record_context: None,
        resume_from: None,
        host_out: None,
        max_turns_override: None,
        timeout_override_seconds: None,
        role_id: "coder".to_string(),
        message: "smoke".to_string(),
        session: session(),
        timeout_seconds: 120,
        skip_preflight: true,
        json: true,
        workdir: None,
        phase_id: None,
        machine: None,
        wait: true,
        compaction: CompactionDispatchArgs::default(),
        profile_name: None,
        config_path: Some(profiles.to_string_lossy().into_owned()),
        force_container: false,
        max_completion_tokens: None,
        image: None,
        model_base_url_override: None,
        step_id: None,
        system_prompt_override: None,
    }
}

/// A one-turn trajectory: one model call with usage, one tool call.
const ONE_TURN: &str = concat!(
    r#"{"model":"darkmux:model-a","prompt_chars":5,"system_chars":10,"ts":1,"type":"dispatch.start"}"#,
    "\n",
    r#"{"finish_reason":"tool_calls","seq":1,"tool_calls":[{"arguments_chars":3,"id":"t1","name":"read"}],"ts":2,"type":"model.completed","usage":{"completion_tokens":7,"prompt_tokens":11,"total_tokens":18}}"#,
    "\n",
    r#"{"args_chars":3,"ok":true,"result_chars":40,"seq":1,"tool_name":"read","tool_seq":0,"ts":3,"type":"tool.completed"}"#,
    "\n",
    r#"{"finish_reason":"stop","seq":2,"tool_calls":[],"ts":4,"type":"model.completed","usage":{"completion_tokens":5,"prompt_tokens":20,"total_tokens":25}}"#,
    "\n",
);

const ENVELOPE: &str =
    r#"{"result":"stop","final_assistant":"done","trajectory_path":"/darkmux-out/.darkmux-runtime/trajectory.jsonl"}"#;

/// Runs `dispatch(opts)` with this process's fd 2 pointed at a file, so the
/// operator-facing lines it prints are observable; returns the result and
/// that stderr. (The container's own stderr is piped, never fd 2.)
fn run(rig: &Rig, opts: DispatchOpts) -> (anyhow::Result<DispatchResult>, String) {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let path = rig.fakes.join(format!("host-stderr.{n}"));
    let file = fs::File::create(&path).unwrap();
    let _ = std::io::stderr().flush();
    // SAFETY: plain fd juggling on this process's own stderr, restored below.
    let saved = unsafe { libc::dup(2) };
    unsafe { libc::dup2(file.as_raw_fd(), 2) };
    let res = dispatch(opts);
    let _ = std::io::stderr().flush();
    unsafe {
        libc::dup2(saved, 2);
        libc::close(saved);
    }
    (res, fs::read_to_string(&path).unwrap_or_default())
}

fn arg_after<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    argv.iter().position(|a| a == flag).and_then(|i| argv.get(i + 1)).map(String::as_str)
}

fn has_pair(argv: &[String], flag: &str, value: &str) -> bool {
    argv.windows(2).any(|w| w[0] == flag && w[1] == value)
}

fn payload(r: &Value) -> &Value {
    r.get("payload").unwrap_or(&Value::Null)
}

/// The liveness bookends of contract 2, in order.
fn bookends(rig: &Rig) -> Vec<String> {
    rig.actions().into_iter().filter(|a| a.starts_with("dispatch.start") || a == "dispatch.complete" || a == "dispatch.error").collect()
}

// ─── 1. refusals before anything runs ───────────────────────────────────
//
// Each of these returns before `dispatch.start`: no record, no container.

#[test]
#[serial]
fn a_bad_enum_setting_refuses_before_any_record_or_docker_call() {
    own_process!();
    let mut rig = Rig::new();
    rig.env().set("DARKMUX_THERMAL_PAUSE_AT", "bogus");
    let (res, _) = run(&rig, rig.opts());
    let err = format!("{:#}", res.expect_err("a bad enum value refuses the dispatch"));
    assert!(err.contains("DARKMUX_THERMAL_PAUSE_AT") && err.contains("bogus"), "{err}");
    assert!(rig.docker_calls().is_empty(), "docker ran: {:?}", rig.docker_verbs());
    assert!(rig.lms_log().is_empty(), "lms ran: {:?}", rig.lms_log());
    assert!(bookends(&rig).is_empty(), "records: {:?}", rig.actions());
}

#[test]
#[serial]
fn an_unknown_key_in_the_profiles_file_refuses_before_any_record_or_docker_call() {
    own_process!();
    let rig = Rig::new();
    fs::write(
        &rig.profiles,
        r#"{"profiles":{"fast":{"models":[{"id":"model-a","n_ctx":32000}]}},"default_profile":"fast","bogus_key":1}"#,
    )
    .unwrap();
    let (res, _) = run(&rig, rig.opts());
    let err = format!("{:#}", res.expect_err("an unknown key refuses the dispatch"));
    assert!(err.contains("bogus_key"), "{err}");
    assert!(rig.docker_calls().is_empty(), "docker ran: {:?}", rig.docker_verbs());
    assert!(bookends(&rig).is_empty(), "records: {:?}", rig.actions());
}

#[test]
#[serial]
fn a_resume_with_no_checkpoint_is_refused_before_model_selection_or_a_record() {
    own_process!();
    let rig = Rig::new();
    let mut o = rig.opts();
    let nowhere = rig.state.join("not-an-out-dir");
    fs::create_dir_all(&nowhere).unwrap();
    o.resume_from = Some(nowhere);
    let (res, _) = run(&rig, o);
    let err = format!("{:#}", res.expect_err("no checkpoint refuses"));
    assert!(err.starts_with("darkmux dispatch --resume-from: darkmux dispatch: RESUME CHECKPOINT NOT FOUND"), "{err}");
    // ODDITY: an out-dir with no origin record is not refused by the origin
    // check; the image is checked first, and the checkpoint check refuses.
    assert_eq!(rig.docker_verbs(), vec!["image inspect"]);
    assert!(rig.lms_log().is_empty(), "refused before model selection: {:?}", rig.lms_log());
    assert!(bookends(&rig).is_empty(), "records: {:?}", rig.actions());
}

#[test]
#[serial]
fn an_unreachable_docker_daemon_refuses_before_a_record() {
    own_process!();
    let rig = Rig::new();
    rig.fake("docker-version-fail", "Cannot connect to the Docker daemon");
    let mut o = rig.opts();
    o.skip_preflight = false;
    let (res, _) = run(&rig, o);
    let err = format!("{:#}", res.expect_err("no daemon, no dispatch"));
    assert!(err.starts_with("darkmux's runtime requires Docker, but `docker version` failed:"), "{err}");
    assert!(err.contains("Cannot connect to the Docker daemon"), "{err}");
    assert_eq!(rig.docker_verbs(), vec!["version --format".to_string()]);
    assert!(bookends(&rig).is_empty(), "records: {:?}", rig.actions());
}

#[test]
#[serial]
fn skip_preflight_skips_the_daemon_probe_but_not_the_image_check() {
    own_process!();
    let rig = Rig::new();
    rig.fake("docker-version-fail", "Cannot connect to the Docker daemon");
    rig.labels(&[("darkmux-runtime:latest", "0.0.1")]);
    let (res, _) = run(&rig, rig.opts());
    let err = format!("{:#}", res.expect_err("a mismatched image is refused"));
    assert!(err.starts_with("no runtime image for darkmux 5.2.0 is available: the local one was skipped and the pull failed."), "{err}");
    assert!(err.contains("0.0.1"), "{err}");
    let verbs = rig.docker_verbs();
    assert!(!verbs.iter().any(|v| v.starts_with("version")), "{verbs:?}");
    assert!(rig.docker_run().is_none(), "{verbs:?}");
    assert!(bookends(&rig).is_empty(), "records: {:?}", rig.actions());
}

#[test]
#[serial]
fn an_unknown_role_is_refused_after_the_image_check() {
    own_process!();
    let rig = Rig::new();
    let mut o = rig.opts();
    o.role_id = "no-such-role".into();
    let (res, _) = run(&rig, o);
    let err = format!("{:#}", res.expect_err("unknown role"));
    assert_eq!(err, "role not found: no-such-role");
    // ODDITY: the image is inspected (and could be pulled) before the role
    // is even looked up.
    assert_eq!(rig.docker_verbs(), vec!["image inspect".to_string()]);
    assert!(rig.docker_run().is_none());
    assert!(bookends(&rig).is_empty(), "records: {:?}", rig.actions());
}

#[test]
#[serial]
fn a_user_role_without_a_prompt_is_refused() {
    own_process!();
    let rig = Rig::new();
    let roles = rig.state.join("roles");
    fs::create_dir_all(&roles).unwrap();
    fs::write(
        roles.join("pin-noprompt.json"),
        r#"{"schema_version":"1.0","id":"pin-noprompt","description":"d","skills":[],"tool_palette":{"allow":["read"],"deny":[]},"escalation_contract":"bail-with-explanation"}"#,
    )
    .unwrap();
    let mut o = rig.opts();
    o.role_id = "pin-noprompt".into();
    let (res, _) = run(&rig, o);
    let err = format!("{:#}", res.expect_err("no prompt"));
    assert!(err.contains("role 'pin-noprompt' has no readable .md system prompt"), "{err}");
    assert!(rig.docker_run().is_none());
    assert!(bookends(&rig).is_empty(), "records: {:?}", rig.actions());
}

#[test]
#[serial]
fn a_missing_workdir_is_refused_before_model_selection() {
    own_process!();
    let rig = Rig::new();
    let mut o = rig.opts();
    o.workdir = Some(rig.state.join("no-such-dir"));
    let (res, _) = run(&rig, o);
    let err = format!("{:#}", res.expect_err("missing workdir"));
    assert!(err.starts_with("workdir path does not exist or cannot be resolved:"), "{err}");
    assert!(rig.lms_log().is_empty(), "model selection ran: {:?}", rig.lms_log());
    assert!(rig.docker_run().is_none());
    assert!(bookends(&rig).is_empty(), "records: {:?}", rig.actions());
}

#[test]
#[serial]
fn a_placeholder_model_fails_model_selection_before_a_record() {
    own_process!();
    let rig = Rig::new();
    fs::write(&rig.profiles, r#"{"profiles":{"fast":{"models":[{"id":"<your-model>","n_ctx":8192}]}},"default_profile":"fast"}"#)
        .unwrap();
    let (res, _) = run(&rig, rig.opts());
    let err = format!("{:#}", res.expect_err("no model"));
    assert!(err.starts_with("model selection failed: darkmux: profile `fast` still names the placeholder `<your-model>`"), "{err}");
    assert!(rig.docker_run().is_none());
    assert!(bookends(&rig).is_empty(), "records: {:?}", rig.actions());
}

/// ODDITY: a `--profile` this machine does not define is not an error: the
/// dispatch runs on `default_profile`, with one stderr line saying so.
#[test]
#[serial]
fn an_unknown_profile_falls_back_to_the_default_with_a_warning() {
    own_process!();
    let rig = Rig::new();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let mut o = rig.opts();
    o.profile_name = Some("no-such-profile".into());
    let (res, stderr) = run(&rig, o);
    assert_eq!(res.expect("runs on the default").exit_code, 0);
    assert!(
        stderr.contains("requested profile `no-such-profile` is not defined on this machine; using default_profile `fast` instead"),
        "{stderr}"
    );
    assert!(has_pair(&rig.docker_run().unwrap(), "--model", "darkmux:model-a"));
}

#[test]
#[serial]
fn a_model_that_will_not_load_fails_before_a_record() {
    own_process!();
    let rig = Rig::new();
    rig.resident(&[]);
    rig.fake("lms-load-fail", "out of memory");
    let (res, _) = run(&rig, rig.opts());
    let err = format!("{:#}", res.expect_err("load failure"));
    assert!(err.starts_with("model selection failed: darkmux: model `model-a` could not load at n_ctx=32000"), "{err}");
    assert_eq!(rig.lms_log(), vec!["ps --json", "load model-a --context-length 32000 --identifier darkmux:model-a -y"]);
    assert!(rig.docker_run().is_none());
    assert!(bookends(&rig).is_empty(), "records: {:?}", rig.actions());
}

// ─── 2. the local container path, clean exit ────────────────────────────

#[test]
#[serial]
fn a_clean_local_dispatch_returns_the_runtime_result_and_brackets_it_with_start_and_complete() {
    own_process!();
    let rig = Rig::new();
    rig.container(ONE_TURN, ENVELOPE, "runtime stderr line\n", 0);
    let (res, _) = run(&rig, rig.opts());
    let res = res.expect("a clean dispatch");

    assert_eq!(res.exit_code, 0);
    assert_eq!(res.stderr, "runtime stderr line\n", "the container's stderr passes through as-is");
    assert_eq!(res.session_id, session());
    let out = res.out_dir.clone().expect("the container path names its out-dir");
    let fold = res.trajectory.as_ref().expect("the tailer's fold");
    assert_eq!((fold.turns(), fold.model_calls, fold.tokens.total), (2, 2, 43));
    assert_eq!(fs::read_to_string(out.join(".prompt.txt")).unwrap(), "smoke", "the message rides a prompt file");

    // The envelope: the runtime's own fields kept, the container path
    // rewritten to the host path, and the host's summary merged in.
    let env: Value = serde_json::from_str(&res.stdout).expect("--json stdout stays one JSON object");
    assert_eq!(env["result"], "stop");
    assert_eq!(env["final_assistant"], "done");
    assert_eq!(
        env["trajectory_path"].as_str().unwrap(),
        out.join(".darkmux-runtime/trajectory.jsonl").to_str().unwrap()
    );
    assert_eq!(env["metrics"]["model"], "darkmux:model-a");
    assert_eq!((env["metrics"]["turns"].as_u64(), env["metrics"]["total_tokens"].as_u64()), (Some(2), Some(43)));
    assert_eq!(env["bounds"]["inactivity_timeout_seconds"], json!({"value": 600, "source": "built-in"}));
    assert!(env.get("detections").is_some(), "{env}");
    // ODDITY: `host` and `host_window` appear only if the sampler ticked
    // before the container exited; a container this fast can beat it, so
    // they are pinned by the sampler tests, which hold the container open.

    // Contract 2: one start, then exactly one terminal, and the runtime
    // telemetry sibling after it. Every record names this execution.
    let actions = rig.actions();
    let start = actions.iter().position(|a| a == "dispatch.start").unwrap();
    let complete = actions.iter().position(|a| a == "dispatch.complete").unwrap();
    let runtime = actions.iter().position(|a| a == "telemetry.runtime").unwrap();
    assert!(start < complete && complete < runtime, "{actions:?}");
    assert_eq!(bookends(&rig), vec!["dispatch.start", "dispatch.complete"]);
    let exec = res.execution.as_ref().unwrap().to_string();
    for r in rig.flows().iter().filter(|r| r.get("session_id").is_some()) {
        assert_eq!(r["execution_id"], exec.as_str(), "{r}");
        assert_eq!(r["session_id"], session().wire(), "{r}");
    }

    let s = rig.record("dispatch.start");
    assert_eq!(s["model"], "darkmux:model-a");
    assert_eq!(s["handle"], "coder");
    let sp = payload(&s);
    assert_eq!(sp["prompt"], "smoke");
    assert_eq!(sp["image"], "darkmux-runtime:latest");
    assert!(sp.get("endpoint").is_none(), "a local dispatch names no endpoint: {sp}");
    assert_eq!(sp["tools_requested"], json!(["read", "search", "edit", "write", "bash", "create_mod"]));

    let c = rig.record("dispatch.complete");
    let cp = payload(&c);
    assert_eq!(c["level"], "info");
    assert_eq!((cp["exit_code"].as_i64(), cp["result_class"].as_str()), (Some(0), Some("ok")));
    assert_eq!((cp["total_turns"].as_u64(), cp["total_tools"].as_u64()), (Some(2), Some(1)));
    assert_eq!(cp["total_tokens"], 43);
    assert_eq!(cp["live"]["enabled"], false);
    assert_eq!(payload(&rig.record("telemetry.runtime"))["turns"], 2);

    // A clean exit kills nothing: the docker calls are the image check and the run.
    assert_eq!(rig.docker_verbs(), vec!["image inspect", "run --rm"]);

    // The auto workspace is kept for inspection; the shared cache exists.
    let ws = sp["workspace"].as_str().unwrap();
    assert!(Path::new(ws).is_dir(), "{ws}");
    assert!(rig.state.join("cache").is_dir());
    // Each turn's usage names the LM Studio endpoint it went through.
    let tokens: Vec<Value> = rig.flows().into_iter().filter(|r| r["action"] == "telemetry.tokens").collect();
    assert_eq!(tokens.len(), 2);
    assert!(tokens.iter().all(|t| payload(t)["endpoint"] == "http://localhost:1234/v1"), "{tokens:?}");
}

#[test]
#[serial]
fn an_unreachable_daemon_is_nudged_about_once_at_the_top() {
    own_process!();
    let rig = Rig::new();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let (res, stderr) = run(&rig, rig.opts());
    res.unwrap();
    assert!(stderr.starts_with("[!] darkmux serve isn't reachable on 127.0.0.1:"), "{stderr}");
    assert_eq!(stderr.matches("darkmux serve isn't reachable").count(), 1);
}

#[test]
#[serial]
fn phase_and_step_ids_ride_the_records() {
    own_process!();
    let rig = Rig::new();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let mut o = rig.opts();
    o.phase_id = Some("phase-7".into());
    o.step_id = Some("step-9".into());
    let (res, _) = run(&rig, o);
    res.unwrap();
    for action in ["dispatch.start", "dispatch.turn", "telemetry.tokens", "dispatch.complete", "telemetry.runtime"] {
        assert_eq!(rig.record(action)["phase_id"], "phase-7", "{action}");
    }
    // The step id rides the tailer's per-event payloads only.
    assert_eq!(payload(&rig.record("dispatch.turn"))["step_id"], "step-9");
    assert!(payload(&rig.record("dispatch.start")).get("step_id").is_none());
}

#[test]
#[serial]
fn a_configured_turn_delay_reaches_a_local_container_but_never_a_hosted_one() {
    own_process!();
    let mut rig = Rig::new();
    rig.env().set("DARKMUX_TURN_DELAY_MS", "500");
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let (res, _) = run(&rig, rig.opts());
    res.unwrap();
    assert!(has_pair(&rig.docker_run().unwrap(), "-e", "DARKMUX_TURN_DELAY_MS=500"));
    assert_eq!(payload(&rig.record("dispatch.start"))["turn_delay_ms"], 500);

    rig.reset_calls();
    rig.env().set("PIN_HOSTED_KEY", "sk-pin");
    hosted_profiles(&rig, "http://127.0.0.1:9/v1");
    let (res, _) = run(&rig, rig.opts());
    res.unwrap();
    assert!(has_pair(&rig.docker_run().unwrap(), "-e", "DARKMUX_TURN_DELAY_MS=0"));
}

#[test]
#[serial]
fn a_hosted_brains_per_dispatch_cap_is_charged_and_warned() {
    own_process!();
    let mut rig = Rig::new();
    rig.env().set("PIN_HOSTED_KEY", "sk-pin");
    fs::write(
        &rig.profiles,
        json!({
            "profiles": {"cloud": {"models": [{"id": "gpt-remote", "n_ctx": 100000, "endpoint": "mock"}]}},
            "endpoints": {"mock": {"url": "http://127.0.0.1:9/v1", "auth": {"type": "bearer", "key_env": "PIN_HOSTED_KEY"}, "limits": {"tokens_per_dispatch": 10}}},
            "default_profile": "cloud"
        })
        .to_string(),
    )
    .unwrap();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let (res, _) = run(&rig, rig.opts());
    res.unwrap();
    let warn = rig.record("budget.warn");
    assert_eq!(warn["level"], "warn");
    let p = payload(&warn);
    assert_eq!((p["scope"].as_str(), p["spent"].as_u64(), p["limit"].as_u64()), (Some("dispatch"), Some(18), Some(10)));
    assert_eq!(rig.actions().iter().filter(|a| *a == "budget.warn").count(), 1, "warned once");
    let tokens: Vec<Value> = rig.flows().into_iter().filter(|r| r["action"] == "telemetry.tokens").collect();
    assert_eq!(tokens.len(), 2);
    for t in &tokens {
        assert_eq!(
            (payload(t)["endpoint"].as_str(), payload(t)["endpoint_id"].as_str()),
            (Some("openai:127.0.0.1:9/gpt-remote"), Some("mock"))
        );
    }
}

#[test]
#[serial]
fn an_unmanaged_compactors_key_is_piped_beside_nothing_else() {
    own_process!();
    let mut rig = Rig::new();
    rig.env().set("PIN_UTIL_KEY", "sk-util");
    fs::write(
        &rig.profiles,
        json!({
            "schema_version": "2.1",
            "profiles": {"fast": {"models": [{"id": "model-a", "n_ctx": 32000}]}},
            "default_profile": "fast",
            "endpoints": {"mlx-local": {"url": "http://127.0.0.1:8080/v1", "auth": {"type": "bearer", "key_env": "PIN_UTIL_KEY"}}},
            "internal": {"utility": {"id": "mlx-util", "n_ctx": 16384, "endpoint": "mlx-local"}}
        })
        .to_string(),
    )
    .unwrap();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let (res, _) = run(&rig, rig.opts());
    res.unwrap();
    let argv = rig.docker_run().unwrap();
    assert!(argv.contains(&"-i".to_string()) && argv.contains(&"--compactor-auth-header-stdin".to_string()), "{argv:?}");
    assert!(!argv.contains(&"--auth-header-stdin".to_string()), "the local brain needs no secret");
    assert_eq!(rig.stdin_blob().as_deref(), Some(r#"{"compactor":{"header":"Authorization","value":"Bearer sk-util"}}"#));
}


#[test]
#[serial]
fn the_container_argv_carries_the_resolved_dispatch() {
    own_process!();
    let rig = Rig::new();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let (res, _) = run(&rig, rig.opts());
    let res = res.unwrap();
    let argv = rig.docker_run().unwrap();
    let out = res.out_dir.unwrap();
    let exec = res.execution.unwrap().to_string();

    assert_eq!(arg_after(&argv, "--name"), Some("darkmux-dispatch-boundary.solo.adhoc.coder.pin"));
    let fence = argv.iter().position(|a| a == "--").unwrap();
    assert_eq!(argv[fence + 1], "sha256:id-of-darkmux-runtime:latest", "the checked image runs by id");
    assert_eq!(argv[fence + 2], "run");
    assert!(argv.contains(&format!("{}:/darkmux-out", out.display())), "{argv:?}");
    assert!(argv.iter().any(|a| a.ends_with(":/workspace") && a.contains("darkmux-dispatch-coder-")), "{argv:?}");
    assert!(argv.contains(&format!("{}:/darkmux-cache", rig.state.join("cache").display())), "{argv:?}");
    for (flag, value) in [
        ("--model", "darkmux:model-a"),
        ("--role-id", "coder"),
        ("--execution-id", exec.as_str()),
        ("--prompt-file", "/darkmux-out/.prompt.txt"),
        ("--base-url", "http://host.docker.internal:1234/v1"),
        ("--allowed-tools", "read,search,edit,write,bash,create_mod"),
        ("--context-window", "32000"),
    ] {
        assert!(has_pair(&argv, flag, value), "`{flag} {value}` missing: {argv:?}");
    }
    assert!(argv.contains(&"--json".to_string()));
    assert!(!argv.contains(&"smoke".to_string()), "the message never rides argv (#386)");
    assert!(!argv.contains(&"-i".to_string()), "no stdin without a secret to pipe");
    let system = arg_after(&argv, "--system").unwrap();
    assert!(system.starts_with("<!--\nThis file is embedded into specialist-role system prompts"), "the preamble leads a specialist prompt");
    assert!(system.contains("\n\n---\n\n# Coder role"), "{system}");
    for env in [
        "DARKMUX_TURN_DELAY_MS=0",
        "DARKMUX_INACTIVITY_TIMEOUT_SECONDS=600",
        "DARKMUX_INACTIVITY_TIMEOUT_SECONDS_SOURCE=built-in",
        "DARKMUX_MAX_PAUSE_MS=900000",
    ] {
        assert!(has_pair(&argv, "-e", env), "`-e {env}` missing: {argv:?}");
    }
    assert!(!argv.contains(&"--max-turns".to_string()) && !argv.contains(&"--max-tokens".to_string()));
}

#[test]
#[serial]
fn a_text_mode_dispatch_returns_the_runtime_stdout_untouched() {
    own_process!();
    let rig = Rig::new();
    let text = "dispatching to model: m\n\n--- final assistant message ---\nOK\n";
    rig.container(ONE_TURN, text, "", 0);
    let mut o = rig.opts();
    o.json = false;
    let (res, stderr) = run(&rig, o);
    assert_eq!(res.unwrap().stdout, text);
    assert!(!stderr.contains("is not an envelope"), "text mode never parses an envelope (#3164): {stderr}");
    assert!(!rig.docker_run().unwrap().contains(&"--json".to_string()));
}

#[test]
#[serial]
fn an_operator_workdir_and_out_dir_are_mounted_as_given_and_kept() {
    own_process!();
    let rig = Rig::new();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let work = rig.state.join("work");
    let out = rig.state.join("out");
    fs::create_dir_all(&work).unwrap();
    let mut o = rig.opts();
    o.workdir = Some(work.clone());
    o.host_out = Some(out.clone());
    o.workspace_read_only = true;
    let (res, _) = run(&rig, o);
    let res = res.unwrap();
    let work = work.canonicalize().unwrap();
    assert_eq!(res.out_dir.as_deref(), Some(out.as_path()));
    let argv = rig.docker_run().unwrap();
    assert!(argv.contains(&format!("{}:/workspace:ro", work.display())), "{argv:?}");
    assert!(argv.contains(&format!("{}:/darkmux-out", out.display())), "{argv:?}");
    assert_eq!(payload(&rig.record("dispatch.start"))["workspace"], work.display().to_string());
    assert!(work.is_dir() && out.join(".prompt.txt").is_file());
}

#[test]
#[serial]
fn turn_and_timeout_overrides_reach_the_container() {
    own_process!();
    let rig = Rig::new();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let mut o = rig.opts();
    o.max_turns_override = Some(7);
    o.timeout_override_seconds = Some(42);
    let (res, _) = run(&rig, o);
    res.unwrap();
    let argv = rig.docker_run().unwrap();
    assert!(has_pair(&argv, "-e", "DARKMUX_INACTIVITY_TIMEOUT_SECONDS=42"), "{argv:?}");
    assert!(has_pair(&argv, "--max-turns", "7"), "{argv:?}");
    let bounds = payload(&rig.record("dispatch.start"))["bounds"].clone();
    assert_eq!(bounds["inactivity_timeout_seconds"], json!({"value": 42, "source": "cli"}));
    assert_eq!(bounds["max_turns"], json!({"value": 7, "source": "launcher"}));
}

#[test]
#[serial]
fn a_base_url_override_skips_residency_and_addresses_the_bare_model() {
    own_process!();
    let rig = Rig::new();
    rig.resident(&[]);
    rig.fake("lms-load-fail", "must not load");
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let mut o = rig.opts();
    o.model_base_url_override = Some("http://127.0.0.1:9/v1".into());
    let (res, _) = run(&rig, o);
    res.expect("no LM Studio residency on the mock path");
    let argv = rig.docker_run().unwrap();
    // ODDITY: a loopback override reaches the container unchanged, where
    // 127.0.0.1 is the container itself, not this host.
    assert_eq!(arg_after(&argv, "--base-url"), Some("http://127.0.0.1:9/v1"));
    assert!(has_pair(&argv, "--model", "model-a"), "the bare id on the wire: {argv:?}");
    assert!(rig.lms_log().iter().all(|l| !l.starts_with("load")), "{:?}", rig.lms_log());
    assert_eq!(payload(&rig.record("telemetry.tokens"))["endpoint"], "http://127.0.0.1:9/v1", "usage names the override");
}

#[test]
#[serial]
fn a_model_that_is_not_resident_is_loaded_namespaced_at_its_context() {
    own_process!();
    let rig = Rig::new();
    rig.resident(&[]);
    rig.fake("lms-ps-after-load.json", lms_ps(&[("darkmux:model-a", "model-a", 32000)]));
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let (res, _) = run(&rig, rig.opts());
    res.unwrap();
    let lms = rig.lms_log();
    assert!(lms.contains(&"load model-a --context-length 32000 --identifier darkmux:model-a -y".to_string()), "{lms:?}");
}

#[test]
#[serial]
fn operator_identity_is_appended_to_a_local_system_prompt() {
    own_process!();
    let rig = Rig::new();
    fs::write(rig.state.join("identity.md"), "PIN-IDENTITY-MARKER").unwrap();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let (res, _) = run(&rig, rig.opts());
    res.unwrap();
    let argv = rig.docker_run().unwrap();
    assert!(arg_after(&argv, "--system").unwrap().contains("PIN-IDENTITY-MARKER"));
}

#[test]
#[serial]
fn a_workdir_whose_git_lives_outside_the_mount_warns_records_and_tells_the_model() {
    own_process!();
    let rig = Rig::new();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let work = rig.state.join("wt");
    fs::create_dir_all(&work).unwrap();
    fs::write(work.join(".git"), "gitdir: /somewhere/else/.git/worktrees/wt\n").unwrap();
    let mut o = rig.opts();
    o.workdir = Some(work);
    let (res, stderr) = run(&rig, o);
    res.unwrap();
    assert!(stderr.contains("is a git WORKTREE, not an ordinary checkout"), "{stderr}");
    // The record lands before `dispatch.start`, and is not a bookend.
    let actions = rig.actions();
    let warn = actions.iter().position(|a| a == "dispatch.workdir_git_unavailable").expect("the git record");
    assert!(warn < actions.iter().position(|a| a == "dispatch.start").unwrap(), "{actions:?}");
    let rec = rig.record("dispatch.workdir_git_unavailable");
    assert_eq!(rec["level"], "warn");
    let p = payload(&rec);
    assert_eq!(p["issue"], 2294);
    assert_eq!(p["checkouts"][0]["gitdir_target"], "/somewhere/else/.git/worktrees/wt");
    assert_eq!(p["checkouts"][0]["container_path"], "/workspace");
    let system = arg_after(&rig.docker_run().unwrap(), "--system").unwrap().to_string();
    assert!(system.contains("workspace-note"), "the model is told: {system}");
}

#[test]
#[serial]
fn a_utility_role_gets_no_preamble_and_unknown_tool_tokens_are_named() {
    own_process!();
    let rig = Rig::new();
    let roles = rig.state.join("roles");
    fs::create_dir_all(&roles).unwrap();
    fs::write(
        roles.join("pin-util.json"),
        r#"{"schema_version":"1.0","id":"pin-util","role_family":"utility","description":"d","skills":[],"tool_palette":{"allow":["read","exce"],"deny":[]},"escalation_contract":"bail-with-explanation"}"#,
    )
    .unwrap();
    fs::write(roles.join("pin-util.md"), "# PIN UTIL PROMPT\n").unwrap();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let mut o = rig.opts();
    o.role_id = "pin-util".into();
    let (res, stderr) = run(&rig, o);
    res.unwrap();
    let argv = rig.docker_run().unwrap();
    assert_eq!(arg_after(&argv, "--system"), Some("# PIN UTIL PROMPT\n"), "no preamble for a utility role");
    assert_eq!(arg_after(&argv, "--allowed-tools"), Some("read,search"), "the unknown token is dropped");
    assert!(stderr.contains("darkmux dispatch: role `pin-util` declares unknown tool-vocab tokens: [exce]"), "{stderr}");
}

#[test]
#[serial]
fn a_role_with_feedback_templates_forwards_them() {
    own_process!();
    let rig = Rig::new();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let mut o = rig.opts();
    o.role_id = "code-reviewer".into();
    let (res, _) = run(&rig, o);
    res.unwrap();
    let argv = rig.docker_run().unwrap();
    let fb = arg_after(&argv, "--feedback-templates-json").expect("templates forwarded");
    let v: Value = serde_json::from_str(fb).unwrap();
    assert!(v.is_object() || v.is_array(), "{fb}");
}

#[test]
#[serial]
fn a_non_darkmux_image_gets_the_runtime_binary_injected() {
    own_process!();
    let rig = Rig::new();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let mut o = rig.opts();
    o.image = Some("rust:slim".into());
    let (res, stderr) = run(&rig, o);
    res.unwrap();
    let _ = stderr;
    assert_eq!(
        rig.docker_verbs(),
        vec!["image inspect", "image inspect", "create --", "cp fakecid:/usr/local/bin/darkmux-runtime", "rm -f", "run --rm"],
        "the darkmux image is checked, then its binary extracted"
    );
    let argv = rig.docker_run().unwrap();
    let fence = argv.iter().position(|a| a == "--").unwrap();
    assert_eq!(argv[fence + 1], "rust:slim", "the operator's image runs by its ref");
    assert!(has_pair(&argv, "--entrypoint", "/darkmux-runtime"), "{argv:?}");
    let bin = rig.state.join("runtime/darkmux-runtime");
    assert!(argv.contains(&format!("{}:/darkmux-runtime:ro", bin.display())), "{argv:?}");
    assert_eq!(payload(&rig.record("dispatch.start"))["image"], "rust:slim");
}

#[test]
#[serial]
fn a_json_dispatch_whose_stdout_is_not_an_envelope_says_so_and_passes_it_through() {
    own_process!();
    let rig = Rig::new();
    rig.container(ONE_TURN, "not json\n", "", 0);
    let (res, stderr) = run(&rig, rig.opts());
    assert_eq!(res.unwrap().stdout, "not json\n");
    assert!(stderr.contains("darkmux: the runtime's stdout is not an envelope this darkmux reads"), "{stderr}");
}

#[test]
#[serial]
fn a_live_channel_dispatch_says_so_on_its_terminal_record() {
    own_process!();
    let rig = Rig::new();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let mut o = rig.opts();
    o.live_channel = true;
    let (res, _) = run(&rig, o);
    res.unwrap();
    assert_eq!(payload(&rig.record("dispatch.complete"))["live"]["enabled"], true);
}

// ─── 3. every exit after `dispatch.start` closes it (contract 2) ────────

#[test]
#[serial]
fn a_failing_container_still_returns_its_result_closes_with_an_error_and_is_killed_by_name() {
    own_process!();
    let rig = Rig::new();
    rig.container(ONE_TURN, r#"{"result":"error","final_assistant":""}"#, "boom\n", 3);
    let (res, _) = run(&rig, rig.opts());
    let res = res.expect("a container that ran is a result, not an Err");
    assert_eq!(res.exit_code, 3);
    assert_eq!(res.stderr, "boom\n");
    assert_eq!(bookends(&rig), vec!["dispatch.start", "dispatch.error"]);
    let term = rig.record("dispatch.error");
    assert_eq!(term["level"], "error");
    let p = payload(&term);
    assert_eq!((p["exit_code"].as_i64(), p["result_class"].as_str()), (Some(3), Some("error")));
    assert_eq!(p["stderr_excerpt"], "boom");
    assert_eq!(p["total_turns"], 2, "the turns it finished are reported");
    assert_eq!(payload(&rig.record("telemetry.runtime"))["turns"], 2);
    // A non-success exit leaves the kill guard armed: one kill by name.
    assert_eq!(rig.docker_verbs(), vec!["image inspect", "run --rm", "kill darkmux-dispatch-boundary.solo.adhoc.coder.pin"]);
    assert!(res.out_dir.unwrap().join(".prompt.txt").is_file(), "the out-dir is kept");
}

#[test]
#[serial]
fn an_error_after_start_closes_with_dispatch_error_and_reclaims_the_auto_workspace() {
    own_process!();
    let rig = Rig::new();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let mut o = rig.opts();
    o.image = Some("bad image".into());
    let (res, _) = run(&rig, o);
    let err = format!("{:#}", res.expect_err("an invalid image ref"));
    assert_eq!(err, "image ref `bad image` contains whitespace or control characters — refusing");
    assert_eq!(bookends(&rig), vec!["dispatch.start", "dispatch.error"]);
    assert!(rig.docker_run().is_none());
    let term = rig.record("dispatch.error");
    // ODDITY: the record carries the bookend guard's generic text, not the
    // error the caller receives.
    assert_eq!(
        payload(&term),
        &json!({"result_class": "error", "error": "dispatch terminated before completion (early return or panic)", "total_turns": 0})
    );
    // ODDITY: the darkmux image's binary is extracted before the operator's
    // image ref is validated.
    assert_eq!(
        rig.docker_verbs(),
        vec!["image inspect", "image inspect", "create --", "cp fakecid:/usr/local/bin/darkmux-runtime", "rm -f"]
    );
    // The auto workspace is reclaimed; the out-dir (and its origin record) stays.
    let mut left: Vec<String> = fs::read_dir(rig.state.join("tmp"))
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    left.sort();
    assert_eq!(left.len(), 3, "{left:?}");
    assert!(left.iter().all(|n| n.starts_with("darkmux-out-coder-")), "{left:?}");
    assert!(left.iter().any(|n| n.ends_with(".execution.lock")) && left.iter().any(|n| n.ends_with(".resume_origin.json")), "{left:?}");
}

#[test]
#[serial]
fn an_interrupt_mid_run_kills_the_container_and_closes_with_an_error() {
    own_process!();
    let rig = Rig::new();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    // `exec`: the registered pid IS the sleeping "container", so the
    // tailer's SIGKILL ends it and leaves no orphan holding the pipes.
    rig.fake("run-script", "exec sleep 30\n");
    let raiser = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(800));
        darkmux_types::interrupt::raise_for_test();
    });
    let started = std::time::Instant::now();
    let (res, _) = run(&rig, rig.opts());
    raiser.join().unwrap();
    let err = format!("{:#}", res.expect_err("an interrupted run is an Err"));
    let _ = started;
    assert_eq!(
        err,
        "darkmux-runtime container dispatch interrupted by an operator signal (SIGINT/SIGTERM/SIGHUP): \
         the container `darkmux-dispatch-boundary.solo.adhoc.coder.pin` was killed mid-run"
    );
    // Closed with the error that ended it and the turns it finished; no
    // `telemetry.runtime` on this exit.
    assert_eq!(bookends(&rig), vec!["dispatch.start", "dispatch.error"]);
    let p = payload(&rig.record("dispatch.error")).clone();
    assert_eq!(p["error"].as_str(), Some(err.as_str()));
    assert_eq!((p["stop_reason"].as_str(), p["total_turns"].as_u64()), (Some("interrupted"), Some(2)));
    assert!(!rig.actions().contains(&"telemetry.runtime".to_string()), "{:?}", rig.actions());
    let kills = rig.docker_verbs().iter().filter(|v| v.starts_with("kill ")).count();
    assert_eq!(kills, 2, "the teardown kill and the armed guard's: {:?}", rig.docker_verbs());
}

#[test]
#[serial]
fn an_inactivity_timeout_marks_stderr_and_kills_by_name() {
    own_process!();
    let rig = Rig::new();
    rig.container(ONE_TURN, ENVELOPE, "partial\n", 137);
    rig.fake("run-script", "sleep 6\n");
    let mut o = rig.opts();
    o.timeout_override_seconds = Some(1);
    let started = std::time::Instant::now();
    let (res, _) = run(&rig, o);
    let res = res.expect("a timed-out container is still a result");
    let _ = started;
    assert_eq!(res.exit_code, 137);
    assert!(
        res.stderr.starts_with(
            "darkmux dispatch: INACTIVITY TIMEOUT — no proof-of-work signal in 1s — container \
             `darkmux-dispatch-boundary.solo.adhoc.coder.pin` was killed by the watchdog."
        ),
        "{}",
        res.stderr
    );
    assert!(res.stderr.contains("This dispatch's budget came from `--timeout`"), "{}", res.stderr);
    assert!(res.stderr.ends_with("\npartial\n"), "the container's own stderr follows the marker: {}", res.stderr);
    assert_eq!(bookends(&rig), vec!["dispatch.start", "dispatch.error"]);
    assert_eq!(payload(&rig.record("dispatch.error"))["exit_code"], 137);
    assert_eq!(
        rig.docker_verbs(),
        vec![
            "image inspect",
            "run --rm",
            "kill darkmux-dispatch-boundary.solo.adhoc.coder.pin",
            "kill darkmux-dispatch-boundary.solo.adhoc.coder.pin"
        ],
        "the watchdog's kill, then the armed guard's"
    );
}

// ─── 4. routing: hosted endpoints ───────────────────────────────────────

fn hosted_profiles(rig: &Rig, url: &str) {
    fs::write(
        &rig.profiles,
        json!({
            "profiles": {"cloud": {"models": [{"id": "gpt-remote", "n_ctx": 100000, "endpoint": "mock"}]}},
            "endpoints": {"mock": {"url": url, "auth": {"type": "bearer", "key_env": "PIN_HOSTED_KEY"}}},
            "default_profile": "cloud"
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
#[serial]
fn a_tool_less_role_on_a_hosted_endpoint_runs_one_http_call_and_no_container() {
    own_process!();
    use httpmock::prelude::*;
    let mut rig = Rig::new();
    rig.env().set("PIN_HOSTED_KEY", "sk-pin");
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200).header("content-type", "application/json").json_body(json!({
            "choices": [{"message": {"role": "assistant", "content": "hosted ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        }));
    });
    hosted_profiles(&rig, &server.url("/v1"));
    let mut o = rig.opts();
    o.role_id = "pr-reviewer".into();
    let (res, _) = run(&rig, o);
    let res = res.expect("the hosted single-shot");
    mock.assert_hits(1);
    assert!(rig.docker_calls().is_empty(), "no container on the single-shot path: {:?}", rig.docker_verbs());
    assert!(rig.lms_log().is_empty(), "no LM Studio: {:?}", rig.lms_log());
    assert_eq!((res.exit_code, res.out_dir.as_ref(), res.trajectory.as_ref()), (0, None, None));
    let env: Value = serde_json::from_str(&res.stdout).unwrap();
    let label = format!("openai:127.0.0.1:{}/gpt-remote", server.port());
    assert_eq!(env["final_assistant"], "hosted ok");
    assert_eq!(env["metrics"]["endpoint"], label.as_str());
    assert_eq!(rig.actions()[1..], ["dispatch.start", "telemetry.tokens", "dispatch.complete"]);
    let s = rig.record("dispatch.start");
    assert_eq!((s["handle"].as_str(), s["model"].as_str()), (Some("pr-reviewer"), Some("gpt-remote")));
    assert_eq!(payload(&s)["endpoint"], label.as_str());
    let c = payload(&rig.record("dispatch.complete")).clone();
    assert_eq!((c["exit_code"].as_i64(), c["total_tokens"].as_u64()), (Some(0), Some(5)));
}

#[test]
#[serial]
fn a_resume_on_a_tool_less_hosted_role_is_refused_before_the_http_call() {
    own_process!();
    use httpmock::prelude::*;
    let mut rig = Rig::new();
    rig.env().set("PIN_HOSTED_KEY", "sk-pin");
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200).json_body(json!({"choices": [{"message": {"content": "x"}}]}));
    });
    hosted_profiles(&rig, &server.url("/v1"));
    let mut o = rig.opts();
    o.role_id = "pr-reviewer".into();
    o.resume_from = Some(rig.state.join("whatever"));
    let (res, _) = run(&rig, o);
    let err = format!("{:#}", res.expect_err("refused"));
    assert!(err.contains("not supported on the unmanaged-endpoint single-shot dispatch path"), "{err}");
    mock.assert_hits(0);
    assert!(rig.docker_calls().is_empty());
    assert!(bookends(&rig).is_empty(), "records: {:?}", rig.actions());
}

#[test]
#[serial]
fn a_tool_granting_role_on_a_hosted_endpoint_runs_in_the_container_with_the_secret_on_stdin() {
    own_process!();
    let mut rig = Rig::new();
    rig.env().set("PIN_HOSTED_KEY", "sk-pin");
    fs::write(rig.state.join("identity.md"), "PIN-IDENTITY-MARKER").unwrap();
    hosted_profiles(&rig, "http://127.0.0.1:9/v1");
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let (res, _) = run(&rig, rig.opts());
    res.expect("the agentic-remote container path");
    let argv = rig.docker_run().unwrap();
    let rt = argv.iter().position(|a| a == "--").unwrap();
    assert!(argv[..rt].contains(&"-i".to_string()), "stdin is opened for the secret: {argv:?}");
    for (flag, value) in [
        ("--model", "gpt-remote"),
        ("--chat-url", "http://127.0.0.1:9/v1/chat/completions"),
        ("--context-window", "100000"),
        // ODDITY: the LM Studio base still rides along on a hosted brain.
        ("--base-url", "http://host.docker.internal:1234/v1"),
    ] {
        assert!(has_pair(&argv, flag, value), "`{flag} {value}` missing: {argv:?}");
    }
    assert!(argv.contains(&"--auth-header-stdin".to_string()));
    assert!(!argv.iter().any(|a| a.contains("sk-pin")), "the secret never rides argv");
    assert_eq!(rig.stdin_blob().as_deref(), Some(r#"{"header":"Authorization","value":"Bearer sk-pin"}"#));
    let sp = payload(&rig.record("dispatch.start")).clone();
    assert_eq!(sp["endpoint"], "openai:127.0.0.1:9/gpt-remote");
    assert_eq!(sp["bounds"]["turn_delay_ms"]["source"], "forced-agentic-remote");
    assert!(rig.lms_log().iter().all(|l| l == "ps --json"), "no residency for a hosted brain: {:?}", rig.lms_log());
    assert!(!arg_after(&argv, "--system").unwrap().contains("PIN-IDENTITY-MARKER"), "identity never leaves the machine");
}

#[test]
#[serial]
fn force_container_puts_a_tool_less_hosted_role_in_the_container() {
    own_process!();
    let mut rig = Rig::new();
    rig.env().set("PIN_HOSTED_KEY", "sk-pin");
    hosted_profiles(&rig, "http://127.0.0.1:9/v1");
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let mut o = rig.opts();
    o.role_id = "pr-reviewer".into();
    o.force_container = true;
    let (res, _) = run(&rig, o);
    res.expect("forced into the container");
    assert!(rig.docker_run().is_some());
}

// ─── 5. compaction staffing ─────────────────────────────────────────────

#[test]
#[serial]
fn a_managed_utility_model_is_loaded_at_its_own_window_before_the_container() {
    own_process!();
    let rig = Rig::new();
    fs::write(
        &rig.profiles,
        json!({
            "profiles": {"fast": {"models": [{"id": "model-a", "n_ctx": 32000}]}},
            "default_profile": "fast",
            "internal": {"utility": {"id": "util-m", "n_ctx": 16384}}
        })
        .to_string(),
    )
    .unwrap();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let (res, stderr) = run(&rig, rig.opts());
    res.unwrap();
    let argv = rig.docker_run().unwrap();
    assert!(!stderr.contains("no compactor is bound"), "{stderr}");
    let lms = rig.lms_log();
    let load = lms.iter().position(|l| l == "load util-m --context-length 16384 --identifier darkmux:util-m -y");
    assert!(load.is_some(), "the utility loads at ITS window (#1616): {lms:?}");
    assert!(has_pair(&argv, "--compactor-model", "darkmux:util-m"), "{argv:?}");
    assert!(has_pair(&argv, "--compactor-context-window", "16384"), "{argv:?}");
    assert!(has_pair(&argv, "--context-window", "32000"), "{argv:?}");
}

#[test]
#[serial]
fn no_bound_compactor_is_said_out_loud() {
    own_process!();
    let rig = Rig::new();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let (res, stderr) = run(&rig, rig.opts());
    res.unwrap();
    assert!(stderr.contains("no compactor is bound for this dispatch"), "{stderr}");
    assert!(!rig.docker_run().unwrap().contains(&"--compactor-model".to_string()));
}

#[test]
#[serial]
fn a_utility_model_on_an_unmanaged_endpoint_compacts_there_and_is_never_loaded() {
    own_process!();
    let rig = Rig::new();
    fs::write(
        &rig.profiles,
        json!({
            "schema_version": "2.1",
            "profiles": {"fast": {"models": [{"id": "model-a", "n_ctx": 32000}]}},
            "default_profile": "fast",
            "endpoints": {"mlx-local": {"url": "http://127.0.0.1:8080/v1", "dialect": "chat-completions-max-tokens"}},
            "internal": {"utility": {"id": "mlx-util", "n_ctx": 16384, "endpoint": "mlx-local"}}
        })
        .to_string(),
    )
    .unwrap();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let (res, _) = run(&rig, rig.opts());
    res.unwrap();
    let argv = rig.docker_run().unwrap();
    for (flag, value) in [
        ("--compactor-model", "mlx-util"),
        ("--compactor-chat-url", "http://host.docker.internal:8080/v1/chat/completions"),
        ("--compactor-dialect", "chat-completions-max-tokens"),
        ("--compactor-context-window", "16384"),
    ] {
        assert!(has_pair(&argv, flag, value), "`{flag} {value}` missing: {argv:?}");
    }
    assert!(rig.lms_log().iter().all(|l| !l.contains("mlx-util")), "{:?}", rig.lms_log());
}

// ─── 6. resume ──────────────────────────────────────────────────────────

const CHECKPOINT: &str = r#"{"schema_version":3,"role_id":"coder","messages":[]}"#;

#[test]
#[serial]
fn a_resume_stages_the_origin_checkpoint_and_a_clean_finish_closes_the_origin() {
    own_process!();
    let rig = Rig::new();
    let work = rig.state.join("work");
    fs::create_dir_all(&work).unwrap();

    // 1. An origin run that leaves a checkpoint and does not finish cleanly.
    rig.container(ONE_TURN, ENVELOPE, "", 1);
    rig.fake("run-script", format!("printf '%s' '{CHECKPOINT}' > \"$out/checkpoint.json\"\n"));
    let mut o = rig.opts();
    o.workdir = Some(work.clone());
    let (first, _) = run(&rig, o);
    let origin = first.unwrap().out_dir.unwrap();
    assert!(origin.join("checkpoint.json").is_file());
    let origin_meta = |dir: &Path| -> Value {
        let mut name = dir.file_name().unwrap().to_os_string();
        name.push(".resume_origin.json");
        serde_json::from_str(&fs::read_to_string(dir.with_file_name(name)).unwrap()).unwrap()
    };
    assert_eq!(origin_meta(&origin)["container"], "darkmux-dispatch-boundary.solo.adhoc.coder.pin");

    // 2. Its resume: the checkpoint lands in the NEW out-dir and the
    // container is told to reload it.
    rig.reset_calls();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let mut o = rig.opts();
    o.workdir = Some(work.clone());
    o.resume_from = Some(origin.clone());
    let (second, stderr) = run(&rig, o);
    let second = second.expect("a valid resume runs");
    let out = second.out_dir.clone().unwrap();
    assert_ne!(out, origin);
    assert_eq!(fs::read_to_string(out.join("checkpoint.json")).unwrap(), CHECKPOINT);
    assert!(
        stderr.contains(&format!("darkmux dispatch: resuming from checkpoint at {} (staged into {})", origin.display(), out.display())),
        "{stderr}"
    );
    let argv = rig.docker_run().unwrap();
    assert!(has_pair(&argv, "--resume", "/darkmux-out/checkpoint.json"), "{argv:?}");
    // A resume continues its origin's execution: the same id on both.
    let starts: Vec<Value> = rig.flows().into_iter().filter(|r| r["action"] == "dispatch.start").collect();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[0]["execution_id"], starts[1]["execution_id"]);
    assert_eq!(second.execution.as_ref().unwrap().to_string(), starts[0]["execution_id"].as_str().unwrap());
    assert!(payload(&starts[0]).get("resumed_from").is_none());
    assert_eq!(payload(&starts[1])["resumed_from"], origin.display().to_string());
    // The resume's container is recorded on its origin as well.
    assert_eq!(origin_meta(&origin)["containers"], json!(["darkmux-dispatch-boundary.solo.adhoc.coder.pin"]));

    // 3. The resume finished cleanly, so its origin is complete: a second
    // resume of it is refused before any container.
    rig.reset_calls();
    let mut o = rig.opts();
    o.workdir = Some(work);
    o.resume_from = Some(origin);
    let (third, _) = run(&rig, o);
    let err = format!("{:#}", third.expect_err("a completed origin cannot be resumed"));
    assert!(err.contains("RESUME ALREADY COMPLETED"), "{err}");
    assert!(rig.docker_calls().is_empty(), "refused before the image check: {:?}", rig.docker_verbs());
}

// ─── 7. the telemetry sampler, seen through the dispatch ────────────────
//
// The sampler ticks every 2s of real time on its own thread. These hold the
// fake container open long enough for at least one more tick and assert what
// was recorded, never how many ticks ran.

fn telemetry_lms(rig: &Rig) -> Vec<Value> {
    rig.flows().into_iter().filter(|r| r["action"] == "telemetry.lms").map(|r| payload(&r).clone()).collect()
}

#[test]
#[serial]
fn the_first_lms_sample_is_a_baseline_tagged_by_seat() {
    own_process!();
    let rig = Rig::new();
    fs::write(
        &rig.profiles,
        json!({
            "profiles": {"fast": {"models": [{"id": "model-a", "n_ctx": 32000}]}},
            "default_profile": "fast",
            "internal": {"utility": {"id": "util-m", "n_ctx": 16384}}
        })
        .to_string(),
    )
    .unwrap();
    rig.resident(&[("darkmux:model-a", "model-a", 32000), ("darkmux:util-m", "util-m", 16384), ("users-own", "users-own", 4096)]);
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    rig.fake("run-script", "sleep 1\n");
    let (res, _) = run(&rig, rig.opts());
    res.unwrap();
    assert_eq!(
        telemetry_lms(&rig),
        vec![
            json!({"event": "load", "model": "model-a", "gb": 1, "role": "primary", "baseline": true}),
            json!({"event": "load", "model": "util-m", "gb": 1, "role": "utility", "baseline": true}),
            json!({"event": "load", "model": "users-own", "gb": 1, "role": "resident", "baseline": true}),
        ]
    );
}

#[test]
#[serial]
fn a_residency_change_mid_run_is_recorded_as_an_unload_and_a_load() {
    own_process!();
    let rig = Rig::new();
    rig.fake("lms-ps-mid.json", lms_ps(&[("darkmux:model-b", "model-b", 8192)]));
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    rig.fake("run-script", "sleep 1\ncp \"$FAKE_DOCKER_STATE_DIR/lms-ps-mid.json\" \"$FAKE_DOCKER_STATE_DIR/lms-ps.json\"\nsleep 3\n");
    let (res, _) = run(&rig, rig.opts());
    res.unwrap();
    assert_eq!(
        telemetry_lms(&rig),
        vec![
            json!({"event": "load", "model": "model-a", "gb": 1, "role": "primary", "baseline": true}),
            json!({"event": "load", "model": "model-b", "gb": 1, "role": "resident"}),
            json!({"event": "unload", "model": "model-a", "role": "primary"}),
        ],
        "a baseline, then only the changes"
    );
}

fn scenario(rig: &mut Rig, lines: &str) -> PathBuf {
    let path = rig.state.join("scenario.jsonl");
    fs::write(&path, lines).unwrap();
    rig.env().set("DARKMUX_HOST_SOURCE_SCRIPT", &path);
    path
}

fn rests(rig: &Rig) -> Vec<Value> {
    rig.flows().into_iter().filter(|r| r["action"] == "dispatch.rest").collect()
}

#[test]
#[serial]
fn a_critical_machine_trips_the_breaker_writes_the_pace_file_and_ejects_darkmux_models() {
    own_process!();
    let mut rig = Rig::new();
    let script = scenario(&mut rig, r#"{"hold_ms": 600000, "thermal": {"state": "critical"}}"#);
    rig.resident(&[("darkmux:model-a", "model-a", 32000), ("users-own", "users-own", 4096)]);
    rig.fake("lms-ps-after-unload.json", lms_ps(&[("users-own", "users-own", 4096)]));
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    rig.fake("run-script", "sleep 3\n");
    let (res, stderr) = run(&rig, rig.opts());
    let out = res.unwrap().out_dir.unwrap();
    let sim = script.display().to_string();
    assert!(stderr.contains("darkmux: ⚠ host source — SIMULATED host readings"), "{stderr}");
    let r = rests(&rig);
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(
        payload(&r[0]),
        &json!({"reason": "thermal-critical", "state": "critical", "pause": true, "simulated_host_source": sim})
    );
    let pace: Value = serde_json::from_str(&fs::read_to_string(out.join("pace.json")).unwrap()).unwrap();
    assert_eq!((pace["pause"].as_bool(), pace["reason"].as_str()), (Some(true), Some("thermal-critical")));
    // Tier 5: every darkmux-owned resident is unloaded; the user's is not.
    let unloads: Vec<String> = rig.lms_log().into_iter().filter(|l| l.starts_with("unload")).collect();
    assert_eq!(unloads, vec!["unload darkmux:model-a"]);
    let eject = payload(&rig.record("thermal.tier5_eject")).clone();
    assert_eq!(eject["ejected"], json!([{"identifier": "darkmux:model-a", "context": 32000}]), "{eject}");
    assert_eq!(eject["user_loaded_count"], 1);
    let hw = payload(&rig.record("dispatch.complete"))["host_window"].clone();
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        assert_eq!((hw["thermal_worst_state"].as_str(), hw["simulated_host_source"].as_str()), (Some("critical"), Some(sim.as_str())));
    } else {
        // ODDITY: the host window exists only when the CPU/RAM/GPU probe read
        // something, so on a host with no such probe even a scripted
        // `critical` never reaches it.
        assert!(hw.is_null(), "{hw}");
    }
}

#[test]
#[serial]
fn a_serious_machine_pauses_the_run_and_resumes_it_when_nominal() {
    own_process!();
    let mut rig = Rig::new();
    rig.env().set("DARKMUX_THERMAL_RESUME_HOLD_MS", "1");
    scenario(&mut rig, "{\"hold_ms\": 2500, \"thermal\": {\"state\": \"serious\"}}\n{\"hold_ms\": 600000, \"thermal\": {\"state\": \"nominal\"}}\n");
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    rig.fake("run-script", "sleep 7\n");
    let (res, _) = run(&rig, rig.opts());
    let out = res.unwrap().out_dir.unwrap();
    let r: Vec<(String, String, bool)> = rests(&rig)
        .iter()
        .map(|r| {
            let p = payload(r);
            (p["reason"].as_str().unwrap().into(), p["state"].as_str().unwrap().into(), p["pause"].as_bool().unwrap())
        })
        .collect();
    assert_eq!(r, vec![("thermal".into(), "serious".into(), true), ("thermal".into(), "nominal".into(), false)]);
    assert!(rests(&rig).iter().all(|r| r["level"] == "info"), "routine pacing is info");
    let pace: Value = serde_json::from_str(&fs::read_to_string(out.join("pace.json")).unwrap()).unwrap();
    assert_eq!(pace["pause"], false, "{pace}");
}

#[test]
#[serial]
fn a_fair_machine_enters_the_duty_cycle_and_leaves_it_when_nominal() {
    own_process!();
    let mut rig = Rig::new();
    rig.env().set("DARKMUX_THERMAL_RESUME_HOLD_MS", "1");
    scenario(&mut rig, "{\"hold_ms\": 2500, \"thermal\": {\"state\": \"fair\"}}\n{\"hold_ms\": 600000, \"thermal\": {\"state\": \"nominal\"}}\n");
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    rig.fake("run-script", "sleep 7\n");
    let (res, _) = run(&rig, rig.opts());
    res.unwrap();
    let r: Vec<Value> = rests(&rig).iter().map(|r| payload(r).clone()).collect();
    assert_eq!(r.len(), 2, "{r:?}");
    assert_eq!((r[0]["reason"].as_str(), r[0]["state"].as_str(), r[0]["pause"].as_bool()), (Some("thermal-duty-cycle"), Some("fair"), Some(false)));
    assert_eq!(r[0]["delay_ms"], 15000, "the entry carries its delay");
    assert_eq!((r[1]["reason"].as_str(), r[1]["state"].as_str(), r[1]["pause"].as_bool()), (Some("thermal-duty-cycle"), Some("nominal"), Some(false)));
    assert!(r[1].get("delay_ms").is_none(), "{:?}", r[1]);
}

#[test]
#[serial]
fn a_low_battery_pauses_the_run() {
    own_process!();
    let mut rig = Rig::new();
    scenario(&mut rig, r#"{"hold_ms": 600000, "battery": {"charge_pct": 10}}"#);
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    rig.fake("run-script", "sleep 3\n");
    let (res, _) = run(&rig, rig.opts());
    res.unwrap();
    let r = rests(&rig);
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0]["level"], "info");
    let p = payload(&r[0]);
    assert_eq!((p["reason"].as_str(), p["state"].as_str(), p["pause"].as_bool()), (Some("battery"), Some("10% (floor 50%)"), Some(true)));
}

#[test]
#[serial]
fn the_episode_limit_holds_the_run_for_the_operator() {
    own_process!();
    let mut rig = Rig::new();
    rig.env().set("DARKMUX_THERMAL_EPISODE_THRESHOLD", "1");
    scenario(&mut rig, r#"{"hold_ms": 600000, "thermal": {"state": "serious"}}"#);
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    rig.fake("run-script", "sleep 3\n");
    let (res, stderr) = run(&rig, rig.opts());
    let out = res.unwrap().out_dir.unwrap();
    assert!(stderr.contains("darkmux: this machine reached `serious` 1 time(s) this mission — pausing indefinitely"), "{stderr}");
    let r = rests(&rig);
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0]["level"], "warn", "an operator-actionable hold");
    let p = payload(&r[0]);
    assert_eq!((p["reason"].as_str(), p["episode"].as_u64(), p["pause"].as_bool()), (Some("thermal-episode-limit"), Some(1), Some(true)));
    assert!(p["checklist"].as_str().unwrap().starts_with("worth checking: ambient temperature"));
    let hint = p["resume_hint"].as_str().unwrap();
    assert!(hint.starts_with(&format!("darkmux dispatch coder --resume-from {} --workdir ", out.display())), "{hint}");
}

#[test]
#[serial]
fn a_dispatch_emits_the_machine_curve_only_when_no_one_else_holds_the_sampler_lock() {
    own_process!();
    let rig = Rig::new();
    let lock = rig.state.join("liveness/host-sampler.lock");
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    // Held by a live process (this test's parent), heartbeating now.
    let parent = std::os::unix::process::parent_id();
    fs::write(
        &lock,
        json!({"pid": parent, "machine_uid": null, "started_ts_ms": now, "heartbeat_ts_ms": now, "interval_ms": 600000, "owner": "daemon"}).to_string(),
    )
    .unwrap();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let (res, _) = run(&rig, rig.opts());
    res.unwrap();
    assert!(!rig.actions().contains(&"machine.telemetry".to_string()), "{:?}", rig.actions());
    // The same dispatch with the lock free is the machine's sampler.
    fs::remove_file(&lock).unwrap();
    rig.fake("run-script", "sleep 1\n");
    let (res, _) = run(&rig, rig.opts());
    res.unwrap();
    let curve = rig.record("machine.telemetry");
    assert_eq!(curve["source"], "host");
    assert!(payload(&curve).get("sampled_at_ms").is_some(), "{curve}");
}

// ─── 8. further early returns, each side of `dispatch.start` ────────────

#[test]
#[serial]
fn a_malformed_user_role_file_fails_role_loading() {
    own_process!();
    let rig = Rig::new();
    let roles = rig.state.join("roles");
    fs::create_dir_all(&roles).unwrap();
    fs::write(
        roles.join("broken.json"),
        r#"{"schema_version":"1.0","id":"broken","role_family":"bogus","description":"d","skills":[],"tool_palette":{"allow":[],"deny":[]},"escalation_contract":"bail-with-explanation"}"#,
    )
    .unwrap();
    let (res, _) = run(&rig, rig.opts());
    let err = format!("{:#}", res.expect_err("a broken role file"));
    assert!(err.starts_with("loading crew roles for internal dispatch: role `broken` has `role_family: \"bogus\"`"), "{err}");
    // The routing step loads the roles first, so this refuses before the
    // image check (and the role-load `?` inside the container path is never
    // the one that fires: ODDITY, the roles are loaded twice).
    assert!(rig.docker_calls().is_empty(), "{:?}", rig.docker_verbs());
    assert!(rig.docker_run().is_none());
    assert!(bookends(&rig).is_empty(), "records: {:?}", rig.actions());
}

#[test]
#[serial]
fn an_out_dir_that_cannot_be_made_fails_before_start_and_reclaims_the_auto_workspace() {
    own_process!();
    let rig = Rig::new();
    let blocker = rig.state.join("a-file");
    fs::write(&blocker, "x").unwrap();
    let mut o = rig.opts();
    o.host_out = Some(blocker.join("out"));
    let (res, _) = run(&rig, o);
    let err = format!("{:#}", res.expect_err("out-dir under a file"));
    assert!(err.contains("a-file/out: Not a directory"), "{err}");
    assert!(rig.docker_run().is_none());
    assert!(bookends(&rig).is_empty(), "records: {:?}", rig.actions());
    let left: Vec<_> = fs::read_dir(rig.state.join("tmp")).unwrap().filter_map(|e| e.ok()).collect();
    assert!(left.is_empty(), "the auto workspace is reclaimed: {:?}", left.iter().map(|e| e.file_name()).collect::<Vec<_>>());
}

#[test]
#[serial]
fn a_hosted_brain_whose_key_is_unset_fails_before_start() {
    own_process!();
    let rig = Rig::new();
    hosted_profiles(&rig, "http://127.0.0.1:9/v1");
    let (res, _) = run(&rig, rig.opts());
    let err = format!("{:#}", res.expect_err("no key"));
    assert!(err.contains("PIN_HOSTED_KEY"), "the error names the variable: {err}");
    assert!(rig.docker_run().is_none());
    assert!(bookends(&rig).is_empty(), "records: {:?}", rig.actions());
}

#[test]
#[serial]
fn an_unmanaged_compactor_whose_key_is_unset_fails_after_start_and_closes_it() {
    own_process!();
    let rig = Rig::new();
    fs::write(
        &rig.profiles,
        json!({
            "schema_version": "2.1",
            "profiles": {"fast": {"models": [{"id": "model-a", "n_ctx": 32000}]}},
            "default_profile": "fast",
            "endpoints": {"mlx-local": {"url": "http://127.0.0.1:8080/v1", "auth": {"type": "bearer", "key_env": "PIN_UTIL_KEY"}}},
            "internal": {"utility": {"id": "mlx-util", "n_ctx": 16384, "endpoint": "mlx-local"}}
        })
        .to_string(),
    )
    .unwrap();
    let mut o = rig.opts();
    o.record_context = Some(crawl_unit(Some("pin-crawl")));
    let (res, _) = run(&rig, o);
    let err = format!("{:#}", res.expect_err("no compactor key"));
    assert!(err.contains("PIN_UTIL_KEY"), "{err}");
    assert!(rig.docker_run().is_none());
    assert_eq!(bookends(&rig), vec!["dispatch.start", "dispatch.error"]);
    // The guard's terminal carries the caller's provenance too.
    assert_eq!(payload(&rig.record("dispatch.error"))["context"], json!({"unit": "unit-1", "workspace": "pin-crawl"}));
}

#[test]
#[serial]
fn a_panic_after_the_threads_start_still_closes_the_start_and_kills_by_name() {
    own_process!();
    let mut rig = Rig::new();
    rig.env().set("DARKMUX_TEST_PANIC_AFTER_GUARD_SPAWN", "1");
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let opts = rig.opts();
    let baseline = darkmux_crew::concurrent_dispatch::active_detached_thread_count();
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch(opts)));
    // The guards only SIGNAL the threads; wait for them to leave, so this
    // process never exits under a live thread (and every kill has landed).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while darkmux_crew::concurrent_dispatch::active_detached_thread_count() != baseline {
        assert!(std::time::Instant::now() < deadline, "a dispatch thread outlived the panic");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let msg = caught.err().and_then(|p| p.downcast_ref::<&str>().map(|s| s.to_string())).expect("dispatch panicked");
    assert!(msg.contains("#2642 test-injected panic"), "{msg}");
    assert_eq!(bookends(&rig), vec!["dispatch.start", "dispatch.error"]);
    assert_eq!(payload(&rig.record("dispatch.error"))["error"], "dispatch terminated before completion (early return or panic)");
    assert!(
        rig.docker_verbs().contains(&"kill darkmux-dispatch-boundary.solo.adhoc.coder.pin".to_string()),
        "{:?}",
        rig.docker_verbs()
    );
}

// ─── 9. more of the sampler ─────────────────────────────────────────────

fn crawl_unit(workspace: Option<&str>) -> darkmux_flow::payload::RecordContext {
    darkmux_flow::payload::RecordContext {
        unit: Some("unit-1".into()),
        workspace: workspace.map(str::to_string),
        ..Default::default()
    }
}

#[test]
#[serial]
fn a_breaker_on_a_crawl_unit_drops_the_crawls_stop_file_and_carries_its_context() {
    own_process!();
    let mut rig = Rig::new();
    scenario(&mut rig, r#"{"hold_ms": 600000, "thermal": {"state": "critical"}}"#);
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    rig.fake("run-script", "sleep 3\n");
    let mut o = rig.opts();
    o.record_context = Some(crawl_unit(Some("pin-crawl")));
    let (res, _) = run(&rig, o);
    res.unwrap();
    let stop = rig.state.join("crawl/pin-crawl/STOP");
    assert!(stop.is_file(), "the breaker stops the crawl");
    // An ad hoc session belongs to no mission, so the STOP names none.
    assert_eq!(fs::read_to_string(&stop).unwrap(), "thermal-critical\n");
    for action in ["dispatch.start", "dispatch.rest", "dispatch.complete"] {
        let p = payload(&rig.record(action)).clone();
        assert_eq!(p["context"], json!({"unit": "unit-1", "workspace": "pin-crawl"}), "{action}: {p}");
    }
    assert!(!rig.actions().iter().any(|a| a.starts_with("thermal.stop")), "{:?}", rig.actions());
}

/// A mission's crawl unit signs the STOP it drops, so the next mission on the
/// same workspace can tell it is not its own (#2454).
#[test]
#[serial]
fn a_breaker_in_a_mission_signs_the_stop_file_with_the_mission() {
    own_process!();
    let mut rig = Rig::new();
    scenario(&mut rig, r#"{"hold_ms": 600000, "thermal": {"state": "critical"}}"#);
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    rig.fake("run-script", "sleep 3\n");
    let mut o = rig.opts();
    o.session = SessionId::step(RunId::mission("m-pin").unwrap(), "s1");
    o.record_context = Some(crawl_unit(Some("pin-crawl")));
    let (res, _) = run(&rig, o);
    res.unwrap();
    assert_eq!(fs::read_to_string(rig.state.join("crawl/pin-crawl/STOP")).unwrap(), "thermal-critical mission=m-pin\n");
}

#[test]
#[serial]
fn a_breaker_on_a_crawl_unit_with_no_stop_path_says_so() {
    own_process!();
    let mut rig = Rig::new();
    scenario(&mut rig, r#"{"hold_ms": 600000, "thermal": {"state": "critical"}}"#);
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    rig.fake("run-script", "sleep 3\n");
    let mut o = rig.opts();
    o.record_context = Some(crawl_unit(None));
    let (res, _) = run(&rig, o);
    res.unwrap();
    let warn: Vec<Value> = rig.flows().into_iter().filter(|r| r["level"] == "warn" && r["source"] == "thermal").collect();
    assert_eq!(warn.len(), 1, "{:?}", rig.actions());
    assert_eq!(warn[0]["action"], "thermal.stop_unresolved");
    let p = payload(&warn[0]);
    assert_eq!(
        (p["stop_written"].as_bool(), p["cause"].as_str(), p["reason"].as_str()),
        (Some(false), Some("path_underivable"), Some("record_context.workspace is missing"))
    );
    assert_eq!(p["context"], json!({"unit": "unit-1"}));
}

#[test]
#[serial]
fn an_operator_hold_on_a_crawl_unit_with_no_stop_path_says_so() {
    own_process!();
    let mut rig = Rig::new();
    rig.env().set("DARKMUX_THERMAL_EPISODE_THRESHOLD", "1");
    scenario(&mut rig, r#"{"hold_ms": 600000, "thermal": {"state": "serious"}}"#);
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    rig.fake("run-script", "sleep 3\n");
    let mut o = rig.opts();
    o.record_context = Some(crawl_unit(None));
    let (res, _) = run(&rig, o);
    res.unwrap();
    let warn: Vec<Value> = rig.flows().into_iter().filter(|r| r["level"] == "warn" && r["source"] == "thermal").collect();
    assert_eq!(warn.len(), 1, "{:?}", rig.actions());
    assert_eq!(warn[0]["action"], "thermal.stop_unresolved");
    let p = payload(&warn[0]);
    assert_eq!(
        (p["stop_written"].as_bool(), p["cause"].as_str(), p["reason"].as_str()),
        (Some(false), Some("path_underivable"), Some("record_context.workspace is missing"))
    );
    assert_eq!(p["context"], json!({"unit": "unit-1"}));
}

#[test]
#[serial]
fn a_pause_held_past_its_limit_trips_the_breaker_without_an_eject() {
    own_process!();
    let mut rig = Rig::new();
    rig.env().set("DARKMUX_THERMAL_MAX_PAUSE_MS", "1");
    scenario(&mut rig, r#"{"hold_ms": 600000, "thermal": {"state": "serious"}}"#);
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    rig.fake("run-script", "sleep 5\n");
    let (res, _) = run(&rig, rig.opts());
    res.unwrap();
    let r: Vec<(String, String, bool)> = rests(&rig)
        .iter()
        .map(|r| {
            let p = payload(r);
            (p["reason"].as_str().unwrap().into(), p["state"].as_str().unwrap().into(), p["pause"].as_bool().unwrap())
        })
        .collect();
    assert_eq!(r, vec![("thermal".into(), "serious".into(), true), ("thermal-critical".into(), "serious".into(), true)]);
    // Tier 5 is for a literal `critical` reading only.
    assert!(rig.lms_log().iter().all(|l| !l.starts_with("unload")), "{:?}", rig.lms_log());
    assert!(!rig.actions().iter().any(|a| a.starts_with("thermal.tier5")), "{:?}", rig.actions());
}

#[test]
#[serial]
fn a_disarmed_thermal_tier_is_announced_at_start() {
    own_process!();
    let mut rig = Rig::new();
    rig.env().set("DARKMUX_THERMAL_RESUME_AT", "nominal");
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let (res, stderr) = run(&rig, rig.opts());
    res.unwrap();
    assert_eq!(stderr.matches("tier 2 DISARMED").count(), 1, "said once, at start");
    assert!(stderr.contains("darkmux: ⚠ thermal ladder — tier 2 DISARMED for this dispatch: resume_at=`nominal`"), "{stderr}");
}

#[test]
#[serial]
fn a_one_ms_machine_interval_stamps_the_previous_writes_cost_on_the_next_sample() {
    own_process!();
    let mut rig = Rig::new();
    rig.env().set("DARKMUX_HOST_SAMPLER_INTERVAL_MS", "1");
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    rig.fake("run-script", "sleep 3\n");
    let (res, _) = run(&rig, rig.opts());
    res.unwrap();
    let curve: Vec<Value> = rig.flows().into_iter().filter(|r| r["action"] == "machine.telemetry").map(|r| payload(&r).clone()).collect();
    assert!(curve.len() >= 2, "{curve:?}");
    assert!(curve[0].get("prev_record_write_ms").is_none(), "the first has no previous write: {}", curve[0]);
    assert!(curve[1]["prev_record_write_ms"].is_u64(), "{}", curve[1]);
    assert_eq!(curve[0]["interval_ms"], 1);
}

#[test]
#[serial]
fn a_zero_machine_sampler_interval_never_emits_the_machine_curve() {
    own_process!();
    let mut rig = Rig::new();
    rig.env().set("DARKMUX_HOST_SAMPLER_INTERVAL_MS", "0");
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let (res, _) = run(&rig, rig.opts());
    res.unwrap();
    assert!(!rig.actions().contains(&"machine.telemetry".to_string()), "{:?}", rig.actions());
    assert!(!rig.state.join("liveness/host-sampler.lock").exists());
}

#[test]
#[serial]
fn a_sampler_lock_released_mid_run_is_taken_over() {
    own_process!();
    let rig = Rig::new();
    let lock = rig.state.join("liveness/host-sampler.lock");
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    let parent = std::os::unix::process::parent_id();
    fs::write(
        &lock,
        json!({"pid": parent, "machine_uid": null, "started_ts_ms": now, "heartbeat_ts_ms": now, "interval_ms": 600000, "owner": "daemon"}).to_string(),
    )
    .unwrap();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    rig.fake("run-script", format!("sleep 1\nrm -f '{}'\nsleep 5\n", lock.display()));
    let (res, _) = run(&rig, rig.opts());
    res.unwrap();
    assert!(rig.actions().contains(&"machine.telemetry".to_string()), "{:?}", rig.actions());
}

/// A named out-dir must be fresh: an existing one is never reused.
#[test]
#[serial]
fn an_existing_out_dir_is_refused_before_start() {
    own_process!();
    let rig = Rig::new();
    let out = rig.state.join("out");
    fs::create_dir_all(&out).unwrap();
    let mut o = rig.opts();
    o.host_out = Some(out.clone());
    let (res, _) = run(&rig, o);
    let err = format!("{:#}", res.expect_err("an existing out-dir"));
    assert!(err.contains(&format!("refusing to create {} — something already exists at that path", out.display())), "{err}");
    assert!(rig.docker_run().is_none());
    assert!(bookends(&rig).is_empty(), "records: {:?}", rig.actions());
}

#[test]
#[serial]
fn a_session_id_with_characters_docker_refuses_reaches_the_container_name_escaped() {
    own_process!();
    let rig = Rig::new();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let mut o = rig.opts();
    o.session = SessionId::adhoc(RunId::standalone("boundary").unwrap(), "coder", "a+b/c d");
    let (res, _) = run(&rig, o);
    res.unwrap();
    // ODDITY: the wire form already escapes them, so the name's own
    // `-` substitution never fires for a session id.
    assert_eq!(arg_after(&rig.docker_run().unwrap(), "--name"), Some("darkmux-dispatch-boundary.solo.adhoc.coder.a_2Bb_2Fc_20d"));
}

#[test]
#[serial]
fn a_role_with_an_empty_feedback_template_map_forwards_none() {
    own_process!();
    let rig = Rig::new();
    let roles = rig.state.join("roles");
    fs::create_dir_all(&roles).unwrap();
    fs::write(
        roles.join("pin-fb.json"),
        r#"{"schema_version":"1.0","id":"pin-fb","description":"d","skills":[],"tool_palette":{"allow":["read"],"deny":[]},"escalation_contract":"bail-with-explanation","feedback_templates":{}}"#,
    )
    .unwrap();
    fs::write(roles.join("pin-fb.md"), "# FB\n").unwrap();
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let mut o = rig.opts();
    o.role_id = "pin-fb".into();
    let (res, _) = run(&rig, o);
    res.unwrap();
    assert!(!rig.docker_run().unwrap().contains(&"--feedback-templates-json".to_string()));
}

/// The compaction trigger drawn on each `telemetry.context`: the earlier of
/// the absolute threshold and ratio x window, else half the window.
#[test]
#[serial]
fn the_context_telemetry_carries_the_earliest_compaction_trigger() {
    own_process!();
    let traj = concat!(
        r#"{"type":"dispatch.context","seq":1,"ts":2,"used":1000,"max":32000}"#,
        "\n"
    );
    for (tokens, ratio, want) in [(None, None, 16000u64), (Some(10000), Some(0.25f32), 8000), (Some(5000), Some(0.25), 5000), (Some(9000), None, 9000)] {
        let rig = Rig::new();
        rig.container(traj, ENVELOPE, "", 0);
        let mut o = rig.opts();
        o.compaction.threshold_tokens = tokens;
        o.compaction.threshold_ratio = ratio;
        let (res, _) = run(&rig, o);
        res.unwrap();
        let ctx = payload(&rig.record("telemetry.context")).clone();
        assert_eq!(ctx, json!({"used": 1000, "max": 32000, "threshold": want}), "tokens={tokens:?} ratio={ratio:?}");
    }
}

// ─── 10. endpoint window budgets (the start gate and the in-run pacer) ──

fn windowed_profiles(rig: &Rig, policy: &str) {
    fs::write(
        &rig.profiles,
        json!({
            "profiles": {"cloud": {"models": [{"id": "gpt-remote", "n_ctx": 100000, "endpoint": "mock"}]}},
            "endpoints": {"mock": {
                "url": "http://127.0.0.1:9/v1",
                "auth": {"type": "bearer", "key_env": "PIN_HOSTED_KEY"},
                "limits": {"window": {"period": "1h", "calls": 1}, "policy": policy}
            }},
            "default_profile": "cloud"
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
#[serial]
fn a_full_window_is_surfaced_before_the_next_dispatch_starts() {
    own_process!();
    let mut rig = Rig::new();
    rig.env().set("PIN_HOSTED_KEY", "sk-pin");
    // The first run spends under `off`, so nothing in this process has
    // surfaced the window yet; then the budget is switched on.
    windowed_profiles(&rig, "off");
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    let (first, _) = run(&rig, rig.opts());
    first.unwrap();
    assert!(!rig.actions().contains(&"budget.warn".to_string()), "{:?}", rig.actions());
    windowed_profiles(&rig, "warn");
    let before = rig.flows().len();
    let (second, _) = run(&rig, rig.opts());
    second.unwrap();
    let after: Vec<Value> = rig.flows().into_iter().skip(before).collect();
    let actions: Vec<&str> = after.iter().map(|r| r["action"].as_str().unwrap_or("?")).collect();
    assert_eq!(&actions[..2], ["budget.warn", "dispatch.start"], "the gate speaks before the start: {actions:?}");
    let p = payload(&after[0]);
    assert_eq!(
        (p["scope"].as_str(), p["metric"].as_str(), p["spent"].as_u64(), p["limit"].as_u64()),
        (Some("endpoint"), Some("calls"), Some(2), Some(1))
    );
}

#[test]
#[serial]
fn a_window_filled_mid_run_pauses_the_container_through_the_pace_file() {
    own_process!();
    let mut rig = Rig::new();
    rig.env().set("PIN_HOSTED_KEY", "sk-pin");
    windowed_profiles(&rig, "wait");
    rig.container(ONE_TURN, ENVELOPE, "", 0);
    // The pacer checks on its first tick, then every 5s.
    rig.fake("run-script", "sleep 8\n");
    let (res, _) = run(&rig, rig.opts());
    let out = res.unwrap().out_dir.unwrap();
    assert!(rig.actions().contains(&"budget.wait".to_string()), "{:?}", rig.actions());
    let r = rests(&rig);
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(payload(&r[0]), &json!({"reason": "budget", "state": "mock", "pause": true}));
    let pace: Value = serde_json::from_str(&fs::read_to_string(out.join("pace.json")).unwrap()).unwrap();
    assert_eq!((pace["pause"].as_bool(), pace["reason"].as_str()), (Some(true), Some("budget")));
}
