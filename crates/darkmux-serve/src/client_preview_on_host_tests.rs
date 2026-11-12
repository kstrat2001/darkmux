//! (#3112) Client previews in a sandbox on the host (Seatbelt): the
//! toolchain grant, placement, the loopback host, the listener watch, the
//! backend a preview is stopped in, and a backend switch's `node_modules`.

use super::*;
use darkmux_platform::GroupListener;

fn a_recipe(v: serde_json::Value) -> Recipe {
    parse_recipe(&v.to_string()).unwrap()
}

/// An executable file at `path` holding `text`.
fn program(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A fake host: `home` with nvm's layout (node a binary, npm a link to a
/// script whose shebang asks `env` for node), and a "system" dir the
/// sandbox reads with no grant, holding `sh`.
struct FakeHost {
    _dir: tempfile::TempDir,
    home: PathBuf,
    nvm: PathBuf,
    system: PathBuf,
}

fn a_fake_host() -> FakeHost {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let home = root.join("home");
    let nvm = home.join(".nvm/versions/node/v24.16.0");
    program(&nvm.join("bin/node"), "\x7fELF not a script");
    program(&nvm.join("lib/node_modules/npm/bin/npm-cli.js"), "#!/usr/bin/env node\n");
    std::os::unix::fs::symlink("../lib/node_modules/npm/bin/npm-cli.js", nvm.join("bin/npm")).unwrap();
    let system = root.join("system/bin");
    program(&system.join("sleep"), "\x7fELF");
    std::fs::create_dir_all(home.join(".cargo/bin")).unwrap();
    FakeHost { _dir: dir, home, nvm, system }
}

impl FakeHost {
    fn path_var(&self) -> String {
        format!("{}:{}:{}:relative/bin:", self.home.join(".cargo/bin").display(), self.nvm.join("bin").display(), self.system.display())
    }
    fn reads(&self) -> impl Fn(&Path) -> bool + '_ {
        move |p: &Path| p.starts_with(self.system.parent().unwrap())
    }
}

/// (#3112) The recipe's programs are found on `client serve`'s PATH; a
/// script's `env` interpreter is found too (npm needs node). Only the
/// install prefix of what the sandbox cannot already read is granted, read
/// only: nvm's version folder, never the home or `~/.nvm` as a whole. The
/// sandbox's PATH keeps the host's order, dropping what it cannot read.
#[test]
fn a_host_toolchain_is_found_on_path_and_only_its_install_prefix_is_granted() {
    let host = a_fake_host();
    let recipe = a_recipe(serde_json::json!({
        "schema_version": "1.0", "command": "PORT=1 exec npm run dev && sleep 1",
        "install": {"command": "cd . && npm ci", "unless_exists": "node_modules"}
    }));
    let got = on_host::resolve_toolchain(&recipe, BackendKind::Seatbelt, &host.path_var(), Some(&host.home), &host.reads()).unwrap();
    assert_eq!(got.read_only, vec![host.nvm.clone()], "nvm's version folder, nothing above it");
    assert_eq!(got.path.as_deref(), Some(format!("{}:{}", host.nvm.join("bin").display(), host.system.display()).as_str()));
    assert!(got.identity.contains(&host.nvm.join("bin/node").display().to_string()), "{}", got.identity);
    assert!(got.identity.contains("npm-cli.js"), "{}", got.identity);
}

/// (#3112) A program the recipe runs that is not on PATH is refused,
/// naming it, where darkmux looked, and how to fix it.
#[test]
fn a_missing_toolchain_is_refused_naming_what_was_looked_for() {
    let host = a_fake_host();
    let recipe = a_recipe(serde_json::json!({"schema_version": "1.0", "command": "pnpm dev"}));
    let why = on_host::resolve_toolchain(&recipe, BackendKind::Seatbelt, &host.path_var(), Some(&host.home), &host.reads()).unwrap_err();
    for part in ["`pnpm`", &host.nvm.join("bin").display().to_string(), "darkmux client serve", "seatbelt", "on PATH"] {
        assert!(why.contains(part), "missing `{part}` in: {why}");
    }
}

/// (#3112) A program whose install folder would be the home itself is
/// refused: a preview never reads the whole home.
#[test]
fn a_toolchain_installed_in_the_home_itself_is_refused() {
    let host = a_fake_host();
    program(&host.home.join("devserve"), "\x7fELF");
    let recipe = a_recipe(serde_json::json!({"schema_version": "1.0", "command": "devserve"}));
    let path = format!("{}:{}", host.home.display(), host.path_var());
    let why = on_host::resolve_toolchain(&recipe, BackendKind::Seatbelt, &path, Some(&host.home), &host.reads()).unwrap_err();
    assert!(why.contains("`devserve`") && why.contains("your home"), "{why}");
}

/// A `~/bin` program grants `~/bin`, not the home above it.
#[test]
fn a_program_in_a_bin_folder_under_the_home_grants_that_folder_only() {
    let host = a_fake_host();
    program(&host.home.join("bin/devserve"), "\x7fELF");
    let recipe = a_recipe(serde_json::json!({"schema_version": "1.0", "command": "devserve"}));
    let path = format!("{}:{}", host.home.join("bin").display(), host.path_var());
    let got = on_host::resolve_toolchain(&recipe, BackendKind::Seatbelt, &path, Some(&host.home), &host.reads()).unwrap();
    assert_eq!(got.read_only, vec![host.home.join("bin")]);
}

/// The programs a recipe's command runs: the first word of each simple
/// command, past assignments, redirections and `exec`/`env`; `env` alone is
/// the program; shell words, paths and variables are not programs on PATH.
#[test]
fn a_commands_programs_are_the_first_word_of_each_simple_command() {
    let got = on_host::programs_of("env > env.txt; cat 'x' > leak.txt 2>&1; PORT=1 exec python3 -m http.server && cd web && ./run.sh | $X || env -i A=1 node a.js & (nohup  npx vite) >out 2> err");
    assert_eq!(got, ["env", "cat", "python3", "node", "npx"]);
    assert!(on_host::programs_of("if true; then echo; fi").iter().all(|p| p == "true" || p == "echo"));
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
        "schema_version": "1.0", "command": "npm run dev",
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
    let at = |pid, addr: &str, port| GroupListener { pid, addr: addr.parse().unwrap(), port };
    let all = vec![at(1, "127.0.0.1", 4100), at(1, "::1", 4101), at(1, "::ffff:127.0.0.1", 4102), at(2, "::", 4200), at(3, "0.0.0.0", 9), at(3, "192.168.1.5", 10)];
    let exposed = on_host::beyond_loopback(&all);
    assert_eq!(exposed, vec![all[3].clone(), all[4].clone(), all[5].clone()]);
    let why = on_host::exposure_reason(BackendKind::Seatbelt, &exposed, 4100, 4200);
    for part in ["[::]:4200 (the HMR port, pid 2)", "0.0.0.0:9 (pid 3)", "192.168.1.5:10 (pid 3)", "seatbelt", "127.0.0.1", "#3559", "server.hmr.host"] {
        assert!(why.contains(part), "missing `{part}` in: {why}");
    }
}

/// (#3112) Why a dev server on the host never answered, from what its
/// group listens on: nothing, IPv6 loopback only, or loopback but no answer.
#[test]
fn an_on_host_dev_server_that_never_answered_says_why_from_its_listeners() {
    let at = |addr: &str, port| GroupListener { pid: 1, addr: addr.parse().unwrap(), port };
    let none = on_host::not_ready_reason(BackendKind::Seatbelt, 4100, Some(&[]));
    assert!(none.contains("nothing in the preview listens on port 4100") && none.contains("127.0.0.1"), "{none}");
    let v6 = on_host::not_ready_reason(BackendKind::Seatbelt, 4100, Some(&[at("::1", 4100)]));
    assert!(v6.contains("[::1]") && v6.contains("127.0.0.1"), "{v6}");
    let quiet = on_host::not_ready_reason(BackendKind::Seatbelt, 4100, Some(&[at("127.0.0.1", 4100)]));
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

/// Whether this host can list a process group's sockets (the watch's
/// source); the tests that need it return early where it cannot.
fn host_lists_sockets() -> bool {
    darkmux_platform::current().tcp_group_listeners(unsafe { libc::getpgid(0) } as u32).is_some()
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
    let previews = previews.with_exposure_every(Duration::from_millis(200));
    let wt = a_workspace(dir.path(), serde_json::json!({"schema_version": "1.0", "command": serve_on("'0.0.0.0'"), "env": {"PORT": "{port}"}}));
    previews.start_in(BackendKind::Seatbelt, "w", &wt, &RecipeSource::Checkout(wt.clone())).unwrap();
    let v = until_status(&previews, "w", &["failed", "ready"], Duration::from_secs(20));
    assert_eq!(v["status"], "failed", "{v}");
    let why = v["error"].as_str().unwrap();
    assert!(why.contains(&format!("0.0.0.0:{}", v["port"])) && why.contains("beyond this Mac's loopback"), "{why}");
    assert!(!seatbelt.removed.lock().unwrap().is_empty(), "its sandbox is removed");
    let wt = a_workspace(dir.path(), serde_json::json!({"schema_version": "1.0", "command": serve_on(HOST_ENV), "env": {"PORT": "{port}", "HOST": "{host}"}}));
    previews.start_in(BackendKind::Seatbelt, "w", &wt, &RecipeSource::Checkout(wt.clone())).unwrap();
    let v = until_status(&previews, "w", &["failed", "ready"], Duration::from_secs(20));
    assert_eq!(v["status"], "ready", "{v}");
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
    let late = "(sleep 2; exec /usr/bin/python3 -c \"import socket,time;s=socket.socket();s.bind(('0.0.0.0',0));s.listen();time.sleep(60)\") & ".to_string() + &serve_on(HOST_ENV);
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
    let source = RecipeSource::Checkout(wt.clone());
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
        Previews::new(dir.path().join("p"), Arc::new(OnHost { kind: BackendKind::Seatbelt })),
        BackendKind::Seatbelt,
        |name| {
            let g = group(name);
            (g, darkmux_platform::current().tcp_group_listeners(g))
        },
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
    let host = OnHost { kind: BackendKind::Seatbelt };
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
    let wide = GroupListener { pid: 7, addr: "0.0.0.0".parse().unwrap(), port: 9 };
    let near = GroupListener { pid: 7, addr: "127.0.0.1".parse().unwrap(), port: 8 };
    let once = darkmux_platform::FakePlatform { group_listeners: vec![(5, vec![wide.clone(), near])], ..Default::default() };
    assert_eq!(on_host::exposed_in(&once, BackendKind::Seatbelt, 5), Ok(vec![wide]));
    assert_eq!(on_host::exposed_in(&once, BackendKind::Seatbelt, 6), Ok(vec![]));
    let blind = darkmux_platform::FakePlatform { group_listeners_unreadable_reads: AtomicU32::new(1), ..Default::default() };
    assert_eq!(on_host::exposed_in(&blind, BackendKind::Seatbelt, 5), Ok(vec![]), "one unreadable read is asked again");
    let blind = darkmux_platform::FakePlatform { group_listeners_unreadable_reads: AtomicU32::new(2), ..Default::default() };
    let why = on_host::exposed_in(&blind, BackendKind::Seatbelt, 5).unwrap_err();
    assert!(why.contains("cannot read what it listens on") && why.contains("darkmux config set runtime.sandbox docker"), "{why}");
}
