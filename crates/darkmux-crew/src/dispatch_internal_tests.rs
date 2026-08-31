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
                "https://example-aoai.cognitiveservices.azure.com/openai/deployments/gpt-4o".into(),
            ),
            ..Default::default()
        };
        assert_eq!(
            remote_endpoint_label(&az, "gpt-4o"),
            "azure:example-aoai.cognitiveservices.azure.com/gpt-4o"
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

    /// (#1260, FIX 2) A bare remote `dispatch` is metered as ONE
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

    // ─── (#2094) read_rest_totals — mirrors read_token_totals exactly ────

    #[test]
    fn read_rest_totals_parses_metrics_json() {
        let out = TempDir::new().unwrap();
        let rt = out.path().join(".darkmux-runtime");
        fs::create_dir_all(&rt).unwrap();
        fs::write(rt.join("metrics.json"), r#"{"rest_ms": 1000, "rests": 2}"#).unwrap();
        let r = read_rest_totals(out.path());
        assert_eq!(r.rest_ms, 1000);
        assert_eq!(r.rests, 2);
    }

    #[test]
    fn read_rest_totals_degrades_to_zero_on_missing_or_malformed() {
        // (#2094 finding 5) Both cases now also fall back to
        // trajectory.jsonl — but neither temp dir has one, so the
        // fallback itself degrades to zero too, same end result as before
        // the fallback existed.
        let missing = TempDir::new().unwrap();
        let r = read_rest_totals(missing.path());
        assert_eq!((r.rest_ms, r.rests), (0, 0), "missing metrics.json + no trajectory → zero");

        let bad = TempDir::new().unwrap();
        let rt = bad.path().join(".darkmux-runtime");
        fs::create_dir_all(&rt).unwrap();
        fs::write(rt.join("metrics.json"), "{not valid json").unwrap();
        let r = read_rest_totals(bad.path());
        assert_eq!((r.rest_ms, r.rests), (0, 0), "malformed metrics.json + no trajectory → zero");
    }

    #[test]
    fn read_rest_totals_absent_fields_falls_back_to_trajectory_or_zero() {
        // A metrics.json from BEFORE #2094 (or a build without the feature)
        // has no rest_ms/rests keys at all. With no trajectory.jsonl either,
        // this must still degrade to zero, not error.
        let out = TempDir::new().unwrap();
        let rt = out.path().join(".darkmux-runtime");
        fs::create_dir_all(&rt).unwrap();
        fs::write(rt.join("metrics.json"), r#"{"total_prompt_tokens": 100}"#).unwrap();
        let r = read_rest_totals(out.path());
        assert_eq!((r.rest_ms, r.rests), (0, 0));
    }

    // ─── #2094 finding 5: rest_ms on the error path ──────────────────────

    #[test]
    fn read_rest_totals_falls_back_to_trajectory_when_metrics_json_is_absent() {
        // No metrics.json at all — e.g. the runtime was SIGKILLed before
        // its exit-time write ran. The runtime.rest events are durably
        // streamed to trajectory.jsonl as they happen, so summing them
        // recovers the totals metrics.json never got the chance to write.
        let out = TempDir::new().unwrap();
        let rt = out.path().join(".darkmux-runtime");
        fs::create_dir_all(&rt).unwrap();
        fs::write(
            rt.join("trajectory.jsonl"),
            "{\"type\":\"runtime.rest\",\"seq\":1,\"ts\":1,\"ms\":500}\n\
             {\"type\":\"model.completed\",\"seq\":1}\n\
             {\"type\":\"runtime.rest\",\"seq\":2,\"ts\":2,\"ms\":300}\n",
        )
        .unwrap();
        let r = read_rest_totals(out.path());
        assert_eq!(r.rest_ms, 800, "sum of both runtime.rest ms fields");
        assert_eq!(r.rests, 2, "count of runtime.rest events, ignoring other event types");
    }

    #[test]
    fn read_rest_totals_falls_back_to_trajectory_when_metrics_lacks_rest_ms() {
        // metrics.json IS present and valid JSON, but (pre-#2094 shape, or
        // a runtime build without the feature) carries no rest_ms key —
        // must still reach for the trajectory fallback rather than
        // treating a present-but-incomplete file as authoritative zero.
        let out = TempDir::new().unwrap();
        let rt = out.path().join(".darkmux-runtime");
        fs::create_dir_all(&rt).unwrap();
        fs::write(rt.join("metrics.json"), r#"{"total_prompt_tokens": 100}"#).unwrap();
        fs::write(
            rt.join("trajectory.jsonl"),
            "{\"type\":\"runtime.rest\",\"seq\":1,\"ts\":1,\"ms\":250}\n",
        )
        .unwrap();
        let r = read_rest_totals(out.path());
        assert_eq!(r.rest_ms, 250);
        assert_eq!(r.rests, 1);
    }

    #[test]
    fn read_rest_totals_prefers_metrics_json_over_trajectory_when_both_present() {
        // metrics.json carrying rest_ms is authoritative — the trajectory
        // fallback only kicks in when metrics.json can't answer at all.
        let out = TempDir::new().unwrap();
        let rt = out.path().join(".darkmux-runtime");
        fs::create_dir_all(&rt).unwrap();
        fs::write(rt.join("metrics.json"), r#"{"rest_ms": 1000, "rests": 2}"#).unwrap();
        fs::write(
            rt.join("trajectory.jsonl"),
            "{\"type\":\"runtime.rest\",\"seq\":1,\"ts\":1,\"ms\":9999}\n",
        )
        .unwrap();
        let r = read_rest_totals(out.path());
        assert_eq!(r.rest_ms, 1000, "metrics.json wins, not the trajectory sum");
        assert_eq!(r.rests, 2);
    }

    #[test]
    fn read_rest_totals_skips_malformed_trajectory_lines() {
        let out = TempDir::new().unwrap();
        let rt = out.path().join(".darkmux-runtime");
        fs::create_dir_all(&rt).unwrap();
        fs::write(
            rt.join("trajectory.jsonl"),
            "not even json\n\
             {\"type\":\"runtime.rest\",\"seq\":1,\"ts\":1,\"ms\":500}\n",
        )
        .unwrap();
        let r = read_rest_totals(out.path());
        assert_eq!(r.rest_ms, 500, "the malformed line is skipped, not fatal to the sum");
        assert_eq!(r.rests, 1);
    }

    // ─── #2094 second round, finding 1: rest_ms max(metrics, tailer) ─────

    #[test]
    fn reconcile_rest_totals_prefers_the_larger_of_metrics_and_tailer() {
        let from_metrics = RestTotals { rest_ms: 0, rests: 0 };
        let from_tailer = RestTotals { rest_ms: 1000, rests: 2 };
        let merged = reconcile_rest_totals(from_metrics, from_tailer);
        assert_eq!(merged.rest_ms, 1000);
        assert_eq!(merged.rests, 2);
    }

    #[test]
    fn reconcile_rest_totals_keeps_metrics_when_it_reports_more_than_the_tailer() {
        // The live tailer's view can be the SMALLER one too (its last poll
        // landed a beat before the runtime's clean-exit flush) — metrics.json
        // is the fuller picture in that direction, so this isn't a blind
        // "trajectory always wins," it's a genuine per-field max.
        let from_metrics = RestTotals { rest_ms: 1000, rests: 2 };
        let from_tailer = RestTotals { rest_ms: 400, rests: 1 };
        let merged = reconcile_rest_totals(from_metrics, from_tailer);
        assert_eq!(merged.rest_ms, 1000);
        assert_eq!(merged.rests, 2);
    }

    #[test]
    fn dispatch_error_terminal_recovers_rest_totals_from_the_live_tailer_when_metrics_json_is_zeroed(
    ) {
        // (#2094 second round, finding 1) Reproduces the exact failure this
        // finding names: the runtime crashed (SIGKILL, hard error) after
        // writing a zeroed metrics.json, but AFTER two real rests had
        // already streamed to trajectory.jsonl and been seen live by the
        // tailer. `read_rest_totals` alone gates its own trajectory
        // fallback on metrics.json KEY PRESENCE, not value — so it trusts
        // the zeroed-but-present `rest_ms`/`rests` and never reaches
        // `sum_rest_totals_from_trajectory` itself. The payload must still
        // surface the real totals by reconciling against what the live
        // tailer (`TrajectorySummary`) actually observed as the events
        // streamed, independent of the post-hoc metrics.json read.
        let out = TempDir::new().unwrap();
        let rt = out.path().join(".darkmux-runtime");
        fs::create_dir_all(&rt).unwrap();
        fs::write(rt.join("metrics.json"), r#"{"rest_ms": 0, "rests": 0}"#).unwrap();
        fs::write(
            rt.join("trajectory.jsonl"),
            "{\"type\":\"runtime.rest\",\"seq\":1,\"ts\":1,\"ms\":400}\n\
             {\"type\":\"runtime.rest\",\"seq\":2,\"ts\":2,\"ms\":600}\n",
        )
        .unwrap();

        // Confirms the gap this finding names: read_rest_totals alone
        // trusts the zeroed-but-present metrics.json and never falls
        // through to the trajectory sum.
        let from_metrics = read_rest_totals(out.path());
        assert_eq!((from_metrics.rest_ms, from_metrics.rests), (0, 0));

        // What the live tailer accumulated processing the same two events
        // as they streamed — TrajectorySummary's own running total, kept
        // independently of the post-hoc metrics.json read.
        let from_tailer = RestTotals { rest_ms: 1000, rests: 2 };

        let merged = reconcile_rest_totals(from_metrics, from_tailer);
        assert_eq!(merged.rest_ms, 1000, "payload must carry the tailer-observed total");
        assert_eq!(merged.rests, 2);
    }

    // ─── #2094 finding 8: turn_delay_effective_ms ─────────────────────────

    #[test]
    fn read_turn_delay_effective_ms_prefers_the_metrics_json_field() {
        let out = TempDir::new().unwrap();
        let rt = out.path().join(".darkmux-runtime");
        fs::create_dir_all(&rt).unwrap();
        fs::write(
            rt.join("metrics.json"),
            r#"{"rest_ms": 1000, "rests": 2, "turn_delay_effective_ms": 500}"#,
        )
        .unwrap();
        let rest = read_rest_totals(out.path());
        let effective = read_turn_delay_effective_ms(out.path(), rest);
        assert_eq!(effective, Some(500), "the exact resolved constant, not the 1000/2 average");
    }

    #[test]
    fn read_turn_delay_effective_ms_falls_back_to_rest_average_when_metrics_json_absent() {
        let out = TempDir::new().unwrap();
        let rt = out.path().join(".darkmux-runtime");
        fs::create_dir_all(&rt).unwrap();
        // No metrics.json at all — trajectory carries two rests of 500ms
        // and 300ms, summing 800ms over 2 rests.
        fs::write(
            rt.join("trajectory.jsonl"),
            "{\"type\":\"runtime.rest\",\"seq\":1,\"ts\":1,\"ms\":500}\n\
             {\"type\":\"runtime.rest\",\"seq\":2,\"ts\":2,\"ms\":300}\n",
        )
        .unwrap();
        let rest = read_rest_totals(out.path());
        assert_eq!((rest.rest_ms, rest.rests), (800, 2));
        let effective = read_turn_delay_effective_ms(out.path(), rest);
        assert_eq!(effective, Some(400), "800ms / 2 rests = 400ms average, the derived fallback");
    }

    #[test]
    fn read_turn_delay_effective_ms_falls_back_when_metrics_json_lacks_the_field() {
        let out = TempDir::new().unwrap();
        let rt = out.path().join(".darkmux-runtime");
        fs::create_dir_all(&rt).unwrap();
        // metrics.json present (pre-finding-8 shape) but has no
        // turn_delay_effective_ms key — must still reach for the average.
        fs::write(rt.join("metrics.json"), r#"{"rest_ms": 600, "rests": 3}"#).unwrap();
        let rest = RestTotals { rest_ms: 600, rests: 3 };
        let effective = read_turn_delay_effective_ms(out.path(), rest);
        assert_eq!(effective, Some(200));
    }

    #[test]
    fn read_turn_delay_effective_ms_is_none_when_nothing_is_knowable() {
        let out = TempDir::new().unwrap();
        // No metrics.json, no trajectory.jsonl, zero rests — genuinely
        // unknowable, not a misleading 0.
        let rest = RestTotals::default();
        let effective = read_turn_delay_effective_ms(out.path(), rest);
        assert_eq!(effective, None);
    }

    #[test]
    fn read_turn_delay_effective_ms_zero_field_does_not_short_circuit_the_derived_average_when_rests_positive(
    ) {
        // (#2094 second round, finding 1) Same shape of bug as rest_ms: a
        // metrics.json written with `turn_delay_effective_ms: 0` (the
        // exit-time write raced a crash and only got a zeroed struct out)
        // must not be trusted as "the resolved cadence was genuinely
        // zero" when `rest` says this dispatch actually rested. Fall
        // through to the derived average instead.
        let out = TempDir::new().unwrap();
        let rt = out.path().join(".darkmux-runtime");
        fs::create_dir_all(&rt).unwrap();
        fs::write(rt.join("metrics.json"), r#"{"turn_delay_effective_ms": 0}"#).unwrap();
        let rest = RestTotals { rest_ms: 1000, rests: 2 };
        let effective = read_turn_delay_effective_ms(out.path(), rest);
        assert_eq!(effective, Some(500), "falls through to 1000/2 = 500, not the zeroed field");
    }

    #[test]
    fn read_turn_delay_effective_ms_zero_field_is_honored_when_there_were_genuinely_no_rests() {
        // A single-turn dispatch that never rested legitimately has
        // turn_delay_effective_ms=0 with rests=0 — that zero IS the truth,
        // not a raced write, and must still be returned as Some(0) rather
        // than falling to None.
        let out = TempDir::new().unwrap();
        let rt = out.path().join(".darkmux-runtime");
        fs::create_dir_all(&rt).unwrap();
        fs::write(rt.join("metrics.json"), r#"{"turn_delay_effective_ms": 0}"#).unwrap();
        let rest = RestTotals::default();
        let effective = read_turn_delay_effective_ms(out.path(), rest);
        assert_eq!(effective, Some(0));
    }

    // ─── #2094 finding 4: never rest an agentic-REMOTE dispatch ──────────

    #[test]
    fn effective_turn_delay_ms_forces_zero_for_agentic_remote_regardless_of_config() {
        // A remote-agentic dispatch's "brain" is a hosted endpoint, not
        // local LMStudio — there is no local GPU on this host to rest, so
        // the configured rest must never reach it even at a large nonzero
        // value.
        assert_eq!(effective_turn_delay_ms(3000, true), 0);
        assert_eq!(effective_turn_delay_ms(0, true), 0);
    }

    #[test]
    fn effective_turn_delay_ms_passes_through_unchanged_for_local() {
        assert_eq!(effective_turn_delay_ms(3000, false), 3000);
        assert_eq!(effective_turn_delay_ms(0, false), 0);
    }

    // ─── MUST FIX 3 (merge-gate review of #2165): pin the two `bounds`
    //     WIRING sites — deleting either insertion failed no test before
    //     this. `dispatch_start_payload_json` below covers the first
    //     (`dispatch()`'s real construction was un-unit-testable before the
    //     #2165 review extracted it into this pure fn); the
    //     `enrich_envelope_with_summary` tests further down cover the
    //     second (`obj.insert("bounds", …)`).

    #[test]
    #[serial]
    fn dispatch_start_payload_json_carries_the_bounds_key_with_the_expected_shape() {
        let prev = std::env::var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL").ok();
        unsafe { std::env::remove_var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL") };

        let payload = dispatch_start_payload_json(
            "darkmux-runtime:latest",
            "read x.txt",
            "system prompt",
            std::path::Path::new("/tmp/ws"),
            false,
        );

        // The wiring itself: this key would be ABSENT entirely if the
        // `"bounds": resolved_runtime_bounds_json(is_agentic_remote)` line
        // were ever deleted from `dispatch_start_payload_json` — proving
        // this assertion actually exercises the insertion, not just the
        // helper's own internal logic (already pinned by the
        // `resolved_runtime_bounds_json_*` tests below).
        assert!(payload.get("bounds").is_some(), "dispatch_start_payload must carry a bounds key: {payload}");
        assert_eq!(
            payload["bounds"]["max_tokens_per_call"],
            serde_json::json!({"value": null, "source": "built-in"}),
            "bounds must be the SAME shape resolved_runtime_bounds_json produces: {payload}"
        );
        // The rest of the payload survives the extraction unchanged —
        // pinning the refactor didn't silently drop or rename a field.
        assert_eq!(payload["runtime"], serde_json::json!("internal"));
        assert_eq!(payload["image"], serde_json::json!("darkmux-runtime:latest"));
        assert_eq!(payload["prompt_chars"], serde_json::json!("read x.txt".chars().count()));
        assert_eq!(payload["workspace"], serde_json::json!("/tmp/ws"));

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL", v),
                None => std::env::remove_var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL"),
            }
        }
    }

    /// The agentic-remote half of the same wiring: `is_agentic_remote=true`
    /// must reach `bounds.turn_delay_ms`'s `forced-agentic-remote` shape
    /// (MUST FIX 2) through this real call path, not just through
    /// `resolved_runtime_bounds_json` called directly.
    #[test]
    fn dispatch_start_payload_json_forces_turn_delay_ms_for_agentic_remote() {
        let payload = dispatch_start_payload_json(
            "darkmux-runtime:latest",
            "msg",
            "sys",
            std::path::Path::new("/tmp/ws"),
            true,
        );
        assert_eq!(payload["bounds"]["turn_delay_ms"]["source"], serde_json::json!("forced-agentic-remote"));
        assert_eq!(payload["turn_delay_ms"], serde_json::json!(0), "the top-level stamp is also forced");
    }

    // ─── #2165: resolved_runtime_bounds_json — the SAME block shared by
    //     dispatch_start_payload["bounds"] and the envelope's own `bounds` ───
    //
    // (#811 test-isolation) darkmux-crew's dev-dependency on
    // `darkmux-types/test-support` makes `config_access::config()` ALWAYS
    // return the empty default in this crate's own test builds (see that
    // feature's own doc) — so the CONFIG tier is structurally unreachable
    // from here. The two tiers actually exercisable at this layer are ENV
    // and BUILT-IN; the CONFIG tier's precedence is pinned instead by
    // `darkmux-types::config_access`'s own `pick_parsed_with_source` /
    // `*_with_source` tests, which construct it directly.
    #[test]
    #[serial]
    fn resolved_runtime_bounds_json_names_built_in_when_nothing_is_set() {
        for k in [
            "DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL",
            "DARKMUX_RUNTIME_REASONING_CHECKPOINT_INTERVAL",
            "DARKMUX_INACTIVITY_TIMEOUT_SECONDS",
            "DARKMUX_RUNTIME_MAX_TURNS",
            "DARKMUX_RUNTIME_MAX_TOKENS",
            "DARKMUX_TURN_DELAY_MS",
            "DARKMUX_FEEDBACK_INJECTION",
        ] {
            unsafe { std::env::remove_var(k) };
        }
        let bounds = resolved_runtime_bounds_json(false);
        assert_eq!(
            bounds["max_tokens_per_call"],
            serde_json::json!({"value": null, "source": "built-in"}),
            "an unset optional knob names built-in with a null value, not an absent field: {bounds}"
        );
        assert_eq!(
            bounds["reasoning_checkpoint_interval_tokens"],
            serde_json::json!({"value": null, "source": "built-in"})
        );
        assert_eq!(
            bounds["inactivity_timeout_seconds"],
            serde_json::json!({"value": 600, "source": "built-in"})
        );
        assert_eq!(bounds["max_turns"], serde_json::json!({"value": null, "source": "built-in"}));
        assert_eq!(bounds["max_tokens"], serde_json::json!({"value": null, "source": "built-in"}));
        assert_eq!(bounds["feedback_injection"], serde_json::json!({"value": true, "source": "built-in"}));
    }

    #[test]
    #[serial]
    fn resolved_runtime_bounds_json_names_env_when_an_env_var_wins() {
        for k in [
            "DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL",
            "DARKMUX_RUNTIME_REASONING_CHECKPOINT_INTERVAL",
            "DARKMUX_INACTIVITY_TIMEOUT_SECONDS",
        ] {
            unsafe { std::env::remove_var(k) };
        }
        unsafe { std::env::set_var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL", "4000") };
        unsafe { std::env::set_var("DARKMUX_RUNTIME_REASONING_CHECKPOINT_INTERVAL", "500") };
        unsafe { std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", "120") };
        let bounds = resolved_runtime_bounds_json(false);
        assert_eq!(bounds["max_tokens_per_call"], serde_json::json!({"value": 4000, "source": "env"}));
        assert_eq!(
            bounds["reasoning_checkpoint_interval_tokens"],
            serde_json::json!({"value": 500, "source": "env"})
        );
        assert_eq!(
            bounds["inactivity_timeout_seconds"],
            serde_json::json!({"value": 120, "source": "env"})
        );
        for k in [
            "DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL",
            "DARKMUX_RUNTIME_REASONING_CHECKPOINT_INTERVAL",
            "DARKMUX_INACTIVITY_TIMEOUT_SECONDS",
        ] {
            unsafe { std::env::remove_var(k) };
        }
    }

    #[test]
    #[serial]
    fn resolved_runtime_bounds_json_turn_delay_ms_passes_through_for_a_local_dispatch() {
        let prev = std::env::var("DARKMUX_TURN_DELAY_MS").ok();
        unsafe { std::env::set_var("DARKMUX_TURN_DELAY_MS", "3000") };
        let bounds = resolved_runtime_bounds_json(false);
        assert_eq!(
            bounds["turn_delay_ms"],
            serde_json::json!({"value": 3000, "source": "env"}),
            "a LOCAL dispatch's turn_delay_ms row is the plain {{value, source}} shape, \
             same as every other knob — got {bounds}"
        );
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_TURN_DELAY_MS", v),
                None => std::env::remove_var("DARKMUX_TURN_DELAY_MS"),
            }
        }
    }

    /// MUST FIX 2 (merge-gate review): `turn_delay_ms`'s provenance was
    /// decorative — the row paired the FORCED effective value (`0` for
    /// agentic-remote) with the CONFIGURED value's source, so a dispatch
    /// with `config.runtime.turn_delay_ms: 5000` that also happened to be
    /// agentic-remote rendered `{"value": 0, "source": "config"}` — a
    /// source that resolved a DIFFERENT number than the one shown. Fixed:
    /// when forced, `source` becomes the literal `"forced-agentic-remote"`
    /// tier and `configured_value` carries what the operator's own knob
    /// actually resolved to (with its real source, nested), so the row
    /// stays self-explaining instead of silently going quiet about the
    /// configured value.
    #[test]
    #[serial]
    fn resolved_runtime_bounds_json_turn_delay_ms_is_self_explaining_when_forced_agentic_remote() {
        let prev = std::env::var("DARKMUX_TURN_DELAY_MS").ok();
        unsafe { std::env::set_var("DARKMUX_TURN_DELAY_MS", "5000") };
        let bounds = resolved_runtime_bounds_json(true);
        assert_eq!(
            bounds["turn_delay_ms"],
            serde_json::json!({
                "value": 0,
                "source": "forced-agentic-remote",
                "configured_value": {"value": 5000, "source": "env"},
            }),
            "the row must show the forced 0 AND the configured 5000 it overrode, not just the \
             forced value paired with the configured value's source — got {bounds}"
        );
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_TURN_DELAY_MS", v),
                None => std::env::remove_var("DARKMUX_TURN_DELAY_MS"),
            }
        }
    }

    /// Even at the built-in default (`0`, no rest configured), an
    /// agentic-remote dispatch still gets the `forced-agentic-remote`
    /// shape — the override applies regardless of what it overrode.
    #[test]
    #[serial]
    fn resolved_runtime_bounds_json_turn_delay_ms_forced_shape_holds_even_at_the_default() {
        let prev = std::env::var("DARKMUX_TURN_DELAY_MS").ok();
        unsafe { std::env::remove_var("DARKMUX_TURN_DELAY_MS") };
        let bounds = resolved_runtime_bounds_json(true);
        assert_eq!(
            bounds["turn_delay_ms"],
            serde_json::json!({
                "value": 0,
                "source": "forced-agentic-remote",
                "configured_value": {"value": 0, "source": "built-in"},
            }),
        );
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_TURN_DELAY_MS", v),
                None => std::env::remove_var("DARKMUX_TURN_DELAY_MS"),
            }
        }
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

    // ─── #1547: role_profiles map reaches dispatch model resolution ────
    // `resolve_role_aware_profile` (the impure, config_access-reading
    // wrapper `resolve_dispatch_model_internal`/`resolve_selected_profile_model`
    // both chain through) can't be exercised end-to-end here: `config_access`
    // is hard-empty under `test`/`test-support` builds (#811), so
    // `config_access::role_profile(role_id)` always returns `None` inside
    // this crate's own test binary regardless of what a real `config.json`
    // holds. `resolve_role_aware_profile_with` is the pure core that takes
    // the mapped binding as an explicit argument instead of reading it live —
    // these tests exercise every precedence arm through IT, proving the
    // actual resolution logic #1547 wires in (not just that a config setter
    // "worked" — the setter already worked before #1547; the bug was that
    // nothing downstream ever READ the value).

    fn role_profiles_test_registry() -> darkmux_types::ProfileRegistry {
        serde_json::from_str(
            r#"{"profiles":{
                    "fast":{"models":[{"id":"model-fast","n_ctx":32000}]},
                    "big":{"models":[{"id":"model-big","n_ctx":128000}]}
                },
                "default_profile":"fast"}"#,
        )
        .unwrap()
    }

    #[test]
    fn resolve_role_aware_profile_with_honors_the_mapped_binding_when_unmapped_by_override() {
        let reg = role_profiles_test_registry();
        let resolved = resolve_role_aware_profile_with("coder", None, Some("big".to_string()), &reg)
            .unwrap()
            .expect("a mapped role must resolve to its bound profile, not fall through to None");
        assert_eq!(resolved.0, "big", "role_profiles.coder=big must resolve to the `big` profile, not `default_profile`");
        assert_eq!(resolved.1.models[0].id, "model-big");
    }

    #[test]
    fn resolve_role_aware_profile_with_explicit_override_wins_over_the_map() {
        // (#1054/#1475 precedence) An explicit `--profile fast` on THIS call
        // wins even when role_profiles maps the role to a different profile —
        // matching the review launcher's existing precedence for this map.
        let reg = role_profiles_test_registry();
        let resolved = resolve_role_aware_profile_with("coder", Some("fast"), Some("big".to_string()), &reg)
            .unwrap()
            .expect("an explicit override must still resolve");
        assert_eq!(resolved.0, "fast", "explicit --profile must win over the role_profiles map binding");
    }

    #[test]
    fn resolve_role_aware_profile_with_falls_to_default_profile_when_unmapped() {
        // No override, no mapped binding -> default_profile (the fresh-user
        // floor), matching `resolve_active(None)`'s existing behavior.
        let reg = role_profiles_test_registry();
        let resolved = resolve_role_aware_profile_with("coder", None, None, &reg)
            .unwrap()
            .expect("default_profile must still resolve when unmapped");
        assert_eq!(resolved.0, "fast");
    }

    #[test]
    fn resolve_role_aware_profile_with_dangling_mapped_binding_is_a_loud_error() {
        // (#1547 doc: "a role BOUND to a profile that doesn't exist in the
        // registry is a LOUD error, config-leniency contract 7") — matches
        // `resolve_role_profile_with`'s existing posture for the SAME map on
        // the review launcher path; dispatch must not silently fall through
        // to default_profile for a typo'd binding.
        let reg = role_profiles_test_registry();
        let err = resolve_role_aware_profile_with("coder", None, Some("ghost-profile".to_string()), &reg)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("coder"), "names the role: {msg}");
        assert!(msg.contains("ghost-profile"), "names the dangling profile: {msg}");
        assert!(
            msg.contains("config set role_profiles.coder"),
            "hint names the fix: {msg}"
        );
    }

    // ─── #1616: the compactor loads at ITS OWN declared n_ctx ──────────

    #[test]
    fn resolve_compactor_load_window_prefers_the_compactor_s_own_n_ctx() {
        // THE bug: a two-model profile (a big-context primary, a
        // small-context compactor) must load the compactor at ITS OWN
        // declared size, never the primary's.
        let (window, used_fallback) = resolve_compactor_load_window(Some(120_000), Some(262_144));
        assert_eq!(window, Some(120_000));
        assert!(!used_fallback, "the compactor's own n_ctx must not be reported as a fallback");
    }

    #[test]
    fn resolve_compactor_load_window_falls_back_to_the_primary_s_window_when_undeclared() {
        // A model with NO declaration falls back to the current (pre-#1616)
        // behavior — the primary's context window — but the fallback is
        // NAMED so the caller can say so in the load message.
        let (window, used_fallback) = resolve_compactor_load_window(None, Some(262_144));
        assert_eq!(window, Some(262_144));
        assert!(used_fallback, "borrowing the primary's window must be reported as a fallback");
    }

    #[test]
    fn resolve_compactor_load_window_is_none_when_neither_side_declares_one() {
        let (window, used_fallback) = resolve_compactor_load_window(None, None);
        assert_eq!(window, None);
        assert!(used_fallback, "still a fallback attempt, even though it resolved to nothing");
    }

    #[test]
    #[serial]
    fn resolve_compactor_n_ctx_internal_reads_the_compactor_s_own_registry_entry() {
        // (#1616 reproduction) The `deep`-shaped profile: a big-context
        // primary PLUS a small-context compactor, both declared in the same
        // profile's `models[]`. Pre-fix, `resolve_context_window_internal`
        // (the primary's window) was reused as the compactor's LOAD ctx —
        // this resolver must instead find the compactor's OWN entry.
        let tmp = TempDir::new().unwrap();
        let pf = tmp.path().join("profiles.json");
        std::fs::write(
            &pf,
            r#"{"profiles":{
                    "deep":{"default_model":"primary-big","models":[
                        {"id":"primary-big","n_ctx":262144},
                        {"id":"darkmux:util-4b","n_ctx":120000}
                    ]}
                },
                "default_profile":"deep"}"#,
        )
        .unwrap();
        let prev = std::env::var("DARKMUX_PROFILES").ok();
        // SAFETY: serialized via #[serial]; restored below.
        unsafe { std::env::remove_var("DARKMUX_PROFILES") };
        let compactor_n_ctx =
            resolve_compactor_n_ctx_internal(None, pf.to_str(), "darkmux:util-4b").unwrap();
        let primary_window = resolve_context_window_internal(None, pf.to_str()).unwrap();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_PROFILES", v),
                None => std::env::remove_var("DARKMUX_PROFILES"),
            }
        }
        assert_eq!(compactor_n_ctx, Some(120_000), "must read the compactor's OWN entry, not the primary's");
        assert_eq!(primary_window, Some(262_144), "the primary's own resolver must be untouched by the fix");
        assert_ne!(compactor_n_ctx, primary_window, "precondition: the two models declare different sizes");
    }

    #[test]
    #[serial]
    fn resolve_compactor_n_ctx_internal_matches_across_the_darkmux_namespace() {
        // The machine-level `internal.utility` binding is typically
        // namespaced (`darkmux:qwen3-4b-instruct-2507`), but an operator's
        // profile entry may name the model either way — both must resolve
        // to the same declared n_ctx.
        let tmp = TempDir::new().unwrap();
        let pf = tmp.path().join("profiles.json");
        std::fs::write(
            &pf,
            r#"{"profiles":{
                    "deep":{"default_model":"primary-big","models":[
                        {"id":"primary-big","n_ctx":262144},
                        {"id":"util-4b","n_ctx":120000}
                    ]}
                },
                "default_profile":"deep"}"#,
        )
        .unwrap();
        let prev = std::env::var("DARKMUX_PROFILES").ok();
        // SAFETY: serialized via #[serial]; restored below.
        unsafe { std::env::remove_var("DARKMUX_PROFILES") };
        // Ask with the NAMESPACED form; the registry entry is bare.
        let compactor_n_ctx =
            resolve_compactor_n_ctx_internal(None, pf.to_str(), "darkmux:util-4b").unwrap();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_PROFILES", v),
                None => std::env::remove_var("DARKMUX_PROFILES"),
            }
        }
        assert_eq!(compactor_n_ctx, Some(120_000));
    }

    #[test]
    #[serial]
    fn resolve_compactor_n_ctx_internal_is_none_when_the_profile_has_no_entry_for_it() {
        // The compactor is a machine-wide binding, decoupled from any
        // profile — a profile that never lists it must resolve `None`, not
        // an error, so the caller falls back to the primary's window.
        let tmp = TempDir::new().unwrap();
        let pf = tmp.path().join("profiles.json");
        std::fs::write(
            &pf,
            r#"{"profiles":{
                    "fast":{"models":[{"id":"model-a","n_ctx":32000}]}
                },
                "default_profile":"fast"}"#,
        )
        .unwrap();
        let prev = std::env::var("DARKMUX_PROFILES").ok();
        // SAFETY: serialized via #[serial]; restored below.
        unsafe { std::env::remove_var("DARKMUX_PROFILES") };
        let compactor_n_ctx =
            resolve_compactor_n_ctx_internal(None, pf.to_str(), "darkmux:util-4b").unwrap();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_PROFILES", v),
                None => std::env::remove_var("DARKMUX_PROFILES"),
            }
        }
        assert_eq!(compactor_n_ctx, None);
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
            false,
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

    /// (#1959 packet 2) `read_only=true` appends `:ro` to the WORKSPACE
    /// bind only — the out-dir mount stays read-write regardless, since the
    /// runtime always writes its own bookkeeping (trajectory, findings)
    /// there. The crawl launcher sets `DispatchOpts::workspace_read_only`
    /// so a role holding only `read`/`exec`/`report_finding` can't write
    /// into the corpus tree even via a shell escape.
    #[test]
    fn apply_volume_mounts_appends_ro_to_workspace_bind_when_read_only() {
        let mut args: Vec<String> = Vec::new();
        apply_volume_mounts(
            &mut args,
            Path::new("/host/workspace"),
            Path::new("/host/out"),
            true,
        );
        assert_eq!(
            args,
            vec![
                "-v",
                "/host/workspace:/workspace:ro",
                "-v",
                "/host/out:/darkmux-out",
            ],
            "workspace bind gains :ro; out-dir bind is unaffected"
        );
    }

    // ─── #2153: resolve_host_out (caller-named out dir) ────────────────

    #[test]
    fn resolve_host_out_none_falls_back_to_a_fresh_tempdir_named_from_role_and_micros() {
        let dir = resolve_host_out(None, "crawler", 424242).unwrap();
        assert!(dir.is_dir(), "the fresh tempdir must exist");
        assert!(
            dir.file_name().unwrap().to_str().unwrap().starts_with("darkmux-out-crawler-424242"),
            "unchanged naming convention: darkmux-out-<role>-<unix_micros>, got {}",
            dir.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_host_out_some_creates_the_named_dir() {
        let parent = TempDir::new().unwrap();
        let named = parent.path().join("units").join("u-0002").join("out");
        // `resolve_host_out` uses `create_dir`, not `create_dir_all` — the
        // PARENT must already exist (the crawl launcher creates it via its
        // own `create_dir_all` before calling this).
        std::fs::create_dir_all(named.parent().unwrap()).unwrap();
        assert!(!named.exists(), "sanity: the dir itself must not exist yet");

        let dir = resolve_host_out(Some(&named), "crawler", 1).unwrap();
        assert_eq!(dir, named, "the caller-named dir is returned verbatim");
        assert!(dir.is_dir(), "the caller-named dir must be created");
    }

    #[cfg(unix)]
    #[test]
    fn resolve_host_out_some_locks_the_dir_to_0700() {
        use std::os::unix::fs::PermissionsExt;
        let parent = TempDir::new().unwrap();
        let named = parent.path().join("out");

        let dir = resolve_host_out(Some(&named), "crawler", 1).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "caller-provided out-dir must be locked to 0700");
    }

    #[test]
    fn resolve_host_out_some_refuses_a_pre_existing_dir_rather_than_reusing_it() {
        // (#2158) A dir already sitting at the caller-named path — a
        // leftover from a prior run, or a planted symlink — must never be
        // silently reused: `create_dir`'s own atomicity (one syscall, no
        // separate exists-check window) is the TOCTOU fix, and this test
        // proves the caller sees a named refusal rather than a mount into
        // whatever was already there.
        let parent = TempDir::new().unwrap();
        let named = parent.path().join("out");
        std::fs::create_dir_all(&named).unwrap();
        // Prove the pre-existing dir is untouched evidence: drop a sentinel
        // file that a wrongly-successful `resolve_host_out` call would have
        // no reason to disturb, then assert it's still there after.
        std::fs::write(named.join("sentinel.txt"), b"pre-existing").unwrap();

        let err = resolve_host_out(Some(&named), "crawler", 1).unwrap_err();
        assert!(
            err.to_string().contains("already exists"),
            "expected a named 'already exists' refusal, got: {err:#}"
        );
        assert!(named.join("sentinel.txt").exists(), "the pre-existing dir must be left untouched");
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
            role_id: "test-role".to_string(),
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
            // (#1548) true here (vs the other assertion test's false) so the
            // two tests together pin BOTH string forms the container's
            // falsy-set reader must distinguish.
            feedback_injection: true,
            // (#2094) Nonzero here so the complete-vector assertion below
            // pins the forwarded `-e DARKMUX_TURN_DELAY_MS=<n>` pair too.
            turn_delay_ms: 3000,
            // (#2094 finding 1) A distinct, non-default value so the
            // complete-vector assertion below pins the forwarded
            // `-e DARKMUX_INACTIVITY_TIMEOUT_SECONDS=<n>` pair too.
            inactivity_timeout_seconds: 900,
            inactivity_timeout_seconds_source: darkmux_types::config_access::Source::Config,
            max_pause_ms_env: None,
            remote_chat_url: None,
            remote_needs_auth: false,
            base_url_override: None,
            workspace_read_only: false,
            resume_checkpoint: false,
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

        // 5b. (#1548) Verify the feedback-injection env var is forwarded —
        // the fix for the #1548 dead surface (`config.feedback_injection: true`
        // in this test's config → `DARKMUX_FEEDBACK_INJECTION=true` on argv).
        assert_eq!(argv[21], "-e");
        assert_eq!(argv[22], "DARKMUX_FEEDBACK_INJECTION=true");

        // 5c. (#2094) Verify the turn-delay env var is forwarded — ALWAYS,
        // including at nonzero values (`config.turn_delay_ms: 3000` in this
        // test's config → `DARKMUX_TURN_DELAY_MS=3000` on argv).
        assert_eq!(argv[23], "-e");
        assert_eq!(argv[24], "DARKMUX_TURN_DELAY_MS=3000");

        // 5d. (#2094 finding 1) Verify the inactivity-timeout env var is
        // forwarded too (`config.inactivity_timeout_seconds: 900` in this
        // test's config → `DARKMUX_INACTIVITY_TIMEOUT_SECONDS=900` on argv)
        // — the piece #2094's original cut left unforwarded, so the
        // runtime's soft-warning detector silently used its own 600s
        // literal default instead of the operator's configured budget.
        assert_eq!(argv[25], "-e");
        assert_eq!(argv[26], "DARKMUX_INACTIVITY_TIMEOUT_SECONDS=900");

        // 5e. (#2165) Verify the inactivity-timeout SOURCE env var is
        // forwarded alongside the value — a distinct, non-default source
        // (`Config`, not the built-in default) so this assertion actually
        // pins forwarding rather than coincidentally matching a default.
        assert_eq!(argv[27], "-e");
        assert_eq!(argv[28], "DARKMUX_INACTIVITY_TIMEOUT_SECONDS_SOURCE=config");

        // 6. Verify runtime injection (non-default image)
        assert_eq!(argv[29], "-v");
        assert_eq!(
            argv[30],
            "/home/op/.darkmux/runtime/darkmux-runtime:/darkmux-runtime:ro"
        );
        assert_eq!(argv[31], "--entrypoint");
        assert_eq!(argv[32], "/darkmux-runtime");

        // 7. Verify `--` + image + runtime CLI args
        assert_eq!(argv[33], "--");
        assert_eq!(argv[34], "rust:slim"); // image
        assert_eq!(argv[35], "run"); // runtime subcommand
        assert_eq!(argv[36], "--model");
        assert_eq!(argv[37], "llama3-8b");
        // (Security audit, #2114 resume follow-up) Unconditional, every
        // dispatch — see `DockerRunConfig::role_id`'s own doc.
        assert_eq!(argv[38], "--role-id");
        assert_eq!(argv[39], "test-role");
        assert_eq!(argv[40], "--system");
        assert_eq!(argv[41], "You are a coding assistant.");
        // (#386) The message goes via the out-dir mount, not argv — argv carries
        // the constant `--prompt-file <container path>`, never the brief itself.
        assert_eq!(argv[42], "--prompt-file");
        assert_eq!(argv[43], "/darkmux-out/.prompt.txt");
        assert!(
            !argv.iter().any(|a| a == "Fix the bug in main.rs"),
            "the message must NOT appear anywhere in the docker argv (#386): {argv:?}"
        );

        // 8. Verify json flag
        assert_eq!(argv[44], "--json");

        // 9. Verify allowed tools
        assert_eq!(argv[45], "--allowed-tools");
        assert_eq!(argv[46], "exec,edit");

        // 10. Verify compaction flags — flag names must match the runtime's
        // accepted set verbatim (an unknown flag exits the container with 2).
        assert_eq!(argv[47], "--compact-threshold-tokens");
        assert_eq!(argv[48], "4096");
        assert_eq!(argv[49], "--compactor-model");
        assert_eq!(argv[50], "util-model");
        assert_eq!(argv[51], "--compact-threshold-ratio");
        assert_eq!(argv[52], "0.75");
        assert_eq!(argv[53], "--context-window");
        assert_eq!(argv[54], "32000");
        assert_eq!(argv[55], "--compact-strategy");
        assert_eq!(argv[56], "structured-slot");
        assert_eq!(argv[57], "--bail-after-compactions");
        assert_eq!(argv[58], "10");
        assert_eq!(argv[59], "--compactor-custom-instructions");
        assert_eq!(argv[60], "Be terse.");

        // 11. Verify feedback templates JSON
        assert_eq!(argv[61], "--feedback-templates-json");
        // The JSON value should contain the error template
        assert!(argv[62].contains("error"));
        assert!(argv[62].contains("An error occurred"));

        // Total arg count: 61 (0..=60) — 53 pre-#1548, +2 for
        // `-e DARKMUX_FEEDBACK_INJECTION=<v>`, +2 for
        // `-e DARKMUX_TURN_DELAY_MS=<ms>` (#2094), +2 for
        // `-e DARKMUX_INACTIVITY_TIMEOUT_SECONDS=<n>` (#2094 finding 1),
        // +2 for `--role-id <id>` (security audit, #2114 resume follow-up), +2 for
        // `-e DARKMUX_INACTIVITY_TIMEOUT_SECONDS_SOURCE=<tier>` (#2165).
        assert_eq!(argv.len(), 63);
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
            role_id: "test-role".to_string(),
            model: "default-model".to_string(),
            system_prompt: "Basic role.".to_string(),
            message: "Hello world".to_string(),
            json: false,
            allowed_tools: None,
            compaction: crate::dispatch::CompactionDispatchArgs::default(),
            feedback_templates: serde_json::Value::Null,
            cache_dir: PathBuf::from("/tmp/cache"),
            // (#1548 QA finding) FALSE here, deliberately. The sibling
            // full-argv test uses `true`, and its comment claimed the two
            // "together pin BOTH string forms" — which was not true: nothing
            // asserted the false form host-side. A comment claiming coverage
            // that doesn't exist is the same defect class this PR removes, so
            // the fix is to make the claim true rather than delete it.
            //
            // The forms are load-bearing and the asymmetry is the reason:
            // the runtime parses `"0"|"off"|"false"|"no"` as falsy and
            // EVERYTHING ELSE as on (runtime/src/feedback.rs). So the OFF
            // state is the fragile one — any rendering outside that set
            // (`False`, `disabled`, a debug-formatted `Some(false)`) reads
            // as ON, and the opt-out silently stops working while the ON
            // case keeps looking fine. Pinning `true` alone would not have
            // caught it, which is exactly why the sibling test's claim to
            // cover "both string forms" mattered.
            feedback_injection: false,
            turn_delay_ms: 0,
            inactivity_timeout_seconds: 600,
            inactivity_timeout_seconds_source: darkmux_types::config_access::Source::BuiltIn,
            max_pause_ms_env: None,
            remote_chat_url: None,
            remote_needs_auth: false,
            base_url_override: None,
            workspace_read_only: false,
            resume_checkpoint: false,
        };

        let argv = build_docker_run_argv(&config);

        // (#1548) The env var is ALWAYS forwarded — minimal dispatch
        // included, and in its off state too. An opt-out that silently
        // stops being transmitted is indistinguishable from the bug.
        assert!(
            argv.contains(&"DARKMUX_FEEDBACK_INJECTION=false".to_string()),
            "the OFF state must be forwarded verbatim — the runtime only honors \
             `0|off|false|no`, so any other rendering silently re-enables it: {argv:?}"
        );

        // (#2094) The turn-delay env var is ALWAYS forwarded too, including
        // at its unconfigured `0` value — never an absent var the container
        // has to interpret as "not configured."
        assert!(
            argv.contains(&"DARKMUX_TURN_DELAY_MS=0".to_string()),
            "the unconfigured (0) turn-delay must still be forwarded verbatim: {argv:?}"
        );

        // (#2094 finding 1) The inactivity-timeout env var is ALWAYS
        // forwarded too — a minimal dispatch still needs the runtime's
        // soft-warning detector to see the operator's real budget, not
        // silently fall back to its own 600s literal default.
        assert!(
            argv.contains(&"DARKMUX_INACTIVITY_TIMEOUT_SECONDS=600".to_string()),
            "the resolved inactivity timeout must be forwarded verbatim: {argv:?}"
        );

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
            role_id: "test-role".to_string(),
            model: "default-model".to_string(),
            system_prompt: "Tool-less reviewer.".to_string(),
            message: "Review this.".to_string(),
            json: true,
            allowed_tools: None,
            compaction: crate::dispatch::CompactionDispatchArgs::default(),
            feedback_templates: serde_json::Value::Null,
            cache_dir: PathBuf::from("/tmp/cache"),
            feedback_injection: true,
            turn_delay_ms: 0,
            inactivity_timeout_seconds: 600,
            inactivity_timeout_seconds_source: darkmux_types::config_access::Source::BuiltIn,
            max_pause_ms_env: None,
            remote_chat_url: None,
            remote_needs_auth: false,
            base_url_override: None,
            workspace_read_only: false,
            resume_checkpoint: false,
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
            role_id: "test-role".to_string(),
            model: "m".to_string(),
            system_prompt: "role".to_string(),
            message: "msg".to_string(),
            json: false,
            allowed_tools: None,
            compaction: crate::dispatch::CompactionDispatchArgs::default(),
            feedback_templates: serde_json::Value::Null,
            cache_dir: PathBuf::from("/tmp/cache"),
            feedback_injection: true,
            turn_delay_ms: 0,
            inactivity_timeout_seconds: 600,
            inactivity_timeout_seconds_source: darkmux_types::config_access::Source::BuiltIn,
            max_pause_ms_env: None,
            remote_chat_url: None,
            remote_needs_auth: false,
            base_url_override: None,
            workspace_read_only: false,
            resume_checkpoint: false,
        }
    }

    /// (#1959 packet 2) The full-argv wiring: `workspace_read_only: true`
    /// on `DockerRunConfig` must reach the actual `docker run` argv as
    /// `-v /tmp/ws:/workspace:ro`, not just be asserted at the
    /// `apply_volume_mounts` unit level above.
    #[test]
    fn build_docker_run_argv_appends_ro_to_workspace_mount_when_read_only() {
        let mut config = base_argv_config();
        config.workspace_read_only = true;
        let argv = build_docker_run_argv(&config);
        assert!(
            argv.windows(2).any(|w| w[0] == "-v" && w[1] == "/tmp/ws:/workspace:ro"),
            "expected -v /tmp/ws:/workspace:ro in argv: {argv:?}"
        );
    }

    // ─── #2114: resume argv emission ──────────────────────────────

    #[test]
    fn build_docker_run_argv_omits_resume_flag_by_default() {
        let argv = build_docker_run_argv(&base_argv_config());
        assert!(!argv.contains(&"--resume".to_string()), "expected no --resume in argv: {argv:?}");
    }

    #[test]
    fn build_docker_run_argv_appends_resume_flag_pointing_at_the_out_dir_checkpoint() {
        // (#2114 finding 3) The checkpoint moved off `/workspace/.darkmux`
        // onto the always-writable, never-`:ro` `/darkmux-out` mount.
        let mut config = base_argv_config();
        config.resume_checkpoint = true;
        let argv = build_docker_run_argv(&config);
        assert!(
            argv.windows(2).any(|w| w[0] == "--resume" && w[1] == "/darkmux-out/checkpoint.json"),
            "expected --resume /darkmux-out/checkpoint.json in argv: {argv:?}"
        );
    }

    // ─── #2114 follow-up: stage_resume_checkpoint (the --resume-from trigger) ──

    /// A minimal, structurally-valid checkpoint body — just enough to pass
    /// `stage_resume_checkpoint`'s sanity check (object, numeric
    /// `schema_version`, array `messages`). Not a real `RunCheckpoint` (this
    /// crate has no access to that type — see `stage_resume_checkpoint`'s
    /// own doc); the runtime is the one authority on full schema validity.
    fn sample_checkpoint_json() -> String {
        sample_checkpoint_json_for_role("coder")
    }

    /// (Security audit, #2114 resume follow-up) Same shape as
    /// `sample_checkpoint_json`, with the `role_id` field parameterized so
    /// role-mismatch tests can name a DIFFERENT role than the resuming
    /// dispatch. Real checkpoints (schema v3+) always carry this field.
    fn sample_checkpoint_json_for_role(role_id: &str) -> String {
        serde_json::json!({
            "schema_version": 3,
            "role_id": role_id,
            "messages": [],
            "turns": 1,
            "total_prompt_tokens": 0,
            "total_completion_tokens": 0,
            "compactions": 0,
            "rest_ms": 0,
            "rests": 0,
            "pending_hand_back": null,
            "pending_tool_calls": null,
            "pending_tool_calls_seq_base": 0,
            "written_at_unix_ms": 0,
        })
        .to_string()
    }

    /// Default fixture workspace path/mode used by every test below that
    /// isn't specifically exercising the workspace-mismatch/escalation
    /// gate — both the "origin" file and the resuming dispatch's own
    /// expected values default to this SAME path/mode, so they match by
    /// construction and the workspace gate is a no-op for those tests.
    const FIXTURE_WORKSPACE: &str = "/tmp/darkmux-test-ws";

    /// (Security audit, #2114 resume follow-up) Writes the
    /// `RESUME_ORIGIN_FILENAME` provenance file `write_resume_origin_meta`
    /// writes in production, so tests exercising the happy path (or the
    /// workspace gate specifically) don't have to hand-roll the JSON.
    fn write_origin(dir: &std::path::Path, workspace: &str, read_only: bool) {
        write_resume_origin_meta(dir, std::path::Path::new(workspace), read_only);
    }

    #[test]
    fn stage_resume_checkpoint_copies_a_valid_checkpoint_into_the_new_out_dir() {
        let prior = TempDir::new().unwrap();
        std::fs::write(prior.path().join(CHECKPOINT_FILENAME), sample_checkpoint_json()).unwrap();
        write_origin(prior.path(), FIXTURE_WORKSPACE, false);
        let new_out = TempDir::new().unwrap();

        stage_resume_checkpoint(
            prior.path(),
            new_out.path(),
            "coder",
            std::path::Path::new(FIXTURE_WORKSPACE),
            false,
        )
        .unwrap();

        let staged = std::fs::read_to_string(new_out.path().join(CHECKPOINT_FILENAME)).unwrap();
        assert_eq!(staged, sample_checkpoint_json(), "staged checkpoint must be a byte-identical copy");
        // The prior dir's own copy is untouched — it stays behind as evidence.
        assert!(prior.path().join(CHECKPOINT_FILENAME).is_file());
    }

    #[test]
    fn stage_resume_checkpoint_missing_file_errors_and_copies_nothing() {
        let prior = TempDir::new().unwrap(); // no checkpoint.json written
        let new_out = TempDir::new().unwrap();

        let err = stage_resume_checkpoint(
            prior.path(),
            new_out.path(),
            "coder",
            std::path::Path::new(FIXTURE_WORKSPACE),
            false,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("RESUME CHECKPOINT NOT FOUND"),
            "expected a named RESUME CHECKPOINT NOT FOUND error, got: {err:#}"
        );
        assert!(
            !new_out.path().join(CHECKPOINT_FILENAME).exists(),
            "no container should ever see a checkpoint that was never validated"
        );
    }

    #[test]
    fn stage_resume_checkpoint_invalid_json_errors_and_copies_nothing() {
        let prior = TempDir::new().unwrap();
        std::fs::write(prior.path().join(CHECKPOINT_FILENAME), "not json at all {{{").unwrap();
        let new_out = TempDir::new().unwrap();

        let err = stage_resume_checkpoint(
            prior.path(),
            new_out.path(),
            "coder",
            std::path::Path::new(FIXTURE_WORKSPACE),
            false,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("RESUME CHECKPOINT INVALID"),
            "expected a named RESUME CHECKPOINT INVALID error, got: {err:#}"
        );
        assert!(!new_out.path().join(CHECKPOINT_FILENAME).exists());
    }

    #[test]
    fn stage_resume_checkpoint_wrong_shape_errors_and_copies_nothing() {
        // Valid JSON, but missing the checkpoint schema's required keys —
        // e.g. some unrelated JSON file that happened to be at this path.
        let prior = TempDir::new().unwrap();
        std::fs::write(prior.path().join(CHECKPOINT_FILENAME), r#"{"hello": "world"}"#).unwrap();
        let new_out = TempDir::new().unwrap();

        let err = stage_resume_checkpoint(
            prior.path(),
            new_out.path(),
            "coder",
            std::path::Path::new(FIXTURE_WORKSPACE),
            false,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("RESUME CHECKPOINT INVALID"),
            "expected a named RESUME CHECKPOINT INVALID error, got: {err:#}"
        );
        assert!(!new_out.path().join(CHECKPOINT_FILENAME).exists());
    }

    /// End-to-end through `dispatch`'s own construction site: a caller that
    /// sets `DispatchOpts::resume_from` to a dir with NO checkpoint gets a
    /// named error and — the mutation-tested guard — the container is never
    /// spawned for it. Exercised via `stage_resume_checkpoint` directly
    /// (the same function `dispatch` calls right after allocating its own
    /// fresh `host_out`, before anything else touches it) rather than a
    /// live `dispatch()` call, which would require a real docker/LMStudio
    /// environment this unit test suite doesn't have.
    #[test]
    fn stage_resume_checkpoint_is_the_only_gate_before_the_copy_lands() {
        let prior = TempDir::new().unwrap();
        // Deliberately truncated write (simulates a killed writer) — valid
        // JSON syntax is required by `stage_resume_checkpoint`'s sanity
        // check, but here the whole file is empty, which fails at the
        // `serde_json::from_str` step, not the schema-shape step.
        std::fs::write(prior.path().join(CHECKPOINT_FILENAME), "").unwrap();
        let new_out = TempDir::new().unwrap();

        let err = stage_resume_checkpoint(
            prior.path(),
            new_out.path(),
            "coder",
            std::path::Path::new(FIXTURE_WORKSPACE),
            false,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("RESUME CHECKPOINT INVALID"));
        assert!(!new_out.path().join(CHECKPOINT_FILENAME).exists());
    }

    // ─── security audit, #2114 resume follow-up: host-side role gate ──────

    #[test]
    fn stage_resume_checkpoint_refuses_a_checkpoint_written_for_a_different_role() {
        let prior = TempDir::new().unwrap();
        std::fs::write(
            prior.path().join(CHECKPOINT_FILENAME),
            sample_checkpoint_json_for_role("role-a"),
        )
        .unwrap();
        write_origin(prior.path(), FIXTURE_WORKSPACE, false);
        let new_out = TempDir::new().unwrap();

        let err = stage_resume_checkpoint(
            prior.path(),
            new_out.path(),
            "role-b",
            std::path::Path::new(FIXTURE_WORKSPACE),
            false,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("RESUME CHECKPOINT ROLE MISMATCH"),
            "expected a named RESUME CHECKPOINT ROLE MISMATCH error, got: {err:#}"
        );
        assert!(
            !new_out.path().join(CHECKPOINT_FILENAME).exists(),
            "a role-mismatched checkpoint must never be staged for the container to see"
        );
    }

    #[test]
    fn stage_resume_checkpoint_refuses_a_checkpoint_with_no_role_id_at_all() {
        // A hand-crafted / forged file could easily omit role_id even
        // though it passes the schema_version + messages shape checks —
        // absence must refuse, never "match anything".
        let prior = TempDir::new().unwrap();
        let body = serde_json::json!({
            "schema_version": 3,
            "messages": [],
            "turns": 0,
            "total_prompt_tokens": 0,
            "total_completion_tokens": 0,
            "compactions": 0,
            "rest_ms": 0,
            "rests": 0,
            "pending_hand_back": null,
            "pending_tool_calls": null,
            "pending_tool_calls_seq_base": 0,
            "written_at_unix_ms": 0,
        })
        .to_string();
        std::fs::write(prior.path().join(CHECKPOINT_FILENAME), body).unwrap();
        write_origin(prior.path(), FIXTURE_WORKSPACE, false);
        let new_out = TempDir::new().unwrap();

        let err = stage_resume_checkpoint(
            prior.path(),
            new_out.path(),
            "coder",
            std::path::Path::new(FIXTURE_WORKSPACE),
            false,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("RESUME CHECKPOINT ROLE MISMATCH"));
        assert!(!new_out.path().join(CHECKPOINT_FILENAME).exists());
    }

    #[test]
    fn stage_resume_checkpoint_refuses_a_stale_pre_v3_checkpoint() {
        // (CONSIDER 6, security audit) A v2 checkpoint has no `role_id` at
        // all — must read as STALE SCHEMA, never as a role mismatch
        // (`role_id: "<missing>"` would misreport an honest version gap).
        let prior = TempDir::new().unwrap();
        let body = serde_json::json!({
            "schema_version": 2,
            "messages": [],
            "turns": 0,
            "total_prompt_tokens": 0,
            "total_completion_tokens": 0,
            "compactions": 0,
            "rest_ms": 0,
            "rests": 0,
            "pending_hand_back": null,
            "pending_tool_calls": null,
            "pending_tool_calls_seq_base": 0,
            "written_at_unix_ms": 0,
        })
        .to_string();
        std::fs::write(prior.path().join(CHECKPOINT_FILENAME), body).unwrap();
        write_origin(prior.path(), FIXTURE_WORKSPACE, false);
        let new_out = TempDir::new().unwrap();

        let err = stage_resume_checkpoint(
            prior.path(),
            new_out.path(),
            "coder",
            std::path::Path::new(FIXTURE_WORKSPACE),
            false,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("RESUME CHECKPOINT STALE SCHEMA"),
            "expected a named STALE SCHEMA error, got: {err:#}"
        );
        assert!(
            !format!("{err:#}").contains("ROLE MISMATCH"),
            "a version gap must never read as a role mismatch: {err:#}"
        );
    }

    #[test]
    fn stage_resume_checkpoint_happy_path_same_role_resumes() {
        let prior = TempDir::new().unwrap();
        std::fs::write(
            prior.path().join(CHECKPOINT_FILENAME),
            sample_checkpoint_json_for_role("coder"),
        )
        .unwrap();
        write_origin(prior.path(), FIXTURE_WORKSPACE, false);
        let new_out = TempDir::new().unwrap();

        stage_resume_checkpoint(
            prior.path(),
            new_out.path(),
            "coder",
            std::path::Path::new(FIXTURE_WORKSPACE),
            false,
        )
        .expect("same-role resume must succeed");
        assert!(new_out.path().join(CHECKPOINT_FILENAME).is_file());
    }

    // ─── security audit, #2114 resume follow-up: host-side workspace gate ─

    #[test]
    fn stage_resume_checkpoint_refuses_with_no_origin_record_at_all() {
        // A checkpoint dir from BEFORE this fix (write_resume_origin_meta
        // didn't exist yet) has no resume_origin.json — refuse, never guess.
        let prior = TempDir::new().unwrap();
        std::fs::write(
            prior.path().join(CHECKPOINT_FILENAME),
            sample_checkpoint_json_for_role("coder"),
        )
        .unwrap();
        // Deliberately no write_origin() call.
        let new_out = TempDir::new().unwrap();

        let err = stage_resume_checkpoint(
            prior.path(),
            new_out.path(),
            "coder",
            std::path::Path::new(FIXTURE_WORKSPACE),
            false,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("RESUME ORIGIN UNKNOWN"),
            "expected a named RESUME ORIGIN UNKNOWN error, got: {err:#}"
        );
        assert!(!new_out.path().join(CHECKPOINT_FILENAME).exists());
    }

    #[test]
    fn stage_resume_checkpoint_refuses_a_different_workspace_path() {
        let prior = TempDir::new().unwrap();
        std::fs::write(
            prior.path().join(CHECKPOINT_FILENAME),
            sample_checkpoint_json_for_role("coder"),
        )
        .unwrap();
        write_origin(prior.path(), "/tmp/original-tree", false);
        let new_out = TempDir::new().unwrap();

        let err = stage_resume_checkpoint(
            prior.path(),
            new_out.path(),
            "coder",
            std::path::Path::new("/tmp/a-different-tree"),
            false,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("RESUME WORKSPACE MISMATCH"),
            "expected a named RESUME WORKSPACE MISMATCH error, got: {err:#}"
        );
        assert!(!new_out.path().join(CHECKPOINT_FILENAME).exists());
    }

    #[test]
    fn stage_resume_checkpoint_refuses_upgrading_a_read_only_origin_to_read_write() {
        // The escalation the audit named: a crawl-kind unit's workspace was
        // :ro (the model couldn't write it); resuming read-write would hand
        // it write access it never had.
        let prior = TempDir::new().unwrap();
        std::fs::write(
            prior.path().join(CHECKPOINT_FILENAME),
            sample_checkpoint_json_for_role("crawler"),
        )
        .unwrap();
        write_origin(prior.path(), FIXTURE_WORKSPACE, true); // origin was read-only
        let new_out = TempDir::new().unwrap();

        let err = stage_resume_checkpoint(
            prior.path(),
            new_out.path(),
            "crawler",
            std::path::Path::new(FIXTURE_WORKSPACE),
            false, // this dispatch would mount read-write
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("RESUME WORKSPACE MOUNT ESCALATION"),
            "expected a named RESUME WORKSPACE MOUNT ESCALATION error, got: {err:#}"
        );
        assert!(!new_out.path().join(CHECKPOINT_FILENAME).exists());
    }

    #[test]
    fn stage_resume_checkpoint_allows_downgrading_a_read_write_origin_to_read_only() {
        // The opposite direction is safe (strictly MORE restrictive than
        // the original run) and must not be refused.
        let prior = TempDir::new().unwrap();
        std::fs::write(
            prior.path().join(CHECKPOINT_FILENAME),
            sample_checkpoint_json_for_role("coder"),
        )
        .unwrap();
        write_origin(prior.path(), FIXTURE_WORKSPACE, false); // origin was read-write
        let new_out = TempDir::new().unwrap();

        stage_resume_checkpoint(
            prior.path(),
            new_out.path(),
            "coder",
            std::path::Path::new(FIXTURE_WORKSPACE),
            true, // this dispatch mounts read-only — strictly safer
        )
        .expect("downgrading to a stricter mount must be allowed");
        assert!(new_out.path().join(CHECKPOINT_FILENAME).is_file());
    }

    // ─── #2114 finding 4: DARKMUX_MAX_PAUSE_MS forwarding ────────

    #[test]
    fn build_docker_run_argv_omits_max_pause_ms_by_default() {
        // base_argv_config() leaves max_pause_ms_env at None (the host
        // never had DARKMUX_MAX_PAUSE_MS set) — the flag must be omitted
        // entirely, not forwarded as some literal "None"/0/empty value.
        let argv = build_docker_run_argv(&base_argv_config());
        assert!(
            !argv.iter().any(|a| a.starts_with("DARKMUX_MAX_PAUSE_MS=")),
            "expected no DARKMUX_MAX_PAUSE_MS in argv when unset: {argv:?}"
        );
    }

    #[test]
    fn build_docker_run_argv_forwards_max_pause_ms_when_set() {
        let mut config = base_argv_config();
        config.max_pause_ms_env = Some(120_000);
        let argv = build_docker_run_argv(&config);
        assert!(
            argv.windows(2).any(|w| w[0] == "-e" && w[1] == "DARKMUX_MAX_PAUSE_MS=120000"),
            "expected -e DARKMUX_MAX_PAUSE_MS=120000 in argv: {argv:?}"
        );
    }

    // ─── N6 (final #2110/#2109 re-check): stale pace.json cleanup ───

    #[test]
    fn clear_stale_pace_file_removes_a_leftover_pace_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            crate::thermal_governor::pace_file_path(dir.path()),
            r#"{"pause": true, "reason": "thermal-critical"}"#,
        )
        .unwrap();
        assert!(crate::thermal_governor::pace_file_path(dir.path()).exists());

        clear_stale_pace_file(dir.path());

        assert!(
            !crate::thermal_governor::pace_file_path(dir.path()).exists(),
            "a leftover pace.json from a prior dispatch must not survive into a new one's first tick"
        );
    }

    #[test]
    fn clear_stale_pace_file_is_a_no_op_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        clear_stale_pace_file(dir.path()); // must not panic — the common case
        assert!(!crate::thermal_governor::pace_file_path(dir.path()).exists());
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
            role_id: "test-role".to_string(),
            model: "default-model".to_string(),
            system_prompt: "Basic role.".to_string(),
            message: "Hello world".to_string(),
            json: false,
            allowed_tools: None,
            compaction: crate::dispatch::CompactionDispatchArgs::default(),
            feedback_templates: serde_json::Value::Null,
            cache_dir: PathBuf::from("/tmp/cache"),
            feedback_injection: true,
            turn_delay_ms: 0,
            inactivity_timeout_seconds: 600,
            inactivity_timeout_seconds_source: darkmux_types::config_access::Source::BuiltIn,
            max_pause_ms_env: None,
            remote_chat_url: None,
            remote_needs_auth: false,
            base_url_override: None,
            workspace_read_only: false,
            resume_checkpoint: false,
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

    // (#632 regression) A `default()` compaction — what bare `dispatch`
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
    #[serial] // (#1882) reaches emit() -> darkmux_flow::record(); contaminates a #[serial] counting test's DARKMUX_FLOWS_DIR tempdir if run concurrently with it
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
    #[serial] // (#1882) reaches emit() -> darkmux_flow::record(); contaminates a #[serial] counting test's DARKMUX_FLOWS_DIR tempdir if run concurrently with it
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
    #[serial] // (#1882) reaches emit() -> darkmux_flow::record(); contaminates a #[serial] counting test's DARKMUX_FLOWS_DIR tempdir if run concurrently with it
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
    #[serial] // (#1882) reaches emit() -> darkmux_flow::record(); contaminates a #[serial] counting test's DARKMUX_FLOWS_DIR tempdir if run concurrently with it
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
    #[serial] // (#1882) reaches emit() -> darkmux_flow::record(); contaminates a #[serial] counting test's DARKMUX_FLOWS_DIR tempdir if run concurrently with it
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
    #[serial] // (#1882) reaches emit() -> darkmux_flow::record(); contaminates a #[serial] counting test's DARKMUX_FLOWS_DIR tempdir if run concurrently with it
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
    #[serial] // (#1882) reaches emit() -> darkmux_flow::record(); contaminates a #[serial] counting test's DARKMUX_FLOWS_DIR tempdir if run concurrently with it
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

    // ─── #2094 finding 2: a rest is host-side proof-of-work too ─────────

    /// A `runtime.rest` trajectory event must reset the host-side
    /// `inactivity_deadline` exactly like `tool.completed`/`compaction` do
    /// — the runtime deliberately sleeps between turns (GPU thermal/power
    /// relief), and without this the host-side hard-kill watchdog would
    /// tick the deadline down toward a kill during time the dispatch was
    /// idle BY DESIGN, not stalled.
    #[test]
    #[serial] // (#1882) reaches emit() -> darkmux_flow::record()
    fn tailer_runtime_rest_event_resets_inactivity_deadline() {
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
        writeln!(f, r#"{{"type":"runtime.rest","seq":2,"ts":1234567890,"ms":500}}"#).unwrap();
        drop(f);

        let before_reset = Instant::now();
        state.poll_and_emit();

        let new_deadline = *shared.lock().unwrap();
        let expected_min =
            before_reset + Duration::from_secs(inactivity_secs) - Duration::from_millis(50);
        assert!(
            new_deadline >= expected_min,
            "a runtime.rest event must reset the inactivity deadline — a \
             deliberate inter-turn rest is not a stalled dispatch"
        );
        assert!(
            new_deadline > original_deadline,
            "reset must overwrite the prior stale deadline"
        );
    }

    /// Each `runtime.rest` trajectory event must emit exactly one
    /// `dispatch.rest` flow record, carrying this rest's own `ms` + `turn`
    /// alongside the RUNNING cumulative `rest_ms`/`rests` totals — so a
    /// rested run is visible on the live flow stream turn-by-turn, not
    /// only summarized at `dispatch.complete`.
    #[test]
    #[serial] // (#1882) reaches emit() -> darkmux_flow::record(); DARKMUX_FLOWS_DIR tempdir
    fn handle_event_runtime_rest_emits_dispatch_rest_with_cumulative_totals() {
        let tmp = TempDir::new().unwrap();
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        let traj_path = tmp.path().join("trajectory.jsonl");
        let mut state = TailerState::new_for_test(
            traj_path,
            "sess-rest".into(),
            "coder".into(),
            "darkmux:qwen3.6".into(),
        );
        state.handle_event(r#"{"type":"runtime.rest","seq":1,"ts":1,"ms":500}"#);
        state.handle_event(r#"{"type":"runtime.rest","seq":2,"ts":2,"ms":500}"#);

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
            .filter(|v| v["session_id"] == "sess-rest" && v["action"] == "dispatch.rest")
            .collect();

        assert_eq!(records.len(), 2, "one dispatch.rest record per runtime.rest event");

        assert_eq!(records[0]["payload"]["ms"], 500);
        assert_eq!(records[0]["payload"]["turn"], 1);
        assert_eq!(records[0]["payload"]["rest_ms"], 500, "first rest: running total = 500");
        assert_eq!(records[0]["payload"]["rests"], 1);

        assert_eq!(records[1]["payload"]["ms"], 500);
        assert_eq!(records[1]["payload"]["turn"], 2);
        assert_eq!(records[1]["payload"]["rest_ms"], 1000, "second rest: running total = 1000");
        assert_eq!(records[1]["payload"]["rests"], 2);
    }

    /// (2026-08-30 fleet-observability finding) A manual pace pause (a
    /// paced `runtime.rest` event carrying `reason`/`state`) and a plain
    /// turn-delay rest were indistinguishable on the flow stream except by
    /// cadence. This pins that `handle_event` forwards `reason` and `state`
    /// verbatim onto `dispatch.rest`'s payload, and that ONLY the paced
    /// event accumulates into `paced_rest_ms` (a plain rest, or a legacy
    /// event with no `reason` at all, does not).
    #[test]
    #[serial] // (#1882) reaches emit() -> darkmux_flow::record(); DARKMUX_FLOWS_DIR tempdir
    fn handle_event_runtime_rest_forwards_reason_and_state_and_tracks_paced_rest_ms() {
        let tmp = TempDir::new().unwrap();
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        let traj_path = tmp.path().join("trajectory.jsonl");
        let mut state = TailerState::new_for_test(
            traj_path,
            "sess-paced-rest".into(),
            "coder".into(),
            "darkmux:qwen3.6".into(),
        );
        // 1: a legacy plain rest with no `reason` field at all (an older
        // runtime image) -> defaults to "turn_delay", not paced.
        state.handle_event(r#"{"type":"runtime.rest","seq":1,"ts":1,"ms":500}"#);
        // 2: an explicit plain turn-delay rest.
        state.handle_event(r#"{"type":"runtime.rest","seq":2,"ts":2,"ms":500,"reason":"turn_delay"}"#);
        // 3: a paced rest with a governor-supplied reason + state.
        state.handle_event(
            r#"{"type":"runtime.rest","seq":3,"ts":3,"ms":2000,"reason":"thermal","state":"critical"}"#,
        );
        // 4: a paced rest with no state (a hand-written pace.json).
        state.handle_event(r#"{"type":"runtime.rest","seq":4,"ts":4,"ms":2000,"reason":"paused"}"#);

        assert_eq!(
            state.summary.paced_rest_ms, 4000,
            "only events 3 and 4 (reason != turn_delay) count as paced"
        );

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
            .filter(|v| v["session_id"] == "sess-paced-rest" && v["action"] == "dispatch.rest")
            .collect();
        assert_eq!(records.len(), 4);

        assert_eq!(records[0]["payload"]["reason"], "turn_delay", "legacy no-reason event defaults");
        assert!(records[0]["payload"].get("state").is_none(), "no state key when the event carries none");

        assert_eq!(records[1]["payload"]["reason"], "turn_delay");

        assert_eq!(records[2]["payload"]["reason"], "thermal");
        assert_eq!(records[2]["payload"]["state"], "critical");

        assert_eq!(records[3]["payload"]["reason"], "paused");
        assert!(records[3]["payload"].get("state").is_none(), "no state key on the event -> no state key on the payload");
    }

    // ─── #1959 (revised) record_context rides every tailer record ─────

    /// `DispatchOpts::record_context` (threaded to `TailerState` via
    /// `with_record_context`) merges under `payload.context` on EVERY
    /// record the tailer emits for this dispatch — proven here against a
    /// `dispatch.tool` record for a `report_finding` call, but the merge
    /// itself (`merge_record_context`) is called from `emit`/`emit_telemetry`
    /// unconditionally, not gated on tool name.
    #[test]
    #[serial] // (#1882) reaches emit() -> darkmux_flow::record(); DARKMUX_FLOWS_DIR tempdir
    fn record_context_merges_under_payload_context_on_a_tool_completed_record() {
        let tmp = TempDir::new().unwrap();
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
            "crawler".into(),
            "darkmux:qwen3.6".into(),
        )
        .with_record_context(Some(serde_json::json!({
            "workspace": "acme",
            "source": "acme-core",
            "sha": "abc123",
            "rule": ["swallowed-error"],
            "unit": "u-0001",
        })));
        state.handle_event(
            r#"{"type":"tool.completed","seq":1,"tool_seq":0,"tool_name":"report_finding","args":"{\"file\":\"a.rs\"}","result":"Recorded. 1 finding(s) so far, 39 remaining in this run's budget.","ok":true}"#,
        );

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
        let record = contents
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["session_id"] == "sess-context" && v["action"] == "dispatch.tool")
            .expect("a dispatch.tool record for the report_finding call");

        assert_eq!(record["payload"]["context"]["workspace"], "acme");
        assert_eq!(record["payload"]["context"]["source"], "acme-core");
        assert_eq!(record["payload"]["context"]["sha"], "abc123");
        assert_eq!(record["payload"]["context"]["unit"], "u-0001");
        // The record's OWN fields survive alongside the merged context —
        // the merge adds a key, it never replaces the payload.
        assert_eq!(record["payload"]["tool_name"], "report_finding");
    }

    /// The other half of the same contract: `record_context: None` (every
    /// caller that doesn't set `DispatchOpts::record_context`) must leave
    /// NO `context` key at all — not `null`, not an empty object. A caller
    /// that never opted in must see byte-identical payloads to before this
    /// feature existed.
    #[test]
    #[serial] // (#1882) reaches emit() -> darkmux_flow::record(); DARKMUX_FLOWS_DIR tempdir
    fn no_record_context_means_no_context_key_at_all() {
        let tmp = TempDir::new().unwrap();
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        let traj_path = tmp.path().join("trajectory.jsonl");
        // No `.with_record_context(...)` — the default `None`.
        let mut state = TailerState::new_for_test(
            traj_path,
            "sess-no-context".into(),
            "crawler".into(),
            "darkmux:qwen3.6".into(),
        );
        state.handle_event(
            r#"{"type":"tool.completed","seq":1,"tool_seq":0,"tool_name":"read","args":"{}","result":"ok","ok":true}"#,
        );

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
        let record = contents
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["session_id"] == "sess-no-context" && v["action"] == "dispatch.tool")
            .expect("a dispatch.tool record");

        assert!(
            record["payload"].as_object().unwrap().get("context").is_none(),
            "no record_context set — payload must carry no `context` key at all: {:?}",
            record["payload"]
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

    // ─── #2165 bound provenance forwarded onto the flow stream ────────

    /// MUST FIX 1 (merge-gate review): the miss #2165 exists to close
    /// happened on the flow stream / envelope, not `trajectory.jsonl` — a
    /// remote reader over the tailnet reads flow records. This pins that
    /// `detector_telemetry_payload` forwards the runtime's `bound` field
    /// VERBATIM for `dispatch.per_turn_cap.salvaged`, and that `detail`
    /// names the kind + provenance in words instead of a bare number.
    #[test]
    fn detector_telemetry_payload_forwards_bound_for_per_turn_cap_salvaged() {
        let event = serde_json::json!({
            "type": "dispatch.per_turn_cap.salvaged",
            "seq": 3,
            "completion_tokens": 999,
            "cap": 1000,
            "salvaged_tool_calls": 2,
            "bound": {"kind": "reasoning_checkpoint_interval", "value": 1000, "source": "built-in"},
        });
        let payload = detector_telemetry_payload("dispatch.per_turn_cap.salvaged", &event)
            .expect("maps per-turn-cap");
        assert_eq!(
            payload["bound"],
            serde_json::json!({"kind": "reasoning_checkpoint_interval", "value": 1000, "source": "built-in"}),
            "the runtime's bound must forward verbatim onto the flow payload"
        );
        let detail = payload["detail"].as_str().unwrap();
        assert!(
            detail.contains("the reasoning check-in interval") && detail.contains("built-in 1000"),
            "detail must name the KIND and provenance in words, not just a bare number; got {detail:?}"
        );
    }

    // ─── #2169 malformed structured tool-call names ────────────────────

    /// `dispatch.tool.malformed_names` → `{kind:"malformed_tool_names",
    /// severity:"warn", detail:<non-empty>, count, model,
    /// sample_name_prefix}` — the issue's own spec names `count`/`model`/
    /// `sample_name_prefix` as explicit payload fields (not just words
    /// inside `detail`), so a consumer aggregating or filtering by model
    /// doesn't have to parse the sentence.
    #[test]
    fn detector_telemetry_payload_maps_malformed_tool_names_event() {
        let event = serde_json::json!({
            "type": "dispatch.tool.malformed_names",
            "seq": 4,
            "count": 5,
            "model": "mistralai/devstral-small-2-2512",
            "sample_name_prefix": "} catch (error) { --- [TOOL_CALLS]",
        });
        let payload = detector_telemetry_payload("dispatch.tool.malformed_names", &event)
            .expect("maps malformed_tool_names");
        assert_eq!(payload["kind"], "malformed_tool_names");
        assert_eq!(payload["severity"], "warn");
        assert_eq!(payload["count"], 5);
        assert_eq!(payload["model"], "mistralai/devstral-small-2-2512");
        assert_eq!(payload["sample_name_prefix"], "} catch (error) { --- [TOOL_CALLS]");
        let detail = payload["detail"].as_str().expect("detail is a string");
        assert!(
            detail.contains('5') && detail.contains("mistralai/devstral-small-2-2512"),
            "detail must weave in count + model; got {detail:?}"
        );
        // (merge-gate MUST FIX 1) No `reason` field on the event — an
        // older runtime image predating the split — must default to
        // "not_a_tool" (the pre-merge-gate meaning of this whole field),
        // not silently drop or crash.
        assert_eq!(payload["reason"], "not_a_tool");
        assert!(
            !detail.contains("not granted"),
            "the not-a-tool wording must never say 'not granted' — that's the OTHER reason's wording"
        );
    }

    /// (merge-gate MUST FIX 1) Sibling of the test above for the OTHER
    /// reason — a real darkmux tool this dispatch's role wasn't granted.
    /// The wording must be DIFFERENT: telling a model "that looks like
    /// quoted code" when it correctly named `bash` is false, and would
    /// not correct the actual mistake.
    #[test]
    fn detector_telemetry_payload_maps_real_tool_not_granted_reason() {
        let event = serde_json::json!({
            "type": "dispatch.tool.malformed_names",
            "seq": 4,
            "count": 1,
            "model": "mistralai/devstral-small-2-2512",
            "sample_name_prefix": "bash",
            "reason": "real_tool_not_granted",
        });
        let payload = detector_telemetry_payload("dispatch.tool.malformed_names", &event)
            .expect("maps malformed_tool_names");
        assert_eq!(payload["kind"], "malformed_tool_names", "same detector kind, different reason");
        assert_eq!(payload["reason"], "real_tool_not_granted");
        let detail = payload["detail"].as_str().unwrap();
        assert!(
            detail.contains("not granted") && detail.contains("bash"),
            "detail must name the offending tool and say it's not granted; got {detail:?}"
        );
        assert!(
            !detail.contains("[TOOL_CALLS]") && !detail.contains("quoted"),
            "must NOT use the not-a-tool wording — bash is a real tool; got {detail:?}"
        );
    }

    /// The `tool.completed`/`dispatch.tool.malformed_names` event pair
    /// accumulates into TWO SEPARATE `TrajectorySummary` counters — a
    /// dispatched-and-failed call increments `tool_calls_failed`; a
    /// coalesced malformed-names event's `count` increments
    /// `tool_calls_invalid_name` — and a malformed-name call never
    /// touches `tool_calls`/`tool_calls_failed` at all (it never
    /// produces its own `tool.completed` event, by construction — the
    /// runtime never dispatches it).
    #[test]
    #[serial] // reaches emit_telemetry() -> darkmux_flow::record() via poll_and_emit()
    fn handle_event_malformed_tool_names_and_tool_failure_accumulate_separately() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trajectory.jsonl");
        let mut state = fixture_state(path.clone());

        let lines = "\
            {\"type\":\"tool.completed\",\"tool_seq\":1,\"tool_name\":\"read\",\"ok\":true}\n\
            {\"type\":\"tool.completed\",\"tool_seq\":2,\"tool_name\":\"bash\",\"ok\":false}\n\
            {\"type\":\"dispatch.tool.malformed_names\",\"seq\":1,\"count\":5,\"model\":\"devstral\",\"sample_name_prefix\":\"bogus\"}\n";
        std::fs::write(&path, lines).unwrap();
        state.poll_and_emit();

        assert_eq!(state.summary.tool_calls, 2, "total_tools counts only DISPATCHED calls");
        assert_eq!(
            state.summary.tool_calls_failed, 1,
            "exactly the one dispatched-and-failed call, never the 5 malformed ones"
        );
        assert_eq!(
            state.summary.tool_calls_invalid_name, 5,
            "the coalesced event's count lands here, not in tool_calls/tool_calls_failed"
        );
        // Also lands in the envelope's `detections` array via the shared
        // producer (#1955) — same payload the flow record carries.
        assert!(
            state.summary.detections.iter().any(|d| d["kind"] == "malformed_tool_names"),
            "must also reach the envelope's detections array"
        );
    }

    /// (merge-gate MUST FIX 1) A dispatch that fires BOTH reasons across
    /// its trajectory — one not-a-tool event, one real-tool-not-granted
    /// event — must accumulate into TWO SEPARATE `TrajectorySummary`
    /// counters, never one merged bucket. Also pins the lenient-on-read
    /// fallback: a THIRD event with no `reason` field at all (an older
    /// runtime image) must land in `tool_calls_invalid_name`, the
    /// pre-merge-gate meaning of that field.
    #[test]
    #[serial]
    fn handle_event_ungranted_and_not_a_tool_reasons_accumulate_separately() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trajectory.jsonl");
        let mut state = fixture_state(path.clone());

        let lines = "\
            {\"type\":\"dispatch.tool.malformed_names\",\"seq\":1,\"count\":3,\"model\":\"devstral\",\"sample_name_prefix\":\"bogus\",\"reason\":\"not_a_tool\"}\n\
            {\"type\":\"dispatch.tool.malformed_names\",\"seq\":2,\"count\":2,\"model\":\"devstral\",\"sample_name_prefix\":\"bash\",\"reason\":\"real_tool_not_granted\"}\n\
            {\"type\":\"dispatch.tool.malformed_names\",\"seq\":3,\"count\":1,\"model\":\"devstral\",\"sample_name_prefix\":\"legacy\"}\n";
        std::fs::write(&path, lines).unwrap();
        state.poll_and_emit();

        assert_eq!(
            state.summary.tool_calls_invalid_name, 4,
            "the not_a_tool event's 3 plus the reason-less (legacy) event's 1 = 4"
        );
        assert_eq!(
            state.summary.tool_calls_ungranted, 2,
            "the real_tool_not_granted event's 2, kept in its OWN bucket"
        );
        let kinds_and_reasons: Vec<(&str, &str)> = state
            .summary
            .detections
            .iter()
            .map(|d| (d["kind"].as_str().unwrap(), d["reason"].as_str().unwrap()))
            .collect();
        assert_eq!(
            kinds_and_reasons,
            vec![
                ("malformed_tool_names", "not_a_tool"),
                ("malformed_tool_names", "real_tool_not_granted"),
                ("malformed_tool_names", "not_a_tool"),
            ],
            "detections array must carry each firing's own reason"
        );
    }

    /// Sibling of the test above for `max_tokens_per_call` — proves the
    /// wording actually discriminates between the two kinds (the exact
    /// #2165 miss: "hit cap 1000" could have been either).
    #[test]
    fn detector_telemetry_payload_forwards_bound_for_per_turn_cap_salvaged_max_tokens_per_call() {
        let event = serde_json::json!({
            "type": "dispatch.per_turn_cap.salvaged",
            "seq": 5,
            "completion_tokens": 3999,
            "cap": 4000,
            "salvaged_tool_calls": 1,
            "bound": {"kind": "max_tokens_per_call", "value": 4000, "source": "config"},
        });
        let payload = detector_telemetry_payload("dispatch.per_turn_cap.salvaged", &event)
            .expect("maps per-turn-cap");
        let detail = payload["detail"].as_str().unwrap();
        assert!(
            detail.contains("the per-call token cap") && detail.contains("config 4000"),
            "detail must name max_tokens_per_call's kind + config provenance; got {detail:?}"
        );
    }

    /// Same forwarding for `dispatch.intra_turn_stall.recovered`.
    #[test]
    fn detector_telemetry_payload_forwards_bound_for_intra_turn_stall_recovered() {
        let event = serde_json::json!({
            "type": "dispatch.intra_turn_stall.recovered",
            "seq": 6,
            "completion_tokens": 500,
            "recoveries_used": 1,
            "recoveries_budget": 2,
            "bound": {"kind": "max_tokens_per_call", "value": 10000, "source": "env"},
        });
        let payload = detector_telemetry_payload("dispatch.intra_turn_stall.recovered", &event)
            .expect("maps intra-turn-stall");
        assert_eq!(
            payload["bound"],
            serde_json::json!({"kind": "max_tokens_per_call", "value": 10000, "source": "env"}),
        );
        let detail = payload["detail"].as_str().unwrap();
        assert!(
            detail.contains("the per-call token cap") && detail.contains("env 10000"),
            "got {detail:?}"
        );
    }

    /// Backward compat: an older runtime image (pre-#2165) never stamps
    /// `bound` on its trajectory events. `detector_telemetry_payload` must
    /// degrade to the pre-#2165 generic wording rather than panicking or
    /// producing a broken sentence — flow records are lenient-on-read.
    #[test]
    fn detector_telemetry_payload_falls_back_to_legacy_wording_when_bound_absent() {
        let event = serde_json::json!({
            "type": "dispatch.per_turn_cap.salvaged",
            "seq": 3,
            "completion_tokens": 999,
            "cap": 1000,
            "salvaged_tool_calls": 2,
        });
        let payload = detector_telemetry_payload("dispatch.per_turn_cap.salvaged", &event)
            .expect("maps per-turn-cap even without a bound field");
        assert!(payload.get("bound").is_none(), "no bound field on the event -> none on the payload");
        let detail = payload["detail"].as_str().unwrap();
        // (#2190) Reworded from "salvaged at the per-call cap" to the
        // non-causal "salvaged; request bound was the per-call cap" — "at"
        // reads as "cut BY that bound", which this record never claims.
        assert!(
            detail.contains("salvaged; request bound was the per-call cap"),
            "must degrade to the (#2190-reworded) generic wording, got {detail:?}"
        );
    }

    /// (#2190) `bound_label` must recognize EVERY `BoundKind` the runtime
    /// side (`runtime/src/bounds.rs`) can emit — this is the exact bug the
    /// issue's live evidence hit: `generation_checkpoint_interval` fell
    /// through to the `other` arm and rendered "an unrecognized bound
    /// (generation_checkpoint_interval))" on a dispatch's THIRD escalating
    /// stall, reading as a broken formatter rather than a named,
    /// well-understood bound (built-in 4000, and nowhere near the 286-648
    /// completion tokens the stalled turns actually emitted). Enumerated by
    /// hand against `runtime/src/bounds.rs::BoundKind` — keep this list in
    /// sync when a variant is added there.
    #[test]
    fn bound_label_names_every_bound_kind() {
        let cases = [
            ("reasoning_checkpoint_interval", "the reasoning check-in interval"),
            ("generation_checkpoint_interval", "the generation check-in interval"),
            ("max_tokens_per_call", "the per-call token cap"),
            ("max_turns", "the max-turns cap"),
            ("max_tokens", "the cumulative max-tokens cap"),
            ("inactivity_timeout", "the inactivity timeout"),
        ];
        for (kind, expected) in cases {
            assert_eq!(
                bound_label(kind),
                expected,
                "bound_label(\"{kind}\") must resolve to a named clause, not fall to \
                 the `other` (\"an unrecognized bound (...)\") arm"
            );
        }
    }

    /// (#2190) Sibling of `detector_telemetry_payload_forwards_bound_for_
    /// intra_turn_stall_recovered` above, for the specific bound the live
    /// evidence hit: `generation_checkpoint_interval`. Pre-fix this rendered
    /// "an unrecognized bound (generation_checkpoint_interval)" in the
    /// detail string; post-fix it names the real clause.
    #[test]
    fn detector_telemetry_payload_names_generation_checkpoint_interval_bound() {
        let event = serde_json::json!({
            "type": "dispatch.intra_turn_stall.recovered",
            "seq": 7,
            "completion_tokens": 648,
            "recoveries_used": 2,
            "recoveries_budget": 2,
            "bound": {"kind": "generation_checkpoint_interval", "value": 4000, "source": "built-in"},
        });
        let payload = detector_telemetry_payload("dispatch.intra_turn_stall.recovered", &event)
            .expect("maps intra-turn-stall");
        let detail = payload["detail"].as_str().unwrap();
        assert!(
            detail.contains("the generation check-in interval (built-in 4000)"),
            "must name the generation check-in interval, not \"an unrecognized bound\"; got {detail:?}"
        );
        assert!(
            !detail.contains("unrecognized"),
            "must never render as an unrecognized bound; got {detail:?}"
        );
        // (#2190) Non-causal wording: "; request bound was X", never "at X"
        // (which reads as "cut BY that bound" — false for this record, which
        // only fires for the genuine runaway-reasoning shape, but the
        // wording itself must not imply causation either way).
        assert!(
            detail.contains("dropped + recovered; request bound was"),
            "must use the non-causal '; request bound was' phrasing, got {detail:?}"
        );
    }

    /// (#2190) The NEW `dispatch.empty_tool_calls.recovered` event type must
    /// route to its own `kind: "empty_tool_calls"` and name the real shape
    /// in `detail` — not the runaway-reasoning wording its sibling event
    /// carries.
    #[test]
    fn detector_telemetry_payload_maps_empty_tool_calls_recovered_event() {
        let event = serde_json::json!({
            "type": "dispatch.empty_tool_calls.recovered",
            "seq": 2,
            "completion_tokens": 286,
            "recoveries_used": 2,
            "recoveries_budget": 2,
            "bound": {"kind": "generation_checkpoint_interval", "value": 4000, "source": "built-in"},
        });
        let payload = detector_telemetry_payload("dispatch.empty_tool_calls.recovered", &event)
            .expect("maps empty_tool_calls");
        assert_eq!(payload["kind"], serde_json::json!("empty_tool_calls"));
        let detail = payload["detail"].as_str().unwrap();
        assert!(
            detail.contains("finish_reason=tool_calls with no tool calls"),
            "detail must name the real shape, got {detail:?}"
        );
        assert!(
            !detail.contains("runaway-reasoning"),
            "must NOT use the runaway-reasoning wording for this shape, got {detail:?}"
        );
    }

    /// (#2190) The escalation record's own detector-telemetry mapping:
    /// `dispatch.escalation.triggered` must route to `kind: "escalation"`
    /// and carry `model`/`prompt_tokens` as EXPLICIT payload fields (not
    /// only inside the human-readable `detail` string) — same pattern as
    /// the malformed-tool-names event's explicit `count`/`model`/
    /// `sample_name_prefix` fields, so a consumer never has to parse prose
    /// to get the fact.
    #[test]
    fn detector_telemetry_payload_maps_escalation_triggered_event() {
        let event = serde_json::json!({
            "type": "dispatch.escalation.triggered",
            "seq": 4,
            "reason": "escalation_empty_tool_calls",
            "model": "devstral-small-2-2512",
            "prompt_tokens": 19133,
        });
        let payload = detector_telemetry_payload("dispatch.escalation.triggered", &event)
            .expect("maps escalation");
        assert_eq!(payload["kind"], serde_json::json!("escalation"));
        assert_eq!(payload["model"], serde_json::json!("devstral-small-2-2512"));
        assert_eq!(payload["prompt_tokens"], serde_json::json!(19133));
        assert_eq!(payload["reason"], serde_json::json!("escalation_empty_tool_calls"));
        let detail = payload["detail"].as_str().unwrap();
        assert!(
            detail.contains("devstral-small-2-2512") && detail.contains("19133"),
            "detail must also name model + prompt_tokens in prose, got {detail:?}"
        );
    }

    /// MUST FIX 1's other half: the SAME payload `detector_telemetry_payload`
    /// builds also feeds `self.summary.detections` (pushed at the
    /// `handle_event` call site), which the finished envelope's
    /// `detections` array carries verbatim (`enrich_envelope_with_summary`).
    /// Proves `bound` survives that hop too, not just the live flow record.
    #[test]
    #[serial] // (#1882) reaches emit() -> darkmux_flow::record(); DARKMUX_FLOWS_DIR tempdir
    fn handle_event_per_turn_cap_salvaged_carries_bound_into_the_detections_summary() {
        let tmp = TempDir::new().unwrap();
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        let traj_path = tmp.path().join("trajectory.jsonl");
        let mut state = TailerState::new_for_test(
            traj_path,
            "sess-bound-detections".into(),
            "coder".into(),
            "darkmux:qwen3.6".into(),
        );
        state.handle_event(
            r#"{"type":"dispatch.per_turn_cap.salvaged","seq":1,"ts":1,"completion_tokens":999,"cap":1000,"salvaged_tool_calls":1,"bound":{"kind":"reasoning_checkpoint_interval","value":1000,"source":"built-in"}}"#,
        );

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

        assert_eq!(state.summary.detections.len(), 1, "one detection recorded");
        assert_eq!(
            state.summary.detections[0]["bound"],
            serde_json::json!({"kind": "reasoning_checkpoint_interval", "value": 1000, "source": "built-in"}),
            "bound must survive into the envelope-bound detections summary, not just the live flow record"
        );
    }

    /// MUST FIX 1's third projection: `dispatch.checkpoint`'s own payload
    /// (built inline, NOT through `detector_telemetry_payload`) must ALSO
    /// forward `bound` — checkpoints are not detector firings, so they need
    /// their own assertion.
    #[test]
    #[serial] // (#1882) reaches emit() -> darkmux_flow::record(); DARKMUX_FLOWS_DIR tempdir
    fn handle_event_dispatch_checkpoint_forwards_bound() {
        let tmp = TempDir::new().unwrap();
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        let traj_path = tmp.path().join("trajectory.jsonl");
        let mut state = TailerState::new_for_test(
            traj_path,
            "sess-checkpoint-bound".into(),
            "coder".into(),
            "darkmux:qwen3.6".into(),
        );
        state.handle_event(
            r#"{"type":"dispatch.checkpoint","seq":1,"ts":1,"checkpoint":1,"slice_tokens":900,"tail_ratio":0.1,"verdict":"continue","bound":{"kind":"reasoning_checkpoint_interval","value":1000,"source":"config"}}"#,
        );

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
        let record = contents
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["session_id"] == "sess-checkpoint-bound" && v["action"] == "dispatch.checkpoint")
            .expect("dispatch.checkpoint record must exist");
        assert_eq!(
            record["payload"]["bound"],
            serde_json::json!({"kind": "reasoning_checkpoint_interval", "value": 1000, "source": "config"}),
            "dispatch.checkpoint's payload must forward the runtime's bound verbatim"
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
    /// (#1544) Every record in `dir` that belongs to `session` — the shape
    /// an assertion should almost always use.
    ///
    /// `DARKMUX_FLOWS_DIR` is process-global, and while a `#[serial]` test
    /// holds it pointed at its own tempdir, EVERY concurrently-running
    /// non-serial test that emits a flow record writes into that tempdir
    /// too. (Verified: 31 of the 166 tests in this file emit without setting
    /// the var or taking the serial lock.) `#[serial]` cannot prevent this —
    /// it only excludes other serial tests, and the polluters are precisely
    /// the ones that never opted in.
    ///
    /// So an unscoped `drain_flow_records` + an exact-count assertion is a
    /// race by construction: it passes or fails on test-scheduling luck.
    /// That is #1544, and it has cost six CI runs. Scoping the read to the
    /// test's OWN session makes the assertion immune to anything a sibling
    /// writes, which is the property the test actually wanted all along.
    ///
    /// This is a containment fix, not the cure. The cure is injecting a sink
    /// per dispatch instead of resolving a process-global env var at write
    /// time — a real refactor, tracked on #1544.
    fn drain_flow_records_for_session(
        dir: &std::path::Path,
        session: &str,
    ) -> Vec<serde_json::Value> {
        drain_flow_records(dir)
            .into_iter()
            .filter(|v| v["session_id"] == session)
            .collect()
    }

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
                None,
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

        let rec = drain_flow_records_for_session(tmp.path(), "sess-orphan")
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

        let emitted = drain_flow_records_for_session(tmp.path(), "sess-clean")
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

        let rec = drain_flow_records_for_session(tmp.path(), "sess-panic")
            .into_iter()
            .find(|v| v["action"] == "dispatch error")
            .expect("guard should emit a dispatch.error terminal on panic unwind");
        assert_eq!(rec["session_id"], "sess-panic");
        assert_eq!(rec["mission_id"], "pre-1.0-compat-sweep");
    }

    /// (#1221) A checkpointed thinking turn is ONE turn on every surface.
    ///
    /// The runtime deliberately does not spend a turn on a checkpoint
    /// continuation, so every `model.completed` from one long thought carries
    /// the SAME `seq`. The host re-derived its own count by incrementing on
    /// each event, so a thirteen-checkpoint turn read as thirteen turns on the
    /// seat-card meter, in `dispatch.complete`'s `total_turns`, and in every
    /// downstream consumer — the exact "one long thought reads as a dozen
    /// turns" symptom this feature exists to remove, surviving in the surface
    /// the operator actually reads. The runtime's own envelope had the right
    /// number the whole time; only this re-derivation was wrong.
    #[test]
    #[serial]
    fn checkpoint_continuations_count_as_one_turn() {
        let tmp = TempDir::new().unwrap();
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        let traj_path = tmp.path().join("trajectory.jsonl");
        let mut state = TailerState::new_for_test(
            traj_path,
            "sess-ckpt".into(),
            "pr-reviewer".into(),
            "darkmux:qwen3.8".into(),
        );

        // One thinking turn, four API calls: seq stays 1 across the
        // continuations. Then a genuinely new turn at seq 2.
        for _ in 0..4 {
            state.handle_event(
                r#"{"type":"model.completed","seq":1,"finish_reason":"length","tool_calls":null,"usage":{"prompt_tokens":100,"completion_tokens":999}}"#,
            );
        }
        state.handle_event(
            r#"{"type":"model.completed","seq":2,"finish_reason":"stop","tool_calls":null,"usage":{"prompt_tokens":200,"completion_tokens":50}}"#,
        );

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
            if let Some(v) = prev_redis {
                std::env::set_var("DARKMUX_REDIS_URL", v);
            }
        }

        assert_eq!(
            state.summary.turns, 2,
            "four checkpoint continuations of one thought plus one real turn is \
             TWO turns, not five — counting API calls inflates every \
             operator-visible turn count for reasoning-heavy dispatches"
        );
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

        let records = drain_flow_records_for_session(tmp.path(), "sess-tokens");
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

    /// (#1483) A multi-turn / multi-tool agent loop stamps EVERY live per-event
    /// flow record (`dispatch.turn`, `dispatch.tool`, `telemetry.tokens`) with
    /// the graph `step_id` — so the mission-graph viewer can attribute the live
    /// turn/tool/token climb to the AGENTIC seat card even when the dispatch's
    /// `session_id` isn't the `step-<id>` default (the coder-phase seat's shared
    /// `mission-run-<…>` session). Each turn/tool record also carries the
    /// AUTHORITATIVE monotonic running count the viewer's meter ticks off.
    #[test]
    #[serial]
    fn handle_event_stamps_step_id_and_running_counts_for_agentic_seat() {
        let tmp = TempDir::new().unwrap();
        // SAFETY: serialized via `#[serial]`; no concurrent env reader.
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        let traj_path = tmp.path().join("trajectory.jsonl");
        // The coder seat's shared mission-run session — NOT `step-<id>`. Without
        // the stamped `step_id`, these records would be unattributable.
        let mut state = TailerState::new_for_test(
            traj_path,
            "mission-run-m1-build-abc".into(),
            "coder".into(),
            "darkmux:qwen3.6".into(),
        )
        .with_step(Some("coder-1".into()));

        // Two turns, three tool calls interleaved — a real agentic loop shape.
        state.handle_event(
            r#"{"type":"model.completed","seq":1,"finish_reason":"tool_calls","usage":{"prompt_tokens":100,"completion_tokens":20}}"#,
        );
        state.handle_event(r#"{"type":"tool.completed","tool_seq":1,"tool_name":"read","ok":true}"#);
        state.handle_event(r#"{"type":"tool.completed","tool_seq":2,"tool_name":"bash","ok":true}"#);
        state.handle_event(
            r#"{"type":"model.completed","seq":2,"finish_reason":"tool_calls","usage":{"prompt_tokens":150,"completion_tokens":30}}"#,
        );
        state.handle_event(r#"{"type":"tool.completed","tool_seq":3,"tool_name":"edit","ok":true}"#);

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

        // dispatch.turn — running count 1,2; step_id stamped on each.
        let turn_recs: Vec<&serde_json::Value> = records
            .iter()
            .filter(|v| v["action"] == "dispatch.turn")
            .collect();
        assert_eq!(turn_recs.len(), 2, "one dispatch.turn per model.completed");
        assert_eq!(turn_recs[0]["payload"]["turns_so_far"], 1);
        assert_eq!(turn_recs[1]["payload"]["turns_so_far"], 2);
        for r in &turn_recs {
            assert_eq!(r["payload"]["step_id"], "coder-1", "turn record must carry step_id");
        }

        // dispatch.tool — running count 1,2,3; step_id stamped on each.
        let tool_recs: Vec<&serde_json::Value> = records
            .iter()
            .filter(|v| v["action"] == "dispatch.tool")
            .collect();
        assert_eq!(tool_recs.len(), 3, "one dispatch.tool per tool.completed");
        assert_eq!(tool_recs[0]["payload"]["tool_calls_so_far"], 1);
        assert_eq!(tool_recs[1]["payload"]["tool_calls_so_far"], 2);
        assert_eq!(tool_recs[2]["payload"]["tool_calls_so_far"], 3);
        for r in &tool_recs {
            assert_eq!(r["payload"]["step_id"], "coder-1", "tool record must carry step_id");
        }

        // telemetry.tokens also attributes to the seat.
        let tok_recs: Vec<&serde_json::Value> = records
            .iter()
            .filter(|v| v["category"] == "telemetry" && v["source"] == "tokens")
            .collect();
        assert_eq!(tok_recs.len(), 2, "one tokens record per usage-bearing turn");
        for r in &tok_recs {
            assert_eq!(r["payload"]["step_id"], "coder-1", "tokens record must carry step_id");
        }
    }

    /// (#1483) A dispatch that is NOT a graph step (`step_id` unset — the
    /// one-off `darkmux dispatch` path) emits the SAME records WITHOUT a
    /// `step_id` payload key, so the field is purely additive and those records
    /// attribute via `session_id` exactly as before the emit half landed.
    #[test]
    #[serial]
    fn handle_event_omits_step_id_when_not_a_graph_step() {
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
            "sess-oneoff".into(),
            "coder".into(),
            "darkmux:qwen3.6".into(),
        );
        state.handle_event(
            r#"{"type":"model.completed","seq":1,"finish_reason":"stop","usage":{"prompt_tokens":100,"completion_tokens":20}}"#,
        );
        state.handle_event(r#"{"type":"tool.completed","tool_seq":1,"tool_name":"read","ok":true}"#);

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

        let records = drain_flow_records_for_session(tmp.path(), "sess-oneoff");
        let turn = records
            .iter()
            .find(|v| v["action"] == "dispatch.turn")
            .expect("a dispatch.turn record");
        assert!(
            turn["payload"].get("step_id").is_none(),
            "no step_id key when the dispatch isn't a graph step; got {turn:?}"
        );
        // The running count still rides even without a step id.
        assert_eq!(turn["payload"]["turns_so_far"], 1);
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
    fn the_verdict_at_the_tail_survives_result_truncation() {
        // (#2007) The whole reason a RESULT elides the MIDDLE rather than the
        // tail: a test run's verdict is its last line. Head-only truncation
        // keeps the setup noise and discards the one line a diagnosis needs.
        let verdict = "Tests: 2 failed, 86 passed, 88 total";
        let body = format!(
            "exit: 1\n--- stdout ---\n{}\n{verdict}\n",
            "PASS tests/services/noise.test.ts\n".repeat(4000)
        );
        assert!(body.len() > 4096, "fixture must actually exceed the cap");

        let out = cap_result_middle(&body, 4096);
        assert!(
            out.contains(verdict),
            "the verdict at the TAIL must survive — that is the point of eliding the middle"
        );
        assert!(out.contains("exit: 1"), "the head must survive too");
        assert!(
            out.contains("middle elided"),
            "the cut must announce itself in-band, never silently"
        );
        assert!(
            out.contains(&format!("{} chars", body.chars().count())),
            "the marker must carry the TRUE original size"
        );

        // The behaviour this replaces — prove it would have lost the verdict,
        // so this test cannot pass for the wrong reason.
        assert!(
            !cap_str(&body, 4096).contains(verdict),
            "guard: if head-only truncation kept the verdict, this test proves nothing"
        );
    }

    #[test]
    fn a_result_under_the_cap_is_returned_untouched() {
        let small = "exit: 0\n--- stdout ---\nok\n";
        assert_eq!(cap_result_middle(small, 4096), small);
        let v = serde_json::Value::String(small.to_string());
        assert_eq!(cap_json_result(Some(&v), 4096), v);
    }

    #[test]
    fn result_elision_never_splits_a_codepoint() {
        // Multibyte throughout, including caps small enough to hit the
        // degenerate path where head and tail would otherwise overlap.
        let s = "héllo wörld ".repeat(500);
        for cap in [32usize, 64, 128, 512, 4096] {
            let out = cap_result_middle(&s, cap);
            let v = serde_json::Value::String(out);
            assert!(
                serde_json::to_string(&v).is_ok(),
                "cap {cap} produced text that will not serialize"
            );
        }
    }

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
    #[serial] // (#1882) reaches emit() -> darkmux_flow::record() via poll_and_emit(); found by a direct sweep beyond the issue's own named seven
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
    #[serial] // (#1882) reaches emit() -> darkmux_flow::record() via poll_and_emit(); found by a direct sweep beyond the issue's own named seven
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
    #[serial] // (#1882) reaches emit() -> darkmux_flow::record() via poll_and_emit(); found by a direct sweep beyond the issue's own named seven
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
    #[serial] // (#1882) reaches emit() -> darkmux_flow::record() via poll_and_emit(); found by a direct sweep beyond the issue's own named seven
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
    #[serial] // (#1882) reaches emit() -> darkmux_flow::record() via poll_and_emit(); found by a direct sweep beyond the issue's own named seven
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

    // (#1280) The #590 `utility_preflight_warning` tests were removed with the
    // warn-only preflight — the utility/compactor model is now ensured resident
    // at its declared context (namespaced) via `ensure_model_loaded_at_ctx`,
    // the same guard the dispatch model gets. That guard's load path is covered
    // by the bounded-`LmsHost` tests in `darkmux-profiles` (#1139/#1276).

    // ─── #1280 utility-residency wiring ────────────────────────────────

    /// The utility model's residency spec is built AT THE COMPACTION CONTEXT
    /// WINDOW — the window is the payload size compaction sends, so the model
    /// must load at least that large (the #1135-class hole this closes).
    #[test]
    fn utility_residency_pm_uses_the_compaction_window_as_n_ctx() {
        let pm = super::utility_residency_pm("util-4b", 68_000);
        assert_eq!(pm.id, "util-4b");
        assert_eq!(pm.n_ctx, Some(68_000));
    }

    /// The residency spec loads under the `darkmux:` namespace — the same
    /// `namespaced_identifier` path `ensure_model_loaded_at_ctx` uses, so
    /// `machine eject` can reclaim the load (the #1280 RAM-leak half).
    #[test]
    fn utility_residency_pm_namespaces_like_the_dispatch_model() {
        let pm = super::utility_residency_pm("util-4b", 68_000);
        assert_eq!(darkmux_profiles::swap::namespaced_identifier(&pm), "darkmux:util-4b");
    }

    /// Warn, never abort: a failed utility load yields a warning naming the
    /// model, the window, and the truncation risk — the dispatch proceeds
    /// (a compaction-less short dispatch still runs).
    #[test]
    fn ensure_utility_resident_warns_and_does_not_abort_on_load_failure() {
        let warning = super::ensure_utility_resident(Some("util-4b"), Some(68_000), |_pm| {
            anyhow::bail!("insufficient RAM")
        })
        .expect("a failed load must produce a warning");
        assert!(warning.contains("util-4b"), "{warning}");
        assert!(warning.contains("68000"), "{warning}");
        assert!(warning.contains("truncate"), "{warning}");
        assert!(warning.contains("WARNING"), "{warning}");
    }

    /// Success and not-configured are both silent (no warning): the load ran
    /// with the window-sized spec on success, and no compactor / no window
    /// means nothing to ensure.
    #[test]
    fn ensure_utility_resident_silent_on_success_or_unconfigured() {
        let loaded: std::sync::Mutex<Vec<(String, Option<u32>)>> = std::sync::Mutex::new(Vec::new());
        let result = super::ensure_utility_resident(Some("util-4b"), Some(68_000), |pm| {
            loaded.lock().unwrap().push((pm.id.clone(), pm.n_ctx));
            Ok(())
        });
        assert!(result.is_none());
        assert_eq!(*loaded.lock().unwrap(), vec![("util-4b".to_string(), Some(68_000))]);

        assert!(super::ensure_utility_resident(None, Some(68_000), |_| Ok(())).is_none());
        assert!(super::ensure_utility_resident(Some("util-4b"), None, |_| Ok(())).is_none());
    }

    // ─── #1139 map_load_result: bounded-load outcome → actionable error ───

    #[test]
    fn map_load_result_ok_is_ok() {
        assert!(super::map_load_result(Ok(Default::default()), "m", 4096).is_ok());
    }

    #[test]
    fn map_load_result_insufficient_resources_is_operator_actionable() {
        use darkmux_gestalt::HostError;
        let err = super::map_load_result(
            Err(HostError::InsufficientResources {
                detail: "out of memory while allocating KV cache".into(),
            }),
            "qwen3-35b",
            65536,
        )
        .unwrap_err()
        .to_string();
        // Names the model, the ctx, the RAM cause, and a concrete next action —
        // never a hang or a silent degrade (#1139).
        assert!(err.contains("insufficient RAM"), "{err}");
        assert!(err.contains("qwen3-35b"), "{err}");
        assert!(err.contains("65536"), "{err}");
        assert!(err.contains("machine eject") || err.contains("lower the profile"), "{err}");
        assert!(err.contains("out of memory while allocating KV cache"), "{err}");
    }

    #[test]
    fn map_load_result_timeout_names_phase_and_the_knob() {
        use darkmux_gestalt::HostError;
        let err = super::map_load_result(
            Err(HostError::Timeout { phase: "load", waited: std::time::Duration::from_secs(600) }),
            "m",
            4096,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("did not finish"), "{err}");
        assert!(err.contains("DARKMUX_MODEL_LOAD_TIMEOUT_SECONDS"), "{err}");
    }

    #[test]
    fn map_load_result_unknown_model_points_at_lms_ls() {
        use darkmux_gestalt::HostError;
        let err = super::map_load_result(
            Err(HostError::UnknownModel { model_key: "ghost".into() }),
            "ghost",
            4096,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("ghost"), "{err}");
        assert!(err.contains("lms ls"), "{err}");
    }

// ── (#1609) Namespace contract: darkmux never unloads user state ────────────

/// The regression itself. A hand-loaded resident serves the same model key as
/// the profile wants, but darkmux does not own it — selecting it as a reload
/// target is what let a dispatch unload the operator's own model mid-run.
#[test]
fn a_foreign_resident_is_never_a_reload_target() {
    assert!(
        !super::is_reloadable_target("qwen3-4b", "qwen3-4b", "qwen3-4b", None),
        "a bare identifier is USER state — matching the model key must not make it ours"
    );
}

/// The other half: the guard must not break the case it exists to serve.
#[test]
fn a_darkmux_owned_resident_of_the_same_model_is_a_reload_target() {
    assert!(super::is_reloadable_target(
        "qwen3-4b",
        "darkmux:qwen3-4b",
        "qwen3-4b",
        None
    ));
}

/// Ownership alone is not enough — a darkmux resident of a DIFFERENT model is
/// still not a target, or a reload would evict an unrelated seat.
#[test]
fn a_darkmux_owned_resident_of_another_model_is_not_a_reload_target() {
    assert!(!super::is_reloadable_target(
        "llama-3-8b",
        "darkmux:llama-3-8b",
        "qwen3-4b",
        None
    ));
}

/// The ForeignDuplicate shape end to end: both copies resident, only ours
/// selectable. This is the exact list `lms ps` returns when an operator has
/// hand-loaded a model darkmux also uses.
#[test]
fn with_both_copies_resident_only_the_darkmux_one_is_selected() {
    let residents = [
        ("qwen3-4b", "qwen3-4b"),           // the operator's, loaded first
        ("qwen3-4b", "darkmux:qwen3-4b"),   // darkmux's own
    ];
    let picked: Vec<&str> = residents
        .iter()
        .filter(|(m, id)| super::is_reloadable_target(m, id, "qwen3-4b", None))
        .map(|(_, id)| *id)
        .collect();
    assert_eq!(
        picked,
        vec!["darkmux:qwen3-4b"],
        "exactly one target, and never the operator's — insertion order must not decide this"
    );
}

// ── (#1615) The namespace is a decoration on the identifier, never the key ───

/// The strip itself, both directions. A bare key must survive untouched — the
/// overwhelmingly common spelling — and a namespaced one must reduce to the key
/// LMStudio actually publishes.
#[test]
fn bare_model_key_strips_only_the_namespace() {
    assert_eq!(super::bare_model_key("qwen3-4b-instruct-2507"), "qwen3-4b-instruct-2507");
    assert_eq!(
        super::bare_model_key("darkmux:qwen3-4b-instruct-2507"),
        "qwen3-4b-instruct-2507"
    );
    // Not a prefix match on anything shorter or adjacent — those are real keys.
    assert_eq!(super::bare_model_key("dark:foo"), "dark:foo");
    assert_eq!(super::bare_model_key("predarkmux:foo"), "predarkmux:foo");
    // Idempotent: stripping an already-bare key is a no-op, so normalizing
    // twice on a path that gains a second call site can never over-strip.
    let once = super::bare_model_key("darkmux:foo");
    assert_eq!(super::bare_model_key(once), once);
}

/// The regression. `internal.utility` accepts a namespaced IDENTIFIER
/// (`darkmux:qwen3-4b-instruct-2507`) where a model KEY belongs, and `lms ps`
/// reports the resident as `modelKey=qwen3-4b-instruct-2507`. Comparing the
/// prefixed string against that key can never match, so darkmux's OWN compactor
/// read as absent on every dispatch — it reloaded each time, and when the load
/// itself was handed the same prefixed string it could not resolve at all.
#[test]
fn a_namespaced_utility_binding_still_finds_darkmuxs_own_resident() {
    let want = super::bare_model_key("darkmux:qwen3-4b-instruct-2507");
    assert!(
        super::is_reloadable_target(
            "qwen3-4b-instruct-2507",
            "darkmux:qwen3-4b-instruct-2507",
            want,
            None
        ),
        "darkmux's own resident must be recognized however the operator spelled the binding"
    );
    // And the #1609 guarantee is undamaged by the normalization: a foreign
    // copy of the same model is still never a target.
    assert!(!super::is_reloadable_target(
        "qwen3-4b-instruct-2507",
        "qwen3-4b-instruct-2507",
        want,
        None
    ));
}

// ── (#1617 review) The documented `identifier` opt-out is still darkmux's ───

/// The regression the first shape of the #1609 guard introduced.
///
/// `ProfileModel.identifier` is a documented namespace opt-out: darkmux loads
/// under the operator's literal string, which does NOT start with `darkmux:`.
/// A prefix-only ownership test therefore rejected darkmux's OWN load — so
/// every dispatch after the first re-announced "loading…", printed the "darkmux
/// does not own and will not unload" notice about its own instance, and loaded
/// again under a name already taken.
#[test]
fn a_profiles_explicit_identifier_is_still_darkmuxs_own_load() {
    assert!(
        super::is_reloadable_target("qwen3-4b", "myid", "qwen3-4b", Some("myid")),
        "a load made under the profile's declared identifier must be recognized as ours"
    );
    // ...and the #1609 guarantee survives it: a hand-loaded BARE copy still
    // cannot be selected, because it cannot carry the declared identifier.
    assert!(
        !super::is_reloadable_target("qwen3-4b", "qwen3-4b", "qwen3-4b", Some("myid")),
        "the operator's own bare copy is never a target, opt-out or not"
    );
    // Nor can some other profile's opt-out name be mistaken for this one's.
    assert!(!super::is_reloadable_target("qwen3-4b", "otherid", "qwen3-4b", Some("myid")));
    // And with no opt-out declared, the namespaced form is what counts —
    // a bare-named resident is user state even though the KEY matches.
    assert!(super::is_reloadable_target("qwen3-4b", "darkmux:qwen3-4b", "qwen3-4b", None));
    assert!(!super::is_reloadable_target("qwen3-4b", "myid", "qwen3-4b", None));
}

/// The identifier darkmux mints must be byte-identical whichever spelling the
/// operator used — otherwise normalizing the key would fork ownership, and
/// `machine eject` would stop recognizing loads made under the other spelling.
#[test]
fn normalizing_the_key_does_not_change_the_minted_identifier() {
    let from_bare =
        darkmux_gestalt::namespaced_identifier(super::bare_model_key("qwen3-4b"), None);
    let from_namespaced =
        darkmux_gestalt::namespaced_identifier(super::bare_model_key("darkmux:qwen3-4b"), None);
    assert_eq!(from_bare, "darkmux:qwen3-4b");
    assert_eq!(from_bare, from_namespaced);
    assert!(darkmux_profiles::swap::is_darkmux_owned(&from_namespaced));
}

// ---------------------------------------------------------------
// (#1955) The host/container seam.
//
// These exist because a real dispatch was run and its output was READ
// rather than EXERCISED. The envelope carried
// `trajectory_path: "/darkmux-out/.darkmux-runtime/trajectory.jsonl"` —
// a path-shaped string that looks entirely correct and does not exist on
// the host. It survived because both sides were individually right: the
// runtime reports its own view and has a passing test asserting exactly
// that container path.
//
// The rule this encodes: a value that means something different on the
// other side of a boundary must be resolved FROM THE CONSUMER'S SIDE
// before it is called verified.
// ---------------------------------------------------------------

#[test]
fn envelope_trajectory_path_is_translated_to_the_host_view() {
    let host_out = std::path::Path::new("/var/folders/xx/T/darkmux-out-analyst-123");
    let runtime_stdout = r#"{"result":"stop","final_assistant":"hi","trajectory_path":"/darkmux-out/.darkmux-runtime/trajectory.jsonl"}"#;

    let out = super::rewrite_container_paths_for_host(runtime_stdout.to_string(), host_out);
    let v: serde_json::Value = serde_json::from_str(&out).expect("still valid JSON");

    assert_eq!(
        v["trajectory_path"],
        "/var/folders/xx/T/darkmux-out-analyst-123/.darkmux-runtime/trajectory.jsonl",
        "the caller must receive a path it can open, not the container's view"
    );
    assert_eq!(v["result"], "stop", "unrelated fields are untouched");
    assert_eq!(v["final_assistant"], "hi");
}

#[test]
fn a_host_shaped_path_is_left_alone() {
    let host_out = std::path::Path::new("/var/folders/xx/T/out");
    let already = r#"{"trajectory_path":"/var/folders/xx/T/out/.darkmux-runtime/trajectory.jsonl"}"#;
    let out = super::rewrite_container_paths_for_host(already.to_string(), host_out);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["trajectory_path"],
        "/var/folders/xx/T/out/.darkmux-runtime/trajectory.jsonl",
        "translation must be idempotent — only the container prefix is rewritten"
    );
}

#[test]
fn non_json_stdout_passes_through_untouched() {
    // The non-`--json` path returns bare completion text. Rewriting must be
    // a no-op there rather than mangling a model's answer that happens to
    // start with a brace.
    let host_out = std::path::Path::new("/var/folders/xx/T/out");
    for raw in [
        "just some model output",
        "",
        "{ not valid json at all",
    ] {
        assert_eq!(
            super::rewrite_container_paths_for_host(raw.to_string(), host_out),
            raw,
            "non-envelope stdout must pass through verbatim: {raw:?}"
        );
    }
}

#[test]
fn an_envelope_without_a_trajectory_path_is_unchanged() {
    // The remote/single-shot envelopes carry no trajectory_path. Absence is
    // not an error and must not become a null or an empty string.
    let host_out = std::path::Path::new("/var/folders/xx/T/out");
    let remote = r#"{"result":"stop","final_assistant":"hi","metrics":{"turns":1}}"#;
    let out = super::rewrite_container_paths_for_host(remote.to_string(), host_out);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(
        v.get("trajectory_path").is_none(),
        "must not invent a field the producer did not emit: {out}"
    );
}

// ---------------------------------------------------------------
// (#1955) The envelope carries what darkmux detected.
//
// Before this, the four detectors wrote to NO orchestrator-reachable
// channel — not stdout, not stderr, only a trajectory file in a temp dir.
// A dispatch that tripped a cycle detector returned an envelope
// byte-indistinguishable from a clean one.
// ---------------------------------------------------------------

fn summary_with(detections: Vec<serde_json::Value>) -> super::TrajectorySummary {
    super::TrajectorySummary {
        detections,
        ..Default::default()
    }
}

#[test]
fn a_detection_reaches_the_envelope() {
    let det = serde_json::json!({
        "kind": "cycle",
        "severity": "warn",
        "detail": "`read` called 3× in the last 5 tool calls",
    });
    let out = super::enrich_envelope_with_summary(
        r#"{"result":"stop"}"#.to_string(),
        &summary_with(vec![det.clone()]),
        &super::HostStats::default(),
        &no_extras(),
        no_findings_dir(),
    serde_json::json!({}),
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["detections"][0], det, "the firing must reach the caller: {out}");
    assert_eq!(v["result"], "stop", "existing fields survive");
}

#[test]
fn a_clean_run_reports_an_empty_array_not_an_absent_field() {
    // Absence is ambiguous between "nothing fired" and "this build does not
    // report detections". `[]` is a positive statement and the caller can
    // act on it.
    let out = super::enrich_envelope_with_summary(
        r#"{"result":"stop"}"#.to_string(),
        &summary_with(vec![]),
        &super::HostStats::default(),
        &no_extras(),
        no_findings_dir(),
    serde_json::json!({}),
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v["detections"].is_array(), "must be present: {out}");
    assert_eq!(v["detections"].as_array().unwrap().len(), 0);
}

/// MUST FIX 3 (merge-gate review of #2165): `obj.insert("bounds", bounds)`
/// was unpinned — every other `enrich_envelope_with_summary` test in this
/// file passes `serde_json::json!({})` as the `bounds` arg, so deleting the
/// insert line failed nothing. Passes a DISTINCTIVE, non-empty value (never
/// the shape `resolved_runtime_bounds_json` would actually produce) so this
/// assertion could only pass if the caller's `bounds` argument genuinely
/// made it into the envelope, not some other field coincidentally matching.
#[test]
fn bounds_argument_survives_into_the_envelope() {
    let distinctive_bounds = serde_json::json!({
        "max_tokens_per_call": {"value": 4000, "source": "config"},
        "__test_marker": "bounds-survival-proof",
    });
    let out = super::enrich_envelope_with_summary(
        r#"{"result":"stop"}"#.to_string(),
        &summary_with(vec![]),
        &super::HostStats::default(),
        &no_extras(),
        no_findings_dir(),
        distinctive_bounds.clone(),
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["bounds"], distinctive_bounds,
        "the bounds argument must reach the envelope verbatim under the \"bounds\" key: {out}"
    );
}

fn summary_over_ratios(ratios: &[f64], concluded: u32) -> super::TrajectorySummary {
    let mut s = super::TrajectorySummary {
        checkpoints_concluded: concluded,
        ..Default::default()
    };
    for r in ratios {
        // Drive the REAL accumulator. Rebuilding this fold by hand is what let
        // an earlier version of these tests pass against a mutation that
        // replaced the running minimum with the last value.
        s.record_checkpoint(Some(*r), false);
    }
    s
}

fn checkpoint_block(s: &super::TrajectorySummary) -> serde_json::Value {
    let out = super::enrich_envelope_with_summary(
        r#"{"result":"stop"}"#.to_string(),
        s,
        &super::HostStats::default(),
        &no_extras(),
        no_findings_dir(),
    serde_json::json!({}),
    );
    serde_json::from_str::<serde_json::Value>(&out).unwrap()["checkpoints"].clone()
}

#[test]
fn checkpoints_are_reduced_not_streamed() {
    // 65 records collapse to the numbers a caller acts on. The full sequence
    // stays in the trajectory, which `trajectory_path` points at.
    let v = checkpoint_block(&summary_over_ratios(&[0.9, 0.2928, 0.8], 1));
    assert_eq!(v["total"], 3);
    assert_eq!(v["concluded"], 1);
    assert!(
        (v["min_tail_ratio"].as_f64().unwrap() - 0.2928).abs() < 1e-9,
        "the worst moment is the trust gate and must survive: {v}"
    );
}

/// (#1959) The regression this replaced. Measured on two real crawls: the
/// DEGENERATE one reported a higher `last_tail_ratio` (0.997) than the clean
/// one (0.976), because it decayed to 0.193, tripped the gate, and recovered.
/// The field an operator reads first ranked the bad run as the healthier one.
#[test]
fn a_run_that_degenerated_and_recovered_does_not_look_healthy() {
    // The real trajectory, abbreviated: decay, gate fires, clean recovery.
    let bad = summary_over_ratios(
        &[1.0, 0.98, 0.64, 0.46, 0.36, 0.30, 0.25, 0.193, 0.99, 0.99, 0.95, 1.0],
        1,
    );
    let good = summary_over_ratios(&[0.99, 0.98, 0.99, 0.976], 0);

    let (b, g) = (checkpoint_block(&bad), checkpoint_block(&good));
    let (bmin, gmin) = (b["min_tail_ratio"].as_f64().unwrap(), g["min_tail_ratio"].as_f64().unwrap());
    assert!(
        bmin < gmin,
        "the degenerate run must not out-rank the clean one: {bmin} vs {gmin}"
    );
    assert!(bmin < 0.25, "its worst checkpoint tripped the gate: {bmin}");
    // Both runs END clean, which is exactly why the last value could not
    // separate them and the minimum can. The mean separates them too, by a
    // wide margin — asserted as a RELATIONSHIP rather than a constant so the
    // test states the property instead of restating the arithmetic.
    let (bmean, gmean) = (
        b["mean_tail_ratio"].as_f64().unwrap(),
        g["mean_tail_ratio"].as_f64().unwrap(),
    );
    assert!(
        bmean < gmean - 0.2,
        "mean says how MUCH of the run was compromised: {bmean} vs {gmean}"
    );
}

/// min alone collapses "one excursion, recovered" into "chronically
/// degenerate" — both have the same minimum. The mean is what separates them,
/// which is why both are carried.
#[test]
fn mean_separates_a_brief_excursion_from_a_chronically_bad_run() {
    let brief = checkpoint_block(&summary_over_ratios(&[0.19, 0.99, 0.99, 0.99], 1));
    let chronic = checkpoint_block(&summary_over_ratios(&[0.19, 0.22, 0.26, 0.21], 1));
    assert_eq!(
        brief["min_tail_ratio"].as_f64().unwrap(),
        chronic["min_tail_ratio"].as_f64().unwrap(),
        "precondition: identical minima"
    );
    assert!(
        brief["mean_tail_ratio"].as_f64().unwrap()
            > chronic["mean_tail_ratio"].as_f64().unwrap() + 0.5,
        "the mean must tell them apart: {brief} vs {chronic}"
    );
}

#[test]
fn a_dispatch_that_never_checkpointed_omits_the_block() {
    // Unlike detections, zero checkpoints is not a finding — most dispatches
    // never hit the boundary. An always-present `{total: 0}` would be noise
    // on every envelope.
    let out = super::enrich_envelope_with_summary(
        r#"{"result":"stop"}"#.to_string(),
        &super::TrajectorySummary::default(),
        &super::HostStats::default(),
        &no_extras(),
        no_findings_dir(),
    serde_json::json!({}),
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v.get("checkpoints").is_none(), "no boundary hit, no block: {out}");
}

#[test]
fn enrichment_does_not_duplicate_the_runtime_metrics_block() {
    // `metrics` is the RUNTIME counting its own work; the summary is the HOST
    // tailer's observation. Where they disagree (#1947) that is signal, so
    // enrichment must not overwrite or shadow it.
    let s = super::TrajectorySummary {
        turns: 9,
        checkpoints: 2,
        ..Default::default()
    };
    let out = super::enrich_envelope_with_summary(
        r#"{"result":"stop","metrics":{"turns":1}}"#.to_string(),
        &s,
        &super::HostStats::default(),
        &no_extras(),
        no_findings_dir(),
    serde_json::json!({}),
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["metrics"]["turns"], 1,
        "the runtime's own count must be left exactly as reported: {out}"
    );
}

#[test]
fn non_envelope_stdout_is_untouched_by_enrichment() {
    let s = summary_with(vec![serde_json::json!({"kind":"cycle"})]);
    for raw in ["plain model output", "", "{ not json"] {
        assert_eq!(
            super::enrich_envelope_with_summary(
                raw.to_string(),
                &s,
                &super::HostStats::default(),
                &no_extras(),
                no_findings_dir(),
                serde_json::json!({}),
            ),
            raw,
            "the non-json path must pass through verbatim: {raw:?}"
        );
    }
}

// ---------------------------------------------------------------
// (#2107) The host reduction: peak/mean/p95/duty, not peak alone.
//
// A peak answers "did this ever spike"; it can't say how hard the host was
// driven ON AVERAGE, which is what `runtime.turn_delay_ms` (#2094) needs.
// `reduce_metric`/`reduce_host_stats` are pure — a scripted sample list in,
// exact numbers out — so these are driven directly, not through a live
// sampler thread.
// ---------------------------------------------------------------

/// A hand-worked fixture: 5 samples at a steady 2000ms cadence, arithmetic
/// checked by hand in the PR description. Exercises peak, mean (including a
/// non-integer mean for `mem`), p95 (nearest-rank), and duty (a value that
/// stays above 80% for exactly one measured gap, plus a trailing above-80
/// sample with no successor — which must contribute NOTHING, since there is
/// no measured interval to attribute to it).
fn worked_samples() -> Vec<super::HostSampleAt> {
    vec![
        super::HostSampleAt { at_ms: 0, cpu: Some(50), mem: Some(60), gpu: Some(70) },
        super::HostSampleAt { at_ms: 2000, cpu: Some(90), mem: Some(65), gpu: Some(85) },
        super::HostSampleAt { at_ms: 4000, cpu: Some(95), mem: Some(68), gpu: Some(90) },
        super::HostSampleAt { at_ms: 6000, cpu: Some(40), mem: Some(62), gpu: Some(30) },
        super::HostSampleAt { at_ms: 8000, cpu: Some(85), mem: Some(78), gpu: Some(95) },
    ]
}

#[test]
fn the_reduction_yields_the_exact_hand_worked_numbers() {
    let stats = super::reduce_host_stats(&worked_samples());

    assert_eq!(stats.samples, 5);
    assert_eq!(stats.sample_interval_ms, Some(2000), "8000ms span / 4 gaps");

    // cpu: [50, 90, 95, 40, 85] — peak 95, mean 72.0, p95 (nearest-rank of
    // [40,50,85,90,95]) 95. above_80: the 90→95 gap (2000ms) and the 95→40
    // gap (2000ms) both start above 80; the trailing 85 has no successor.
    assert_eq!(stats.cpu.peak_pct, Some(95));
    assert_eq!(stats.cpu.mean_pct, Some(72.0));
    assert_eq!(stats.cpu.p95_pct, Some(95));
    assert_eq!(stats.cpu.above_80_ms, 4000, "trailing above-80 sample must not count: {stats:?}");

    // mem: [60, 65, 68, 62, 78] — never crosses 80, so above_80_ms is 0 and
    // the non-integer mean (333/5 = 66.6 exactly) proves the ONE-DECIMAL
    // rounding, not a truncation to an integer.
    assert_eq!(stats.mem.peak_pct, Some(78));
    assert_eq!(stats.mem.mean_pct, Some(66.6));
    assert_eq!(stats.mem.p95_pct, Some(78));
    assert_eq!(stats.mem.above_80_ms, 0);

    // gpu: [70, 85, 90, 30, 95] — same duty shape as cpu (85→90 and 90→30
    // gaps), on different underlying values.
    assert_eq!(stats.gpu.peak_pct, Some(95));
    assert_eq!(stats.gpu.mean_pct, Some(74.0));
    assert_eq!(stats.gpu.p95_pct, Some(95));
    assert_eq!(stats.gpu.above_80_ms, 4000);
}

#[test]
fn mean_rounds_to_one_decimal_not_a_truncated_integer() {
    // 1+1+2 = 4 / 3 = 1.3333… — a truncating or floor'd reduction would read
    // 1, silently discarding the fractional signal a ~2s cadence can still
    // carry across even a few samples.
    let m = super::reduce_metric(&[(0, 1), (1, 1), (2, 2)]);
    assert_eq!(m.mean_pct, Some(1.3));
}

#[test]
fn an_empty_metric_reduces_to_all_none_not_zero() {
    // Zero is a real value a metric can report; "no reading" is a different
    // claim and must not collapse into it.
    let m = super::reduce_metric(&[]);
    assert_eq!(m.peak_pct, None);
    assert_eq!(m.mean_pct, None);
    assert_eq!(m.p95_pct, None);
    assert_eq!(m.above_80_ms, 0);
}

#[test]
fn a_single_sample_has_no_measured_interval() {
    // One point has no gap to measure — `sample_interval_ms` must say so
    // rather than assert an interval that was never observed.
    let stats = super::reduce_host_stats(&[super::HostSampleAt {
        at_ms: 0,
        cpu: Some(50),
        mem: Some(50),
        gpu: Some(50),
    }]);
    assert_eq!(stats.samples, 1);
    assert_eq!(stats.sample_interval_ms, None);
    // A single reading is still its own peak/mean/p95 — one point IS the
    // whole distribution.
    assert_eq!(stats.cpu.peak_pct, Some(50));
    assert_eq!(stats.cpu.mean_pct, Some(50.0));
    assert_eq!(stats.cpu.p95_pct, Some(50));
}

#[test]
fn host_stats_reach_the_envelope_nested_by_metric_with_top_level_aliases() {
    let stats = super::reduce_host_stats(&worked_samples());
    let out = super::enrich_envelope_with_summary(
        r#"{"result":"stop"}"#.to_string(),
        &super::TrajectorySummary::default(),
        &stats,
        &no_extras(),
        no_findings_dir(),
    serde_json::json!({}),
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["host"]["cpu"]["peak_pct"], 95);
    assert_eq!(v["host"]["cpu"]["mean_pct"], 72.0);
    assert_eq!(v["host"]["cpu"]["p95_pct"], 95);
    assert_eq!(v["host"]["cpu"]["above_80_ms"], 4000);
    assert_eq!(v["host"]["mem"]["peak_pct"], 78);
    assert_eq!(v["host"]["gpu"]["peak_pct"], 95);
    assert_eq!(v["host"]["samples"], 5);
    assert_eq!(v["host"]["sample_interval_ms"], 2000);
    // (#2107) Deprecated top-level aliases — kept for one release for any
    // reader still looking at the pre-#2107 shape. They must read straight
    // off the SAME nested peaks, never a second, independently-computed
    // figure that could drift from them.
    assert_eq!(
        v["host"]["peak_cpu_pct"], 95,
        "alias must mirror host.cpu.peak_pct exactly: {out}"
    );
    assert_eq!(
        v["host"]["peak_mem_pct"], 78,
        "alias must mirror host.mem.peak_pct exactly: {out}"
    );
}

#[test]
fn power_thermal_and_energy_reach_the_envelope_without_disturbing_the_2107_shape() {
    use crate::host_probe::{HostExtraAt, PowerSample, ThermalSample};
    // `None`: this test is about envelope SHAPE (the #2107/#2108 additive
    // contract below), not the sleep-gap cap — that's covered by its own
    // tests in host_probe::mod.rs and darkmux-serve's host_sampler.rs.
    let extras = crate::host_probe::reduce_host_extras(
        &[
            HostExtraAt {
                at_ms: 0,
                power: Some(PowerSample { cpu_mw: 1000.0, gpu_mw: 100.0, ane_mw: 0.0 }),
                thermal: Some(ThermalSample { state: "nominal".into(), cpu_speed_limit_pct: 100 }),
            },
            HostExtraAt {
                at_ms: 3_600_000,
                power: Some(PowerSample { cpu_mw: 3000.0, gpu_mw: 100.0, ane_mw: 0.0 }),
                thermal: Some(ThermalSample { state: "serious".into(), cpu_speed_limit_pct: 62 }),
            },
        ],
        None,
    );
    let stats = super::reduce_host_stats(&worked_samples());
    let out = super::enrich_envelope_with_summary(
        r#"{"result":"stop"}"#.to_string(),
        &super::TrajectorySummary::default(),
        &stats,
        &extras,
        no_findings_dir(),
    serde_json::json!({}),
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["host"]["power"]["cpu"]["peak_mw"], 3000);
    assert_eq!(v["host"]["power"]["cpu"]["mean_mw"], 2000.0);
    assert_eq!(v["host"]["power"]["gpu"]["mean_mw"], 100.0);
    assert_eq!(v["host"]["power"]["total"]["peak_mw"], 3100);
    assert_eq!(v["host"]["thermal"]["worst_state"], "serious");
    assert_eq!(v["host"]["thermal"]["min_cpu_speed_limit_pct"], 62);
    // 1100 mW held for exactly one hour → 1100 mWh (left-Riemann: the FIRST
    // sample's power holds until the second).
    let e = v["host"]["energy_mwh"].as_f64().expect("energy");
    assert!((e - 1100.0).abs() < 0.001, "got {e}: {out}");

    // ADDITIVE only: every #2107 field is byte-identical to what the same
    // stats produce with no extras at all.
    let without = super::enrich_envelope_with_summary(
        r#"{"result":"stop"}"#.to_string(),
        &super::TrajectorySummary::default(),
        &stats,
        &no_extras(),
        no_findings_dir(),
    serde_json::json!({}),
    );
    let w: serde_json::Value = serde_json::from_str(&without).unwrap();
    for key in ["cpu", "mem", "gpu", "samples", "sample_interval_ms", "peak_cpu_pct", "peak_mem_pct"] {
        assert_eq!(v["host"][key], w["host"][key], "#2108 must not move `host.{key}`");
    }
}

#[test]
fn a_host_without_power_or_thermal_sources_omits_those_blocks() {
    // The non-Apple-Silicon case, and the "IOReport unavailable" case: the
    // cpu/mem/gpu reduction still lands, and the blocks the probe could not
    // read are ABSENT rather than zeroed.
    let out = super::enrich_envelope_with_summary(
        r#"{"result":"stop"}"#.to_string(),
        &super::TrajectorySummary::default(),
        &super::reduce_host_stats(&worked_samples()),
        &no_extras(),
        no_findings_dir(),
    serde_json::json!({}),
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v["host"]["cpu"]["peak_pct"].is_number(), "the #2107 block still lands");
    for key in ["power", "thermal", "energy_mwh"] {
        assert!(
            v["host"].get(key).is_none(),
            "`host.{key}` must be absent (not measured), never zeroed: {out}"
        );
    }
}

#[test]
fn an_unsampled_run_omits_the_host_block_rather_than_reporting_zero() {
    // A dispatch too short to sample, or one where sampling failed, must not
    // claim it observed an idle machine. Absent means "not measured".
    let out = super::enrich_envelope_with_summary(
        r#"{"result":"stop"}"#.to_string(),
        &super::TrajectorySummary::default(),
        &super::HostStats::default(),
        &no_extras(),
        no_findings_dir(),
    serde_json::json!({}),
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(
        v.get("host").is_none(),
        "unsampled must be absent, never a zeroed block that reads as measured: {out}"
    );
}

// ---------------------------------------------------------------
// (#1959) The envelope says what the crawl FOUND.
//
// A crawler dispatch converged, wrote its findings to a temp dir, and
// returned an envelope byte-indistinguishable from one that found nothing.
// The findings were also never copied into the lab run dir, so the run's
// durable artifact recorded that work happened and not what it produced.
// ---------------------------------------------------------------

/// (#2108) The "this host reported no power/thermal source" extras — the
/// state every pre-#2108 assertion in this file was implicitly written
/// against, so the #2107 `host` block's shape stays pinned by tests that say
/// nothing about power. The #2108 fields get their own tests below.
fn no_extras() -> crate::host_probe::HostExtras {
    crate::host_probe::HostExtras::default()
}

/// A path with no `findings.jsonl` under it — the shape every non-crawler
/// dispatch has, and the reason the block must be absent rather than zeroed.
fn no_findings_dir() -> &'static std::path::Path {
    std::path::Path::new("/nonexistent/darkmux-out-test")
}

fn out_dir_with_findings(lines: &[&str]) -> tempfile::TempDir {
    let td = tempfile::tempdir().expect("tempdir");
    let rt = td.path().join(".darkmux-runtime");
    std::fs::create_dir_all(&rt).expect("mkdir");
    std::fs::write(rt.join("findings.jsonl"), lines.join("\n") + "\n").expect("write");
    td
}

#[test]
fn the_envelope_reports_how_many_findings_the_crawl_recorded() {
    let td = out_dir_with_findings(&[
        r#"{"file":"lib.rs","line":147}"#,
        r#"{"file":"lib.rs","line":416}"#,
        r#"{"file":"lib.rs","line":424}"#,
    ]);
    let out = super::enrich_envelope_with_summary(
        r#"{"result":"stop"}"#.to_string(),
        &super::TrajectorySummary::default(),
        &super::HostStats::default(),
        &no_extras(),
        td.path(),
    serde_json::json!({}),
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["findings"]["count"], 3, "the caller's next action turns on this: {out}");
    assert!(
        v["findings"]["path"].as_str().unwrap().ends_with("findings.jsonl"),
        "a count with no way to read them is half an answer: {out}"
    );
    assert_eq!(v["result"], "stop", "existing fields survive");
}

#[test]
fn a_trailing_newline_is_not_a_finding() {
    // A count that says 2 when the file holds 1 is worse than no count.
    let td = out_dir_with_findings(&[r#"{"file":"lib.rs","line":147}"#]);
    let out = super::enrich_envelope_with_summary(
        r#"{"result":"stop"}"#.to_string(),
        &super::TrajectorySummary::default(),
        &super::HostStats::default(),
        &no_extras(),
        td.path(),
    serde_json::json!({}),
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["findings"]["count"], 1, "records, not lines: {out}");
}

#[test]
fn no_findings_file_means_the_channel_was_never_used_not_that_nothing_was_found() {
    // The distinction is load-bearing and is why this is not a zeroed block:
    // the runtime creates the file on the first successful `report_finding`,
    // so for a role HOLDING that tool an absent file is the #1959 failure —
    // the model decided the tool "is not available in this runtime" and
    // narrated its findings into prose instead. `count: 0` cannot say that.
    let out = super::enrich_envelope_with_summary(
        r#"{"result":"stop"}"#.to_string(),
        &super::TrajectorySummary::default(),
        &super::HostStats::default(),
        &no_extras(),
        no_findings_dir(),
    serde_json::json!({}),
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(
        v.get("findings").is_none(),
        "absence is the signal; a zeroed block would erase it: {out}"
    );
}

    // ── first-run failure modes (2026-08-28 probes) ──────────────────────

    #[test]
    fn placeholder_model_ids_are_recognized() {
        assert!(is_placeholder_model_id("<your-worker-model-id>"));
        assert!(is_placeholder_model_id("<anything>"));
        assert!(!is_placeholder_model_id("qwen/qwen3.6-35b-a3b"));
        assert!(!is_placeholder_model_id("<not-closed"));
        assert!(!is_placeholder_model_id(""));
    }

    #[test]
    fn placeholder_model_error_names_the_profile_the_file_and_the_fix() {
        let msg = placeholder_model_error("balanced", "<your-worker-model-id>", std::path::Path::new("/home/x/.darkmux/profiles.json"));
        assert!(msg.contains("`balanced`"), "{msg}");
        assert!(msg.contains("/home/x/.darkmux/profiles.json"), "{msg}");
        assert!(msg.contains("lms ls"), "{msg}");
        assert!(msg.contains("darkmux init"), "{msg}");
    }

    #[test]
    fn a_missing_lms_binary_is_not_blamed_on_ram() {
        use darkmux_gestalt::HostError;
        let err = map_load_result(
            Err(HostError::CommandFailed { detail: "`/nonexistent/lms` was not found".into() }),
            "qwen3-4b",
            32000,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("was not found"), "{err}");
        assert!(!err.to_lowercase().contains("insufficient ram"), "a spawn failure is not a RAM problem: {err}");
    }

    #[test]
    fn an_unknown_model_error_says_how_to_get_one() {
        use darkmux_gestalt::HostError;
        let err = map_load_result(Err(HostError::UnknownModel { model_key: "nobody/7b".into() }), "nobody/7b", 32000)
            .unwrap_err()
            .to_string();
        assert!(err.contains("nobody/7b"), "{err}");
        assert!(err.contains("lms ls"), "{err}");
        assert!(err.to_lowercase().contains("download"), "{err}");
    }

    #[test]
    fn a_connection_refusal_names_the_url_and_the_server_to_start() {
        let msg = describe_curl_failure("http://127.0.0.1:1234/v1/chat/completions", 7, "curl: (7) Failed to connect");
        assert!(msg.contains("http://127.0.0.1:1234"), "{msg}");
        assert!(msg.contains("LM Studio"), "{msg}");
        assert!(msg.contains("lms server start"), "{msg}");
        assert!(!msg.contains("hosted"), "a local URL is not a hosted endpoint: {msg}");
        let other = describe_curl_failure("https://api.example.com/v1/chat/completions", 22, "curl: (22) 401");
        assert!(other.contains("exit 22") && other.contains("401"), "{other}");
        assert!(!other.contains("lms server start"), "an HTTP error is not a connection refusal: {other}");
    }

    // ─── #2131 review round 2, MUST-FIX 2 — the container dispatch path ───
    // is now interruptible: the docker spawn site registers its child pid
    // (mirroring the hosted-curl path above) and the trajectory tailer's
    // own poll loop checks `interrupt::is_set()` each tick, killing every
    // registered child and returning promptly instead of running out the
    // dispatch's own inactivity timeout.

    #[test]
    #[serial]
    fn run_tailer_kills_registered_children_on_interrupt_and_returns_promptly() {
        // A real integration proof of the tailer's own interrupt-check —
        // the ONE poll point the docker/coder/crawl dispatch path has
        // between spawning the container and the main thread's blocking
        // `wait_with_output()` returning (`run_step_graph` gives it none
        // of its own). No Docker or runtime image needed (unavailable in
        // this sandbox; the release-gate doctrine reserves real-container
        // runs for dogfood, not `cargo test`): a real OS child (`sleep
        // 100` — a fake dispatch, standing in for the docker child the
        // spawn site would otherwise register) is registered through the
        // SAME `child_registry::register` call the spawn site now makes,
        // and `interrupt::simulate_sigterm_for_test()` drives the SAME
        // flag a real caught SIGTERM would. Asserts the child is actually
        // dead — reaped, killed by signal — well within 5 seconds, not
        // just that a flag flipped.
        darkmux_types::interrupt::reset_for_test();
        darkmux_types::child_registry::reset_for_test();

        let mut fake_dispatch_child = std::process::Command::new("sleep")
            .arg("100")
            .spawn()
            .expect("spawning a real OS child (sleep) for this test");
        let fake_pid = fake_dispatch_child.id();
        darkmux_types::child_registry::register(fake_pid);

        // Fire the SAME flag a real caught SIGTERM would set — BEFORE
        // starting the tailer, so its first poll iteration already
        // observes it (mirrors a signal arriving mid-dispatch, well
        // before the container would exit on its own).
        darkmux_types::interrupt::simulate_sigterm_for_test();
        assert!(darkmux_types::interrupt::is_set(), "the simulated SIGTERM must set the flag");

        let out_dir = TempDir::new().unwrap();
        let inactivity_deadline = Arc::new(Mutex::new(Instant::now() + Duration::from_secs(600)));
        let started = Instant::now();
        let _summary = run_tailer(
            out_dir.path().to_path_buf(),
            "test-session".to_string(),
            "coder".to_string(),
            "test-model".to_string(),
            None,
            None,
            None,
            // stop_flag — deliberately never set. Only the interrupt
            // check may end this loop; if that check is missing or
            // broken, this call hangs (well past any sane test timeout)
            // instead of silently passing.
            Arc::new(AtomicBool::new(false)),
            inactivity_deadline,
            600,
            None,
            None,
        );
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(5),
            "the tailer must observe the interrupt and return well within 5s, took {elapsed:?}"
        );

        // The registered child must actually be dead. `kill_all` sends
        // SIGKILL by pid; reap it (avoiding a zombie) and check it died
        // BY SIGNAL, not a normal exit (which would just mean `sleep`
        // happened to be interruptible some other way, proving nothing
        // about this code path).
        let status = fake_dispatch_child.wait().expect("the killed child must be reapable");
        assert!(
            !status.success(),
            "a `sleep 100` that exits within this test's wall-clock must have been KILLED, not \
             finished naturally"
        );
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(
                status.signal(),
                Some(darkmux_types::child_registry::SIGKILL),
                "must die by SIGKILL — the signal `kill_all` sends"
            );
        }

        darkmux_types::child_registry::deregister(fake_pid);
        darkmux_types::interrupt::reset_for_test();
        darkmux_types::child_registry::reset_for_test();
    }

    // (#2131 review round 2, NEW-1) `docker_kill_by_name` is the ONE place
    // that stops a container by its deterministic name — shared now by the
    // inactivity watchdog, the wait-error teardown, and the interrupt
    // teardown, so a single test covers all three call sites at once.
    // Before this fix, the interrupt branch killed only the registered
    // `docker run` CLIENT pid, which does NOT stop the container (measured
    // live: `docker ps` still showed it `Up` after `kill -9` on the CLI's
    // pid). No real Docker/`docker ps` is available in this sandbox, so
    // this is the mocked equivalent: a fake `docker` executable on `PATH`
    // that records its own argv, standing in for the real `docker`
    // binary this function shells out to.
    #[test]
    #[serial]
    fn docker_kill_by_name_invokes_docker_kill_with_the_exact_container_name() {
        let dir = TempDir::new().unwrap();
        let record_path = dir.path().join("invoked-with.txt");
        let fake_docker_path = dir.path().join("docker");
        // A trivial shell script standing in for `docker`: writes its own
        // argv (space-joined) to `record_path`, then exits 0 — enough to
        // prove WHAT this function invokes without needing the real
        // Docker CLI or daemon.
        std::fs::write(
            &fake_docker_path,
            format!("#!/bin/sh\necho \"$@\" > {}\n", record_path.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake_docker_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_docker_path, perms).unwrap();
        }

        let prev_path = std::env::var("PATH").ok();
        // SAFETY: serialized via #[serial].
        unsafe {
            std::env::set_var(
                "PATH",
                format!("{}:{}", dir.path().display(), prev_path.clone().unwrap_or_default()),
            );
        }

        docker_kill_by_name("darkmux-test-container-2131");

        // SAFETY: serialized via #[serial].
        unsafe {
            match &prev_path {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
        }

        let recorded = std::fs::read_to_string(&record_path)
            .expect("docker_kill_by_name must have invoked the fake `docker` on PATH");
        assert_eq!(
            recorded.trim(),
            "kill darkmux-test-container-2131",
            "must call `docker kill <exact container name>`, not some other subcommand or a \
             different name"
        );
    }

    // ─── (#2111) should_record_machine_telemetry — periodic curve cadence ─

    #[test]
    fn should_record_machine_telemetry_every_5th_of_12_yields_exactly_2() {
        let every = 5;
        let hits: Vec<u64> =
            (1..=12u64).filter(|&i| super::should_record_machine_telemetry(i, every)).collect();
        assert_eq!(hits, vec![5, 10], "exactly the 5th and 10th of 12 ticks");
    }

    #[test]
    fn should_record_machine_telemetry_zero_disables_entirely() {
        for i in 1..=50u64 {
            assert!(
                !super::should_record_machine_telemetry(i, 0),
                "every=0 must never fire, tick {i}"
            );
        }
    }

    // ─── (#2111) build_machine_telemetry_record — payload + mission context ─

    #[test]
    fn build_machine_telemetry_record_carries_mission_context_and_full_payload() {
        use crate::host_probe::{CpuCluster, HostSampleFull, PowerSample, ThermalSample};
        let sample = HostSampleFull {
            cost_ms: 7,
            cpu_pct: Some(42),
            cpu_clusters: Some(vec![CpuCluster {
                name: "Super".into(),
                cores: 6,
                pct: Some(30),
                mhz: Some(4200),
            }]),
            mem_pct: Some(55),
            gpu_pct: Some(12),
            gpu_mhz: Some(500),
            gpu_mem_bytes: Some(1_000_000),
            thermal: Some(ThermalSample { state: "fair".into(), cpu_speed_limit_pct: 90 }),
            power: Some(PowerSample { cpu_mw: 800.0, gpu_mw: 100.0, ane_mw: 0.0 }),
        };
        let record_context = Some(serde_json::json!({ "unit": "u-1", "source": "repo" }));
        let rec = super::build_machine_telemetry_record(
            &sample,
            12_345,
            "coder",
            "session-1",
            "qwen3.6-35b-a3b",
            Some("mission-1"),
            Some("phase-1"),
            &record_context,
        );
        assert_eq!(rec.action, "machine.telemetry");
        assert!(matches!(rec.category, darkmux_flow::Category::Telemetry));
        assert_eq!(rec.mission_id.as_deref(), Some("mission-1"));
        assert_eq!(rec.phase_id.as_deref(), Some("phase-1"));
        assert_eq!(rec.session_id.as_deref(), Some("session-1"));
        assert_eq!(rec.model.as_deref(), Some("qwen3.6-35b-a3b"));
        let payload = rec.payload.expect("payload present");
        assert_eq!(payload["cpu_pct"], 42);
        assert_eq!(payload["mem_pct"], 55);
        assert_eq!(payload["gpu_pct"], 12);
        assert_eq!(payload["gpu_mhz"], 500);
        assert_eq!(payload["sampler_cost_ms"], 7);
        assert_eq!(payload["sampled_at_ms"], 12345);
        assert_eq!(payload["thermal"]["state"], "fair");
        assert_eq!(payload["thermal"]["cpu_speed_limit_pct"], 90);
        assert_eq!(payload["power_mw"]["total"], 900);
        assert_eq!(payload["cpu_clusters"][0]["name"], "Super");
        assert_eq!(payload["context"]["unit"], "u-1", "record_context merges under payload.context");
    }

    #[test]
    fn build_machine_telemetry_record_omits_mission_and_context_when_absent() {
        use crate::host_probe::HostSampleFull;
        let sample = HostSampleFull { cpu_pct: Some(10), ..Default::default() };
        let rec = super::build_machine_telemetry_record(
            &sample, 0, "coder", "session-1", "some-model", None, None, &None,
        );
        assert!(rec.mission_id.is_none());
        assert!(rec.phase_id.is_none());
        let payload = rec.payload.expect("payload present");
        assert!(payload.get("context").is_none(), "no record_context ⇒ no payload.context key");
    }

    // ─── (#2111) host_window_json — the dispatch-summary flow-record field ─

    #[test]
    fn host_window_reaches_the_envelope_with_the_flattened_summary_shape() {
        use crate::host_probe::{HostExtraAt, PowerSample, ThermalSample};
        let extras = crate::host_probe::reduce_host_extras(
            &[
                HostExtraAt {
                    at_ms: 0,
                    power: Some(PowerSample { cpu_mw: 1000.0, gpu_mw: 100.0, ane_mw: 0.0 }),
                    thermal: Some(ThermalSample { state: "nominal".into(), cpu_speed_limit_pct: 100 }),
                },
                HostExtraAt {
                    at_ms: 8000,
                    power: Some(PowerSample { cpu_mw: 3000.0, gpu_mw: 100.0, ane_mw: 0.0 }),
                    thermal: Some(ThermalSample { state: "serious".into(), cpu_speed_limit_pct: 62 }),
                },
            ],
            Some(2000),
        );
        let stats = super::reduce_host_stats(&worked_samples());
        let out = super::enrich_envelope_with_summary(
            r#"{"result":"stop"}"#.to_string(),
            &super::TrajectorySummary::default(),
            &stats,
            &extras,
            no_findings_dir(),
            serde_json::json!({}),
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["host_window"]["thermal_worst_state"], "serious");
        assert_eq!(v["host_window"]["min_cpu_speed_limit_pct"], 62);
        // above_nominal_ms uses left-Riemann duty: a gap counts only when
        // the sample that STARTS it is non-nominal. Here sample 0 (at
        // t=0) IS nominal and sample 1 (at t=8000) is serious, so the
        // nominal→serious gap contributes 0 — the elevated state hasn't
        // been observed to persist for any measured interval yet.
        assert_eq!(v["host_window"]["above_nominal_ms"], 0);
        assert_eq!(v["host_window"]["samples"], 5);
        assert_eq!(v["host_window"]["span_ms"], 8000, "2000ms interval * 4 gaps across 5 samples");
        let total = &v["host_window"]["power_mw_total"];
        // Two readings, 1100mW and 3100mW: mean (1100+3100)/2 = 2100.0;
        // nearest-rank p95 of a 2-element sorted set is the larger, 3100;
        // max is 3100.
        assert_eq!(total["mean"], 2100.0);
        assert_eq!(total["p95"], 3100);
        assert_eq!(total["max"], 3100);
    }

    #[test]
    fn an_unsampled_run_omits_host_window_too() {
        let out = super::enrich_envelope_with_summary(
            r#"{"result":"stop"}"#.to_string(),
            &super::TrajectorySummary::default(),
            &super::HostStats::default(),
            &no_extras(),
            no_findings_dir(),
            serde_json::json!({}),
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("host_window").is_none(), "unsampled must omit host_window, never zero it");
    }

    // ─── (#2111 review finding) build_dispatch_complete_payload — host_window
    //     reaches the FLOW RECORD, not just the envelope ─────────────────────

    #[test]
    fn build_dispatch_complete_payload_carries_host_window_on_success() {
        use crate::host_probe::{HostExtraAt, PowerSample, ThermalSample};
        let extras = crate::host_probe::reduce_host_extras(
            &[
                HostExtraAt {
                    at_ms: 0,
                    power: Some(PowerSample { cpu_mw: 1000.0, gpu_mw: 100.0, ane_mw: 0.0 }),
                    thermal: Some(ThermalSample { state: "nominal".into(), cpu_speed_limit_pct: 100 }),
                },
                HostExtraAt {
                    at_ms: 8000,
                    power: Some(PowerSample { cpu_mw: 3000.0, gpu_mw: 100.0, ane_mw: 0.0 }),
                    thermal: Some(ThermalSample { state: "serious".into(), cpu_speed_limit_pct: 62 }),
                },
            ],
            Some(2000),
        );
        let stats = super::reduce_host_stats(&worked_samples());
        let payload = super::build_dispatch_complete_payload(
            1000,
            super::RestTotals { rest_ms: 200, rests: 1 },
            Some(50),
            "stdout-body",
            "",
            0,
            &super::TrajectorySummary::default(),
            super::TokenTotals { prompt: 10, completion: 20 },
            None,
            &stats,
            &extras,
            &None,
            None,
        );
        assert_eq!(
            payload["host_window"]["thermal_worst_state"], "serious",
            "the FLOW RECORD payload must carry host_window, not just the envelope: {payload}"
        );
        assert_eq!(payload["host_window"]["min_cpu_speed_limit_pct"], 62);
        assert_eq!(payload["host_window"]["samples"], 5);
        assert_eq!(payload["wall_ms"], 1000);
        assert_eq!(payload["result_class"], "ok");
    }

    #[test]
    fn build_dispatch_complete_payload_carries_host_window_on_the_error_path_too() {
        use crate::host_probe::{HostExtraAt, PowerSample, ThermalSample};
        let extras = crate::host_probe::reduce_host_extras(
            &[
                HostExtraAt {
                    at_ms: 0,
                    power: Some(PowerSample { cpu_mw: 500.0, gpu_mw: 50.0, ane_mw: 0.0 }),
                    thermal: Some(ThermalSample { state: "fair".into(), cpu_speed_limit_pct: 90 }),
                },
                HostExtraAt {
                    at_ms: 2000,
                    power: Some(PowerSample { cpu_mw: 600.0, gpu_mw: 60.0, ane_mw: 0.0 }),
                    thermal: Some(ThermalSample { state: "critical".into(), cpu_speed_limit_pct: 40 }),
                },
            ],
            Some(2000),
        );
        let stats = super::reduce_host_stats(&worked_samples());
        let payload = super::build_dispatch_complete_payload(
            500,
            super::RestTotals::default(),
            None,
            "",
            "boom: container crashed",
            137,
            &super::TrajectorySummary::default(),
            super::TokenTotals::default(),
            Some("azure/gpt-x"),
            &stats,
            &extras,
            &None,
            None,
        );
        assert_eq!(payload["result_class"], "error");
        assert_eq!(payload["endpoint"], "azure/gpt-x");
        assert_eq!(
            payload["host_window"]["thermal_worst_state"], "critical",
            "the ERROR payload must carry host_window too, not just the success path: {payload}"
        );
        assert!(
            payload["stderr_excerpt"].is_string(),
            "error path must carry a stderr excerpt: {payload}"
        );
    }

    #[test]
    fn build_dispatch_complete_payload_omits_host_window_when_unsampled() {
        let payload = super::build_dispatch_complete_payload(
            10,
            super::RestTotals::default(),
            None,
            "",
            "",
            0,
            &super::TrajectorySummary::default(),
            super::TokenTotals::default(),
            None,
            &super::HostStats::default(),
            &no_extras(),
            &None,
            None,
        );
        assert!(
            payload.get("host_window").is_none(),
            "unsampled must omit host_window on the FLOW RECORD too, never zero it"
        );
    }
