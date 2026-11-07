//! Tests for `client_pairing`: the store, the CLI-side verbs, and the HTTP
//! contract driven through the real router. No test dispatches: the executor
//! is injected (and the production child runner is driven with a fake child).

use super::*;
use axum::body::Body;
use axum::http::Request;
use std::sync::atomic::{AtomicU64, Ordering};
use tower::ServiceExt;

const SECRET: &str = "app-secret-0123456789abcdef";

struct Harness {
    _dir: tempfile::TempDir,
    store: Arc<ClientStore>,
    clock: Arc<AtomicU64>,
    base: PathBuf,
    ran: Arc<Mutex<Vec<WorkSpec>>>,
    state: ClientServerState,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(ClientStore::at(dir.path().join("clients.json")));
    let clock = Arc::new(AtomicU64::new(1_000_000));
    let base = dir.path().join("worktrees");
    std::fs::create_dir_all(base.join("wt1")).unwrap();
    let canon_base = base.canonicalize().unwrap();
    let ran = Arc::new(Mutex::new(Vec::new()));
    let c = clock.clone();
    let r = ran.clone();
    let state = ClientServerState {
        store: store.clone(),
        now: Arc::new(move || c.load(Ordering::SeqCst)),
        role_known: Arc::new(|role| Ok(role == "coder" || role == "radio-host")),
        resolve_profile: Arc::new(|_role, profile| {
            if profile == "fast" || profile == "deep" {
                Ok(())
            } else {
                Err(format!("profile {profile} is not defined on this machine"))
            }
        }),
        check_workdir: Arc::new(move |p| {
            darkmux_types::workdir::validate_remote_workdir_in(p, &canon_base)
                .map_err(|e| format!("{e:#}"))
        }),
        execute: Arc::new(move |spec: WorkSpec| {
            r.lock().unwrap().push(spec.clone());
            Ok(
                serde_json::json!({"result": "stop", "final_assistant": format!("ran {}", spec.role)}),
            )
        }),
        works: Arc::new(Mutex::new(HashMap::new())),
    };
    Harness {
        base,
        _dir: dir,
        store,
        clock,
        ran,
        state,
    }
}

async fn call(h: &Harness, req: Request<Body>) -> (u16, Value) {
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

fn pair_body(workspace: bool) -> Value {
    serde_json::json!({
        "device_name": "Kain's iPhone",
        "code": "4F7-K2Q",
        "pair_secret": SECRET,
        "scopes": {"profiles": ["fast"], "workspace": workspace},
    })
}

async fn request_pairing(h: &Harness, workspace: bool) -> String {
    let (code, v) = call(h, post("/client/pair", None, pair_body(workspace))).await;
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
async fn paired(h: &Harness, workspace: bool) -> String {
    let id = request_pairing(h, workspace).await;
    approve(&h.store, &id, (h.state.now)()).unwrap();
    let (code, v) = poll(h, &id, SECRET).await;
    assert_eq!(code, 200, "{v}");
    v["token"].as_str().unwrap().to_string()
}

// ─── Pairing ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn pairing_happy_path_delivers_a_token_that_whoami_accepts() {
    let h = harness();
    let id = request_pairing(&h, false).await;
    assert_eq!(
        poll(&h, &id, SECRET).await,
        (200, serde_json::json!({"status": "pending"}))
    );

    let pending = list_pending(&h.store, (h.state.now)()).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].device_name, "Kain's iPhone");
    assert_eq!(pending[0].code, "4F7-K2Q");

    let name = approve(&h.store, &id, (h.state.now)()).unwrap();
    let (code, v) = poll(&h, &id, SECRET).await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["status"], "approved");
    assert_eq!(v["client_name"], name);
    assert_eq!(
        v["scopes"],
        serde_json::json!({"profiles": ["fast"], "workspace": false})
    );
    let token = v["token"].as_str().unwrap();
    assert_eq!(token.len(), 64, "32 random bytes, hex");
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));

    let (code, who) = call(
        &h,
        get(
            "/client/whoami",
            &[("authorization", &format!("Bearer {token}"))],
        ),
    )
    .await;
    assert_eq!(code, 200, "{who}");
    assert_eq!(
        who,
        serde_json::json!({"client_name": name, "scopes": {"profiles": ["fast"], "workspace": false}})
    );
}

#[tokio::test]
async fn the_token_and_pair_secret_are_stored_only_as_hashes() {
    let h = harness();
    let token = paired(&h, false).await;
    let text = std::fs::read_to_string(h.store.path()).unwrap();
    assert!(!text.contains(&token), "the token never reaches disk");
    assert!(!text.contains(SECRET), "the pair secret never reaches disk");
}

#[cfg(unix)]
#[tokio::test]
async fn clients_json_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let h = harness();
    let _ = paired(&h, false).await;
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
    let id = request_pairing(&h, false).await;
    approve(&h.store, &id, (h.state.now)()).unwrap();
    let (code, v) = poll(&h, &id, "not-the-secret-at-all-xx").await;
    assert_eq!(code, 404, "{v}");
    assert!(v.get("token").is_none());
    let (code, _) = call(&h, get(&format!("/client/pair/{id}"), &[])).await;
    assert_eq!(code, 404, "a missing secret is a wrong secret");
    // The right secret still collects it: the wrong one consumed nothing.
    let (code, v) = poll(&h, &id, SECRET).await;
    assert_eq!(code, 200);
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
    let id = request_pairing(&h, false).await;
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
    let id = request_pairing(&h, false).await;
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
    let id = request_pairing(&h, false).await;
    approve(&h.store, &id, (h.state.now)()).unwrap();
    let (code, v) = poll(&h, &id, SECRET).await;
    assert_eq!(code, 200);
    assert!(v["token"].is_string());
    let (code, v) = poll(&h, &id, SECRET).await;
    assert_eq!(code, 404, "the second poll gets nothing: {v}");
    assert_eq!(list_clients(&h.store).unwrap().len(), 1);
}

#[tokio::test]
async fn a_denied_request_says_so() {
    let h = harness();
    let id = request_pairing(&h, false).await;
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
    let _ = request_pairing(&h, false).await;
    let found = find_by_code(&h.store, "4f7-k2q", (h.state.now)()).unwrap();
    assert_eq!(found.code, "4F7-K2Q", "codes match case-insensitively");
    assert!(find_by_code(&h.store, "ZZZ-ZZZ", (h.state.now)()).is_err());
}

#[tokio::test]
async fn a_malformed_pair_request_is_refused_with_json() {
    let h = harness();
    let mut short = pair_body(false);
    short["pair_secret"] = "short".into();
    let (code, v) = call(&h, post("/client/pair", None, short)).await;
    assert_eq!(code, 400);
    assert!(v["error"].is_string());
    let (code, v) = call(
        &h,
        Request::builder()
            .method("POST")
            .uri("/client/pair")
            .body(Body::from("{not json"))
            .unwrap(),
    )
    .await;
    assert_eq!(code, 400, "{v}");
}

#[tokio::test]
async fn an_oversized_body_is_refused_with_json() {
    let h = harness();
    let mut big = pair_body(false);
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
        let _ = request_pairing(&h, false).await;
    }
    let (code, v) = call(&h, post("/client/pair", None, pair_body(false))).await;
    assert_eq!(code, 429, "{v}");
}

// ─── Tokens ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn whoami_without_or_with_a_wrong_token_is_401() {
    let h = harness();
    let _ = paired(&h, false).await;
    let (code, v) = call(&h, get("/client/whoami", &[])).await;
    assert_eq!(code, 401);
    assert!(v["error"].is_string());
    let wrong = "0".repeat(64);
    let (code, _) = call(
        &h,
        get(
            "/client/whoami",
            &[("authorization", &format!("Bearer {wrong}"))],
        ),
    )
    .await;
    assert_eq!(code, 401);
}

#[tokio::test]
async fn a_revoked_client_is_401_everywhere() {
    let h = harness();
    let token = paired(&h, false).await;
    let name = list_clients(&h.store).unwrap()[0].name.clone();
    assert!(revoke(&h.store, &name).unwrap());
    let auth = format!("Bearer {token}");
    let (code, _) = call(&h, get("/client/whoami", &[("authorization", &auth)])).await;
    assert_eq!(code, 401);
    let (code, _) = call(
        &h,
        post("/client/work", Some(&token), work_body("fast", None)),
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
        r#"{"schema_version":"1.0","pending":[],"clients":[{"name":"a","created":1,"scopes":{"profiles":[],"workspace":false},"token_hash":"00","admin":true}]}"#,
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

    std::fs::write(&path, r#"{"schema_version":"1.0","pendng":[]}"#).unwrap();
    let err = ClientStore::at(path).read().unwrap_err().to_string();
    assert!(
        err.contains("did you mean `pending`"),
        "suggests the closest key: {err}"
    );
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
    let taken = vec!["kain-s-iphone".to_string()];
    assert_eq!(client_name_for("Kain's iPhone", &taken), "kain-s-iphone-2");
    assert_eq!(client_name_for("!!!", &[]), "client");
}

// ─── Work ────────────────────────────────────────────────────────────────

fn work_body(profile: &str, workdir: Option<&str>) -> Value {
    serde_json::json!({"role": "coder", "message": "hello", "profile": profile, "workdir": workdir})
}

async fn wait_done(h: &Harness, token: &str, id: &str) -> Value {
    let auth = format!("Bearer {token}");
    for _ in 0..200 {
        let (code, v) = call(
            h,
            get(&format!("/client/work/{id}"), &[("authorization", &auth)]),
        )
        .await;
        assert_eq!(code, 200, "{v}");
        if v["status"] != "running" {
            return v;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("work {id} never finished");
}

#[tokio::test]
async fn in_scope_work_runs_and_its_result_comes_back() {
    let h = harness();
    let token = paired(&h, false).await;
    let (code, v) = call(
        &h,
        post("/client/work", Some(&token), work_body("fast", None)),
    )
    .await;
    assert_eq!(code, 202, "{v}");
    let id = v["work_id"].as_str().unwrap().to_string();
    let done = wait_done(&h, &token, &id).await;
    assert_eq!(done["status"], "done");
    assert_eq!(done["result"]["final_assistant"], "ran coder");
    let ran = h.ran.lock().unwrap();
    assert_eq!(ran.len(), 1);
    assert_eq!(ran[0].work_id, id);
    assert_eq!(ran[0].profile, "fast");
    assert_eq!(ran[0].message, "hello");
}

#[tokio::test]
async fn a_failed_execution_reports_its_error() {
    let mut h = harness();
    h.state.execute = Arc::new(|_| Err("dispatch exited 1".to_string()));
    let token = paired(&h, false).await;
    let (_, v) = call(
        &h,
        post("/client/work", Some(&token), work_body("fast", None)),
    )
    .await;
    let done = wait_done(&h, &token, v["work_id"].as_str().unwrap()).await;
    assert_eq!(
        done,
        serde_json::json!({"status": "error", "error": "dispatch exited 1"})
    );
}

#[tokio::test]
async fn a_profile_outside_the_scopes_is_403_and_nothing_runs() {
    let h = harness();
    let token = paired(&h, false).await;
    let (code, v) = call(
        &h,
        post("/client/work", Some(&token), work_body("deep", None)),
    )
    .await;
    assert_eq!(code, 403, "{v}");
    assert!(v["error"].as_str().unwrap().contains("deep"));
    assert!(h.ran.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_granted_profile_that_routes_to_another_machine_is_403() {
    let h = harness();
    let id = {
        let mut b = pair_body(false);
        b["scopes"]["profiles"] = serde_json::json!(["fast@studio"]);
        let (_, v) = call(&h, post("/client/pair", None, b)).await;
        v["request_id"].as_str().unwrap().to_string()
    };
    approve(&h.store, &id, (h.state.now)()).unwrap();
    let token = poll(&h, &id, SECRET).await.1["token"]
        .as_str()
        .unwrap()
        .to_string();
    let (code, v) = call(
        &h,
        post("/client/work", Some(&token), work_body("fast@studio", None)),
    )
    .await;
    assert_eq!(code, 403, "{v}");
    assert!(h.ran.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_granted_profile_this_machine_does_not_define_is_403() {
    let h = harness();
    let id = {
        let mut b = pair_body(false);
        b["scopes"]["profiles"] = serde_json::json!(["ghost"]);
        let (_, v) = call(&h, post("/client/pair", None, b)).await;
        v["request_id"].as_str().unwrap().to_string()
    };
    approve(&h.store, &id, (h.state.now)()).unwrap();
    let token = poll(&h, &id, SECRET).await.1["token"]
        .as_str()
        .unwrap()
        .to_string();
    let (code, v) = call(
        &h,
        post("/client/work", Some(&token), work_body("ghost", None)),
    )
    .await;
    assert_eq!(code, 403, "never falls back to the default profile: {v}");
    assert!(h.ran.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_workdir_without_the_workspace_grant_is_403() {
    let h = harness();
    let token = paired(&h, false).await;
    let wt = h.base.join("wt1");
    let (code, v) = call(
        &h,
        post("/client/work", Some(&token), work_body("fast", wt.to_str())),
    )
    .await;
    assert_eq!(code, 403, "{v}");
    assert!(v["error"].as_str().unwrap().contains("workspace"));
    assert!(h.ran.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_workdir_outside_the_worktrees_base_is_403() {
    let h = harness();
    let token = paired(&h, true).await;
    let outside = h._dir.path().to_str().unwrap().to_string();
    let (code, v) = call(
        &h,
        post(
            "/client/work",
            Some(&token),
            work_body("fast", Some(&outside)),
        ),
    )
    .await;
    assert_eq!(code, 403, "{v}");
    assert!(h.ran.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_workdir_under_the_base_with_the_grant_runs_canonicalized() {
    let h = harness();
    let token = paired(&h, true).await;
    let wt = h.base.join("wt1");
    let (code, v) = call(
        &h,
        post("/client/work", Some(&token), work_body("fast", wt.to_str())),
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
    let token = paired(&h, false).await;
    let mut b = work_body("fast", None);
    b["role"] = "--help".into();
    let (code, v) = call(&h, post("/client/work", Some(&token), b)).await;
    assert_eq!(code, 400, "{v}");
    assert!(h.ran.lock().unwrap().is_empty());
}

#[tokio::test]
async fn work_is_visible_only_to_the_client_that_created_it() {
    let h = harness();
    let a = paired(&h, false).await;
    let b = paired(&h, false).await;
    let (_, v) = call(&h, post("/client/work", Some(&a), work_body("fast", None))).await;
    let id = v["work_id"].as_str().unwrap().to_string();
    let (code, _) = call(
        &h,
        get(
            &format!("/client/work/{id}"),
            &[("authorization", &format!("Bearer {b}"))],
        ),
    )
    .await;
    assert_eq!(code, 404);
    let (code, _) = call(&h, get(&format!("/client/work/{id}"), &[])).await;
    assert_eq!(code, 401);
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
    std::fs::write(&ok, "#!/bin/sh\nmsg=$(cat)\nprintf '{\"result\":\"stop\",\"final_assistant\":\"%s %s\"}' \"$2\" \"$msg\"\n").unwrap();
    std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o700)).unwrap();
    let spec = WorkSpec {
        work_id: "w-1".into(),
        role: "coder".into(),
        profile: "fast".into(),
        workdir: None,
        message: "hi".into(),
    };
    let v = run_dispatch_child(&ok, &spec).unwrap();
    assert_eq!(v["final_assistant"], "coder hi");

    let bad = dir.path().join("bad");
    std::fs::write(&bad, "#!/bin/sh\ncat >/dev/null\necho 'boom' >&2\nexit 3\n").unwrap();
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o700)).unwrap();
    let e = run_dispatch_child(&bad, &spec).unwrap_err();
    assert!(e.contains("exited") && e.contains("boom"), "{e}");
}
