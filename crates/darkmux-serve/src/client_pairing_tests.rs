//! Tests for `client_pairing`: the store, the CLI-side verbs, and the HTTP
//! contract driven through the real router. No test dispatches: the executor
//! is injected (and the production child runner is driven with a fake child),
//! and the machine's `clients.profiles` policy is injected too, so no test
//! reads a real `config.json`.

use super::*;
use crate::client_profiles::{describe_profile, describe_profiles, work_stays_here};
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
    /// URLs the injected clone was asked for, and dirs it fetched.
    cloned: Arc<Mutex<Vec<String>>>,
    fetched: Arc<Mutex<Vec<PathBuf>>>,
    /// A git repo with one commit, standing in for every remote.
    fixture: PathBuf,
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
        // hosted API, so it isn't local.
        profile_is_local: Arc::new(|_role, name| match name {
            "fast" | "deep" => Ok(true),
            "hosted" => Ok(false),
            other => Err(format!("profile {other} is not in the registry")),
        }),
        registry_profiles: Arc::new(|| {
            Ok(vec![
                "deep".into(),
                "fast".into(),
                "hosted".into(),
                "utility".into(),
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
            // A "wait" message runs until it is cancelled, as a dispatch child would.
            if spec.message == "wait" || spec.message == "edit-wait" {
                for _ in 0..500 {
                    if cancel.requested() {
                        return Err(RunError {
                            reason: "exit 143/SIGTERM".into(),
                            error: "stopped".into(),
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
            Ok(
                serde_json::json!({"result": "stop", "final_assistant": format!("ran {}", spec.role)}),
            )
        }),
        works: Arc::new(Mutex::new(HashMap::new())),
        worktrees_base: Arc::new(move || Ok(wb.clone())),
        // Every "remote" is the fixture; origin is then set to the URL asked
        // for, as a real clone would leave it. A URL naming "fail" fails
        // after leaving a partial directory behind, as an interrupted clone can.
        clone_repo: Arc::new(move |url, git_ref, dest| {
            cl.lock().unwrap().push(url.to_string());
            if url.contains("fail") {
                std::fs::create_dir_all(dest.join("half")).unwrap();
                return Err("authentication failed".into());
            }
            let mut args = vec!["clone", "--quiet"];
            if let Some(r) = git_ref {
                args.extend(["--branch", r]);
            }
            let fx = fx.to_str().unwrap().to_string();
            let d = dest.to_str().unwrap().to_string();
            args.extend([fx.as_str(), d.as_str()]);
            run_git(None, &args);
            run_git(Some(dest), &["remote", "set-url", "origin", url]);
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
                    return Err("fetch failed".into());
                }
            }
            Ok(())
        }),
        workspace_lock: Arc::new(Mutex::new(())),
        previews: Arc::new(crate::client_preview::Previews::new(
            dir.path().join("previews"),
            std::process::id(),
        )),
        bound: Some(BOUND.parse().unwrap()),
    };
    Harness {
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

/// The address the harness's listener is taken to be bound on.
const BOUND: &str = "127.0.0.1:8767";

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

/// Pair, approve through the CLI-side verb, collect the token.
async fn paired(h: &Harness) -> String {
    let id = request_pairing(h).await;
    approve(&h.store, &id, (h.state.now)()).unwrap();
    let (code, v) = poll(h, &id, SECRET).await;
    assert_eq!(code, 200, "{v}");
    v["token"].as_str().unwrap().to_string()
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

#[tokio::test]
async fn pending_requests_are_capped() {
    let h = harness();
    for _ in 0..MAX_PENDING {
        let _ = request_pairing(&h).await;
    }
    let (code, v) = call(&h, post("/client/pair", None, pair_body())).await;
    assert_eq!(code, 429, "{v}");
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
        })
    });
    let token = paired(&h).await;
    let (_, v) = call(&h, post("/client/work", Some(&token), work_for("coder"))).await;
    let done = wait_done(&h, &token, v["work_id"].as_str().unwrap()).await;
    assert_eq!(
        done,
        serde_json::json!({"status": "error", "error": "boom on stderr", "reason": "exit 1"})
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
    assert!(v["error"].as_str().unwrap().contains("your own checkout"), "{v}");
    let (_mine, name, ws) = checkout_workspace(&h, &token).await;
    write_recipe(&ws, A_RECIPE);
    let (code, v) = call(
        &h,
        get(&format!("/client/recipe?workspace={name}"), &[("authorization", &bearer(&token))]),
    )
    .await;
    assert_eq!(code, 404, "{v}");
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

#[tokio::test]
async fn cancelling_a_finished_run_is_409() {
    let h = harness();
    let token = paired(&h).await;
    let (_, v) = call(&h, post("/client/work", Some(&token), work_for("coder"))).await;
    let id = v["work_id"].as_str().unwrap().to_string();
    let _ = wait_done(&h, &token, &id).await;
    assert_eq!(
        cancel(&h, Some(&token), &id).await,
        (409, serde_json::json!({"error": "not running"}))
    );
}

/// What the production teardown would have done, recorded.
#[derive(Default)]
struct FakeTeardown {
    start: Option<StartedRun>,
    terminal_exists: bool,
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
    let before = snapshot_tree(dir.path()).expect("a git work copy snapshots");
    std::fs::write(
        dir.path().join("pre.txt"),
        "already here\nadded by the run\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("new.txt"), "a\nb\n").unwrap();
    let c = changes_since(dir.path(), &before).unwrap();
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
    let before = snapshot_tree(dir.path()).unwrap();
    assert_eq!(
        changes_since(dir.path(), &before).unwrap(),
        serde_json::json!({"files": [], "diff": ""})
    );
}

#[test]
fn a_large_diff_is_capped_with_a_marker() {
    let dir = tempfile::tempdir().unwrap();
    git_repo(dir.path());
    let before = snapshot_tree(dir.path()).unwrap();
    let big: String = (0..5000)
        .map(|i| format!("line number {i} of a long file\n"))
        .collect();
    std::fs::write(dir.path().join("big.txt"), &big).unwrap();
    let c = changes_since(dir.path(), &before).unwrap();
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

#[test]
fn a_workdir_that_is_not_a_git_repo_has_no_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
    assert_eq!(snapshot_tree(dir.path()), None);
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
    assert_eq!(v["branch"], "main");
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
    assert_eq!(v["branch"], "dev");
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
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("authentication failed"),
        "{v}"
    );
    let left: Vec<_> = std::fs::read_dir(&h.base)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .collect();
    assert_eq!(
        left,
        vec![std::ffi::OsString::from("wt1")],
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
    assert_eq!(v["branch"], "main");
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
    assert_eq!(names.len(), 2, "only wt1 and inside: {names:?}");
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

/// A git workspace under the base with an uncommitted edit of the owner's
/// own (`notes.txt`), which Keep and Discard must never touch.
fn git_workspace(h: &Harness, name: &str) -> PathBuf {
    let wt = h.base.join(name);
    fixture_repo(&wt);
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
    std::fs::write(wt.join("[ab].txt"), "made by the run\n").unwrap();
    std::fs::write(wt.join("b.txt"), "the owner's, uncommitted\n").unwrap();
    let (sha, _) = commit_run(&wt, &["[ab].txt".to_string()], "coder: glob (work w-1)").unwrap();
    let files = run_git(Some(&wt), &["show", "--name-only", "--format=", &sha]);
    assert_eq!(files.lines().collect::<Vec<_>>(), ["[ab].txt"], "{files}");
    let status = run_git(Some(&wt), &["status", "--porcelain", "--", "b.txt"]);
    assert!(status.contains("?? b.txt"), "b.txt stays the owner's, uncommitted: {status}");
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

#[tokio::test]
async fn undo_refuses_what_it_cant_do() {
    let h = harness();
    let token = paired(&h).await;
    let other = paired(&h).await;
    git_workspace(&h, "site");
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

const SERVE: &str = "exec python3 -m http.server \"$PORT\" --bind 127.0.0.1";

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
        serde_json::json!({"command": SERVE, "env": {"PORT": "{port}"}, "ready": {"http": "/"}}),
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
        serde_json::json!({"command": format!("touch pwned; {SERVE}"), "env": {"PORT": "{port}"},
            "install": {"command": "touch pwned-install"}}),
    );
    let (code, v) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 404, "{v}");
    let e = v["error"].as_str().unwrap();
    assert!(e.contains("add .darkmux/preview.json to your own checkout"), "{v}");
    assert!(e.contains(mine.to_str().unwrap()), "names the checkout: {v}");
    recipe(&mine, serde_json::json!({"command": SERVE, "env": {"PORT": "{port}"}}));
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
    recipe(&ws, serde_json::json!({"command": format!("touch pwned; {SERVE}")}));
    let (code, v) = start_preview(&h, &token, "hcr").await;
    assert_eq!(code, 404, "{v}");
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
    recipe(&mine, serde_json::json!({"command": SERVE, "env": {"PORT": "{port}"}}));
    // A run rewrites the workspace's `.git` to a repository of its own.
    std::fs::remove_file(ws.join(".git")).unwrap();
    run_git(Some(&ws), &["init", "--quiet"]);
    let (code, v) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 404, "{v}");
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
    recipe(&moved, serde_json::json!({"command": SERVE, "env": {"PORT": "{port}"}}));
    let (code, v) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 404, "{v}");
    assert!(v["error"].as_str().unwrap().contains("symlink"), "{v}");
}

#[tokio::test]
async fn install_runs_only_when_its_marker_is_missing() {
    let h = harness();
    let token = paired(&h).await;
    let (mine, name, ws) = served_workspace(&h, &token).await;
    recipe(
        &mine,
        serde_json::json!({
            "command": SERVE, "env": {"PORT": "{port}"},
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
        serde_json::json!({"command": SERVE, "env": {"PORT": "{port}"}, "env_from_source": [".env", ".env.local"]}),
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
        serde_json::json!({"command": SERVE, "env_from_source": ["../../etc/passwd"]}),
    );
    let (code, v) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 422, "{v}");
    recipe(&mine, serde_json::json!({"command": SERVE, "comand": "typo"}));
    let (code, v) = start_preview(&h, &token, &name).await;
    assert_eq!(code, 422, "{v}");
    assert!(v["error"].as_str().unwrap().contains("unknown key `comand`"), "{v}");
    write_recipe(&mine, &serde_json::json!({"command": SERVE}).to_string());
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

#[tokio::test]
async fn a_preview_ends_when_darkmux_does() {
    let dir = tempfile::tempdir().unwrap();
    let wt = dir.path().join("w");
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(wt.join("index.html"), "up").unwrap();
    let checkout = dir.path().join("mine");
    std::fs::create_dir_all(&checkout).unwrap();
    recipe(
        &checkout,
        serde_json::json!({"command": SERVE, "env": {"PORT": "{port}"}}),
    );
    // "darkmux" here is a short-lived process; when it exits, so does the preview.
    let mut stand_in = std::process::Command::new("sleep")
        .arg("3")
        .spawn()
        .unwrap();
    let previews = crate::client_preview::Previews::new(dir.path().join("p"), stand_in.id());
    let (v, _) = previews.start("w", &wt, &checkout).unwrap();
    let port = v["port"].as_u64().unwrap();
    let mut up = false;
    for _ in 0..100 {
        if answers(port) {
            up = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(up, "started");
    stand_in.wait().unwrap();
    let mut gone = false;
    for _ in 0..100 {
        if !answers(port) {
            gone = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(gone, "the preview outlived darkmux");
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

#[tokio::test]
async fn continues_must_name_a_finished_run_of_yours_in_the_same_work_copy() {
    let h = harness();
    let token = paired(&h).await;
    git_workspace(&h, "site");
    let first = run_in(&h, &token, "site", "edit-readme").await;
    for (ws, cont) in [("site", "w-nope"), ("wt1", first.as_str())] {
        let (code, v) = call(&h, post("/client/work", Some(&token), serde_json::json!({"role": "coder", "message": "edit", "workspace": ws, "continues": cont}))).await;
        assert_eq!(code, 409, "{ws} {cont}: {v}");
    }
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
        }),
        "deep" => Ok(ClientProfileInfo {
            name: "deep".into(),
            description: None,
            model: Some("m-deep".into()),
            endpoint: "lmstudio".into(),
            reachable: false,
            loaded: Some(false),
            error: None,
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
