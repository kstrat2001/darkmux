    use super::*;
    use serial_test::serial;
    use std::io::Write;
    use tempfile::TempDir;

    // ─── #1405 review: operator identity is local-only ────────────────────

    #[test]
    fn identity_augmentation_is_local_only() {
        // Data-boundary pin: identity augmentation applies to locally-served
        // dispatches only; any remote-brained dispatch (single-shot or
        // agentic-remote container) skips it.
        assert!(identity_augmentation_allowed(false), "local dispatch augments");
        assert!(
            !identity_augmentation_allowed(true),
            "remote-brained dispatch must never carry operator identity"
        );
    }

    // ─── #1312: operator-declared key_env override (563 root fix) ────────

    #[test]
    #[serial]
    fn resolve_endpoint_secret_prefers_key_env_over_keychain_and_never_spawns_security() {
        // key_env names a set env var → its value is used and `security` is NEVER
        // spawned (the headless-runner root fix — a locked keychain can't hang
        // what isn't read). The keychain item is a clearly-fake name that no real
        // Keychain could satisfy, so a pass proves the env tier resolved.
        let var = "DARKMUX_TEST_ENDPOINT_KEY_1312";
        let prev = std::env::var(var).ok();
        unsafe { std::env::set_var(var, "env-provided-secret"); }

        let auth = darkmux_types::EndpointAuth {
            auth_type: Some(darkmux_types::EndpointAuthType::ApiKey),
            keychain: Some("darkmux-test-nonexistent-item-1312".to_string()),
            key_env: Some(var.to_string()),
            extras: Default::default(),
        };
        let secret = resolve_endpoint_secret(&auth).expect("key_env resolves");
        assert_eq!(secret, "env-provided-secret");

        unsafe {
            match prev {
                Some(v) => std::env::set_var(var, v),
                None => std::env::remove_var(var),
            }
        }
    }

    #[test]
    #[serial]
    fn resolve_endpoint_secret_bails_when_no_source_configured() {
        // key_env's var absent AND no keychain → a loud "no credential" error,
        // naming the declared var so the operator knows what to export.
        let var = "DARKMUX_TEST_ENDPOINT_KEY_ABSENT_1312";
        let prev = std::env::var(var).ok();
        unsafe { std::env::remove_var(var); }

        let auth = darkmux_types::EndpointAuth {
            auth_type: Some(darkmux_types::EndpointAuthType::ApiKey),
            keychain: None,
            key_env: Some(var.to_string()),
            extras: Default::default(),
        };
        let err = resolve_endpoint_secret(&auth).expect_err("no source → bail");
        assert!(err.to_string().contains(var), "error should name the declared var: {err}");

        if let Some(v) = prev {
            unsafe { std::env::set_var(var, v); }
        }
    }

    // ─── #1177: hosted-dispatch helpers ─────────────────────────────────

    #[test]
    fn remote_chat_url_builds_azure_and_openai_forms() {
        // Azure deployment base + api_version query.
        let az = darkmux_types::ModelEndpoint {
            url: Some(
                "https://x.cognitiveservices.azure.com/openai/deployments/gpt-4o".into(),
            ),
            api_version: Some("2025-01-01-preview".into()),
            ..Default::default()
        };
        assert_eq!(
            remote_chat_url(&az),
            "https://x.cognitiveservices.azure.com/openai/deployments/gpt-4o/chat/completions?api-version=2025-01-01-preview"
        );
        // OpenAI-style base, no api_version, trailing slash trimmed (not doubled).
        let oai = darkmux_types::ModelEndpoint {
            url: Some("https://api.openai.com/v1/".into()),
            ..Default::default()
        };
        assert_eq!(
            remote_chat_url(&oai),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn remote_endpoint_label_names_kind_host_and_model() {
        let az = darkmux_types::ModelEndpoint {
            url: Some(
                "https://finherogpt.cognitiveservices.azure.com/openai/deployments/gpt-4o".into(),
            ),
            ..Default::default()
        };
        assert_eq!(
            remote_endpoint_label(&az, "gpt-4o"),
            "azure:finherogpt.cognitiveservices.azure.com/gpt-4o"
        );
        // Non-azure host ⇒ openai kind; label carries host + model, never the path or auth.
        let oai = darkmux_types::ModelEndpoint {
            url: Some("https://api.openai.com/v1".into()),
            ..Default::default()
        };
        assert_eq!(
            remote_endpoint_label(&oai, "gpt-4.1"),
            "openai:api.openai.com/gpt-4.1"
        );
    }

    #[test]
    fn remote_routing_predicate_local_vs_remote() {
        // The fork's routing predicate: a model with no endpoint (or a
        // url-less endpoint) is LOCAL ⇒ container path; a url'd endpoint is
        // REMOTE ⇒ dispatch_remote. This is the guard that keeps local
        // dispatches out of the hosted fork.
        let local_no_ep = darkmux_types::ProfileModel {
            id: "qwen".into(),
            n_ctx: Some(40960),
            ..Default::default()
        };
        assert!(!local_no_ep
            .endpoint
            .as_ref()
            .is_some_and(|e| e.is_remote()));
        let remote = darkmux_types::ProfileModel {
            id: "gpt-4o".into(),
            n_ctx: Some(200000),
            endpoint: Some(darkmux_types::ModelEndpoint {
                url: Some("https://x.azure.com/openai/deployments/gpt-4o".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(remote.endpoint.as_ref().is_some_and(|e| e.is_remote()));
    }

    // ─── #1199: container-path routing + single-shot cap ───────────────

    #[test]
    fn container_path_required_honors_tools_and_force() {
        // Construct via serde — Role has no Default (loader tests' pattern).
        let tool_less: crate::types::Role = serde_json::from_str(
            r#"{"id":"r","description":"d","tool_palette":{"allow":[],"deny":[]},"escalation_contract":"bail-with-explanation"}"#,
        )
        .unwrap();
        assert!(!container_path_required(&tool_less, false));
        assert!(
            container_path_required(&tool_less, true),
            "force_container routes a tool-less role through the container"
        );
        let mut tooled = tool_less.clone();
        tooled.tool_palette.allow = vec!["read".into()];
        assert!(container_path_required(&tooled, false));
    }

    #[test]
    fn single_shot_body_cap_defaults_and_overrides() {
        let default_body = single_shot_body("m", "sys", "msg", None, None);
        assert_eq!(default_body["max_completion_tokens"], 4096);
        let capped = single_shot_body("m", "sys", "msg", Some(16000), None);
        assert_eq!(capped["max_completion_tokens"], 16000);
        assert_eq!(capped["model"], "m");
        assert_eq!(capped["messages"][1]["content"], "msg");
    }

    /// Reasoning-effort contract: the parameter passes through verbatim, is
    /// OMITTED entirely when unset (the endpoint's own default must apply —
    /// sending an explicit value would pin what should float), and raises the
    /// completion-cap default to 16384 because reasoning tokens bill inside
    /// `max_completion_tokens` (a 4096 cap under high effort returns empty
    /// content). An explicit cap still wins over the effort-raised default.
    #[test]
    fn single_shot_body_reasoning_effort_contract() {
        let plain = single_shot_body("m", "sys", "msg", None, None);
        assert!(
            plain.get("reasoning_effort").is_none(),
            "unset effort must OMIT the parameter, not send a default"
        );
        let effort = single_shot_body("m", "sys", "msg", None, Some("high"));
        assert_eq!(effort["reasoning_effort"], "high");
        assert_eq!(
            effort["max_completion_tokens"], 16384,
            "effort without an explicit cap must raise the default (reasoning bills inside the cap)"
        );
        let both = single_shot_body("m", "sys", "msg", Some(8000), Some("low"));
        assert_eq!(both["reasoning_effort"], "low");
        assert_eq!(both["max_completion_tokens"], 8000, "an explicit cap wins");
    }

    /// (#1260, FIX 2) A bare remote `crew dispatch` is metered as ONE
    /// execution: any positive per-execution allowance admits the single
    /// hosted call; a zero allowance (a hard operator opt-out) refuses it
    /// with a typed error NAMING the bucket, never dispatching off the meter.
    #[test]
    fn admit_remote_execution_gates_on_the_per_execution_budget() {
        assert!(admit_remote_execution(500_000).is_ok(), "a positive allowance admits the one call");
        assert!(admit_remote_execution(1).is_ok(), "even a tiny positive allowance admits a single call");
        let err = admit_remote_execution(0).unwrap_err().to_string();
        assert!(err.contains("remote token budget exhausted"), "{err}");
        assert!(err.contains("max_tokens_per_execution"), "the error names the bucket: {err}");
    }

    /// Hosted-response classification (pure): the happy path passes through;
    /// object-shaped errors (Azure/OpenAI) and ARRAY-shaped errors (Google's
    /// OpenAI-compat layer, observed live 2026-07-05) both surface their
    /// message; 429 / RESOURCE_EXHAUSTED classifies as retryable while every
    /// other error is terminal; a choices-less non-error body stays loud
    /// (#1135 healthy-while-broken) — but a message object with an ABSENT
    /// content field is Ok (#1222 packet 2; extraction reads it as "").
    #[test]
    fn parse_hosted_response_classifies_errors_and_rate_limits() {
        let ok = parse_hosted_response(
            br#"{"choices":[{"message":{"content":"hi"}}]}"#,
        );
        assert!(ok.is_ok());

        match parse_hosted_response(br#"{"error":{"code":401,"message":"bad key"}}"#) {
            Err(HostedCallError::Other(e)) => assert!(e.to_string().contains("bad key")),
            _ => panic!("object-shaped non-429 error must be terminal"),
        }
        match parse_hosted_response(br#"{"error":{"code":429,"message":"quota"}}"#) {
            Err(HostedCallError::RateLimited(m)) => assert!(m.contains("quota")),
            _ => panic!("object-shaped 429 must classify retryable"),
        }
        // Google's array shape — previously fell through to "missing choices".
        match parse_hosted_response(
            br#"[{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"You exceeded your current quota"}}]"#,
        ) {
            Err(HostedCallError::RateLimited(m)) => assert!(m.contains("exceeded")),
            _ => panic!("array-shaped 429 must classify retryable"),
        }
        match parse_hosted_response(br#"[{"error":{"code":400,"message":"bad request"}}]"#) {
            Err(HostedCallError::Other(e)) => assert!(e.to_string().contains("bad request")),
            _ => panic!("array-shaped non-429 must be terminal"),
        }
        // Capacity shedding is retryable in all three observed shapes:
        // 503 code, UNAVAILABLE status, and Google's "high demand" message
        // inside an HTTP-200 body (observed live 2026-07-05).
        match parse_hosted_response(br#"{"error":{"code":503,"message":"overloaded"}}"#) {
            Err(HostedCallError::RateLimited(_)) => {}
            _ => panic!("503 must classify retryable"),
        }
        match parse_hosted_response(
            br#"[{"error":{"status":"UNAVAILABLE","message":"The service is currently unavailable."}}]"#,
        ) {
            Err(HostedCallError::RateLimited(_)) => {}
            _ => panic!("UNAVAILABLE must classify retryable"),
        }
        match parse_hosted_response(
            br#"{"error":{"message":"This model is currently experiencing high demand. Spikes in demand are usually temporary. Please try again later."}}"#,
        ) {
            Err(HostedCallError::RateLimited(m)) => assert!(m.contains("high demand")),
            _ => panic!("high-demand shedding must classify retryable"),
        }
        match parse_hosted_response(br#"{"id":"x"}"#) {
            Err(HostedCallError::Other(e)) => {
                assert!(e.to_string().contains("missing choices[0].message"))
            }
            _ => panic!("choices-less body must fail loud"),
        }
        // Absent `content` under a PRESENT message object is Ok (#1222
        // packet 2) — some OpenAI-compat reasoning backends omit content
        // entirely on length-truncation; both consumers extract it as "".
        assert!(
            parse_hosted_response(br#"{"choices":[{"message":{"role":"assistant"},"finish_reason":"length"}]}"#).is_ok(),
            "message object without a content field must classify as Ok"
        );
        match parse_hosted_response(b"not json") {
            Err(HostedCallError::Other(e)) => assert!(e.to_string().contains("parsing")),
            _ => panic!("non-JSON must fail loud"),
        }
    }

    /// The endpoint field round-trips (a hand-written profiles file is the
    /// operator surface for this knob).
    #[test]
    fn model_endpoint_reasoning_effort_round_trips() {
        let e: darkmux_types::ModelEndpoint = serde_json::from_str(
            r#"{"url":"https://x/v1","reasoning_effort":"high"}"#,
        )
        .unwrap();
        assert_eq!(e.reasoning_effort.as_deref(), Some("high"));
        let none: darkmux_types::ModelEndpoint =
            serde_json::from_str(r#"{"url":"https://x/v1"}"#).unwrap();
        assert!(none.reasoning_effort.is_none());
    }

    // ─── #1177: doctor --probe (probe_remote_endpoint) ─────────────────

    /// One-shot HTTP mock on a real loopback socket: accepts a single
    /// connection, reads the full request (head + Content-Length body),
    /// responds with `body_json`, and hands the captured request back
    /// through the returned channel. No HTTP-mock dep — the probe path
    /// shells out to real `curl`, so a real socket is the honest test.
    fn one_shot_http_mock(
        body_json: &'static str,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write as IoWrite};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            // Read head, then exactly Content-Length body bytes.
            let (head_end, content_len) = loop {
                let n = stream.read(&mut chunk).unwrap();
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                    let cl = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    break (pos + 4, cl);
                }
            };
            while buf.len() < head_end + content_len {
                let n = stream.read(&mut chunk).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body_json.len(),
                body_json
            );
            stream.write_all(resp.as_bytes()).unwrap();
            let _ = tx.send(String::from_utf8_lossy(&buf).to_string());
        });
        (format!("http://127.0.0.1:{port}/v1"), rx)
    }

    #[test]
    fn probe_remote_endpoint_round_trips_and_reports_served_model_and_cost() {
        let (base_url, rx) = one_shot_http_mock(
            r#"{"model":"mock-model-v1","usage":{"total_tokens":7},"choices":[{"message":{"content":"ok"}}]}"#,
        );
        let ep = darkmux_types::ModelEndpoint {
            url: Some(base_url),
            ..Default::default() // no auth ⇒ Keychain untouched (CI-safe)
        };
        let report = probe_remote_endpoint(&ep, "probe-model-x", 15).unwrap();
        assert_eq!(report.served_model.as_deref(), Some("mock-model-v1"));
        assert_eq!(report.total_tokens, Some(7));
        // Host segment keeps the port (host:port form); model id closes the label.
        assert!(
            report.label.contains("127.0.0.1") && report.label.ends_with("/probe-model-x"),
            "label carries host + model: {}",
            report.label
        );
        // The probe mirrors dispatch_remote's request form — same parameter
        // name, the caller's model id, and the POST landing on the same
        // chat-completions path a real dispatch uses.
        let request = rx.recv().unwrap();
        assert!(request.contains("POST /v1/chat/completions"), "{request}");
        assert!(request.contains("\"max_completion_tokens\":64"), "{request}");
        assert!(request.contains("\"model\":\"probe-model-x\""), "{request}");
    }

    #[test]
    fn probe_remote_endpoint_surfaces_the_endpoint_error_verbatim() {
        // An HTTP-200-with-error-object body (curl without --fail also maps
        // real 4xx bodies through this same path) — the endpoint's own
        // message is the operator's diagnosis, so it must survive verbatim.
        let (base_url, _rx) = one_shot_http_mock(
            r#"{"error":{"message":"Access denied due to invalid subscription key"}}"#,
        );
        let ep = darkmux_types::ModelEndpoint {
            url: Some(base_url),
            ..Default::default()
        };
        let err = probe_remote_endpoint(&ep, "probe-model-x", 15).unwrap_err();
        assert!(
            err.to_string()
                .contains("Access denied due to invalid subscription key"),
            "endpoint error must surface verbatim: {err:#}"
        );
    }

    // ─── #888: auto-workspace cleanup guard ───────────────────────────

    #[test]
    fn auto_workspace_cleanup_removes_dir_when_armed_on_drop() {
        // Simulates an error/panic exit before the dispatch completed: the
        // armed guard reclaims the auto-allocated scratch workspace.
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("darkmux-dispatch-coder-123");
        std::fs::create_dir_all(workspace.join("subdir")).unwrap();
        std::fs::write(workspace.join("subdir/scratch.txt"), b"agent work").unwrap();
        {
            let _guard = AutoWorkspaceCleanup {
                workspace: Some(workspace.clone()),
                armed: true,
            };
        } // drop here
        assert!(!workspace.exists(), "armed guard must reclaim the workspace on drop");
    }

    #[test]
    fn auto_workspace_cleanup_retains_dir_when_disarmed() {
        // Simulates a completed dispatch: disarmed → workspace retained for
        // inspection (status quo).
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("darkmux-dispatch-coder-456");
        std::fs::create_dir_all(&workspace).unwrap();
        {
            let mut guard = AutoWorkspaceCleanup {
                workspace: Some(workspace.clone()),
                armed: true,
            };
            guard.disarm();
        } // drop here
        assert!(workspace.exists(), "disarmed guard must retain the workspace");
    }

    #[test]
    fn auto_workspace_cleanup_never_touches_operator_workdir() {
        // The `--workdir` case stores `None`, so an armed drop is a no-op —
        // an operator-provided path is never reclaimed by construction.
        let tmp = TempDir::new().unwrap();
        let operator_workdir = tmp.path().join("my-repo");
        std::fs::create_dir_all(&operator_workdir).unwrap();
        {
            let _guard = AutoWorkspaceCleanup {
                workspace: None,
                armed: true,
            };
        } // drop here — must NOT touch anything
        assert!(operator_workdir.exists(), "operator --workdir must never be cleaned");
    }

    // ─── #680: Docker-runtime preflight status → bail mapping ─────────

    #[test]
    fn preflight_ready_is_ok() {
        assert!(preflight_result_for(DockerRuntimeStatus::Ready).is_ok());
    }

    #[test]
    fn preflight_binary_missing_bails_with_install_hint() {
        let msg = preflight_result_for(DockerRuntimeStatus::BinaryMissing)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("isn't on PATH"), "{msg}");
        assert!(msg.contains("Install Docker Desktop"), "{msg}");
    }

    #[test]
    fn preflight_daemon_unreachable_surfaces_stderr_and_start_hint() {
        let msg = preflight_result_for(DockerRuntimeStatus::DaemonUnreachable(
            "Cannot connect to the Docker daemon".to_string(),
        ))
        .unwrap_err()
        .to_string();
        assert!(msg.contains("`docker version` failed"), "{msg}");
        assert!(msg.contains("Start Docker Desktop"), "{msg}");
        assert!(msg.contains("Cannot connect to the Docker daemon"), "{msg}");
    }

    #[test]
    fn preflight_image_missing_mentions_pull_then_build_fallback() {
        // ImageMissing is the fallback mapper message now — the live dispatch
        // path pulls the GHCR image on demand (#759) rather than bailing. The
        // message should name the GHCR pull as primary and build-from-source
        // as the fallback.
        let msg = preflight_result_for(DockerRuntimeStatus::ImageMissing)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("GHCR"), "{msg}");
        assert!(msg.contains("ghcr.io/kstrat2001/darkmux-runtime:"), "{msg}");
        assert!(
            msg.contains("docker build -t darkmux-runtime:latest runtime/"),
            "{msg}"
        );
    }

    #[test]
    fn ghcr_image_pins_to_the_crate_version() {
        // Pull-on-demand pins to the darkmux binary version so a `brew upgrade`
        // fetches the matching image (#759).
        assert_eq!(
            ghcr_runtime_image(),
            format!("ghcr.io/kstrat2001/darkmux-runtime:{}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn darkmux_runtime_image_recognized_no_inject() {
        // Both the local dev tag and any GHCR-published tag are darkmux's own
        // images (binary baked in → no injection). An operator `--image` is not.
        assert!(is_darkmux_runtime_image(RUNTIME_IMAGE));
        assert!(is_darkmux_runtime_image(&ghcr_runtime_image()));
        assert!(is_darkmux_runtime_image(
            "ghcr.io/kstrat2001/darkmux-runtime:1.2.3"
        ));
        assert!(!is_darkmux_runtime_image("rust:slim"));
        assert!(!is_darkmux_runtime_image("ubuntu:24.04"));
        // A lookalike repo prefix without the `:tag` separator must not match.
        assert!(!is_darkmux_runtime_image(
            "ghcr.io/kstrat2001/darkmux-runtime-evil:latest"
        ));
    }

    #[test]
    fn preflight_probe_error_bails_with_underlying_error() {
        // The one arm whose behavior changed (old `.context(...)?` → `anyhow!`).
        let msg = preflight_result_for(DockerRuntimeStatus::ProbeError("boom".to_string()))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("running `docker images`"), "{msg}");
        assert!(msg.contains("boom"), "{msg}");
    }

    // ─── #368: compaction-flag passthrough to runtime CLI ────────────

    // ─── out-of-band bookkeeping: volume mounts ──────────────────────

    #[test]
    fn read_token_totals_parses_metrics_json() {
        // metrics.json lives under <out_dir>/.darkmux-runtime/ — the same
        // out-dir the trajectory tailer reads. read_token_totals pulls the
        // runtime's recorded prompt/completion totals; total() is derived.
        let out = TempDir::new().unwrap();
        let rt = out.path().join(".darkmux-runtime");
        fs::create_dir_all(&rt).unwrap();
        fs::write(
            rt.join("metrics.json"),
            r#"{"total_prompt_tokens": 1200, "total_completion_tokens": 345, "turns": 4}"#,
        )
        .unwrap();
        let t = read_token_totals(out.path());
        assert_eq!(t.prompt, 1200);
        assert_eq!(t.completion, 345);
        assert_eq!(t.total(), 1545);
    }

    #[test]
    fn read_token_totals_degrades_to_zero_on_missing_or_malformed() {
        // Observability enrichment, never a dispatch-failing path: a missing
        // file (container died before writing) or malformed JSON yields zero
        // totals rather than erroring.
        let missing = TempDir::new().unwrap();
        let t = read_token_totals(missing.path());
        assert_eq!(t.total(), 0, "missing metrics.json → zero totals");

        let bad = TempDir::new().unwrap();
        let rt = bad.path().join(".darkmux-runtime");
        fs::create_dir_all(&rt).unwrap();
        fs::write(rt.join("metrics.json"), "{not valid json").unwrap();
        let t = read_token_totals(bad.path());
        assert_eq!(t.total(), 0, "malformed metrics.json → zero totals");
    }

    #[test]
    fn token_totals_total_saturates() {
        // Guard the derived sum against overflow on absurd inputs (the
        // runtime caps real totals far below this, but the helper must not
        // panic in a release build with overflow checks off — saturate).
        let t = TokenTotals { prompt: u32::MAX, completion: 10 };
        assert_eq!(t.total(), u32::MAX);
    }

    #[test]
    #[serial]
    fn config_path_reaches_dispatch_resolvers_not_just_env() {
        // (#984) Regression: the dispatch's resolvers called `load_registry(None)`,
        // so a lab `--profiles-file` silently used the default registry's model —
        // the flag reached lab run's own lookup but never the dispatch. With
        // `config_path` threaded, a resolver loads from the passed file even with
        // NO `DARKMUX_PROFILES` env. `resolve_context_window_internal` is the
        // simplest of the three resolvers to exercise; the model + utility
        // resolvers thread `config_path` identically. Fails on the old
        // `load_registry(None)`; passes on `load_registry(config_path)`.
        let tmp = TempDir::new().unwrap();
        let pf = tmp.path().join("profiles.json");
        std::fs::write(
            &pf,
            r#"{"profiles":{"probe":{"models":[{"id":"probe-model","n_ctx":12345,"role":"primary"}],"runtime":{"compaction":{"mode":"default"}}}},"default_profile":"probe"}"#,
        )
        .unwrap();
        // Clear the env so this proves the FLAG (config_path), not the env workaround.
        let prev = std::env::var("DARKMUX_PROFILES").ok();
        // SAFETY: serialized via #[serial]; restored below.
        unsafe { std::env::remove_var("DARKMUX_PROFILES") };
        let from_flag = resolve_context_window_internal(None, pf.to_str()).unwrap();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_PROFILES", v),
                None => std::env::remove_var("DARKMUX_PROFILES"),
            }
        }
        assert_eq!(
            from_flag,
            Some(12345),
            "config_path (lab --profiles-file) must reach the dispatch resolver, \
             not fall through to env/default (#984)"
        );
    }

    // ─── #1269: registry-load blast radius ──────────────────────────

    #[test]
    fn resolve_dispatch_model_internal_hard_fails_on_malformed_registry_no_probe_fallback() {
        // A genuine registry-LOAD failure (malformed JSON, not a bad crew)
        // must produce ONE clear, named hard error and never fall through to
        // the deprecated `probe_loaded_model()` — routing a broken config
        // file into an unrelated LMStudio probe just compounds the error.
        // If this test somehow DID fall through to probe_loaded_model(), it
        // would shell out to `curl`/LMStudio and either hang or fail in a
        // way unrelated to the assertion below — the error text alone
        // proves which path was taken.
        let tmp = TempDir::new().unwrap();
        let pf = tmp.path().join("profiles.json");
        std::fs::write(&pf, "this is not valid json at all").unwrap();

        let role: crate::types::Role = serde_json::from_str(
            r#"{"id":"r","description":"d","tool_palette":{"allow":[],"deny":[]},"escalation_contract":"bail-with-explanation"}"#,
        )
        .unwrap();

        let err = resolve_dispatch_model_internal(&role, None, pf.to_str(), false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not loadable"),
            "expected the hard-stop registry-load error, got: {msg}"
        );
        assert!(
            !msg.contains("falling back") && !msg.contains("probe_loaded_model()"),
            "must NOT mention the deprecated probe fallback for a load failure: {msg}"
        );
    }

    #[test]
    #[serial]
    fn resolve_context_window_internal_unaffected_by_invalid_sibling_crew() {
        // (#1269) A registry with one invalid crew (a remote-endpoint seat,
        // rejected only at `resolve_crew` time) must not budge dispatch-side
        // resolution of an UNRELATED profile — the exact blast-radius the
        // Studio hit in production. `resolve_context_window_internal` is the
        // LMStudio-free half of dispatch resolution (mirrors
        // `config_path_reaches_dispatch_resolvers_not_just_env` above);
        // model selection additionally touches `lms` to load the model,
        // which isn't safe to exercise in a unit test.
        let tmp = TempDir::new().unwrap();
        let pf = tmp.path().join("profiles.json");
        std::fs::write(
            &pf,
            r#"{"profiles":{
                    "fast":{"models":[{"id":"model-a","n_ctx":32000}]},
                    "cloud":{"models":[
                        {"id":"gpt-remote","n_ctx":100000,
                         "endpoint":{"url":"https://example.azure.com/openai"}}
                    ]}
                },
                "default_profile":"fast",
                "crews":{"bad":{"seats":{"review-probe":[{"profile":"cloud"}]}}}}"#,
        )
        .unwrap();
        let prev = std::env::var("DARKMUX_PROFILES").ok();
        // SAFETY: serialized via #[serial]; restored below.
        unsafe { std::env::remove_var("DARKMUX_PROFILES") };
        let window = resolve_context_window_internal(None, pf.to_str()).unwrap();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_PROFILES", v),
                None => std::env::remove_var("DARKMUX_PROFILES"),
            }
        }
        assert_eq!(
            window,
            Some(32000),
            "an invalid crew elsewhere in the registry must not affect \
             resolution of the sibling default profile (#1269)"
        );
    }

    // ─── #1282: quarantine-aware dispatch resolution ────────────────

    fn quarantine_test_role() -> crate::types::Role {
        serde_json::from_str(
            r#"{"id":"r","description":"d","tool_palette":{"allow":[],"deny":[]},"escalation_contract":"bail-with-explanation"}"#,
        )
        .unwrap()
    }

    #[test]
    fn resolve_dispatch_model_internal_bails_on_quarantined_requested_profile() {
        // (#1282) A REQUESTED profile whose registry entry was quarantined at
        // load must hard-fail with the entry's own parse error — pre-fix it
        // fell into the #1054 "not defined" fallback and dispatched the
        // default profile's model instead. If this test somehow DID take the
        // fallback, resolution would proceed toward LMStudio (`lms` load /
        // probe) and fail in a way unrelated to the assertions below — the
        // error text alone proves which path was taken.
        let tmp = TempDir::new().unwrap();
        let pf = tmp.path().join("profiles.json");
        std::fs::write(
            &pf,
            r#"{"profiles":{
                    "fast":{"models":[{"id":"model-a","n_ctx":32000}]},
                    "review":{"models":[{"n_ctx":32000}]}
                },
                "default_profile":"fast"}"#,
        )
        .unwrap();

        let err =
            resolve_dispatch_model_internal(&quarantine_test_role(), Some("review"), pf.to_str(), false)
                .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("quarantined"), "got: {msg}");
        assert!(msg.contains("\"review\""), "got: {msg}");
        assert!(msg.contains("missing field `id`"), "got: {msg}");
        assert!(msg.contains("darkmux doctor"), "got: {msg}");
    }

    #[test]
    fn resolve_dispatch_model_internal_bails_on_quarantined_default_profile_no_probe() {
        // (#1282) A quarantined `default_profile` must hard-fail — pre-fix,
        // `resolve_active` returned None and the code fell through to the
        // deprecated `probe_loaded_model()`, dispatching against whatever
        // LMStudio happened to have loaded. Same caveat as the malformed-
        // registry test above: on regression this would shell out toward
        // LMStudio; the error text proves the path.
        let tmp = TempDir::new().unwrap();
        let pf = tmp.path().join("profiles.json");
        std::fs::write(
            &pf,
            r#"{"profiles":{
                    "fast":{"models":[{"id":"model-a","n_ctx":32000}]},
                    "broken":{"models":[{"n_ctx":32000}]}
                },
                "default_profile":"broken"}"#,
        )
        .unwrap();

        let err = resolve_dispatch_model_internal(&quarantine_test_role(), None, pf.to_str(), false)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("quarantined"), "got: {msg}");
        assert!(msg.contains("\"broken\""), "got: {msg}");
        assert!(msg.contains("darkmux doctor"), "got: {msg}");
        assert!(
            !msg.contains("falling back") && !msg.contains("probe_loaded_model()"),
            "must NOT take the deprecated probe fallback for a quarantined default: {msg}"
        );
    }

    #[test]
    fn resolve_context_window_internal_bails_on_quarantined_profile() {
        // (#1282) The context-window resolver mirrors model selection: a
        // quarantined requested profile errs instead of silently sizing the
        // window from the default profile; a quarantined default errs
        // instead of reporting "no window" for an entry that IS in the file.
        let tmp = TempDir::new().unwrap();
        let pf = tmp.path().join("profiles.json");
        std::fs::write(
            &pf,
            r#"{"profiles":{
                    "fast":{"models":[{"id":"model-a","n_ctx":32000}]},
                    "review":{"models":[{"n_ctx":32000}]}
                },
                "default_profile":"fast"}"#,
        )
        .unwrap();
        let err = resolve_context_window_internal(Some("review"), pf.to_str()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("quarantined") && msg.contains("\"review\""), "got: {msg}");

        // A healthy requested profile on the same registry still resolves.
        let window = resolve_context_window_internal(Some("fast"), pf.to_str()).unwrap();
        assert_eq!(window, Some(32000));
    }

    #[test]
    fn apply_volume_mounts_emits_workspace_and_out_dir() {
        // The runtime's OWN bookkeeping goes to `/darkmux-out`, SEPARATE
        // from the agent's `/workspace`. This literal MUST stay in sync
        // with `runtime::trajectory::RUNTIME_OUT_BASE` (the two crates
        // can't share the const — the runtime is built into the image).
        let mut args: Vec<String> = Vec::new();
        apply_volume_mounts(
            &mut args,
            Path::new("/host/workspace"),
            Path::new("/host/out"),
        );
        assert_eq!(
            args,
            vec![
                "-v",
                "/host/workspace:/workspace",
                "-v",
                "/host/out:/darkmux-out",
            ],
            "both binds present; out-dir mounts at /darkmux-out"
        );
        // Defensive: the runtime must NOT be told to write its
        // bookkeeping into the workspace tree.
        assert!(
            !args
                .iter()
                .any(|a| a.ends_with(":/workspace") && a.contains("/host/out")),
            "out-dir must not be mounted at /workspace"
        );
    }

    #[test]
    fn apply_runtime_injection_mounts_binary_and_overrides_entrypoint() {
        // (#703) Injecting into a non-default image: bind the static binary
        // read-only at /darkmux-runtime and force the entrypoint to it. These
        // are docker-run OPTIONS (must precede the image arg).
        let mut args: Vec<String> = Vec::new();
        apply_runtime_injection(&mut args, Path::new("/home/op/.darkmux/runtime/darkmux-runtime"));
        assert_eq!(
            args,
            vec![
                "-v",
                "/home/op/.darkmux/runtime/darkmux-runtime:/darkmux-runtime:ro",
                "--entrypoint",
                "/darkmux-runtime",
            ],
            "binary bound read-only at /darkmux-runtime; entrypoint overridden to it"
        );
    }

    #[test]
    fn apply_cache_mount_binds_cache_and_points_package_managers_at_it() {
        // (#703 Slice 3) Shared toolchain cache mounted at /darkmux-cache with
        // CARGO_HOME / npm / pip env redirected so the inner loop reuses
        // downloads across dispatches.
        let mut args: Vec<String> = Vec::new();
        apply_cache_mount(&mut args, Path::new("/home/op/.darkmux/cache"));
        assert_eq!(
            args,
            vec![
                "-v",
                "/home/op/.darkmux/cache:/darkmux-cache",
                "-e",
                "CARGO_HOME=/darkmux-cache/cargo",
                "-e",
                "npm_config_cache=/darkmux-cache/npm",
                "-e",
                "PIP_CACHE_DIR=/darkmux-cache/pip",
            ],
            "cache bound at /darkmux-cache; cargo/npm/pip caches redirected into it"
        );
    }

    // ─── #839 + #842: full docker-run argv assertion ──────────────

    #[test]
    fn build_docker_run_argv_asserts_complete_vector() {
        // (#842) Representative dispatch: non-default image (inject=true),
        // with compaction, allowed tools, and json mode. Asserts the
        // COMPLETE argv vector including all hardening flags (#839).
        let config = DockerRunConfig {
            output_schema: None,
            container_name: "darkmux-dispatch-test-123".to_string(),
            workspace: PathBuf::from("/host/workspace"),
            host_out: PathBuf::from("/host/out"),
            inject: true,
            runtime_binary: Some(PathBuf::from(
                "/home/op/.darkmux/runtime/darkmux-runtime",
            )),
            image: "rust:slim".to_string(),
            model: "llama3-8b".to_string(),
            system_prompt: "You are a coding assistant.".to_string(),
            message: "Fix the bug in main.rs".to_string(),
            json: true,
            allowed_tools: Some(vec!["exec".to_string(), "edit".to_string()]),
            compaction: crate::dispatch::CompactionDispatchArgs {
                threshold_tokens: Some(4096),
                compactor_model: Some("util-model".to_string()),
                threshold_ratio: Some(0.75),
                context_window: Some(32000),
                // All six compaction flags set so each emission is pinned —
                // the regression that shipped a wrong flag name + dropped
                // three of these (strategy/bail/custom) is exactly what an
                // all-fields-set assertion catches.
                strategy: Some(darkmux_types::CompactionStrategy::StructuredSlot),
                bail_after_compactions: Some(10u32),
                custom_instructions: Some("Be terse.".to_string()),
            },
            feedback_templates: serde_json::json!({
                "error": "An error occurred."
            }),
            cache_dir: PathBuf::from("/home/op/.darkmux/cache"),
            remote_chat_url: None,
            remote_needs_auth: false,
            base_url_override: None,
        };

        let argv = build_docker_run_argv(&config);

        // 1. Verify the hardening flags are present (#839)
        assert!(
            argv.contains(&format!("--cap-drop={}", DOCKER_CAP_DROP)),
            "must include --cap-drop ALL"
        );
        assert!(
            argv.contains(&format!("--security-opt={}", DOCKER_SECURITY_OPT)),
            "must include --security-opt no-new-privileges"
        );
        assert!(
            argv.contains(&format!("--pids-limit={}", DOCKER_PIDS_LIMIT)),
            "must include --pids-limit 512"
        );
        assert!(
            argv.contains(&format!("--memory={}", DOCKER_MEMORY)),
            "must include --memory 4g"
        );

        // 2. Verify the full argv structure
        assert_eq!(argv[0], "docker");
        assert_eq!(argv[1], "run");
        assert_eq!(argv[2], "--rm");
        assert_eq!(argv[3], "--name");
        assert_eq!(argv[4], "darkmux-dispatch-test-123");

        // 3. Verify hardening flags follow immediately after --name
        assert_eq!(argv[5], format!("--cap-drop={}", DOCKER_CAP_DROP));
        assert_eq!(argv[6], format!("--security-opt={}", DOCKER_SECURITY_OPT));
        assert_eq!(argv[7], format!("--pids-limit={}", DOCKER_PIDS_LIMIT));
        assert_eq!(argv[8], format!("--memory={}", DOCKER_MEMORY));

        // 4. Verify volume mounts (workspace + out-dir)
        assert_eq!(argv[9], "-v");
        assert_eq!(argv[10], "/host/workspace:/workspace");
        assert_eq!(argv[11], "-v");
        assert_eq!(argv[12], "/host/out:/darkmux-out");

        // 5. Verify cache mount + env vars (real host:container bind, not a
        // bare anonymous volume).
        assert_eq!(argv[13], "-v");
        assert_eq!(argv[14], "/home/op/.darkmux/cache:/darkmux-cache");
        assert_eq!(argv[15], "-e");
        assert_eq!(argv[16], "CARGO_HOME=/darkmux-cache/cargo");
        assert_eq!(argv[17], "-e");
        assert_eq!(argv[18], "npm_config_cache=/darkmux-cache/npm");
        assert_eq!(argv[19], "-e");
        assert_eq!(argv[20], "PIP_CACHE_DIR=/darkmux-cache/pip");

        // 6. Verify runtime injection (non-default image)
        assert_eq!(argv[21], "-v");
        assert_eq!(
            argv[22],
            "/home/op/.darkmux/runtime/darkmux-runtime:/darkmux-runtime:ro"
        );
        assert_eq!(argv[23], "--entrypoint");
        assert_eq!(argv[24], "/darkmux-runtime");

        // 7. Verify `--` + image + runtime CLI args
        assert_eq!(argv[25], "--");
        assert_eq!(argv[26], "rust:slim"); // image
        assert_eq!(argv[27], "run"); // runtime subcommand
        assert_eq!(argv[28], "--model");
        assert_eq!(argv[29], "llama3-8b");
        assert_eq!(argv[30], "--system");
        assert_eq!(argv[31], "You are a coding assistant.");
        // (#386) The message goes via the out-dir mount, not argv — argv carries
        // the constant `--prompt-file <container path>`, never the brief itself.
        assert_eq!(argv[32], "--prompt-file");
        assert_eq!(argv[33], "/darkmux-out/.prompt.txt");
        assert!(
            !argv.iter().any(|a| a == "Fix the bug in main.rs"),
            "the message must NOT appear anywhere in the docker argv (#386): {argv:?}"
        );

        // 8. Verify json flag
        assert_eq!(argv[34], "--json");

        // 9. Verify allowed tools
        assert_eq!(argv[35], "--allowed-tools");
        assert_eq!(argv[36], "exec,edit");

        // 10. Verify compaction flags — flag names must match the runtime's
        // accepted set verbatim (an unknown flag exits the container with 2).
        assert_eq!(argv[37], "--compact-threshold-tokens");
        assert_eq!(argv[38], "4096");
        assert_eq!(argv[39], "--compactor-model");
        assert_eq!(argv[40], "util-model");
        assert_eq!(argv[41], "--compact-threshold-ratio");
        assert_eq!(argv[42], "0.75");
        assert_eq!(argv[43], "--context-window");
        assert_eq!(argv[44], "32000");
        assert_eq!(argv[45], "--compact-strategy");
        assert_eq!(argv[46], "structured-slot");
        assert_eq!(argv[47], "--bail-after-compactions");
        assert_eq!(argv[48], "10");
        assert_eq!(argv[49], "--compactor-custom-instructions");
        assert_eq!(argv[50], "Be terse.");

        // 11. Verify feedback templates JSON
        assert_eq!(argv[51], "--feedback-templates-json");
        // The JSON value should contain the error template
        assert!(argv[52].contains("error"));
        assert!(argv[52].contains("An error occurred"));

        // Total arg count: 53 (0..=52)
        assert_eq!(argv.len(), 53);
    }

    #[test]
    fn build_docker_run_argv_minimal_dispatch_no_injection() {
        // Minimal dispatch: default darkmux image (inject=false), no
        // compaction, no allowed tools, no json — asserts that optional
        // flags are omitted when not set.
        let config = DockerRunConfig {
            output_schema: None,
            container_name: "darkmux-dispatch-min".to_string(),
            workspace: PathBuf::from("/tmp/ws"),
            host_out: PathBuf::from("/tmp/out"),
            inject: false,
            runtime_binary: None,
            image: "darkmux-runtime:latest".to_string(),
            model: "default-model".to_string(),
            system_prompt: "Basic role.".to_string(),
            message: "Hello world".to_string(),
            json: false,
            allowed_tools: None,
            compaction: crate::dispatch::CompactionDispatchArgs::default(),
            feedback_templates: serde_json::Value::Null,
            cache_dir: PathBuf::from("/tmp/cache"),
            remote_chat_url: None,
            remote_needs_auth: false,
            base_url_override: None,
        };

        let argv = build_docker_run_argv(&config);

        // Should NOT contain optional flags
        assert!(
            !argv.contains(&"--json".to_string()),
            "minimal dispatch should NOT have --json"
        );
        assert!(
            !argv.iter().any(|a| a.starts_with("--allowed-tools")),
            "minimal dispatch should NOT have --allowed-tools"
        );
        assert!(
            !argv.iter().any(|a| a.starts_with("--compact")),
            "minimal dispatch should NOT have --compact-* flags"
        );
        assert!(
            !argv.iter().any(|a| a.starts_with("--compactor-model")),
            "minimal dispatch should NOT have --compactor-model"
        );
        assert!(
            !argv.iter().any(|a| a.starts_with("--context-window")),
            "minimal dispatch should NOT have --context-window"
        );

        // Should still contain hardening flags
        assert!(argv.contains(&format!("--cap-drop={}", DOCKER_CAP_DROP)));
        assert!(argv.contains(&format!("--security-opt={}", DOCKER_SECURITY_OPT)));
        assert!(argv.contains(&format!("--pids-limit={}", DOCKER_PIDS_LIMIT)));
        assert!(argv.contains(&format!("--memory={}", DOCKER_MEMORY)));

        // Should NOT contain injection args (no binary mount/entrypoint)
        assert!(
            !argv.iter().any(|a| a.contains("/darkmux-runtime:ro")),
            "minimal dispatch should NOT have runtime binary mount"
        );

        // Verify image and model are present after --
        let dash_idx = argv.iter().position(|a| *a == "--").unwrap();
        assert_eq!(argv[dash_idx + 1], "darkmux-runtime:latest");
        assert_eq!(argv[dash_idx + 2], "run");

        // (#1038) No output_schema ⇒ no --response-schema flag.
        assert!(
            !argv.iter().any(|a| a == "--response-schema"),
            "absent output_schema must NOT emit --response-schema"
        );
    }

    #[test]
    fn build_docker_run_argv_output_schema_emits_response_schema_flag() {
        // (#1038) The grammar-constrained-output branch: a Some(output_schema)
        // must serialize to a `--response-schema <json>` flag pair in the argv.
        // The whole feature rides on this flag reaching the runtime — exercise
        // the live branch, not just the absent case (the #975 lesson: assert
        // the real construction, not only the omit path).
        let schema = serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "verdict": { "type": "string" } },
            "required": ["verdict"]
        });
        let config = DockerRunConfig {
            output_schema: Some(schema.clone()),
            container_name: "darkmux-dispatch-schema".to_string(),
            workspace: PathBuf::from("/tmp/ws"),
            host_out: PathBuf::from("/tmp/out"),
            inject: false,
            runtime_binary: None,
            image: "darkmux-runtime:latest".to_string(),
            model: "default-model".to_string(),
            system_prompt: "Tool-less reviewer.".to_string(),
            message: "Review this.".to_string(),
            json: true,
            allowed_tools: None,
            compaction: crate::dispatch::CompactionDispatchArgs::default(),
            feedback_templates: serde_json::Value::Null,
            cache_dir: PathBuf::from("/tmp/cache"),
            remote_chat_url: None,
            remote_needs_auth: false,
            base_url_override: None,
        };

        let argv = build_docker_run_argv(&config);

        let idx = argv
            .iter()
            .position(|a| a == "--response-schema")
            .expect("Some(output_schema) must emit --response-schema");
        // The flag's value is the schema serialized as a single JSON string,
        // and it must round-trip back to the exact schema (no corruption).
        let value = &argv[idx + 1];
        let parsed: serde_json::Value =
            serde_json::from_str(value).expect("--response-schema value must be valid JSON");
        assert_eq!(parsed, schema, "schema must round-trip through argv intact");
    }

    // ─── #842 edge cases the complete-vector test doesn't exercise ──────
    // The complete-vector test only ever runs StructuredSlot + a non-empty
    // allowed-tools vec + a non-empty feedback object. These pin the three
    // remaining branches: the Narrative→kebab mapping, the
    // Some(empty)-vs-None allowed-tools fork (block-all vs allow-all), and
    // the empty-feedback-object guard.

    /// A minimal valid config: no optional flags set. Each test below flips
    /// exactly one field so the assertion isolates that branch.
    fn base_argv_config() -> DockerRunConfig {
        DockerRunConfig {
            output_schema: None,
            container_name: "darkmux-edge".to_string(),
            workspace: PathBuf::from("/tmp/ws"),
            host_out: PathBuf::from("/tmp/out"),
            inject: false,
            runtime_binary: None,
            image: "darkmux-runtime:latest".to_string(),
            model: "m".to_string(),
            system_prompt: "role".to_string(),
            message: "msg".to_string(),
            json: false,
            allowed_tools: None,
            compaction: crate::dispatch::CompactionDispatchArgs::default(),
            feedback_templates: serde_json::Value::Null,
            cache_dir: PathBuf::from("/tmp/cache"),
            remote_chat_url: None,
            remote_needs_auth: false,
            base_url_override: None,
        }
    }

    // ─── #1187: agentic-remote argv emission ─────────────────────

    #[test]
    fn build_docker_run_argv_omits_remote_flags_when_not_remote() {
        let argv = build_docker_run_argv(&base_argv_config());
        assert!(
            !argv
                .iter()
                .any(|a| a == "--chat-url" || a == "--auth-header-stdin" || a == "-i"),
            "a local dispatch must not emit any remote-brain flags: {argv:?}"
        );
    }

    #[test]
    fn build_docker_run_argv_emits_chat_url_without_stdin_flags_when_no_auth() {
        let mut config = base_argv_config();
        config.remote_chat_url = Some("https://api.openai.com/v1/chat/completions".to_string());
        config.remote_needs_auth = false;
        let argv = build_docker_run_argv(&config);
        assert!(
            argv.windows(2).any(|w| w[0] == "--chat-url"
                && w[1] == "https://api.openai.com/v1/chat/completions"),
            "expected --chat-url with the exact URL: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a == "--auth-header-stdin" || a == "-i"),
            "no auth needed ⇒ no stdin-piping flags: {argv:?}"
        );
    }

    #[test]
    fn build_docker_run_argv_emits_stdin_flags_when_auth_needed() {
        let mut config = base_argv_config();
        config.remote_chat_url = Some(
            "https://x.cognitiveservices.azure.com/openai/deployments/gpt-4o/chat/completions?api-version=2025-01-01-preview"
                .to_string(),
        );
        config.remote_needs_auth = true;
        let argv = build_docker_run_argv(&config);
        assert!(
            argv.iter().any(|a| a == "-i"),
            "expected -i (keep stdin open) so the container can receive the auth header: {argv:?}"
        );
        assert!(
            argv.iter().any(|a| a == "--auth-header-stdin"),
            "expected --auth-header-stdin (no value — never a file path): {argv:?}"
        );
        // The secret NEVER appears on argv at all — it's piped over stdin
        // post-spawn, not passed as a flag value of any kind.
        assert!(
            !argv.iter().any(|a| a.contains("api-key") || a.contains("Bearer") || a.contains(".auth-header")),
            "argv must never carry the auth header name/value or a file path: {argv:?}"
        );
    }

    // ─── #1187: role_wants_agentic_remote routing ────────────────

    #[test]
    fn role_wants_agentic_remote_true_for_nonempty_allow() {
        use crate::types::{EscalationContract, Role, ToolPalette};
        let role = Role {
            output_schema: None,
            id: "code-reviewer".into(),
            description: "test".into(),
            skills: vec![],
            tool_palette: ToolPalette {
                allow: vec!["read".into(), "exec".into()],
                deny: vec![],
            },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: None,
            feedback_templates: None,
        };
        assert!(role_wants_agentic_remote(&role));
    }

    #[test]
    fn role_wants_agentic_remote_false_for_empty_allow() {
        use crate::types::{EscalationContract, Role, ToolPalette};
        let role = Role {
            output_schema: None,
            id: "pr-reviewer".into(),
            description: "test".into(),
            skills: vec![],
            tool_palette: ToolPalette::default(), // empty allow — the pr-reviewer shape
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: None,
            feedback_templates: None,
        };
        assert!(
            !role_wants_agentic_remote(&role),
            "a tool-less role must stay on the light single-shot remote path"
        );
    }

    // ─── #1187: remote auth-header stdin write ───────────────────

    /// (#1187) The stdin design's whole point is that NOTHING lands on any
    /// filesystem — verify by writing to an in-memory pipe (not a temp file)
    /// and reading the other end back, confirming the exact JSON shape a
    /// `bash`-capable model would never be able to intercept because it
    /// never exists as a file.
    #[test]
    fn write_remote_auth_header_stdin_sends_expected_json_over_the_pipe() {
        use std::io::Read;
        use std::process::{Command, Stdio};
        // `cat` echoes stdin to stdout — a stand-in for the container
        // reading its own stdin at startup, without needing a real
        // darkmux-runtime binary in this unit test.
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawning cat");
        let mut stdin = child.stdin.take().expect("stdin piped");
        write_remote_auth_header_stdin(&mut stdin, "api-key", "super-secret-do-not-log").unwrap();
        drop(stdin); // EOF — cat exits once its input closes

        let mut out = String::new();
        child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
        child.wait().unwrap();

        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["header"], "api-key");
        assert_eq!(v["value"], "super-secret-do-not-log");
    }

    #[test]
    fn build_docker_run_argv_compaction_strategy_narrative_is_kebab() {
        // Only StructuredSlot is exercised by the complete-vector test; a typo
        // in the Narrative arm (line ~333) would ship green. The runtime
        // rejects an unknown flag value, so the kebab string must be exact.
        let mut config = base_argv_config();
        config.compaction.strategy = Some(darkmux_types::CompactionStrategy::Narrative);
        let argv = build_docker_run_argv(&config);
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--compact-strategy" && w[1] == "narrative"),
            "Narrative must map to the kebab `narrative`, got: {argv:?}"
        );
        // And NOT the Debug-derived PascalCase, which the runtime would reject.
        assert!(
            !argv.iter().any(|a| a == "Narrative"),
            "must not emit the enum's Debug form: {argv:?}"
        );
    }

    #[test]
    fn build_docker_run_argv_empty_allowed_tools_is_block_all_not_omitted() {
        // Some(vec![]) and None are DIFFERENT contracts: Some(empty) emits
        // `--allowed-tools ""` (block-all — a sandbox-bounding semantic), None
        // omits the flag (full catalog). A bug collapsing them is sandbox-
        // adjacent, so pin that the two configs produce different argv.
        let mut empty = base_argv_config();
        empty.allowed_tools = Some(vec![]);
        let empty_argv = build_docker_run_argv(&empty);
        let pos = empty_argv
            .iter()
            .position(|a| a == "--allowed-tools")
            .expect("Some(empty) must still emit --allowed-tools");
        assert_eq!(
            empty_argv[pos + 1], "",
            "block-all is the empty CSV, got: {:?}",
            empty_argv[pos + 1]
        );

        let none_argv = build_docker_run_argv(&base_argv_config());
        assert!(
            !none_argv.iter().any(|a| a == "--allowed-tools"),
            "None must omit the flag entirely (allow-all): {none_argv:?}"
        );
    }

    #[test]
    fn build_docker_run_argv_empty_feedback_object_omits_flag() {
        // The guard is `as_object().is_some_and(|o| !o.is_empty())`. An empty
        // object must NOT emit the flag (it would be a useless empty payload);
        // a non-empty object must. Pin both sides of the guard.
        let mut empty = base_argv_config();
        empty.feedback_templates = serde_json::json!({});
        assert!(
            !build_docker_run_argv(&empty)
                .iter()
                .any(|a| a == "--feedback-templates-json"),
            "empty feedback object must omit the flag"
        );

        let mut filled = base_argv_config();
        filled.feedback_templates = serde_json::json!({ "cycle": "regroup" });
        assert!(
            build_docker_run_argv(&filled)
                .iter()
                .any(|a| a == "--feedback-templates-json"),
            "non-empty feedback object must emit the flag"
        );
    }

    #[test]
    fn docker_command_from_argv_uses_argv0_as_program_not_an_arg() {
        // (#975) Regression: the consumer must build the Command as
        // program=argv[0] + args=argv[1..], NOT push the whole vector. Pushing
        // argv[0] ("docker") as an argument ran `docker docker run …`, which
        // docker rejected with exit 125 — the core internal-runtime dispatch was
        // dead in 1.3.x + 1.4.0. This is the missing test layer #842 named: it
        // inspects the REAL Command the dispatch executes, not just
        // build_docker_run_argv's output vector (which the other tests cover).
        let config = DockerRunConfig {
            output_schema: None,
            container_name: "darkmux-dispatch-reg".to_string(),
            workspace: PathBuf::from("/tmp/ws"),
            host_out: PathBuf::from("/tmp/out"),
            inject: false,
            runtime_binary: None,
            image: "darkmux-runtime:latest".to_string(),
            model: "default-model".to_string(),
            system_prompt: "Basic role.".to_string(),
            message: "Hello world".to_string(),
            json: false,
            allowed_tools: None,
            compaction: crate::dispatch::CompactionDispatchArgs::default(),
            feedback_templates: serde_json::Value::Null,
            cache_dir: PathBuf::from("/tmp/cache"),
            remote_chat_url: None,
            remote_needs_auth: false,
            base_url_override: None,
        };
        let argv = build_docker_run_argv(&config);
        let cmd = docker_command_from_argv(&argv);

        // Program is `docker`, and the arguments start at `run` — never a
        // spurious leading `docker`.
        assert_eq!(cmd.get_program().to_str().unwrap(), "docker");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args.first().map(String::as_str),
            Some("run"),
            "args must start at `run`, not a second `docker`: {args:?}"
        );
        assert_eq!(args.get(1).map(String::as_str), Some("--rm"));
        assert!(
            !args.contains(&"docker".to_string()),
            "no spurious `docker` in the arguments (the #975 `docker docker run` bug): {args:?}"
        );
    }

    // ─── #408: strict-selection opt-in parsing ───────────────────────

    #[serial]
    #[test]
    fn strict_selection_enabled_reads_env_truthy_values() {
        let prev = std::env::var("DARKMUX_STRICT_SELECTION").ok();

        unsafe { std::env::remove_var("DARKMUX_STRICT_SELECTION"); }
        assert!(!strict_selection_enabled(), "unset ⇒ off (back-compat default)");

        for truthy in ["1", "true", "TRUE", "Yes", " on "] {
            unsafe { std::env::set_var("DARKMUX_STRICT_SELECTION", truthy); }
            assert!(strict_selection_enabled(), "`{truthy}` should enable strict mode");
        }
        for falsy in ["0", "false", "no", "off", ""] {
            unsafe { std::env::set_var("DARKMUX_STRICT_SELECTION", falsy); }
            assert!(!strict_selection_enabled(), "`{falsy}` should NOT enable strict mode");
        }

        match prev {
            Some(v) => unsafe { std::env::set_var("DARKMUX_STRICT_SELECTION", v) },
            None => unsafe { std::env::remove_var("DARKMUX_STRICT_SELECTION") },
        }
    }

    #[test]
    fn apply_compaction_flags_omits_when_all_none() {
        let mut args: Vec<String> = Vec::new();
        let compaction = crate::dispatch::CompactionDispatchArgs::default();
        apply_compaction_flags(&mut args, &compaction);
        assert!(
            !args.iter().any(|a| a.starts_with("--compact") || a == "--context-window"),
            "default config should emit no compaction flags; got {args:?}"
        );
    }

    #[test]
    fn apply_compaction_flags_emits_threshold_when_set() {
        let mut args: Vec<String> = Vec::new();
        let compaction = crate::dispatch::CompactionDispatchArgs {
            threshold_tokens: Some(35_000),
            ..Default::default()
        };
        apply_compaction_flags(&mut args, &compaction);
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--compact-threshold-tokens" && w[1] == "35000"),
            "expected --compact-threshold-tokens 35000; got {args:?}"
        );
    }

    #[test]
    fn ensure_context_window_fills_when_none() {
        let mut c = crate::dispatch::CompactionDispatchArgs::default();
        assert_eq!(c.context_window, None);
        ensure_context_window(&mut c, Some(101_000));
        assert_eq!(c.context_window, Some(101_000));
    }

    #[test]
    fn ensure_context_window_preserves_existing() {
        let mut c = crate::dispatch::CompactionDispatchArgs {
            context_window: Some(50_000),
            ..Default::default()
        };
        ensure_context_window(&mut c, Some(101_000));
        assert_eq!(
            c.context_window,
            Some(50_000),
            "an already-set window must win over the fallback"
        );
    }

    #[test]
    fn ensure_context_window_stays_none_without_fallback() {
        let mut c = crate::dispatch::CompactionDispatchArgs::default();
        ensure_context_window(&mut c, None);
        assert_eq!(c.context_window, None);
    }

    // (#632 regression) A `default()` compaction — what bare `crew dispatch`
    // and the lab `prompt` provider build — must emit `--context-window`
    // once the guard fills it, so the runtime can derive its compaction
    // threshold instead of hard-erroring.
    #[test]
    fn ensure_context_window_then_apply_emits_flag() {
        let mut args: Vec<String> = Vec::new();
        let mut compaction = crate::dispatch::CompactionDispatchArgs::default();
        ensure_context_window(&mut compaction, Some(262_144));
        apply_compaction_flags(&mut args, &compaction);
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--context-window" && w[1] == "262144"),
            "expected --context-window 262144 after the guard; got {args:?}"
        );
    }

    #[test]
    fn apply_compaction_flags_emits_all_when_set() {
        let mut args: Vec<String> = Vec::new();
        let compaction = crate::dispatch::CompactionDispatchArgs {
            threshold_tokens: Some(45_000),
            compactor_model: Some("custom-compactor".to_string()),
            threshold_ratio: Some(0.35),
            context_window: Some(101_000),
            strategy: Some(darkmux_types::CompactionStrategy::StructuredSlot),
            bail_after_compactions: None,
            custom_instructions: None,
        };
        apply_compaction_flags(&mut args, &compaction);
        assert!(args.iter().any(|a| a == "--compact-threshold-tokens"));
        assert!(args.iter().any(|a| a == "45000"));
        assert!(args.iter().any(|a| a == "--compactor-model"));
        assert!(args.iter().any(|a| a == "custom-compactor"));
        assert!(args.iter().any(|a| a == "--compact-threshold-ratio"));
        assert!(args.iter().any(|a| a == "--context-window"));
        assert!(args.iter().any(|a| a == "101000"));
        assert!(args.iter().any(|a| a == "--compact-strategy"));
        assert!(args.iter().any(|a| a == "structured-slot"));
    }

    /// (#377) Escalation bound emits `--bail-after-compactions N`
    /// when set; omitted when None (back-compat with pre-#377 runtime
    /// + back-compat with operators who haven't configured the bound).
    #[test]
    fn apply_compaction_flags_emits_bail_when_set() {
        let mut args: Vec<String> = Vec::new();
        let compaction = crate::dispatch::CompactionDispatchArgs {
            bail_after_compactions: Some(3),
            custom_instructions: None,
            ..Default::default()
        };
        apply_compaction_flags(&mut args, &compaction);
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--bail-after-compactions" && w[1] == "3"),
            "expected --bail-after-compactions 3; got {args:?}"
        );
    }

    #[test]
    fn apply_compaction_flags_omits_bail_when_none() {
        let mut args: Vec<String> = Vec::new();
        let compaction = crate::dispatch::CompactionDispatchArgs::default();
        apply_compaction_flags(&mut args, &compaction);
        assert!(
            !args.iter().any(|a| a == "--bail-after-compactions"),
            "no bail flag should appear when bail_after_compactions is None; got {args:?}"
        );
    }

    /// (#383) Custom instructions emit `--compactor-custom-instructions
    /// <text>` when set; omitted when None. Schema-isolation contract:
    /// the typed `profile.runtime.compaction.custom_instructions` is
    /// the only source the runtime sees — extras["customInstructions"]
    /// is dead-letter (handled by the `from_profile_ignores_extras_*`
    /// tests below).
    #[test]
    fn apply_compaction_flags_emits_custom_instructions_when_set() {
        let mut args: Vec<String> = Vec::new();
        let compaction = crate::dispatch::CompactionDispatchArgs {
            custom_instructions: Some(
                "Preserve verbatim X / list active files with what was learned".into(),
            ),
            ..Default::default()
        };
        apply_compaction_flags(&mut args, &compaction);
        assert!(
            args.windows(2).any(|w| w[0] == "--compactor-custom-instructions"
                && w[1] == "Preserve verbatim X / list active files with what was learned"),
            "expected --compactor-custom-instructions with operator text; got {args:?}"
        );
    }

    #[test]
    fn apply_compaction_flags_omits_custom_instructions_when_none() {
        let mut args: Vec<String> = Vec::new();
        let compaction = crate::dispatch::CompactionDispatchArgs::default();
        apply_compaction_flags(&mut args, &compaction);
        assert!(
            !args.iter().any(|a| a == "--compactor-custom-instructions"),
            "no custom-instructions flag should appear when None; got {args:?}"
        );
    }

    /// (#372 T2-C) Strategy alone (no other overrides) still emits
    /// just `--compact-strategy <kebab>` so the runtime can pick up
    /// the operator's tier-2 opt-in without requiring the operator
    /// to also override threshold/model/etc.
    #[test]
    fn apply_compaction_flags_strategy_only_emits_just_strategy_flag() {
        let mut args: Vec<String> = Vec::new();
        let compaction = crate::dispatch::CompactionDispatchArgs {
            strategy: Some(darkmux_types::CompactionStrategy::StructuredSlot),
            ..Default::default()
        };
        apply_compaction_flags(&mut args, &compaction);
        assert!(args.windows(2).any(|w| w[0] == "--compact-strategy" && w[1] == "structured-slot"));
        // Only the strategy flag should be present.
        assert!(!args.iter().any(|a| a == "--compact-threshold-tokens"));
        assert!(!args.iter().any(|a| a == "--context-window"));
    }

    #[test]
    fn from_profile_reads_typed_strategy_field() {
        use darkmux_types::{
            CompactionStrategy, Profile, ProfileModel, ProfileRuntime,
            RuntimeCompactionConfig,
        };
        let profile = Profile {
            extras: Default::default(),
            description: None,
            default_model: None,
            models: vec![ProfileModel {
                endpoint: None,
                extras: Default::default(),
                id: "primary".into(),
                n_ctx: Some(100_000),
                capabilities: Default::default(),
                identifier: None,
            }],
            runtime: Some(ProfileRuntime {
                context_tokens: None,
                compaction: Some(RuntimeCompactionConfig {
                    strategy: Some(CompactionStrategy::StructuredSlot),
                    threshold_tokens: None,
                    threshold_ratio: None,
                    tier1: None,
                    tier2: None,
                    reserve: None,
                    custom_instructions: None,
                    extras: Default::default(),
                }),
            }),
            use_when: None,
        };
        let args = crate::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert_eq!(args.strategy, Some(CompactionStrategy::StructuredSlot));
    }

    /// (#377) `from_profile` reads `compaction.reserve.bail_after_compactions`
    /// (typed field that landed in #357) and surfaces it on
    /// `CompactionDispatchArgs` so apply_compaction_flags can plumb the
    /// `--bail-after-compactions N` CLI flag to the runtime. Profile-
    /// level only at this chunk; per-role override comes in chunk 4.
    #[test]
    fn from_profile_derives_bail_after_compactions_from_reserve() {
        use darkmux_types::{
            Profile, ProfileModel, ProfileRuntime, ReserveConfig,
            RuntimeCompactionConfig,
        };
        let profile = Profile {
            extras: Default::default(),
            description: None,
            default_model: None,
            models: vec![ProfileModel {
                endpoint: None,
                extras: Default::default(),
                id: "primary-x".into(),
                n_ctx: Some(100_000),
                capabilities: Default::default(),
                identifier: None,
            }],
            runtime: Some(ProfileRuntime {
                context_tokens: None,
                compaction: Some(RuntimeCompactionConfig {
                    strategy: None,
                    threshold_tokens: None,
                    threshold_ratio: None,
                    tier1: None,
                    tier2: None,
                    reserve: Some(ReserveConfig {
                        bail_after_token_count: None,
                        bail_after_compactions: Some(3),
                    }),
                    custom_instructions: None,
                    extras: Default::default(),
                }),
            }),
            use_when: None,
        };
        let args = crate::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert_eq!(args.bail_after_compactions, Some(3));
    }

    /// (#1282) A default model with NO declared `n_ctx` (endpoint-bearing —
    /// the provider owns the real window) derives `context_window: None`,
    /// which keeps the formula trigger OFF rather than inventing a window.
    #[test]
    fn from_profile_endpoint_default_model_without_n_ctx_yields_no_context_window() {
        use darkmux_types::{ModelEndpoint, Profile, ProfileModel};
        let profile = Profile {
            extras: Default::default(),
            description: None,
            default_model: None,
            models: vec![ProfileModel {
                endpoint: Some(ModelEndpoint {
                    url: Some("https://example.azure.com/openai".into()),
                    ..Default::default()
                }),
                extras: Default::default(),
                id: "gpt-remote".into(),
                n_ctx: None,
                capabilities: Default::default(),
                identifier: None,
            }],
            runtime: None,
            use_when: None,
        };
        let args = crate::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert_eq!(args.context_window, None, "no declared n_ctx ⇒ no window");
    }

    /// (#377) Per-role override wins over profile fallback. Operator
    /// pins `bail_after_compactions = 2` on the coder role; profile
    /// default is 5. Resolved value must be the role's 2, NOT the
    /// profile's 5.
    #[test]
    fn apply_role_override_overlays_role_bail_on_top_of_profile() {
        use crate::dispatch::CompactionDispatchArgs;
        use crate::types::{EscalationContract, Role, ToolPalette};
        let mut args = CompactionDispatchArgs {
            bail_after_compactions: Some(5), // profile default
            ..Default::default()
        };
        let role = Role {
            output_schema: None,
            id: "coder".into(),
            description: "test".into(),
            skills: vec![],
            tool_palette: ToolPalette::default(),
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: Some(2), // role pin
            escalation_posture: None,
            role_family: None,
            feedback_templates: None,
        };
        args.apply_role_override(&role);
        assert_eq!(args.bail_after_compactions, Some(2), "role pin wins");
    }

    /// (#377) When the role's `bail_after_compactions` is None, the
    /// profile fallback survives. Catches the regression where
    /// apply_role_override unconditionally writes the field (would
    /// clobber profile defaults to None for roles that haven't opted
    /// into per-role escalation pinning).
    #[test]
    fn apply_role_override_preserves_profile_default_when_role_unset() {
        use crate::dispatch::CompactionDispatchArgs;
        use crate::types::{EscalationContract, Role, ToolPalette};
        let mut args = CompactionDispatchArgs {
            bail_after_compactions: Some(5), // profile default
            ..Default::default()
        };
        let role = Role {
            output_schema: None,
            id: "coder".into(),
            description: "test".into(),
            skills: vec![],
            tool_palette: ToolPalette::default(),
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None, // role didn't pin
            escalation_posture: None,
            role_family: None,
            feedback_templates: None,
        };
        args.apply_role_override(&role);
        assert_eq!(
            args.bail_after_compactions,
            Some(5),
            "profile fallback survives when role doesn't pin"
        );
    }

    #[test]
    fn from_profile_bail_after_compactions_is_none_when_reserve_absent() {
        use darkmux_types::{
            Profile, ProfileModel, ProfileRuntime, RuntimeCompactionConfig,
        };
        let profile = Profile {
            extras: Default::default(),
            description: None,
            default_model: None,
            models: vec![ProfileModel {
                endpoint: None,
                extras: Default::default(),
                id: "primary-x".into(),
                n_ctx: Some(100_000),
                capabilities: Default::default(),
                identifier: None,
            }],
            runtime: Some(ProfileRuntime {
                context_tokens: None,
                compaction: Some(RuntimeCompactionConfig {
                    strategy: None,
                    threshold_tokens: None,
                    threshold_ratio: None,
                    tier1: None,
                    tier2: None,
                    reserve: None,
                    custom_instructions: None,
                    extras: Default::default(),
                }),
            }),
            use_when: None,
        };
        let args = crate::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert_eq!(args.bail_after_compactions, None);
    }

    #[test]
    fn from_profile_derives_typed_threshold() {
        use darkmux_types::{
            Profile, ProfileModel, ProfileRuntime, RuntimeCompactionConfig,
        };
        let profile = Profile {
            extras: Default::default(),
            description: None,
            default_model: None,
            models: vec![ProfileModel {
                endpoint: None,
                extras: Default::default(),
                id: "primary-x".into(),
                n_ctx: Some(100_000),
                capabilities: Default::default(),
                identifier: None,
            }],
            runtime: Some(ProfileRuntime {
                context_tokens: None,
                compaction: Some(RuntimeCompactionConfig {
                    strategy: None,
                    threshold_tokens: Some(40_000),
                    threshold_ratio: None,
                    tier1: None,
                    tier2: None,
                    reserve: None,
                    custom_instructions: None,
                    extras: Default::default(),
                }),
            }),
            use_when: None,
        };
        let args = crate::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert_eq!(args.threshold_tokens, Some(40_000));
        assert_eq!(args.context_window, Some(100_000), "primary n_ctx");
    }

    #[test]
    fn from_profile_derives_typed_threshold_ratio() {
        // (#368 clean break) `threshold_ratio` reads ONLY from the
        // typed schema field. `compactor_model` does NOT read from
        // extras at all (Beat-39 smoke caught HTTP 400 when openclaw's
        // `lmstudio/<id>` format was passed to LMStudio's direct API
        // which only knows the bare/namespaced form). Until a typed
        // `compaction.compactor_model` lands, runtime uses default.
        use darkmux_types::{
            Profile, ProfileModel, ProfileRuntime, RuntimeCompactionConfig,
        };
        let mut extras = serde_json::Map::new();
        // This openclaw-flavored value must NOT influence the dispatch.
        extras.insert(
            "model".into(),
            serde_json::json!("lmstudio/qwen3-4b-instruct-2507"),
        );
        let profile = Profile {
            extras: Default::default(),
            description: None,
            default_model: None,
            models: vec![ProfileModel {
                endpoint: None,
                extras: Default::default(),
                id: "primary-x".into(),
                n_ctx: Some(101_000),
                capabilities: Default::default(),
                identifier: None,
            }],
            runtime: Some(ProfileRuntime {
                context_tokens: None,
                compaction: Some(RuntimeCompactionConfig {
                    strategy: None,
                    threshold_tokens: None,
                    threshold_ratio: Some(0.35),
                    tier1: None,
                    tier2: None,
                    reserve: None,
                    custom_instructions: None,
                    extras,
                }),
            }),
            use_when: None,
        };
        let args = crate::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert_eq!(args.threshold_ratio, Some(0.35));
        assert!(
            args.compactor_model.is_none(),
            "clean break: openclaw extras `model` must NOT auto-populate compactor_model \
             (would pass `lmstudio/<id>` prefix to LMStudio's direct API → HTTP 400)"
        );
        assert_eq!(args.context_window, Some(101_000));
    }

    /// (#368 clean break invariant) When ONLY `extras["maxHistoryShare"]`
    /// is set — no typed `threshold_ratio` — the host MUST NOT silently
    /// translate openclaw's history-cap to darkmux's pre-trigger ratio.
    /// They're different concepts; mapping across would surface in
    /// methodology citations as "this run tuned to X" when the operator
    /// never actually expressed X in the darkmux-side surface.
    #[test]
    fn from_profile_ignores_openclaw_maxhistoryshare_extras() {
        use darkmux_types::{
            Profile, ProfileModel, ProfileRuntime, RuntimeCompactionConfig,
        };
        let mut extras = serde_json::Map::new();
        // Operator carries openclaw's historical config — this should
        // pass through untouched in extras (for any downstream
        // openclaw-aware consumer) but NOT influence darkmux's trigger.
        extras.insert("maxHistoryShare".into(), serde_json::json!(0.35));
        let profile = Profile {
            extras: Default::default(),
            description: None,
            default_model: None,
            models: vec![ProfileModel {
                endpoint: None,
                extras: Default::default(),
                id: "primary-x".into(),
                n_ctx: Some(100_000),
                capabilities: Default::default(),
                identifier: None,
            }],
            runtime: Some(ProfileRuntime {
                context_tokens: None,
                compaction: Some(RuntimeCompactionConfig {
                    strategy: None,
                    threshold_tokens: None,
                    threshold_ratio: None,
                    tier1: None,
                    tier2: None,
                    reserve: None,
                    custom_instructions: None,
                    extras,
                }),
            }),
            use_when: None,
        };
        let args = crate::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert!(
            args.threshold_ratio.is_none(),
            "clean break: openclaw extras must NOT auto-populate threshold_ratio"
        );
    }

    /// (#383) `from_profile` reads the typed `custom_instructions`
    /// field. Schema-isolation invariant: typed field is the only
    /// source the internal runtime sees.
    #[test]
    fn from_profile_reads_typed_custom_instructions() {
        use darkmux_types::{
            Profile, ProfileModel, ProfileRuntime, RuntimeCompactionConfig,
        };
        let profile = Profile {
            extras: Default::default(),
            description: None,
            default_model: None,
            models: vec![ProfileModel {
                endpoint: None,
                extras: Default::default(),
                id: "primary-x".into(),
                n_ctx: Some(100_000),
                capabilities: Default::default(),
                identifier: None,
            }],
            runtime: Some(ProfileRuntime {
                context_tokens: None,
                compaction: Some(RuntimeCompactionConfig {
                    strategy: None,
                    threshold_tokens: None,
                    threshold_ratio: None,
                    tier1: None,
                    tier2: None,
                    reserve: None,
                    custom_instructions: Some(
                        "Preserve verbatim X / list active files".into(),
                    ),
                    extras: Default::default(),
                }),
            }),
            use_when: None,
        };
        let args = crate::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert_eq!(
            args.custom_instructions.as_deref(),
            Some("Preserve verbatim X / list active files")
        );
    }

    /// (#383) `from_profile` IGNORES the openclaw-shape
    /// `extras["customInstructions"]` passthrough — schema-isolation
    /// doctrine (DESIGN.md "Schema isolation: each runtime owns its
    /// own config"). Operators on legacy profiles need to migrate to
    /// the typed `custom_instructions` field; a follow-up under [#380](https://github.com/kstrat2001/darkmux/issues/380) surfaces them via
    /// doctor warning.
    #[test]
    fn from_profile_ignores_openclaw_custom_instructions_extras() {
        use darkmux_types::{
            Profile, ProfileModel, ProfileRuntime, RuntimeCompactionConfig,
        };
        let mut extras = serde_json::Map::new();
        // Operator carries an openclaw-era `customInstructions` string
        // in their profile (the heuristic used to write this; will
        // stop in a follow-up under #380). The internal runtime MUST NOT pick it up — the
        // typed field is the only valid source.
        extras.insert(
            "customInstructions".into(),
            serde_json::json!("openclaw-shape passthrough — must be ignored by internal runtime"),
        );
        let profile = Profile {
            extras: Default::default(),
            description: None,
            default_model: None,
            models: vec![ProfileModel {
                endpoint: None,
                extras: Default::default(),
                id: "primary-x".into(),
                n_ctx: Some(100_000),
                capabilities: Default::default(),
                identifier: None,
            }],
            runtime: Some(ProfileRuntime {
                context_tokens: None,
                compaction: Some(RuntimeCompactionConfig {
                    strategy: None,
                    threshold_tokens: None,
                    threshold_ratio: None,
                    tier1: None,
                    tier2: None,
                    reserve: None,
                    custom_instructions: None,
                    extras,
                }),
            }),
            use_when: None,
        };
        let args = crate::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert!(
            args.custom_instructions.is_none(),
            "schema isolation: openclaw extras[customInstructions] must NOT auto-populate typed custom_instructions"
        );
    }

    #[test]
    fn from_profile_handles_missing_compaction_block() {
        use darkmux_types::{Profile, ProfileModel};
        let profile = Profile {
            extras: Default::default(),
            description: None,
            default_model: None,
            models: vec![ProfileModel {
                endpoint: None,
                extras: Default::default(),
                id: "primary-x".into(),
                n_ctx: Some(50_000),
                capabilities: Default::default(),
                identifier: None,
            }],
            runtime: None,
            use_when: None,
        };
        let args = crate::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert!(args.threshold_tokens.is_none());
        assert!(args.compactor_model.is_none());
        assert!(args.threshold_ratio.is_none());
        // Primary n_ctx still captured even without compaction block.
        assert_eq!(args.context_window, Some(50_000));
    }

    // ─── #363, #457: inactivity timeout (formerly wall-clock deadline) ─

    #[test]
    #[serial]
    fn inactivity_timeout_defaults_when_env_unset() {
        // Saved + restored — tests share process env, so be polite.
        let prev = std::env::var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS").ok();
        unsafe { std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS") };
        assert_eq!(inactivity_timeout_seconds(), 600); // the config_access default
        if let Some(v) = prev {
            unsafe { std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", v) };
        }
    }

    #[test]
    #[serial]
    fn inactivity_timeout_reads_env_override() {
        let prev = std::env::var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS").ok();
        unsafe { std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", "30") };
        assert_eq!(inactivity_timeout_seconds(), 30);
        unsafe { std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS") };
        if let Some(v) = prev {
            unsafe { std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", v) };
        }
    }

    #[test]
    #[serial]
    fn inactivity_timeout_falls_back_on_garbage_env() {
        let prev = std::env::var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS").ok();
        unsafe { std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", "not-a-number") };
        assert_eq!(inactivity_timeout_seconds(), 600); // the config_access default
        unsafe { std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS") };
        if let Some(v) = prev {
            unsafe { std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", v) };
        }
    }

    /// (#890) The inactivity-deadline mutex guards the hard-kill
    /// watchdog. If a panic elsewhere (e.g. the tailer, which also holds
    /// this lock) poisons it, the safety-net consumers must still read
    /// and write the deadline rather than panic — otherwise the watchdog
    /// thread dies on its next tick and the hard-kill is silently
    /// disabled. `lock_deadline` recovers the poison; the old watchdog's
    /// bare `.lock().unwrap()` would panic here.
    #[test]
    fn lock_deadline_survives_a_poisoned_mutex() {
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        let deadline = Arc::new(Mutex::new(Instant::now()));

        // Poison the mutex the way a tailer panic would: panic while
        // holding the lock.
        let poisoner = Arc::clone(&deadline);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("poison the deadline mutex");
        })
        .join();
        assert!(
            deadline.lock().is_err(),
            "precondition: the mutex must be poisoned"
        );

        // The safety-net accessor must recover, not panic — both the
        // tailer's write and the watchdog's read.
        let want = Instant::now() + Duration::from_secs(60);
        *super::lock_deadline(&deadline) = want;
        assert_eq!(*super::lock_deadline(&deadline), want);
    }

    /// (#457) Compaction-reset: when the tailer processes a `compaction`
    /// trajectory event, it must push the shared inactivity deadline
    /// forward by `inactivity_secs`. The watchdog thread reads this
    /// deadline each tick; without the reset, productive dispatches
    /// that legitimately need many minutes between compactions get
    /// killed at the absolute initial deadline.
    ///
    /// This test exercises the `TailerState::poll_and_emit` path that
    /// fires the reset; the watchdog mechanism itself (polling + kill)
    /// is integration-tested empirically since it requires a real
    /// docker container.
    #[test]
    fn tailer_compaction_event_resets_inactivity_deadline() {
        use std::io::Write;

        let tmp = TempDir::new().unwrap();
        let traj_path = tmp.path().join("trajectory.jsonl");

        // Initialize the shared deadline to a value far in the past so
        // we can observe whether the reset moved it forward.
        let inactivity_secs = 600u64;
        let original_deadline =
            Instant::now() - Duration::from_secs(3600); // 1hr in the past
        let shared = Arc::new(Mutex::new(original_deadline));

        let mut state = TailerState::new(
            traj_path.clone(),
            "test-session".into(),
            "test-role".into(),
            "test-model".into(),
            Arc::clone(&shared),
            inactivity_secs,
        );

        // Write a compaction event to the trajectory file.
        let mut f = std::fs::File::create(&traj_path).unwrap();
        writeln!(
            f,
            r#"{{"type":"compaction","seq":1,"generation":1,"before_messages":40,"after_messages":7,"summary_chars":1500}}"#
        )
        .unwrap();
        drop(f);

        let before_reset = Instant::now();
        state.poll_and_emit();
        let after_reset = Instant::now();

        let new_deadline = *shared.lock().unwrap();

        // The new deadline must be at least `inactivity_secs` ahead of
        // when poll_and_emit ran — confirms the reset fired. Allow a
        // small slop (1s) so the test isn't brittle on slow CI.
        let expected_min =
            before_reset + Duration::from_secs(inactivity_secs) - Duration::from_millis(50);
        let expected_max =
            after_reset + Duration::from_secs(inactivity_secs) + Duration::from_secs(1);
        assert!(
            new_deadline >= expected_min,
            "deadline must advance by ~inactivity_secs after compaction event; \
             saw new_deadline at less than expected_min"
        );
        assert!(
            new_deadline <= expected_max,
            "deadline must not advance by more than ~inactivity_secs; \
             saw new_deadline at more than expected_max (off by a multiplier?)"
        );
        // Also: the new deadline must be strictly later than the
        // original (1hr-in-the-past) — proves the reset overwrote the
        // stale value rather than no-oping.
        assert!(
            new_deadline > original_deadline,
            "reset must overwrite the prior stale deadline"
        );
    }

    /// (#457 → #464) Counter-test: events that don't indicate
    /// observable progress (model turn completions, reasoning,
    /// streaming markers) must NOT reset the inactivity deadline.
    /// Compaction and tool.completed DO reset (covered by their own
    /// tests). Per-mole-hole detectors guard against pathological
    /// tool patterns (cycle / cascade / drift / reasoning-loop).
    #[test]
    fn tailer_non_progress_events_do_not_reset_inactivity_deadline() {
        use std::io::Write;

        let tmp = TempDir::new().unwrap();
        let traj_path = tmp.path().join("trajectory.jsonl");

        let original_deadline = Instant::now() - Duration::from_secs(3600);
        let shared = Arc::new(Mutex::new(original_deadline));

        let mut state = TailerState::new(
            traj_path.clone(),
            "test-session".into(),
            "test-role".into(),
            "test-model".into(),
            Arc::clone(&shared),
            600,
        );

        // Write events that are NOT progress signals — turn boundary,
        // reasoning, streaming markers. None of these indicate the
        // model produced verified output.
        let mut f = std::fs::File::create(&traj_path).unwrap();
        writeln!(
            f,
            r#"{{"type":"model.completed","seq":1,"finish_reason":"stop","usage":{{"completion_tokens":100}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"model.reasoning","seq":1,"reasoning_chars":500,"reasoning_text":"thinking..."}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"model.streaming.start","seq":1,"ts":1234567890}}"#
        )
        .unwrap();
        drop(f);

        state.poll_and_emit();

        let unchanged_deadline = *shared.lock().unwrap();
        assert_eq!(
            unchanged_deadline, original_deadline,
            "non-progress events (turn / reasoning / streaming) must not \
             reset the inactivity deadline; only proof-of-work signals \
             (compaction, tool.completed) qualify"
        );
    }

    /// (#464) Tool completion is the second proof-of-work signal
    /// (alongside compaction). A successful tool call — read, bash,
    /// edit, write — means the model is actively producing or
    /// inspecting state. The deadline pushes forward so productive
    /// dispatches don't get killed by a deadline that was designed
    /// around compaction frequency.
    ///
    /// Per-mole-hole detectors (cycle, cascade, drift, reasoning-loop)
    /// guard against pathological tool patterns. The deadline trusts
    /// activity; the detectors catch struggle.
    ///
    /// (#469) Resolved: the `tool.completed` schema now carries an `ok`
    /// discriminator and ONLY a successful tool call resets the deadline
    /// — see `tailer_failed_tool_completed_does_not_reset_inactivity_deadline`
    /// for the failure case. This test covers the success path (`ok:true`).
    #[test]
    fn tailer_tool_completed_event_resets_inactivity_deadline() {
        use std::io::Write;

        let tmp = TempDir::new().unwrap();
        let traj_path = tmp.path().join("trajectory.jsonl");

        let inactivity_secs = 600u64;
        let original_deadline = Instant::now() - Duration::from_secs(3600);
        let shared = Arc::new(Mutex::new(original_deadline));

        let mut state = TailerState::new(
            traj_path.clone(),
            "test-session".into(),
            "test-role".into(),
            "test-model".into(),
            Arc::clone(&shared),
            inactivity_secs,
        );

        let mut f = std::fs::File::create(&traj_path).unwrap();
        writeln!(
            f,
            r#"{{"type":"tool.completed","seq":3,"tool_seq":0,"tool_name":"bash","args_chars":50,"result_chars":1024,"ok":true}}"#
        )
        .unwrap();
        drop(f);

        let before_reset = Instant::now();
        state.poll_and_emit();
        let after_reset = Instant::now();

        let new_deadline = *shared.lock().unwrap();
        let expected_min =
            before_reset + Duration::from_secs(inactivity_secs) - Duration::from_millis(50);
        let expected_max =
            after_reset + Duration::from_secs(inactivity_secs) + Duration::from_secs(1);
        assert!(
            new_deadline >= expected_min,
            "successful tool.completed must reset deadline by ~inactivity_secs"
        );
        assert!(
            new_deadline <= expected_max,
            "deadline must not advance more than ~inactivity_secs"
        );
        assert!(
            new_deadline > original_deadline,
            "reset must overwrite stale deadline"
        );
    }

    /// (#469) A FAILED tool call (`ok:false`) must NOT reset the
    /// inactivity deadline. This closes the fast-fail loophole: a model
    /// emitting varying failing tool calls (different args → cycle
    /// detector misses; failures interleaved with reads → failure-rate
    /// detector's consecutive count never trips) can no longer keep the
    /// deadline alive indefinitely.
    #[test]
    fn tailer_failed_tool_completed_does_not_reset_inactivity_deadline() {
        use std::io::Write;

        let tmp = TempDir::new().unwrap();
        let traj_path = tmp.path().join("trajectory.jsonl");

        let inactivity_secs = 600u64;
        let original_deadline = Instant::now() - Duration::from_secs(3600);
        let shared = Arc::new(Mutex::new(original_deadline));

        let mut state = TailerState::new(
            traj_path.clone(),
            "test-session".into(),
            "test-role".into(),
            "test-model".into(),
            Arc::clone(&shared),
            inactivity_secs,
        );

        let mut f = std::fs::File::create(&traj_path).unwrap();
        writeln!(
            f,
            r#"{{"type":"tool.completed","seq":3,"tool_seq":0,"tool_name":"bash","args_chars":50,"result_chars":80,"ok":false}}"#
        )
        .unwrap();
        drop(f);

        state.poll_and_emit();

        let unchanged_deadline = *shared.lock().unwrap();
        assert_eq!(
            unchanged_deadline, original_deadline,
            "a failed tool.completed (ok:false) must NOT reset the deadline (#469)"
        );
    }

    /// (#469) Backward-compat: a `tool.completed` event with no `ok`
    /// field (pre-#469 trajectory) is treated as success and resets the
    /// deadline, so old data behaves as it did before the field landed.
    #[test]
    fn tailer_tool_completed_without_ok_field_resets_deadline() {
        use std::io::Write;

        let tmp = TempDir::new().unwrap();
        let traj_path = tmp.path().join("trajectory.jsonl");

        let inactivity_secs = 600u64;
        let original_deadline = Instant::now() - Duration::from_secs(3600);
        let shared = Arc::new(Mutex::new(original_deadline));

        let mut state = TailerState::new(
            traj_path.clone(),
            "test-session".into(),
            "test-role".into(),
            "test-model".into(),
            Arc::clone(&shared),
            inactivity_secs,
        );

        let mut f = std::fs::File::create(&traj_path).unwrap();
        // No `ok` field — pre-#469 shape.
        writeln!(
            f,
            r#"{{"type":"tool.completed","seq":3,"tool_seq":0,"tool_name":"bash","args_chars":50,"result_chars":1024}}"#
        )
        .unwrap();
        drop(f);

        state.poll_and_emit();

        let new_deadline = *shared.lock().unwrap();
        assert!(
            new_deadline > original_deadline,
            "missing ok defaults to success → deadline resets (backward compat)"
        );
    }

    /// (#464) Multiple proof-of-work events in one poll cycle move
    /// the deadline to the LATEST reset (not stale to the first).
    /// Compaction + tool.completed in the same poll → deadline ≈
    /// now + inactivity_secs, not stale to whichever fired first.
    #[test]
    fn tailer_multiple_proof_of_work_events_advance_to_latest() {
        use std::io::Write;

        let tmp = TempDir::new().unwrap();
        let traj_path = tmp.path().join("trajectory.jsonl");

        let inactivity_secs = 600u64;
        let original_deadline = Instant::now() - Duration::from_secs(3600);
        let shared = Arc::new(Mutex::new(original_deadline));

        let mut state = TailerState::new(
            traj_path.clone(),
            "test-session".into(),
            "test-role".into(),
            "test-model".into(),
            Arc::clone(&shared),
            inactivity_secs,
        );

        let mut f = std::fs::File::create(&traj_path).unwrap();
        writeln!(
            f,
            r#"{{"type":"compaction","seq":1,"generation":1,"before_messages":40,"after_messages":7,"summary_chars":1500}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"tool.completed","seq":2,"tool_seq":0,"tool_name":"edit","args_chars":500,"result_chars":100}}"#
        )
        .unwrap();
        drop(f);

        let before_reset = Instant::now();
        state.poll_and_emit();

        let new_deadline = *shared.lock().unwrap();
        let expected_min =
            before_reset + Duration::from_secs(inactivity_secs) - Duration::from_millis(50);
        assert!(
            new_deadline >= expected_min,
            "deadline must reflect the latest reset, not stale to an earlier event"
        );
    }

    /// (#1222 shakedown-3) Streamed chunks are the THIRD proof-of-work
    /// signal: a `model.partial` event proves the model delivered tokens
    /// (transport-level liveness), so it must reset the inactivity deadline.
    /// Two dispatches were watchdog-killed 8+ minutes into legitimate
    /// reasoning under a raised per-call cap because only tool.completed /
    /// compaction reset it. A wedged server delivers no chunks → no events
    /// → true hangs still die.
    #[test]
    fn tailer_model_partial_resets_the_inactivity_deadline() {
        use std::io::Write;

        let tmp = TempDir::new().unwrap();
        let traj_path = tmp.path().join("trajectory.jsonl");

        let inactivity_secs = 600u64;
        let original_deadline = Instant::now() - Duration::from_secs(3600);
        let shared = Arc::new(Mutex::new(original_deadline));

        let mut state = TailerState::new(
            traj_path.clone(),
            "test-session".into(),
            "test-role".into(),
            "test-model".into(),
            Arc::clone(&shared),
            inactivity_secs,
        );

        let mut f = std::fs::File::create(&traj_path).unwrap();
        writeln!(
            f,
            r#"{{"type":"model.partial","seq":4,"partial_index":120,"cumulative_chars":40960}}"#
        )
        .unwrap();
        drop(f);

        let before_reset = Instant::now();
        state.poll_and_emit();

        let new_deadline = *shared.lock().unwrap();
        let expected_min =
            before_reset + Duration::from_secs(inactivity_secs) - Duration::from_millis(50);
        assert!(
            new_deadline >= expected_min,
            "a streamed chunk (model.partial) must reset the inactivity \
             deadline — an actively generating model is not a hung dispatch"
        );
    }

    // ─── #557 slice 2 detector telemetry ──────────────────────────────

    /// The pure mapping helper: a `dispatch.cycle.suspected` event →
    /// `{kind:"cycle", severity:"warn", detail:<non-empty>}`. Pure (no
    /// flow sink, no IO) so the kind/severity/detail contract is asserted
    /// deterministically. All five detector kinds route through this fn.
    #[test]
    fn detector_telemetry_payload_maps_cycle_event() {
        let event = serde_json::json!({
            "type": "dispatch.cycle.suspected",
            "seq": 7,
            "tool_name": "read",
            "count": 3,
            "window_size": 10,
        });
        let payload =
            detector_telemetry_payload("dispatch.cycle.suspected", &event).expect("maps cycle");
        assert_eq!(payload["kind"], "cycle");
        assert_eq!(payload["severity"], "warn");
        let detail = payload["detail"].as_str().expect("detail is a string");
        assert!(!detail.is_empty(), "detail must be non-empty");
        assert!(
            detail.contains("read") && detail.contains('3') && detail.contains("10"),
            "detail must weave in the event fields; got {detail:?}"
        );
    }

    /// `intra_turn_stall.recovered` with a null `completion_tokens`
    /// (upstream omitted `usage`) renders "unknown", not a misleading 0.
    #[test]
    fn detector_telemetry_payload_renders_unknown_for_null_completion_tokens() {
        let event = serde_json::json!({
            "type": "dispatch.intra_turn_stall.recovered",
            "seq": 4,
            "completion_tokens": serde_json::Value::Null,
            "recoveries_used": 1,
            "recoveries_budget": 3,
        });
        let payload = detector_telemetry_payload("dispatch.intra_turn_stall.recovered", &event)
            .expect("maps intra-turn-stall");
        assert_eq!(payload["kind"], "intra-turn-stall");
        assert_eq!(payload["severity"], "info");
        let detail = payload["detail"].as_str().unwrap();
        assert!(
            detail.contains("unknown tokens"),
            "null completion_tokens must render as 'unknown'; got {detail:?}"
        );
    }

    /// (#795) A `model.completed` event with a full `usage` object maps
    /// to the per-turn `telemetry.tokens` payload: turn_seq carried
    /// through, total derived as prompt + completion.
    #[test]
    fn turn_tokens_payload_maps_usage_to_per_turn_payload() {
        let event = serde_json::json!({
            "type": "model.completed",
            "seq": 12,
            "finish_reason": "tool_calls",
            "usage": { "prompt_tokens": 24000, "completion_tokens": 850 },
        });
        let payload = turn_tokens_payload(&event).expect("maps usage");
        assert_eq!(payload["turn_seq"], 12);
        assert_eq!(payload["prompt_tokens"], 24000);
        assert_eq!(payload["completion_tokens"], 850);
        assert_eq!(payload["total_tokens"], 24850);
    }

    /// (#795) No `usage` (or JSON-null usage — upstream omitted it) emits
    /// NOTHING. Such turns also don't accumulate into the runtime's
    /// metrics totals, so skipping preserves the records-sum-to-total
    /// invariant rather than injecting a zero-noise record.
    #[test]
    fn turn_tokens_payload_skips_absent_or_null_usage() {
        let absent = serde_json::json!({ "type": "model.completed", "seq": 3 });
        assert!(turn_tokens_payload(&absent).is_none(), "absent usage → no record");
        let null = serde_json::json!({
            "type": "model.completed", "seq": 3, "usage": serde_json::Value::Null,
        });
        assert!(turn_tokens_payload(&null).is_none(), "null usage → no record");
    }

    /// (#795) Defensive: a `usage` object missing a count degrades that
    /// count to 0 (the runtime always writes both fields; this guards
    /// hand-rolled or cross-runtime trajectories).
    #[test]
    fn turn_tokens_payload_defaults_missing_counts_to_zero() {
        let event = serde_json::json!({
            "type": "model.completed",
            "seq": 1,
            "usage": { "completion_tokens": 500 },
        });
        let payload = turn_tokens_payload(&event).expect("partial usage still maps");
        assert_eq!(payload["prompt_tokens"], 0);
        assert_eq!(payload["completion_tokens"], 500);
        assert_eq!(payload["total_tokens"], 500);
    }

    /// Integration shape: feed a `dispatch.cycle.suspected` trajectory
    /// line through `handle_event` and assert the emitted FlowRecord is a
    /// telemetry record (`category:"telemetry"`, `source:"detector"`)
    /// with a `kind:"cycle"`/`severity:"warn"`/non-empty `detail` payload.
    ///
    /// Capture mechanism: `LocalFileSink` resolves `DARKMUX_FLOWS_DIR`
    /// per write (see the #507 note on the sink), so pointing it at a
    /// tempdir and reading the day-file back observes exactly the record
    /// `handle_event` → `emit_telemetry` → `darkmux_flow::record` wrote.
    /// `#[serial]` guards the shared env var (other flow tests mutate it).
    /// (#717) Read every flow record the default sink wrote to a tempdir
    /// day-file. Shared helper for the bookend-guard tests below.
    fn drain_flow_records(dir: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension().and_then(|x| x.to_str()) == Some("jsonl")
                    && p.file_name().and_then(|n| n.to_str()) != Some("trajectory.jsonl")
            })
            .flat_map(|p| std::fs::read_to_string(&p).unwrap_or_default().into_bytes())
            .collect::<Vec<u8>>()
            .split(|&b| b == b'\n')
            .filter_map(|l| serde_json::from_slice::<serde_json::Value>(l).ok())
            .collect()
    }

    #[test]
    #[serial]
    fn bookend_guard_armed_emits_dispatch_error_on_drop() {
        // An armed guard dropped without disarming (the `?`-return / panic
        // path) emits a `dispatch.error` terminal carrying the same mission
        // so the orphaned start is bookended + stays grouped (#717/#714).
        let tmp = TempDir::new().unwrap();
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        {
            let mut sink = |r: darkmux_flow::FlowRecord| {
                let _ = darkmux_flow::record(r);
            };
            let mut guard = DispatchBookendGuard::new(
                &mut sink,
                "coder".into(),
                "sess-orphan".into(),
                "darkmux:qwen3.6".into(),
                Some("pre-1.0-compat-sweep".into()),
                Some("s694".into()),
            );
            guard.open(crate::dispatch::build_dispatch_record_with_payload(
                darkmux_flow::Level::Info,
                "dispatch start",
                "coder",
                "sess-orphan",
                Some("darkmux:qwen3.6"),
                Some("pre-1.0-compat-sweep"),
                Some("s694"),
                None,
            ));
            // drop here (end of scope) — no close/disarm
        }

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
            match prev_redis {
                Some(v) => std::env::set_var("DARKMUX_REDIS_URL", v),
                None => std::env::remove_var("DARKMUX_REDIS_URL"),
            }
        }

        let rec = drain_flow_records(tmp.path())
            .into_iter()
            .find(|v| v["action"] == "dispatch error")
            .expect("armed guard should emit a dispatch.error terminal on drop");
        assert_eq!(rec["session_id"], "sess-orphan");
        assert_eq!(rec["mission_id"], "pre-1.0-compat-sweep");
        assert_eq!(rec["phase_id"], "s694");
        assert_eq!(rec["payload"]["result_class"], "error");
    }

    #[test]
    #[serial]
    fn bookend_guard_disarmed_emits_nothing_on_drop() {
        // The happy path (and container-ran-but-failed path) disarm after
        // their own terminal record — the guard must then stay silent so the
        // dispatch isn't double-counted.
        let tmp = TempDir::new().unwrap();
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        {
            let mut sink = |r: darkmux_flow::FlowRecord| {
                let _ = darkmux_flow::record(r);
            };
            let mut guard = DispatchBookendGuard::new(
                &mut sink,
                "coder".into(),
                "sess-clean".into(),
                "darkmux:qwen3.6".into(),
                None,
                None,
            );
            guard.open(crate::dispatch::build_dispatch_record_with_payload(
                darkmux_flow::Level::Info,
                "dispatch start",
                "coder",
                "sess-clean",
                Some("darkmux:qwen3.6"),
                None,
                None,
                None,
            ));
            guard.disarm();
        }

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
            match prev_redis {
                Some(v) => std::env::set_var("DARKMUX_REDIS_URL", v),
                None => std::env::remove_var("DARKMUX_REDIS_URL"),
            }
        }

        let emitted = drain_flow_records(tmp.path())
            .into_iter()
            .any(|v| v["action"] == "dispatch error");
        assert!(!emitted, "disarmed guard must not emit any terminal record");
    }

    #[test]
    #[serial]
    fn bookend_guard_fires_on_panic_unwind() {
        // The RAII headline: a panic between start and disarm still bookends
        // the start. Rust runs Drop on unwind, so the guard emits its
        // dispatch.error even when the dispatch panics mid-flight (#717).
        let tmp = TempDir::new().unwrap();
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        // Silence the expected panic backtrace so test output stays clean.
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(|| {
            let mut sink = |r: darkmux_flow::FlowRecord| {
                let _ = darkmux_flow::record(r);
            };
            let mut guard = DispatchBookendGuard::new(
                &mut sink,
                "coder".into(),
                "sess-panic".into(),
                "darkmux:qwen3.6".into(),
                Some("pre-1.0-compat-sweep".into()),
                None,
            );
            guard.open(crate::dispatch::build_dispatch_record_with_payload(
                darkmux_flow::Level::Info,
                "dispatch start",
                "coder",
                "sess-panic",
                Some("darkmux:qwen3.6"),
                Some("pre-1.0-compat-sweep"),
                None,
                None,
            ));
            panic!("simulated mid-dispatch panic");
        });
        std::panic::set_hook(prev_hook);
        assert!(result.is_err(), "the closure should have panicked");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
            match prev_redis {
                Some(v) => std::env::set_var("DARKMUX_REDIS_URL", v),
                None => std::env::remove_var("DARKMUX_REDIS_URL"),
            }
        }

        let rec = drain_flow_records(tmp.path())
            .into_iter()
            .find(|v| v["action"] == "dispatch error")
            .expect("guard should emit a dispatch.error terminal on panic unwind");
        assert_eq!(rec["session_id"], "sess-panic");
        assert_eq!(rec["mission_id"], "pre-1.0-compat-sweep");
    }

    #[test]
    #[serial]
    fn handle_event_cycle_emits_telemetry_record() {
        let tmp = TempDir::new().unwrap();
        // SAFETY: serialized via `#[serial]`; no concurrent env reader.
        // Scrub DARKMUX_REDIS_URL so a stray operator-shell value can't
        // make the default-sink write block on an unreachable peer
        // (the 75s/record timeout the flow crate's isolate helper guards
        // against; we inline the scrub since that helper is test-support
        // gated and not visible here).
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        let traj_path = tmp.path().join("trajectory.jsonl");
        let mut state = TailerState::new_for_test(
            traj_path,
            "sess-cycle".into(),
            "coder".into(),
            "darkmux:qwen3.6".into(),
        );
        state.handle_event(
            r#"{"type":"dispatch.cycle.suspected","seq":9,"tool_name":"read","canonical_args":"{}","count":3,"window_size":10}"#,
        );

        // Restore env BEFORE assertions so a failing assert can't leak it.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
            match prev_redis {
                Some(v) => std::env::set_var("DARKMUX_REDIS_URL", v),
                None => std::env::remove_var("DARKMUX_REDIS_URL"),
            }
        }

        // Find the day-file the sink wrote (YYYY-MM-DD.jsonl) without
        // needing the crate-private day helper — glob the tempdir.
        let day_file = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| {
                p.extension().and_then(|x| x.to_str()) == Some("jsonl")
                    && p.file_name().and_then(|n| n.to_str()) != Some("trajectory.jsonl")
            })
            .expect("a flow day-file should have been written");

        let contents = std::fs::read_to_string(&day_file).unwrap();
        let telemetry = contents
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["category"] == "telemetry")
            .expect("a telemetry record should have been emitted");

        assert_eq!(telemetry["category"], "telemetry");
        assert_eq!(telemetry["source"], "detector");
        assert_eq!(telemetry["action"], "telemetry.detector");
        assert_eq!(telemetry["handle"], "coder");
        assert_eq!(telemetry["payload"]["kind"], "cycle");
        assert_eq!(telemetry["payload"]["severity"], "warn");
        assert!(
            telemetry["payload"]["detail"]
                .as_str()
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            "detail must be present and non-empty"
        );
    }

    /// (#795) A `model.completed` trajectory line with usage → BOTH a
    /// `dispatch.turn` Work record AND a per-turn `category=telemetry,
    /// source=tokens` record carrying that turn's billed usage + turn_seq.
    /// Same capture mechanism + env-scrub as the cycle test above.
    #[test]
    #[serial]
    fn handle_event_model_completed_emits_per_turn_tokens_telemetry() {
        let tmp = TempDir::new().unwrap();
        // SAFETY: serialized via `#[serial]`; no concurrent env reader.
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        let traj_path = tmp.path().join("trajectory.jsonl");
        let mut state = TailerState::new_for_test(
            traj_path,
            "sess-tokens".into(),
            "coder".into(),
            "darkmux:qwen3.6".into(),
        );
        state.handle_event(
            r#"{"type":"model.completed","seq":5,"finish_reason":"tool_calls","usage":{"prompt_tokens":31000,"completion_tokens":1200}}"#,
        );
        // A no-usage turn must emit dispatch.turn but NO tokens record.
        state.handle_event(r#"{"type":"model.completed","seq":6,"finish_reason":"stop"}"#);

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
            match prev_redis {
                Some(v) => std::env::set_var("DARKMUX_REDIS_URL", v),
                None => std::env::remove_var("DARKMUX_REDIS_URL"),
            }
        }

        let records = drain_flow_records(tmp.path());
        let tokens: Vec<&serde_json::Value> = records
            .iter()
            .filter(|v| v["category"] == "telemetry" && v["source"] == "tokens")
            .collect();
        assert_eq!(
            tokens.len(),
            1,
            "exactly one tokens record (the no-usage turn must not emit one); got {tokens:?}"
        );
        let rec = tokens[0];
        assert_eq!(rec["action"], "telemetry.tokens");
        assert_eq!(rec["handle"], "coder");
        assert_eq!(rec["session_id"], "sess-tokens");
        assert_eq!(rec["payload"]["turn_seq"], 5);
        assert_eq!(rec["payload"]["prompt_tokens"], 31000);
        assert_eq!(rec["payload"]["completion_tokens"], 1200);
        assert_eq!(rec["payload"]["total_tokens"], 32200);
        // Both turns still produced their dispatch.turn Work records.
        let turns = records.iter().filter(|v| v["action"] == "dispatch.turn").count();
        assert_eq!(turns, 2, "dispatch.turn unaffected by the telemetry emission");
    }

    /// (#557 slice 3) A `dispatch.context` trajectory line → a
    /// `category=telemetry, source=context` flow record carrying
    /// `{used, max}`. Same capture mechanism + env-scrub as the cycle
    /// test above (tempdir DARKMUX_FLOWS_DIR + DARKMUX_REDIS_URL scrub +
    /// `#[serial]`).
    #[test]
    #[serial]
    fn handle_event_context_emits_telemetry_record() {
        let tmp = TempDir::new().unwrap();
        // SAFETY: serialized via `#[serial]`; no concurrent env reader.
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        let traj_path = tmp.path().join("trajectory.jsonl");
        let mut state = TailerState::new_for_test(
            traj_path,
            "sess-context".into(),
            "coder".into(),
            "darkmux:qwen3.6".into(),
        );
        state.handle_event(
            r#"{"type":"dispatch.context","seq":3,"used":42000,"max":101000}"#,
        );

        // Restore env BEFORE assertions so a failing assert can't leak it.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
            match prev_redis {
                Some(v) => std::env::set_var("DARKMUX_REDIS_URL", v),
                None => std::env::remove_var("DARKMUX_REDIS_URL"),
            }
        }

        let day_file = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| {
                p.extension().and_then(|x| x.to_str()) == Some("jsonl")
                    && p.file_name().and_then(|n| n.to_str()) != Some("trajectory.jsonl")
            })
            .expect("a flow day-file should have been written");

        let contents = std::fs::read_to_string(&day_file).unwrap();
        // Scope to THIS test's session_id — the process-global
        // `DARKMUX_FLOWS_DIR` is shared with concurrent non-serial tailing
        // tests that also write telemetry records here.
        let telemetry = contents
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| {
                v["category"] == "telemetry"
                    && v["source"] == "context"
                    && v["session_id"] == "sess-context"
            })
            .expect("a telemetry record should have been emitted");

        assert_eq!(telemetry["category"], "telemetry");
        assert_eq!(telemetry["source"], "context");
        assert_eq!(telemetry["action"], "telemetry.context");
        assert_eq!(telemetry["payload"]["used"], 42000);
        assert_eq!(telemetry["payload"]["max"], 101000);
    }

    /// (#557 slice 3) A `compaction` trajectory line carrying
    /// `tokens_before`/`tokens_after` → BOTH the existing
    /// `dispatch.compaction` Work record (category=work) AND a new
    /// `source=compaction` telemetry record reading `{from, to}`.
    #[test]
    #[serial]
    fn handle_event_compaction_emits_work_and_telemetry_records() {
        let tmp = TempDir::new().unwrap();
        // SAFETY: serialized via `#[serial]`; no concurrent env reader.
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        let traj_path = tmp.path().join("trajectory.jsonl");
        let mut state = TailerState::new_for_test(
            traj_path,
            "sess-compaction".into(),
            "coder".into(),
            "darkmux:qwen3.6".into(),
        );
        state.handle_event(
            r#"{"type":"compaction","generation":1,"before_messages":30,"after_messages":6,"summary_chars":1500,"tokens_before":48000,"tokens_after":9000}"#,
        );

        // Restore env BEFORE assertions so a failing assert can't leak it.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
            match prev_redis {
                Some(v) => std::env::set_var("DARKMUX_REDIS_URL", v),
                None => std::env::remove_var("DARKMUX_REDIS_URL"),
            }
        }

        let day_file = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| {
                p.extension().and_then(|x| x.to_str()) == Some("jsonl")
                    && p.file_name().and_then(|n| n.to_str()) != Some("trajectory.jsonl")
            })
            .expect("a flow day-file should have been written");

        let contents = std::fs::read_to_string(&day_file).unwrap();
        let records: Vec<serde_json::Value> = contents
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .collect();

        // The existing dispatch.compaction Work record must still fire.
        let work = records
            .iter()
            .find(|v| {
                v["action"] == "dispatch.compaction"
                    && v["session_id"] == "sess-compaction"
            })
            .expect("the dispatch.compaction work record should still be emitted");
        assert_eq!(work["payload"]["generation"], 1);
        assert_eq!(work["payload"]["before_messages"], 30);
        assert_eq!(work["payload"]["after_messages"], 6);

        // The new compaction telemetry record carries {from, to}.
        // Scope to THIS test's session_id: the process-global
        // `DARKMUX_FLOWS_DIR` (set above) is also the write target for any
        // non-serial live-tailing test that runs concurrently and emits
        // `test-role`/`test-session` records — including compaction lines
        // without tokens_before/after. Match on our unique session so the
        // assertion can't latch onto a foreign record.
        let telemetry = records
            .iter()
            .find(|v| {
                v["category"] == "telemetry"
                    && v["source"] == "compaction"
                    && v["session_id"] == "sess-compaction"
            })
            .expect("a source=compaction telemetry record should have been emitted");
        assert_eq!(telemetry["action"], "telemetry.compaction");
        assert_eq!(telemetry["payload"]["from"], 48000);
        assert_eq!(telemetry["payload"]["to"], 9000);
    }

    // ─── role tool_palette → runtime allowed-tools mapping ────────────

    fn palette(allow: &[&str], deny: &[&str]) -> crate::types::ToolPalette {
        crate::types::ToolPalette {
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn allowed_tools_empty_palette_returns_none_so_runtime_uses_full_catalog() {
        let p = palette(&[], &[]);
        assert_eq!(compute_runtime_allowed_tools(&p), None);
    }

    #[test]
    fn allowed_tools_coder_palette_exposes_all_runtime_tools() {
        // coder role: allow [read, edit, write, exec, process], deny []
        let p = palette(&["read", "edit", "write", "exec", "process"], &[]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        // Expected: read + search (from "read"), edit, write, bash (from "exec").
        // "process" has no runtime equivalent; silently dropped.
        let mut sorted = result.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["bash", "edit", "read", "search", "write"]);
    }

    #[test]
    fn allowed_tools_code_reviewer_palette_excludes_edit_and_write() {
        // code-reviewer: allow [read, exec, update_plan], deny [edit, write, process]
        let p = palette(&["read", "exec", "update_plan"], &["edit", "write", "process"]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        let mut sorted = result.clone();
        sorted.sort();
        // Expected: read + search (from "read"), bash (from "exec").
        // "update_plan" has no runtime equivalent.
        assert_eq!(sorted, vec!["bash", "read", "search"]);
        // Hard regression guard: code-reviewer must NEVER see edit/write.
        assert!(!result.contains(&"edit".to_string()));
        assert!(!result.contains(&"write".to_string()));
    }

    #[test]
    fn allowed_tools_deny_overrides_allow() {
        // Pathological: same tool in both lists. Deny wins.
        let p = palette(&["edit"], &["edit"]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        assert!(result.is_empty(), "deny must win over allow; got {result:?}");
    }

    #[test]
    fn allowed_tools_unknown_role_vocab_silently_dropped() {
        let p = palette(&["fake-tool", "not-a-thing"], &[]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        assert!(result.is_empty(), "unknown role-vocab → empty; got {result:?}");
    }

    #[test]
    fn allowed_tools_role_read_expands_to_runtime_read_and_search() {
        // Conceptual contract: role "read" means "the model may read";
        // runtime "search" is a specialized read (find pattern in tree)
        // that's implied by the broader "read" allowance.
        let p = palette(&["read"], &[]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        let mut sorted = result.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["read", "search"]);
    }

    // ─── #340: unknown role-vocab token detection ───────────────────

    #[test]
    fn unknown_role_vocab_empty_palette_returns_empty() {
        let p = palette(&[], &[]);
        assert!(unknown_role_vocab_tokens(&p).is_empty());
    }

    #[test]
    fn unknown_role_vocab_all_known_tokens_returns_empty() {
        let p = palette(&["read", "edit", "write"], &["exec", "process", "update_plan"]);
        assert!(unknown_role_vocab_tokens(&p).is_empty());
    }

    /// Typo in allow list (the canonical failure shape from #340 spec —
    /// "exce" instead of "exec" silently drops the agent's only exec
    /// capability).
    #[test]
    fn unknown_role_vocab_typo_in_allow_is_surfaced() {
        let p = palette(&["read", "exce"], &[]);
        assert_eq!(unknown_role_vocab_tokens(&p), vec!["exce".to_string()]);
    }

    /// Typos in both lists are deduplicated + sorted.
    #[test]
    fn unknown_role_vocab_dedupes_and_sorts_across_allow_and_deny() {
        let p = palette(&["fake-tool", "exce"], &["fake-tool", "another-typo"]);
        let unknowns = unknown_role_vocab_tokens(&p);
        assert_eq!(
            unknowns,
            vec![
                "another-typo".to_string(),
                "exce".to_string(),
                "fake-tool".to_string(),
            ]
        );
    }

    /// Future tokens (vendor-specific, not yet wired) are flagged as
    /// unknown — operator-facing signal that the manifest references
    /// a token darkmux doesn't know how to map yet. NOT a hard error;
    /// the dispatch can still proceed (the unknown just drops from
    /// the runtime catalog) but the operator gets warned.
    #[test]
    fn unknown_role_vocab_future_token_is_flagged() {
        let p = palette(&["read", "vendor-specific-tool"], &[]);
        assert_eq!(
            unknown_role_vocab_tokens(&p),
            vec!["vendor-specific-tool".to_string()]
        );
    }

    /// Known tokens (including those with no runtime equivalent like
    /// `process` and `update_plan`) MUST NOT be flagged. They're
    /// intentionally dropped, not accidentally.
    #[test]
    fn unknown_role_vocab_does_not_flag_known_no_runtime_tokens() {
        let p = palette(&["process", "update_plan"], &[]);
        assert!(
            unknown_role_vocab_tokens(&p).is_empty(),
            "process and update_plan are known role-vocab; should NOT be flagged"
        );
    }

    #[test]
    fn known_role_vocab_csv_contains_all_known_tokens() {
        let csv = known_role_vocab_csv();
        for token in ["read", "edit", "write", "exec", "process", "update_plan"] {
            assert!(
                csv.contains(token),
                "known_role_vocab_csv missing `{token}`; got: {csv}"
            );
        }
    }

    #[test]
    fn allowed_tools_role_exec_maps_to_runtime_bash() {
        let p = palette(&["exec"], &[]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        assert_eq!(result, vec!["bash".to_string()]);
    }

    /// QA NIT 1 — deny strips ALL of a role-vocab token's runtime
    /// expansions, not just the literal name. Pins the contract: if a
    /// future refactor switched deny to "literal-string only," role
    /// "read" denied would still leak `search` (which expands from
    /// "read"). Regression guard for the expansion-stripping invariant.
    #[test]
    fn allowed_tools_deny_role_read_strips_both_read_and_search() {
        let p = palette(&["read"], &["read"]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        assert!(
            result.is_empty(),
            "denying role-vocab `read` must strip BOTH runtime `read` and `search`; got {result:?}"
        );
    }

    /// Sibling: partial overlap. `allow:["read","exec"], deny:["read"]`
    /// must result in `["bash"]` only — both `read` and `search` removed
    /// by the deny.
    #[test]
    fn allowed_tools_deny_role_read_alongside_allowed_exec_leaves_only_bash() {
        let p = palette(&["read", "exec"], &["read"]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        assert_eq!(result, vec!["bash".to_string()]);
    }

    // ─── cap_reasoning_text (S6) ──────────────────────────────────────

    #[test]
    fn cap_reasoning_text_passes_through_short_string() {
        let v = serde_json::Value::String("short".into());
        let out = cap_reasoning_text(Some(&v));
        assert_eq!(out, v);
    }

    #[test]
    fn cap_reasoning_text_passes_through_null() {
        assert_eq!(cap_reasoning_text(None), serde_json::Value::Null);
    }

    #[test]
    fn cap_reasoning_text_passes_through_non_string() {
        let v = serde_json::Value::Number(42.into());
        let out = cap_reasoning_text(Some(&v));
        assert_eq!(out, v);
    }

    #[test]
    fn cap_reasoning_text_truncates_oversize_and_marks() {
        let oversize = "x".repeat(MAX_REASONING_TEXT_BYTES + 100);
        let v = serde_json::Value::String(oversize.clone());
        let out = cap_reasoning_text(Some(&v));
        let s = out.as_str().expect("output is string");
        assert!(s.len() < oversize.len(), "must be shorter than input");
        assert!(s.contains("[truncated"), "must carry truncation marker");
        assert!(s.contains(&oversize.len().to_string()), "marker must include original byte count");
    }

    #[test]
    fn cap_reasoning_text_truncates_at_utf8_boundary() {
        // Build a string where the byte just past the cap is mid-codepoint
        // (4-byte emoji starting at a position near the cap). Result must
        // still be valid UTF-8.
        let pad_bytes = MAX_REASONING_TEXT_BYTES - 1;
        let mut s = "a".repeat(pad_bytes);
        s.push('🦀'); // 4 bytes, starts at pad_bytes
        s.push_str(&"b".repeat(50));
        let v = serde_json::Value::String(s);
        let out = cap_reasoning_text(Some(&v));
        let truncated = out.as_str().expect("output is string");
        // The marker is appended; the actual truncated content is valid UTF-8
        // because String::from_utf8_lossy isn't used — we sliced on a boundary.
        assert!(truncated.is_char_boundary(0));
        assert!(truncated.contains("[truncated"));
    }

    // ─── #237: bounding container-written trajectory fields at ingest ──
    #[test]
    fn cap_json_str_bounds_short_fields() {
        // A container could write a pathologically large tool_name / finish_reason.
        let huge = "z".repeat(MAX_TRAJ_FIELD_BYTES + 5000);
        let v = serde_json::Value::String(huge.clone());
        let out = cap_json_str(Some(&v), MAX_TRAJ_FIELD_BYTES);
        let s = out.as_str().expect("string out");
        assert!(s.len() <= MAX_TRAJ_FIELD_BYTES + 100, "bounded near the cap (+marker)");
        assert!(s.contains("[truncated"), "carries the marker");
        assert!(s.contains(&huge.len().to_string()), "marker names the original size");
        // Short values are untouched.
        let small = serde_json::json!("read");
        assert_eq!(cap_json_str(Some(&small), MAX_TRAJ_FIELD_BYTES), small);
        // Non-string + None pass through / null.
        let n = serde_json::json!(42);
        assert_eq!(cap_json_str(Some(&n), MAX_TRAJ_FIELD_BYTES), n);
        assert_eq!(cap_json_str(None, MAX_TRAJ_FIELD_BYTES), serde_json::Value::Null);
    }

    #[test]
    fn detector_detail_is_bounded_against_oversize_tool_name() {
        // A container-injected cycle event with a giant tool_name must not
        // produce an unbounded detector `detail` in the telemetry record.
        let huge = "t".repeat(100_000);
        let event = serde_json::json!({
            "type": "dispatch.cycle.suspected",
            "tool_name": huge,
            "count": 3,
            "window_size": 10,
        });
        let payload = detector_telemetry_payload("dispatch.cycle.suspected", &event)
            .expect("cycle event yields a payload");
        let detail = payload["detail"].as_str().expect("detail string");
        assert!(detail.len() <= MAX_TRAJ_FIELD_BYTES + 100, "detail bounded near the cap");
        assert!(detail.contains("[truncated"), "carries the marker");
        assert_eq!(payload["kind"], "cycle");
        assert_eq!(payload["severity"], "warn");
    }

    // ─── #994 engagement-context capture (slice 1): area.files ────────

    /// A cycle on a file-bearing tool (edit/write/read/search) stamps
    /// `area.files` with the path the runtime canonicalized into the event,
    /// keying the caution to the file it happened in.
    #[test]
    fn detector_telemetry_payload_stamps_area_files_for_file_cycle() {
        let event = serde_json::json!({
            "type": "dispatch.cycle.suspected",
            "seq": 9,
            "tool_name": "edit",
            "canonical_args": r#"{"path":"src/lib.rs"}"#,
            "count": 3,
            "window_size": 10,
        });
        let payload =
            detector_telemetry_payload("dispatch.cycle.suspected", &event).expect("maps cycle");
        assert_eq!(
            payload["area"]["files"],
            serde_json::json!(["src/lib.rs"]),
            "a file-bearing cycle keys the caution to the edited path"
        );
    }

    /// A `bash` cycle carries `{command: …}` — no file — so no `area` is
    /// stamped (a fileless area would be noise; the firing stays
    /// engagement-level, not pinned to a file that doesn't exist).
    #[test]
    fn detector_telemetry_payload_omits_area_for_fileless_cycle() {
        let event = serde_json::json!({
            "type": "dispatch.cycle.suspected",
            "tool_name": "bash",
            "canonical_args": r#"{"command":"ls -la"}"#,
            "count": 3,
            "window_size": 10,
        });
        let payload =
            detector_telemetry_payload("dispatch.cycle.suspected", &event).expect("maps cycle");
        assert!(payload.get("area").is_none(), "bash cycle has no file area");
    }

    /// A `search` cycle DOES carry a `path` in its canonical args — but it's
    /// the search *root directory*, not a target file. The tool allowlist must
    /// exclude it so a directory is never stamped into `area.files` (the
    /// category error CONSIDER-1 in the #994-capture QA caught).
    #[test]
    fn detector_telemetry_payload_omits_area_for_search_cycle_directory_path() {
        let event = serde_json::json!({
            "type": "dispatch.cycle.suspected",
            "tool_name": "search",
            "canonical_args": r#"{"pattern":"TODO","path":"src/"}"#,
            "count": 3,
            "window_size": 10,
        });
        let payload =
            detector_telemetry_payload("dispatch.cycle.suspected", &event).expect("maps cycle");
        assert!(
            payload.get("area").is_none(),
            "search's path is a directory, not a file — must not be stamped as area.files"
        );
    }

    /// Malformed `canonical_args` degrade to no `area` rather than dropping the
    /// firing or panicking — the detail/kind/severity still render.
    #[test]
    fn detector_telemetry_payload_omits_area_on_malformed_canonical_args() {
        let event = serde_json::json!({
            "type": "dispatch.cycle.suspected",
            "tool_name": "edit",
            "canonical_args": "not json{{",
            "count": 3,
            "window_size": 10,
        });
        let payload = detector_telemetry_payload("dispatch.cycle.suspected", &event)
            .expect("maps cycle even with bad args");
        assert!(payload.get("area").is_none());
        assert_eq!(payload["kind"], "cycle");
    }

    /// A cycle event with no `canonical_args` at all (the pre-#994 event shape
    /// the other detector tests use) stamps no `area` — guards those existing
    /// assertions against this slice.
    #[test]
    fn detector_telemetry_payload_omits_area_when_canonical_args_absent() {
        let event = serde_json::json!({
            "type": "dispatch.cycle.suspected",
            "tool_name": "read",
            "count": 3,
            "window_size": 10,
        });
        let payload =
            detector_telemetry_payload("dispatch.cycle.suspected", &event).expect("maps cycle");
        assert!(payload.get("area").is_none());
    }

    /// Turn-level detectors fire with no single target file, so they never
    /// stamp `area` (engagement-level cautions in #994 terms).
    #[test]
    fn detector_telemetry_payload_omits_area_for_turn_level_detector() {
        let event = serde_json::json!({
            "type": "dispatch.reasoning_loop.suspected",
            "count": 4,
            "window_size": 6,
        });
        let payload = detector_telemetry_payload("dispatch.reasoning_loop.suspected", &event)
            .expect("maps reasoning-loop");
        assert!(payload.get("area").is_none());
    }

    /// A pathologically long container-written path is bounded the same way the
    /// detector `detail` is (#237) — it can't bloat the telemetry record.
    #[test]
    fn detector_area_path_is_bounded() {
        let huge = "p".repeat(100_000);
        let event = serde_json::json!({
            "type": "dispatch.cycle.suspected",
            "tool_name": "edit",
            "canonical_args": serde_json::json!({ "path": huge }).to_string(),
            "count": 3,
            "window_size": 10,
        });
        let payload =
            detector_telemetry_payload("dispatch.cycle.suspected", &event).expect("maps cycle");
        let file = payload["area"]["files"][0].as_str().expect("file string");
        assert!(file.len() <= MAX_TRAJ_FIELD_BYTES + 100, "path bounded near the cap");
        assert!(file.contains("[truncated"), "carries the marker");
    }

    /// (#1001) The firing-time `code_hash` the runtime captures is forwarded
    /// into `area.code_hash` (for staleness ranking), alongside the file.
    #[test]
    fn detector_area_forwards_code_hash() {
        let event = serde_json::json!({
            "type": "dispatch.cycle.suspected",
            "tool_name": "edit",
            "canonical_args": r#"{"path":"src/a.rs"}"#,
            "code_hash": "deadbeef",
            "count": 3,
            "window_size": 10,
        });
        let area = detector_area("dispatch.cycle.suspected", &event).expect("has area");
        assert_eq!(area["files"][0], "src/a.rs");
        assert_eq!(area["code_hash"], "deadbeef");
    }

    /// (#1001) A cycle on a file tool with no captured hash (non-code target)
    /// still yields the file area, just without `code_hash`.
    #[test]
    fn detector_area_omits_code_hash_when_absent() {
        let event = serde_json::json!({
            "type": "dispatch.cycle.suspected",
            "tool_name": "read",
            "canonical_args": r#"{"path":"src/a.rs"}"#,
            "count": 3,
            "window_size": 10,
        });
        let area = detector_area("dispatch.cycle.suspected", &event).expect("has area");
        assert_eq!(area["files"][0], "src/a.rs");
        assert!(area.get("code_hash").is_none(), "no code_hash key when uncaptured");
    }

    /// (#1001) The tool-failure-cascade detector now carries `canonical_args`,
    /// so a failure on a file-editing tool keys to its file + code_hash like a
    /// cycle does; a failure on a non-file tool (`bash`) is engagement-level.
    #[test]
    fn detector_area_maps_tool_repeated_failure() {
        let edit_fail = serde_json::json!({
            "type": "dispatch.tool.repeated_failure",
            "tool_name": "edit",
            "canonical_args": r#"{"path":"src/b.rs"}"#,
            "code_hash": "cafe",
            "failure_count": 3,
        });
        let area = detector_area("dispatch.tool.repeated_failure", &edit_fail).expect("file area");
        assert_eq!(area["files"][0], "src/b.rs");
        assert_eq!(area["code_hash"], "cafe");

        let bash_fail = serde_json::json!({
            "type": "dispatch.tool.repeated_failure",
            "tool_name": "bash",
            "canonical_args": r#"{"command":"ls"}"#,
            "failure_count": 3,
        });
        assert!(
            detector_area("dispatch.tool.repeated_failure", &bash_fail).is_none(),
            "a bash failure is engagement-level, not file-scoped"
        );
    }

    /// `extract_tool_target_path` mirrors the runtime parser: pulls a string
    /// `path`; `None` on missing / non-string / malformed.
    #[test]
    fn extract_tool_target_path_pulls_path_and_degrades() {
        assert_eq!(
            extract_tool_target_path(r#"{"path":"a/b.rs","offset":1}"#).as_deref(),
            Some("a/b.rs")
        );
        assert!(extract_tool_target_path(r#"{"command":"ls"}"#).is_none());
        assert!(extract_tool_target_path(r#"{"path":42}"#).is_none());
        assert!(extract_tool_target_path("not json").is_none());
    }

    // ─── TailerState::poll_and_emit (live tailing) ────────────────────

    fn fixture_state(trajectory_path: PathBuf) -> TailerState {
        TailerState::new_for_test(
            trajectory_path,
            "test-session".into(),
            "test-role".into(),
            "test-model".into(),
        )
    }

    #[test]
    fn tailer_state_with_mission_stamps_fields() {
        // (#714) The production tailer chains `.with_mission(...)` so every
        // per-event flow record it emits carries the dispatch's mission/phase
        // and groups under the mission in the observability view. Default
        // (test/one-off) is None.
        let tmp = TempDir::new().unwrap();
        let bare = fixture_state(tmp.path().join("t.jsonl"));
        assert!(bare.mission_id.is_none() && bare.phase_id.is_none());

        let stamped = fixture_state(tmp.path().join("t.jsonl")).with_mission(
            Some("pre-1.0-compat-sweep".into()),
            Some("s694-profiles-schema".into()),
        );
        assert_eq!(stamped.mission_id.as_deref(), Some("pre-1.0-compat-sweep"));
        assert_eq!(stamped.phase_id.as_deref(), Some("s694-profiles-schema"));
    }

    #[test]
    fn tailer_state_handles_missing_file() {
        // poll_and_emit must be a no-op when the trajectory file doesn't
        // exist yet (container hasn't written anything).
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("never-written.jsonl");
        let mut state = fixture_state(path);
        state.poll_and_emit(); // no panic; no events
        assert_eq!(state.offset, 0);
        assert!(state.pending.is_empty());
    }

    #[test]
    fn tailer_state_carries_partial_line_across_polls() {
        // Write the first half of a line, poll, write the second half,
        // poll again — the state's pending buffer must stitch them together
        // and only dispatch the event once the newline arrives.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trajectory.jsonl");
        let mut state = fixture_state(path.clone());

        // First write: incomplete (no newline)
        {
            let mut f = std::fs::File::create(&path).unwrap();
            write!(f, "{{\"type\":\"model.compl").unwrap();
        }
        state.poll_and_emit();
        assert_eq!(state.summary.turns, 0, "no complete line yet");
        assert!(!state.pending.is_empty(), "partial line carried");

        // Second write: appends the rest of the line with newline
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(f, "eted\",\"seq\":1,\"finish_reason\":\"stop\"}}").unwrap();
        }
        state.poll_and_emit();
        assert_eq!(state.summary.turns, 1, "complete line dispatched after second poll");
        assert!(state.pending.is_empty(), "pending drained after newline");
    }

    /// Regression guard for #329 — multi-byte UTF-8 characters split
    /// across reads must not corrupt to U+FFFD.
    ///
    /// The `drain_complete_lines_from_bytes` helper is the pure-
    /// function extract that makes the bug directly testable. Before
    /// the fix: pending was String; each poll did from_utf8_lossy on
    /// partial bytes; emoji split across the boundary became U+FFFD.
    /// After the fix: pending is Vec<u8>; decode happens once per
    /// complete line.
    #[test]
    fn drain_complete_lines_preserves_multibyte_across_extends() {
        let mut pending: Vec<u8> = Vec::new();

        // First chunk: prefix + first 2 bytes of 🦀.
        pending.extend_from_slice(b"{\"reasoning_text\":\"");
        pending.extend_from_slice(b"\xF0\x9F");
        let lines = drain_complete_lines_from_bytes(&mut pending);
        assert!(lines.is_empty(), "no newline yet — nothing drained");

        // Second chunk: last 2 bytes of 🦀, close out, newline.
        pending.extend_from_slice(b"\xA6\x80 reactor\"}\n");
        let lines = drain_complete_lines_from_bytes(&mut pending);
        assert_eq!(lines.len(), 1, "complete line drained");
        assert!(
            lines[0].contains("🦀 reactor"),
            "multi-byte char must round-trip intact; got: {}",
            lines[0]
        );
        assert!(
            !lines[0].contains('\u{FFFD}'),
            "no replacement chars should appear; got: {}",
            lines[0]
        );
        assert!(pending.is_empty(), "pending drained after newline");
    }

    /// Two complete lines in one buffer, plus a partial third line.
    /// The helper must drain both complete lines and leave the
    /// partial third in pending.
    #[test]
    fn drain_complete_lines_handles_multiple_lines_per_call() {
        let mut pending: Vec<u8> = b"line one\nline two\npartial".to_vec();
        let lines = drain_complete_lines_from_bytes(&mut pending);
        assert_eq!(lines, vec!["line one".to_string(), "line two".to_string()]);
        assert_eq!(pending, b"partial");
    }

    /// Empty lines (consecutive newlines) are skipped — matches the
    /// pre-fix behavior of the line-emit loop.
    #[test]
    fn drain_complete_lines_skips_empty_lines() {
        let mut pending: Vec<u8> = b"alpha\n\nbeta\n".to_vec();
        let lines = drain_complete_lines_from_bytes(&mut pending);
        assert_eq!(lines, vec!["alpha".to_string(), "beta".to_string()]);
        assert!(pending.is_empty());
    }

    /// End-to-end through TailerState: write a line with an emoji
    /// split across two polls; the tailer's handle_event sees the
    /// intact line. Verified by writing a model.completed event
    /// (which the summary DOES track) interleaved with the emoji
    /// line — the turn count proves the second line was parsed.
    #[test]
    fn tailer_state_dispatches_event_after_multibyte_split() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trajectory.jsonl");
        let mut state = fixture_state(path.clone());

        // First write: model.completed line intact + start of a
        // second line containing 🦀 (4-byte UTF-8 seq), broken
        // mid-codepoint.
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(b"{\"type\":\"model.completed\",\"seq\":1,\"finish_reason\":\"stop\"}\n")
                .unwrap();
            f.write_all(b"{\"type\":\"model.reasoning\",\"reasoning_text\":\"")
                .unwrap();
            f.write_all(b"\xF0\x9F").unwrap();
        }
        state.poll_and_emit();
        assert_eq!(state.summary.turns, 1, "first line dispatched");

        // Second write: completes the 🦀 + closes the JSON.
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"\xA6\x80 reactor\"}\n").unwrap();
        }
        state.poll_and_emit();
        // pending should be empty — both lines now drained.
        assert!(
            state.pending.is_empty(),
            "all lines drained after second poll; got pending={:?}",
            state.pending
        );
    }

    #[test]
    fn tailer_state_resets_on_truncation() {
        // Defensive path: if the file shrinks below our offset, the
        // tailer must reset its offset to 0 rather than trying to seek
        // past EOF.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trajectory.jsonl");
        let mut state = fixture_state(path.clone());

        std::fs::write(&path, b"some-bytes\n").unwrap();
        state.poll_and_emit();
        let offset_before = state.offset;
        assert!(offset_before > 0);

        // Truncate to a smaller size.
        std::fs::write(&path, b"").unwrap();
        state.poll_and_emit();
        // After truncation poll, offset should reset to 0 (file is empty,
        // so 0 ≤ size = 0 and offset is 0).
        assert_eq!(state.offset, 0);
    }

    #[test]
    fn tailer_skips_malformed_lines() {
        // A non-JSON line in the trajectory must not crash the tailer or
        // stop later events from being processed.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trajectory.jsonl");
        let mut state = fixture_state(path.clone());

        let lines = "not json\n\
            {\"type\":\"tool.completed\",\"tool_seq\":1,\"tool_name\":\"bash\"}\n";
        std::fs::write(&path, lines).unwrap();
        state.poll_and_emit();
        assert_eq!(state.summary.tool_calls, 1, "later valid event still processed");
    }

    // ─── Heartbeat rate limiting ──────────────────────────────────────

    #[test]
    fn heartbeat_first_partial_emits() {
        // The very first model.partial should produce a heartbeat (no
        // prior last_heartbeat_at).
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trajectory.jsonl");
        let mut state = fixture_state(path.clone());

        let line = r#"{"type":"model.partial","seq":1,"partial_index":0,"cumulative_chars":10}"#;
        std::fs::write(&path, format!("{line}\n")).unwrap();
        state.poll_and_emit();
        assert_eq!(state.summary.heartbeats, 1);
        assert!(state.last_heartbeat_at.is_some());
    }

    #[test]
    fn heartbeat_rate_limits_consecutive_partials() {
        // Two model.partial events back-to-back (under the 2s window)
        // should produce exactly one heartbeat.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trajectory.jsonl");
        let mut state = fixture_state(path.clone());

        let lines = "\
            {\"type\":\"model.partial\",\"seq\":1,\"partial_index\":0,\"cumulative_chars\":10}\n\
            {\"type\":\"model.partial\",\"seq\":1,\"partial_index\":1,\"cumulative_chars\":20}\n";
        std::fs::write(&path, lines).unwrap();
        state.poll_and_emit();
        assert_eq!(state.summary.heartbeats, 1, "second partial within window must be coalesced");
    }


    // first_user_symlink_in / is_macos_firmlink tests moved to
    // `darkmux_types::workdir::tests` as part of Wave-E.2 (#255).

    // ─── #590 utility_preflight_warning ───────────────────────────────
    fn lm(identifier: &str, model: &str) -> darkmux_types::LoadedModel {
        darkmux_types::LoadedModel {
            identifier: identifier.into(),
            model: model.into(),
            status: "loaded".into(),
            size: "3 GB".into(),
            context: 4096,
        }
    }

    #[test]
    fn utility_preflight_no_warning_when_loaded() {
        let loaded = vec![lm("darkmux:util-4b", "util-4b"), lm("worker", "worker-35b")];
        // Matched by modelKey...
        assert!(super::utility_preflight_warning("util-4b", &loaded).is_none());
        // ...or by the namespaced identifier.
        assert!(super::utility_preflight_warning("darkmux:util-4b", &loaded).is_none());
    }

    #[test]
    fn utility_preflight_warns_loudly_when_not_loaded() {
        let loaded = vec![lm("worker", "worker-35b")];
        let w = super::utility_preflight_warning("util-4b", &loaded)
            .expect("a registered-but-unloaded util model must warn");
        assert!(w.contains("util-4b"), "names the model: {w}");
        assert!(w.contains("NOT loaded"), "states the problem: {w}");
    }
