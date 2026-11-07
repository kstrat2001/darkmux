//! Tests for `client_pairing`: the store, the CLI-side verbs, and the HTTP
//! contract driven through the real router. No test dispatches: the executor
//! is injected (and the production child runner is driven with a fake child),
//! and the machine's `clients.profiles` policy is injected too, so no test
//! reads a real `config.json`.

use super::*;
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
    let state = ClientServerState {
        store: store.clone(),
        now: Arc::new(move || c.load(Ordering::SeqCst)),
        role_known: Arc::new(|role| Ok(role == "coder" || role == "radio-host")),
        // A requested profile resolves to itself when defined here; an
        // omitted one resolves through the role's binding, as `darkmux
        // dispatch <role>` would: `coder` is bound to `fast`, `radio-host` to
        // `deep`. `fast@studio` "resolves" on purpose, so the `@` guard in
        // `admit` is the only thing refusing it.
        resolve_profile: Arc::new(|role, profile| match profile {
            Some(p) if ["fast", "deep", "fast@studio"].contains(&p) => Ok(p.to_string()),
            Some(p) => Err(format!("profile {p} is not defined on this machine")),
            None if role == "coder" => Ok("fast".to_string()),
            None if role == "radio-host" => Ok("deep".to_string()),
            None => Err("no profile resolves".to_string()),
        }),
        profiles_policy: Arc::new(move || pol.lock().unwrap().clone()),
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
            // A "wait" message runs until it is cancelled, as a dispatch child would.
            if spec.message == "wait" {
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
            Ok(
                serde_json::json!({"result": "stop", "final_assistant": format!("ran {}", spec.role)}),
            )
        }),
        works: Arc::new(Mutex::new(HashMap::new())),
    };
    Harness {
        base,
        dir,
        store,
        clock,
        ran,
        policy,
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

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// The body an app built against the first contract sends: `scopes` included
/// (it is ignored).
fn pair_body() -> Value {
    serde_json::json!({
        "device_name": "Kain's iPhone",
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
    assert_eq!(pending[0].device_name, "Kain's iPhone");
    assert_eq!(pending[0].code, "4F7-K2Q");

    let name = approve(&h.store, &id, (h.state.now)()).unwrap();
    let (code, v) = poll(&h, &id, SECRET).await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["status"], "approved");
    assert_eq!(v["client_name"], name);
    assert_eq!(
        v["scopes"],
        serde_json::json!({"profiles": null, "workspace": true}),
        "unset policy reads as null"
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
        serde_json::json!({"client_name": name, "scopes": {"profiles": null, "workspace": true}})
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
    let taken = vec!["kain-s-iphone".to_string()];
    assert_eq!(client_name_for("Kain's iPhone", &taken), "kain-s-iphone-2");
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
async fn with_the_setting_unset_a_named_profile_other_than_the_roles_own_is_403() {
    let h = harness();
    let token = paired(&h).await;
    let (code, v) = call(
        &h,
        post("/client/work", Some(&token), work_body(Some("deep"), None)),
    )
    .await;
    assert_eq!(code, 403, "coder's own profile is fast: {v}");
    assert!(
        v["error"].as_str().unwrap().contains("clients.profiles"),
        "names the setting: {v}"
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

#[tokio::test]
async fn a_recipe_under_the_base_is_returned() {
    let h = harness();
    let token = paired(&h).await;
    let wt = h.base.join("wt1");
    write_recipe(&wt, r#"{"command": ["npm", "run", "dev"], "port": 5173}"#);
    let (code, v) = call(
        &h,
        get(&recipe_url(&wt), &[("authorization", &bearer(&token))]),
    )
    .await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(
        v,
        serde_json::json!({"recipe": {"command": ["npm", "run", "dev"], "port": 5173}})
    );
}

#[tokio::test]
async fn no_recipe_is_404() {
    let h = harness();
    let token = paired(&h).await;
    let (code, v) = call(
        &h,
        get(
            &recipe_url(&h.base.join("wt1")),
            &[("authorization", &bearer(&token))],
        ),
    )
    .await;
    assert_eq!((code, v), (404, serde_json::json!({"error": "no recipe"})));
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
    let outside = h.dir.path().join("secret.json");
    std::fs::write(&outside, r#"{"secret": true}"#).unwrap();
    let wt = h.base.join("wt1");
    std::fs::create_dir_all(wt.join(".darkmux")).unwrap();
    std::os::unix::fs::symlink(&outside, wt.join(".darkmux/preview.json")).unwrap();
    let (code, v) = call(
        &h,
        get(&recipe_url(&wt), &[("authorization", &bearer(&token))]),
    )
    .await;
    assert_eq!(code, 403, "{v}");
}

#[tokio::test]
async fn an_oversized_or_non_object_recipe_is_refused() {
    let h = harness();
    let token = paired(&h).await;
    let wt = h.base.join("wt1");
    write_recipe(
        &wt,
        &format!(r#"{{"pad": "{}"}}"#, "x".repeat(MAX_RECIPE_BYTES as usize)),
    );
    let (code, _) = call(
        &h,
        get(&recipe_url(&wt), &[("authorization", &bearer(&token))]),
    )
    .await;
    assert_eq!(code, 413);
    write_recipe(&wt, "[1, 2]");
    let (code, _) = call(
        &h,
        get(&recipe_url(&wt), &[("authorization", &bearer(&token))]),
    )
    .await;
    assert_eq!(code, 422);
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
