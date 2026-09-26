//! (#2902 step 3) The one endpoint/model resolver.
//!
//! Characterization first: the table below pins what every profile shape
//! this repo ships or tests resolves to on the wire, so the consolidation
//! that follows can be shown not to move any of it.

#[cfg(test)]
mod equivalence_tests {
    use darkmux_types::ProfileModel;

    /// One profile model shape, and what a dispatch against it puts on the
    /// wire. `auth` is `(header name, credential source)` where the source
    /// is `env:<VAR>` or `keychain:<item>`, never a value.
    struct Row {
        name: &'static str,
        model: &'static str,
        chat_url: &'static str,
        wire_model: &'static str,
        managed: bool,
        cap_field: &'static str,
        auth: Option<(&'static str, &'static str)>,
        n_ctx: Option<u32>,
    }

    const LMS: &str = "http://127.0.0.1:4321";
    const KEY_ENV: &str = "DMX_2902_EQUIV_KEY";

    fn rows() -> Vec<Row> {
        vec![
            Row {
                name: "managed, bare (profiles.example.json fast/balanced/deep)",
                model: r#"{"id":"qwen3.6-35b-a3b","n_ctx":100000}"#,
                chat_url: "http://127.0.0.1:4321/v1/chat/completions",
                wire_model: "darkmux:qwen3.6-35b-a3b",
                managed: true,
                cap_field: "max_tokens",
                auth: None,
                n_ctx: Some(100000),
            },
            Row {
                name: "managed, identifier opt-out",
                model: r#"{"id":"worker-35b","n_ctx":65536,"identifier":"my-alias"}"#,
                chat_url: "http://127.0.0.1:4321/v1/chat/completions",
                wire_model: "my-alias",
                managed: true,
                cap_field: "max_tokens",
                auth: None,
                n_ctx: Some(65536),
            },
            Row {
                name: "managed, namespaced id in the registry",
                model: r#"{"id":"darkmux:worker-35b","n_ctx":65536}"#,
                chat_url: "http://127.0.0.1:4321/v1/chat/completions",
                wire_model: "darkmux:worker-35b",
                managed: true,
                cap_field: "max_tokens",
                auth: None,
                n_ctx: Some(65536),
            },
            Row {
                name: "inline endpoint with no url (reasoning_effort only)",
                model: r#"{"id":"local-ep","n_ctx":8000,"endpoint":{"reasoning_effort":"high"}}"#,
                chat_url: "http://127.0.0.1:4321/v1/chat/completions",
                wire_model: "darkmux:local-ep",
                managed: true,
                cap_field: "max_tokens",
                auth: None,
                n_ctx: Some(8000),
            },
            Row {
                name: "hosted, api_version (profiles.example.json hosted-frontier)",
                model: r#"{"id":"gpt-5.1","endpoint":{"url":"https://r.cognitiveservices.azure.com/openai/deployments/d","api_version":"2025-01-01-preview"}}"#,
                chat_url: "https://r.cognitiveservices.azure.com/openai/deployments/d/chat/completions?api-version=2025-01-01-preview",
                wire_model: "gpt-5.1",
                managed: false,
                cap_field: "max_completion_tokens",
                auth: None,
                n_ctx: None,
            },
            Row {
                name: "hosted, bearer keychain, declared window (guide mod seats)",
                model: r#"{"id":"grok-4","n_ctx":128000,"endpoint":{"url":"https://api.x.ai/v1","auth":{"type":"bearer","keychain":"darkmux-grok"}}}"#,
                chat_url: "https://api.x.ai/v1/chat/completions",
                wire_model: "grok-4",
                managed: false,
                cap_field: "max_completion_tokens",
                auth: Some(("Authorization", "keychain:darkmux-grok")),
                n_ctx: Some(128000),
            },
            Row {
                name: "hosted, api-key from key_env, trailing slash",
                model: r#"{"id":"gpt-4o","endpoint":{"url":"https://x.openai.azure.com/openai/deployments/gpt-4o/","api_version":"v1","auth":{"type":"api-key","key_env":"DMX_2902_EQUIV_KEY","keychain":"unused-item"}}}"#,
                chat_url: "https://x.openai.azure.com/openai/deployments/gpt-4o/chat/completions?api-version=v1",
                wire_model: "gpt-4o",
                managed: false,
                cap_field: "max_completion_tokens",
                auth: Some(("api-key", "env:DMX_2902_EQUIV_KEY")),
                n_ctx: None,
            },
            Row {
                name: "hosted, auth block with no type sends no header",
                model: r#"{"id":"proxy-model","endpoint":{"url":"http://localhost:8080/v1","auth":{"keychain":"x"}}}"#,
                chat_url: "http://localhost:8080/v1/chat/completions",
                wire_model: "proxy-model",
                managed: false,
                cap_field: "max_completion_tokens",
                auth: None,
                n_ctx: None,
            },
        ]
    }

    /// What the code resolves `model` (one profile's only model) to.
    struct Observed {
        chat_url: String,
        wire_model: String,
        managed: bool,
        cap_field: &'static str,
        auth: Option<(String, String)>,
        n_ctx: Option<u32>,
    }

    fn observe(model_json: &str, pf: &std::path::Path) -> Observed {
        let pm: ProfileModel = serde_json::from_str(model_json).unwrap();
        let managed = !pm.is_remote();
        let chat_url = if managed {
            crate::single_shot::local_chat_url(None)
        } else {
            crate::dispatch_internal::remote_chat_url(pm.endpoint.as_ref().unwrap())
        };
        let wire_model = if managed {
            darkmux_gestalt::namespaced_identifier(
                crate::dispatch_internal::bare_model_key(&pm.id),
                pm.identifier.as_deref(),
            )
        } else {
            pm.id.clone()
        };
        let body = if managed {
            crate::single_shot::local_chat_body(&wire_model, "s", "u", 0.7, 10)
        } else {
            crate::single_shot::hosted_chat_body(&wire_model, "s", "u", 10, None)
        };
        let cap_field = if body.get("max_tokens").is_some() { "max_tokens" } else { "max_completion_tokens" };
        let auth = if managed {
            None
        } else {
            let ep = pm.endpoint.as_ref().unwrap();
            match ep.auth.as_ref() {
                Some(a) if a.auth_type.is_some() => {
                    // Only an env-sourced credential is resolved for real
                    // here; a Keychain item is never read by a test.
                    let env_present = a.key_env.as_deref().is_some_and(|v| std::env::var(v).is_ok());
                    let header = if env_present {
                        crate::dispatch_internal::remote_auth_header(ep).ok().flatten().map(|(h, _)| h)
                    } else {
                        None
                    };
                    let source = match a.key_env.as_deref() {
                        Some(v) if std::env::var(v).is_ok() => format!("env:{v}"),
                        _ => format!("keychain:{}", a.keychain.clone().unwrap_or_default()),
                    };
                    // A keychain-only row is never read here: the header
                    // name follows from the declared type alone.
                    let header = header.unwrap_or_else(|| match a.auth_type {
                        Some(darkmux_types::EndpointAuthType::ApiKey) => "api-key".to_string(),
                        _ => "Authorization".to_string(),
                    });
                    Some((header, source))
                }
                _ => None,
            }
        };
        let n_ctx = crate::dispatch_internal::resolve_context_window_internal(None, Some("p"), pf.to_str()).unwrap();
        Observed { chat_url, wire_model, managed, cap_field, auth, n_ctx }
    }

    #[test]
    #[serial_test::serial] // mutates DARKMUX_LMSTUDIO_URL and a key env var
    fn every_shipped_profile_shape_resolves_to_the_same_wire_facts() {
        let prev_url = std::env::var("DARKMUX_LMSTUDIO_URL").ok();
        unsafe {
            std::env::set_var("DARKMUX_LMSTUDIO_URL", LMS);
            std::env::set_var(KEY_ENV, "fake-test-value");
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let mut failures = Vec::new();
        for row in rows() {
            let pf = tmp.path().join("profiles.json");
            std::fs::write(
                &pf,
                format!(r#"{{"profiles":{{"p":{{"models":[{}]}}}},"default_profile":"p"}}"#, row.model),
            )
            .unwrap();
            let got = observe(row.model, &pf);
            let want_auth = row.auth.map(|(h, s)| (h.to_string(), s.to_string()));
            if got.chat_url != row.chat_url
                || got.wire_model != row.wire_model
                || got.managed != row.managed
                || got.cap_field != row.cap_field
                || got.auth != want_auth
                || got.n_ctx != row.n_ctx
            {
                failures.push(format!(
                    "{}: url={} model={} managed={} cap={} auth={:?} n_ctx={:?}",
                    row.name, got.chat_url, got.wire_model, got.managed, got.cap_field, got.auth, got.n_ctx
                ));
            }
        }
        unsafe {
            std::env::remove_var(KEY_ENV);
            match prev_url {
                Some(v) => std::env::set_var("DARKMUX_LMSTUDIO_URL", v),
                None => std::env::remove_var("DARKMUX_LMSTUDIO_URL"),
            }
        }
        assert!(failures.is_empty(), "resolution moved:\n{}", failures.join("\n"));
    }
}
