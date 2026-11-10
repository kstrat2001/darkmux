//! Tests for `client_pairing`: the store, the CLI-side verbs, and the HTTP
//! contract driven through the real router. No test dispatches: the executor
//! is injected (and the production child runner is driven with a fake child),
//! and the machine's `clients.profiles` policy is injected too, so no test
//! reads a real `config.json`.

use super::*;
use crate::client_profiles::{describe_profile, describe_profiles, work_stays_here};
use crate::client_git::WorkCopy;

/// A test repository at `dir` as host git sees it (its own `.git`).
fn wc(dir: &Path) -> WorkCopy {
    WorkCopy {
        work_tree: dir.to_path_buf(),
        git_dir: dir.join(".git"),
    }
}
use axum::body::Body;
use axum::http::Request;
use std::sync::atomic::{AtomicU64, Ordering};
use tower::ServiceExt;

const SECRET: &str = "app-secret-0123456789abcdef";

type Policy = Result<Option<Vec<String>>, String>;

struct Harness {
    dir: tempfile::TempDir,
    store: Arc<ClientStore>,
    clock: Arc<AtomicU64>,
    base: PathBuf,
    ran: Arc<Mutex<Vec<WorkSpec>>>,
    /// What `clients.profiles` reads as right now (default: unset).
    policy: Arc<Mutex<Policy>>,
    /// What `clients.preview.enabled` reads as right now (default here: on).
    preview_on: Arc<Mutex<bool>>,
    /// What `clients.preview.idle_minutes` reads as right now (default 15).
    preview_idle: Arc<Mutex<Result<u64, String>>>,
    /// Plays the preview containers' part, without Docker.
    containers: Arc<crate::client_preview::HostEmulator>,
    /// URLs the injected clone was asked for, and dirs it fetched.
    cloned: Arc<Mutex<Vec<String>>>,
    fetched: Arc<Mutex<Vec<PathBuf>>>,
    /// A git repo with one commit, standing in for every remote.
    fixture: PathBuf,
    /// What the run lifecycle says each running work item is doing
    /// (default: nothing besides its turns).
    activity: Arc<Mutex<(Option<crate::runs::RunActivity>, Option<u64>)>>,
    state: ClientServerState,
}

impl Harness {
    fn set_policy(&self, p: Policy) {
        *self.policy.lock().unwrap() = p;
    }
    fn allow(&self, names: &[&str]) {
        self.set_policy(Ok(Some(names.iter().map(|s| s.to_string()).collect())));
    }
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(ClientStore::at(dir.path().join("clients.json")));
    let clock = Arc::new(AtomicU64::new(1_000_000));
    let base = dir.path().join("worktrees");
    std::fs::create_dir_all(base.join("wt1")).unwrap();
    let canon_base = base.canonicalize().unwrap();
    let ran = Arc::new(Mutex::new(Vec::new()));
    let policy: Arc<Mutex<Policy>> = Arc::new(Mutex::new(Ok(None)));
    let preview_on = Arc::new(Mutex::new(true));
    let pe = preview_on.clone();
    let preview_idle = Arc::new(Mutex::new(Ok(15)));
    let pi = preview_idle.clone();
    let pc = clock.clone();
    let containers = Arc::new(crate::client_preview::HostEmulator::default());
    let emu: Arc<dyn crate::client_preview::PreviewContainers> = containers.clone();
    let activity = Arc::new(Mutex::new((None, None)));
    let act = activity.clone();
    let (c, r, pol) = (clock.clone(), ran.clone(), policy.clone());
    let fixture = dir.path().join("fixture");
    fixture_repo(&fixture);
    let cloned = Arc::new(Mutex::new(Vec::new()));
    let fetched = Arc::new(Mutex::new(Vec::new()));
    let (fx, cl, fe, wb) = (
        fixture.clone(),
        cloned.clone(),
        fetched.clone(),
        canon_base.clone(),
    );
    let state = ClientServerState {
        store: store.clone(),
        now: Arc::new(move || c.load(Ordering::SeqCst)),
        role_known: Arc::new(|role| Ok(["coder", "radio-host", "researcher"].contains(&role))),
        // A requested profile resolves to itself when defined here; an
        // omitted one resolves through the role's binding, as `darkmux
        // dispatch <role>` would: `coder` is bound to `fast`, `radio-host` to
        // `deep`. `fast@studio` "resolves" on purpose, so the `@` guard in
        // `admit` is the only thing refusing it.
        resolve_profile: Arc::new(|role, profile| match profile {
            Some(p) if ["fast", "deep", "hosted", "fast@studio"].contains(&p) => Ok(p.to_string()),
            Some("utility") => Err(
                "profile utility holds only the utility model; work never runs on it".to_string(),
            ),
            Some(p) => Err(format!("profile {p} is not defined on this machine")),
            None if role == "coder" => Ok("fast".to_string()),
            None if role == "radio-host" => Ok("deep".to_string()),
            // `researcher` is bound to the hosted profile.
            None if role == "researcher" => Ok("hosted".to_string()),
            None => Err("no profile resolves".to_string()),
        }),
        profiles_policy: Arc::new(move || pol.lock().unwrap().clone()),
        // `fast` and `deep` run on this machine; `hosted` sends work to a
        // hosted API, so it isn't local. `utility` (LM Studio, but only the
        // utility model) and `fast@studio` "are local" on purpose, so the
        // listing's resolve and `@` filters are the only things that keep
        // them out.
        profile_is_local: Arc::new(|_role, name| match name {
            "fast" | "deep" | "utility" | "fast@studio" => Ok(true),
            "hosted" => Ok(false),
            other => Err(format!("profile {other} is not in the registry")),
        }),
        registry_profiles: Arc::new(|| {
            Ok(vec![
                "deep".into(),
                "fast".into(),
                "hosted".into(),
                "utility".into(),
                "fast@studio".into(),
            ])
        }),
        machine: ClientMachine {
            id: "m-0123456789abcdef".into(),
            name: "studio".into(),
        },
        profile_info: Arc::new(|_role, names| names.iter().map(|n| test_profile_info(n)).collect()),
        check_workdir: Arc::new(move |p| {
            darkmux_types::workdir::validate_remote_workdir_in(p, &canon_base)
                .map_err(|e| format!("{e:#}"))
        }),
        execute: Arc::new(move |spec: WorkSpec, cancel: Arc<CancelFlag>| {
            r.lock().unwrap().push(spec.clone());
            // A "slow" message holds its seat a while, so a test can fill the cap.
            if spec.message == "slow" {
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
            // "edit-wait" changes the work copy, then runs on like "wait".
            if spec.message == "edit-wait" {
                if let Some(w) = &spec.workdir {
                    std::fs::write(w.join("in-progress.txt"), "half done\n").unwrap();
                }
            }
            // "edit-fail" changes the work copy, then fails.
            if spec.message == "edit-fail" {
                if let Some(w) = &spec.workdir {
                    std::fs::write(w.join("in-progress.txt"), "half done\n").unwrap();
                }
                return Err(RunError { reason: "exit 1".into(), error: "it broke".into(), code: None });
            }
            // "finish-then-cancel" edits and finishes, and its cancel lands
            // only after its result is in (after `dispatch.complete`).
            if spec.message == "finish-then-cancel" {
                if let Some(w) = &spec.workdir {
                    std::fs::write(w.join("made-by-run.txt"), "one\n").unwrap();
                }
                cancel.request();
            }
            // "refused-busy" fails as a dispatch whose `dispatch.error` said `busy`.
            if spec.message == "refused-busy" {
                return Err(RunError { reason: "exit 1".into(), error: "memory is held".into(), code: Some("busy".into()) });
            }
            // A "wait" message runs until it is cancelled, as a dispatch child would.
            if spec.message == "wait" || spec.message == "edit-wait" {
                for _ in 0..500 {
                    if cancel.requested() {
                        return Err(RunError {
                            reason: "exit 143/SIGTERM".into(),
                            error: "stopped".into(),
                            code: None,
                        });
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            // An "edit" message changes the work copy, as a coder would.
            if spec.message == "edit" {
                if let Some(w) = &spec.workdir {
                    std::fs::write(w.join("made-by-run.txt"), "one\ntwo\n").unwrap();
                }
            }
            // "edit-readme" changes a tracked file; "edit-other" adds another.
            if spec.message == "edit-readme" {
                if let Some(w) = &spec.workdir {
                    std::fs::write(w.join("README.md"), "changed by the run\n").unwrap();
                }
            }
            if spec.message == "edit-readme-again" {
                if let Some(w) = &spec.workdir {
                    std::fs::write(w.join("README.md"), "changed again\n").unwrap();
                }
            }
            if spec.message == "edit-other" {
                if let Some(w) = &spec.workdir {
                    std::fs::write(w.join("other.txt"), "tweak\n").unwrap();
                }
            }
            // "nested-init" makes an empty repository in `sub/` (as `git
            // init` in a subfolder does) and edits a tracked file.
            if spec.message == "nested-init" {
                if let Some(w) = &spec.workdir {
                    std::fs::create_dir_all(w.join("sub")).unwrap();
                    assert!(std::process::Command::new("git").args(["init", "--quiet"]).arg(w.join("sub")).status().unwrap().success());
                    std::fs::write(w.join("README.md"), "changed by the run\n").unwrap();
                }
            }
            // "unreadable" leaves a file nobody can read, beside an edit.
            if spec.message == "unreadable" {
                if let Some(w) = &spec.workdir {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::write(w.join("locked.txt"), "secret\n").unwrap();
                    std::fs::set_permissions(w.join("locked.txt"), std::fs::Permissions::from_mode(0o000)).unwrap();
                    std::fs::write(w.join("README.md"), "changed by the run\n").unwrap();
                }
            }
            // A hostile run: it plants git config the host would obey (an
            // fsmonitor hook, or a clean filter its .gitattributes selects)
            // in a fresh `.git` where the work copy's was, then edits a file.
            if let Some(kind) = spec.message.strip_prefix("plant-") {
                if let Some(w) = &spec.workdir {
                    plant_git_config(w, kind);
                    std::fs::write(w.join("made-by-run.txt"), "one\ntwo\n").unwrap();
                }
            }
            Ok(
                serde_json::json!({"result": "stop", "final_assistant": format!("ran {}", spec.role)}),
            )
        }),
        works: Arc::new(Mutex::new(HashMap::new())),
        work_activity: Arc::new(move |_role, _work_id| *act.lock().unwrap()),
        worktrees_base: Arc::new(move || Ok(wb.clone())),
        // Every "remote" is the fixture; origin is then set to the URL asked
        // for, as a real clone would leave it. A URL naming "fail" fails
        // after leaving a partial directory behind, as an interrupted clone can.
        clone_repo: Arc::new(move |url, git_ref, dest, git_dir| {
            cl.lock().unwrap().push(url.to_string());
            if url.contains("fail") {
                std::fs::create_dir_all(dest.join("half")).unwrap();
                return Err("authentication failed".into());
            }
            let gd = git_dir.to_str().unwrap().to_string();
            let mut args = vec!["clone", "--quiet", "--separate-git-dir", gd.as_str()];
            if let Some(r) = git_ref {
                args.extend(["--branch", r]);
            }
            let fx = fx.to_str().unwrap().to_string();
            let d = dest.to_str().unwrap().to_string();
            args.extend([fx.as_str(), d.as_str()]);
            run_git(None, &args);
            run_git(Some(dest), &["remote", "set-url", "origin", url]);
            // The identity the operator's own git config gives a commit:
            // a machine with none (a CI runner, whose account has an empty
            // name) refuses the run's commit otherwise.
            run_git(Some(dest), &["config", "user.name", "Owner"]);
            run_git(Some(dest), &["config", "user.email", "owner@example.com"]);
            Ok(())
        }),
        // Records every fetch; a checkout whose origin is a local path (a
        // test fixture) is really fetched, so refs move as they would.
        fetch_repo: Arc::new(move |d| {
            fe.lock().unwrap().push(d.to_path_buf());
            let origin = std::process::Command::new("git")
                .arg("-C")
                .arg(d)
                .args(["remote", "get-url", "origin"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            if origin.starts_with('/') {
                let ok = std::process::Command::new("git")
                    .arg("-C")
                    .arg(d)
                    .args(["fetch", "--quiet", "origin"])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if !ok {
                    return Err(crate::client_workspace::Coded::new(
                        darkmux_types::client_code::ClientCode::FetchFailed,
                        "fetch failed",
                    ));
                }
            }
            Ok(())
        }),
        workspace_asks: Arc::new(crate::client_workspace::Asks::default()),
        // On the harness's clock, so a test moves idle time by hand.
        previews: Arc::new(
            crate::client_preview::Previews::new(dir.path().join("previews"), emu.clone())
                .with_clock(Arc::new(move || pc.load(Ordering::SeqCst))),
        ),
        bound: Some(BOUND.parse().unwrap()),
        // On here, so the preview tests can run one; production's default is
        // off (`clients.preview.enabled`).
        preview_enabled: Arc::new(move || Ok(*pe.lock().unwrap())),
        preview_idle_minutes: Arc::new(move || pi.lock().unwrap().clone()),
        peer_processes: Arc::new(|_bound, wanted| Some(test_peer_processes(wanted))),
        pair_peers: Arc::new(Mutex::new(HashMap::new())),
        pair_lookups: Arc::new(PeerRounds::default()),
    };
    Harness {
        activity,
        preview_on,
        preview_idle,
        containers,
        cloned,
        fetched,
        fixture,
        base,
        dir,
        store,
        clock,
        ran,
        policy,
        state,
    }
}

/// The process at each of `wanted`'s ends, as the harness tells it: the app
/// is pid 100 (any port); `CONTAINER_PEER_PORTS..` are Docker's process
/// (pid 900); `UNKNOWN_PEER_PORTS..` cannot be told.
fn test_peer_processes(wanted: &[SocketAddr]) -> HashMap<SocketAddr, u32> {
    wanted
        .iter()
        .filter_map(|peer| match peer.port() {
            p if p >= UNKNOWN_PEER_PORTS => None,
            p if p >= CONTAINER_PEER_PORTS => Some((*peer, 900)),
            _ => Some((*peer, 100)),
        })
        .collect()
}

/// The address the harness's listener is taken to be bound on.
const BOUND: &str = "127.0.0.1:8767";

/// Peer ports the harness reads as Docker's process (pid 900), the way
/// every container's connection reaches this machine.
const CONTAINER_PEER_PORTS: u16 = 20_000;
/// Peer ports whose process the harness cannot tell.
const UNKNOWN_PEER_PORTS: u16 = 30_000;

/// A request as this machine's app sends it: from a loopback peer, with a
/// `Host` naming the listener (each added only when the test set none).
async fn call(h: &Harness, mut req: Request<Body>) -> (u16, Value) {
    if req.extensions().get::<axum::extract::ConnectInfo<SocketAddr>>().is_none() {
        req.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 5555))));
    }
    if !req.headers().contains_key(axum::http::header::HOST) {
        req.headers_mut().insert(axum::http::header::HOST, BOUND.parse().unwrap());
    }
    call_raw(h, req).await
}

/// A request exactly as built: no peer or `Host` added.
async fn call_raw(h: &Harness, req: Request<Body>) -> (u16, Value) {
    let resp = router(h.state.clone()).oneshot(req).await.unwrap();
    let code = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|e| panic!("every response is JSON ({e}): {bytes:?}"));
    (code, v)
}

fn post(path: &str, bearer: Option<&str>, body: Value) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn get(path: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut b = Request::builder().method("GET").uri(path);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(Body::empty()).unwrap()
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// The body an app built against the first contract sends: `scopes` included
/// (it is ignored).
fn pair_body() -> Value {
    serde_json::json!({
        "device_name": "Sam's iPhone",
        "code": "4F7-K2Q",
        "pair_secret": SECRET,
        "scopes": {"profiles": ["fast"], "workspace": false},
    })
}

async fn request_pairing(h: &Harness) -> String {
    let (code, v) = call(h, post("/client/pair", None, pair_body())).await;
    assert_eq!(code, 202, "{v}");
    v["request_id"].as_str().unwrap().to_string()
}

async fn poll(h: &Harness, id: &str, secret: &str) -> (u16, Value) {
    call(
        h,
        get(&format!("/client/pair/{id}"), &[("x-pair-secret", secret)]),
    )
    .await
}

/// Pair, approve through the CLI-side verb, collect the token. `wt1` is a
/// workspace darkmux made for the first client a test pairs.
async fn paired(h: &Harness) -> String {
    let id = request_pairing(h).await;
    approve(&h.store, &id, (h.state.now)()).unwrap();
    let (code, v) = poll(h, &id, SECRET).await;
    assert_eq!(code, 200, "{v}");
    if let [only] = h.store.read().unwrap().clients.as_slice() {
        own(h, "wt1", &only.id);
    }
    v["token"].as_str().unwrap().to_string()
}

/// The id of the paired client named `name`.
fn client_id(h: &Harness, name: &str) -> String {
    h.store.read().unwrap().clients.into_iter().find(|c| c.name == name).unwrap().id
}

// ─── Only this machine ───────────────────────────────────────────────────

/// A `POST /client/pair` from a loopback peer carrying `headers` (a `Host`
/// among them, or none), with `content_type`.
fn forged_pair(headers: &[(&str, &str)], content_type: &str) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/client/pair")
        .header("content-type", content_type);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let mut req = b.body(Body::from(pair_body().to_string())).unwrap();
    req.extensions_mut()
        .insert(axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 5555))));
    req
}

fn pending_count(h: &Harness) -> usize {
    h.store.read().unwrap().pending.len()
}

/// A page rebound by DNS to 127.0.0.1, a browser page on any origin, and a
/// form post (`text/plain`, which a browser sends without asking) are all
/// refused before anything is recorded: native apps on this machine send
/// none of these.
#[tokio::test]
async fn a_request_that_is_not_this_machines_app_is_refused_and_records_nothing() {
    let h = harness();
    let json = "application/json";
    for (why, req) in [
        ("a rebound Host", forged_pair(&[("host", "evil.example")], json)),
        ("a rebound Host with a port", forged_pair(&[("host", "evil.example:8767")], json)),
        ("an Origin", forged_pair(&[("host", BOUND), ("origin", "http://evil.example")], json)),
        ("a null Origin", forged_pair(&[("host", BOUND), ("origin", "null")], json)),
        ("a text/plain body", forged_pair(&[("host", BOUND)], "text/plain")),
        ("a form body", forged_pair(&[("host", BOUND)], "application/x-www-form-urlencoded")),
        ("another port", forged_pair(&[("host", "127.0.0.1:9999")], json)),
        ("a proxy header", forged_pair(&[("host", BOUND), ("x-forwarded-for", "100.64.0.7")], json)),
        ("no Host", forged_pair(&[], json)),
    ] {
        let (code, v) = call_raw(&h, req).await;
        assert!(code == 403 || code == 415, "{why}: {code} {v}");
        assert!(v["error"].is_string(), "{why}: {v}");
        assert_eq!(pending_count(&h), 0, "{why}: a pending request was recorded");
    }
    // A request from a peer that is not loopback, whatever it says.
    let mut req = forged_pair(&[("host", BOUND)], json);
    req.extensions_mut()
        .insert(axum::extract::ConnectInfo(SocketAddr::from(([100, 64, 0, 7], 5555))));
    let (code, _) = call_raw(&h, req).await;
    assert_eq!(code, 403);
    // And one with no peer at all (no connection to judge).
    let mut req = forged_pair(&[("host", BOUND)], json);
    req.extensions_mut().remove::<axum::extract::ConnectInfo<SocketAddr>>();
    let (code, _) = call_raw(&h, req).await;
    assert_eq!(code, 403);
    assert_eq!(pending_count(&h), 0);
    // The same request from this machine's app goes through.
    for host in [BOUND, "localhost:8767", "localhost", "[::1]:8767", "127.0.0.1"] {
        let (code, v) = call_raw(&h, forged_pair(&[("host", host)], "application/json; charset=utf-8")).await;
        assert_eq!(code, 202, "{host}: {v}");
    }
}

/// Every route sits behind the gate, the authenticated ones included, and a
/// bodyless POST (cancel, a preview start) needs no content type.
#[tokio::test]
async fn every_route_refuses_a_browser_and_a_bodyless_post_needs_no_type() {
    let h = harness();
    let token = paired(&h).await;
    let auth = bearer(&token);
    for (method, path) in [
        ("GET", "/client/whoami"),
        ("GET", "/client/profiles"),
        ("POST", "/client/work"),
        ("GET", "/client/work/w-1"),
        ("POST", "/client/work/w-1/cancel"),
        ("POST", "/client/workspaces"),
        ("GET", "/client/workspaces/wt1"),
        ("POST", "/client/workspaces/wt1/preview"),
        ("DELETE", "/client/workspaces/wt1/preview"),
        ("GET", "/client/recipe?workspace=wt1"),
        ("GET", "/nope"),
    ] {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", &auth)
            .header("origin", "https://evil.example")
            .body(Body::empty())
            .unwrap();
        let (code, v) = call(&h, req).await;
        assert_eq!(code, 403, "{method} {path}: {v}");
    }
    let req = Request::builder()
        .method("POST")
        .uri("/client/work/w-unknown/cancel")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();
    let (code, v) = call(&h, req).await;
    assert_eq!(code, 404, "past the gate: {v}");
}

// ─── Pairing ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn pairing_happy_path_delivers_a_token_that_whoami_accepts() {
    let h = harness();
    let id = request_pairing(&h).await;
    assert_eq!(
        poll(&h, &id, SECRET).await,
        (200, serde_json::json!({"status": "pending"}))
    );

    let pending = list_pending(&h.store, (h.state.now)()).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].device_name, "Sam's iPhone");
    assert_eq!(pending[0].code, "4F7-K2Q");

    let name = approve(&h.store, &id, (h.state.now)()).unwrap();
    let (code, v) = poll(&h, &id, SECRET).await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["status"], "approved");
    assert_eq!(v["client_name"], name);
    assert_eq!(
        v["scopes"],
        serde_json::json!({"profiles": "local", "workspace": true}),
        "unset policy: any profile that runs on this machine"
    );
    let token = v["token"].as_str().unwrap();
    assert_eq!(token.len(), 64, "32 random bytes, hex");
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));

    let (code, who) = call(
        &h,
        get("/client/whoami", &[("authorization", &bearer(token))]),
    )
    .await;
    assert_eq!(code, 200, "{who}");
    assert_eq!(
        who,
        serde_json::json!({"client_name": name, "scopes": {"profiles": "local", "workspace": true},
                           "machine": {"id": "m-0123456789abcdef", "name": "studio"}})
    );
}

#[tokio::test]
async fn whoami_reports_the_machine_policy_as_it_is_now() {
    let h = harness();
    let token = paired(&h).await;
    h.allow(&["fast", "deep"]);
    let (_, who) = call(
        &h,
        get("/client/whoami", &[("authorization", &bearer(&token))]),
    )
    .await;
    assert_eq!(
        who["scopes"],
        serde_json::json!({"profiles": ["fast", "deep"], "workspace": true})
    );
}

#[tokio::test]
async fn a_pairing_request_may_omit_or_carry_any_scopes() {
    let h = harness();
    let mut b = pair_body();
    b.as_object_mut().unwrap().remove("scopes");
    let (code, v) = call(&h, post("/client/pair", None, b)).await;
    assert_eq!(code, 202, "no scopes: {v}");
    let mut b = pair_body();
    b["scopes"] = serde_json::json!({"profiles": [], "workspace": true});
    let (code, v) = call(&h, post("/client/pair", None, b)).await;
    assert_eq!(code, 202, "empty profiles: {v}");
    let text = std::fs::read_to_string(h.store.path()).unwrap();
    assert!(
        !text.contains("scopes"),
        "a pairing records identity only: {text}"
    );
}

#[tokio::test]
async fn the_token_and_pair_secret_are_stored_only_as_hashes() {
    let h = harness();
    let token = paired(&h).await;
    let text = std::fs::read_to_string(h.store.path()).unwrap();
    assert!(!text.contains(&token), "the token never reaches disk");
    assert!(!text.contains(SECRET), "the pair secret never reaches disk");
}

#[cfg(unix)]
#[test]
fn a_store_that_existed_world_readable_is_tightened_to_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clients.json");
    std::fs::write(&path, "").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    ClientStore::at(path.clone()).read().unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[tokio::test]
async fn clients_json_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let h = harness();
    let _ = paired(&h).await;
    let mode = std::fs::metadata(h.store.path())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[tokio::test]
async fn a_wrong_pair_secret_is_a_404_and_reveals_nothing() {
    let h = harness();
    let id = request_pairing(&h).await;
    approve(&h.store, &id, (h.state.now)()).unwrap();
    let (code, v) = poll(&h, &id, "not-the-secret-at-all-xx").await;
    assert_eq!(code, 404, "{v}");
    assert!(v.get("token").is_none());
    let (code, _) = call(&h, get(&format!("/client/pair/{id}"), &[])).await;
    assert_eq!(code, 404, "a missing secret is a wrong secret");
    let (code, v) = poll(&h, &id, SECRET).await;
    assert_eq!(code, 200, "the wrong one consumed nothing");
    assert_eq!(v["status"], "approved");
}

#[tokio::test]
async fn an_unknown_request_id_is_a_404() {
    let h = harness();
    let (code, _) = poll(&h, "r-doesnotexist", SECRET).await;
    assert_eq!(code, 404);
}

#[tokio::test]
async fn a_request_expires_after_ten_minutes_and_cannot_then_be_approved() {
    let h = harness();
    let id = request_pairing(&h).await;
    h.clock.fetch_add(PENDING_TTL_SECS + 1, Ordering::SeqCst);
    assert_eq!(
        poll(&h, &id, SECRET).await,
        (200, serde_json::json!({"status": "expired"}))
    );
    assert!(
        list_pending(&h.store, (h.state.now)()).unwrap().is_empty(),
        "an expired request is not offered"
    );
    assert!(
        approve(&h.store, &id, (h.state.now)()).is_err(),
        "an expired request is refused"
    );
}

#[tokio::test]
async fn an_approved_request_not_collected_within_ten_minutes_expires() {
    let h = harness();
    let id = request_pairing(&h).await;
    approve(&h.store, &id, (h.state.now)()).unwrap();
    h.clock.fetch_add(PENDING_TTL_SECS + 1, Ordering::SeqCst);
    assert_eq!(
        poll(&h, &id, SECRET).await,
        (200, serde_json::json!({"status": "expired"}))
    );
    assert!(list_clients(&h.store).unwrap().is_empty());
}

#[tokio::test]
async fn the_token_is_delivered_exactly_once() {
    let h = harness();
    let id = request_pairing(&h).await;
    approve(&h.store, &id, (h.state.now)()).unwrap();
    let (code, v) = poll(&h, &id, SECRET).await;
    assert_eq!(code, 200);
    assert!(v["token"].is_string());
    let (code, v) = poll(&h, &id, SECRET).await;
    assert_eq!(code, 404, "the second poll gets nothing: {v}");
    assert_eq!(list_clients(&h.store).unwrap().len(), 1);
}

#[tokio::test]
async fn an_unreadable_policy_does_not_consume_the_token_delivery() {
    let h = harness();
    let id = request_pairing(&h).await;
    approve(&h.store, &id, (h.state.now)()).unwrap();
    h.set_policy(Err("config.json does not parse".into()));
    let (code, v) = poll(&h, &id, SECRET).await;
    assert_eq!(code, 503, "{v}");
    h.set_policy(Ok(None));
    let (code, v) = poll(&h, &id, SECRET).await;
    assert_eq!(code, 200, "still collectable: {v}");
    assert!(v["token"].is_string());
}

/// The reviewer's probe: one request waiting, then a flood from one
/// process that holds 40 requests open, then the app. Who sent each request
/// is found in rounds (one look at every connection, about 0.2 s here), each
/// answering every request waiting when it began, so the app waits for at
/// most two rounds whatever the flood holds open, and the rounds stay few.
/// A lone request asks nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_open_flood_delays_pairing_by_at_most_two_lookup_rounds() {
    let mut h = harness();
    let (rounds, inside, most) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let (c, n, m) = (rounds.clone(), inside.clone(), most.clone());
    h.state.peer_processes = Arc::new(move |_bound, wanted| {
        c.fetch_add(1, Ordering::SeqCst);
        m.fetch_max(n.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(200));
        n.fetch_sub(1, Ordering::SeqCst);
        Some(test_peer_processes(wanted))
    });
    let _ = request_pairing(&h).await;
    assert_eq!(rounds.load(Ordering::SeqCst), 0, "a lone request asks nothing");
    let flood = (0..40u16).map(|i| call(&h, pair_from(CONTAINER_PEER_PORTS + i, "X")));
    let app = async {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let started = std::time::Instant::now();
        let (code, v) = call(&h, pair_from(5555, "4F7-K2Q")).await;
        (code, v, started.elapsed())
    };
    let (flooded, (code, v, waited)) = tokio::join!(futures::future::join_all(flood), app);
    assert_eq!(code, 202, "{v}");
    // The round count below is the bound; the wall clock only has to tell
    // ~2 rounds apart from one lookup per queued request (8 s here).
    assert!(waited < std::time::Duration::from_secs(2), "the app waited {waited:?}");
    assert!(flooded.iter().all(|(code, _)| *code == 202));
    assert!(rounds.load(Ordering::SeqCst) <= 3, "{} rounds", rounds.load(Ordering::SeqCst));
    assert_eq!(most.load(Ordering::SeqCst), 1, "never two rounds at once");
}

/// A request whose sender went away while it queued for a round starts
/// none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_whose_sender_left_while_queued_starts_no_round() {
    let mut h = harness();
    let rounds = Arc::new(AtomicU64::new(0));
    let c = rounds.clone();
    h.state.peer_processes = Arc::new(move |_bound, wanted| {
        c.fetch_add(1, Ordering::SeqCst);
        Some(test_peer_processes(wanted))
    });
    let _ = request_pairing(&h).await;
    let held = h.state.pair_lookups.turn.clone().lock_owned().await;
    let gone = tokio::time::timeout(std::time::Duration::from_millis(50), call(&h, pair_from(CONTAINER_PEER_PORTS, "X"))).await;
    assert!(gone.is_err(), "it queued behind the held turn");
    drop(held);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(rounds.load(Ordering::SeqCst), 0);
}

/// An approval the app has not collected yet can be taken back: revoking
/// the client name it will have (or denying the request) leaves a token
/// nobody can collect, and the app hears `denied`.
#[tokio::test]
async fn an_uncollected_approval_can_be_revoked_or_denied() {
    let h = harness();
    let now = (h.state.now)();
    let id = request_pairing(&h).await;
    let name = approve(&h.store, &id, now).unwrap();
    assert!(revoke(&h.store, &name).unwrap(), "revoke finds the uncollected approval");
    assert_eq!(poll(&h, &id, SECRET).await, (200, serde_json::json!({"status": "denied"})));
    assert!(list_clients(&h.store).unwrap().is_empty(), "no token was ever minted");
    let id = request_pairing(&h).await;
    approve(&h.store, &id, now).unwrap();
    deny(&h.store, &id, now).unwrap();
    assert_eq!(poll(&h, &id, SECRET).await, (200, serde_json::json!({"status": "denied"})));
    assert!(list_clients(&h.store).unwrap().is_empty());
}

#[tokio::test]
async fn a_denied_request_says_so() {
    let h = harness();
    let id = request_pairing(&h).await;
    deny(&h.store, &id, (h.state.now)()).unwrap();
    assert_eq!(
        poll(&h, &id, SECRET).await,
        (200, serde_json::json!({"status": "denied"}))
    );
    assert!(list_clients(&h.store).unwrap().is_empty());
}

#[tokio::test]
async fn approve_by_code_finds_the_one_live_request() {
    let h = harness();
    let _ = request_pairing(&h).await;
    let found = find_by_code(&h.store, "4f7-k2q", (h.state.now)()).unwrap();
    assert_eq!(found.code, "4F7-K2Q", "codes match case-insensitively");
    assert!(find_by_code(&h.store, "ZZZ-ZZZ", (h.state.now)()).is_err());
}

#[tokio::test]
async fn a_malformed_pair_request_is_refused_with_json() {
    let h = harness();
    let mut short = pair_body();
    short["pair_secret"] = "short".into();
    let (code, v) = call(&h, post("/client/pair", None, short)).await;
    assert_eq!(code, 400);
    assert!(v["error"].is_string());
    let (code, v) = call(
        &h,
        Request::builder()
            .method("POST")
            .uri("/client/pair")
            .header("content-type", "application/json")
            .body(Body::from("{not json"))
            .unwrap(),
    )
    .await;
    assert_eq!(code, 400, "{v}");
}

#[tokio::test]
async fn an_oversized_body_is_refused_with_json() {
    let h = harness();
    let mut big = pair_body();
    big["device_name"] = "x".repeat(MAX_BODY_BYTES + 10).into();
    let (code, v) = call(&h, post("/client/pair", None, big)).await;
    assert_eq!(code, 413, "{v}");
    assert!(v["error"].is_string());
}

#[tokio::test]
async fn unknown_paths_and_methods_answer_json() {
    let h = harness();
    let (code, v) = call(&h, get("/nope", &[])).await;
    assert_eq!(code, 404);
    assert!(v["error"].is_string());
    let (code, v) = call(&h, get("/client/pair", &[])).await;
    assert_eq!(code, 405);
    assert!(v["error"].is_string());
}

/// A pairing request from a loopback peer on `port`, with a forged `Host`
/// naming the listener (what a container reaching this machine through
/// Docker's port forwarding can send), showing `code`.
fn pair_from(port: u16, code: &str) -> Request<Body> {
    let mut body = pair_body();
    body["code"] = code.into();
    let mut req = post("/client/pair", None, body);
    req.headers_mut().insert(axum::http::header::HOST, BOUND.parse().unwrap());
    req.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], port))));
    req
}

/// With no process to tell requests apart by, the waiting requests are
/// capped as a whole.
#[tokio::test]
async fn pending_requests_are_capped() {
    let h = harness();
    for i in 0..MAX_PENDING as u16 {
        let (code, v) = call(&h, pair_from(UNKNOWN_PEER_PORTS + i, "X")).await;
        assert_eq!(code, 202, "{v}");
    }
    let (code, v) = call(&h, pair_from(UNKNOWN_PEER_PORTS + 99, "X")).await;
    assert_eq!(code, 429, "{v}");
}

/// Every container's connection reaches this listener from one process on
/// this machine (Docker's), with whatever `Host` it forges. That process
/// holds at most a couple of the waiting requests, its newest replacing its
/// oldest, so a flood from it never fills the queue: the app on this
/// machine still pairs, by the code it shows.
#[tokio::test]
async fn a_container_flooding_pair_requests_never_crowds_out_the_app() {
    let h = harness();
    for i in 0..(MAX_PENDING as u16 * 2) {
        let (code, v) = call(&h, pair_from(CONTAINER_PEER_PORTS + i, &format!("EVIL-{i}"))).await;
        assert_eq!(code, 202, "{v}");
    }
    let id = request_pairing(&h).await;
    let now = (h.state.now)();
    // Docker's first request, made to an empty queue, asked the OS nothing
    // and counts only toward the cap; its others hold at most the bound.
    assert!(list_pending(&h.store, now).unwrap().len() <= PAIR_REQUESTS_PER_PROCESS + 2);
    let found = find_by_code(&h.store, "4f7-k2q", now).unwrap();
    assert_eq!(found.request_id, id);
    approve(&h.store, &id, now).unwrap();
    let (code, v) = poll(&h, &id, SECRET).await;
    assert_eq!(code, 200, "the app pairs: {v}");
    assert!(v["token"].is_string());
    // What it remembers of each request's process goes with the request.
    let _ = request_pairing(&h).await;
    assert!(!h.state.pair_peers.lock().unwrap().contains_key(&id), "a collected request is forgotten");
}

/// The app's own newest request replaces its oldest past the same bound,
/// so an app that asks again (a new code) is never refused. Its first, made
/// to an empty queue, asked the OS nothing, so it counts only toward the
/// cap and is never replaced.
#[tokio::test]
async fn an_app_that_asks_again_replaces_its_oldest_request() {
    let h = harness();
    let mut ids = Vec::new();
    for _ in 0..PAIR_REQUESTS_PER_PROCESS + 2 {
        ids.push(request_pairing(&h).await);
    }
    let live: Vec<String> = list_pending(&h.store, (h.state.now)()).unwrap().into_iter().map(|p| p.request_id).collect();
    assert_eq!(live, [ids[0].clone(), ids[2].clone(), ids[3].clone()], "the oldest it was asked about went");
    let (code, _) = poll(&h, &ids[1], SECRET).await;
    assert_eq!(code, 404);
}

// ─── Tokens ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn whoami_without_or_with_a_wrong_token_is_401() {
    let h = harness();
    let _ = paired(&h).await;
    let (code, v) = call(&h, get("/client/whoami", &[])).await;
    assert_eq!(code, 401);
    assert!(v["error"].is_string());
    let (code, _) = call(
        &h,
        get(
            "/client/whoami",
            &[("authorization", &bearer(&"0".repeat(64)))],
        ),
    )
    .await;
    assert_eq!(code, 401);
}

#[tokio::test]
async fn a_revoked_client_is_401_everywhere() {
    let h = harness();
    let token = paired(&h).await;
    let name = list_clients(&h.store).unwrap()[0].name.clone();
    assert!(revoke(&h.store, &name).unwrap());
    let (code, _) = call(
        &h,
        get("/client/whoami", &[("authorization", &bearer(&token))]),
    )
    .await;
    assert_eq!(code, 401);
    let (code, _) = call(
        &h,
        post("/client/work", Some(&token), work_body(Some("fast"), None)),
    )
    .await;
    assert_eq!(code, 401);
    assert!(
        !revoke(&h.store, &name).unwrap(),
        "a second revoke finds nothing"
    );
}

#[test]
fn secrets_are_compared_as_blake3_hashes_in_constant_time() {
    let stored = hash_hex("abc");
    assert!(secret_matches(&stored, "abc"));
    assert!(!secret_matches(&stored, "abd"));
    assert!(!secret_matches("not-hex", "abc"));
    // Structural: the one comparison site compares `blake3::Hash` values,
    // whose `PartialEq` is constant-time, never the hex strings.
    let src = include_str!("client_pairing.rs");
    let body = src
        .split("fn secret_matches")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(
        body.contains("blake3::Hash::from_hex"),
        "the stored hash is parsed into a blake3::Hash: {body}"
    );
    assert!(
        !body.contains("stored_hex =="),
        "never a string compare: {body}"
    );
    assert!(
        !body.contains("as_bytes() =="),
        "never a byte-slice compare: {body}"
    );
}

#[test]
fn tokens_are_32_random_bytes_hex() {
    let a = random_hex(32).unwrap();
    let b = random_hex(32).unwrap();
    assert_eq!(a.len(), 64);
    assert_ne!(a, b);
}

// ─── The config gate ─────────────────────────────────────────────────────

/// (review) `client serve` runs the config gate before it listens, like
/// `darkmux serve`: a misspelled `clients.profile` refuses the start instead
/// of reading as unset (which would allow every local profile).
#[test]
#[serial_test::serial]
fn client_serve_refuses_to_start_on_a_misspelled_clients_setting() {
    let cfg: darkmux_types::config::DarkmuxConfig =
        serde_json::from_str(r#"{"clients": {"profile": ["fast"]}}"#).unwrap();
    let guard = darkmux_types::config_access::set_config_for_test(cfg);
    let e = preflight().expect_err("refused").to_string();
    assert!(
        e.contains("unknown key `clients.profile`") && e.contains("did you mean `clients.profiles`?"),
        "{e}"
    );
    drop(guard);
    let clean: darkmux_types::config::DarkmuxConfig =
        serde_json::from_str(r#"{"clients": {"profiles": ["fast"]}}"#).unwrap();
    let _guard = darkmux_types::config_access::set_config_for_test(clean);
    assert!(preflight().is_ok());
}

/// The policy is read fresh on every request, through the same gate: a
/// misspelling written while serving refuses work, never reads as unset.
#[test]
#[serial_test::serial]
fn a_misspelled_policy_written_while_serving_refuses_work() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.json");
    let read = |text: &str| {
        std::fs::write(&path, text).unwrap();
        with_env_vars(&[], || {
            unsafe { std::env::remove_var("DARKMUX_CLIENTS_PROFILES") };
            read_profiles_policy_at(&path)
        })
    };
    let e = read(r#"{"clients": {"profile": ["fast"]}}"#).unwrap_err();
    assert!(e.contains("clients.profile") && e.contains("refusing client work"), "{e}");
    assert_eq!(read(r#"{"clients": {"profiles": ["fast"]}}"#), Ok(Some(vec!["fast".to_string()])));
    assert_eq!(read(r#"{}"#), Ok(None));
    std::fs::remove_file(&path).unwrap();
    assert_eq!(read_profiles_policy_at(&path), Ok(None), "no file is unset");
}

/// A config that cannot be read (here a folder where the file should be)
/// or that is not JSON refuses client work, naming the file; it never
/// reads as the defaults.
#[test]
fn an_unreadable_or_broken_config_refuses_client_work() {
    let tmp = tempfile::tempdir().unwrap();
    let e = read_config_at(tmp.path()).unwrap_err();
    assert!(e.contains("could not be read") && e.contains("refusing client work"), "{e}");
    let path = tmp.path().join("config.json");
    for text in ["{not json", r#"{"clients": {"profiles": 5}}"#] {
        std::fs::write(&path, text).unwrap();
        let e = read_config_at(&path).unwrap_err();
        assert!(e.contains("config.json") && e.contains("refusing client work"), "{text}: {e}");
    }
}

/// Replacing the store replaces the whole file; one it cannot write fails
/// rather than reporting the store saved, and leaves the old store whole.
#[test]
fn replacing_the_store_replaces_the_file_or_fails_and_keeps_the_old() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("clients.json");
    std::fs::write(&path, "x".repeat(4096)).unwrap();
    let doc = ClientsFile::default();
    replace_store(&path, &doc).unwrap();
    let saved = format!("{}\n", serde_json::to_string_pretty(&doc).unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), saved, "nothing of the old text is left");
    let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&path).unwrap().permissions());
    assert_eq!(mode & 0o777, 0o600, "written owner-only, not made so afterward");
    // A folder where its temp file goes: the new store cannot be written.
    std::fs::create_dir_all(tmp.path().join("clients.json.tmp/held")).unwrap();
    let changed = ClientsFile { legacy_owner: Some("c-1".into()), ..ClientsFile::default() };
    assert!(replace_store(&path, &changed).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), saved, "the old store is untouched");
}

// ─── Bind ────────────────────────────────────────────────────────────────

#[test]
fn the_bind_address_is_loopback_only() {
    assert_eq!(
        bind_addr(DEFAULT_PORT),
        SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT))
    );
}

#[tokio::test]
async fn the_listener_actually_binds_loopback() {
    let l = bind_listener(0).await.unwrap();
    assert!(l.local_addr().unwrap().ip().is_loopback());
}

// ─── Store ───────────────────────────────────────────────────────────────

#[test]
fn clients_json_refuses_unknown_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clients.json");
    std::fs::write(
        &path,
        r#"{"schema_version":"1.1","pending":[],"clients":[{"name":"a","created":1,"token_hash":"00","admin":true}]}"#,
    )
    .unwrap();
    let err = ClientStore::at(path.clone())
        .read()
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("clients[0].admin"),
        "names the key path: {err}"
    );
    assert!(err.contains("clients.json"), "names the file: {err}");

    std::fs::write(&path, r#"{"schema_version":"1.1","pendng":[]}"#).unwrap();
    let err = ClientStore::at(path.clone())
        .read()
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("did you mean `pending`"),
        "suggests the closest key: {err}"
    );

    std::fs::write(
        &path,
        r#"{"schema_version":"1.1","clients":[{"name":"a","created":1,"token_hash":"00","scopes":{}}]}"#,
    )
    .unwrap();
    assert!(
        ClientStore::at(path).read().is_err(),
        "at 1.1 a record's `scopes` is an unknown key"
    );
}

#[test]
fn a_1_0_store_is_read_with_its_scopes_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clients.json");
    std::fs::write(
        &path,
        r#"{"schema_version":"1.0","pending":[{"request_id":"r-1","device_name":"p","code":"A","pair_secret_hash":"00","scopes":{"profiles":["fast"],"workspace":false},"created":1,"status":"pending"}],"clients":[{"name":"a","created":1,"scopes":{"profiles":[],"workspace":true},"token_hash":"00"}]}"#,
    )
    .unwrap();
    let f = ClientStore::at(path).read().unwrap();
    assert_eq!(f.schema_version, CLIENTS_SCHEMA_VERSION);
    assert_eq!(f.pending.len(), 1);
    assert_eq!(f.clients[0].name, "a");
}

/// A 1.1 store's clients get an id each when it is read, written at once,
/// so the next read sees the same ids.
#[test]
fn a_1_1_store_gives_each_client_a_stable_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clients.json");
    std::fs::write(
        &path,
        r#"{"schema_version":"1.1","pending":[],"clients":[{"name":"a","created":1,"token_hash":"00"},{"name":"b","created":2,"token_hash":"00"}]}"#,
    )
    .unwrap();
    let store = ClientStore::at(path.clone());
    let first: Vec<String> = store.read().unwrap().clients.into_iter().map(|c| c.id).collect();
    assert!(first.iter().all(|id| id.starts_with("c-") && id.len() == 18), "{first:?}");
    assert_ne!(first[0], first[1]);
    let again: Vec<String> = store.read().unwrap().clients.into_iter().map(|c| c.id).collect();
    assert_eq!(first, again, "stable across reads");
    assert_eq!(store.peek().unwrap().clients.into_iter().map(|c| c.id).collect::<Vec<_>>(), first);
    assert!(std::fs::read_to_string(&path).unwrap().contains(r#""schema_version": "1.2""#));
}

/// `peek` (doctor's read) never writes: an older store stays as it was and
/// an absent one is not created.
#[test]
fn peeking_at_the_store_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clients.json");
    assert!(ClientStore::at(path.clone()).peek().unwrap().clients.is_empty());
    assert!(!path.exists(), "an absent store is not created");
    let old = r#"{"schema_version":"1.1","pending":[],"clients":[{"name":"a","created":1,"token_hash":"00"}]}"#;
    std::fs::write(&path, old).unwrap();
    assert_eq!(ClientStore::at(path.clone()).peek().unwrap().clients.len(), 1);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), old, "not migrated on disk");
}

/// `peek` reads under the store's shared lock, so it never catches a store
/// half rewritten (truncated, then written) by a writer holding the lock.
#[test]
fn peeking_never_sees_a_half_written_store() {
    use std::io::{Seek, Write};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clients.json");
    let store = ClientStore::at(path.clone());
    store.update(|f| { f.clients.push(ApprovedClient { name: "a".into(), id: "c-0000000000000001".into(), created: 1, token_hash: "00".into() }); Ok(()) }).unwrap();
    let full = std::fs::read_to_string(&path).unwrap();
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let p2 = path.clone();
    let writer = std::thread::spawn(move || {
        let mut guard = darkmux_types::flock::lock_exclusive(&p2).unwrap();
        let file = guard.file();
        file.set_len(0).unwrap();
        file.rewind().unwrap();
        file.write_all(&full.as_bytes()[..full.len() / 2]).unwrap();
        locked_tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        file.set_len(0).unwrap();
        file.rewind().unwrap();
        file.write_all(full.as_bytes()).unwrap();
    });
    locked_rx.recv().unwrap();
    let seen = store.peek().expect("never a half-written store");
    assert_eq!(seen.clients.len(), 1);
    writer.join().unwrap();
}

#[test]
fn an_absent_store_reads_as_empty() {
    let dir = tempfile::tempdir().unwrap();
    let f = ClientStore::at(dir.path().join("clients.json"))
        .read()
        .unwrap();
    assert!(f.pending.is_empty() && f.clients.is_empty());
}

#[test]
fn client_names_are_slugs_and_unique() {
    let taken = vec!["sam-s-iphone".to_string()];
    assert_eq!(client_name_for("Sam's iPhone", &taken), "sam-s-iphone-2");
    assert_eq!(client_name_for("!!!", &[]), "client");
}

// ─── Work ────────────────────────────────────────────────────────────────

fn work_body(profile: Option<&str>, workdir: Option<&str>) -> Value {
    serde_json::json!({"role": "coder", "message": "hello", "profile": profile, "workdir": workdir})
}

/// (#3451 review) A client's `workdir` must be exactly one workspace
/// directly under the worktrees base. The base itself, darkmux's held
/// repositories and records beside the workspaces (dot entries), a path
/// inside a workspace and a symlink to any of them are refused before
/// anything runs; a real workspace is admitted.
#[cfg(unix)]
#[tokio::test]
async fn a_client_workdir_is_exactly_one_workspace() {
    let h = harness();
    let token = paired(&h).await;
    let base = h.base.canonicalize().unwrap();
    std::fs::create_dir_all(base.join(".p.git/objects")).unwrap();
    std::fs::write(
        base.join(".p.template"),
        "e044900cd94d086c841616b05ed63638aa402be4\n",
    )
    .unwrap();
    std::fs::create_dir_all(base.join(".p.tmpl-dir")).unwrap();
    std::fs::create_dir_all(base.join("wt1/sub")).unwrap();
    std::os::unix::fs::symlink(base.join(".p.git"), base.join("linked")).unwrap();
    std::os::unix::fs::symlink(&base, h.dir.path().join("base-link")).unwrap();
    let refused = [
        base.clone(),
        base.join(".p.git"),
        base.join(".p.git/objects"),
        base.join(".p.template"),
        base.join(".p.tmpl-dir"),
        base.join("wt1/sub"),
        base.join("linked"),
        h.dir.path().join("base-link"),
    ];
    for w in refused {
        let (code, v) = call(
            &h,
            post(
                "/client/work",
                Some(&token),
                work_body(Some("fast"), w.to_str()),
            ),
        )
        .await;
        assert!(code == 403 || code == 404, "{}: {code} {v}", w.display());
    }
    assert!(h.ran.lock().unwrap().is_empty(), "nothing ran");
    let (code, v) = call(
        &h,
        post(
            "/client/work",
            Some(&token),
            work_body(Some("fast"), base.join("wt1").to_str()),
        ),
    )
    .await;
    assert_eq!(code, 202, "a real workspace: {v}");
}

fn work_for(role: &str) -> Value {
    serde_json::json!({"role": role, "message": "hello"})
}

async fn wait_done(h: &Harness, token: &str, id: &str) -> Value {
    for _ in 0..300 {
        let (code, v) = call(
            h,
            get(
                &format!("/client/work/{id}"),
                &[("authorization", &bearer(token))],
            ),
        )
        .await;
        assert_eq!(code, 200, "{v}");
        if v["status"] != "running" && v["status"] != "cancelling" {
            return v;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("work {id} never finished");
}

#[tokio::test]
async fn work_on_the_roles_own_profile_runs_and_its_result_comes_back() {
    let h = harness();
    let token = paired(&h).await;
    let (code, v) = call(
        &h,
        post("/client/work", Some(&token), work_body(Some("fast"), None)),
    )
    .await;
    assert_eq!(code, 202, "{v}");
    assert_eq!(v["profile"], "fast", "the reply names the profile");
    let id = v["work_id"].as_str().unwrap().to_string();
    let done = wait_done(&h, &token, &id).await;
    assert_eq!(done["status"], "done");
    assert_eq!(done["result"]["final_assistant"], "ran coder");
    let ran = h.ran.lock().unwrap();
    assert_eq!(ran.len(), 1);
    assert_eq!(
        (
            ran[0].work_id.as_str(),
            ran[0].profile.as_str(),
            ran[0].message.as_str()
        ),
        (id.as_str(), "fast", "hello")
    );
}

#[tokio::test]
async fn an_omitted_profile_resolves_through_the_role_binding_and_the_reply_names_it() {
    let h = harness();
    let token = paired(&h).await;
    for role in ["coder", "radio-host"] {
        let (code, v) = call(&h, post("/client/work", Some(&token), work_for(role))).await;
        assert_eq!(
            code, 202,
            "unset policy allows each role's own profile: {v}"
        );
        let _ = wait_done(&h, &token, v["work_id"].as_str().unwrap()).await;
    }
    let ran: Vec<String> = h
        .ran
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.profile.clone())
        .collect();
    assert_eq!(
        ran,
        ["fast", "deep"],
        "what runs is the resolved name, passed explicitly"
    );
    let mut null = work_for("coder");
    null["profile"] = Value::Null;
    let (code, v) = call(&h, post("/client/work", Some(&token), null)).await;
    assert_eq!(code, 202, "a null profile is an omitted one: {v}");
    assert_eq!(v["profile"], "fast");
}

#[tokio::test]
async fn with_the_setting_unset_any_local_profile_runs() {
    let h = harness();
    let token = paired(&h).await;
    let (code, v) = call(
        &h,
        post("/client/work", Some(&token), work_body(Some("deep"), None)),
    )
    .await;
    assert_eq!(
        code, 202,
        "deep runs on this machine, though coder's own is fast: {v}"
    );
    assert_eq!(v["profile"], "deep");
}

#[tokio::test]
async fn with_the_setting_unset_a_hosted_profile_is_403() {
    let h = harness();
    let token = paired(&h).await;
    let (code, v) = call(
        &h,
        post(
            "/client/work",
            Some(&token),
            work_body(Some("hosted"), None),
        ),
    )
    .await;
    assert_eq!(code, 403, "{v}");
    let e = v["error"].as_str().unwrap();
    assert!(
        e.contains("isn't local") && e.contains("clients.profiles"),
        "says why and names the setting: {v}"
    );
    assert!(h.ran.lock().unwrap().is_empty());
}

/// (review) With the setting unset, an omitted profile is checked like a
/// named one: a role bound to a hosted profile does not run on it.
#[tokio::test]
async fn with_the_setting_unset_an_omitted_profile_that_is_hosted_is_403() {
    let h = harness();
    let token = paired(&h).await;
    let (code, v) = call(
        &h,
        post("/client/work", Some(&token), work_for("researcher")),
    )
    .await;
    assert_eq!(code, 403, "{v}");
    let e = v["error"].as_str().unwrap();
    assert!(e.contains("hosted") && e.contains("isn't local"), "{v}");
    assert!(h.ran.lock().unwrap().is_empty());
    // Granted by name, it runs.
    h.allow(&["hosted"]);
    let (code, v) = call(
        &h,
        post("/client/work", Some(&token), work_for("researcher")),
    )
    .await;
    assert_eq!(code, 202, "{v}");
}

/// (review) A locality check that cannot answer refuses, also for an
/// omitted profile.
#[tokio::test]
async fn an_unanswerable_locality_check_refuses_an_omitted_profile() {
    let mut h = harness();
    h.state.profile_is_local = Arc::new(|_, _| Err("the registry did not load".to_string()));
    let token = paired(&h).await;
    let (code, v) = call(&h, post("/client/work", Some(&token), work_for("coder"))).await;
    assert_eq!(code, 403, "{v}");
    assert!(v["error"].as_str().unwrap().contains("did not load"), "{v}");
    assert!(h.ran.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_utility_only_profile_is_403() {
    let h = harness();
    let token = paired(&h).await;
    let (code, v) = call(
        &h,
        post(
            "/client/work",
            Some(&token),
            work_body(Some("utility"), None),
        ),
    )
    .await;
    assert_eq!(code, 403, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("utility model"),
        "{v}"
    );
    assert!(h.ran.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_profile_outside_clients_profiles_is_403_and_nothing_runs() {
    let h = harness();
    let token = paired(&h).await;
    h.allow(&["fast"]);
    let (code, v) = call(
        &h,
        post("/client/work", Some(&token), work_body(Some("deep"), None)),
    )
    .await;
    assert_eq!(code, 403, "{v}");
    assert!(v["error"].as_str().unwrap().contains("deep"));
    assert!(h.ran.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_omitted_profile_that_resolves_outside_clients_profiles_is_403() {
    let h = harness();
    let token = paired(&h).await;
    h.allow(&["fast"]); // radio-host is bound to deep
    let (code, v) = call(
        &h,
        post("/client/work", Some(&token), work_for("radio-host")),
    )
    .await;
    assert_eq!(code, 403, "{v}");
    assert!(v["error"].as_str().unwrap().contains("deep"), "{v}");
    assert!(h.ran.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_listed_profile_other_than_the_roles_own_runs() {
    let h = harness();
    let token = paired(&h).await;
    h.allow(&["fast", "deep"]);
    let (code, v) = call(
        &h,
        post("/client/work", Some(&token), work_body(Some("deep"), None)),
    )
    .await;
    assert_eq!(code, 202, "{v}");
    assert_eq!(v["profile"], "deep");
}

#[tokio::test]
async fn an_empty_clients_profiles_allows_nothing() {
    let h = harness();
    let token = paired(&h).await;
    h.allow(&[]);
    let (code, v) = call(&h, post("/client/work", Some(&token), work_for("coder"))).await;
    assert_eq!(code, 403, "{v}");
}

#[tokio::test]
async fn an_unreadable_policy_refuses_work() {
    let h = harness();
    let token = paired(&h).await;
    h.set_policy(Err("config.json does not parse".into()));
    let (code, v) = call(&h, post("/client/work", Some(&token), work_for("coder"))).await;
    assert_eq!(code, 503, "{v}");
    assert!(h.ran.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_profile_that_routes_to_another_machine_is_403() {
    let h = harness();
    let token = paired(&h).await;
    h.allow(&["fast@studio"]);
    let (code, v) = call(
        &h,
        post(
            "/client/work",
            Some(&token),
            work_body(Some("fast@studio"), None),
        ),
    )
    .await;
    assert_eq!(code, 403, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("another machine"),
        "{v}"
    );
    assert!(h.ran.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_profile_this_machine_does_not_define_is_403() {
    let h = harness();
    let token = paired(&h).await;
    h.allow(&["ghost"]);
    let (code, v) = call(
        &h,
        post("/client/work", Some(&token), work_body(Some("ghost"), None)),
    )
    .await;
    assert_eq!(code, 403, "never falls back to the default profile: {v}");
    assert!(h.ran.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_workdir_outside_the_worktrees_base_is_403() {
    let h = harness();
    let token = paired(&h).await;
    let outside = h.dir.path().to_str().unwrap().to_string();
    let (code, v) = call(
        &h,
        post(
            "/client/work",
            Some(&token),
            work_body(Some("fast"), Some(&outside)),
        ),
    )
    .await;
    assert_eq!(code, 403, "{v}");
    let (code, v) = call(
        &h,
        post(
            "/client/work",
            Some(&token),
            work_body(Some("fast"), Some("worktrees/wt1")),
        ),
    )
    .await;
    assert_eq!(code, 403, "a relative workdir is refused: {v}");
    assert!(h.ran.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_workdir_under_the_base_runs_canonicalized() {
    let h = harness();
    let token = paired(&h).await;
    let wt = h.base.join("wt1");
    let (code, v) = call(
        &h,
        post(
            "/client/work",
            Some(&token),
            work_body(Some("fast"), wt.to_str()),
        ),
    )
    .await;
    assert_eq!(code, 202, "{v}");
    let _ = wait_done(&h, &token, v["work_id"].as_str().unwrap()).await;
    assert_eq!(
        h.ran.lock().unwrap()[0].workdir.as_deref(),
        Some(wt.canonicalize().unwrap().as_path())
    );
}

#[tokio::test]
async fn an_unknown_role_is_400() {
    let h = harness();
    let token = paired(&h).await;
    let mut b = work_body(Some("fast"), None);
    b["role"] = "--help".into();
    let (code, v) = call(&h, post("/client/work", Some(&token), b)).await;
    assert_eq!(code, 400, "{v}");
    assert!(h.ran.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_client_runs_at_most_two_items_at_once() {
    let h = harness();
    let token = paired(&h).await;
    let mut slow = work_for("coder");
    slow["message"] = "slow".into();
    for _ in 0..2 {
        let (code, v) = call(&h, post("/client/work", Some(&token), slow.clone())).await;
        assert_eq!(code, 202, "{v}");
    }
    let (code, v) = call(&h, post("/client/work", Some(&token), slow.clone())).await;
    assert_eq!(code, 429, "{v}");
}

#[tokio::test]
async fn a_failed_execution_reports_its_error() {
    let mut h = harness();
    h.state.execute = Arc::new(|_, _| {
        Err(RunError {
            reason: "exit 1".into(),
            error: "boom on stderr".into(),
            code: None,
        })
    });
    let token = paired(&h).await;
    let (_, v) = call(&h, post("/client/work", Some(&token), work_for("coder"))).await;
    let done = wait_done(&h, &token, v["work_id"].as_str().unwrap()).await;
    assert_eq!(
        done,
        serde_json::json!({"status": "error", "error": "boom on stderr", "reason": "exit 1", "code": "dispatch_failed"})
    );
}

#[tokio::test]
async fn work_is_visible_only_to_the_client_that_created_it() {
    let h = harness();
    let a = paired(&h).await;
    let b = paired(&h).await;
    let (_, v) = call(&h, post("/client/work", Some(&a), work_for("coder"))).await;
    let id = v["work_id"].as_str().unwrap().to_string();
    let (code, _) = call(
        &h,
        get(
            &format!("/client/work/{id}"),
            &[("authorization", &bearer(&b))],
        ),
    )
    .await;
    assert_eq!(code, 404);
    let (code, _) = call(&h, get(&format!("/client/work/{id}"), &[])).await;
    assert_eq!(code, 401);
}

// ─── Recipe ──────────────────────────────────────────────────────────────

fn recipe_url(workdir: &Path) -> String {
    let w: String = workdir
        .to_str()
        .unwrap()
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    format!("/client/recipe?workdir={w}")
}

fn write_recipe(wt: &Path, text: &str) {
    std::fs::create_dir_all(wt.join(".darkmux")).unwrap();
    std::fs::write(wt.join(".darkmux/preview.json"), text).unwrap();
}

/// A workspace made from the user's own checkout (`source_path`): the
/// checkout, the workspace's name and its directory.
async fn checkout_workspace(h: &Harness, token: &str) -> (PathBuf, String, PathBuf) {
    let mine = h.dir.path().join("site");
    fixture_repo(&mine);
    // The owner's checkout has the owner's identity, as a real one does.
    run_git(Some(&mine), &["config", "user.name", "Owner"]);
    run_git(Some(&mine), &["config", "user.email", "owner@example.com"]);
    let (code, v) = call(
        h,
        post(
            "/client/workspaces",
            Some(token),
            serde_json::json!({"source_path": mine.to_str(), "bot": "b"}),
        ),
    )
    .await;
    assert_eq!(code, 201, "{v}");
    let name = v["workspace"].as_str().unwrap().to_string();
    let ws = h.base.join(&name);
    (mine, name, ws)
}

const A_RECIPE: &str = r#"{"schema_version": "1.0", "command": "npm run dev", "env": {"PORT": "{port}"}}"#;

/// The recipe comes from the user's own checkout, by workspace name or by
/// the workspace's directory, checked.
#[tokio::test]
async fn a_recipe_is_read_from_the_users_own_checkout() {
    let h = harness();
    let token = paired(&h).await;
    let (mine, name, ws) = checkout_workspace(&h, &token).await;
    write_recipe(&mine, A_RECIPE);
    write_recipe(&ws, r#"{"schema_version": "1.0", "command": "touch pwned"}"#);
    for url in [recipe_url(&ws), format!("/client/recipe?workspace={name}")] {
        let (code, v) = call(&h, get(&url, &[("authorization", &bearer(&token))])).await;
        assert_eq!(code, 200, "{url}: {v}");
        assert_eq!(v["recipe"]["command"], "npm run dev", "the checkout's, never the workspace's: {v}");
        assert_eq!(v["recipe"]["env"]["PORT"], "{port}");
    }
    let (code, _) = call(&h, get("/client/recipe", &[("authorization", &bearer(&token))])).await;
    assert_eq!(code, 400);
}

/// No checkout of the user's (a plain folder, a clone), or none of the
/// recipe in it, is a 404 that says what to do; a workspace's own copy
/// never counts.
#[tokio::test]
async fn no_recipe_is_404_and_says_where_to_put_one() {
    let h = harness();
    let token = paired(&h).await;
    write_recipe(&h.base.join("wt1"), A_RECIPE);
    let (code, v) = call(
        &h,
        get(&recipe_url(&h.base.join("wt1")), &[("authorization", &bearer(&token))]),
    )
    .await;
    assert_eq!(code, 404, "{v}");
    assert_eq!(v["code"], "no_source_checkout", "{v}");
    assert!(v["error"].as_str().unwrap().contains("your own checkout"), "{v}");
    let (_mine, name, ws) = checkout_workspace(&h, &token).await;
    write_recipe(&ws, A_RECIPE);
    let (code, v) = call(
        &h,
        get(&format!("/client/recipe?workspace={name}"), &[("authorization", &bearer(&token))]),
    )
    .await;
    assert_eq!(code, 404, "{v}");
    assert_eq!(v["code"], "no_recipe", "{v}");
    let e = v["error"].as_str().unwrap();
    assert!(
        e.contains("add .darkmux/preview.json to your own checkout") && e.contains("never read"),
        "{v}"
    );
    // A folder below a workspace is not a workspace.
    std::fs::create_dir_all(ws.join("sub")).unwrap();
    let (code, v) = call(&h, get(&recipe_url(&ws.join("sub")), &[("authorization", &bearer(&token))])).await;
    assert_eq!(code, 404, "{v}");
}

#[tokio::test]
async fn a_recipe_outside_the_worktrees_base_is_403() {
    let h = harness();
    let token = paired(&h).await;
    let outside = h.dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    write_recipe(&outside, r#"{"secret": true}"#);
    let (code, v) = call(
        &h,
        get(&recipe_url(&outside), &[("authorization", &bearer(&token))]),
    )
    .await;
    assert_eq!(code, 403, "{v}");
    assert!(v.get("recipe").is_none());
}

#[tokio::test]
async fn a_recipe_needs_a_token() {
    let h = harness();
    let _ = paired(&h).await;
    let wt = h.base.join("wt1");
    write_recipe(&wt, "{}");
    let (code, _) = call(&h, get(&recipe_url(&wt), &[])).await;
    assert_eq!(code, 401);
    let (code, _) = call(
        &h,
        get(
            &recipe_url(&wt),
            &[("authorization", &bearer(&"0".repeat(64)))],
        ),
    )
    .await;
    assert_eq!(code, 401);
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_recipe_is_refused() {
    let h = harness();
    let token = paired(&h).await;
    let (mine, name, _ws) = checkout_workspace(&h, &token).await;
    let outside = h.dir.path().join("secret.json");
    std::fs::write(&outside, A_RECIPE).unwrap();
    std::fs::create_dir_all(mine.join(".darkmux")).unwrap();
    std::os::unix::fs::symlink(&outside, mine.join(".darkmux/preview.json")).unwrap();
    let (code, v) = call(
        &h,
        get(&format!("/client/recipe?workspace={name}"), &[("authorization", &bearer(&token))]),
    )
    .await;
    assert_eq!(code, 403, "{v}");
}

#[tokio::test]
async fn an_oversized_unknown_keyed_or_non_object_recipe_is_refused() {
    let h = harness();
    let token = paired(&h).await;
    let (mine, name, _ws) = checkout_workspace(&h, &token).await;
    let url = format!("/client/recipe?workspace={name}");
    let auth = bearer(&token);
    write_recipe(
        &mine,
        &format!(r#"{{"pad": "{}"}}"#, "x".repeat(MAX_RECIPE_BYTES as usize)),
    );
    let (code, _) = call(&h, get(&url, &[("authorization", &auth)])).await;
    assert_eq!(code, 413);
    write_recipe(&mine, "[1, 2]");
    let (code, _) = call(&h, get(&url, &[("authorization", &auth)])).await;
    assert_eq!(code, 422);
    write_recipe(&mine, r#"{"schema_version": "1.0", "command": "x", "port": 5173}"#);
    let (code, v) = call(&h, get(&url, &[("authorization", &auth)])).await;
    assert_eq!(code, 422, "{v}");
    assert_eq!(v["code"], "recipe_invalid", "{v}");
    assert!(v["error"].as_str().unwrap().contains("unknown key `port`"), "{v}");
}

// ─── The production executor ─────────────────────────────────────────────

#[test]
fn the_dispatch_child_gets_the_existing_dispatch_verb_and_the_message_on_stdin() {
    let spec = WorkSpec {
        work_id: "w-1".into(),
        role: "coder".into(),
        profile: "fast".into(),
        workdir: Some(PathBuf::from("/wt")),
        message: "--not-a-flag".into(),
    };
    let args: Vec<String> = dispatch_args(&spec)
        .into_iter()
        .map(|a| a.into_string().unwrap())
        .collect();
    assert_eq!(
        args,
        [
            "dispatch",
            "coder",
            "--profile",
            "fast",
            "--workdir",
            "/wt",
            "--json",
            "--name",
            "w-1"
        ]
    );
    assert!(
        !args.iter().any(|a| a.contains("not-a-flag")),
        "the message never rides argv"
    );
}

#[cfg(unix)]
#[test]
fn the_child_runner_parses_the_json_envelope_and_reports_failures() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let ok = dir.path().join("ok");
    std::fs::write(&ok, "#!/bin/sh\nmsg=$(cat)\nprintf '{\"result\":\"stop\",\"final_assistant\":\"%s %s\"}' \"$2\" \"$msg\"\n")
        .unwrap();
    std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o700)).unwrap();
    let spec = WorkSpec {
        work_id: "w-1".into(),
        role: "coder".into(),
        profile: "fast".into(),
        workdir: None,
        message: "hi".into(),
    };
    let v = run_dispatch_child(
        &ok,
        &spec,
        &CancelFlag::default(),
        GRACE,
        &FakeTeardown::default(),
    )
    .unwrap();
    assert_eq!(v["final_assistant"], "coder hi");

    let bad = dir.path().join("bad");
    std::fs::write(&bad, "#!/bin/sh\ncat >/dev/null\necho 'boom' >&2\nexit 3\n").unwrap();
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o700)).unwrap();
    let e = run_dispatch_child(
        &bad,
        &spec,
        &CancelFlag::default(),
        GRACE,
        &FakeTeardown::default(),
    )
    .unwrap_err();
    assert_eq!(e.reason, "exit 3");
    assert_eq!(e.error, "boom", "the raw stderr");
    assert_eq!(e.code, None, "no dispatch.error code recorded");

    // (#3468) A child that failed carries the code its `dispatch.error` named.
    let refused = FakeTeardown { code: Some("wont_fit".into()), ..FakeTeardown::default() };
    let e = run_dispatch_child(&bad, &spec, &CancelFlag::default(), GRACE, &refused).unwrap_err();
    assert_eq!(e.code.as_deref(), Some("wont_fit"));

    // (#3505) A refused dispatch prints its refusal envelope on stdout: that
    // is a failed run carrying the envelope's code, never a finished one.
    let no = dir.path().join("no");
    std::fs::write(
        &no,
        "#!/bin/sh
cat >/dev/null
printf '{\"result\":\"refused\",\"code\":\"busy\",\"error\":\"held\"}'\necho 'held' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&no, std::fs::Permissions::from_mode(0o700)).unwrap();
    let e = run_dispatch_child(&no, &spec, &CancelFlag::default(), GRACE, &FakeTeardown::default()).unwrap_err();
    assert_eq!((e.reason.as_str(), e.code.as_deref()), ("exit 1", Some("busy")), "{e:?}");
    assert_eq!(e.error, "held");
}

// ─── Cancel ──────────────────────────────────────────────────────────────

fn waiting_work() -> Value {
    serde_json::json!({"role": "coder", "message": "wait"})
}

async fn cancel(h: &Harness, token: Option<&str>, id: &str) -> (u16, Value) {
    call(
        h,
        post(&format!("/client/work/{id}/cancel"), token, Value::Null),
    )
    .await
}

#[tokio::test]
async fn cancelling_a_running_item_ends_it_as_cancelled() {
    let h = harness();
    let token = paired(&h).await;
    let (_, v) = call(&h, post("/client/work", Some(&token), waiting_work())).await;
    let id = v["work_id"].as_str().unwrap().to_string();
    assert_eq!(
        cancel(&h, Some(&token), &id).await,
        (202, serde_json::json!({"status": "cancelling"}))
    );
    let done = wait_done(&h, &token, &id).await;
    assert_eq!(
        done,
        serde_json::json!({"status": "cancelled", "reason": "cancelled", "error": "stopped"})
    );
}

#[tokio::test]
async fn only_the_client_that_started_a_run_may_cancel_it() {
    let h = harness();
    let a = paired(&h).await;
    let b = paired(&h).await;
    let (_, v) = call(&h, post("/client/work", Some(&a), waiting_work())).await;
    let id = v["work_id"].as_str().unwrap().to_string();
    let (code, v) = cancel(&h, Some(&b), &id).await;
    assert_eq!(code, 404, "another client's id is not found: {v}");
    let (code, _) = cancel(&h, None, &id).await;
    assert_eq!(code, 401);
    let (code, _) = cancel(&h, Some(&"0".repeat(64)), &id).await;
    assert_eq!(code, 401);
    let (code, _) = cancel(&h, Some(&a), "w-nope").await;
    assert_eq!(code, 404);
    // Nothing above stopped it: it is still running until its owner cancels.
    let (_, s) = call(
        &h,
        get(
            &format!("/client/work/{id}"),
            &[("authorization", &bearer(&a))],
        ),
    )
    .await;
    assert_eq!(s["status"], "running");
    assert_eq!(cancel(&h, Some(&a), &id).await.0, 202);
}

/// (#3467) A running work item whose dispatch is starting its model says
/// so, with the elapsed time, as the run lifecycle decides it; once the
/// model is ready it reads plain `running` again (no extra keys).
#[tokio::test]
async fn running_work_says_when_it_is_starting_its_model() {
    let h = harness();
    let t = paired(&h).await;
    let (_, v) = call(&h, post("/client/work", Some(&t), waiting_work())).await;
    let id = v["work_id"].as_str().unwrap().to_string();
    let status = || async { call(&h, get(&format!("/client/work/{id}"), &[("authorization", &bearer(&t))])).await.1 };
    *h.activity.lock().unwrap() = (Some(crate::runs::RunActivity::StartingModel), Some(20_000));
    assert_eq!(status().await, serde_json::json!({"status": "running", "activity": "starting_model", "activity_elapsed_ms": 20_000}));
    *h.activity.lock().unwrap() = (None, None);
    assert_eq!(status().await, serde_json::json!({"status": "running"}));
    assert_eq!(cancel(&h, Some(&t), &id).await.0, 202);
}

/// (#3467) The production reader: the work's dispatch session
/// (`.adhoc.<role>.<work_id>`) read from the flow day files through the run
/// lifecycle, incrementally (a later read sees what was appended). Another
/// session's model start is not this work's.
#[test]
fn the_work_activity_reader_follows_the_dispatchs_records() {
    let dir = tempfile::tempdir().unwrap();
    let day = darkmux_flow::day_utc_now();
    let path = dir.path().join(format!("{day}.jsonl"));
    let append = |recs: &[serde_json::Value]| {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).unwrap();
        for r in recs {
            writeln!(f, "{r}").unwrap();
        }
    };
    let rec = |session: &str, action: &str, payload: serde_json::Value| {
        serde_json::json!({"ts": darkmux_flow::ts_utc_now(), "action": action, "session_id": session,
            "execution_id": format!("exec-{session}"), "payload": payload})
    };
    let model = |ms: u64| serde_json::json!({"engine": "tensorfold", "endpoint": "tf", "model": "TF/m", "elapsed_ms": ms});
    let ours = "run-x.adhoc.coder.w-1";
    let store = crate::flow_window::FlowRecordStore::new(crate::client_pairing::feeds_work_activity);
    let read = || crate::client_pairing::work_activity_in(&store, dir.path(), "coder", "w-1");
    let theirs = "run-y.adhoc.coder.w-2";
    append(&[
        rec(ours, "dispatch.start", serde_json::json!({})),
        rec(theirs, "dispatch.start", serde_json::json!({})),
        rec(theirs, "dispatch.model.start", model(0)),
    ]);
    assert_eq!(read(), (None, None), "another work's start is not this one's");
    append(&[rec(ours, "dispatch.model.start", model(0)), rec(ours, "dispatch.model.loading", model(30_000))]);
    assert_eq!(read(), (Some(crate::runs::RunActivity::StartingModel), Some(30_000)));
    append(&[rec(ours, "dispatch.model.ready", model(31_000))]);
    assert_eq!(read(), (None, None), "ready: running as usual");
    // (#3504) A queued wait reads `queued`, its time the latest wait record's.
    append(&[rec(ours, "dispatch.queue.wait", model(0)), rec(ours, "dispatch.queue.wait", model(20_000))]);
    assert_eq!(read(), (Some(crate::runs::RunActivity::Queued), Some(20_000)));
    append(&[rec(ours, "dispatch.queue.resume", model(21_000))]);
    assert_eq!(read(), (None, None), "resumed: running as usual");

    // (#3467 review) A role id outside `[A-Za-z0-9-]` is escaped on the
    // wire; its work is still found.
    let escaped = darkmux_types::session_id::SessionId::adhoc(
        darkmux_types::session_id::RunId::mission("dispatch-code-1-a-0").unwrap(),
        "code_writer",
        "w-7",
    )
    .wire();
    append(&[rec(&escaped, "dispatch.start", serde_json::json!({})), rec(&escaped, "dispatch.model.start", model(0))]);
    let got = crate::client_pairing::work_activity_in(&store, dir.path(), "code_writer", "w-7");
    assert_eq!(got, (Some(crate::runs::RunActivity::StartingModel), Some(0)));
}

#[tokio::test]
async fn cancelling_a_finished_run_is_409() {
    let h = harness();
    let token = paired(&h).await;
    let (_, v) = call(&h, post("/client/work", Some(&token), work_for("coder"))).await;
    let id = v["work_id"].as_str().unwrap().to_string();
    let _ = wait_done(&h, &token, &id).await;
    assert_eq!(
        cancel(&h, Some(&token), &id).await,
        (409, serde_json::json!({"error": "not running", "code": "conflict"}))
    );
}

/// What the production teardown would have done, recorded.
#[derive(Default)]
struct FakeTeardown {
    start: Option<StartedRun>,
    terminal_exists: bool,
    /// The `code` the run's `dispatch.error` named, as the flow would say.
    code: Option<String>,
    stopped_for: Mutex<Vec<u32>>,
    emitted: Mutex<Vec<StartedRun>>,
}

impl HardKillTeardown for FakeTeardown {
    fn stop_containers(&self, host_pid: u32) {
        self.stopped_for.lock().unwrap().push(host_pid);
    }
    fn find_start(&self, _spec: &WorkSpec) -> Option<StartedRun> {
        self.start.clone()
    }
    fn has_terminal(&self, _run: &StartedRun) -> bool {
        self.terminal_exists
    }
    fn emit_cancelled(&self, run: &StartedRun) {
        self.emitted.lock().unwrap().push(run.clone());
    }
    fn dispatch_error_code(&self, _spec: &WorkSpec) -> Option<String> {
        self.code.clone()
    }
}

const GRACE: std::time::Duration = std::time::Duration::from_millis(300);

fn started() -> StartedRun {
    StartedRun {
        session: darkmux_types::session_id::SessionId::adhoc(
            darkmux_types::session_id::RunId::mission("dispatch-coder-1-a-0").unwrap(),
            "coder",
            "w-1",
        )
        .wire(),
        execution: "exec-1".into(),
        role: "coder".into(),
        model: Some("darkmux:qwen".into()),
    }
}

#[cfg(unix)]
fn fake_child(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
    p
}

fn spec_w1() -> WorkSpec {
    WorkSpec {
        work_id: "w-1".into(),
        role: "coder".into(),
        profile: "fast".into(),
        workdir: None,
        message: "hi".into(),
    }
}

/// Request a cancel once the fake child says it is ready (its signal
/// disposition set), so TERM never races its `trap`.
fn cancel_when_ready(child: &Path) -> Arc<CancelFlag> {
    let flag = Arc::new(CancelFlag::default());
    let (f, ready) = (
        flag.clone(),
        PathBuf::from(format!("{}.ready", child.display())),
    );
    std::thread::spawn(move || {
        for _ in 0..500 {
            if ready.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        f.request();
    });
    flag
}

#[cfg(unix)]
#[test]
fn a_child_that_exits_on_term_is_left_to_dispatchs_own_teardown() {
    let dir = tempfile::tempdir().unwrap();
    let child = fake_child(
        dir.path(),
        "polite",
        "cat >/dev/null\n: > \"$0.ready\"\nexec sleep 30",
    );
    let teardown = FakeTeardown {
        start: Some(started()),
        ..Default::default()
    };
    let t0 = std::time::Instant::now();
    let r = run_dispatch_child(
        &child,
        &spec_w1(),
        &cancel_when_ready(&child),
        std::time::Duration::from_secs(20),
        &teardown,
    );
    assert!(r.is_err());
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(5),
        "SIGTERM ended it, no wait for the grace"
    );
    assert!(
        teardown.stopped_for.lock().unwrap().is_empty(),
        "dispatch kills its own container on TERM"
    );
    assert!(
        teardown.emitted.lock().unwrap().is_empty(),
        "and writes its own terminal"
    );
}

#[cfg(unix)]
#[test]
fn a_child_that_ignores_term_is_killed_after_the_grace_and_torn_down() {
    let dir = tempfile::tempdir().unwrap();
    let child = fake_child(
        dir.path(),
        "stubborn",
        "trap '' TERM\ncat >/dev/null\n: > \"$0.ready\"\nexec sleep 30",
    );
    let teardown = FakeTeardown {
        start: Some(started()),
        ..Default::default()
    };
    let t0 = std::time::Instant::now();
    let r = run_dispatch_child(
        &child,
        &spec_w1(),
        &cancel_when_ready(&child),
        GRACE,
        &teardown,
    );
    let took = t0.elapsed();
    assert!(r.is_err());
    assert!(took >= GRACE, "TERM first, then the grace: {took:?}");
    assert!(
        took < std::time::Duration::from_secs(10),
        "then KILL, not the child's own 30 s: {took:?}"
    );
    let stopped = teardown.stopped_for.lock().unwrap().clone();
    assert_eq!(
        stopped.len(),
        1,
        "the dead child's containers are stopped once"
    );
    assert!(stopped[0] > 0);
    assert_eq!(
        teardown.emitted.lock().unwrap().as_slice(),
        &[started()],
        "one terminal, for its execution"
    );
}

#[test]
fn after_a_hard_kill_the_terminal_is_written_only_when_none_exists() {
    let already = FakeTeardown {
        start: Some(started()),
        terminal_exists: true,
        ..Default::default()
    };
    after_hard_kill(&already, &spec_w1(), 42);
    assert!(
        already.emitted.lock().unwrap().is_empty(),
        "the child wrote its terminal before the KILL"
    );
    assert_eq!(already.stopped_for.lock().unwrap().as_slice(), &[42]);

    let never_started = FakeTeardown::default();
    after_hard_kill(&never_started, &spec_w1(), 42);
    assert!(
        never_started.emitted.lock().unwrap().is_empty(),
        "no dispatch.start, no execution to end"
    );
    assert_eq!(
        never_started.stopped_for.lock().unwrap().as_slice(),
        &[42],
        "containers are stopped regardless"
    );
}

/// The cancelled terminal goes to the sink; a sink that fails, or a run
/// whose ids cannot be read, says why instead of passing silently.
#[test]
fn the_cancelled_terminal_is_written_or_the_failure_named() {
    let mut got = None;
    write_cancelled(&started(), |rec| {
        got = Some(rec);
        Ok(())
    })
    .unwrap();
    assert_eq!(got.map(|r| r.action), Some(darkmux_flow::FlowAction::DispatchError));
    let err = write_cancelled(&started(), |_| Err(anyhow!("disk full"))).unwrap_err();
    assert!(err.contains("exec-1") && err.contains("disk full"), "{err}");
    let mut bad = started();
    bad.session = "not a session".into();
    assert!(write_cancelled(&bad, |_| panic!("never written")).is_err());
}

#[test]
fn the_cancelled_terminal_is_a_dispatch_error_naming_the_stop() {
    let rec = cancelled_record(&started()).unwrap();
    assert_eq!(rec.action, darkmux_flow::FlowAction::DispatchError);
    assert_eq!(
        rec.execution_id.as_ref().map(|e| e.as_str()),
        Some("exec-1")
    );
    let v = serde_json::to_value(&rec).unwrap();
    assert_eq!(v["payload"]["stop_reason"], "cancelled", "{v}");
}

#[test]
fn the_start_is_found_by_its_work_id_in_the_flow_day_files() {
    let dir = tempfile::tempdir().unwrap();
    let line = |action: &str, session: &str, exec: &str| {
        serde_json::json!({"action": action, "session_id": session, "execution_id": exec, "model": "darkmux:q"}).to_string()
    };
    let text = [
        line(
            "dispatch.start",
            "dispatch-coder-1-a-0.adhoc.coder.w-other",
            "exec-a",
        ),
        line(
            "dispatch.start",
            "dispatch-coder-1-b-0.adhoc.coder.w-1",
            "exec-b",
        ),
        line(
            "dispatch.error",
            "dispatch-coder-1-c-0.adhoc.coder.w-9",
            "exec-c",
        ),
    ]
    .join("\n");
    std::fs::write(dir.path().join("2026-11-07.jsonl"), text).unwrap();
    let run = find_start_in(dir.path(), &spec_w1()).unwrap();
    assert_eq!(
        (run.execution.as_str(), run.session.as_str()),
        ("exec-b", "dispatch-coder-1-b-0.adhoc.coder.w-1")
    );
    assert!(terminal_in(dir.path(), "exec-c"));
    assert!(
        !terminal_in(dir.path(), "exec-b"),
        "a start is not a terminal"
    );

    // (#3467 review) A role id outside `[A-Za-z0-9-]` is escaped in the
    // session id (`code_writer` is `code_5Fwriter` on the wire): found all
    // the same.
    let escaped = darkmux_types::session_id::SessionId::adhoc(
        darkmux_types::session_id::RunId::mission("dispatch-code-1-a-0").unwrap(),
        "code_writer",
        "w-1",
    )
    .wire();
    assert!(!escaped.contains("code_writer"), "the wire escapes it: {escaped}");
    std::fs::write(dir.path().join("2026-11-08.jsonl"), line("dispatch.start", &escaped, "exec-u")).unwrap();
    let spec = WorkSpec { role: "code_writer".into(), ..spec_w1() };
    assert_eq!(find_start_in(dir.path(), &spec).map(|r| r.execution).as_deref(), Some("exec-u"));
}

#[test]
fn the_container_stop_judges_only_the_killed_childs_containers_gone() {
    use darkmux_crew::orphan_reaper::{orphaned_dispatch_containers, ListedContainer, LocalHost};
    let local = LocalHost {
        machine: "m".into(),
        pid_ns: "ns".into(),
    };
    let c = |name: &str, pid: u32| ListedContainer {
        name: name.into(),
        host_pid: Some(pid),
        host_start: Some(7),
        host_machine: Some("m".into()),
        host_pid_ns: Some("ns".into()),
    };
    let listed = [
        c("darkmux-dispatch-ours", 42),
        c("darkmux-dispatch-another-run", 43),
    ];
    let orphans = orphaned_dispatch_containers(&listed, Some(&local), only_this_host_is_gone(42));
    let names: Vec<&str> = orphans.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(
        names,
        ["darkmux-dispatch-ours"],
        "another run's container is never stopped"
    );
}

// ─── Changes and reasons ─────────────────────────────────────────────────

fn git_repo(dir: &Path) {
    let ok = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git init");
}

#[test]
fn changes_exclude_what_the_work_copy_held_before_the_run() {
    let dir = tempfile::tempdir().unwrap();
    git_repo(dir.path());
    std::fs::write(dir.path().join("pre.txt"), "already here\n").unwrap();
    std::fs::write(dir.path().join("untouched.txt"), "left alone\n").unwrap();
    let before = snapshot_tree(&wc(dir.path())).expect("a git work copy snapshots");
    std::fs::write(
        dir.path().join("pre.txt"),
        "already here\nadded by the run\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("new.txt"), "a\nb\n").unwrap();
    let c = serde_json::to_value(changes_since(&wc(dir.path()), &before.tree).unwrap()).unwrap();
    assert_eq!(
        c["files"],
        serde_json::json!([{"path": "new.txt", "added": 2, "removed": 0}, {"path": "pre.txt", "added": 1, "removed": 0}])
    );
    let diff = c["diff"].as_str().unwrap();
    assert!(diff.contains("+added by the run"), "{diff}");
    assert!(
        !diff.contains("+already here"),
        "pre-existing uncommitted content is not the run's: {diff}"
    );
    assert!(!diff.contains("untouched"), "{diff}");
    assert!(
        !dir.path().join(".git/index").exists(),
        "the work copy's own index is never written"
    );
}

#[test]
fn an_unchanged_work_copy_reports_no_changes() {
    let dir = tempfile::tempdir().unwrap();
    git_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
    let before = snapshot_tree(&wc(dir.path())).unwrap();
    assert_eq!(
        serde_json::to_value(changes_since(&wc(dir.path()), &before.tree).unwrap()).unwrap(),
        serde_json::json!({"files": [], "diff": "", "skipped": []})
    );
}

#[test]
fn a_large_diff_is_capped_with_a_marker() {
    let dir = tempfile::tempdir().unwrap();
    git_repo(dir.path());
    let before = snapshot_tree(&wc(dir.path())).unwrap();
    let big: String = (0..5000)
        .map(|i| format!("line number {i} of a long file\n"))
        .collect();
    std::fs::write(dir.path().join("big.txt"), &big).unwrap();
    let c = serde_json::to_value(changes_since(&wc(dir.path()), &before.tree).unwrap()).unwrap();
    let diff = c["diff"].as_str().unwrap();
    assert!(
        diff.len() <= MAX_DIFF_BYTES + DIFF_TRUNCATED_MARKER.len(),
        "{}",
        diff.len()
    );
    assert!(diff.ends_with(DIFF_TRUNCATED_MARKER));
    assert_eq!(
        c["files"],
        serde_json::json!([{"path": "big.txt", "added": 5000, "removed": 0}]),
        "the file list is not cut"
    );
}

/// (#3457) A run that turns the work copy's submodule into a repository of
/// its own (and adds an untracked nested one), each carrying an fsmonitor
/// and a clean filter, and sets `ignore = none` for the submodule in
/// `.gitmodules`: the snapshot and the diff of what changed run none of it.
#[test]
fn snapshot_runs_nothing_a_planted_submodule_or_nested_repo_names() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let marker = root.join("host-ran-a-nested-repos-config");
    let sub_src = root.join("subsrc");
    std::fs::create_dir_all(&sub_src).unwrap();
    run_git(Some(&sub_src), &["init", "-q"]);
    std::fs::write(sub_src.join("s.txt"), "s\n").unwrap();
    run_git(Some(&sub_src), &["add", "s.txt"]);
    run_git(Some(&sub_src), &["commit", "-qm", "s"]);
    let (w, g) = (root.join("w"), root.join(".w.git"));
    run_git(None, &["init", "-q", "--separate-git-dir", g.to_str().unwrap(), w.to_str().unwrap()]);
    run_git(Some(&w), &["-c", "protocol.file.allow=always", "submodule", "add", "-q", sub_src.to_str().unwrap(), "sub"]);
    run_git(Some(&w), &["commit", "-qm", "with sub"]);
    let wd = WorkCopy { work_tree: w.clone(), git_dir: g.canonicalize().unwrap() };
    let before = snapshot_tree(&wd).expect("snapshots");

    let plant = |repo: &Path| {
        let _ = std::fs::remove_dir_all(repo);
        run_git(None, &["clone", "-q", sub_src.to_str().unwrap(), repo.to_str().unwrap()]);
        let cfg = repo.join(".git").join("config");
        let mut config = std::fs::read_to_string(&cfg).unwrap();
        config.push_str(&format!(
            "[core]\n\tfsmonitor = \"touch '{m}'; false\"\n[filter \"x\"]\n\tclean = \"touch '{m}'; cat\"\n",
            m = marker.display()
        ));
        std::fs::write(&cfg, config).unwrap();
        std::fs::write(repo.join(".gitattributes"), "* filter=x\n").unwrap();
    };
    plant(&w.join("sub"));
    plant(&w.join("nested"));
    let mut gitmodules = std::fs::read_to_string(w.join(".gitmodules")).unwrap();
    gitmodules.push_str("\tignore = none\n");
    std::fs::write(w.join(".gitmodules"), gitmodules).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    for repo in ["sub", "nested"] {
        std::fs::write(w.join(repo).join("s.txt"), "t\n").unwrap();
    }

    // The probe arms: git that checks the submodule's work tree runs it.
    let _ = std::process::Command::new("git")
        .arg(format!("--git-dir={}", g.display()))
        .arg(format!("--work-tree={}", w.display()))
        .args(["status", "--porcelain"])
        .current_dir(&w)
        .output()
        .unwrap();
    assert!(marker.exists(), "the fixture must arm: plain status runs the planted submodule's config");
    std::fs::remove_file(&marker).unwrap();

    let changes = changes_since(&wd, &before.tree).expect("the changes read");
    assert!(!marker.exists(), "host git ran a planted repository's config");
    assert!(changes.skipped.contains(&"nested".to_string()), "{:?}", changes.skipped);
}

/// (#3457) A work copy whose held repository's config defines a filter
/// driver (git-crypt, `git lfs install --local`, or one a run wrote) is
/// refused for a snapshot and a commit, naming the driver and the fix, never
/// read or committed in its unfiltered form.
#[test]
fn a_work_copy_whose_repo_config_defines_a_filter_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    git_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
    run_git(Some(dir.path()), &["config", "filter.crypt.clean", "cat"]);
    let err = snapshot_tree(&wc(dir.path())).unwrap_err();
    assert!(err.contains("filter.crypt") && err.contains("--global"), "{err}");
    let err = commit_run(&wc(dir.path()), &["a.txt".to_string()], "m").unwrap_err();
    assert!(err.contains("filter.crypt"), "{err}");
}

/// The operator's global `diff.submodule = diff` makes a diff that shows a
/// changed submodule run `git diff` inside it, in the repository the run
/// planted there: reading a run's changes still runs none of its commands.
#[test]
#[serial_test::serial]
fn reading_changes_runs_nothing_a_planted_submodule_names_under_diff_submodule_diff() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let marker = root.join("host-ran-a-planted-submodule");
    let sub_src = root.join("subsrc");
    std::fs::create_dir_all(&sub_src).unwrap();
    run_git(Some(&sub_src), &["init", "-q"]);
    std::fs::write(sub_src.join("s.txt"), "s\n").unwrap();
    run_git(Some(&sub_src), &["add", "s.txt"]);
    run_git(Some(&sub_src), &["commit", "-qm", "s"]);
    let (w, g) = (root.join("w"), root.join(".w.git"));
    run_git(None, &["init", "-q", "--separate-git-dir", g.to_str().unwrap(), w.to_str().unwrap()]);
    run_git(Some(&w), &["-c", "protocol.file.allow=always", "submodule", "add", "-q", sub_src.to_str().unwrap(), "sub"]);
    run_git(Some(&w), &["commit", "-qm", "with sub"]);
    run_git(Some(&w), &["submodule", "--quiet", "deinit", "-f", "--", "sub"]);
    let wd = WorkCopy { work_tree: w.clone(), git_dir: g.canonicalize().unwrap() };
    let global = root.join("global-config");
    std::fs::write(&global, "[diff]\n\tsubmodule = diff\n").unwrap();
    let prev = std::env::var_os("GIT_CONFIG_GLOBAL");
    // SAFETY: #[serial]; restored below.
    unsafe { std::env::set_var("GIT_CONFIG_GLOBAL", &global) };
    let read = (|| {
        let before = snapshot_tree(&wd)?;
        // The run: its own clone with a commit of its own, carrying filters
        // and a textconv, and a changed file.
        let sub = w.join("sub");
        let _ = std::fs::remove_dir_all(&sub);
        run_git(None, &["clone", "-q", sub_src.to_str().unwrap(), sub.to_str().unwrap()]);
        let cfg = sub.join(".git").join("config");
        let mut config = std::fs::read_to_string(&cfg).unwrap();
        config.push_str(&format!(
            "[core]\n\tfsmonitor = \"touch '{m}'; false\"\n[filter \"x\"]\n\tclean = \"touch '{m}'; cat\"\n\
             \tsmudge = \"touch '{m}'; cat\"\n[diff \"x\"]\n\ttextconv = \"touch '{m}'; cat\"\n",
            m = marker.display()
        ));
        std::fs::write(&cfg, config).unwrap();
        std::fs::write(sub.join(".gitattributes"), "* filter=x diff=x\n").unwrap();
        run_git(Some(&sub), &["-c", "filter.x.clean=cat", "add", ".gitattributes"]);
        run_git(Some(&sub), &["-c", "filter.x.clean=cat", "commit", "-qm", "planted"]);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(sub.join("s.txt"), "t\n").unwrap();
        let _ = std::fs::remove_file(&marker);
        changes_since(&wd, &before.tree)
    })();
    unsafe {
        match prev {
            Some(v) => std::env::set_var("GIT_CONFIG_GLOBAL", v),
            None => std::env::remove_var("GIT_CONFIG_GLOBAL"),
        }
    }
    let changes = read.expect("the changes read");
    assert!(changes.files.iter().any(|f| f.path == "sub"), "the fixture must arm: {:?}", changes.files);
    assert!(!marker.exists(), "reading the changes ran the planted submodule's config");
}

/// A run that fills the work copy's unpopulated submodule with a repository
/// of its own, at the very commit the index records, so git sees no new
/// commit and checks its files instead, which spawns git inside it. The
/// changes read report the submodule; committing them runs nothing the
/// planted repository names, and its gitlink stays out of the commit while
/// the run's ordinary file goes in.
#[test]
fn committing_a_run_runs_nothing_a_planted_submodule_names() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let marker = root.join("host-ran-a-planted-submodule");
    let sub_src = root.join("subsrc");
    std::fs::create_dir_all(&sub_src).unwrap();
    run_git(Some(&sub_src), &["init", "-q"]);
    std::fs::write(sub_src.join("s.txt"), "s\n").unwrap();
    run_git(Some(&sub_src), &["add", "s.txt"]);
    run_git(Some(&sub_src), &["commit", "-qm", "s"]);
    let (w, g) = (root.join("w"), root.join(".w.git"));
    run_git(None, &["init", "-q", "--separate-git-dir", g.to_str().unwrap(), w.to_str().unwrap()]);
    std::fs::write(w.join("a.txt"), "a\n").unwrap();
    run_git(Some(&w), &["add", "a.txt"]);
    run_git(Some(&w), &["-c", "protocol.file.allow=always", "submodule", "add", "-q", sub_src.to_str().unwrap(), "sub"]);
    run_git(Some(&w), &["commit", "-qm", "with sub"]);
    // Unpopulated, as in a fresh worktree: the gitlink is in the index, the
    // folder is empty.
    run_git(Some(&w), &["submodule", "--quiet", "deinit", "-f", "--", "sub"]);
    run_git(Some(&w), &["config", "user.name", "t"]);
    run_git(Some(&w), &["config", "user.email", "t@t"]);
    let wd = WorkCopy { work_tree: w.clone(), git_dir: g.canonicalize().unwrap() };
    let before = snapshot_tree(&wd).expect("snapshots");

    // The run: its own clone at the recorded commit, carrying an fsmonitor
    // and a clean filter, a changed file inside it, `ignore = none`.
    let sub = w.join("sub");
    let _ = std::fs::remove_dir_all(&sub);
    run_git(None, &["clone", "-q", sub_src.to_str().unwrap(), sub.to_str().unwrap()]);
    let cfg = sub.join(".git").join("config");
    let mut config = std::fs::read_to_string(&cfg).unwrap();
    config.push_str(&format!(
        "[core]\n\tfsmonitor = \"touch '{m}'; false\"\n[filter \"x\"]\n\tclean = \"touch '{m}'; cat\"\n",
        m = marker.display()
    ));
    std::fs::write(&cfg, config).unwrap();
    std::fs::write(sub.join(".gitattributes"), "* filter=x\n").unwrap();
    let mut gitmodules = std::fs::read_to_string(w.join(".gitmodules")).unwrap();
    gitmodules.push_str("\tignore = none\n");
    std::fs::write(w.join(".gitmodules"), gitmodules).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(sub.join("s.txt"), "t\n").unwrap();
    std::fs::write(w.join("a.txt"), "changed by the run\n").unwrap();

    let changes = changes_since(&wd, &before.tree).expect("the changes read");
    assert!(!marker.exists(), "reading the changes ran the planted config");
    let paths: Vec<String> = changes.files.iter().map(|f| f.path.clone()).collect();
    assert!(paths.contains(&"sub".to_string()), "the fixture must arm: the submodule is a change: {paths:?}");

    let (sha, _) = commit_run(&wd, &paths, "coder: t (work w-1)").expect("the run's files commit");
    assert!(!marker.exists(), "committing the run ran the planted submodule's config");
    let committed = run_git(None, &[&format!("--git-dir={}", g.display()), "show", "--name-only", "--format=", &sha]);
    assert!(committed.lines().any(|l| l == "a.txt"), "the run's file is committed: {committed}");
    assert!(!committed.lines().any(|l| l == "sub"), "a gitlink is never committed: {committed}");
}

/// Each way a submodule reaches a run's paths stays out of its commit: a
/// gitlink the index records (here one whose folder the run deleted), a
/// repository the run made (no gitlink yet), and a file inside a populated
/// submodule. The run's ordinary file still commits.
#[test]
fn a_runs_submodules_stay_out_of_its_commit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let sub_src = root.join("subsrc");
    std::fs::create_dir_all(&sub_src).unwrap();
    run_git(Some(&sub_src), &["init", "-q"]);
    std::fs::write(sub_src.join("s.txt"), "s\n").unwrap();
    run_git(Some(&sub_src), &["add", "s.txt"]);
    run_git(Some(&sub_src), &["commit", "-qm", "s"]);
    let w = root.join("w");
    std::fs::create_dir_all(&w).unwrap();
    git_repo(&w);
    run_git(Some(&w), &["config", "user.name", "t"]);
    run_git(Some(&w), &["config", "user.email", "t@t"]);
    std::fs::write(w.join("a.txt"), "a\n").unwrap();
    run_git(Some(&w), &["add", "a.txt"]);
    for s in ["gone", "kept"] {
        run_git(Some(&w), &["-c", "protocol.file.allow=always", "submodule", "add", "-q", sub_src.to_str().unwrap(), s]);
    }
    run_git(Some(&w), &["commit", "-qm", "with subs"]);
    std::fs::remove_dir_all(w.join("gone")).unwrap();
    run_git(None, &["clone", "-q", sub_src.to_str().unwrap(), w.join("made").to_str().unwrap()]);
    std::fs::write(w.join("kept").join("s.txt"), "t\n").unwrap();
    std::fs::write(w.join("a.txt"), "changed by the run\n").unwrap();

    let paths: Vec<String> = ["a.txt", "gone", "made", "kept/s.txt"].iter().map(|s| s.to_string()).collect();
    let (sha, _) = commit_run(&wc(&w), &paths, "m").expect("the run's file commits");
    let committed = run_git(Some(&w), &["show", "--name-only", "--format=", &sha]);
    assert_eq!(committed, "a.txt", "only the run's ordinary file is committed");
}

/// A run whose only change is a submodule commits nothing, and says why,
/// rather than handing git an empty path list (which would commit whatever
/// else is staged).
#[test]
fn a_run_that_changed_only_a_submodule_commits_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let sub_src = root.join("subsrc");
    std::fs::create_dir_all(&sub_src).unwrap();
    run_git(Some(&sub_src), &["init", "-q"]);
    std::fs::write(sub_src.join("s.txt"), "s\n").unwrap();
    run_git(Some(&sub_src), &["add", "s.txt"]);
    run_git(Some(&sub_src), &["commit", "-qm", "s"]);
    let w = root.join("w");
    std::fs::create_dir_all(&w).unwrap();
    git_repo(&w);
    run_git(Some(&w), &["-c", "protocol.file.allow=always", "submodule", "add", "-q", sub_src.to_str().unwrap(), "sub"]);
    run_git(Some(&w), &["commit", "-qm", "with sub"]);
    run_git(Some(&w), &["config", "user.name", "t"]);
    run_git(Some(&w), &["config", "user.email", "t@t"]);
    std::fs::write(w.join("staged.txt"), "the user's, staged\n").unwrap();
    run_git(Some(&w), &["add", "staged.txt"]);
    let head = run_git(Some(&w), &["rev-parse", "HEAD"]);
    let err = commit_run(&wc(&w), &["sub".to_string()], "m").unwrap_err();
    assert!(err.contains("submodule"), "{err}");
    assert_eq!(run_git(Some(&w), &["rev-parse", "HEAD"]), head, "nothing was committed");
}

#[test]
fn a_workdir_that_is_not_a_git_repo_has_no_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
    assert!(snapshot_tree(&wc(dir.path())).is_err());
}

#[tokio::test]
async fn a_done_run_reports_its_changes_and_null_outside_git() {
    let h = harness();
    let token = paired(&h).await;
    let wt = h.base.join("wt1");
    let mut body = work_body(None, wt.to_str());
    body["message"] = "edit".into();
    let (_, v) = call(&h, post("/client/work", Some(&token), body.clone())).await;
    let done = wait_done(&h, &token, v["work_id"].as_str().unwrap()).await;
    assert_eq!(done["status"], "done");
    assert_eq!(done["changes"], Value::Null, "not a git repo");

    git_repo(&wt);
    let (_, v) = call(&h, post("/client/work", Some(&token), body.clone())).await;
    let done = wait_done(&h, &token, v["work_id"].as_str().unwrap()).await;
    assert_eq!(done["changes"], Value::Null, "a repository darkmux did not make: host git never reads it");
    hold(&h, "wt1");
    std::fs::remove_file(wt.join("made-by-run.txt")).unwrap();
    std::fs::write(wt.join("mine.txt"), "uncommitted, the user's\n").unwrap();
    let (_, v) = call(&h, post("/client/work", Some(&token), body)).await;
    let done = wait_done(&h, &token, v["work_id"].as_str().unwrap()).await;
    assert_eq!(
        done["changes"]["files"],
        serde_json::json!([{"path": "made-by-run.txt", "added": 2, "removed": 0}])
    );

    let (_, v) = call(&h, post("/client/work", Some(&token), work_for("coder"))).await;
    let done = wait_done(&h, &token, v["work_id"].as_str().unwrap()).await;
    assert_eq!(done["changes"], Value::Null, "no workdir, nothing to diff");
}

/// Once `client serve` begins to stop, a request that could start
/// something (work, a preview, a pairing) is refused with `unavailable`
/// and starts nothing; reads still answer.
#[tokio::test]
async fn once_stopping_a_request_that_could_start_something_is_refused() {
    let h = harness();
    let token = paired(&h).await;
    h.state.previews.begin_shutdown();
    let (code, v) = call(&h, post("/client/work", Some(&token), work_for("coder"))).await;
    assert_eq!((code, v["code"].as_str()), (503, Some("unavailable")), "{v}");
    let (code, v) = call(&h, post("/client/workspaces/wt1/preview", Some(&token), serde_json::json!({}))).await;
    assert_eq!((code, v["code"].as_str()), (503, Some("unavailable")), "{v}");
    assert!(h.ran.lock().unwrap().is_empty(), "no work ran");
    assert!(h.containers.specs.lock().unwrap().is_empty(), "no preview container");
    let (code, _) = call(&h, get("/client/profiles", &[("authorization", &format!("Bearer {token}"))])).await;
    assert_eq!(code, 200, "reads still answer");
}

#[test]
fn exit_reasons_are_short_and_machine_readable() {
    assert_eq!(exit_reason(Some(143), None), "exit 143/SIGTERM");
    assert_eq!(exit_reason(Some(130), None), "exit 130/SIGINT");
    assert_eq!(exit_reason(Some(2), None), "exit 2");
    assert_eq!(exit_reason(None, Some(9)), "signal 9/SIGKILL");
    assert_eq!(exit_reason(None, Some(15)), "signal 15/SIGTERM");
    assert_eq!(
        exit_reason(Some(0), None),
        "no output",
        "a clean exit without an envelope"
    );
}

// ─── Workspaces ──────────────────────────────────────────────────────────

fn run_git(dir: Option<&Path>, args: &[&str]) -> String {
    let mut cmd = std::process::Command::new("git");
    if let Some(d) = dir {
        cmd.arg("-C").arg(d);
    }
    let out = cmd
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Where a planted git hook would leave its mark if the host ran it: beside
/// the worktrees base, outside every work copy.
fn host_ran_marker(work_copy: &Path) -> PathBuf {
    work_copy.parent().unwrap().parent().unwrap().join("host-ran-the-runs-git-config")
}

/// What a hostile run does to its work copy's git: replaces `.git` (a file
/// or a directory) with a fresh repository whose config makes git run a
/// command, by `core.fsmonitor` or by a clean filter `.gitattributes` names.
fn plant_git_config(w: &Path, kind: &str) {
    let marker = host_ran_marker(w);
    let dot_git = w.join(".git");
    if dot_git.is_dir() {
        std::fs::remove_dir_all(&dot_git).unwrap();
    } else {
        let _ = std::fs::remove_file(&dot_git);
    }
    let ok = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .arg(w)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let config = match kind {
        "fsmonitor" => format!("[core]\n\tfsmonitor = \"touch {}; false\"\n", marker.display()),
        "filter" => {
            std::fs::write(w.join(".gitattributes"), "*.txt filter=evil\n").unwrap();
            format!("[filter \"evil\"]\n\tclean = \"touch {}; cat\"\n", marker.display())
        }
        other => panic!("no plant {other}"),
    };
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(dot_git.join("config"))
        .unwrap()
        .write_all(config.as_bytes())
        .unwrap();
}

/// A repo with one commit on `main` and a `dev` branch.
fn fixture_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    run_git(Some(dir), &["init", "--quiet", "-b", "main"]);
    std::fs::write(dir.join("README.md"), "hi\n").unwrap();
    run_git(Some(dir), &["add", "."]);
    run_git(Some(dir), &["commit", "--quiet", "-m", "first"]);
    run_git(Some(dir), &["branch", "dev"]);
}

const HCR: &str = "https://github.com/kstrat2001/hcr";

fn ws_body(v: Value) -> Value {
    v
}

#[tokio::test]
async fn workspaces_need_a_token() {
    let h = harness();
    let (code, _) = call(
        &h,
        post(
            "/client/workspaces",
            None,
            serde_json::json!({"repo_url": HCR}),
        ),
    )
    .await;
    assert_eq!(code, 401);
    let (code, _) = call(&h, get("/client/workspaces/hcr", &[])).await;
    assert_eq!(code, 401);
    assert!(h.cloned.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_repo_url_is_cloned_into_a_named_workspace() {
    let h = harness();
    let token = paired(&h).await;
    let (code, v) = call(
        &h,
        post(
            "/client/workspaces",
            Some(&token),
            ws_body(serde_json::json!({"repo_url": HCR})),
        ),
    )
    .await;
    assert_eq!(code, 201, "{v}");
    assert_eq!(v["workspace"], "hcr");
    assert!(crate::client_workspace::is_bot_branch(v["branch"].as_str().unwrap(), "app"), "{v}");
    assert_eq!(v["repo"], "github.com/kstrat2001/hcr");
    assert!(!v["head"].as_str().unwrap().is_empty());
    assert!(h.base.join("hcr/README.md").exists());
    assert!(!h.base.join(".hcr.partial").exists());
    // Asked again (in its ssh spelling), it's reused and fetched, not cloned.
    let (code, v) = call(
        &h,
        post(
            "/client/workspaces",
            Some(&token),
            serde_json::json!({"repo_url": "git@github.com:kstrat2001/hcr.git"}),
        ),
    )
    .await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["workspace"], "hcr");
    assert_eq!(v["created"], false);
    assert_eq!(h.cloned.lock().unwrap().len(), 1);
    assert_eq!(h.fetched.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_ref_is_cloned_on_that_branch() {
    let h = harness();
    let token = paired(&h).await;
    let (code, v) = call(
        &h,
        post(
            "/client/workspaces",
            Some(&token),
            serde_json::json!({"repo_url": HCR, "ref": "dev"}),
        ),
    )
    .await;
    assert_eq!(code, 201, "{v}");
    assert_eq!(v["from"], "dev", "{v}");
    let branch = v["branch"].as_str().unwrap();
    assert!(crate::client_workspace::is_bot_branch(branch, "app"), "{v}");
    assert_eq!(held_git(&h, "hcr", &["rev-parse", branch]), held_git(&h, "hcr", &["rev-parse", "dev"]), "cut from dev");
}

#[tokio::test]
async fn an_existing_checkout_of_the_repo_is_reused_whatever_its_name() {
    let h = harness();
    let token = paired(&h).await;
    let wt = h.base.join("hcr-preview");
    run_git(
        None,
        &[
            "clone",
            "--quiet",
            h.fixture.to_str().unwrap(),
            wt.to_str().unwrap(),
        ],
    );
    run_git(
        Some(&wt),
        &[
            "remote",
            "set-url",
            "origin",
            "git@github.com:kstrat2001/hcr.git",
        ],
    );
    run_git(Some(&wt), &["checkout", "--quiet", "-b", "mux/app-abc123"]);
    hold(&h, "hcr-preview");
    let (code, v) = call(
        &h,
        post(
            "/client/workspaces",
            Some(&token),
            serde_json::json!({"repo_url": HCR}),
        ),
    )
    .await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["workspace"], "hcr-preview");
    assert!(h.cloned.lock().unwrap().is_empty());
}

/// A held clone on no bot's branch (its default branch, as clones were
/// before they got one) is never handed to a bot: its runs would commit to
/// the default branch.
#[tokio::test]
async fn a_clone_on_no_bots_branch_is_never_handed_out() {
    let h = harness();
    let token = paired(&h).await;
    let wt = h.base.join("hcr");
    run_git(None, &["clone", "--quiet", h.fixture.to_str().unwrap(), wt.to_str().unwrap()]);
    run_git(Some(&wt), &["remote", "set-url", "origin", HCR]);
    hold(&h, "hcr");
    let (code, v) = call(&h, post("/client/workspaces", Some(&token), serde_json::json!({"repo_url": HCR}))).await;
    assert_eq!((code, v["workspace"].as_str()), (201, Some("kstrat2001-hcr")), "{v}");
    assert_eq!(run_git(Some(&wt), &["branch", "--show-current"]), "main", "the old clone is left as it was");
}

#[tokio::test]
async fn a_name_taken_by_another_folder_falls_back_to_owner_repo() {
    let h = harness();
    let token = paired(&h).await;
    std::fs::create_dir_all(h.base.join("hcr")).unwrap();
    let (code, v) = call(
        &h,
        post(
            "/client/workspaces",
            Some(&token),
            serde_json::json!({"repo_url": HCR}),
        ),
    )
    .await;
    assert_eq!(code, 201, "{v}");
    assert_eq!(v["workspace"], "kstrat2001-hcr");
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_checkout_is_never_reused_or_clobbered() {
    let h = harness();
    let token = paired(&h).await;
    // A checkout of the same repo OUTSIDE the base, linked in under the repo's name.
    let outside = h.dir.path().join("outside-hcr");
    run_git(
        None,
        &[
            "clone",
            "--quiet",
            h.fixture.to_str().unwrap(),
            outside.to_str().unwrap(),
        ],
    );
    run_git(Some(&outside), &["remote", "set-url", "origin", HCR]);
    std::os::unix::fs::symlink(&outside, h.base.join("hcr")).unwrap();
    let (code, v) = call(
        &h,
        post(
            "/client/workspaces",
            Some(&token),
            serde_json::json!({"repo_url": HCR}),
        ),
    )
    .await;
    assert_eq!(code, 201, "{v}");
    assert_eq!(
        v["workspace"], "kstrat2001-hcr",
        "the link is neither reused nor overwritten"
    );
    assert!(std::fs::symlink_metadata(h.base.join("hcr"))
        .unwrap()
        .file_type()
        .is_symlink());
    // And it can't be worked in or reported by name.
    let (code, _) = call(
        &h,
        get(
            "/client/workspaces/hcr",
            &[("authorization", &bearer(&token))],
        ),
    )
    .await;
    assert_eq!(code, 403);
    let (code, _) = call(
        &h,
        post(
            "/client/work",
            Some(&token),
            serde_json::json!({"role": "coder", "message": "hello", "workspace": "hcr"}),
        ),
    )
    .await;
    assert_eq!(code, 403);
    assert!(h.ran.lock().unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn a_dangling_link_still_takes_its_name() {
    let h = harness();
    let token = paired(&h).await;
    std::os::unix::fs::symlink(h.dir.path().join("gone"), h.base.join("hcr")).unwrap();
    let (code, v) = call(
        &h,
        post(
            "/client/workspaces",
            Some(&token),
            serde_json::json!({"repo_url": HCR}),
        ),
    )
    .await;
    assert_eq!(code, 201, "{v}");
    assert_eq!(v["workspace"], "kstrat2001-hcr");
}

#[tokio::test]
async fn only_plain_https_or_ssh_repo_urls_and_safe_refs_are_cloned() {
    let h = harness();
    let token = paired(&h).await;
    for body in [
        serde_json::json!({"repo_url": "file:///etc"}),
        serde_json::json!({"repo_url": "ext::sh -c id"}),
        serde_json::json!({"repo_url": "https://github.com/a/b/../../c"}),
        serde_json::json!({"repo_url": "--upload-pack=touch /tmp/x"}),
        serde_json::json!({"repo_url": HCR, "ref": "--upload-pack=x"}),
        serde_json::json!({"repo_url": HCR, "ref": "a..b"}),
        serde_json::json!({}),
        serde_json::json!({"repo_url": HCR, "source_path": "/tmp"}),
        serde_json::json!({"repo_url": HCR, "extra": 1}),
    ] {
        let (code, v) = call(&h, post("/client/workspaces", Some(&token), body.clone())).await;
        assert_eq!(code, 400, "{body} -> {v}");
    }
    assert!(h.cloned.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_failed_clone_leaves_nothing_behind() {
    let h = harness();
    let token = paired(&h).await;
    let (code, v) = call(
        &h,
        post(
            "/client/workspaces",
            Some(&token),
            serde_json::json!({"repo_url": "https://github.com/someone/fail"}),
        ),
    )
    .await;
    assert_eq!(code, 502, "{v}");
    assert_eq!(v["code"], "clone_failed", "{v}");
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("authentication failed"),
        "{v}"
    );
    let mut left: Vec<_> = std::fs::read_dir(&h.base)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .collect();
    left.sort();
    assert_eq!(
        left,
        vec![std::ffi::OsString::from(".wt1.client"), std::ffi::OsString::from("wt1")],
        "only what was there before"
    );
}

#[tokio::test]
async fn workspace_status_reports_branch_head_and_repo() {
    let h = harness();
    let token = paired(&h).await;
    call(
        &h,
        post(
            "/client/workspaces",
            Some(&token),
            serde_json::json!({"repo_url": HCR}),
        ),
    )
    .await;
    let auth = bearer(&token);
    let (code, v) = call(
        &h,
        get("/client/workspaces/hcr", &[("authorization", &auth)]),
    )
    .await;
    assert_eq!(code, 200, "{v}");
    assert!(crate::client_workspace::is_bot_branch(v["branch"].as_str().unwrap(), "app"), "{v}");
    assert_eq!(v["repo"], "github.com/kstrat2001/hcr");
    let (code, _) = call(
        &h,
        get("/client/workspaces/nope", &[("authorization", &auth)]),
    )
    .await;
    assert_eq!(code, 404);
    let (code, _) = call(
        &h,
        get("/client/workspaces/..x", &[("authorization", &auth)]),
    )
    .await;
    assert_eq!(code, 400);
}

#[tokio::test]
async fn work_can_name_a_workspace_instead_of_a_path() {
    let h = harness();
    let token = paired(&h).await;
    call(
        &h,
        post(
            "/client/workspaces",
            Some(&token),
            serde_json::json!({"repo_url": HCR}),
        ),
    )
    .await;
    let (code, v) = call(
        &h,
        post(
            "/client/work",
            Some(&token),
            serde_json::json!({"role": "coder", "message": "hello", "workspace": "hcr"}),
        ),
    )
    .await;
    assert_eq!(code, 202, "{v}");
    let _ = wait_done(&h, &token, v["work_id"].as_str().unwrap()).await;
    assert_eq!(
        h.ran.lock().unwrap()[0].workdir.as_deref(),
        Some(h.base.join("hcr").canonicalize().unwrap().as_path())
    );
}

#[tokio::test]
async fn work_refuses_bad_unknown_or_doubled_workspaces() {
    let h = harness();
    let token = paired(&h).await;
    let wt1 = h.base.join("wt1");
    for (body, want) in [
        (
            serde_json::json!({"role": "coder", "message": "hi", "workspace": "../wt1"}),
            400,
        ),
        (
            serde_json::json!({"role": "coder", "message": "hi", "workspace": ".."}),
            400,
        ),
        (
            serde_json::json!({"role": "coder", "message": "hi", "workspace": "nope"}),
            404,
        ),
        (
            serde_json::json!({"role": "coder", "message": "hi", "workspace": "wt1", "workdir": wt1.to_str()}),
            400,
        ),
    ] {
        let (code, v) = call(&h, post("/client/work", Some(&token), body.clone())).await;
        assert_eq!(code, want, "{body} -> {v}");
    }
    assert!(h.ran.lock().unwrap().is_empty());
}

/// A paired app runs work only in a workspace darkmux made for it. A
/// mission's worktree folder under the base (`<base>/<repo>/<phase>`),
/// another app's workspace, and a folder darkmux holds a repository for but
/// made for no app are refused by name and by path, with a code, before
/// anything runs.
#[tokio::test]
async fn work_runs_only_in_a_workspace_made_for_this_app() {
    let h = harness();
    let token = paired(&h).await;
    std::fs::create_dir_all(h.base.join("darkmux-public/phase-1")).unwrap();
    git_workspace(&h, "theirs");
    own(&h, "theirs", "c-0000000000000000");
    git_workspace(&h, "nobodys");
    // A workspace from before owners, in a store with no legacy owner.
    std::fs::remove_file(h.base.join(".nobodys.client")).unwrap();
    let base = h.base.canonicalize().unwrap();
    for name in ["darkmux-public", "theirs", "nobodys"] {
        for body in [
            serde_json::json!({"role": "coder", "message": "hi", "workspace": name}),
            serde_json::json!({"role": "coder", "message": "hi", "workdir": base.join(name).to_str()}),
        ] {
            let (code, v) = call(&h, post("/client/work", Some(&token), body.clone())).await;
            assert_eq!(code, 403, "{body} -> {v}");
            assert_eq!(v["code"], "forbidden", "{body} -> {v}");
        }
    }
    assert!(h.ran.lock().unwrap().is_empty(), "nothing ran");
    // Nor can it look at or start anything in another app's workspace.
    for req in [
        get("/client/workspaces/theirs", &[("authorization", &bearer(&token))]),
        get("/client/workspaces/theirs/changes", &[("authorization", &bearer(&token))]),
        get("/client/recipe?workspace=theirs", &[("authorization", &bearer(&token))]),
        post("/client/workspaces/theirs/preview", Some(&token), serde_json::json!({})),
        get("/client/workspaces/theirs/preview", &[("authorization", &bearer(&token))]),
        Request::builder()
            .method("DELETE")
            .uri("/client/workspaces/theirs/preview")
            .header("authorization", bearer(&token))
            .body(Body::empty())
            .unwrap(),
    ] {
        let (code, v) = call(&h, req).await;
        assert_eq!((code, v["code"].as_str()), (403, Some("forbidden")), "{v}");
    }
}

/// Two apps asking for the same repo each get a workspace of their own, and
/// neither can run in the other's.
#[tokio::test]
async fn each_app_gets_its_own_workspace_of_a_repo() {
    let h = harness();
    let a = paired(&h).await;
    let b = paired(&h).await;
    let make = |t: &str| post("/client/workspaces", Some(t), serde_json::json!({"repo_url": HCR}));
    let (code, va) = call(&h, make(&a)).await;
    assert_eq!(code, 201, "{va}");
    let (code, vb) = call(&h, make(&b)).await;
    assert_eq!(code, 201, "b gets its own, not a's: {vb}");
    assert_ne!(va["workspace"], vb["workspace"]);
    let (code, again) = call(&h, make(&a)).await;
    assert_eq!((code, &again["workspace"]), (200, &va["workspace"]), "a reuses its own: {again}");
    let (code, v) = call(
        &h,
        post("/client/work", Some(&b), serde_json::json!({"role": "coder", "message": "hi", "workspace": va["workspace"]})),
    )
    .await;
    assert_eq!((code, v["code"].as_str()), (403, Some("forbidden")), "{v}");
    assert!(h.ran.lock().unwrap().is_empty());
}

/// `git <args>` against workspace `name`'s held repository (a clone's
/// `--separate-git-dir`).
fn held_git(h: &Harness, name: &str, args: &[&str]) -> String {
    let gd = crate::client_git::clone_git_dir(&h.base, name);
    let mut all = vec!["--git-dir", gd.to_str().unwrap()];
    all.extend_from_slice(args);
    run_git(None, &all)
}

/// A workspace cloned from a repo link starts on a `mux/<bot>-` branch of
/// its own, cut where the clone checked out, so its runs commit there and
/// the default branch never moves; `/changes` compares that branch with it.
#[tokio::test]
async fn a_cloned_workspaces_runs_commit_to_its_own_mux_branch_never_the_default() {
    let h = harness();
    let token = paired(&h).await;
    let body = serde_json::json!({"repo_url": HCR, "bot": "Bot A"});
    let (code, v) = call(&h, post("/client/workspaces", Some(&token), body)).await;
    assert_eq!(code, 201, "{v}");
    let branch = v["branch"].as_str().unwrap().to_string();
    assert!(crate::client_workspace::is_bot_branch(&branch, "bot-a"), "{v}");
    assert_eq!(v["from"], "origin/main", "{v}");
    let main_before = held_git(&h, "hcr", &["rev-parse", "main"]);
    let id = run_in(&h, &token, "hcr", "edit-readme").await;
    let done = status(&h, &token, &id).await;
    assert_eq!(done["branch"], branch.as_str(), "{done}");
    assert_eq!(held_git(&h, "hcr", &["rev-parse", "main"]), main_before, "the default branch never moves");
    let tip = held_git(&h, "hcr", &["rev-parse", &branch]);
    assert!(tip.starts_with(done["commit"].as_str().unwrap()), "{tip} {done}");
    let (code, c) = call(&h, get("/client/workspaces/hcr/changes", &[("authorization", &bearer(&token))])).await;
    assert_eq!(code, 200, "{c}");
    assert_eq!((c["branch"].as_str(), c["base_branch"].as_str()), (Some(branch.as_str()), Some("main")), "{c}");
    let files: Vec<&str> = c["files"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert_eq!(files, ["README.md"], "{c}");
}

/// Two bots of one app on one repo each get a workspace and a branch of
/// their own. A repo-link request never gets a worktree (the other bot's
/// local folder) nor a bot's whose name only starts with this one's; the
/// same bot asking again gets its own back.
#[tokio::test]
async fn two_bots_of_one_app_get_their_own_workspace_and_branch_of_a_repo() {
    let h = harness();
    let token = paired(&h).await;
    let mine = h.dir.path().join("my-site");
    fixture_repo(&mine);
    run_git(Some(&mine), &["remote", "add", "origin", HCR]);
    let ask = |body: Value| post("/client/workspaces", Some(&token), body);
    let (code, a) = call(&h, ask(serde_json::json!({"source_path": mine.to_str(), "bot": "Bot A"}))).await;
    assert_eq!((code, a["workspace"].as_str()), (201, Some("my-site-bot-a")), "{a}");
    let (code, b) = call(&h, ask(serde_json::json!({"repo_url": HCR, "bot": "Bot B"}))).await;
    assert_eq!(code, 201, "B gets its own, never A's worktree: {b}");
    assert!(crate::client_workspace::is_bot_branch(b["branch"].as_str().unwrap(), "bot-b"), "{b}");
    let (code, a_clone) = call(&h, ask(serde_json::json!({"repo_url": HCR, "bot": "Bot A"}))).await;
    assert_eq!(code, 201, "a repo link never gets a worktree, even its own bot's: {a_clone}");
    assert!(crate::client_workspace::is_bot_branch(a_clone["branch"].as_str().unwrap(), "bot-a"), "{a_clone}");
    let (code, again) = call(&h, ask(serde_json::json!({"repo_url": HCR, "bot": "Bot B"}))).await;
    assert_eq!((code, &again["workspace"], &again["branch"]), (200, &b["workspace"], &b["branch"]), "B reuses its own: {again}");
    // Bot "Bot" is not bot "Bot B", whose branch starts `mux/bot-`.
    let (code, plain) = call(&h, ask(serde_json::json!({"repo_url": HCR, "bot": "Bot"}))).await;
    assert_eq!(code, 201, "{plain}");
    assert!(crate::client_workspace::is_bot_branch(plain["branch"].as_str().unwrap(), "bot"), "{plain}");
    let (code, wt) = call(&h, ask(serde_json::json!({"source_path": mine.to_str(), "bot": "Bot"}))).await;
    assert_eq!(code, 201, "never bot A's worktree: {wt}");
    let names = [&a, &b, &a_clone, &plain, &wt].map(|v| v["workspace"].as_str().unwrap().to_string());
    let distinct: std::collections::HashSet<_> = names.iter().collect();
    assert_eq!(distinct.len(), names.len(), "{names:?}");
}

/// Bots whose names make the same slug ("Bot A" and "bot_a"; names of no
/// Latin letters, and no name at all, which all make `app`; long names that
/// cut to the same 32 characters) each get a workspace and branch of their
/// own, from a repo link or a local checkout; each gets its own back.
#[tokio::test]
async fn bots_whose_names_make_the_same_slug_get_their_own_workspaces() {
    let h = harness();
    let token = paired(&h).await;
    let mine = h.dir.path().join("my-site");
    fixture_repo(&mine);
    let long = |tail: &str| format!("{}{tail}", "x".repeat(32));
    let pairs: [(Option<String>, String); 4] = [
        (Some("Bot A".into()), "bot_a".into()),
        (Some("日本".into()), "中文".into()),
        (None, "русский".into()),
        (Some(long(" one")), long(" two")),
    ];
    for (first, second) in pairs {
        for source in [serde_json::json!({"repo_url": HCR}), serde_json::json!({"source_path": mine.to_str()})] {
            let ask = |bot: Option<&String>| {
                let mut body = source.clone();
                if let Some(b) = bot {
                    body["bot"] = b.as_str().into();
                }
                post("/client/workspaces", Some(&token), body)
            };
            let (code, a) = call(&h, ask(first.as_ref())).await;
            assert_eq!(code, 201, "{first:?} {source}: {a}");
            let (code, b) = call(&h, ask(Some(&second))).await;
            assert_eq!(code, 201, "{second} gets its own, not {first:?}'s: {b}");
            assert_ne!((&a["workspace"], &a["branch"]), (&b["workspace"], &b["branch"]));
            for (bot, made) in [(first.as_ref(), &a), (Some(&second), &b)] {
                let (code, again) = call(&h, ask(bot)).await;
                assert_eq!((code, &again["workspace"]), (200, &made["workspace"]), "{bot:?} gets its own back: {again}");
            }
        }
    }
}

/// A clone left on a branch of the user's (as clones were made before they
/// got a branch of their own: on the default branch, or on the ref asked
/// for, which an app keeps using by name) is moved onto a bot's branch
/// before a run starts in it, taking what it already holds along: that
/// branch never moves again. Until then `/changes` refuses it, since a PR
/// or backup from it would push the bot's commits straight onto it.
#[tokio::test]
async fn a_clone_left_on_a_users_branch_moves_onto_a_bots_branch_before_a_run() {
    for (git_ref, old_branch) in [(None, "main"), (Some("dev"), "dev")] {
        let h = harness();
        let token = paired(&h).await;
        let mut body = serde_json::json!({"repo_url": HCR});
        if let Some(r) = git_ref {
            body["ref"] = r.into();
        }
        let (code, v) = call(&h, post("/client/workspaces", Some(&token), body)).await;
        assert_eq!(code, 201, "{v}");
        // The old shape: a run already committed on the user's branch, which
        // is checked out.
        let first = run_in(&h, &token, "hcr", "edit-readme").await;
        let earlier = held_git(&h, "hcr", &["rev-parse", "HEAD"]);
        held_git(&h, "hcr", &["update-ref", &format!("refs/heads/{old_branch}"), &earlier]);
        held_git(&h, "hcr", &["symbolic-ref", "HEAD", &format!("refs/heads/{old_branch}")]);
        let (code, c) = changes_of(&h, &token, "hcr").await;
        assert_eq!((code, c["code"].as_str()), (409, Some("no_own_branch")), "{old_branch}: {c}");
        assert!(c["error"].as_str().unwrap().contains(old_branch), "{c}");
        let id = run_after(&h, &token, "hcr", "edit-other", Some(&first)).await;
        let done = status(&h, &token, &id).await;
        let branch = done["branch"].as_str().unwrap().to_string();
        assert!(crate::client_workspace::is_bot_branch(&branch, "app"), "{old_branch}: {done}");
        assert_eq!(held_git(&h, "hcr", &["rev-parse", old_branch]), earlier, "{old_branch} never moves again");
        let (code, info) = call(&h, get("/client/workspaces/hcr", &[("authorization", &bearer(&token))])).await;
        assert_eq!((code, info["branch"].as_str()), (200, Some(branch.as_str())), "{info}");
        let (code, c) = changes_of(&h, &token, "hcr").await;
        assert_eq!((code, c["branch"].as_str(), c["base_branch"].as_str()), (200, Some(branch.as_str()), Some("main")), "{c}");
        let mut files: Vec<&str> = c["files"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
        files.sort();
        assert_eq!(files, ["README.md", "other.txt"], "what {old_branch} already held rides along: {c}");
    }
}

async fn changes_of(h: &Harness, token: &str, name: &str) -> (u16, Value) {
    call(h, get(&format!("/client/workspaces/{name}/changes"), &[("authorization", &bearer(token))])).await
}

// ---- workspace locking: one name at a time, never one for all --------------

/// A clone that waits, for any URL naming `slow`, until the test opens its
/// gate: (the state cloning that way, a flag set once it is waiting, the
/// gate).
fn slow_clone_state(h: &Harness) -> (ClientServerState, Arc<std::sync::atomic::AtomicBool>, Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::atomic::AtomicBool;
    let (waiting, open) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (w, o, inner) = (waiting.clone(), open.clone(), h.state.clone_repo.clone());
    let state = ClientServerState {
        clone_repo: Arc::new(move |url, r, dest, gd| {
            if url.contains("slow") {
                w.store(true, Ordering::SeqCst);
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                while !o.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            inner(url, r, dest, gd)
        }),
        ..h.state.clone()
    };
    (state, waiting, open)
}

/// Waits (up to 10 s) for `flag`.
async fn until_set(flag: &std::sync::atomic::AtomicBool) {
    for _ in 0..1000 {
        if flag.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("never set");
}

/// A clone that takes its time (up to ten minutes, for a big repo) holds
/// up nobody else: another bot's workspace, a reuse, a template project and
/// a worktree are all made meanwhile.
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_clone_never_holds_up_another_bots_workspace() {
    let h = harness();
    let token = paired(&h).await;
    let (state, waiting, open) = slow_clone_state(&h);
    let ask = |body: Value| post("/client/workspaces", Some(&token), body);
    let (code, v) = call_with(state.clone(), ask(serde_json::json!({"repo_url": HCR, "bot": "Quick"}))).await;
    assert_eq!(code, 201, "{v}");
    let slow = tokio::spawn(call_with(
        state.clone(),
        ask(serde_json::json!({"repo_url": "https://github.com/kstrat2001/slow", "bot": "Slow"})),
    ));
    until_set(&waiting).await;
    let mine = h.dir.path().join("my-site");
    fixture_repo(&mine);
    for (body, want) in [
        (serde_json::json!({"repo_url": HCR, "bot": "Other"}), 201),
        (serde_json::json!({"repo_url": HCR, "bot": "Quick"}), 200),
        (serde_json::json!({"repo_url": HCR, "template": true, "bot": "Baker"}), 201),
        (serde_json::json!({"source_path": mine.to_str(), "bot": "Quick"}), 201),
    ] {
        let asked = tokio::time::timeout(std::time::Duration::from_secs(10), call_with(state.clone(), ask(body.clone())));
        let (code, v) = asked.await.unwrap_or_else(|_| panic!("{body} waited for another bot's clone"));
        assert_eq!(code, want, "{body}: {v}");
    }
    open.store(true, Ordering::SeqCst);
    let (code, v) = slow.await.unwrap();
    assert_eq!((code, v["workspace"].as_str()), (201, Some("slow")), "{v}");
}

/// One bot asking for one repo twice at once gets one workspace: the second
/// request waits for the first's clone and reuses it, never a second clone.
#[tokio::test(flavor = "multi_thread")]
async fn one_bot_asking_twice_at_once_gets_one_workspace() {
    let h = harness();
    let token = paired(&h).await;
    let (state, waiting, open) = slow_clone_state(&h);
    let body = serde_json::json!({"repo_url": "https://github.com/kstrat2001/slow", "bot": "Slow"});
    let first = tokio::spawn(call_with(state.clone(), post("/client/workspaces", Some(&token), body.clone())));
    until_set(&waiting).await;
    let second = tokio::spawn(call_with(state.clone(), post("/client/workspaces", Some(&token), body)));
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!second.is_finished(), "the second waits for the first's clone");
    open.store(true, Ordering::SeqCst);
    let ((c1, v1), (c2, v2)) = (first.await.unwrap(), second.await.unwrap());
    assert_eq!((c1, c2), (201, 200), "{v1} {v2}");
    assert_eq!(v1["workspace"], v2["workspace"]);
    assert_eq!(h.cloned.lock().unwrap().len(), 1);
}

/// Looking for a bot's workspace to reuse runs no git in a workspace its
/// records give to another bot or another app: there, every git call would
/// hang (its repository's config is a pipe nobody writes).
#[tokio::test(flavor = "multi_thread")]
async fn reuse_runs_no_git_in_another_bots_or_apps_workspace() {
    let h = harness();
    let token = paired(&h).await;
    let ask = |bot: &str| post("/client/workspaces", Some(&token), serde_json::json!({"repo_url": HCR, "bot": bot}));
    let (code, other) = call(&h, ask("Other")).await;
    assert_eq!(code, 201, "{other}");
    let (code, theirs) = call(&h, ask("Theirs")).await;
    assert_eq!(code, 201, "{theirs}");
    // Another app's, for a bot of the very name asking.
    crate::client_workspace::record_owner(&h.base, theirs["workspace"].as_str().unwrap(), "c-someone-else").unwrap();
    crate::client_workspace::record_bot(&h.base, theirs["workspace"].as_str().unwrap(), "Mine").unwrap();
    let pipes: Vec<PathBuf> = [&other, &theirs]
        .iter()
        .map(|v| crate::client_git::clone_git_dir(&h.base, v["workspace"].as_str().unwrap()).join("config"))
        .collect();
    for config in &pipes {
        std::fs::remove_file(config).unwrap();
        let fifo = std::ffi::CString::new(config.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    }
    let made = tokio::time::timeout(std::time::Duration::from_secs(10), call(&h, ask("Mine")));
    let Ok((code, v)) = made.await else {
        // Let a git waiting on a pipe go (it reads an empty config), so the
        // failure is reported now rather than at the runner's bound.
        for _ in 0..50 {
            for config in &pipes {
                use std::os::unix::fs::OpenOptionsExt;
                let _ = std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(config);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("git ran in another bot's or app's workspace");
    };
    assert_eq!(code, 201, "{v}");
}

/// (measurement, not a check) How long reusing a bot's clone takes with 1
/// and with 21 workspaces of the repo under the base, 20 of them other
/// bots'. `cargo nextest run -p darkmux-serve reuse_time --run-ignored only
/// --no-capture`.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn reuse_time_at_1_and_21_workspaces() {
    let h = harness();
    let token = paired(&h).await;
    let ask = |bot: &str| post("/client/workspaces", Some(&token), serde_json::json!({"repo_url": HCR, "bot": bot}));
    assert_eq!(call(&h, ask("Mine")).await.0, 201);
    reuse_timed(&h, &token, "1 workspace").await;
    for n in 0..20 {
        assert_eq!(call(&h, ask(&format!("Other {n}"))).await.0, 201);
    }
    reuse_timed(&h, &token, "21 workspaces").await;
}

/// Prints the median of five reuses of bot `Mine`'s clone of HCR.
async fn reuse_timed(h: &Harness, token: &str, label: &str) {
    let mut took = Vec::new();
    for _ in 0..5 {
        let start = std::time::Instant::now();
        let (code, v) = call(h, post("/client/workspaces", Some(token), serde_json::json!({"repo_url": HCR, "bot": "Mine"}))).await;
        assert_eq!(code, 200, "{v}");
        took.push(start.elapsed().as_millis());
    }
    took.sort();
    eprintln!("reuse at {label}: median {} ms (of {took:?})", took[2]);
}

// ---- removing a workspace ---------------------------------------------------

/// `DELETE /client/workspaces/{name}`, with `body` when given.
async fn remove_ws(h: &Harness, token: &str, name: &str, body: Option<Value>) -> (u16, Value) {
    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/client/workspaces/{name}"))
        .header("authorization", bearer(token))
        .header("content-type", "application/json")
        .body(body.map(|b| Body::from(b.to_string())).unwrap_or_else(Body::empty))
        .unwrap();
    call(h, req).await
}

/// What darkmux keeps under the base for workspace `name`, by file name.
fn kept_for(h: &Harness, name: &str) -> Vec<String> {
    let mut kept: Vec<String> = std::fs::read_dir(&h.base)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|f| f == name || f.starts_with(&format!(".{name}.")))
        .collect();
    kept.sort();
    kept
}

/// An app removes its clone: the folder, its repository and every record
/// beside it go, and asking for the repo again makes a new one under the
/// same name.
#[tokio::test]
async fn an_app_removes_its_clone_and_its_name_is_free_again() {
    let h = harness();
    let token = paired(&h).await;
    let ask = || post("/client/workspaces", Some(&token), serde_json::json!({"repo_url": HCR, "bot": "Bot A"}));
    let (code, v) = call(&h, ask()).await;
    assert_eq!(code, 201, "{v}");
    assert!(!kept_for(&h, "hcr").is_empty());
    let (code, gone) = remove_ws(&h, &token, "hcr", None).await;
    assert_eq!(code, 200, "{gone}");
    assert_eq!((gone["kind"].as_str(), gone["branch"].as_str()), (Some("clone"), v["branch"].as_str()), "{gone}");
    assert_eq!((gone["branch_removed"].as_bool(), gone["preview_stopped"].as_bool()), (Some(true), Some(false)));
    assert_eq!(kept_for(&h, "hcr"), Vec::<String>::new(), "nothing of it is left");
    let (code, _) = call(&h, get("/client/workspaces/hcr", &[("authorization", &bearer(&token))])).await;
    assert_eq!(code, 404);
    let (code, again) = call(&h, ask()).await;
    assert_eq!((code, again["workspace"].as_str()), (201, Some("hcr")), "{again}");
}

/// A bot's template projects take `<bot>`, `<bot>-2` ... `<bot>-99`; once
/// all are taken the next is refused, and removing one frees its name.
#[tokio::test]
async fn a_template_name_is_free_again_once_its_project_is_removed() {
    let h = harness();
    let token = paired(&h).await;
    for n in 2..100 {
        std::fs::create_dir(h.base.join(format!("bakery-{n}"))).unwrap();
    }
    let ask = || post("/client/workspaces", Some(&token), serde_json::json!({"repo_url": HCR, "template": true, "bot": "Bakery"}));
    let (code, v) = call(&h, ask()).await;
    assert_eq!((code, v["workspace"].as_str()), (201, Some("bakery")), "{v}");
    let (code, v) = call(&h, ask()).await;
    assert_eq!((code, v["code"].as_str()), (409, Some("conflict")), "every name is taken: {v}");
    let (code, gone) = remove_ws(&h, &token, "bakery", None).await;
    assert_eq!((code, gone["kind"].as_str()), (200, Some("template")), "{gone}");
    let (code, v) = call(&h, ask()).await;
    assert_eq!((code, v["workspace"].as_str()), (201, Some("bakery")), "{v}");
}

/// A clone or a template project whose runs committed holds commits that
/// are nowhere else: removing it is refused with `unsaved_commits` until
/// the app says `force`.
#[tokio::test]
async fn a_workspace_holding_commits_nowhere_else_is_removed_only_with_force() {
    let h = harness();
    let token = paired(&h).await;
    for body in [serde_json::json!({"repo_url": HCR}), serde_json::json!({"repo_url": HCR, "template": true, "bot": "Bakery"})] {
        let (code, v) = call(&h, post("/client/workspaces", Some(&token), body)).await;
        assert_eq!(code, 201, "{v}");
        let name = v["workspace"].as_str().unwrap().to_string();
        let _ = run_in(&h, &token, &name, "edit").await;
        let (code, refused) = remove_ws(&h, &token, &name, None).await;
        assert_eq!((code, refused["code"].as_str()), (409, Some("unsaved_commits")), "{refused}");
        assert!(h.base.join(&name).join("made-by-run.txt").exists(), "nothing was removed");
        let (code, gone) = remove_ws(&h, &token, &name, Some(serde_json::json!({"force": true}))).await;
        assert_eq!(code, 200, "{gone}");
        assert_eq!(kept_for(&h, &name), Vec::<String>::new());
    }
}

/// Removing a worktree of the user's checkout takes its folder and its
/// admin directory there. Its `mux/` branch goes once another branch holds
/// its commits; one holding a run's commits nothing else holds stays
/// (`force` takes it). The user's own branches and files never change.
#[tokio::test]
async fn removing_a_worktree_never_touches_the_users_branches() {
    let h = harness();
    let token = paired(&h).await;
    let (mine, name, _ws) = checkout_workspace(&h, &token).await;
    let users = || (run_git(Some(&mine), &["for-each-ref", "--format=%(refname) %(objectname)", "refs/heads/main", "refs/heads/dev"]), std::fs::read_to_string(mine.join("README.md")).unwrap());
    let before = users();
    let worktrees = || run_git(Some(&mine), &["worktree", "list", "--porcelain"]).matches("worktree ").count();
    assert_eq!(worktrees(), 2);
    let (code, gone) = remove_ws(&h, &token, &name, None).await;
    assert_eq!((code, gone["kind"].as_str(), gone["branch_removed"].as_bool()), (200, Some("worktree"), Some(true)), "a branch main holds goes: {gone}");
    let branch = gone["branch"].as_str().unwrap().to_string();
    assert!(run_git(Some(&mine), &["branch", "--list", &branch]).is_empty(), "{branch} is gone");
    assert_eq!(worktrees(), 1, "its admin directory is gone");
    assert_eq!(kept_for(&h, &name), Vec::<String>::new());
    for force in [false, true] {
        let (code, v) = call(&h, post("/client/workspaces", Some(&token), serde_json::json!({"source_path": mine.to_str(), "bot": "b"}))).await;
        assert_eq!(code, 201, "{v}");
        let _ = run_in(&h, &token, &name, "edit").await;
        let (code, gone) = remove_ws(&h, &token, &name, Some(serde_json::json!({"force": force}))).await;
        assert_eq!((code, gone["branch_removed"].as_bool()), (200, Some(force)), "{gone}");
        let listed = run_git(Some(&mine), &["branch", "--list", gone["branch"].as_str().unwrap()]);
        assert_eq!(listed.is_empty(), force, "the run's commits stay on their branch unless forced: {listed:?}");
        assert_eq!(worktrees(), 1);
    }
    assert_eq!(users(), before, "the user's branches and files are as they were");
}

/// A workspace is not removed while something uses it: a run in it, or a
/// hold another process took (`client workspace rm` holds it alone; a run
/// in `client serve` shares it). Neither is anything started in one being
/// removed. Only the app that owns it may remove it.
#[tokio::test]
async fn a_workspace_in_use_is_not_removed_nor_used_while_removed() {
    use crate::client_workspace::{try_hold, Hold};
    let h = harness();
    let token = paired(&h).await;
    let other = paired(&h).await;
    let (code, v) = call(&h, post("/client/workspaces", Some(&token), serde_json::json!({"repo_url": HCR}))).await;
    assert_eq!(code, 201, "{v}");
    let edited = run_in(&h, &token, "hcr", "edit").await;
    let id = start_in(&h, &token, "hcr", "wait").await;
    assert!(try_hold(&h.base, "hcr", Hold::Sole).unwrap().is_none(), "a run holds its workspace's name");
    let (code, refused) = remove_ws(&h, &token, "hcr", None).await;
    assert_eq!((code, refused["code"].as_str()), (409, Some("conflict")), "a run is in it: {refused}");
    let _ = cancel(&h, Some(&token), &id).await;
    let _ = wait_done(&h, &token, &id).await;
    assert!(try_hold(&h.base, "hcr", Hold::Sole).unwrap().is_some(), "and lets go once it has ended");
    let elsewhere = try_hold(&h.base, "hcr", Hold::Shared).unwrap().unwrap();
    let (code, refused) = remove_ws(&h, &token, "hcr", None).await;
    assert_eq!((code, refused["code"].as_str()), (409, Some("conflict")), "held elsewhere: {refused}");
    drop(elsewhere);
    let removing = try_hold(&h.base, "hcr", Hold::Sole).unwrap().unwrap();
    let body = serde_json::json!({"role": "coder", "message": "edit", "workspace": "hcr"});
    let (code, refused) = call(&h, post("/client/work", Some(&token), body)).await;
    assert_eq!((code, refused["code"].as_str()), (409, Some("conflict")), "no run starts in one being removed: {refused}");
    let (code, refused) = call(&h, post(&format!("/client/work/{edited}/undo"), Some(&token), Value::Null)).await;
    assert_eq!((code, refused["code"].as_str()), (409, Some("conflict")), "no Undo either: {refused}");
    assert!(refused["error"].as_str().unwrap().contains("being made or removed"), "{refused}");
    drop(removing);
    let (code, _) = remove_ws(&h, &other, "hcr", None).await;
    assert_eq!(code, 403, "another app's");
    let (code, _) = remove_ws(&h, &token, "nope", None).await;
    assert_eq!(code, 404);
    let (code, _) = remove_ws(&h, &token, "..", None).await;
    assert_eq!(code, 400);
    let (code, refused) = remove_ws(&h, &token, "wt1", None).await;
    assert_eq!(code, 403, "a folder darkmux holds no repository for: {refused}");
    assert!(h.base.join("wt1").is_dir());
    let (code, _) = remove_ws(&h, &token, "hcr", Some(serde_json::json!({"forse": true}))).await;
    assert_eq!(code, 400, "an unknown key");
    let (code, gone) = remove_ws(&h, &token, "hcr", Some(serde_json::json!({"force": true}))).await;
    assert_eq!(code, 200, "{gone}");
}

/// A removed workspace's live preview is stopped first, and no preview
/// starts in one being removed.
#[tokio::test]
async fn removing_a_workspace_stops_its_preview_first() {
    use crate::client_workspace::{try_hold, Hold};
    let h = harness();
    let token = paired(&h).await;
    let (mine, name, _ws) = served_workspace(&h, &token).await;
    recipe(&mine, serde_json::json!({"command": serve(), "env": {"PORT": "{port}"}, "ready": {"http": "/"}}));
    let removing = try_hold(&h.base, &name, Hold::Sole).unwrap().unwrap();
    let (code, refused) = start_preview(&h, &token, &name).await;
    assert_eq!((code, refused["code"].as_str()), (409, Some("conflict")), "{refused}");
    drop(removing);
    let (code, v) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 202, "{v}");
    let ready = preview_until(&h, &token, &name, &["ready", "failed"]).await;
    assert_eq!(ready["status"], "ready", "{ready}");
    let port = ready["port"].as_u64().unwrap();
    let (code, gone) = remove_ws(&h, &token, &name, None).await;
    assert_eq!((code, gone["preview_stopped"].as_bool()), (200, Some(true)), "{gone}");
    assert!(!answers(port), "its dev server is gone");
    assert!(!h.base.join(&name).exists());
}

/// A clone a stopped `client serve` left half done (`.<name>.partial` and
/// the repository it was cloning into) is swept as `client serve` starts,
/// so its name is free again; one with a workspace of that name keeps that
/// workspace's repository, and one a request holds now is left.
#[test]
fn a_half_done_clone_is_swept_and_its_name_freed() {
    use crate::client_workspace::{sweep_partials, try_hold, Hold};
    let h = harness();
    let base = &h.base;
    for name in ["gone", "kept", "busy"] {
        std::fs::create_dir_all(base.join(format!(".{name}.partial/half"))).unwrap();
        std::fs::create_dir_all(crate::client_git::clone_git_dir(base, name)).unwrap();
    }
    std::fs::create_dir(base.join("kept")).unwrap();
    let busy = try_hold(base, "busy", Hold::Sole).unwrap().unwrap();
    assert_eq!(sweep_partials(base), ["gone", "kept"]);
    assert!(!name_taken(base, "gone"), "its name is free again");
    assert_eq!(kept_for(&h, "gone"), Vec::<String>::new());
    let kept = kept_for(&h, "kept");
    assert!(kept.contains(&".kept.git".to_string()) && kept.contains(&"kept".to_string()), "the workspace's repository stays: {kept:?}");
    assert!(!kept.contains(&".kept.partial".to_string()), "{kept:?}");
    assert!(base.join(".busy.partial").exists(), "a clone in progress is left");
    drop(busy);
}

/// The fixture's clone, but of an empty repository (no commit, so `HEAD`
/// is unborn).
fn empty_repo_state(h: &Harness) -> ClientServerState {
    let empty = h.dir.path().join("empty.git");
    run_git(None, &["init", "--quiet", "--bare", empty.to_str().unwrap()]);
    ClientServerState {
        clone_repo: Arc::new(move |url, _r, dest, gd| {
            run_git(None, &["clone", "--quiet", "--separate-git-dir", gd.to_str().unwrap(), empty.to_str().unwrap(), dest.to_str().unwrap()]);
            run_git(Some(dest), &["remote", "set-url", "origin", url]);
            run_git(Some(dest), &["config", "user.name", "Owner"]);
            run_git(Some(dest), &["config", "user.email", "owner@example.com"]);
            Ok(())
        }),
        ..h.state.clone()
    }
}

/// An empty repository clones onto a bot's branch too (its `HEAD` is unborn,
/// so the branch is born with the first run's commit), and is reused.
#[tokio::test]
async fn an_empty_repo_clones_onto_a_bots_branch() {
    let h = harness();
    let token = paired(&h).await;
    let state = empty_repo_state(&h);
    let ask = || post("/client/workspaces", Some(&token), serde_json::json!({"repo_url": HCR, "bot": "Bot A"}));
    let (code, v) = call_with(state.clone(), ask()).await;
    assert_eq!(code, 201, "{v}");
    let branch = v["branch"].as_str().unwrap().to_string();
    assert!(crate::client_workspace::is_bot_branch(&branch, "bot-a"), "{v}");
    let (code, again) = call_with(state, ask()).await;
    assert_eq!((code, &again["workspace"]), (200, &v["workspace"]), "{again}");
    let id = run_in(&h, &token, "hcr", "edit").await;
    let done = status(&h, &token, &id).await;
    assert_eq!(done["branch"], branch.as_str(), "{done}");
    assert!(done["commit"].is_string(), "{done}");
}

/// A clone that cannot be put on a bot's branch (its repo has a branch
/// named `mux`, so no `mux/...` branch can exist) fails and leaves nothing
/// behind: no folder, no repository, no records.
#[tokio::test]
async fn a_clone_that_cannot_get_a_bots_branch_leaves_nothing_behind() {
    let h = harness();
    let token = paired(&h).await;
    run_git(Some(&h.fixture), &["branch", "mux"]);
    let (code, v) = call(&h, post("/client/workspaces", Some(&token), serde_json::json!({"repo_url": HCR, "ref": "mux"}))).await;
    assert_eq!((code, v["code"].as_str()), (422, Some("branch_failed")), "{v}");
    let left: Vec<String> = std::fs::read_dir(&h.base)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("hcr"))
        .collect();
    assert!(left.is_empty(), "left behind: {left:?}");
}

/// A run that cannot move its old clone onto a branch of its own (the repo
/// has a local branch named `mux`, so no `mux/...` branch can exist) is
/// refused with its code and forgotten, so the copy is not held busy: once
/// the cause is gone, a run goes ahead there.
#[tokio::test]
async fn a_run_whose_clone_cannot_move_off_a_users_branch_is_refused_and_forgotten() {
    let h = harness();
    let token = paired(&h).await;
    let (code, v) = call(&h, post("/client/workspaces", Some(&token), serde_json::json!({"repo_url": HCR}))).await;
    assert_eq!(code, 201, "{v}");
    let made = v["branch"].as_str().unwrap().to_string();
    held_git(&h, "hcr", &["symbolic-ref", "HEAD", "refs/heads/main"]);
    held_git(&h, "hcr", &["branch", "-D", &made]);
    held_git(&h, "hcr", &["branch", "mux"]);
    let work = || post("/client/work", Some(&token), serde_json::json!({"role": "coder", "message": "edit", "workspace": "hcr"}));
    for attempt in 1..=2 {
        let (code, v) = call(&h, work()).await;
        assert_eq!((code, v["code"].as_str()), (422, Some("branch_failed")), "attempt {attempt}, never busy: {v}");
    }
    assert!(h.ran.lock().unwrap().is_empty());
    held_git(&h, "hcr", &["branch", "-D", "mux"]);
    let (code, v) = call(&h, work()).await;
    assert_eq!(code, 202, "{v}");
    let done = wait_done(&h, &token, v["work_id"].as_str().unwrap()).await;
    assert!(crate::client_workspace::is_bot_branch(done["branch"].as_str().unwrap(), "app"), "{done}");
}

/// A workspace from before bots' exact names were recorded is claimed by
/// the first bot that reuses it: that bot's name is recorded then, so a
/// later bot whose name makes the same slug gets its own, from a local
/// checkout or a repo link.
#[tokio::test]
async fn the_first_bot_to_reuse_an_unrecorded_workspace_keeps_it() {
    let h = harness();
    let token = paired(&h).await;
    let mine = h.dir.path().join("my-site");
    fixture_repo(&mine);
    for source in [serde_json::json!({"source_path": mine.to_str()}), serde_json::json!({"repo_url": HCR})] {
        let ask = |bot: &str| {
            let mut body = source.clone();
            body["bot"] = bot.into();
            post("/client/workspaces", Some(&token), body)
        };
        let (code, a) = call(&h, ask("Bot A")).await;
        assert_eq!(code, 201, "{a}");
        let name = a["workspace"].as_str().unwrap().to_string();
        // As a workspace made before the record existed.
        std::fs::remove_file(h.base.join(format!(".{name}.bot"))).unwrap();
        let (code, again) = call(&h, ask("Bot A")).await;
        assert_eq!((code, again["workspace"].as_str()), (200, Some(name.as_str())), "{again}");
        assert_eq!(crate::client_workspace::bot_of(&h.base, &name).as_deref(), Some("Bot A"));
        let (code, other) = call(&h, ask("bot_a")).await;
        assert_eq!(code, 201, "bot_a gets its own, not Bot A's: {other}");
        assert_ne!(other["workspace"], again["workspace"]);
    }
}

/// A checkout folder of 40 characters and a 32-character bot still make a
/// valid workspace name, its numbered fallbacks included.
#[tokio::test]
async fn a_long_checkout_and_bot_name_still_fit_a_workspace_name() {
    let h = harness();
    let token = paired(&h).await;
    let mine = h.dir.path().join("a".repeat(40));
    fixture_repo(&mine);
    let bot = "b".repeat(32);
    let (code, v) = call(&h, post("/client/workspaces", Some(&token), serde_json::json!({"source_path": mine.to_str(), "bot": bot}))).await;
    assert_eq!(code, 201, "{v}");
    let first = v["workspace"].as_str().unwrap().to_string();
    assert!(crate::client_workspace::valid_name(&first), "{first}");
    let source = mine.canonicalize().unwrap();
    let names = worktree_names(&source, &bot);
    assert_eq!(names[0], first);
    for n in &names {
        assert!(crate::client_workspace::valid_name(n), "{n}");
    }
    let last = names.last().unwrap();
    assert!(last.ends_with("-99"), "every numbered name fits, the last too: {last}");
}

/// A store written before clients had ids, holding one client that
/// presents `token`.
fn store_from_before_ids(h: &Harness, token: &str) {
    let doc = serde_json::json!({
        "schema_version": "1.1",
        "pending": [],
        "clients": [{"name": "mux-bot", "created": 1, "token_hash": hash_hex(token)}],
    });
    std::fs::write(h.store.path(), doc.to_string()).unwrap();
}

/// A workspace from before owners is the client's that was the only one
/// paired when the store moved to ids: decided then, once. It stays that
/// client's after a second app pairs; once that client is revoked nothing
/// is given away.
#[tokio::test]
async fn workspaces_from_before_owners_go_to_the_client_paired_when_the_store_moved() {
    let h = harness();
    let first = "first-token-0123456789abcdef";
    store_from_before_ids(&h, first);
    let heir = h.store.read().unwrap().clients[0].id.clone();
    assert_eq!(h.store.read().unwrap().legacy_owner, Some(heir.clone()));
    let second = paired(&h).await;
    git_workspace(&h, "site");
    std::fs::remove_file(h.base.join(".site.client")).unwrap();
    let (code, v) = call(&h, post("/client/work", Some(&second), serde_json::json!({"role": "coder", "message": "hi", "workspace": "site"}))).await;
    assert_eq!(code, 403, "never the second app's: {v}");
    let id = run_in(&h, first, "site", "hello").await;
    assert_eq!(status(&h, first, &id).await["status"], "done");
    assert_eq!(crate::client_workspace::owner_of(&h.base, "site"), Some(heir));
    assert!(revoke(&h.store, "mux-bot").unwrap());
    git_workspace(&h, "old");
    std::fs::remove_file(h.base.join(".old.client")).unwrap();
    let (code, v) = call(&h, post("/client/work", Some(&second), serde_json::json!({"role": "coder", "message": "hi", "workspace": "old"}))).await;
    assert_eq!(code, 403, "nothing adopts once the heir is revoked: {v}");
    assert_eq!(crate::client_workspace::owner_of(&h.base, "old"), None);
}

/// The reviewer's probe: A and B paired (no store migration, so no legacy
/// owner), A's workspace from before owners. Revoking A leaves B the only
/// client, and B still does not get it.
#[tokio::test]
async fn the_last_client_left_does_not_inherit_unowned_workspaces() {
    let h = harness();
    let a = paired(&h).await;
    let b = paired(&h).await;
    let (code, v) = call(&h, post("/client/workspaces", Some(&a), serde_json::json!({"repo_url": HCR}))).await;
    assert_eq!(code, 201, "{v}");
    std::fs::remove_file(h.base.join(".hcr.client")).unwrap();
    let work = |t: &str| post("/client/work", Some(t), serde_json::json!({"role": "coder", "message": "hi", "workspace": "hcr"}));
    assert_eq!(call(&h, work(&b)).await.0, 403);
    assert!(revoke(&h.store, FIRST_CLIENT).unwrap());
    let (code, v) = call(&h, work(&b)).await;
    assert_eq!(code, 403, "{v}");
    assert_eq!(crate::client_workspace::owner_of(&h.base, "hcr"), None);
}

/// A store moved to ids with two clients paired, or none, has no legacy
/// owner.
#[test]
fn a_store_moved_with_several_clients_or_none_names_no_legacy_owner() {
    let dir = tempfile::tempdir().unwrap();
    for (n, clients) in [(0, "[]"), (2, r#"[{"name":"a","created":1,"token_hash":"00"},{"name":"b","created":1,"token_hash":"00"}]"#)] {
        let path = dir.path().join(format!("clients-{n}.json"));
        std::fs::write(&path, format!(r#"{{"schema_version":"1.1","pending":[],"clients":{clients}}}"#)).unwrap();
        assert_eq!(ClientStore::at(path).read().unwrap().legacy_owner, None, "{n} clients");
    }
}

/// With two apps paired, a workspace from before owners goes to neither,
/// whatever either asks for (its repo URL included); the operator gives it
/// to one with `client claim`, and doctor's list shows it until then.
#[tokio::test]
async fn with_two_apps_paired_only_the_operator_gives_out_an_unowned_workspace() {
    let h = harness();
    let a = paired(&h).await;
    let make = |t: &str| post("/client/workspaces", Some(t), serde_json::json!({"repo_url": HCR}));
    let (code, v) = call(&h, make(&a)).await;
    assert_eq!((code, v["workspace"].as_str()), (201, Some("hcr")), "{v}");
    std::fs::remove_file(h.base.join(".hcr.client")).unwrap();
    let b = paired(&h).await;
    assert_eq!(unowned_workspaces(&h.store, &h.base).unwrap(), ["hcr"]);
    let (code, v) = call(&h, make(&b)).await;
    assert_eq!(code, 201, "b gets its own, never the unowned one: {v}");
    assert_ne!(v["workspace"], "hcr");
    let work = |t: &str| post("/client/work", Some(t), serde_json::json!({"role": "coder", "message": "hi", "workspace": "hcr"}));
    for t in [&a, &b] {
        let (code, v) = call(&h, work(t)).await;
        assert_eq!((code, v["code"].as_str()), (403, Some("forbidden")), "{v}");
    }
    assert!(claim(&h.store, &h.base, "hcr", "nobody").is_err(), "an unknown client");
    std::fs::create_dir_all(h.base.join("darkmux-public/phase-1")).unwrap();
    assert!(claim(&h.store, &h.base, "darkmux-public", FIRST_CLIENT).is_err(), "a mission's worktrees");
    claim(&h.store, &h.base, "hcr", FIRST_CLIENT).unwrap();
    assert!(unowned_workspaces(&h.store, &h.base).unwrap().is_empty());
    let (code, v) = call(&h, work(&a)).await;
    assert_eq!(code, 202, "{v}");
    let (code, _) = call(&h, work(&b)).await;
    assert_eq!(code, 403);
}

/// A client's workspaces and runs belong to its id, not its name: after it
/// is revoked, a new pairing given the same name inherits neither. Its
/// workspaces stay unowned until the operator gives them to someone.
#[tokio::test]
async fn a_new_pairing_never_inherits_a_revoked_clients_workspaces_or_runs() {
    let h = harness();
    let old = paired(&h).await;
    let (code, v) = call(&h, post("/client/workspaces", Some(&old), serde_json::json!({"repo_url": HCR}))).await;
    assert_eq!(code, 201, "{v}");
    let run = run_in(&h, &old, "hcr", "hello").await;
    assert!(revoke(&h.store, FIRST_CLIENT).unwrap());
    let new = paired(&h).await;
    assert_eq!(list_clients(&h.store).unwrap()[0].name, FIRST_CLIENT, "the name is reused");
    let (code, v) = call(&h, post("/client/work", Some(&new), serde_json::json!({"role": "coder", "message": "hi", "workspace": "hcr"}))).await;
    assert_eq!((code, v["code"].as_str()), (403, Some("forbidden")), "{v}");
    let (code, _) = call(&h, get(&format!("/client/work/{run}"), &[("authorization", &bearer(&new))])).await;
    assert_eq!(code, 404, "the revoked client's run is not the new one's");
    let (code, v) = call(&h, post("/client/workspaces", Some(&new), serde_json::json!({"repo_url": HCR}))).await;
    assert_eq!(code, 201, "a workspace of its own: {v}");
    assert_ne!(v["workspace"], "hcr");
    assert_eq!(unowned_workspaces(&h.store, &h.base).unwrap(), ["hcr"]);
}

/// The owner record is written only when it changes, and replaced in one
/// rename: a reader never finds a workspace with no owner while it changes.
#[test]
fn an_owner_record_is_replaced_atomically_and_only_when_it_changes() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().to_path_buf();
    let ws = crate::client_workspace::record_owner;
    ws(&base, "site", "c-a").unwrap();
    let ino = std::fs::metadata(base.join(".site.client")).unwrap().ino();
    ws(&base, "site", "c-a").unwrap();
    assert_eq!(std::fs::metadata(base.join(".site.client")).unwrap().ino(), ino, "no write when it already says so");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (b2, st) = (base.clone(), stop.clone());
    let writer = std::thread::spawn(move || {
        let mut i = 0u64;
        while !st.load(Ordering::SeqCst) {
            ws(&b2, "site", if i.is_multiple_of(2) { "c-b" } else { "c-a" }).unwrap();
            i += 1;
        }
    });
    for _ in 0..20_000 {
        assert!(crate::client_workspace::owner_of(&base, "site").is_some(), "never unowned mid-change");
    }
    stop.store(true, Ordering::SeqCst);
    writer.join().unwrap();
    let names: Vec<String> = std::fs::read_dir(&base).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    assert_eq!(names, [".site.client"], "no temporary file left");
}

// A local checkout becomes a worktree on a new branch; the owner's own
// working tree, branch and uncommitted files are untouched.

#[tokio::test]
async fn a_local_checkout_becomes_a_worktree_on_a_new_branch() {
    let h = harness();
    let token = paired(&h).await;
    let mine = h.dir.path().join("my-site");
    fixture_repo(&mine);
    run_git(Some(&mine), &["remote", "add", "origin", HCR]);
    std::fs::write(mine.join("README.md"), "my uncommitted edit\n").unwrap();
    // The checkout's own hooks never run for an app's request.
    let hooked = h.dir.path().join("hook-ran");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let hook = mine.join(".git/hooks/post-checkout");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", hooked.display())).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let body = serde_json::json!({"source_path": mine.to_str(), "bot": "Human Code Reader"});
    let (code, v) = call(&h, post("/client/workspaces", Some(&token), body.clone())).await;
    assert_eq!(code, 201, "{v}");
    assert_eq!(v["workspace"], "my-site-human-code-reader");
    let branch = v["branch"].as_str().unwrap();
    assert!(branch.starts_with("mux/human-code-reader-"), "{branch}");
    assert_eq!(v["repo"], "github.com/kstrat2001/hcr");
    assert!(!hooked.exists(), "post-checkout hook ran");
    let ws = h.base.join("my-site-human-code-reader");
    assert_eq!(
        std::fs::read_to_string(ws.join("README.md")).unwrap(),
        "hi\n",
        "from the commit, not my edit"
    );
    // My checkout: same branch, my edit still there, and it now knows the branch.
    assert_eq!(run_git(Some(&mine), &["branch", "--show-current"]), "main");
    assert_eq!(
        std::fs::read_to_string(mine.join("README.md")).unwrap(),
        "my uncommitted edit\n"
    );
    assert!(run_git(Some(&mine), &["branch", "--list", branch]).contains(branch));
    // Asked again by the same bot: the same workspace.
    let (code, v) = call(&h, post("/client/workspaces", Some(&token), body)).await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["workspace"], "my-site-human-code-reader");
    // Another bot gets its own.
    let (code, v) = call(
        &h,
        post(
            "/client/workspaces",
            Some(&token),
            serde_json::json!({"source_path": mine.to_str(), "bot": "other"}),
        ),
    )
    .await;
    assert_eq!(code, 201, "{v}");
    assert_eq!(v["workspace"], "my-site-other");
}

#[tokio::test]
async fn a_source_path_must_be_the_top_of_a_real_checkout_outside_the_base() {
    let h = harness();
    let token = paired(&h).await;
    let plain = h.dir.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let repo = h.dir.path().join("repo");
    fixture_repo(&repo);
    std::fs::create_dir_all(repo.join("sub")).unwrap();
    let inside = h.base.join("inside");
    fixture_repo(&inside);
    for (path, why) in [
        ("relative/path".to_string(), "absolute"),
        (plain.to_str().unwrap().to_string(), "not a git repository"),
        (repo.join("sub").to_str().unwrap().to_string(), "top folder"),
        (
            h.dir.path().join("missing").to_str().unwrap().to_string(),
            "does not exist",
        ),
        (
            inside.to_str().unwrap().to_string(),
            "inside the worktrees base",
        ),
    ] {
        let (code, v) = call(
            &h,
            post(
                "/client/workspaces",
                Some(&token),
                serde_json::json!({"source_path": path}),
            ),
        )
        .await;
        assert_eq!(code, 403, "{path} -> {v}");
        assert!(v["error"].as_str().unwrap().contains(why), "{path}: {v}");
    }
    #[cfg(unix)]
    {
        let link = h.dir.path().join("link-to-repo");
        std::os::unix::fs::symlink(&repo, &link).unwrap();
        let (code, v) = call(
            &h,
            post(
                "/client/workspaces",
                Some(&token),
                serde_json::json!({"source_path": link.to_str()}),
            ),
        )
        .await;
        assert_eq!(code, 403, "{v}");
        assert!(v["error"].as_str().unwrap().contains("symlink"), "{v}");
    }
    let names: Vec<_> = std::fs::read_dir(&h.base)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .collect();
    assert_eq!(names.len(), 3, "only wt1, its owner record and inside: {names:?}");
}

#[tokio::test]
async fn a_local_checkout_worktree_starts_from_the_freshly_fetched_default_branch() {
    let h = harness();
    let token = paired(&h).await;
    // The owner's checkout: cloned from "upstream", now on a feature branch
    // with an extra commit and an uncommitted edit.
    let upstream = h.dir.path().join("upstream");
    fixture_repo(&upstream);
    let mine = h.dir.path().join("site");
    run_git(
        None,
        &[
            "clone",
            "--quiet",
            upstream.to_str().unwrap(),
            mine.to_str().unwrap(),
        ],
    );
    run_git(Some(&mine), &["checkout", "--quiet", "-b", "feature"]);
    std::fs::write(mine.join("feature.txt"), "wip\n").unwrap();
    run_git(Some(&mine), &["add", "."]);
    run_git(Some(&mine), &["commit", "--quiet", "-m", "feature work"]);
    std::fs::write(mine.join("README.md"), "uncommitted\n").unwrap();
    // Meanwhile a PR merges upstream.
    std::fs::write(upstream.join("merged.txt"), "from the PR\n").unwrap();
    run_git(Some(&upstream), &["add", "."]);
    run_git(Some(&upstream), &["commit", "--quiet", "-m", "merge #9"]);

    let (code, v) = call(
        &h,
        post(
            "/client/workspaces",
            Some(&token),
            serde_json::json!({"source_path": mine.to_str(), "bot": "hcr"}),
        ),
    )
    .await;
    assert_eq!(code, 201, "{v}");
    let ws = h.base.join(v["workspace"].as_str().unwrap());
    assert!(
        ws.join("merged.txt").exists(),
        "starts from the fetched default branch"
    );
    assert!(
        !ws.join("feature.txt").exists(),
        "not from the checkout's feature branch"
    );
    // The owner's checkout is untouched: branch, files, uncommitted edit.
    assert_eq!(
        run_git(Some(&mine), &["branch", "--show-current"]),
        "feature"
    );
    assert_eq!(
        std::fs::read_to_string(mine.join("README.md")).unwrap(),
        "uncommitted\n"
    );
    assert!(!mine.join("merged.txt").exists());
    assert_eq!(
        h.fetched.lock().unwrap().len(),
        1,
        "the checkout was fetched (refs only)"
    );
}

// ─── Keep and Discard ────────────────────────────────────────────────────

/// Makes the git checkout at `<base>/<name>` a workspace as darkmux makes
/// one: its repository moved outside the work tree, and recorded.
fn hold(h: &Harness, name: &str) {
    let wt = h.base.join(name);
    let held = crate::client_git::clone_git_dir(&h.base, name);
    std::fs::rename(wt.join(".git"), &held).unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", held.display())).unwrap();
    crate::client_git::record_git_dir(&h.base, name, &held).unwrap();
    // Made for the first client paired, when one is.
    if let Some(first) = h.store.read().unwrap().clients.first() {
        own(h, name, &first.id);
    }
}

/// The name the first client a test pairs gets ("Sam's iPhone").
const FIRST_CLIENT: &str = "sam-s-iphone";

/// Records workspace `name` as made for the client whose id is `client`, as
/// darkmux records it.
fn own(h: &Harness, name: &str, client: &str) {
    crate::client_workspace::record_owner(&h.base, name, client).unwrap();
}

/// A git workspace under the base with an uncommitted edit of the owner's
/// own (`notes.txt`), which Keep and Discard must never touch.
fn git_workspace(h: &Harness, name: &str) -> PathBuf {
    let wt = h.base.join(name);
    fixture_repo(&wt);
    hold(h, name);
    run_git(Some(&wt), &["config", "user.name", "Owner"]);
    run_git(Some(&wt), &["config", "user.email", "owner@example.com"]);
    run_git(Some(&wt), &["checkout", "--quiet", "-b", "mux/test-abc123"]);
    std::fs::write(wt.join("notes.txt"), "mine, uncommitted\n").unwrap();
    wt
}

async fn run_in(h: &Harness, token: &str, workspace: &str, message: &str) -> String {
    run_after(h, token, workspace, message, None).await
}

/// A run that continues `continues` (a Tweak) when given.
async fn run_after(
    h: &Harness,
    token: &str,
    workspace: &str,
    message: &str,
    continues: Option<&str>,
) -> String {
    let mut body = serde_json::json!({"role": "coder", "message": message, "workspace": workspace});
    if let Some(c) = continues {
        body["continues"] = c.into();
    }
    let (code, v) = call(h, post("/client/work", Some(token), body)).await;
    assert_eq!(code, 202, "{v}");
    let id = v["work_id"].as_str().unwrap().to_string();
    let _ = wait_done(h, token, &id).await;
    id
}

async fn status(h: &Harness, token: &str, id: &str) -> Value {
    call(
        h,
        get(
            &format!("/client/work/{id}"),
            &[("authorization", &bearer(token))],
        ),
    )
    .await
    .1
}

/// (review) A run's paths are passed to git literally: a file named
/// `[ab].txt` is not a glob that also matches the owner's uncommitted
/// `b.txt`.
#[tokio::test]
async fn a_runs_paths_are_committed_literally_never_as_globs() {
    let h = harness();
    let wt = git_workspace(&h, "glob");
    // `b.txt` is tracked and the owner has changed it; `a.txt` is new and
    // the owner's too. The glob `[ab].txt` would match both.
    std::fs::write(wt.join("b.txt"), "committed\n").unwrap();
    run_git(Some(&wt), &["add", "b.txt"]);
    run_git(Some(&wt), &["commit", "--quiet", "-m", "b"]);
    std::fs::write(wt.join("b.txt"), "the owner's, uncommitted\n").unwrap();
    std::fs::write(wt.join("a.txt"), "the owner's, untracked\n").unwrap();
    std::fs::write(wt.join("[ab].txt"), "made by the run\n").unwrap();
    let (sha, _) = commit_run(&crate::client_git::work_copy_of(&h.base, "glob", &wt).unwrap(), &["[ab].txt".to_string()], "coder: glob (work w-1)").unwrap();
    let files = run_git(Some(&wt), &["show", "--name-only", "--format=", &sha]);
    assert_eq!(files.lines().collect::<Vec<_>>(), ["[ab].txt"], "{files}");
    let status = run_git(Some(&wt), &["status", "--porcelain", "--", "a.txt", "b.txt"]);
    assert!(
        status.contains("?? a.txt") && status.contains("M b.txt"),
        "a.txt and b.txt stay the owner's, uncommitted: {status}"
    );
}

/// (review, critical) After a run, the host's git never reads git config
/// the run could write: a run that plants an fsmonitor hook or a clean
/// filter in the work copy's `.git` gets none of it run on the host, and its
/// change is still committed to the workspace's own repository.
#[tokio::test]
async fn host_git_never_runs_config_a_run_planted() {
    let h = harness();
    let token = paired(&h).await;
    let (code, v) = call(&h, post("/client/workspaces", Some(&token), serde_json::json!({"repo_url": HCR}))).await;
    assert_eq!(code, 201, "{v}");
    let (_mine, checkout_ws, _) = checkout_workspace(&h, &token).await;
    for (workspace, plant, files) in [
        ("hcr", "plant-fsmonitor", vec!["made-by-run.txt"]),
        ("hcr", "plant-filter", vec![".gitattributes", "made-by-run.txt"]),
        (checkout_ws.as_str(), "plant-fsmonitor", vec!["made-by-run.txt"]),
    ] {
        let id = run_in(&h, &token, workspace, plant).await;
        let v = status(&h, &token, &id).await;
        let marker = host_ran_marker(&h.base.join(workspace));
        assert!(!marker.exists(), "{workspace} {plant}: the host ran the run's git config");
        assert_eq!(v["status"], "done", "{v}");
        assert!(v["commit"].is_string(), "{workspace} {plant}: the run's change was committed: {v}");
        let committed: Vec<&str> = v["changes"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        assert_eq!(committed, files, "{workspace} {plant}: {v}");
        // The workspace's repository (not the planted one) holds the commit.
        let (code, info) = call(
            &h,
            get(&format!("/client/workspaces/{workspace}"), &[("authorization", &bearer(&token))]),
        )
        .await;
        assert_eq!(code, 200, "{info}");
        assert!(v["commit"].as_str().unwrap().starts_with(info["head"].as_str().unwrap()), "{workspace}: {info} {v}");
        assert!(!marker.exists(), "{workspace}: reading its status ran the planted config");
        // Undo it so the next plant starts clean.
        let (code, u) = call(&h, post(&format!("/client/work/{id}/undo"), Some(&token), serde_json::json!({}))).await;
        assert_eq!(code, 200, "{u}");
        assert!(!marker.exists());
    }
}

/// (review) A repository a run makes inside the work copy (`git init sub`)
/// does not break the snapshot: it is left out and named, the run's own
/// edit is reported and committed, and later runs still see their changes.
#[tokio::test]
async fn a_nested_repository_is_left_out_of_the_snapshot_and_named() {
    let h = harness();
    let token = paired(&h).await;
    let _wt = git_workspace(&h, "nest");
    let id = run_in(&h, &token, "nest", "nested-init").await;
    let v = status(&h, &token, &id).await;
    assert_eq!(v["changes_error"], Value::Null, "{v}");
    let files: Vec<&str> = v["changes"]["files"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert_eq!(files, ["README.md"], "{v}");
    assert_eq!(v["changes"]["skipped"], serde_json::json!(["sub"]), "{v}");
    assert!(v["commit"].is_string(), "{v}");
    let id = run_in(&h, &token, "nest", "edit-other").await;
    let v = status(&h, &token, &id).await;
    let files: Vec<&str> = v["changes"]["files"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert_eq!(files, ["other.txt"], "a later run still sees its change: {v}");
}

/// (review) A snapshot that fails is never "no changes": the run says the
/// snapshot failed and why, and nothing is committed on a guess.
#[cfg(unix)]
#[tokio::test]
async fn a_failed_snapshot_is_reported_never_read_as_no_changes() {
    let h = harness();
    let token = paired(&h).await;
    let wt = git_workspace(&h, "locked");
    let id = run_in(&h, &token, "locked", "unreadable").await;
    let v = status(&h, &token, &id).await;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(wt.join("locked.txt"), std::fs::Permissions::from_mode(0o600));
    }
    assert_eq!(v["status"], "done", "{v}");
    assert_eq!(v["changes"], Value::Null, "{v}");
    assert_eq!(v["commit"], Value::Null, "{v}");
    let e = v["changes_error"].as_str().unwrap_or_else(|| panic!("the failure is named: {v}"));
    assert!(e.contains("locked.txt"), "{v}");
}

/// (review) When the workspace cannot be recorded after `git worktree add`
/// ran, the worktree and its `mux/` branch are taken back out: a retry
/// never leaves an orphan behind.
#[tokio::test]
async fn a_worktree_that_cannot_be_recorded_is_removed_again() {
    let h = harness();
    let token = paired(&h).await;
    let mine = h.dir.path().join("site");
    fixture_repo(&mine);
    // The record cannot be written: its name is taken by a folder.
    let base = h.base.canonicalize().unwrap();
    std::fs::create_dir_all(base.join(".site-b.gitdir")).unwrap();
    let (code, v) = call(
        &h,
        post("/client/workspaces", Some(&token), serde_json::json!({"source_path": mine.to_str(), "bot": "b"})),
    )
    .await;
    assert_eq!(code, 500, "{v}");
    assert!(!base.join("site-b").exists(), "the worktree folder is gone");
    let branches = run_git(Some(&mine), &["branch", "--list", "mux/*"]);
    assert_eq!(branches, "", "no orphan branch");
    let worktrees = run_git(Some(&mine), &["worktree", "list", "--porcelain"]);
    assert_eq!(worktrees.matches("worktree ").count(), 1, "only the checkout itself: {worktrees}");
}

#[tokio::test]
async fn a_finished_run_commits_exactly_its_files_to_the_branch() {
    let h = harness();
    let token = paired(&h).await;
    let wt = git_workspace(&h, "site");
    let (code, v) = call(&h, post("/client/work", Some(&token), serde_json::json!({"role": "coder", "message": "edit-readme", "workspace": "site", "title": "Fix #8: gutter"}))).await;
    assert_eq!(code, 202, "{v}");
    let id = v["work_id"].as_str().unwrap().to_string();
    let done = wait_done(&h, &token, &id).await;
    let commit = done["commit"]
        .as_str()
        .unwrap_or_else(|| panic!("a commit: {done}"))
        .to_string();
    assert_eq!(done["branch"], "mux/test-abc123");
    assert_eq!(
        run_git(Some(&wt), &["log", "-1", "--format=%h %s"]),
        format!("{commit} coder: Fix #8: gutter (work {id})")
    );
    assert_eq!(
        run_git(Some(&wt), &["show", "--name-only", "--format=", "HEAD"]),
        "README.md"
    );
    assert_eq!(
        run_git(Some(&wt), &["status", "--porcelain"]),
        "?? notes.txt",
        "the owner's own file stays out"
    );
}

#[tokio::test]
async fn a_run_that_changed_nothing_makes_no_commit() {
    let h = harness();
    let token = paired(&h).await;
    let wt = git_workspace(&h, "site");
    let id = run_in(&h, &token, "site", "hello").await;
    let done = status(&h, &token, &id).await;
    assert_eq!(done["commit"], Value::Null, "{done}");
    assert_eq!(run_git(Some(&wt), &["log", "--oneline"]).lines().count(), 1);
}

#[tokio::test]
async fn undo_reverts_that_runs_commit_and_nothing_else() {
    let h = harness();
    let token = paired(&h).await;
    let wt = git_workspace(&h, "site");
    let id = run_in(&h, &token, "site", "edit-readme").await;
    let (code, v) = call(
        &h,
        post(
            &format!("/client/work/{id}/undo"),
            Some(&token),
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(code, 200, "{v}");
    assert!(v["revert"].as_str().is_some(), "{v}");
    assert_eq!(
        std::fs::read_to_string(wt.join("README.md")).unwrap(),
        "hi\n"
    );
    assert_eq!(
        run_git(Some(&wt), &["log", "--oneline"]).lines().count(),
        3,
        "history kept: first, run, revert"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("notes.txt")).unwrap(),
        "mine, uncommitted\n"
    );
    let (code, v) = call(
        &h,
        post(
            &format!("/client/work/{id}/undo"),
            Some(&token),
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(code, 409, "undone once");
    assert!(
        v["error"].as_str().unwrap().contains("already undone"),
        "{v}"
    );
}

#[tokio::test]
async fn undo_waits_while_a_later_run_changed_the_same_files() {
    let h = harness();
    let token = paired(&h).await;
    git_workspace(&h, "site");
    let first = run_in(&h, &token, "site", "edit-readme").await;
    let second = run_in(&h, &token, "site", "edit-readme-again").await;
    let (code, v) = call(
        &h,
        post(
            &format!("/client/work/{first}/undo"),
            Some(&token),
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(code, 409, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains(&second),
        "names the later run: {v}"
    );
    let (code, v) = call(
        &h,
        post(
            &format!("/client/work/{second}/undo"),
            Some(&token),
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(code, 200, "{v}");
    let (code, v) = call(
        &h,
        post(
            &format!("/client/work/{first}/undo"),
            Some(&token),
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(code, 200, "once the later run is undone: {v}");
}

#[tokio::test]
async fn a_tweak_is_its_own_run_with_its_own_commit() {
    let h = harness();
    let token = paired(&h).await;
    let wt = git_workspace(&h, "site");
    let first = run_in(&h, &token, "site", "edit-readme").await;
    let second = run_after(&h, &token, "site", "edit-other", Some(&first)).await;
    let a = status(&h, &token, &first).await["commit"]
        .as_str()
        .unwrap()
        .to_string();
    let b = status(&h, &token, &second).await["commit"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(a, b);
    assert_eq!(
        run_git(Some(&wt), &["show", "--name-only", "--format=", &b]),
        "other.txt"
    );
}

#[tokio::test]
async fn a_detached_work_copy_isnt_committed_and_says_why() {
    let h = harness();
    let token = paired(&h).await;
    let wt = git_workspace(&h, "site");
    run_git(Some(&wt), &["checkout", "--quiet", "--detach"]);
    let id = run_in(&h, &token, "site", "edit-readme").await;
    let done = status(&h, &token, &id).await;
    assert_eq!(done["commit"], Value::Null);
    assert!(
        done["commit_error"]
            .as_str()
            .unwrap_or("")
            .contains("branch"),
        "{done}"
    );
}

fn undo(id: &str, token: &str) -> Request<Body> {
    post(&format!("/client/work/{id}/undo"), Some(token), serde_json::json!({}))
}

/// Starts `message` in `workspace` and returns its id, without waiting.
async fn start_in(h: &Harness, token: &str, workspace: &str, message: &str) -> String {
    let body = serde_json::json!({"role": "coder", "message": message, "workspace": workspace});
    let (code, v) = call(h, post("/client/work", Some(token), body)).await;
    assert_eq!(code, 202, "{v}");
    v["work_id"].as_str().unwrap().to_string()
}

/// Undo is refused, with a code, while another run is working in the same
/// work copy: the revert would land under that run's changes. Once it ends,
/// the same Undo goes ahead.
#[tokio::test]
async fn undo_is_refused_while_another_run_works_in_the_same_copy() {
    let h = harness();
    let token = paired(&h).await;
    let wt = git_workspace(&h, "site");
    let first = run_in(&h, &token, "site", "edit-readme").await;
    let head = run_git(Some(&wt), &["rev-parse", "HEAD"]);
    let busy = start_in(&h, &token, "site", "wait").await;
    let (code, v) = call(&h, undo(&first, &token)).await;
    assert_eq!((code, v["code"].as_str()), (409, Some("conflict")), "{v}");
    assert!(v["error"].as_str().unwrap().contains(&busy), "names the run: {v}");
    assert_eq!(run_git(Some(&wt), &["rev-parse", "HEAD"]), head, "nothing reverted");
    let _ = cancel(&h, Some(&token), &busy).await;
    let _ = wait_done(&h, &token, &busy).await;
    let (code, v) = call(&h, undo(&first, &token)).await;
    assert_eq!(code, 200, "{v}");
}

/// Two Undos of one run racing past the route's own "already undone" check
/// revert it once: the second, holding the copy after the first let go,
/// sees the run undone.
#[tokio::test]
async fn an_undo_that_lost_the_race_finds_the_run_already_undone() {
    let h = harness();
    let token = paired(&h).await;
    git_workspace(&h, "site");
    let first = run_in(&h, &token, "site", "edit-readme").await;
    let held = UndoHold::take(&h.state.works, &first).unwrap_or_else(|_| panic!("the copy is free"));
    assert!(UndoHold::take(&h.state.works, &first).is_err(), "held by the first Undo");
    h.state.works.lock().unwrap().get_mut(&first).unwrap().decided = Some("undone by abc".into());
    drop(held);
    let refused = UndoHold::take(&h.state.works, &first).err().expect("already undone");
    assert_eq!(refused.status(), StatusCode::CONFLICT);
}

/// No run starts in a work copy while an Undo is reverting there.
#[tokio::test]
async fn no_run_starts_while_an_undo_reverts_in_the_same_copy() {
    let h = harness();
    let token = paired(&h).await;
    git_workspace(&h, "site");
    let first = run_in(&h, &token, "site", "edit-readme").await;
    h.state.works.lock().unwrap().get_mut(&first).unwrap().undoing = true;
    let body = serde_json::json!({"role": "coder", "message": "edit-other", "workspace": "site"});
    let (code, v) = call(&h, post("/client/work", Some(&token), body.clone())).await;
    assert_eq!((code, v["code"].as_str()), (409, Some("conflict")), "{v}");
    assert!(v["error"].as_str().unwrap().contains("being undone"), "{v}");
    h.state.works.lock().unwrap().get_mut(&first).unwrap().undoing = false;
    let (code, v) = call(&h, post("/client/work", Some(&token), body)).await;
    assert_eq!(code, 202, "{v}");
}

/// An Undo still reverting is remembered however long ago its run was
/// made, so no run starts in that copy meanwhile.
#[tokio::test]
async fn an_undo_in_progress_is_never_forgotten_as_old_work() {
    let h = harness();
    let token = paired(&h).await;
    git_workspace(&h, "site");
    let first = run_in(&h, &token, "site", "edit-readme").await;
    h.state.works.lock().unwrap().get_mut(&first).unwrap().undoing = true;
    h.clock.fetch_add(KEEP_WORK_SECS + 1, Ordering::SeqCst);
    let body = serde_json::json!({"role": "coder", "message": "edit-other", "workspace": "site"});
    let (code, v) = call(&h, post("/client/work", Some(&token), body)).await;
    assert_eq!((code, v["code"].as_str()), (409, Some("conflict")), "{v}");
}

/// A run that is cancelled after editing the work copy gets its edits as
/// its own commit, which Undo can revert; they never ride along in the next
/// run's commit.
#[tokio::test]
async fn a_cancelled_runs_edits_are_its_own_commit_never_the_next_runs() {
    let h = harness();
    let token = paired(&h).await;
    let wt = git_workspace(&h, "site");
    let id = start_in(&h, &token, "site", "edit-wait").await;
    let _ = cancel(&h, Some(&token), &id).await;
    let done = wait_done(&h, &token, &id).await;
    assert_eq!(done["status"], "cancelled", "{done}");
    assert!(done["commit"].as_str().is_some(), "{done}");
    assert_eq!(done["files"], serde_json::json!(["in-progress.txt"]), "{done}");
    assert_eq!(run_git(Some(&wt), &["show", "--name-only", "--format=%s", "HEAD"]), format!("coder: edit-wait (work {id}, cancelled)\n\nin-progress.txt"));
    let next = run_in(&h, &token, "site", "edit-other").await;
    assert_eq!(status(&h, &token, &next).await["changes"]["files"][0]["path"], "other.txt");
    assert_eq!(run_git(Some(&wt), &["show", "--name-only", "--format=", "HEAD"]), "other.txt");
    assert_eq!(run_git(Some(&wt), &["status", "--porcelain"]), "?? notes.txt");
    let (code, v) = call(&h, undo(&id, &token)).await;
    assert_eq!(code, 200, "{v}");
    assert!(!wt.join("in-progress.txt").exists());
}

/// A run that fails after editing the work copy is committed the same way,
/// and still says why it failed.
#[tokio::test]
async fn a_failed_runs_edits_are_its_own_commit() {
    let h = harness();
    let token = paired(&h).await;
    let wt = git_workspace(&h, "site");
    let id = run_in(&h, &token, "site", "edit-fail").await;
    let done = status(&h, &token, &id).await;
    assert_eq!((done["status"].as_str(), done["code"].as_str()), (Some("error"), Some("dispatch_failed")), "{done}");
    assert!(done["commit"].as_str().is_some(), "{done}");
    assert_eq!(run_git(Some(&wt), &["show", "--name-only", "--format=%s", "HEAD"]), format!("coder: edit-fail (work {id}, failed)\n\nin-progress.txt"));
    let next = run_in(&h, &token, "site", "edit-other").await;
    assert_eq!(status(&h, &token, &next).await["changes"]["files"][0]["path"], "other.txt");
}

/// A cancel that lands after the run already produced its result does not
/// relabel it: it is done, with its commit.
#[tokio::test]
async fn a_cancel_after_the_run_finished_leaves_it_done() {
    let h = harness();
    let token = paired(&h).await;
    git_workspace(&h, "site");
    let id = run_in(&h, &token, "site", "finish-then-cancel").await;
    let done = status(&h, &token, &id).await;
    assert_eq!(done["status"], "done", "{done}");
    assert!(done["commit"].as_str().is_some(), "{done}");
}

#[tokio::test]
async fn undo_refuses_what_it_cant_do() {
    let h = harness();
    let token = paired(&h).await;
    let other = paired(&h).await;
    git_workspace(&h, "site");
    own(&h, "site", &client_id(&h, FIRST_CLIENT));
    let quiet = run_in(&h, &token, "site", "hello").await;
    let (code, v) = call(
        &h,
        post(
            &format!("/client/work/{quiet}/undo"),
            Some(&token),
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(code, 409, "nothing to undo: {v}");
    let id = run_in(&h, &token, "site", "edit-readme").await;
    let (code, _) = call(
        &h,
        post(
            &format!("/client/work/{id}/undo"),
            Some(&other),
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(code, 404);
    let (code, _) = call(
        &h,
        post(
            &format!("/client/work/{id}/undo"),
            None,
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(code, 401);
    let (code, v) = call(
        &h,
        post(
            "/client/work",
            Some(&token),
            serde_json::json!({"role": "coder", "message": "wait", "workspace": "site"}),
        ),
    )
    .await;
    assert_eq!(code, 202, "{v}");
    let busy = v["work_id"].as_str().unwrap().to_string();
    let (code, _) = call(
        &h,
        post(
            &format!("/client/work/{busy}/undo"),
            Some(&token),
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(code, 409, "not finished");
    call(
        &h,
        post(
            &format!("/client/work/{busy}/cancel"),
            Some(&token),
            serde_json::json!({}),
        ),
    )
    .await;
}

#[tokio::test]
async fn old_clients_keep_and_discard_still_work() {
    let h = harness();
    let token = paired(&h).await;
    let wt = git_workspace(&h, "site");
    let id = run_in(&h, &token, "site", "edit-readme").await;
    let commit = status(&h, &token, &id).await["commit"]
        .as_str()
        .unwrap()
        .to_string();
    // Keep: the commit is already there; it just reports it.
    let (code, v) = call(
        &h,
        post(
            &format!("/client/work/{id}/keep"),
            Some(&token),
            serde_json::json!({"message": "x"}),
        ),
    )
    .await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["commit"], commit.as_str());
    assert_eq!(v["files"], serde_json::json!(["README.md"]));
    assert_eq!(
        run_git(Some(&wt), &["log", "--oneline"]).lines().count(),
        2,
        "no second commit"
    );
    // Discard: an Undo.
    let (code, v) = call(
        &h,
        post(
            &format!("/client/work/{id}/discard"),
            Some(&token),
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(
        std::fs::read_to_string(wt.join("README.md")).unwrap(),
        "hi\n"
    );
}

// ─── Previews ────────────────────────────────────────────────────────────

/// The previews' dev server: this test binary itself, running
/// [`fake_dev_server`], so a preview test depends on nothing the machine
/// may lack or start slowly (an interpreter, a package). It serves `$PORT`
/// as soon as it has bound it and exits on SIGTERM like a real dev server.
fn serve() -> String {
    let exe = std::env::current_exe().unwrap();
    format!(
        "FAKE_DEV_SERVER=1 exec '{}' --exact client_pairing::tests::fake_dev_server --ignored --nocapture --test-threads 1",
        exe.display()
    )
}

/// Not a test: the stand-in dev server [`serve`] starts. Without
/// `FAKE_DEV_SERVER` it does nothing. It answers every request with 200 and
/// the working directory's `index.html` (else "ok").
#[test]
#[ignore]
fn fake_dev_server() {
    if std::env::var_os("FAKE_DEV_SERVER").is_none() {
        return;
    }
    use std::io::{Read, Write};
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(0);
    let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    for stream in listener.incoming() {
        let Ok(mut s) = stream else { continue };
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf);
        let body = std::fs::read("index.html").unwrap_or_else(|_| b"ok".to_vec());
        let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
        let _ = s.write_all(head.as_bytes());
        let _ = s.write_all(&body);
    }
}

/// Writes a recipe (with this darkmux's `schema_version` unless it names one).
fn recipe(checkout: &Path, mut json: Value) {
    if json.get("schema_version").is_none() {
        json["schema_version"] = darkmux_types::data_version::PREVIEW_RECIPE_SCHEMA_VERSION.into();
    }
    write_recipe(checkout, &json.to_string());
}

/// Polls the preview until its status is one of `done`. The panic names the
/// last status the listener reported, with its error and log.
async fn preview_until(h: &Harness, token: &str, name: &str, done: &[&str]) -> Value {
    let auth = bearer(token);
    let mut last = Value::Null;
    // Up to 120 s: under a full parallel suite a cold dev server can be slow
    // to answer (the listener's own deadline is 180 s).
    for _ in 0..2400 {
        let (code, v) = call(
            h,
            get(
                &format!("/client/workspaces/{name}/preview"),
                &[("authorization", &auth)],
            ),
        )
        .await;
        assert_eq!(code, 200, "{v}");
        if done.contains(&v["status"].as_str().unwrap()) {
            return v;
        }
        last = v;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("preview {name} never reached {done:?}; last: {last}");
}

fn answers(port: u64) -> bool {
    std::net::TcpStream::connect(("127.0.0.1", port as u16)).is_ok()
}

async fn start_preview(h: &Harness, token: &str, name: &str) -> (u16, Value) {
    call(
        h,
        post(
            &format!("/client/workspaces/{name}/preview"),
            Some(token),
            serde_json::json!({}),
        ),
    )
    .await
}

/// A workspace made from the user's checkout, with a page for the dev
/// server to serve (so readiness never depends on a directory listing).
async fn served_workspace(h: &Harness, token: &str) -> (PathBuf, String, PathBuf) {
    let (mine, name, ws) = checkout_workspace(h, token).await;
    std::fs::write(ws.join("index.html"), "hello preview").unwrap();
    (mine, name, ws)
}

#[tokio::test]
async fn a_preview_runs_from_the_recipe_and_stops() {
    let h = harness();
    let token = paired(&h).await;
    let (mine, name, _ws) = served_workspace(&h, &token).await;
    recipe(
        &mine,
        serde_json::json!({"command": serve(), "env": {"PORT": "{port}"}, "ready": {"http": "/"}}),
    );
    let (code, v) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 202, "{v}");
    let ready = preview_until(&h, &token, &name, &["ready", "failed"]).await;
    assert_eq!(ready["status"], "ready", "{ready}");
    let port = ready["port"].as_u64().unwrap();
    assert_eq!(ready["url"], format!("http://127.0.0.1:{port}/"));
    assert_ne!(ready["port"], ready["hmr_port"]);
    assert!(answers(port));
    // Asking again returns the same one.
    let (code, again) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 200);
    assert_eq!(again["port"], ready["port"]);
    // Stop: the server is gone.
    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/client/workspaces/{name}/preview"))
        .header("authorization", bearer(&token))
        .body(Body::empty())
        .unwrap();
    let asked = std::time::Instant::now();
    let (code, _) = call(&h, req).await;
    assert_eq!(code, 200);
    assert!(
        asked.elapsed() < std::time::Duration::from_secs(3),
        "a polite stop, not the KILL fallback"
    );
    for _ in 0..100 {
        if !answers(port) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(!answers(port), "stopped");
}

/// (review, operator decision 2026-11-09) A preview runs only from the
/// recipe in the user's own checkout. A recipe a run wrote in the workspace
/// is never read: with none in the checkout there is no preview, and with
/// one there, it is the checkout's that runs.
#[tokio::test]
async fn a_preview_never_runs_the_workspaces_own_recipe() {
    let h = harness();
    let token = paired(&h).await;
    let (mine, name, ws) = served_workspace(&h, &token).await;
    recipe(
        &ws,
        serde_json::json!({"command": format!("touch pwned; {}", serve()), "env": {"PORT": "{port}"},
            "install": {"command": "touch pwned-install"}}),
    );
    let (code, v) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 404, "{v}");
    let e = v["error"].as_str().unwrap();
    assert!(e.contains("add .darkmux/preview.json to your own checkout"), "{v}");
    assert!(e.contains(mine.to_str().unwrap()), "names the checkout: {v}");
    recipe(&mine, serde_json::json!({"command": serve(), "env": {"PORT": "{port}"}}));
    let (code, v) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 202, "{v}");
    let ready = preview_until(&h, &token, &name, &["ready", "failed"]).await;
    assert_eq!(ready["status"], "ready", "{ready}");
    h.state.previews.stop(&name);
    assert!(!ws.join("pwned").exists() && !ws.join("pwned-install").exists(), "the workspace's recipe ran");
}

/// A workspace cloned from a URL has no checkout of the user's, so no
/// preview, even with a recipe in it; the refusal names the repo.
#[tokio::test]
async fn a_cloned_workspace_gets_no_preview() {
    let h = harness();
    let token = paired(&h).await;
    let (code, v) = call(
        &h,
        post("/client/workspaces", Some(&token), ws_body(serde_json::json!({"repo_url": "https://github.com/kstrat2001/hcr"}))),
    )
    .await;
    assert_eq!(code, 201, "{v}");
    let ws = h.base.join("hcr");
    recipe(&ws, serde_json::json!({"command": format!("touch pwned; {}", serve())}));
    let (code, v) = start_preview(&h, &token, "hcr").await;
    assert_eq!(code, 404, "{v}");
    assert_eq!(v["code"], "no_source_checkout", "{v}");
    let e = v["error"].as_str().unwrap();
    assert!(e.contains("not made from a checkout of yours") && e.contains("github.com/kstrat2001/hcr"), "{v}");
    assert!(!ws.join("pwned").exists());
}

/// What darkmux recorded is what counts: a clone that takes the name of an
/// old checkout-made workspace inherits nothing, and a workspace whose `.git`
/// no longer points at the recorded checkout gets no preview.
#[tokio::test]
async fn a_preview_needs_the_workspace_to_still_be_a_worktree_of_the_checkout() {
    let h = harness();
    let token = paired(&h).await;
    let (mine, name, ws) = served_workspace(&h, &token).await;
    recipe(&mine, serde_json::json!({"command": serve(), "env": {"PORT": "{port}"}}));
    // A run rewrites the workspace's `.git`: host git never reads it, so the
    // preview is unaffected.
    std::fs::remove_file(ws.join(".git")).unwrap();
    run_git(Some(&ws), &["init", "--quiet"]);
    let (code, v) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 202, "{v}");
    h.state.previews.stop(&name);
    // The checkout drops the worktree (`git worktree prune` after its admin
    // dir went): the workspace is no longer a worktree of it.
    let admin: Vec<_> = std::fs::read_dir(mine.join(".git/worktrees")).unwrap().collect();
    for a in admin {
        std::fs::remove_dir_all(a.unwrap().path()).unwrap();
    }
    let (code, v) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 404, "{v}");
    assert_eq!(v["code"], "no_source_checkout", "{v}");
    assert!(v["error"].as_str().unwrap().contains("no longer a worktree"), "{v}");
    // A stale record under the name a clone then takes.
    let base = h.base.canonicalize().unwrap();
    std::fs::write(base.join(".hcr.source"), format!("{}\n", mine.display())).unwrap();
    let (code, v) = call(
        &h,
        post("/client/workspaces", Some(&token), ws_body(serde_json::json!({"repo_url": "https://github.com/kstrat2001/hcr"}))),
    )
    .await;
    assert_eq!(code, 201, "{v}");
    assert!(!base.join(".hcr.source").exists(), "the clone forgot the old record");
    let (code, v) = start_preview(&h, &token, "hcr").await;
    assert_eq!(code, 404, "{v}");
}

/// The recorded checkout is checked again at every start: one that has
/// since become a symlink (to the same repository, even) is not the user's
/// checkout any more.
#[cfg(unix)]
#[tokio::test]
async fn a_recorded_checkout_that_became_a_symlink_gets_no_preview() {
    let h = harness();
    let token = paired(&h).await;
    let (mine, name, _ws) = served_workspace(&h, &token).await;
    let moved = h.dir.path().join("site-moved");
    std::fs::rename(&mine, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &mine).unwrap();
    recipe(&moved, serde_json::json!({"command": serve(), "env": {"PORT": "{port}"}}));
    let (code, v) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 404, "{v}");
    assert!(v["error"].as_str().unwrap().contains("symlink"), "{v}");
}

/// (review) darkmux's records beside a workspace count only as plain files:
/// a record that is a symlink (to a file naming the user's checkout, or a
/// repository) is no record.
#[cfg(unix)]
#[tokio::test]
async fn a_record_that_is_a_symlink_is_no_record() {
    let h = harness();
    let token = paired(&h).await;
    let (mine, _name, _ws) = checkout_workspace(&h, &token).await;
    recipe(&mine, serde_json::json!({"command": serve(), "env": {"PORT": "{port}"}}));
    let (code, v) = call(&h, post("/client/workspaces", Some(&token), serde_json::json!({"repo_url": HCR}))).await;
    assert_eq!(code, 201, "{v}");
    let base = h.base.canonicalize().unwrap();
    let named = h.dir.path().join("names-mine.txt");
    std::fs::write(&named, format!("{}\n", mine.canonicalize().unwrap().display())).unwrap();
    std::os::unix::fs::symlink(&named, base.join(".hcr.source")).unwrap();
    let (code, v) = start_preview(&h, &token, "hcr").await;
    assert_eq!(code, 404, "{v}");
    assert!(v["error"].as_str().unwrap().contains("not made from a checkout of yours"), "{v}");
    // The repository record too: linked, host git has no repository there.
    let held = base.join(".hcr.gitdir");
    let real = h.dir.path().join("hcr-gitdir-record");
    std::fs::rename(&held, &real).unwrap();
    std::os::unix::fs::symlink(&real, &held).unwrap();
    let (code, v) = call(&h, get("/client/workspaces/hcr", &[("authorization", &bearer(&token))])).await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["head"], "", "no repository read through a linked record: {v}");
}

#[tokio::test]
async fn install_runs_only_when_its_marker_is_missing() {
    let h = harness();
    let token = paired(&h).await;
    let (mine, name, ws) = served_workspace(&h, &token).await;
    recipe(
        &mine,
        serde_json::json!({
            "command": serve(), "env": {"PORT": "{port}"},
            "install": {"command": "mkdir deps && echo ran >> installs.txt", "unless_exists": "deps"}
        }),
    );
    start_preview(&h, &token, &name).await;
    assert_eq!(
        preview_until(&h, &token, &name, &["ready", "failed"]).await["status"],
        "ready"
    );
    h.state.previews.stop(&name);
    start_preview(&h, &token, &name).await;
    assert_eq!(
        preview_until(&h, &token, &name, &["ready", "failed"]).await["status"],
        "ready"
    );
    h.state.previews.stop(&name);
    assert_eq!(
        std::fs::read_to_string(ws.join("installs.txt")).unwrap(),
        "ran\n",
        "installed once, in the workspace"
    );
}

/// (#3484) Starts the preview with `body` (none when `None`), waits until it
/// is ready or failed, and stops it. Returns the start's code and reply.
async fn start_with(h: &Harness, token: &str, name: &str, body: Option<Value>) -> (u16, Value) {
    let path = format!("/client/workspaces/{name}/preview");
    let req = match body {
        Some(b) => post(&path, Some(token), b),
        None => Request::builder()
            .method("POST")
            .uri(&path)
            .header("authorization", bearer(token))
            .body(Body::empty())
            .unwrap(),
    };
    let (code, v) = call(h, req).await;
    if code == 200 || code == 202 {
        let done = preview_until(h, token, name, &["ready", "failed"]).await;
        assert_eq!(done["status"], "ready", "{done}");
        h.state.previews.stop(name);
    }
    (code, v)
}

/// A served workspace whose recipe installs into `node_modules`, counting
/// installs in `installs.txt`.
async fn installing_workspace(h: &Harness, token: &str) -> (PathBuf, String, PathBuf) {
    let (mine, name, ws) = served_workspace(h, token).await;
    recipe(
        &mine,
        serde_json::json!({
            "command": serve(), "env": {"PORT": "{port}"},
            "install": {"command": "mkdir -p node_modules && echo ran >> installs.txt", "unless_exists": "node_modules"}
        }),
    );
    (mine, name, ws)
}

fn installs(ws: &Path) -> usize {
    std::fs::read_to_string(ws.join("installs.txt"))
        .unwrap_or_default()
        .lines()
        .count()
}

/// (#3484) A plain start (no body, as before) installs once, then skips;
/// `{"clean": true}` deletes `node_modules` and the install record and
/// reinstalls; a clean start of a stopped preview just starts it fresh.
#[tokio::test]
async fn a_clean_restart_deletes_node_modules_and_reinstalls() {
    let h = harness();
    let token = paired(&h).await;
    let (_mine, name, ws) = installing_workspace(&h, &token).await;
    assert_eq!(
        start_with(&h, &token, &name, None).await.0,
        202,
        "no body: a plain start"
    );
    assert_eq!(installs(&ws), 1);
    std::fs::write(ws.join("node_modules/stale.txt"), "old").unwrap();
    assert_eq!(
        start_with(&h, &token, &name, Some(serde_json::json!({})))
            .await
            .0,
        202
    );
    assert_eq!(installs(&ws), 1, "an empty body is a plain start: skipped");
    assert_eq!(
        start_with(&h, &token, &name, Some(serde_json::json!({"clean": false})))
            .await
            .0,
        202
    );
    assert_eq!(installs(&ws), 1, "clean: false is a plain start");
    // The preview is stopped here; a clean start just starts it fresh.
    let (code, v) = start_with(&h, &token, &name, Some(serde_json::json!({"clean": true}))).await;
    assert_eq!(code, 202, "{v}");
    assert_eq!(installs(&ws), 2, "reinstalled");
    assert!(
        !ws.join("node_modules/stale.txt").exists(),
        "node_modules was deleted first"
    );
    // A clean restart of a RUNNING preview stops it first.
    let path = format!("/client/workspaces/{name}/preview");
    let (code, _) = call(&h, post(&path, Some(&token), serde_json::json!({}))).await;
    assert_eq!(code, 202);
    preview_until(&h, &token, &name, &["ready"]).await;
    let (code, v) = start_with(&h, &token, &name, Some(serde_json::json!({"clean": true}))).await;
    assert_eq!(code, 202, "a running preview is restarted: {v}");
    assert_eq!(installs(&ws), 3);
}

/// (#3484) The clean restart's delete never follows a link: a
/// `node_modules` that is a symlink out of the workspace is unlinked, and
/// what it pointed at is untouched.
#[cfg(unix)]
#[tokio::test]
async fn a_clean_restart_unlinks_a_symlinked_node_modules_and_never_follows_it() {
    let h = harness();
    let token = paired(&h).await;
    let (_mine, name, ws) = installing_workspace(&h, &token).await;
    let outside = h.dir.path().join("outside-modules");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep.txt"), "mine").unwrap();
    std::os::unix::fs::symlink(&outside, ws.join("node_modules")).unwrap();
    let (code, v) = start_with(&h, &token, &name, Some(serde_json::json!({"clean": true}))).await;
    assert_eq!(code, 202, "{v}");
    assert_eq!(
        std::fs::read_to_string(outside.join("keep.txt")).unwrap(),
        "mine",
        "the target is untouched"
    );
    assert!(
        !std::fs::symlink_metadata(ws.join("node_modules"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link is gone, and the install made a real folder"
    );
    assert_eq!(installs(&ws), 1);
}

/// (#3484) The start body is parsed strictly: an unknown key is refused
/// with a code, and nothing starts.
#[tokio::test]
async fn a_preview_start_refuses_an_unknown_body_key() {
    let h = harness();
    let token = paired(&h).await;
    let (_mine, name, ws) = installing_workspace(&h, &token).await;
    let (code, v) = start_with(
        &h,
        &token,
        &name,
        Some(serde_json::json!({"clean": true, "cleen": 1})),
    )
    .await;
    assert_eq!(code, 400, "{v}");
    assert_eq!(v["code"], "bad_request", "{v}");
    assert!(v["error"].as_str().unwrap().contains("cleen"), "{v}");
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(installs(&ws), 0, "nothing started");
    assert!(h.state.previews.status(&name).is_none());
}

#[tokio::test]
async fn a_worktree_preview_gets_env_files_from_its_checkout_without_overwriting() {
    let h = harness();
    let token = paired(&h).await;
    let (mine, name, ws) = served_workspace(&h, &token).await;
    std::fs::write(mine.join(".env"), "SECRET=from-source\n").unwrap();
    std::fs::write(mine.join(".env.local"), "LOCAL=from-source\n").unwrap();
    std::fs::write(ws.join(".env.local"), "LOCAL=already-here\n").unwrap();
    recipe(
        &mine,
        serde_json::json!({"command": serve(), "env": {"PORT": "{port}"}, "env_from_source": [".env", ".env.local"]}),
    );
    start_preview(&h, &token, &name).await;
    assert_eq!(
        preview_until(&h, &token, &name, &["ready", "failed"]).await["status"],
        "ready"
    );
    h.state.previews.stop(&name);
    assert_eq!(
        std::fs::read_to_string(ws.join(".env")).unwrap(),
        "SECRET=from-source\n"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join(".env.local")).unwrap(),
        "LOCAL=already-here\n",
        "never overwritten"
    );
}

#[tokio::test]
async fn a_failing_dev_server_says_why() {
    let h = harness();
    let token = paired(&h).await;
    let (mine, name, _ws) = checkout_workspace(&h, &token).await;
    recipe(
        &mine,
        serde_json::json!({"command": "echo boom-from-the-server; exit 3"}),
    );
    start_preview(&h, &token, &name).await;
    let v = preview_until(&h, &token, &name, &["ready", "failed"]).await;
    assert_eq!(v["status"], "failed", "{v}");
    assert!(v["error"].as_str().unwrap().contains("exited"), "{v}");
    assert!(
        v["log_tail"]
            .as_str()
            .unwrap()
            .contains("boom-from-the-server"),
        "{v}"
    );
}

/// Reading and stopping a preview need the token, like starting one: a
/// caller without it learns nothing and stops nothing.
#[tokio::test]
async fn preview_status_and_stop_need_a_token() {
    let h = harness();
    let token = paired(&h).await;
    let (mine, name, _ws) = served_workspace(&h, &token).await;
    recipe(&mine, serde_json::json!({"command": serve(), "env": {"PORT": "{port}"}}));
    let (code, v) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 202, "{v}");
    let ready = preview_until(&h, &token, &name, &["ready", "failed"]).await;
    let path = format!("/client/workspaces/{name}/preview");
    for auth in [None, Some(bearer(&"0".repeat(64)))] {
        let headers: Vec<(&str, &str)> = auth.iter().map(|a| ("authorization", a.as_str())).collect();
        let (code, v) = call(&h, get(&path, &headers)).await;
        assert_eq!(code, 401, "status: {v}");
        assert!(v.get("port").is_none(), "{v}");
        let mut b = Request::builder().method("DELETE").uri(&path);
        for (k, val) in &headers {
            b = b.header(*k, *val);
        }
        let (code, v) = call(&h, b.body(Body::empty()).unwrap()).await;
        assert_eq!(code, 401, "stop: {v}");
    }
    assert!(answers(ready["port"].as_u64().unwrap()), "still running");
    h.state.previews.stop(&name);
}

/// (operator decision 2026-11-09) Previews are off unless this machine turns
/// them on: a preview runs the workspace's own code on this machine, outside
/// the sandbox. Off, starting or reading one is refused with what it would
/// do and how to turn it on, and nothing starts; stopping one still works.
#[tokio::test]
async fn previews_are_refused_while_clients_preview_enabled_is_off() {
    let h = harness();
    let token = paired(&h).await;
    let (mine, name, ws) = served_workspace(&h, &token).await;
    recipe(&mine, serde_json::json!({"command": format!("touch ran; {}", serve()), "env": {"PORT": "{port}"}}));
    *h.preview_on.lock().unwrap() = false;
    let (code, v) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 403, "{v}");
    assert_eq!(v["code"], "preview_disabled", "{v}");
    let e = v["error"].as_str().unwrap();
    assert!(
        e.contains("darkmux config set clients.preview.enabled true")
            && e.contains("runs the workspace's own code")
            && e.contains("in a container with network access"),
        "{v}"
    );
    let (code, v) = call(&h, get(&format!("/client/workspaces/{name}/preview"), &[("authorization", &bearer(&token))])).await;
    assert_eq!(code, 403, "{v}");
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!ws.join("ran").exists(), "nothing started");
    // An unreadable switch refuses too.
    *h.preview_on.lock().unwrap() = true;
    let mut h2 = h;
    h2.state.preview_enabled = Arc::new(|| Err("config.json does not parse".to_string()));
    let (code, v) = start_preview(&h2, &token, &name).await;
    assert_eq!(code, 503, "{v}");
}

/// The switch reads `clients.preview.enabled` fresh, through the config
/// gate: absent is off, and a misspelling refuses rather than reading as off
/// or on.
#[test]
#[serial_test::serial]
fn clients_preview_enabled_is_read_fresh_and_defaults_off() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.json");
    let read = |text: &str| {
        std::fs::write(&path, text).unwrap();
        unsafe { std::env::remove_var("DARKMUX_CLIENTS_PREVIEW_ENABLED") };
        read_preview_enabled_at(&path)
    };
    assert_eq!(read("{}"), Ok(false), "absent is off");
    assert_eq!(read(r#"{"clients": {"preview": {"enabled": true}}}"#), Ok(true));
    assert_eq!(read(r#"{"clients": {"preview": {"enabled": false}}}"#), Ok(false));
    let e = read(r#"{"clients": {"preview": {"enable": true}}}"#).unwrap_err();
    assert!(e.contains("clients.preview.enable"), "{e}");
    std::fs::remove_file(&path).unwrap();
    assert_eq!(read_preview_enabled_at(&path), Ok(false), "no file is off");
}

/// (#3459) Once a minute `client serve` stops a preview no app has asked
/// about (a start or a status) for `clients.preview.idle_minutes`, read at
/// each check: a status restarts the count, a setting changed between checks
/// applies at the next one, and `0` never stops one.
#[tokio::test]
async fn a_preview_no_app_asks_about_is_stopped_by_the_idle_check() {
    let h = harness();
    let token = paired(&h).await;
    let (mine, name, _ws) = served_workspace(&h, &token).await;
    recipe(
        &mine,
        serde_json::json!({"command": serve(), "env": {"PORT": "{port}"}}),
    );
    let (code, v) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 202, "{v}");
    let ready = preview_until(&h, &token, &name, &["ready", "failed"]).await;
    assert_eq!(ready["status"], "ready", "{ready}");
    let port = ready["port"].as_u64().unwrap();
    let minutes = |m: u64| h.clock.fetch_add(m * 60, Ordering::SeqCst);
    let mut skipped = None;
    minutes(14);
    assert!(
        preview_idle_tick(&h.state, &mut skipped).is_empty(),
        "14 minutes is not idle"
    );
    let (code, _) = call(
        &h,
        get(
            &format!("/client/workspaces/{name}/preview"),
            &[("authorization", &bearer(&token))],
        ),
    )
    .await;
    assert_eq!(code, 200, "an app asks");
    minutes(14);
    assert!(
        preview_idle_tick(&h.state, &mut skipped).is_empty(),
        "the status restarted the count"
    );
    minutes(60);
    *h.preview_idle.lock().unwrap() = Ok(0);
    assert!(
        preview_idle_tick(&h.state, &mut skipped).is_empty(),
        "0 is never"
    );
    *h.preview_idle.lock().unwrap() = Ok(15);
    let lines = preview_idle_tick(&h.state, &mut skipped);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains(&name) && lines[0].contains("clients.preview.idle_minutes"),
        "{lines:?}"
    );
    for _ in 0..100 {
        if !answers(port) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(!answers(port), "the dev server is gone");
    let (code, v) = call(
        &h,
        get(
            &format!("/client/workspaces/{name}/preview"),
            &[("authorization", &bearer(&token))],
        ),
    )
    .await;
    assert_eq!((code, v["status"].as_str()), (200, Some("stopped")), "{v}");
}

/// (#3459) The idle check's thread ticks on its own, and ends when told.
#[tokio::test]
async fn the_idle_check_thread_stops_idle_previews_on_its_own() {
    let h = harness();
    let token = paired(&h).await;
    let (mine, name, _ws) = served_workspace(&h, &token).await;
    recipe(
        &mine,
        serde_json::json!({"command": serve(), "env": {"PORT": "{port}"}}),
    );
    start_preview(&h, &token, &name).await;
    let ready = preview_until(&h, &token, &name, &["ready", "failed"]).await;
    assert_eq!(ready["status"], "ready", "{ready}");
    h.clock.fetch_add(16 * 60, Ordering::SeqCst);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let thread = spawn_preview_idle_check(
        h.state.clone(),
        std::time::Duration::from_millis(20),
        stop.clone(),
    );
    // Watched from outside (a status would be an app asking).
    let port = ready["port"].as_u64().unwrap();
    for _ in 0..200 {
        if !answers(port) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(!answers(port), "stopped by the thread");
    assert_eq!(h.state.previews.status(&name).unwrap().status, "stopped");
    stop.store(true, Ordering::SeqCst);
    thread.join().unwrap();
}

/// (#3459) While `clients.preview.idle_minutes` cannot be read, the idle
/// check stops nothing on a guessed value, and says why once, not every
/// minute.
#[tokio::test]
async fn an_unreadable_idle_setting_stops_nothing_and_says_so_once() {
    let h = harness();
    let token = paired(&h).await;
    let (mine, name, _ws) = served_workspace(&h, &token).await;
    recipe(
        &mine,
        serde_json::json!({"command": serve(), "env": {"PORT": "{port}"}}),
    );
    start_preview(&h, &token, &name).await;
    let ready = preview_until(&h, &token, &name, &["ready", "failed"]).await;
    assert_eq!(ready["status"], "ready", "{ready}");
    h.clock.fetch_add(24 * 3600, Ordering::SeqCst);
    *h.preview_idle.lock().unwrap() = Err("config.json does not parse".to_string());
    let mut skipped = None;
    let first = preview_idle_tick(&h.state, &mut skipped);
    assert_eq!(first.len(), 1, "{first:?}");
    assert!(first[0].contains("config.json does not parse"), "{first:?}");
    assert!(
        preview_idle_tick(&h.state, &mut skipped).is_empty(),
        "said once"
    );
    assert!(answers(ready["port"].as_u64().unwrap()), "still running");
    h.state.previews.stop(&name);
}

/// (#3459) The idle setting is read fresh, through the config gate: absent
/// is 15, `0` is kept, and a misspelled key refuses rather than reading as
/// the default.
#[test]
#[serial_test::serial]
fn clients_preview_idle_minutes_is_read_fresh_and_defaults_to_15() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.json");
    let read = |text: &str| {
        std::fs::write(&path, text).unwrap();
        unsafe { std::env::remove_var("DARKMUX_CLIENTS_PREVIEW_IDLE_MINUTES") };
        read_preview_idle_minutes_at(&path)
    };
    assert_eq!(read("{}"), Ok(15), "absent is 15");
    assert_eq!(
        read(r#"{"clients": {"preview": {"idle_minutes": 30}}}"#),
        Ok(30)
    );
    assert_eq!(
        read(r#"{"clients": {"preview": {"idle_minutes": 0}}}"#),
        Ok(0)
    );
    let e = read(r#"{"clients": {"preview": {"idle_minute": 5}}}"#).unwrap_err();
    assert!(e.contains("clients.preview.idle_minute"), "{e}");
    std::fs::remove_file(&path).unwrap();
    assert_eq!(read_preview_idle_minutes_at(&path), Ok(15), "no file is 15");
}

#[tokio::test]
async fn previews_refuse_what_they_cant_run() {
    let h = harness();
    let token = paired(&h).await;
    let (code, _) = call(
        &h,
        post(
            "/client/workspaces/wt1/preview",
            None,
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(code, 401);
    let (code, v) = start_preview(&h, &token, "wt1").await;
    assert_eq!(code, 404, "no checkout of yours: {v}");
    let (code, _) = start_preview(&h, &token, "nope").await;
    assert_eq!(code, 404);
    let (mine, name, _ws) = checkout_workspace(&h, &token).await;
    let (code, v) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 404, "no recipe in the checkout: {v}");
    recipe(
        &mine,
        serde_json::json!({"command": serve(), "env_from_source": ["../../etc/passwd"]}),
    );
    let (code, v) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 422, "{v}");
    recipe(&mine, serde_json::json!({"command": serve(), "comand": "typo"}));
    let (code, v) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 422, "{v}");
    assert!(v["error"].as_str().unwrap().contains("unknown key `comand`"), "{v}");
    write_recipe(&mine, &serde_json::json!({"command": serve()}).to_string());
    let (code, v) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 422, "no schema_version: {v}");
    let (code, _) = call(
        &h,
        get(
            &format!("/client/workspaces/{name}/preview"),
            &[("authorization", &bearer(&token))],
        ),
    )
    .await;
    assert_eq!(code, 404, "nothing started");
}

/// (#3456) A started preview's container mounts the workspace, never the
/// user's checkout the recipe came from, and serves from it: the page is
/// the workspace's. The shape `docker run` gets is asserted in
/// `client_preview`'s tests.
#[tokio::test]
async fn a_preview_container_mounts_the_workspace_not_the_checkout() {
    let h = harness();
    let token = paired(&h).await;
    let (mine, name, ws) = served_workspace(&h, &token).await;
    recipe(
        &mine,
        serde_json::json!({"command": serve(), "env": {"PORT": "{port}"}}),
    );
    start_preview(&h, &token, &name).await;
    let ready = preview_until(&h, &token, &name, &["ready", "failed"]).await;
    assert_eq!(ready["status"], "ready", "{ready}");
    let specs = h.containers.specs.lock().unwrap().clone();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].workspace, ws.canonicalize().unwrap());
    assert_ne!(specs[0].workspace, mine.canonicalize().unwrap());
    let port = ready["port"].as_u64().unwrap() as u16;
    assert_eq!(
        specs[0].ports,
        [port, ready["hmr_port"].as_u64().unwrap() as u16]
    );
    h.state.previews.stop(&name);
    assert_eq!(
        *h.containers.removed.lock().unwrap(),
        std::slice::from_ref(&specs[0].name)
    );
}

#[tokio::test]
async fn a_new_run_goes_ahead_after_a_committed_run() {
    let h = harness();
    let token = paired(&h).await;
    git_workspace(&h, "site");
    let _first = run_in(&h, &token, "site", "edit-readme").await;
    let (code, v) = call(
        &h,
        post(
            "/client/work",
            Some(&token),
            serde_json::json!({"role": "coder", "message": "edit-other", "workspace": "site"}),
        ),
    )
    .await;
    assert_eq!(
        code, 202,
        "nothing waits uncommitted, so a new run goes ahead: {v}"
    );
}

/// A Tweak may not name a run darkmux still knows that is another app's or
/// ran in another work copy. A run that failed or was cancelled can be
/// continued like a finished one: its edits are committed the same way.
#[tokio::test]
async fn continues_may_not_name_another_apps_run_or_one_in_another_work_copy() {
    let h = harness();
    let token = paired(&h).await;
    let other = paired(&h).await;
    git_workspace(&h, "site");
    git_workspace(&h, "elsewhere");
    // The other app's run in this very work copy, before the operator
    // gave the copy to this app (`client claim`).
    let ids: Vec<String> = h.store.read().unwrap().clients.iter().map(|c| c.id.clone()).collect();
    own(&h, "site", &ids[1]);
    let foreign = run_in(&h, &other, "site", "edit-readme").await;
    own(&h, "site", &ids[0]);
    let first = run_in(&h, &token, "site", "edit-other").await;
    let tweak = |ws: &str, cont: &str| post("/client/work", Some(&token), serde_json::json!({"role": "coder", "message": "edit", "workspace": ws, "continues": cont}));
    for (ws, cont) in [("elsewhere", first.as_str()), ("site", foreign.as_str())] {
        let (code, v) = call(&h, tweak(ws, cont)).await;
        assert_eq!((code, v["code"].as_str()), (409, Some("conflict")), "{ws} {cont}: {v}");
    }
    let failed = run_in(&h, &token, "site", "edit-fail").await;
    assert_eq!(status(&h, &token, &failed).await["status"], "error");
    let (code, v) = call(&h, tweak("site", &failed)).await;
    assert_eq!(code, 202, "a failed run's committed edits can be tweaked: {v}");
}

/// "Tweak it" the next morning, or after `client serve` restarts: a
/// `continues` naming a run darkmux has forgotten is a plain new run in the
/// same work copy, never a refusal.
#[tokio::test]
async fn continues_naming_a_forgotten_run_is_a_plain_new_run() {
    let h = harness();
    let token = paired(&h).await;
    git_workspace(&h, "site");
    let first = run_in(&h, &token, "site", "edit-readme").await;
    h.clock.fetch_add(KEEP_WORK_SECS + 1, Ordering::SeqCst);
    let next_morning = run_after(&h, &token, "site", "edit-other", Some(&first)).await;
    assert_eq!(status(&h, &token, &next_morning).await["status"], "done");
    assert_eq!(h.state.works.lock().unwrap()[&next_morning].continues, None, "an unknown id labels nothing");
    // A restart forgets every run.
    h.state.works.lock().unwrap().clear();
    let after_restart = run_after(&h, &token, "site", "edit-readme-again", Some(&next_morning)).await;
    assert_eq!(status(&h, &token, &after_restart).await["status"], "done");
    let known = run_after(&h, &token, "site", "edit", Some(&after_restart)).await;
    assert_eq!(h.state.works.lock().unwrap()[&known].continues.as_deref(), Some(after_restart.as_str()));
}

/// Finished work is kept for its keep time from when it ended, not from
/// when it started: a long run is still there to poll right after it ends.
#[tokio::test]
async fn ended_work_is_kept_for_the_keep_time_from_when_it_ended() {
    let h = harness();
    let token = paired(&h).await;
    let (code, v) = call(&h, post("/client/work", Some(&token), serde_json::json!({"role": "coder", "message": "wait"}))).await;
    assert_eq!(code, 202, "{v}");
    let long = v["work_id"].as_str().unwrap().to_string();
    h.clock.fetch_add(KEEP_WORK_SECS - 10, Ordering::SeqCst);
    call(&h, post(&format!("/client/work/{long}/cancel"), Some(&token), serde_json::json!({}))).await;
    assert_eq!(wait_done(&h, &token, &long).await["status"], "cancelled");
    h.clock.fetch_add(20, Ordering::SeqCst);
    assert_eq!(poll_after_pruning(&h, &token, &long).await, 200, "ended 20 s ago, so it is kept");
    h.clock.fetch_add(KEEP_WORK_SECS, Ordering::SeqCst);
    assert_eq!(poll_after_pruning(&h, &token, &long).await, 404, "past its keep time from when it ended");
}

/// Polls run `id` after a new work request, which forgets old work first.
async fn poll_after_pruning(h: &Harness, token: &str, id: &str) -> u16 {
    let (code, v) = call(h, post("/client/work", Some(token), serde_json::json!({"role": "coder", "message": "hello"}))).await;
    assert_eq!(code, 202, "{v}");
    let polled = call(h, get(&format!("/client/work/{id}"), &[("authorization", &bearer(token))])).await.0;
    wait_done(h, token, v["work_id"].as_str().unwrap()).await;
    polled
}

#[tokio::test]
async fn a_second_run_cant_start_while_one_is_working_in_the_same_copy() {
    let h = harness();
    let token = paired(&h).await;
    git_workspace(&h, "site");
    let (code, v) = call(
        &h,
        post(
            "/client/work",
            Some(&token),
            serde_json::json!({"role": "coder", "message": "wait", "workspace": "site"}),
        ),
    )
    .await;
    assert_eq!(code, 202, "{v}");
    let busy = v["work_id"].as_str().unwrap().to_string();
    let (code, v) = call(
        &h,
        post(
            "/client/work",
            Some(&token),
            serde_json::json!({"role": "coder", "message": "hello", "workspace": "site"}),
        ),
    )
    .await;
    assert_eq!(code, 409, "{v}");
    call(
        &h,
        post(
            &format!("/client/work/{busy}/cancel"),
            Some(&token),
            serde_json::json!({}),
        ),
    )
    .await;
}

// ─── Changes for an app-opened PR ────────────────────────────────────────

#[tokio::test]
async fn changes_lists_the_branchs_commits_against_origins_default_branch() {
    let h = harness();
    let token = paired(&h).await;
    let upstream = h.dir.path().join("upstream");
    fixture_repo(&upstream);
    std::fs::write(upstream.join("gone.txt"), "to delete\n").unwrap();
    run_git(Some(&upstream), &["add", "."]);
    run_git(Some(&upstream), &["commit", "--quiet", "-m", "add gone"]);
    let wt = h.base.join("site");
    run_git(
        None,
        &[
            "clone",
            "--quiet",
            upstream.to_str().unwrap(),
            wt.to_str().unwrap(),
        ],
    );
    run_git(Some(&wt), &["config", "user.name", "Owner"]);
    run_git(Some(&wt), &["config", "user.email", "owner@example.com"]);
    run_git(Some(&wt), &["checkout", "--quiet", "-b", "mux/site-abc123"]);
    hold(&h, "site");
    let base = run_git(Some(&wt), &["rev-parse", "origin/main"]);
    std::fs::write(wt.join("README.md"), "changed\n").unwrap();
    std::fs::create_dir_all(wt.join("img")).unwrap();
    std::fs::write(wt.join("img/dot.bin"), [0u8, 159, 146, 150]).unwrap();
    std::fs::remove_file(wt.join("gone.txt")).unwrap();
    run_git(Some(&wt), &["add", "-A"]);
    run_git(Some(&wt), &["commit", "--quiet", "-m", "run"]);
    std::fs::write(wt.join("notes.txt"), "uncommitted, not in the PR\n").unwrap();
    std::fs::write(wt.join("README.md"), "an uncommitted edit after the run\n").unwrap();

    let (code, v) = call(
        &h,
        get(
            "/client/workspaces/site/changes",
            &[("authorization", &bearer(&token))],
        ),
    )
    .await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["base"], base.as_str());
    assert_eq!(v["branch"], "mux/site-abc123");
    assert_eq!(v["base_branch"], "main");
    let files = v["files"].as_array().unwrap();
    let by = |p: &str| {
        files
            .iter()
            .find(|f| f["path"] == p)
            .cloned()
            .unwrap_or(Value::Null)
    };
    let decode = |v: &Value| crate::client_workspace::base64_decode(v.as_str().unwrap()).unwrap();
    assert_eq!(decode(&by("README.md")["content"]), b"changed\n");
    assert_eq!(
        decode(&by("img/dot.bin")["content"]),
        vec![0u8, 159, 146, 150]
    );
    assert_eq!(by("gone.txt")["deleted"], true);
    assert_eq!(by("notes.txt"), Value::Null, "only committed changes");
    assert_eq!(files.len(), 3);
}

/// (#3480) A project darkmux made from a template has no origin: its
/// changes are what the branch holds since darkmux's first commit, on the
/// `main` that commit started. A work copy with no origin that isn't one
/// is still refused.
#[tokio::test]
async fn a_template_project_compares_with_its_first_commit() {
    let h = harness();
    let token = paired(&h).await;
    let wt = h.base.join("book");
    fixture_repo(&wt);
    hold(&h, "book");
    run_git(Some(&wt), &["config", "user.name", "Owner"]);
    run_git(Some(&wt), &["config", "user.email", "owner@example.com"]);
    let first = run_git(Some(&wt), &["rev-parse", "HEAD"]);
    std::fs::write(wt.join("README.md"), "a run's change\n").unwrap();
    std::fs::write(wt.join("chapter.md"), "# One\n").unwrap();
    run_git(Some(&wt), &["add", "-A"]);
    run_git(Some(&wt), &["commit", "--quiet", "-m", "run"]);
    let head = run_git(Some(&wt), &["rev-parse", "HEAD"]);
    let auth = bearer(&token);
    let changes = || get("/client/workspaces/book/changes", &[("authorization", &auth)]);

    let (code, v) = call(&h, changes()).await;
    assert_eq!(code, 409, "not recorded as a template project: {v}");

    crate::client_workspace::record_template_commit(&h.base, "book", &first).unwrap();
    let (code, v) = call(&h, changes()).await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["base"], first.as_str());
    assert_eq!(v["base_branch"], "main");
    assert_eq!(v["head"], head.as_str());
    assert_eq!(v["branch"], "main");
    let mut paths: Vec<&str> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    paths.sort();
    assert_eq!(paths, ["README.md", "chapter.md"]);
}

#[tokio::test]
async fn changes_needs_a_token_and_a_workspace_with_a_remote_default() {
    let h = harness();
    let token = paired(&h).await;
    let (code, _) = call(&h, get("/client/workspaces/site/changes", &[])).await;
    assert_eq!(code, 401);
    let (code, _) = call(
        &h,
        get(
            "/client/workspaces/nope/changes",
            &[("authorization", &bearer(&token))],
        ),
    )
    .await;
    assert_eq!(code, 404);
    // wt1 isn't a git checkout with an origin.
    let (code, v) = call(
        &h,
        get(
            "/client/workspaces/wt1/changes",
            &[("authorization", &bearer(&token))],
        ),
    )
    .await;
    assert_eq!(code, 409, "{v}");
}

// ---- preview request logging -------------------------------------------

#[test]
fn preview_log_names_the_workspace_and_outcome() {
    assert_eq!(
        preview_log_line("hcr", "start", "starting (202)"),
        "darkmux client serve: preview hcr: start → starting (202)"
    );
}

#[test]
fn preview_status_is_logged_only_when_it_changes() {
    let seen = PreviewLogSeen::default();
    assert!(seen.changed("hcr-a", "starting"));
    assert!(
        !seen.changed("hcr-a", "starting"),
        "a poll that changed nothing stays quiet"
    );
    assert!(seen.changed("hcr-a", "ready"));
    assert!(seen.changed("hcr-b", "starting"), "per workspace");
}

// ---- GET /client/profiles: what an app may run here, and what's up ---------

#[tokio::test]
async fn profiles_need_a_token() {
    let h = harness();
    let (code, _) = call(&h, get("/client/profiles", &[])).await;
    assert_eq!(code, 401);
}

#[tokio::test]
async fn profiles_unset_policy_lists_every_local_one_the_roles_own_first() {
    let h = harness();
    let t = paired(&h).await;
    let (code, v) = call(
        &h,
        get("/client/profiles", &[("authorization", &bearer(&t))]),
    )
    .await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["role"], "coder");
    let ps = v["profiles"].as_array().unwrap();
    let names: Vec<&str> = ps.iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        ["fast", "deep"],
        "own first, then the other local ones; not hosted, not utility: {v}"
    );
    assert_eq!(ps[0]["model"], "m-fast");
    assert_eq!(ps[0]["endpoint"], "url");
    assert_eq!(ps[0]["reachable"], true);
    assert_eq!(ps[0]["loaded"], true);
}

#[tokio::test]
async fn profiles_follow_the_grant_in_its_order() {
    let h = harness();
    let t = paired(&h).await;
    *h.policy.lock().unwrap() = Ok(Some(vec!["deep".into(), "fast".into(), "gone".into()]));
    let (code, v) = call(
        &h,
        get("/client/profiles", &[("authorization", &bearer(&t))]),
    )
    .await;
    assert_eq!(code, 200, "{v}");
    let names: Vec<&str> = v["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["deep", "fast", "gone"]);
    assert_eq!(v["profiles"][0]["reachable"], false);
    // A granted name the registry doesn't have is listed, unreachable, with why.
    assert_eq!(v["profiles"][2]["reachable"], false);
    assert!(
        v["profiles"][2]["error"]
            .as_str()
            .unwrap()
            .contains("not in the registry"),
        "{v}"
    );
}

#[tokio::test]
async fn profiles_for_another_role() {
    let h = harness();
    let t = paired(&h).await;
    let (code, v) = call(
        &h,
        get(
            "/client/profiles?role=radio-host",
            &[("authorization", &bearer(&t))],
        ),
    )
    .await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["role"], "radio-host");
    assert_eq!(v["profiles"][0]["name"], "deep");
}

/// (review) With the setting unset, the role's own profile is listed only
/// when it runs here: a role bound to a hosted profile sees just the local
/// ones.
#[tokio::test]
async fn profiles_never_list_a_roles_own_profile_that_is_hosted() {
    let h = harness();
    let t = paired(&h).await;
    let (code, v) = call(
        &h,
        get(
            "/client/profiles?role=researcher",
            &[("authorization", &bearer(&t))],
        ),
    )
    .await;
    assert_eq!(code, 200, "{v}");
    let names: Vec<&str> = v["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["deep", "fast"], "{v}");
}

#[tokio::test]
async fn profiles_refuse_an_unknown_role() {
    let h = harness();
    let t = paired(&h).await;
    let (code, _) = call(
        &h,
        get(
            "/client/profiles?role=nope",
            &[("authorization", &bearer(&t))],
        ),
    )
    .await;
    assert_eq!(code, 400);
}

/// The harness's profiles: `fast` is served and loaded; `deep` is declared
/// but its server is down; anything else isn't in the registry.
fn test_profile_info(name: &str) -> Result<ClientProfileInfo, String> {
    match name {
        "fast" => Ok(ClientProfileInfo {
            name: "fast".into(),
            description: Some("Fast coder".into()),
            model: Some("m-fast".into()),
            endpoint: "url".into(),
            reachable: true,
            loaded: Some(true),
            error: None,
            availability: darkmux_profiles::engine::availability::Availability::external(),
        }),
        "deep" => Ok(ClientProfileInfo {
            name: "deep".into(),
            description: None,
            model: Some("m-deep".into()),
            endpoint: "lmstudio".into(),
            reachable: false,
            loaded: Some(false),
            error: None,
            availability: darkmux_profiles::engine::availability::Availability::startable(),
        }),
        other => Err(format!("profile {other} is not in the registry")),
    }
}

// ---- describe_profile, the production profile_info ------------------------

/// Sets env vars for one closure, restoring them after (serial tests only).
fn with_env_vars<T>(vars: &[(&str, &std::ffi::OsStr)], f: impl FnOnce() -> T) -> T {
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

/// A server that answers every request with 401, as a key-guarded model
/// server does: reachable.
fn answering_401() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().take(1) {
            use std::io::{Read, Write};
            let mut s = stream.unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let _ = s.write_all(
                b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            );
        }
    });
    port
}

#[test]
#[serial_test::serial]
fn describe_profile_reads_the_registry_and_probes_its_server() {
    let tmp = tempfile::tempdir().unwrap();
    let up = answering_401();
    // A port nothing listens on: bind, note it, drop it.
    let down = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let registry = tmp.path().join("profiles.json");
    std::fs::write(
        &registry,
        format!(
            r#"{{"profiles":{{
                "quick":{{"description":"Quick coder","models":[{{"id":"flint","n_ctx":32000,"endpoint":"up"}}]}},
                "gone":{{"models":[{{"id":"big","n_ctx":32000,"endpoint":"down"}}]}}
            }},"endpoints":{{
                "up":{{"url":"http://127.0.0.1:{up}/v1"}},
                "down":{{"url":"http://127.0.0.1:{down}/v1"}}
            }}}}"#
        ),
    )
    .unwrap();
    let (quick, gone, missing) =
        with_env_vars(&[("DARKMUX_PROFILES", registry.as_os_str())], || {
            (
                describe_profile("coder", "quick"),
                describe_profile("coder", "gone"),
                describe_profile("coder", "nope"),
            )
        });
    let quick = quick.unwrap();
    assert_eq!(quick.description.as_deref(), Some("Quick coder"));
    assert_eq!(quick.model.as_deref(), Some("flint"));
    assert_eq!(quick.endpoint, "url");
    assert!(quick.reachable, "a 401 means the server is up: {quick:?}");
    let gone = gone.unwrap();
    assert!(!gone.reachable, "nothing listens there: {gone:?}");
    assert!(missing
        .unwrap_err()
        .contains("not in this machine's profiles.json"));
}

/// An executable shell script `name` in `dir` running `body`.
fn script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A freshly written script's first exec can take seconds on a loaded
    // Mac; run it once so a bounded probe measures the probe, not that.
    let _ = std::process::Command::new(&path).arg("--version").output();
    path
}

/// (#3466) Each listed profile carries its availability: a stopped
/// Tensorfold server whose engine is verified and has the model is
/// `startable` (so Auto mode keeps it), LM Studio not answering is
/// `unknown`, and an unmanaged server is `external`. The older `reachable`
/// and `loaded` fields read as before for LM Studio and the URL server.
#[test]
#[serial_test::serial]
fn describe_profiles_reports_each_profiles_availability() {
    let tmp = tempfile::tempdir().unwrap();
    let lms = script(tmp.path(), "lms", "exit 1\n");
    let tf = script(
        tmp.path(),
        "tensorfold-native",
        "case \"$1\" in\n  --version) echo 'tensorfold-native 1.2.4';;\n  \
         capabilities) echo '{\"schema\":1,\"serve\":{\"flags\":{}}}';;\n  \
         models) echo '{\"schema\":1,\"models\":[{\"repo\":\"TF/m\"}]}';;\n  *) exit 9;;\nesac\n",
    );
    let up = answering_401();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let registry = tmp.path().join("profiles.json");
    std::fs::write(
        &registry,
        format!(
            r#"{{"profiles":{{
                "lms":{{"models":[{{"id":"qwen","n_ctx":32000}}]}},
                "tf":{{"models":[{{"id":"TF/m","n_ctx":32000,"endpoint":"tf"}}]}},
                "url":{{"models":[{{"id":"flint","n_ctx":32000,"endpoint":"up"}}]}}
            }},"endpoints":{{
                "tf":{{"managed":"tensorfold","port":{port}}},
                "up":{{"url":"http://127.0.0.1:{up}/v1"}}
            }}}}"#
        ),
    )
    .unwrap();
    let names = ["lms", "tf", "url"].map(String::from);
    let got = with_env_vars(
        &[
            ("DARKMUX_PROFILES", registry.as_os_str()),
            ("DARKMUX_LMS_BIN", lms.as_os_str()),
            ("DARKMUX_TENSORFOLD_BIN", tf.as_os_str()),
        ],
        || describe_profiles("coder", &names),
    );
    let [lms, tf, url] = [0, 1, 2].map(|i| got[i].clone().unwrap());
    use darkmux_profiles::engine::availability::AvailabilityState as S;
    assert_eq!(lms.availability.state, S::Unknown, "{lms:?}");
    assert!(lms.availability.reason.as_deref().unwrap().contains("LM Studio did not answer"), "{lms:?}");
    assert_eq!((lms.reachable, lms.loaded), (false, None), "LM Studio's older fields as before");
    assert_eq!(tf.availability.state, S::Startable, "{tf:?}");
    assert_eq!(tf.endpoint, "tensorfold");
    assert_eq!((tf.reachable, tf.loaded), (true, Some(false)), "a Tensorfold profile's older fields follow its availability");
    assert_eq!(url.availability.state, S::External, "{url:?}");
    assert!(url.reachable, "the URL server is still probed: {url:?}");
}

/// (#3466 review) An engine-server profile is `reachable` only when darkmux
/// can make it resident now (`resident` or `startable`): an app that picks
/// "the first that answers" must not pick one whose dispatch would fail
/// `engine_unavailable`. An unverified Tensorfold, with LM Studio down too.
#[test]
#[serial_test::serial]
fn an_unavailable_engine_server_is_not_reachable() {
    let tmp = tempfile::tempdir().unwrap();
    let lms = script(tmp.path(), "lms", "exit 1\n");
    let tf = script(
        tmp.path(),
        "tensorfold-native",
        "case \"$1\" in\n  --version) echo 'tensorfold-native 1.3.0';;\n  \
         capabilities) echo '{\"schema\":1,\"serve\":{\"flags\":{}}}';;\n  \
         models) echo '{\"schema\":1,\"models\":[{\"repo\":\"TF/m\"}]}';;\n  *) exit 9;;\nesac\n",
    );
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let registry = tmp.path().join("profiles.json");
    std::fs::write(
        &registry,
        format!(r#"{{"profiles":{{"tf":{{"models":[{{"id":"TF/m","n_ctx":32000,"endpoint":"tf"}}]}}}},
            "endpoints":{{"tf":{{"managed":"tensorfold","port":{port}}}}}}}"#),
    )
    .unwrap();
    let got = with_env_vars(
        &[
            ("DARKMUX_PROFILES", registry.as_os_str()),
            ("DARKMUX_LMS_BIN", lms.as_os_str()),
            ("DARKMUX_TENSORFOLD_BIN", tf.as_os_str()),
        ],
        || describe_profiles("coder", &["tf".to_string()]),
    );
    let tf = got[0].clone().unwrap();
    use darkmux_profiles::engine::availability::AvailabilityState as S;
    assert_eq!(tf.availability.state, S::Unavailable, "{tf:?}");
    assert_eq!((tf.reachable, tf.loaded), (false, Some(false)), "{tf:?}");
}

/// (#3466 review) One rule for every managed engine: an LM Studio profile is
/// `reachable` only when darkmux can make it resident now. LM Studio
/// answering is not enough when the model is not downloaded (its dispatch
/// would fail); a downloaded one is.
#[test]
#[serial_test::serial]
fn an_lm_studio_model_that_is_not_downloaded_is_not_reachable() {
    let tmp = tempfile::tempdir().unwrap();
    let lms = script(
        tmp.path(),
        "lms",
        "case \"$1\" in\n  ps) echo '[]';;\n  ls) echo '[{\"modelKey\":\"have\",\"type\":\"llm\"}]';;\n  *) exit 1;;\nesac\n",
    );
    let registry = tmp.path().join("profiles.json");
    std::fs::write(
        &registry,
        r#"{"profiles":{"have":{"models":[{"id":"have","n_ctx":8192}]},"missing":{"models":[{"id":"missing","n_ctx":8192}]}}}"#,
    )
    .unwrap();
    let got = with_env_vars(
        &[("DARKMUX_PROFILES", registry.as_os_str()), ("DARKMUX_LMS_BIN", lms.as_os_str())],
        || describe_profiles("coder", &["have".to_string(), "missing".to_string()]),
    );
    let [have, missing] = [0, 1].map(|i| got[i].clone().unwrap());
    use darkmux_profiles::engine::availability::AvailabilityState as S;
    assert_eq!(have.availability.state, S::Startable, "{have:?}");
    assert_eq!((have.reachable, have.loaded), (true, Some(false)), "{have:?}");
    assert_eq!(missing.availability.state, S::Unavailable, "{missing:?}");
    assert_eq!((missing.reachable, missing.loaded), (false, Some(false)), "LM Studio answered, but the model is not there: {missing:?}");
}

/// (#3466 review) The client listing reads the dispatch preflight: a role
/// file with a key its schema does not know refuses every dispatch, so
/// every listed profile is `unavailable` (and not reachable).
#[test]
#[serial_test::serial]
fn a_dispatch_the_preflight_refuses_lists_unavailable() {
    let state = darkmux_types::test_isolation::IsolatedState::new();
    std::fs::create_dir_all(state.join("roles")).unwrap();
    std::fs::write(state.join("roles/coder.json"), r#"{"id":"coder","zzz_bogus":1}"#).unwrap();
    let registry = state.join("profiles.json");
    std::fs::write(&registry, r#"{"profiles":{"lms":{"models":[{"id":"qwen","n_ctx":8192}]}}}"#).unwrap();
    let got = with_env_vars(
        &[("DARKMUX_PROFILES", registry.as_os_str()), ("DARKMUX_LMS_BIN", std::ffi::OsStr::new("/nonexistent/lms"))],
        || describe_profiles("coder", &["lms".to_string()]),
    );
    let lms = got[0].clone().unwrap();
    use darkmux_profiles::engine::availability::AvailabilityState as S;
    assert_eq!(lms.availability.state, S::Unavailable, "{lms:?}");
    assert!(lms.availability.reason.as_deref().unwrap().contains("zzz_bogus"), "{lms:?}");
    assert!(!lms.reachable);
}

/// (review) Whether a role's work on a profile stays on this machine is the
/// resolved target's own classification, the one dispatch routes on
/// (`brain_leaves_this_machine`): LM Studio stays; an unmanaged endpoint stays
/// only when it is the machine's `default_engine` reached on loopback. A URL
/// whose userinfo says `localhost` but whose host is elsewhere is hosted, and
/// an endpoint id the registry does not define refuses.
#[test]
#[serial_test::serial]
fn a_profile_is_local_only_when_its_selected_model_stays_on_this_machine() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = tmp.path().join("profiles.json");
    std::fs::write(
        &registry,
        r#"{"profiles":{
            "managed":{"models":[{"id":"q","n_ctx":32000}]},
            "engine":{"models":[{"id":"f","n_ctx":32000,"endpoint":"mlx"}]},
            "loop6":{"models":[{"id":"f","n_ctx":32000,"endpoint":"l6"}]},
            "hosted":{"models":[{"id":"c","n_ctx":32000,"endpoint":"api"}]},
            "userinfo":{"models":[{"id":"c","n_ctx":32000,"endpoint":"sneaky"}]},
            "unresolved":{"models":[{"id":"c","n_ctx":32000,"endpoint":"nope"}]}
        },"endpoints":{
            "mlx":{"url":"http://127.0.0.1:11500/v1"},
            "l6":{"url":"http://[::1]:8090/v1"},
            "api":{"url":"https://api.example.com/v1"},
            "sneaky":{"url":"https://localhost:x@api.example.com/v1"}
        }}"#,
    )
    .unwrap();
    let got = with_env_vars(
        &[
            ("DARKMUX_PROFILES", registry.as_os_str()),
            ("DARKMUX_DEFAULT_ENGINE", std::ffi::OsStr::new("mlx")),
        ],
        || {
            ["managed", "engine", "loop6", "hosted", "userinfo", "unresolved", "absent"]
                .map(|n| work_stays_here("coder", n))
        },
    );
    let [managed, engine, loop6, hosted, userinfo, unresolved, absent] = got;
    assert_eq!(managed, Ok(true), "LM Studio stays");
    assert_eq!(engine, Ok(true), "the default engine on loopback stays");
    assert_eq!(loop6, Ok(false), "a loopback server that is not the default engine is not this machine's");
    assert_eq!(hosted, Ok(false));
    assert_eq!(userinfo, Ok(false), "the host is api.example.com, whatever the userinfo says");
    assert!(unresolved.as_ref().is_err_and(|e| e.contains("nope")), "{unresolved:?}");
    assert!(absent.as_ref().is_err_and(|e| e.contains("not in this machine's profiles.json")), "{absent:?}");
}

#[tokio::test]
async fn profiles_name_the_machine_they_run_on() {
    let h = harness();
    let t = paired(&h).await;
    let (code, v) = call(
        &h,
        get("/client/profiles", &[("authorization", &bearer(&t))]),
    )
    .await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(
        v["machine"],
        serde_json::json!({"id": "m-0123456789abcdef", "name": "studio"})
    );
}

/// (#3466) On the wire each profile's availability is `{state, reason}`,
/// beside the older `reachable` and `loaded`; an undescribable one is
/// `unknown`, its reason the error.
#[tokio::test]
async fn profiles_carry_their_availability_on_the_wire() {
    let h = harness();
    let t = paired(&h).await;
    let (code, v) = call(&h, get("/client/profiles?role=coder", &[("authorization", &bearer(&t))])).await;
    assert_eq!(code, 200, "{v}");
    let by = |n: &str| v["profiles"].as_array().unwrap().iter().find(|p| p["name"] == n).cloned();
    let fast = by("fast").unwrap_or_else(|| panic!("{v}"));
    assert_eq!(fast["availability"], serde_json::json!({"state": "external", "reason": null}));
    assert_eq!(fast["reachable"], true, "the older field stays");
    assert_eq!(by("deep").unwrap()["availability"]["state"], "startable");
    let unknown = ClientProfileInfo::unknown("gone", "profile gone is not in the registry".into());
    let wire = serde_json::to_value(&unknown).unwrap();
    assert_eq!(wire["availability"], serde_json::json!({"state": "unknown", "reason": "profile gone is not in the registry"}));
}

#[test]
fn the_machine_id_is_stable_and_never_the_raw_hardware_uuid() {
    let a = ClientMachine::from_parts(Some("00000000-0000-4000-8000-ABCDEF000001"), "MacBook-Pro");
    let b = ClientMachine::from_parts(Some("00000000-0000-4000-8000-ABCDEF000001"), "renamed");
    assert_eq!(a.id, b.id, "a rename keeps the id");
    assert!(a.id.starts_with("m-") && a.id.len() == 18, "{}", a.id);
    assert!(
        !a.id.to_lowercase().contains("abcdef000001"),
        "a hash, not the uuid"
    );
    assert_eq!(a.name, "MacBook-Pro");
    let other =
        ClientMachine::from_parts(Some("00000000-0000-4000-8000-ABCDEF000002"), "MacBook-Pro");
    assert_ne!(a.id, other.id);
    // No hardware uuid (some platforms): still an id, from the name.
    let unknown = ClientMachine::from_parts(None, "box");
    assert!(unknown.id.starts_with("m-"));
}

/// A model server that takes the connection and never answers: a probe of
/// it waits out its whole timeout.
fn never_answering() -> (std::net::TcpListener, u16) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

#[test]
#[serial_test::serial]
fn describing_many_profiles_looks_at_each_server_once() {
    let tmp = tempfile::tempdir().unwrap();
    // Three servers that hang, two profiles on each: looked at one by one
    // that's six 1 s probes, past an app's 5 s wait (three, even probing
    // each server once in turn); all at the same time, it's about one.
    let (_a, pa) = never_answering();
    let (_b, pb) = never_answering();
    let (_c, pc) = never_answering();
    let profiles: Vec<String> = (0..6)
        .map(|i| {
            format!(
                r#""p{i}":{{"models":[{{"id":"m{i}","n_ctx":32000,"endpoint":"{}"}}]}}"#,
                ["a", "b", "c"][i % 3]
            )
        })
        .collect();
    let registry = tmp.path().join("profiles.json");
    std::fs::write(
        &registry,
        format!(
            r#"{{"profiles":{{{}}},"endpoints":{{
                "a":{{"url":"http://127.0.0.1:{pa}/v1"}},
                "b":{{"url":"http://127.0.0.1:{pb}/v1"}},
                "c":{{"url":"http://127.0.0.1:{pc}/v1"}}
            }}}}"#,
            profiles.join(",")
        ),
    )
    .unwrap();
    let mut names: Vec<String> = (0..6).map(|i| format!("p{i}")).collect();
    names.push("nope".into());
    let started = std::time::Instant::now();
    let got = with_env_vars(&[("DARKMUX_PROFILES", registry.as_os_str())], || {
        describe_profiles("coder", &names)
    });
    let took = started.elapsed();
    assert!(
        took < std::time::Duration::from_millis(2500),
        "took {took:?}"
    );
    assert_eq!(got.len(), 7, "one answer per name, in order");
    for (i, info) in got.iter().take(6).enumerate() {
        let info = info.as_ref().unwrap();
        assert_eq!(info.name, format!("p{i}"));
        assert_eq!(info.model.as_deref(), Some(format!("m{i}").as_str()));
        assert!(!info.reachable, "it never answered: {info:?}");
    }
    assert!(got[6]
        .as_ref()
        .unwrap_err()
        .contains("not in this machine's profiles.json"));
}

// ---- template workspaces (a new project from a template) -------------------

#[tokio::test]
async fn a_template_becomes_a_fresh_local_project_named_for_the_bot() {
    let h = harness();
    let token = paired(&h).await;
    let (code, v) = call(
        &h,
        post(
            "/client/workspaces",
            Some(&token),
            serde_json::json!({"repo_url": HCR, "ref": "dev", "template": true, "bot": "Bakery"}),
        ),
    )
    .await;
    assert_eq!(code, 201, "{v}");
    assert_eq!(v["workspace"], "bakery");
    assert_eq!(v["branch"], "main", "a fresh repo on main, runs commit there");
    assert_eq!(v["repo"], Value::Null, "no remote: it's local until it goes to GitHub");
    assert_eq!(v["from"], "github.com/kstrat2001/hcr@dev");
    let dir = h.base.join("bakery");
    assert!(dir.join("README.md").exists());
    assert_eq!(run_git(Some(&dir), &["rev-list", "--count", "HEAD"]).trim(), "1", "none of the template's history");
    assert_eq!(
        run_git(Some(&dir), &["log", "-1", "--format=%s"]).trim(),
        "Start from github.com/kstrat2001/hcr@dev"
    );
    assert!(run_git(Some(&dir), &["remote"]).trim().is_empty());
    assert!(run_git(Some(&dir), &["status", "--porcelain"]).trim().is_empty(), "everything is in the first commit");
    assert!(!h.base.join(".bakery.partial").exists());
}

/// `req` through the router with `state` in place of the harness's, sent
/// as this machine's app sends it.
async fn call_with(state: ClientServerState, mut req: Request<Body>) -> (u16, Value) {
    req.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 5555))));
    req.headers_mut().insert(axum::http::header::HOST, BOUND.parse().unwrap());
    let resp = router(state).oneshot(req).await.unwrap();
    let code = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (code, serde_json::from_slice(&bytes).unwrap())
}

/// With no worktrees base to put it in, neither a clone nor a template
/// project is made: an internal error, and nothing cloned.
#[tokio::test]
async fn no_worktrees_base_makes_no_workspace() {
    let h = harness();
    let token = paired(&h).await;
    let state = ClientServerState { worktrees_base: Arc::new(|| Err("no base".to_string())), ..h.state.clone() };
    for body in [
        serde_json::json!({"repo_url": HCR}),
        serde_json::json!({"repo_url": HCR, "template": true, "bot": "Bakery"}),
    ] {
        let (code, v) = call_with(state.clone(), post("/client/workspaces", Some(&token), body.clone())).await;
        assert_eq!((code, v["code"].as_str()), (500, Some("internal")), "{body}: {v}");
    }
    assert!(h.cloned.lock().unwrap().is_empty(), "nothing was cloned");
}

/// A template whose files git cannot read cannot become a project: the
/// start fails and leaves nothing behind (no folder, no repository, no
/// records), so the name is free again.
#[tokio::test]
async fn a_template_that_cannot_be_committed_leaves_nothing_behind() {
    use std::os::unix::fs::PermissionsExt;
    let h = harness();
    let token = paired(&h).await;
    let clone = h.state.clone_repo.clone();
    let state = ClientServerState {
        clone_repo: Arc::new(move |url, r, dest, gd| {
            clone(url, r, dest, gd)?;
            std::fs::set_permissions(dest.join("README.md"), std::fs::Permissions::from_mode(0o000)).unwrap();
            Ok(())
        }),
        ..h.state.clone()
    };
    let body = serde_json::json!({"repo_url": HCR, "template": true, "bot": "Bakery"});
    let (code, v) = call_with(state, post("/client/workspaces", Some(&token), body)).await;
    assert_eq!((code, v["code"].as_str()), (500, Some("internal")), "{v}");
    let left: Vec<String> = std::fs::read_dir(&h.base)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("bakery"))
        .collect();
    assert!(left.is_empty(), "left behind: {left:?}");
}

/// Host git touches a workdir only as a workspace directly under the
/// worktrees base whose repository darkmux recorded: never with no base,
/// never a folder deeper down, never one with no record.
#[tokio::test]
async fn host_git_touches_only_a_recorded_workspace_directly_under_the_base() {
    let h = harness();
    let token = paired(&h).await;
    let (code, v) = call(&h, post("/client/workspaces", Some(&token), serde_json::json!({"repo_url": HCR}))).await;
    assert_eq!(code, 201, "{v}");
    let base = h.base.canonicalize().unwrap();
    let hcr = base.join("hcr");
    assert_eq!(work_copy_for(&h.state, &hcr).map(|w| w.work_tree), Some(hcr.clone()), "the recorded workspace");
    std::fs::create_dir_all(hcr.join("deeper")).unwrap();
    assert!(work_copy_for(&h.state, &hcr.join("deeper")).is_none(), "a folder inside it");
    std::fs::create_dir_all(base.join("unrecorded")).unwrap();
    assert!(work_copy_for(&h.state, &base.join("unrecorded")).is_none(), "no record");
    let no_base = ClientServerState { worktrees_base: Arc::new(|| Err("no base".to_string())), ..h.state.clone() };
    assert!(work_copy_for(&no_base, &hcr).is_none(), "no base");
    assert!(work_copy_for(&h.state, Path::new("/")).is_none(), "no name");
}

/// A fresh project's first commit carries darkmux's own identity when the
/// machine has none, and everything in the project.
#[test]
#[serial_test::serial]
fn a_first_commit_on_a_machine_with_no_git_identity_is_darkmuxs() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (w, g) = (root.join("w"), root.join(".w.git"));
    run_git(None, &["init", "-q", "--separate-git-dir", g.to_str().unwrap(), w.to_str().unwrap()]);
    std::fs::write(w.join("page.txt"), "hi\n").unwrap();
    let wd = WorkCopy { work_tree: w.clone(), git_dir: g.clone() };
    let empty = root.join("empty-global");
    std::fs::write(&empty, "").unwrap();
    let sha = with_env_vars(&[("GIT_CONFIG_GLOBAL", empty.as_os_str())], || first_commit(&wd, "Start")).unwrap();
    let gd = format!("--git-dir={}", g.display());
    assert_eq!(run_git(None, &[&gd, "log", "-1", "--format=%an <%ae>", &sha]), "darkmux <darkmux@localhost>");
    assert_eq!(run_git(None, &[&gd, "show", "--name-only", "--format=", &sha]), "page.txt");
}

/// A project made from the fixture as a template, after committing `with`
/// (a recipe, and a page to serve) to the fixture; its name and directory.
async fn template_project(h: &Harness, token: &str, with: Option<Value>) -> (String, PathBuf) {
    if let Some(json) = with {
        recipe(&h.fixture, json);
        std::fs::write(h.fixture.join("index.html"), "from the template").unwrap();
        run_git(Some(&h.fixture), &["add", "-A"]);
        run_git(Some(&h.fixture), &["commit", "--quiet", "-m", "recipe"]);
    }
    let body = serde_json::json!({"repo_url": HCR, "template": true, "bot": "Bakery"});
    let (code, v) = call(h, post("/client/workspaces", Some(token), body)).await;
    assert_eq!(code, 201, "{v}");
    let name = v["workspace"].as_str().unwrap().to_string();
    let dir = h.base.join(&name);
    (name, dir)
}

/// (#3460) A project made from a template previews from the recipe in its
/// first commit, darkmux's own, recorded beside the workspace: `GET
/// /client/recipe` and a start both use it, and an edit to the recipe in
/// the workspace (uncommitted, then committed by a run) changes nothing.
#[tokio::test]
async fn a_template_project_previews_from_its_first_commits_recipe() {
    let h = harness();
    let token = paired(&h).await;
    let json = serde_json::json!({"command": serve(), "env": {"PORT": "{port}"}});
    let (name, ws) = template_project(&h, &token, Some(json)).await;
    let base = h.base.canonicalize().unwrap();
    let recorded = std::fs::read_to_string(base.join(format!(".{name}.template"))).unwrap();
    assert_eq!(
        recorded.trim(),
        run_git(Some(&ws), &["rev-parse", "HEAD"]),
        "the first commit, beside the workspace"
    );
    let auth = bearer(&token);
    let (code, v) = call(
        &h,
        get(
            &format!("/client/recipe?workspace={name}"),
            &[("authorization", &auth)],
        ),
    )
    .await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["recipe"]["command"], serve());
    let hostile = serde_json::json!({"command": format!("touch pwned; {}", serve()), "env": {"PORT": "{port}"}});
    for commit in [false, true] {
        recipe(&ws, hostile.clone());
        if commit {
            run_git(
                Some(&ws),
                &["commit", "--quiet", "-am", "a run edits the recipe"],
            );
        }
        let (code, v) = start_preview(&h, &token, &name).await;
        assert_eq!(code, 202, "{v}");
        let ready = preview_until(&h, &token, &name, &["ready", "failed"]).await;
        assert_eq!(ready["status"], "ready", "{ready}");
        h.state.previews.stop(&name);
        let script = h
            .containers
            .specs
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .script
            .clone();
        assert!(
            !script.contains("pwned") && script.contains(&serve()),
            "committed={commit}: {script}"
        );
        let (_, v) = call(
            &h,
            get(
                &format!("/client/recipe?workspace={name}"),
                &[("authorization", &auth)],
            ),
        )
        .await;
        assert_eq!(v["recipe"]["command"], serve(), "committed={commit}");
    }
    assert!(!ws.join("pwned").exists(), "the workspace's recipe ran");
}

/// (#3460 review) A template whose `.gitignore` (or the user's global
/// excludes) ignores `.darkmux/` still has its recipe in the project's first
/// commit, so it still previews.
#[tokio::test]
async fn an_ignored_recipe_still_reaches_the_first_commit() {
    let h = harness();
    let token = paired(&h).await;
    recipe(
        &h.fixture,
        serde_json::json!({"command": serve(), "env": {"PORT": "{port}"}}),
    );
    std::fs::write(h.fixture.join(".gitignore"), ".darkmux/\n").unwrap();
    run_git(
        Some(&h.fixture),
        &["add", "-f", ".gitignore", ".darkmux/preview.json"],
    );
    run_git(
        Some(&h.fixture),
        &["commit", "--quiet", "-m", "ignored recipe"],
    );
    let (name, ws) = template_project(&h, &token, None).await;
    let listed = run_git(Some(&ws), &["ls-tree", "--name-only", "-r", "HEAD"]);
    assert!(
        listed.lines().any(|l| l == ".darkmux/preview.json"),
        "{listed}"
    );
    let (code, v) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 202, "{v}");
    h.state.previews.stop(&name);
}

/// (#3460) A template recipe that lists `env_from_source` files is refused
/// naming the key (a template project has no checkout of the user's to
/// copy them from), and one without a recipe says why. A clone that takes
/// the name of an old template project inherits nothing.
#[tokio::test]
async fn template_projects_without_a_usable_recipe_say_why() {
    let h = harness();
    let token = paired(&h).await;
    let (bare, _) = template_project(&h, &token, None).await;
    let (code, v) = start_preview(&h, &token, &bare).await;
    assert_eq!(code, 404, "{v}");
    assert!(v["error"].as_str().unwrap().contains("first commit"), "{v}");
    let json = serde_json::json!({"command": serve(), "env_from_source": [".env"]});
    let (name, _) = template_project(&h, &token, Some(json)).await;
    let (code, v) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 422, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("`env_from_source`"),
        "{v}"
    );
    let (code, v) = call(
        &h,
        get(
            &format!("/client/recipe?workspace={name}"),
            &[("authorization", &bearer(&token))],
        ),
    )
    .await;
    assert_eq!(code, 422, "{v}");
    // A stale template record under the name a clone then takes.
    let base = h.base.canonicalize().unwrap();
    let first = std::fs::read_to_string(base.join(format!(".{name}.template"))).unwrap();
    std::fs::write(base.join(".hcr.template"), first).unwrap();
    let (code, v) = call(
        &h,
        post(
            "/client/workspaces",
            Some(&token),
            serde_json::json!({"repo_url": HCR}),
        ),
    )
    .await;
    assert_eq!(code, 201, "{v}");
    assert!(
        !base.join(".hcr.template").exists(),
        "the clone forgot the old record"
    );
    let (code, v) = start_preview(&h, &token, "hcr").await;
    assert_eq!(code, 404, "{v}");
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("not made from a checkout of yours"),
        "{v}"
    );
}

#[tokio::test]
async fn a_template_is_never_reused() {
    let h = harness();
    let token = paired(&h).await;
    let body = serde_json::json!({"repo_url": HCR, "ref": "dev", "template": true, "bot": "Bakery"});
    let (first, a) = call(&h, post("/client/workspaces", Some(&token), body.clone())).await;
    let (second, b) = call(&h, post("/client/workspaces", Some(&token), body)).await;
    assert_eq!((first, second), (201, 201), "{a} {b}");
    assert_eq!(b["workspace"], "bakery-2", "a second project, never the first one's files");
    assert_eq!(h.cloned.lock().unwrap().len(), 2);
    // And a plain clone of the same repo never picks up a template project.
    let (code, c) = call(&h, post("/client/workspaces", Some(&token), serde_json::json!({"repo_url": HCR}))).await;
    assert_eq!(code, 201, "{c}");
    assert_eq!(c["workspace"], "hcr");
}

#[tokio::test]
async fn a_template_needs_a_repo_url_and_nothing_else() {
    let h = harness();
    let token = paired(&h).await;
    for body in [
        serde_json::json!({"template": true, "bot": "Bakery"}),
        serde_json::json!({"source_path": "/tmp/x", "template": true}),
    ] {
        let (code, v) = call(&h, post("/client/workspaces", Some(&token), body.clone())).await;
        assert_eq!(code, 400, "{body}: {v}");
    }
    assert!(h.cloned.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_failed_template_clone_leaves_nothing_behind() {
    let h = harness();
    let token = paired(&h).await;
    let (code, v) = call(
        &h,
        post(
            "/client/workspaces",
            Some(&token),
            serde_json::json!({"repo_url": "https://github.com/kstrat2001/fail", "template": true, "bot": "Bakery"}),
        ),
    )
    .await;
    assert_eq!(code, 502, "{v}");
    assert!(!h.base.join("bakery").exists() && !h.base.join(".bakery.partial").exists());
}

// ---- refusal codes (#3468) -------------------------------------------------

/// (#3468) Every refusal body carries a `code` beside its sentence: the
/// generic refusals of the request itself each have theirs.
#[tokio::test]
async fn every_refusal_names_its_code() {
    let h = harness();
    let token = paired(&h).await;
    let code_of = |v: &Value| v["code"].as_str().map(str::to_string);
    let (_, v) = call(&h, get("/client/nope", &[])).await;
    assert_eq!(code_of(&v).as_deref(), Some("not_found"), "{v}");
    let (status, v) = call(&h, get("/client/whoami", &[])).await;
    assert_eq!((status, code_of(&v).as_deref()), (401, Some("unauthorized")), "{v}");
    let (status, v) = call(&h, get("/client/whoami", &[("authorization", &bearer(&token)), ("origin", "https://evil.example")])).await;
    assert_eq!((status, code_of(&v).as_deref()), (403, Some("not_local")), "{v}");
    let (status, v) = call(&h, post("/client/work", Some(&token), serde_json::json!({"role": "coder", "message": ""}))).await;
    assert_eq!((status, code_of(&v).as_deref()), (400, Some("bad_request")), "{v}");
    let (status, v) = call(&h, get("/client/work/w-nope", &[("authorization", &bearer(&token))])).await;
    assert_eq!((status, code_of(&v).as_deref()), (404, Some("not_found")), "{v}");
}

/// (#3468) A failed work item carries its code: the one its run's
/// `dispatch.error` named (`busy` here), else `dispatch_failed`.
#[tokio::test]
async fn a_failed_work_item_carries_its_dispatch_code() {
    let h = harness();
    let token = paired(&h).await;
    let (_, v) = call(&h, post("/client/work", Some(&token), serde_json::json!({"role": "coder", "message": "refused-busy"}))).await;
    let id = v["work_id"].as_str().unwrap().to_string();
    let v = wait_done(&h, &token, &id).await;
    assert_eq!((v["status"].as_str(), v["code"].as_str()), (Some("error"), Some("busy")), "{v}");
}

/// (#3468) A template project previews from its first commit's recipe
/// (#3460); when that commit has none, the refusal is `template_no_recipe`,
/// told apart from a checkout with none (`no_recipe`).
#[tokio::test]
async fn a_template_whose_first_commit_has_no_recipe_is_template_no_recipe() {
    let h = harness();
    let token = paired(&h).await;
    let (code, v) = call(
        &h,
        post("/client/workspaces", Some(&token), serde_json::json!({"repo_url": HCR, "template": true, "bot": "Bakery"})),
    )
    .await;
    assert_eq!(code, 201, "{v}");
    let (code, v) = start_preview(&h, &token, "bakery").await;
    assert_eq!((code, v["code"].as_str()), (404, Some("template_no_recipe")), "{v}");
}

/// (#3468) A checkout whose own git config defines a filter driver is
/// refused as a workspace with `filter_in_repo_config` (#3457).
#[tokio::test]
async fn a_checkout_with_a_repo_filter_is_refused_with_its_code() {
    let h = harness();
    let token = paired(&h).await;
    let mine = h.dir.path().join("crypt");
    fixture_repo(&mine);
    run_git(Some(&mine), &["config", "filter.crypt.clean", "cat"]);
    let (code, v) = call(&h, post("/client/workspaces", Some(&token), serde_json::json!({"source_path": mine.to_str(), "bot": "c"}))).await;
    assert_eq!((code, v["code"].as_str()), (422, Some("filter_in_repo_config")), "{v}");
}

/// (#3468) The production reader of a failed run's code: its dispatch
/// session's `dispatch.error` `code`, read from the flow day files; another
/// work's is not this one's, and a terminal with none names none.
#[test]
fn the_failed_run_code_reader_reads_the_dispatch_error() {
    let dir = tempfile::tempdir().unwrap();
    let day = darkmux_flow::day_utc_now();
    let rec = |session: &str, action: &str, payload: Value| {
        serde_json::json!({"ts": darkmux_flow::ts_utc_now(), "action": action, "session_id": session,
            "execution_id": format!("exec-{session}"), "payload": payload})
    };
    let lines = [
        rec("run-x.adhoc.coder.w-1", "dispatch.start", serde_json::json!({})),
        rec("run-x.adhoc.coder.w-1", "dispatch.error", serde_json::json!({"error": "no fit", "code": "wont_fit"})),
        // Only a `dispatch.error` names the failure, whatever else a record carries.
        rec("run-x.adhoc.coder.w-1", "dispatch.model.loading", serde_json::json!({"engine": "e", "endpoint": "p", "model": "m", "code": "not_a_failure"})),
        rec("run-y.adhoc.coder.w-2", "dispatch.error", serde_json::json!({"error": "held", "code": "busy"})),
        rec("run-z.adhoc.coder.w-3", "dispatch.error", serde_json::json!({"error": "crashed"})),
    ];
    std::fs::write(dir.path().join(format!("{day}.jsonl")), lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
    let store = crate::flow_window::FlowRecordStore::new(crate::client_pairing::feeds_work_activity);
    let read = |id: &str| crate::client_pairing::dispatch_error_code_in(&store, dir.path(), "coder", id);
    assert_eq!(read("w-1").as_deref(), Some("wont_fit"));
    // (#3468 review) A role id escaped on the wire is found too.
    let escaped = darkmux_types::session_id::SessionId::adhoc(
        darkmux_types::session_id::RunId::mission("dispatch-code-1-a-0").unwrap(),
        "code_writer",
        "w-5",
    )
    .wire();
    std::fs::OpenOptions::new()
        .append(true)
        .open(dir.path().join(format!("{day}.jsonl")))
        .and_then(|mut f| {
            use std::io::Write;
            writeln!(f, "{}", rec(&escaped, "dispatch.error", serde_json::json!({"error": "held", "code": "busy"})))
        })
        .unwrap();
    assert_eq!(
        crate::client_pairing::dispatch_error_code_in(&store, dir.path(), "code_writer", "w-5").as_deref(),
        Some("busy")
    );
    assert_eq!(read("w-3"), None, "a terminal with no code names none");
    assert_eq!(read("w-9"), None, "no record");
}

/// (#3468 review) A path segment the router cannot read (not UTF-8) is
/// refused as JSON with a code, like every other refusal, never axum's
/// plain-text rejection.
#[tokio::test]
async fn an_unreadable_path_segment_is_a_coded_json_refusal() {
    let h = harness();
    let token = paired(&h).await;
    for req in [
        get("/client/work/%FF", &[("authorization", &bearer(&token))]),
        get("/client/pair/%FF", &[("x-pair-secret", SECRET)]),
        post("/client/workspaces/%FF/preview", Some(&token), serde_json::json!({})),
    ] {
        let (code, v) = call(&h, req).await;
        assert_eq!((code, v["code"].as_str()), (400, Some("bad_request")), "{v}");
        assert!(v["error"].is_string(), "{v}");
    }
}

/// (#3468 review) A handler that panics outside `spawn_blocking` answers a
/// coded 500 instead of dropping the connection.
#[tokio::test]
async fn a_panicking_handler_is_a_coded_500() {
    let mut h = harness();
    let token = paired(&h).await;
    h.state.profiles_policy = Arc::new(|| panic!("the policy read panicked"));
    let (code, v) = call(&h, get("/client/whoami", &[("authorization", &bearer(&token))])).await;
    assert_eq!((code, v["code"].as_str()), (500, Some("internal")), "{v}");
}

/// (#3468 review) Every client route takes its path parameters through
/// `ClientPath`, so none answers axum's plain-text rejection.
#[test]
fn every_client_route_reads_its_path_through_client_path() {
    let src = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/client_pairing.rs")).unwrap();
    let bare: Vec<&str> = src.lines().filter(|l| l.contains(": UrlPath<") || l.contains("): UrlPath<")).collect();
    assert!(bare.is_empty(), "a handler takes a bare `UrlPath`: {bare:?}");
    assert!(src.matches(": ClientPath<").count() >= 10, "the routes were found");
}

// ─── Production wiring ───────────────────────────────────────────────────

/// What a work request may run on is the profile `darkmux dispatch` would
/// resolve: the profile asked for, else the role's own, never another than
/// the one asked for and never a utility-only one; an unresolved one says
/// why.
#[test]
fn the_profile_verdict_follows_dispatchs_resolution() {
    use darkmux_fleet::{ProfileResolution as R, WorkSeat};
    let work = |p: &str| R::Work { profile: p.into(), seat: WorkSeat::Local { model: "m".into() } };
    assert_eq!(profile_verdict(None, work("fast")), Ok("fast".to_string()));
    assert_eq!(profile_verdict(Some("fast"), work("fast")), Ok("fast".to_string()));
    assert_eq!(
        profile_verdict(Some("fast"), work("deep")),
        Err("profile fast resolves to deep here; refusing".to_string())
    );
    assert_eq!(
        profile_verdict(Some("utility"), R::UtilityOnly("utility".into())),
        Err("profile utility holds only the utility model; work never runs on it".to_string())
    );
    assert_eq!(profile_verdict(None, R::Unresolved("no profile".into())), Err("no profile".to_string()));
}

/// The worktrees base is made when missing and handed back canonical; one
/// that cannot be made says which path and why.
#[test]
fn the_worktrees_base_is_made_and_canonical() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("a/../worktrees");
    assert_eq!(canonical_base(&base).unwrap(), dir.path().canonicalize().unwrap().join("worktrees"));
    assert!(dir.path().join("worktrees").is_dir());
    std::fs::write(dir.path().join("file"), "").unwrap();
    let refused = canonical_base(&dir.path().join("file/worktrees")).unwrap_err();
    assert!(refused.contains("file/worktrees"), "{refused}");
}

/// The production peer lookup names this process at both ends of a real
/// loopback connection, answers only the peers asked about, and says
/// nothing before the listener is bound.
#[test]
fn the_production_peer_lookup_answers_the_peers_asked_about() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let local = listener.local_addr().unwrap();
    let clients = [std::net::TcpStream::connect(local).unwrap(), std::net::TcpStream::connect(local).unwrap()];
    let _accepted = [listener.accept().unwrap(), listener.accept().unwrap()];
    let asked = clients[0].local_addr().unwrap();
    let found = peer_processes_here(Some(local), &[asked]).unwrap();
    assert_eq!(found, HashMap::from([(asked, std::process::id())]));
    assert_eq!(peer_processes_here(None, &[asked]), None);
}

/// A store whose file cannot be opened and locked (a folder sits where it
/// should be) refuses every read and change.
#[test]
fn a_store_that_cannot_be_locked_refuses() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("clients.json")).unwrap();
    let store = ClientStore::at(dir.path().join("clients.json"));
    assert!(store.read().is_err());
    assert!(store.update(|f| { f.legacy_owner = Some("x".into()); Ok(()) }).is_err());
    assert!(dir.path().join("clients.json").is_dir(), "nothing written");
}

// ─── Runs and previews end when they should ──────────────────────────────

/// How one work item stands, read from the listener's own table (a revoked
/// client can no longer ask).
fn work_word(h: &Harness, id: &str) -> &'static str {
    match h.state.works.lock().unwrap().get(id).map(|w| w.status.clone()) {
        Some(WorkStatus::Running) => "running",
        Some(WorkStatus::Done { .. }) => "done",
        Some(WorkStatus::Error(_)) => "error",
        Some(WorkStatus::Cancelled(_)) => "cancelled",
        None => "unknown",
    }
}

/// Waits (up to 5 s) for work `id` to stand at `word`.
async fn work_reaches(h: &Harness, id: &str, word: &str) -> bool {
    for _ in 0..500 {
        if work_word(h, id) == word {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    false
}

/// An admission that takes `ms` (its role check reads the disk).
fn slow_admission(h: &mut Harness, ms: u64) {
    h.state.role_known = Arc::new(move |_role| {
        std::thread::sleep(std::time::Duration::from_millis(ms));
        Ok(true)
    });
}

/// An app that hangs up while its work request is being admitted never
/// leaves a run registered with nothing running it: the run it asked for
/// runs to its end, so its work copy and its run slot come free again.
#[tokio::test]
async fn a_disconnect_during_admission_never_leaves_a_run_with_no_runner() {
    let mut h = harness();
    let token = paired(&h).await;
    slow_admission(&mut h, 400);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    h.state.bound = Some(addr);
    let app = router(h.state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await
    });
    let body = work_for("coder").to_string();
    let request = format!(
        "POST /client/work HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut socket = std::net::TcpStream::connect(addr).unwrap();
    std::io::Write::write_all(&mut socket, request.as_bytes()).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    drop(socket);
    // Admission ends at 400 ms; the run itself is instant.
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    let ids: Vec<String> = h.state.works.lock().unwrap().keys().cloned().collect();
    assert_eq!(ids.len(), 1, "the request was admitted");
    assert_eq!(h.ran.lock().unwrap().len(), 1, "its run has a runner");
    assert!(work_reaches(&h, &ids[0], "done").await, "it stands at {}", work_word(&h, &ids[0]));
    server.abort();
}

/// A work request still being admitted when `client serve` begins to stop
/// is refused and registers nothing: the stop looks at the running work
/// once, and nothing joins it afterward.
#[tokio::test]
async fn work_still_being_admitted_when_the_stop_begins_is_refused() {
    let mut h = harness();
    let token = paired(&h).await;
    slow_admission(&mut h, 300);
    let mut req = post("/client/work", Some(&token), work_for("coder"));
    req.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 5555))));
    req.headers_mut().insert(axum::http::header::HOST, BOUND.parse().unwrap());
    let app = router(h.state.clone());
    let asked = tokio::spawn(async move { app.oneshot(req).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    h.state.previews.begin_shutdown();
    let resp = asked.await.unwrap();
    assert_eq!(resp.status().as_u16(), 503);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["code"], "unavailable", "{v}");
    assert!(h.state.works.lock().unwrap().is_empty(), "nothing registered");
    assert!(h.ran.lock().unwrap().is_empty(), "nothing ran");
}

/// A run in flight when `client serve` stops is canceled and ends as a
/// canceled run does: in its terminal state, its edits committed as its
/// own.
#[tokio::test]
async fn a_run_in_flight_at_shutdown_ends_canceled_with_its_commit() {
    let h = harness();
    let token = paired(&h).await;
    let wt = git_workspace(&h, "site");
    let id = start_in(&h, &token, "site", "edit-wait").await;
    for _ in 0..500 {
        if wt.join("in-progress.txt").exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let began = begin_stop(&h.state.previews, &h.state.works);
    let within = std::time::Duration::from_secs(5);
    let left = finish_stop(h.state.previews.clone(), h.state.works.clone(), began + within).await;
    assert_eq!(left, Vec::<String>::new(), "nothing is left running");
    assert!(began.elapsed() < within, "it ended at its cancel, not at the bound");
    let done = status(&h, &token, &id).await;
    assert_eq!(done["status"], "cancelled", "{done}");
    assert!(done["commit"].as_str().is_some(), "{done}");
    assert_eq!(
        run_git(Some(&wt), &["show", "--name-only", "--format=%s", "HEAD"]),
        format!("coder: edit-wait (work {id}, cancelled)\n\nin-progress.txt")
    );
}

/// A run that ignores its cancel never holds the exit: the wait for it ends
/// at its bound and names it, and the runtime is left behind at its own
/// bound while the run's thread is still going.
#[test]
fn shutdown_never_waits_past_its_bound_for_a_run_that_ignores_its_cancel() {
    let mut h = harness();
    h.state.execute = Arc::new(|_spec, _cancel| {
        std::thread::sleep(std::time::Duration::from_secs(3));
        Ok(serde_json::json!({"result": "stop"}))
    });
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let token = rt.block_on(paired(&h));
    let (code, v) = rt.block_on(call(&h, post("/client/work", Some(&token), work_for("coder"))));
    assert_eq!(code, 202, "{v}");
    let began = std::time::Instant::now();
    let bound = std::time::Duration::from_millis(200);
    let left = end_running_work(&h.state.works, began + bound);
    assert_eq!(left, vec![v["work_id"].as_str().unwrap().to_string()]);
    assert!(began.elapsed() >= bound, "it waited its bound");
    end_runtime(rt, bound);
    let took = began.elapsed();
    assert!(took < std::time::Duration::from_secs(2), "the exit waited {took:?} on a 3 s run");
}

/// A wedged docker (each `docker stop` and `rm` waiting out its own bound)
/// never holds the exit past the stop's bound: the wait for the previews'
/// containers ends there too, and the runtime is left at its own bound
/// while their removal still runs.
#[test]
fn shutdown_never_waits_past_its_bound_for_a_wedged_preview_stop() {
    let h = harness();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let token = rt.block_on(paired(&h));
    let (mine, name, _ws) = rt.block_on(served_workspace(&h, &token));
    recipe(&mine, serde_json::json!({"command": serve(), "env": {"PORT": "{port}"}, "ready": {"http": "/"}}));
    let (code, v) = rt.block_on(start_preview(&h, &token, &name));
    assert_eq!(code, 202, "{v}");
    let ready = rt.block_on(preview_until(&h, &token, &name, &["ready", "failed"]));
    assert_eq!(ready["status"], "ready", "{ready}");
    *h.containers.remove_delay.lock().unwrap() = std::time::Duration::from_secs(5);
    let began = begin_stop(&h.state.previews, &h.state.works);
    let bound = std::time::Duration::from_millis(300);
    rt.block_on(finish_stop(h.state.previews.clone(), h.state.works.clone(), began + bound));
    assert!(began.elapsed() < std::time::Duration::from_secs(2), "the stop waited {:?} on a wedged docker", began.elapsed());
    end_runtime(rt, bound);
    let took = began.elapsed();
    assert!(took < std::time::Duration::from_secs(3), "the exit waited {took:?} on a wedged docker");
}

/// `client revoke` ends what the client started: at the next look its
/// running work is canceled and its previews stopped, said once. Another
/// client's work runs on.
#[tokio::test]
async fn a_revoked_clients_runs_are_canceled_and_its_previews_stopped() {
    let h = harness();
    let token = paired(&h).await;
    let other = paired(&h).await;
    let (mine, name, _ws) = served_workspace(&h, &token).await;
    recipe(&mine, serde_json::json!({"command": serve(), "env": {"PORT": "{port}"}}));
    let (code, v) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 202, "{v}");
    let ready = preview_until(&h, &token, &name, &["ready", "failed"]).await;
    assert_eq!(ready["status"], "ready", "{ready}");
    let port = ready["port"].as_u64().unwrap();
    let (_, v) = call(&h, post("/client/work", Some(&token), waiting_work())).await;
    let revoked_run = v["work_id"].as_str().unwrap().to_string();
    let (_, v) = call(&h, post("/client/work", Some(&other), waiting_work())).await;
    let others_run = v["work_id"].as_str().unwrap().to_string();
    assert!(unpaired_tick(&h.state).is_empty(), "both are paired: nothing ends");
    assert_eq!(work_word(&h, &revoked_run), "running");

    let revoked = list_clients(&h.store).unwrap()[0].name.clone();
    assert!(revoke(&h.store, &revoked).unwrap());
    let lines = unpaired_tick(&h.state);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines.iter().any(|l| l.contains(&revoked_run)), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains(&name)), "{lines:?}");
    assert!(work_reaches(&h, &revoked_run, "cancelled").await, "it stands at {}", work_word(&h, &revoked_run));
    assert_eq!(h.state.previews.status(&name).unwrap().status, "stopped");
    for _ in 0..100 {
        if !answers(port) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(!answers(port), "the dev server is gone");
    assert_eq!(work_word(&h, &others_run), "running", "a paired client's run is left alone");
    assert!(unpaired_tick(&h.state).is_empty(), "said once");
    assert_eq!(cancel(&h, Some(&other), &others_run).await.0, 202);
}

/// A revoked client's run that is slow to end after its cancel is
/// canceled, and said, once: later looks leave it to end.
#[tokio::test]
async fn a_revoked_clients_run_is_canceled_once_while_it_ends() {
    let mut h = harness();
    h.state.execute = Arc::new(|_spec, _cancel| {
        std::thread::sleep(std::time::Duration::from_millis(600));
        Ok(serde_json::json!({"result": "stop"}))
    });
    let token = paired(&h).await;
    let (_, v) = call(&h, post("/client/work", Some(&token), work_for("coder"))).await;
    let id = v["work_id"].as_str().unwrap().to_string();
    let name = list_clients(&h.store).unwrap()[0].name.clone();
    assert!(revoke(&h.store, &name).unwrap());
    assert_eq!(unpaired_tick(&h.state).len(), 1);
    let again = unpaired_tick(&h.state);
    assert_eq!(work_word(&h, &id), "running", "still ending");
    assert_eq!(again, Vec::<String>::new(), "said once");
}

/// A store that cannot be read ends nothing: a client is unpaired only when
/// the store says so.
#[tokio::test]
async fn an_unreadable_store_ends_no_ones_work() {
    let h = harness();
    let token = paired(&h).await;
    let (_, v) = call(&h, post("/client/work", Some(&token), waiting_work())).await;
    let id = v["work_id"].as_str().unwrap().to_string();
    let kept = std::fs::read(h.store.path()).unwrap();
    std::fs::write(h.store.path(), "{ not json").unwrap();
    assert!(unpaired_tick(&h.state).is_empty());
    assert_eq!(work_word(&h, &id), "running");
    std::fs::write(h.store.path(), kept).unwrap();
    assert_eq!(cancel(&h, Some(&token), &id).await.0, 202);
}

/// The check's thread ends a revoked client's work on its own: soon after
/// the store changes (well inside a long interval), and at its interval
/// for a client revoked before it began to watch.
#[tokio::test]
async fn the_check_thread_ends_a_revoked_clients_work_on_its_own() {
    for (interval_ms, revoke_first) in [(3_600_000, false), (20, true)] {
        let h = harness();
        let token = paired(&h).await;
        let (_, v) = call(&h, post("/client/work", Some(&token), waiting_work())).await;
        let id = v["work_id"].as_str().unwrap().to_string();
        let name = list_clients(&h.store).unwrap()[0].name.clone();
        if revoke_first {
            assert!(revoke(&h.store, &name).unwrap());
        }
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = spawn_preview_idle_check(
            h.state.clone(),
            std::time::Duration::from_millis(interval_ms),
            stop.clone(),
        );
        if !revoke_first {
            // Its first look at the store is behind it.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            assert_eq!(work_word(&h, &id), "running");
            assert!(revoke(&h.store, &name).unwrap());
        }
        let ended = work_reaches(&h, &id, "cancelled").await;
        stop.store(true, Ordering::SeqCst);
        thread.join().unwrap();
        assert!(ended, "interval {interval_ms} ms: it stands at {}", work_word(&h, &id));
    }
}

/// A change replaces the store in one rename: a reader that opened it
/// before the change still reads the whole old store (it is never emptied
/// in place), the new one is owner-only, and no temp file is left.
#[test]
fn the_store_is_replaced_whole_never_rewritten_in_place() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let store = ClientStore::at(dir.path().join("clients.json"));
    store.update(|f| { f.legacy_owner = Some("before".into()); Ok(()) }).unwrap();
    let mut opened_before = std::fs::File::open(store.path()).unwrap();
    store.update(|f| { f.legacy_owner = Some("after".into()); Ok(()) }).unwrap();
    let mut old = String::new();
    opened_before.read_to_string(&mut old).unwrap();
    let old: ClientsFile = serde_json::from_str(&old).expect("the old store, whole");
    assert_eq!(old.legacy_owner.as_deref(), Some("before"));
    assert_eq!(store.read().unwrap().legacy_owner.as_deref(), Some("after"));
    assert_eq!(std::fs::metadata(store.path()).unwrap().permissions().mode() & 0o777, 0o600);
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["clients.json"], "no temp file left");
}

/// A change that waited for the store's lock builds on the change made
/// before it, never on the file that change replaced.
#[test]
fn a_change_that_waited_for_the_lock_builds_on_the_one_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(ClientStore::at(dir.path().join("clients.json")));
    store.update(|f| { f.legacy_owner = Some("start".into()); Ok(()) }).unwrap();
    let (first, holding) = (store.clone(), Arc::new(std::sync::Barrier::new(2)));
    let held = holding.clone();
    let a = std::thread::spawn(move || {
        first
            .update(|f| {
                held.wait();
                // The second change is waiting on the lock by now.
                std::thread::sleep(std::time::Duration::from_millis(300));
                f.legacy_owner = Some("first".into());
                Ok(())
            })
            .unwrap();
    });
    holding.wait();
    let client = ApprovedClient { name: "second".into(), id: "c-2".into(), created: 1, token_hash: "h".into() };
    store.update(|f| { f.clients.push(client.clone()); Ok(()) }).unwrap();
    a.join().unwrap();
    let now = store.read().unwrap();
    assert_eq!(now.legacy_owner.as_deref(), Some("first"), "the first change survived the second");
    assert_eq!(now.clients, vec![client]);
}

/// A read that waited for the store's lock reads the store as the change
/// it waited on left it, never the file that change replaced.
#[test]
fn a_peek_that_waited_for_the_lock_reads_the_change_it_waited_on() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(ClientStore::at(dir.path().join("clients.json")));
    store.update(|f| { f.legacy_owner = Some("start".into()); Ok(()) }).unwrap();
    let (writer, holding) = (store.clone(), Arc::new(std::sync::Barrier::new(2)));
    let held = holding.clone();
    let a = std::thread::spawn(move || {
        writer
            .update(|f| {
                held.wait();
                // The read is waiting on the lock by now.
                std::thread::sleep(std::time::Duration::from_millis(300));
                f.legacy_owner = Some("changed".into());
                Ok(())
            })
            .unwrap();
    });
    holding.wait();
    let seen = store.peek().unwrap();
    a.join().unwrap();
    assert_eq!(seen.legacy_owner.as_deref(), Some("changed"));
}
