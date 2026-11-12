//! (#3112) Client previews in a sandbox on the host (Seatbelt): placement,
//! the loopback host, the listener watch, the post-stop sweep, the backend a
//! preview is stopped in, and a backend switch's `node_modules`. The
//! toolchain grant's own tests are `darkmux_sandbox::toolchain`'s.

use super::*;
use darkmux_platform::PortListener;

fn a_recipe(v: serde_json::Value) -> Recipe {
    parse_recipe(&v.to_string()).unwrap()
}

/// A run in a sandbox on the host, as a start builds it.
fn a_run_in(dir: &Path) -> RunIn {
    let ws = dir.join("ws/site");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(dir.join("p")).unwrap();
    RunIn {
        name: "site".into(),
        run: "r1".into(),
        workdir: ws,
        checkout: None,
        log: dir.join("p/site.log"),
        port: 4100,
        hmr_port: 4200,
        ready_within: READY_WITHIN,
        exposure_every: EXPOSURE_CHECK_EVERY,
    }
}

fn a_toolchain(dir: &Path) -> Toolchain {
    Toolchain { path: Some(format!("{}:/usr/bin:/bin", dir.join("node/bin").display())), read_only: vec![dir.join("node")], identity: "node=x".into() }
}

/// (#3112) A Seatbelt preview's policy: every path where the sandbox sees
/// it (the workspace and the heartbeat at their host paths, the watchdog
/// reading the host path), a host shell as its program, its toolchain and
/// its own log read only, PATH to the toolchain, no container user and no
/// `HOME=/tmp` (the sandbox gives it a private home).
#[test]
fn a_seatbelt_preview_sees_its_paths_where_they_are_and_runs_the_host_shell() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let at = a_run_in(&root);
    let recipe = a_recipe(serde_json::json!({"schema_version": "1.0", "command": "npm run dev"}));
    let spec = spec_in(BackendKind::Seatbelt, &recipe, &at, false, ("seatbelt".into(), Some("501:20".into())), vec!["darkmux.dispatch.host-pid=7".into()], &a_toolchain(&root));
    let policy = preview_policy(&spec);
    assert_eq!(policy.workdir.as_deref(), Some(root.join("ws/site").to_str().unwrap()));
    assert_eq!(policy.entrypoint.as_deref(), Some("/bin/sh"));
    assert_eq!(policy.user, None, "Seatbelt runs as the caller");
    let ro: Vec<&Path> = policy.mounts.iter().filter(|m| m.access == Access::ReadOnly).map(|m| m.host.as_path()).collect();
    assert_eq!(ro, [spec.heartbeat.as_path(), root.join("node").as_path(), at.log.as_path()]);
    assert_eq!(policy.mounts[0].host, at.workdir);
    assert_eq!(policy.mounts[0].access, Access::ReadWrite);
    let beat = format!("{}/{HEARTBEAT_FILE}", root.join("p/heartbeats").join(&spec.name).display());
    assert!(spec.script.contains(&beat), "the watchdog reads the heartbeat where the sandbox sees it: {}", spec.script);
    assert!(!spec.script.contains(HEARTBEAT_AT), "{}", spec.script);
    assert!(!policy.env.iter().any(|(k, v)| k == "HOME" && v == "/tmp"), "{:?}", policy.env);
    assert_eq!(policy.env[0], ("PATH".to_string(), a_toolchain(&root).path.unwrap()));
    assert_eq!(policy.ports, vec![4100, 4200], "the same two ports, chosen the same way");
}

/// (#3112) darkmux gives the dev server a loopback host on the host: a
/// recipe's every-address HOST (`0.0.0.0`, `::`) and `{host}` become
/// `127.0.0.1`, so a recipe written for Docker runs unchanged. Under Docker
/// `{host}` is `0.0.0.0` (the container's port is published on loopback)
/// and every other value is as written.
#[test]
fn the_dev_server_host_is_loopback_on_the_host_and_every_address_in_a_container() {
    let recipe = a_recipe(serde_json::json!({
        "schema_version": "1.1", "command": "npm run dev",
        "env": {"HOST": "0.0.0.0", "BIND": "::", "H2": "{host}", "URL": "http://{host}:{port}/", "KEEP": "10.0.0.0"}
    }));
    let value = |env: &[(String, String)], k: &str| env.iter().rev().find(|(key, _)| key == k).map(|(_, v)| v.clone()).unwrap();
    let host = container_env_for(&recipe, 4100, 4200, BackendKind::Seatbelt, None);
    assert_eq!(
        ["HOST", "BIND", "H2", "URL", "KEEP"].map(|k| value(&host, k)),
        ["127.0.0.1", "127.0.0.1", "127.0.0.1", "http://127.0.0.1:4100/", "10.0.0.0"]
    );
    let docker = container_env_for(&recipe, 4100, 4200, BackendKind::Docker, None);
    assert_eq!(["HOST", "BIND", "H2", "URL"].map(|k| value(&docker, k)), ["0.0.0.0", "::", "0.0.0.0", "http://0.0.0.0:4100/"]);
    assert_eq!(container_env(&recipe, 4100, 4200), docker, "Docker's environment is the one it always was");
}

/// (#3112) Only a listener beyond the loopback counts as exposed: every
/// address (`0.0.0.0`, `::`) and a LAN address do; `127.x`, `::1` and
/// `::ffff:127.x` do not. The reason names each one, the ports darkmux gave
/// the preview, and the fix.
#[test]
fn a_listener_beyond_the_loopback_is_exposed_and_named() {
    let at = |pid, addr: &str, scope: Option<&str>| PortListener { pid: Some(pid), addr: addr.parse().unwrap(), scope: scope.map(str::to_string) };
    let all = vec![at(1, "127.0.0.1", None), at(1, "::1", None), at(1, "::ffff:127.0.0.1", None), at(1, "fe80::1", Some("lo0")), at(2, "::", None), at(3, "0.0.0.0", None), at(3, "192.168.1.5", None), at(4, "fe80::1", Some("en0"))];
    let exposed = on_host::beyond_loopback(&all);
    assert_eq!(exposed, all[4..].to_vec(), "every address, a LAN address and a link-local one off the loopback interface");
    let found = vec![(4200, all[4].clone()), (4100, all[5].clone()), (4100, PortListener { pid: None, ..all[6].clone() })];
    let why = on_host::exposure_reason(BackendKind::Seatbelt, &found, 4100, 4200);
    for part in ["[::]:4200 (the HMR port, pid 2)", "0.0.0.0:4100 (the app port, pid 3)", "192.168.1.5:4100 (the app port)", "seatbelt", "127.0.0.1", "#3559", "server.hmr.host"] {
        assert!(why.contains(part), "missing `{part}` in: {why}");
    }
}

/// (#3112) Why a dev server on the host never answered, from what its
/// group listens on: nothing, IPv6 loopback only, or loopback but no answer.
#[test]
fn an_on_host_dev_server_that_never_answered_says_why_from_its_listeners() {
    let at = |addr: &str| PortListener { pid: Some(1), addr: addr.parse().unwrap(), scope: None };
    let none = on_host::not_ready_reason(BackendKind::Seatbelt, 4100, Some(&[]));
    assert!(none.contains("nothing listens on port 4100") && none.contains("127.0.0.1"), "{none}");
    let v6 = on_host::not_ready_reason(BackendKind::Seatbelt, 4100, Some(&[at("::1")]));
    assert!(v6.contains("[::1]") && v6.contains("127.0.0.1"), "{v6}");
    let quiet = on_host::not_ready_reason(BackendKind::Seatbelt, 4100, Some(&[at("127.0.0.1")]));
    assert!(quiet.contains("listens on 127.0.0.1:4100") && quiet.contains("2xx or 3xx"), "{quiet}");
    let unknown = on_host::not_ready_reason(BackendKind::Seatbelt, 4100, None);
    assert!(unknown.contains("didn't answer on port 4100"), "{unknown}");
}

/// A workspace whose own recipe is `recipe`.
fn a_workspace(dir: &Path, recipe: serde_json::Value) -> PathBuf {
    let wt = dir.join("base/w");
    std::fs::create_dir_all(wt.join(".darkmux")).unwrap();
    std::fs::write(wt.join(".darkmux/preview.json"), recipe.to_string()).unwrap();
    wt
}

fn until_status(previews: &Previews, name: &str, want: &[&str], within: Duration) -> serde_json::Value {
    let deadline = Instant::now() + within;
    loop {
        let v = serde_json::to_value(previews.status(name).unwrap()).unwrap();
        if want.contains(&v["status"].as_str().unwrap_or("")) || Instant::now() > deadline {
            return v;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Previews whose Seatbelt backend is an on-host emulator and whose Docker
/// backend is another; returns both emulators.
fn two_backends(dir: &Path) -> (Previews, Arc<HostEmulator>, Arc<HostEmulator>) {
    let docker = Arc::new(HostEmulator::default());
    let seatbelt = Arc::new(HostEmulator::default());
    *seatbelt.on_host.lock().unwrap() = true;
    let previews = Previews::new(dir.join("p"), docker.clone()).with_backend(BackendKind::Seatbelt, seatbelt.clone());
    (previews, docker, seatbelt)
}

/// Whether this host can list a port's sockets (the watch's source); the
/// tests that need it return early where it cannot.
fn host_lists_sockets() -> bool {
    darkmux_platform::current().tcp_port_listeners(1).is_some()
}

/// A dev server that binds `host` (a Python expression: a literal address
/// written into its command, or `os.environ['HOST']`) on `$PORT` and
/// answers every request 200.
fn serve_on(host: &str) -> String {
    format!(
        "exec /usr/bin/python3 -c \"import http.server as h,os\nclass Q(h.BaseHTTPRequestHandler):\n  def do_GET(s): s.send_response(200); s.end_headers(); s.wfile.write(b'ok')\n  def log_message(s,*a): pass\nh.HTTPServer(({host}, int(os.environ['PORT'])), Q).serve_forever()\""
    )
}

/// The host from the recipe's `HOST`.
const HOST_ENV: &str = "os.environ['HOST']";

/// (#3112) On the host a dev server that binds every address is stopped
/// before its preview is handed over as ready, saying what listened where;
/// one on the loopback runs and stays ready across several watches. Runs
/// the real listener watch against the emulated sandbox's process group.
#[test]
fn a_preview_on_the_host_that_listens_beyond_the_loopback_is_stopped() {
    if !host_lists_sockets() {
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    let (previews, _docker, seatbelt) = two_backends(dir.path());
    // Only the read as it first answers can catch it: the periodic one is
    // a minute away.
    let previews = previews.with_exposure_every(Duration::from_secs(60));
    let wt = a_workspace(dir.path(), serde_json::json!({"schema_version": "1.0", "command": serve_on("'0.0.0.0'"), "env": {"PORT": "{port}"}}));
    previews.start_in(BackendKind::Seatbelt, "w", &wt, &RecipeSource::Checkout(wt.clone())).unwrap();
    let v = until_status(&previews, "w", &["failed", "ready"], Duration::from_secs(20));
    assert_eq!(v["status"], "failed", "{v}");
    let why = v["error"].as_str().unwrap();
    assert!(why.contains(&format!("0.0.0.0:{}", v["port"])) && why.contains("beyond this Mac's loopback"), "{why}");
    assert!(!seatbelt.removed.lock().unwrap().is_empty(), "its sandbox is removed");
    let wt = a_workspace(dir.path(), serde_json::json!({"schema_version": "1.1", "command": serve_on(HOST_ENV), "env": {"PORT": "{port}", "HOST": "{host}"}}));
    previews.start_in(BackendKind::Seatbelt, "w", &wt, &RecipeSource::Checkout(wt.clone())).unwrap();
    let v = until_status(&previews, "w", &["failed", "ready"], Duration::from_secs(20));
    assert_eq!(v["status"], "ready", "{v}");
    previews.stop("w");
    // Watched every 0.2 s, a loopback server stays ready.
    let previews = previews.with_exposure_every(Duration::from_millis(200));
    previews.start_in(BackendKind::Seatbelt, "w", &wt, &RecipeSource::Checkout(wt.clone())).unwrap();
    assert_eq!(until_status(&previews, "w", &["failed", "ready"], Duration::from_secs(20))["status"], "ready");
    std::thread::sleep(Duration::from_millis(1000));
    assert_eq!(until_status(&previews, "w", &[], Duration::ZERO)["status"], "ready", "still ready after several watches");
    previews.stop("w");
}

/// (#3112) Once ready, a listener that appears beyond the loopback later
/// (a second server the code starts) stops the preview at the next watch.
#[test]
fn a_listener_that_appears_after_ready_stops_the_preview() {
    if !host_lists_sockets() {
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    let (previews, _docker, _seatbelt) = two_backends(dir.path());
    let previews = previews.with_exposure_every(Duration::from_millis(200));
    // The second server takes the HMR port, which the kernel lets it bind.
    let late = "(sleep 2; exec /usr/bin/python3 -c \"import os,socket,time;s=socket.socket();s.bind(('0.0.0.0',int(os.environ['DARKMUX_PREVIEW_HMR_PORT'])));s.listen();time.sleep(60)\") & ".to_string() + &serve_on(HOST_ENV);
    let wt = a_workspace(dir.path(), serde_json::json!({"schema_version": "1.0", "command": late, "env": {"PORT": "{port}", "HOST": "0.0.0.0"}}));
    previews.start_in(BackendKind::Seatbelt, "w", &wt, &RecipeSource::Checkout(wt.clone())).unwrap();
    let ready = until_status(&previews, "w", &["failed", "ready"], Duration::from_secs(20));
    assert_eq!(ready["status"], "ready", "{ready}");
    let v = until_status(&previews, "w", &["failed"], Duration::from_secs(20));
    assert_eq!(v["status"], "failed", "{v}");
    assert!(v["error"].as_str().unwrap().contains("beyond this Mac's loopback"), "{v}");
}

/// (#3112) A preview is stopped in the backend it was started in: one
/// started under Seatbelt is created and removed by Seatbelt's runner, never
/// Docker's, and one started under Docker by Docker's, whatever is selected
/// when it stops.
#[test]
fn a_preview_is_stopped_in_the_backend_it_started_in() {
    let dir = tempfile::TempDir::new().unwrap();
    let (previews, docker, seatbelt) = two_backends(dir.path());
    let wt = a_workspace(dir.path(), serde_json::json!({"schema_version": "1.0", "command": "sleep 30"}));
    let source = RecipeSource::Checkout(wt.clone());
    previews.start_in(BackendKind::Seatbelt, "w", &wt, &source).unwrap();
    for _ in 0..200 {
        if previews.entries.lock().unwrap()["w"].lock().unwrap().container.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(seatbelt.specs.lock().unwrap()[0].backend, BackendKind::Seatbelt);
    assert!(previews.stop("w"));
    assert_eq!(seatbelt.removed.lock().unwrap().len(), 1, "removed where it was made");
    assert!(docker.removed.lock().unwrap().is_empty() && docker.specs.lock().unwrap().is_empty(), "Docker never saw it");
    previews.start_in(BackendKind::Docker, "w", &wt, &source).unwrap();
    for _ in 0..200 {
        if !docker.specs.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(100));
    assert!(previews.stop("w"));
    assert_eq!(docker.specs.lock().unwrap()[0].backend, BackendKind::Docker);
    assert_eq!(docker.removed.lock().unwrap().len(), 1);
    assert_eq!(seatbelt.removed.lock().unwrap().len(), 1);
}

/// (#3112) A workspace installed under one backend and previewed under
/// another reinstalls: its `unless_exists` path (Linux-built `node_modules`
/// from Docker, which this Mac's node cannot load) is deleted in darkmux's
/// workspace copy and the install runs, and the preview log and the serve
/// log say so. The same backend again skips the install and deletes
/// nothing; the record names the backend that made it.
#[test]
fn a_backend_switch_reinstalls_saying_what_it_deleted() {
    let dir = tempfile::TempDir::new().unwrap();
    let (previews, _docker, seatbelt) = two_backends(dir.path());
    let recipe = serde_json::json!({
        "schema_version": "1.0", "command": "sleep 30",
        "install": {"command": "mkdir -p node_modules && echo mac > node_modules/built && echo installed >> installs.txt", "unless_exists": "node_modules"}
    });
    let wt = a_workspace(dir.path(), recipe);
    std::fs::create_dir_all(wt.join("node_modules")).unwrap();
    std::fs::write(wt.join("node_modules/built"), "linux").unwrap();
    let docker_made = install_fingerprint(&wt, "mkdir -p node_modules && echo mac > node_modules/built && echo installed >> installs.txt", "sha256:runtime");
    crate::client_workspace::record_install(&dir.path().join("base"), "w", &docker_made).unwrap();
    // The recipe comes from the user's own checkout, never the workspace.
    let mine = dir.path().join("mine");
    std::fs::create_dir_all(mine.join(".darkmux")).unwrap();
    std::fs::copy(wt.join(".darkmux/preview.json"), mine.join(".darkmux/preview.json")).unwrap();
    let source = RecipeSource::Checkout(mine);
    let log = dir.path().join("p/w.log");
    let run_once = |want_spec: usize| {
        previews.start_in(BackendKind::Seatbelt, "w", &wt, &source).unwrap();
        for _ in 0..400 {
            if seatbelt.specs.lock().unwrap().len() >= want_spec && log_has(&log, SERVING_LINE) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        // The start notices the dev server line (and records the install)
        // on its next look, 300 ms apart.
        std::thread::sleep(Duration::from_millis(800));
        let said = std::fs::read_to_string(&log).unwrap_or_default();
        previews.stop("w");
        said
    };
    let said = run_once(1);
    assert_eq!(std::fs::read_to_string(wt.join("node_modules/built")).unwrap().trim(), "mac", "reinstalled for this Mac");
    assert!(said.contains("node_modules") && said.contains("deleted") && said.contains("Docker"), "{said}");
    let record = std::fs::read_to_string(dir.path().join("base/.w.install")).unwrap();
    assert!(record.trim().starts_with("seatbelt:"), "{record}");
    let said = run_once(2);
    assert!(!said.contains("deleted"), "the same backend deletes nothing: {said}");
    assert_eq!(std::fs::read_to_string(wt.join("installs.txt")).unwrap().lines().count(), 1, "and skips the install");
}

/// (#3112) A real Seatbelt preview: it serves the workspace, its
/// environment holds nothing `client serve` was started with, it cannot
/// read a host path beside the workspace, it binds the loopback though its
/// recipe asks for every address, and a stop ends its whole process group.
/// macOS only (it needs `sandbox-exec`); the Docker twin is the opt-in
/// `a_preview_runs_in_the_runtime_image_with_no_host_files_or_host_env`.
#[test]
#[serial_test::serial]
fn a_seatbelt_preview_runs_with_no_host_files_or_host_env() {
    if !darkmux_sandbox::SeatbeltBackend::available() || !host_lists_sockets() {
        return;
    }
    let state = tempfile::TempDir::new().unwrap();
    let prev_state = std::env::var_os(darkmux_sandbox::seatbelt::STATE_DIR_ENV);
    unsafe { std::env::set_var(darkmux_sandbox::seatbelt::STATE_DIR_ENV, state.path()) };
    let dir = tempfile::TempDir::new().unwrap();
    let group = |name: &str| -> u32 { std::fs::read_to_string(state.path().join(name).join("pid")).unwrap().trim().parse().unwrap() };
    let ((group, listened), _) = super::tests_shared::preview_with_no_host_files_or_host_env(
        dir.path(),
        Previews::new(dir.path().join("p"), Arc::new(OnHost::new(BackendKind::Seatbelt))),
        BackendKind::Seatbelt,
        |name, port| (group(name), darkmux_platform::current().tcp_port_listeners(port)),
    );
    let gone = (0..50).any(|_| {
        std::thread::sleep(Duration::from_millis(100));
        unsafe { libc::kill(-(group as i32), 0) != 0 }
    });
    unsafe {
        match prev_state {
            Some(v) => std::env::set_var(darkmux_sandbox::seatbelt::STATE_DIR_ENV, v),
            None => std::env::remove_var(darkmux_sandbox::seatbelt::STATE_DIR_ENV),
        }
    }
    let listened = listened.expect("the dev server's listeners were read");
    assert!(!listened.is_empty() && listened.iter().all(|l| l.addr.is_loopback()), "{listened:?}");
    assert!(gone, "a stop ends the whole process group");
}

/// (#3112) A Seatbelt preview's watchdog ends it once the heartbeat goes
/// stale, and with it everything its script started (a dev server still
/// running), counting looks not the clock. Driven with a 0.1 s watch.
#[test]
#[serial_test::serial]
fn a_stale_seatbelt_preview_ends_with_its_dev_server() {
    if !darkmux_sandbox::SeatbeltBackend::available() {
        return;
    }
    let state = tempfile::TempDir::new().unwrap();
    let prev_state = std::env::var_os(darkmux_sandbox::seatbelt::STATE_DIR_ENV);
    unsafe { std::env::set_var(darkmux_sandbox::seatbelt::STATE_DIR_ENV, state.path()) };
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let at = a_run_in(&root);
    let recipe = a_recipe(serde_json::json!({"schema_version": "1.0", "command": "(while :; do echo x >> beat.txt; sleep 0.2; done) & sleep 300"}));
    let mut spec = spec_in(BackendKind::Seatbelt, &recipe, &at, false, ("seatbelt".into(), None), Vec::new(), &Toolchain::default());
    std::fs::create_dir_all(&spec.heartbeat).unwrap();
    let beat = spec.heartbeat.join(HEARTBEAT_FILE);
    std::fs::write(&beat, "").unwrap();
    let watch = Watch { file: beat.display().to_string(), missed: 3, every: "0.1".into(), grace: "0.5".into() };
    spec.script = container_script(&recipe, false, &watch);
    let host = OnHost::new(BackendKind::Seatbelt);
    host.create(&spec).unwrap();
    let mut child = host.start(&spec.name, &at.log).unwrap();
    let ended = (0..100).find_map(|_| {
        std::thread::sleep(Duration::from_millis(100));
        child.try_wait().unwrap()
    });
    let grown = |f: &Path| std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
    let after = grown(&at.workdir.join("beat.txt"));
    std::thread::sleep(Duration::from_millis(800));
    let later = grown(&at.workdir.join("beat.txt"));
    host.remove(&spec.name);
    unsafe {
        match prev_state {
            Some(v) => std::env::set_var(darkmux_sandbox::seatbelt::STATE_DIR_ENV, v),
            None => std::env::remove_var(darkmux_sandbox::seatbelt::STATE_DIR_ENV),
        }
    }
    let said = std::fs::read_to_string(&at.log).unwrap_or_default();
    assert!(ended.is_some(), "the stale preview never ended: {said}");
    assert!(said.contains(STALE_LINE), "{said}");
    assert!(after > 0, "the background server never ran: {said}");
    assert_eq!(later, after, "the dev server outlived its stale preview");
}

/// (#3112) A backend previews have no runner for is refused, naming it and
/// the way to a container; it never runs in another backend.
#[test]
fn a_backend_with_no_preview_runner_is_refused() {
    let dir = tempfile::TempDir::new().unwrap();
    let docker = Arc::new(HostEmulator::default());
    let previews = Previews::new(dir.path().join("p"), docker.clone());
    let wt = a_workspace(dir.path(), serde_json::json!({"schema_version": "1.0", "command": "sleep 30"}));
    let why = previews.start_in(BackendKind::Seatbelt, "w", &wt, &RecipeSource::Checkout(wt.clone())).unwrap_err();
    assert!(why.message().contains("previews cannot run in the `seatbelt` sandbox") && why.message().contains("darkmux config set runtime.sandbox docker"), "{why:?}");
    std::thread::sleep(Duration::from_millis(100));
    assert!(docker.specs.lock().unwrap().is_empty(), "nothing ran in Docker instead");
}

/// (#3112) The watch reads a group's listeners through the platform: an
/// unreadable read is asked again, and only two in a row stop the preview,
/// saying darkmux cannot vouch for it and the way to a container; a
/// listener beyond the loopback is reported, one on it is not.
#[test]
fn the_watch_asks_once_more_then_stops_what_it_cannot_read() {
    use std::sync::atomic::AtomicU32;
    let wide = PortListener { pid: Some(7), addr: "0.0.0.0".parse().unwrap(), scope: None };
    let near = PortListener { pid: Some(7), addr: "127.0.0.1".parse().unwrap(), scope: None };
    let once = darkmux_platform::FakePlatform { port_listeners: vec![(5, vec![wide.clone(), near.clone()]), (6, vec![near])], ..Default::default() };
    assert_eq!(on_host::exposed_in(&once, BackendKind::Seatbelt, [5, 6]), Ok(vec![(5, wide)]));
    assert_eq!(on_host::exposed_in(&once, BackendKind::Seatbelt, [6, 7]), Ok(vec![]));
    let blind = darkmux_platform::FakePlatform { port_listeners_unreadable_reads: AtomicU32::new(1), ..Default::default() };
    assert_eq!(on_host::exposed_in(&blind, BackendKind::Seatbelt, [5, 6]), Ok(vec![]), "one unreadable read is asked again");
    let blind = darkmux_platform::FakePlatform { port_listeners_unreadable_reads: AtomicU32::new(2), ..Default::default() };
    let why = on_host::exposed_in(&blind, BackendKind::Seatbelt, [5, 6]).unwrap_err();
    assert!(why.contains("cannot read what listens on its port 5") && why.contains("darkmux config set runtime.sandbox docker"), "{why}");
}

/// (#3112 review) The post-stop sweep kills what still holds the preview's
/// ports, every owner the platform names (the socket's last user and every
/// holder found), but only one that started no earlier than the sandbox:
/// an older process on the port is never touched. It stops looking once
/// the ports are free.
#[test]
fn the_sweep_kills_only_port_holders_started_since_the_sandbox() {
    let holder = |pid| PortListener { pid: Some(pid), addr: "0.0.0.0".parse().unwrap(), scope: None };
    let fake = darkmux_platform::FakePlatform {
        port_listeners: vec![(4100, vec![holder(7), holder(8)])],
        tcp_listeners: vec![(4100, vec![9])],
        process_start_stamps: vec![(7, 200), (8, 50), (9, 100)],
        ..Default::default()
    };
    let killed = Mutex::new(Vec::new());
    let left = on_host::sweep_ports(&fake, [4100, 4200], 100, |pid| {
        killed.lock().unwrap().push(pid);
        true
    });
    let mut killed = killed.into_inner().unwrap();
    killed.sort_unstable();
    killed.dedup();
    assert_eq!(killed, [7, 9], "the holders started since the sandbox (100), never the older 8");
    assert_eq!(left.len(), 2, "this fake keeps listening, so it is reported");
    let free = darkmux_platform::FakePlatform::default();
    assert!(on_host::sweep_ports(&free, [4100, 4200], 100, |_| panic!("nothing to kill")).is_empty());
}

/// A real Seatbelt preview's sandbox root for the duration of a test.
struct StateDir {
    dir: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
}

impl StateDir {
    fn set() -> StateDir {
        let dir = tempfile::TempDir::new().unwrap();
        let prev = std::env::var_os(darkmux_sandbox::seatbelt::STATE_DIR_ENV);
        unsafe { std::env::set_var(darkmux_sandbox::seatbelt::STATE_DIR_ENV, dir.path()) };
        StateDir { dir, prev }
    }
}

impl Drop for StateDir {
    fn drop(&mut self) {
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var(darkmux_sandbox::seatbelt::STATE_DIR_ENV, v),
                None => std::env::remove_var(darkmux_sandbox::seatbelt::STATE_DIR_ENV),
            }
        }
        let _ = &self.dir;
    }
}

/// Previews in real Seatbelt sandboxes (the production runner, its
/// toolchain found on this PATH), watched every 0.2 s.
fn real_seatbelt(dir: &Path) -> Previews {
    Previews::new(dir.join("p"), Arc::new(OnHost::new(BackendKind::Seatbelt)))
        .with_exposure_every(Duration::from_millis(200))
}

/// Whether anything listens on `port`.
fn listened(port: u16) -> bool {
    darkmux_platform::current().tcp_port_listeners(port).is_some_and(|l| !l.is_empty())
}

/// (#3112 review, BLOCKER 1) A dev server that binds every address and then
/// keeps forking a new pid (each child inheriting the socket, the parent
/// exiting) is still stopped: the watch reads the port from the kernel's
/// socket table, not by walking the sandbox's processes, which the hopper
/// races. Through the real runner: its `python3 -c "..."` (quotes holding
/// `;` and `(`) is resolved on this PATH. Afterwards the port is free.
#[test]
#[serial_test::serial]
fn a_fork_hopping_listener_on_every_address_is_stopped() {
    if !darkmux_sandbox::SeatbeltBackend::available() || !host_lists_sockets() {
        return;
    }
    let _state = StateDir::set();
    let dir = tempfile::TempDir::new().unwrap();
    let previews = real_seatbelt(dir.path());
    // The first process stays (so the dev server has not exited) and hands
    // the socket to a child that hops: forks, and the parent exits, over and
    // over, so no one pid holds it for more than a few milliseconds.
    let hopper = r#"exec python3 -c "import os, socket, time; s = socket.socket(); s.bind(('0.0.0.0', int(os.environ['PORT']))); s.listen()
if os.fork():
    s.close(); time.sleep(600)
while True:
    if os.fork(): os._exit(0)
    time.sleep(0.005)""#;
    let wt = a_workspace(dir.path(), serde_json::json!({"schema_version": "1.1", "command": hopper, "env": {"PORT": "{port}"}}));
    previews.start_in(BackendKind::Seatbelt, "w", &wt, &RecipeSource::Checkout(wt.clone())).unwrap();
    let v = until_status(&previews, "w", &["failed", "ready"], Duration::from_secs(30));
    assert_eq!(v["status"], "failed", "{v}");
    let why = v["error"].as_str().unwrap();
    assert!(why.contains(&format!("0.0.0.0:{} (the app port", v["port"])), "{why}");
    let port = v["port"].as_u64().unwrap() as u16;
    let free = (0..40).any(|_| {
        std::thread::sleep(Duration::from_millis(100));
        !listened(port)
    });
    assert!(free, "the hopper still holds port {port}");
}

/// (#3112 review, BLOCKER 1) A listener on a port darkmux did not assign is
/// refused by the kernel (the sandbox may bind only its app and HMR ports),
/// whatever address it asks for; the assigned port still serves.
#[test]
#[serial_test::serial]
fn a_listener_on_an_unassigned_port_is_refused_by_the_kernel() {
    if !darkmux_sandbox::SeatbeltBackend::available() || !host_lists_sockets() {
        return;
    }
    let _state = StateDir::set();
    let dir = tempfile::TempDir::new().unwrap();
    let previews = real_seatbelt(dir.path());
    let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let probe = format!(
        r#"python3 -c "import socket
for host in ('127.0.0.1', '0.0.0.0'):
    s = socket.socket()
    try:
        s.bind((host, {other})); s.listen(); print('bound', host)
    except OSError as e:
        print('refused', host, e.errno)" > probe.txt; {}"#,
        serve_on(HOST_ENV)
    );
    let wt = a_workspace(dir.path(), serde_json::json!({"schema_version": "1.1", "command": probe, "env": {"PORT": "{port}", "HOST": "{host}"}}));
    previews.start_in(BackendKind::Seatbelt, "w", &wt, &RecipeSource::Checkout(wt.clone())).unwrap();
    let v = until_status(&previews, "w", &["failed", "ready"], Duration::from_secs(30));
    previews.stop("w");
    assert_eq!(v["status"], "ready", "the assigned port serves: {v}");
    let said = std::fs::read_to_string(wt.join("probe.txt")).unwrap_or_default();
    assert_eq!(said, "refused 127.0.0.1 1
refused 0.0.0.0 1
", "binds on port {other}");
}

/// (#3112 review, MAJOR 2) A dev server that moves to a process group of
/// its own (`setpgid`) escapes the sandbox's group kill; after a stop, the
/// sweep finds it on the port it holds and kills it, so the port is free
/// and nothing answers. One that left the group and holds no port is not
/// found (documented).
#[test]
#[serial_test::serial]
fn a_dev_server_that_leaves_the_group_is_swept_off_its_port() {
    if !darkmux_sandbox::SeatbeltBackend::available() || !host_lists_sockets() {
        return;
    }
    let _state = StateDir::set();
    let dir = tempfile::TempDir::new().unwrap();
    let previews = real_seatbelt(dir.path());
    let escape = r#"python3 -c "import os; os.setpgid(0, 0); os.execvp('sh', ['sh', '-c', os.environ['SERVE']])""#;
    let wt = a_workspace(dir.path(), serde_json::json!({"schema_version": "1.1", "command": escape, "env": {"PORT": "{port}", "HOST": "{host}", "SERVE": serve_on(HOST_ENV)}}));
    previews.start_in(BackendKind::Seatbelt, "w", &wt, &RecipeSource::Checkout(wt.clone())).unwrap();
    let v = until_status(&previews, "w", &["failed", "ready"], Duration::from_secs(30));
    assert_eq!(v["status"], "ready", "{v}");
    let port = v["port"].as_u64().unwrap() as u16;
    assert!(previews.stop("w"));
    assert!(!listened(port), "the escaped server still holds port {port}");
    assert!(!http_ok(port, "/"), "the escaped server still answers");
}

/// (#3112 review, MINOR 6) `{host}` is new meaning in the recipe format, so
/// it is schema 1.1: a 1.1 recipe gets the dev server's address there; a
/// 1.0 recipe keeps `{host}` as the text it always was (in every sandbox),
/// and a darkmux that reads only 1.0 refuses a 1.1 recipe as newer rather
/// than pass `{host}` through. This darkmux reads both.
#[test]
fn host_is_recipe_schema_1_1_and_a_1_0_recipe_keeps_its_meaning() {
    assert_eq!(darkmux_types::data_version::PREVIEW_RECIPE_SCHEMA_VERSION, "1.1");
    let value = |v: &str, kind| {
        let recipe = a_recipe(serde_json::json!({"schema_version": v, "command": "x", "env": {"H": "{host}:{port}"}}));
        container_env_for(&recipe, 4100, 4200, kind, None).into_iter().find(|(k, _)| k == "H").unwrap().1
    };
    assert_eq!(value("1.1", BackendKind::Seatbelt), "127.0.0.1:4100");
    assert_eq!(value("1.1", BackendKind::Docker), "0.0.0.0:4100");
    assert_eq!(value("1.0", BackendKind::Seatbelt), "{host}:4100");
    assert_eq!(value("1.0", BackendKind::Docker), "{host}:4100");
    let older_reader = darkmux_types::data_version::newer(&serde_json::json!({"schema_version": "1.1"}), "1.0");
    assert_eq!(older_reader.as_deref(), Some("1.1"), "a 1.0 reader refuses a 1.1 recipe as newer");
}

/// (#3112 review, MINOR 9) A backend switch never deletes in the user's
/// own checkout: a workspace that is its own checkout is refused before
/// anything is deleted, the same guard `clean` has.
#[test]
fn a_backend_switch_never_deletes_in_the_users_own_checkout() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut at = a_run_in(&root);
    at.checkout = Some(at.workdir.clone());
    std::fs::create_dir_all(at.workdir.join("node_modules")).unwrap();
    let recipe = a_recipe(serde_json::json!({"schema_version": "1.1", "command": "x", "install": {"command": "npm ci", "unless_exists": "node_modules"}}));
    let refused = reinstall_for(BackendKind::Seatbelt, BackendKind::Docker, &recipe, &at);
    assert!(matches!(&refused, Err(Halt::Failed(why)) if why.contains("your own checkout")), "refused");
    assert!(at.workdir.join("node_modules").is_dir(), "nothing deleted in the user's checkout");
}

/// (#3112 review, MINOR 5) The reinstall deletes through the same guard as
/// `clean` (`remove_inside`): an `unless_exists` path under a folder that is
/// a link out of the workspace is refused, and nothing outside is deleted.
#[test]
fn a_backend_switch_never_deletes_through_a_linked_folder() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let at = a_run_in(&root);
    let outside = root.join("outside");
    std::fs::create_dir_all(outside.join("node_modules/keep")).unwrap();
    std::os::unix::fs::symlink(&outside, at.workdir.join("web")).unwrap();
    let recipe = a_recipe(serde_json::json!({"schema_version": "1.1", "command": "x", "install": {"command": "npm ci", "unless_exists": "web/node_modules"}}));
    let refused = reinstall_for(BackendKind::Seatbelt, BackendKind::Docker, &recipe, &at);
    assert!(refused.is_err(), "deleted through a link");
    assert!(outside.join("node_modules/keep").is_dir(), "deleted outside the workspace");
}

/// (#3112 review, MINOR 7) A preview on the host names its sandbox as a
/// sandbox, never a container: its start line, a start that never came,
/// and a stale heartbeat. Docker's wording is the one it always was.
#[test]
fn a_preview_on_the_host_is_never_called_a_container() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let at = a_run_in(&root);
    let recipe = a_recipe(serde_json::json!({"schema_version": "1.1", "command": "x"}));
    let spec = spec_in(BackendKind::Seatbelt, &recipe, &at, false, ("seatbelt".into(), None), Vec::new(), &Toolchain::default());
    assert!(spec.script.starts_with("echo 'darkmux-preview: sandbox started'"), "{}", spec.script);
    let log = root.join("log");
    std::fs::write(&log, "sandbox-exec: something failed\n").unwrap();
    let never = exit_reason_in(BackendKind::Seatbelt, &recipe, &log, Err(std::io::Error::other("gone")));
    assert!(never.starts_with("the preview sandbox did not start"), "{never}");
    std::fs::write(&log, format!("{ON_HOST_STARTED_LINE}\n{STALE_LINE}\n")).unwrap();
    let stale = exit_reason_in(BackendKind::Seatbelt, &recipe, &log, Err(std::io::Error::other("gone")));
    assert_eq!(stale, "the preview's sandbox stopped itself: darkmux stopped answering");
    std::fs::write(&log, format!("{STARTED_LINE}\n{STALE_LINE}\n")).unwrap();
    assert_eq!(exit_reason(&recipe, &log, Err(std::io::Error::other("gone"))), "the preview's container stopped itself: darkmux stopped answering");
}
