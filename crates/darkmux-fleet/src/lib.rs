//! darkmux-fleet — fleet topology, the Redis work queue, the runner loop,
//! and dispatch routing. Split by concern into the `roster`, `queue`,
//! `runner`, and `routing` submodules (#508); this file is the crate facade.

mod queue;
mod roster;
mod routing;
mod runner;

pub use queue::*;
pub use roster::*;
pub use routing::*;
pub use runner::*;

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::path::PathBuf;

    // Module-private helpers under test, reached explicitly across the
    // post-#508 submodule split (they are pub(crate), not part of the
    // crate's public re-export surface).
    use crate::queue::{extract_field, parse_xreadgroup_response};
    use crate::roster::parse_address;
    use crate::routing::{
        completion_to_dispatch_result, match_completion, scan_flow_entries_for_completion,
    };

    // ─── completion_to_dispatch_result (Wave-E.6 #255) ────────────────

    fn completion(
        result_class: &str,
        payload: Option<serde_json::Value>,
    ) -> CompletionResult {
        CompletionResult {
            session_id: "test-sess".to_string(),
            result_class: result_class.to_string(),
            wall_ms: Some(1234),
            payload,
        }
    }

    #[test]
    fn completion_extracts_explicit_exit_code_from_payload() {
        // Runner emitted exit_code=42 (e.g. a build script's exit
        // code). Translation must surface it verbatim, NOT squash
        // to 1 via result_class.
        let c = completion(
            "error",
            Some(serde_json::json!({"result_class": "error", "exit_code": 42})),
        );
        let r = completion_to_dispatch_result(c);
        assert_eq!(
            r.exit_code, 42,
            "operator-facing exit code must match runner's"
        );
        assert!(
            r.stdout.contains("exit_code=42"),
            "stdout includes exit code"
        );
    }

    #[test]
    fn completion_extracts_zero_exit_code_even_on_ok() {
        let c = completion(
            "ok",
            Some(serde_json::json!({"result_class": "ok", "exit_code": 0})),
        );
        let r = completion_to_dispatch_result(c);
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn completion_falls_back_to_zero_on_ok_without_exit_code() {
        // Payload present but no exit_code field; result_class=ok →
        // fallback 0.
        let c = completion("ok", Some(serde_json::json!({"result_class": "ok"})));
        let r = completion_to_dispatch_result(c);
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn completion_falls_back_to_one_on_error_without_exit_code() {
        let c = completion("error", Some(serde_json::json!({"result_class": "error"})));
        let r = completion_to_dispatch_result(c);
        assert_eq!(r.exit_code, 1);
    }

    #[test]
    fn completion_falls_back_when_payload_absent() {
        let c = completion("error", None);
        let r = completion_to_dispatch_result(c);
        assert_eq!(r.exit_code, 1);
    }

    #[test]
    fn completion_passes_session_id_through() {
        let mut c = completion("ok", None);
        c.session_id = "mission-foo-sprint-bar-12345-0".to_string();
        let r = completion_to_dispatch_result(c);
        assert_eq!(r.session_id, "mission-foo-sprint-bar-12345-0");
        assert!(r.stdout.contains("mission-foo-sprint-bar-12345-0"));
    }

    #[test]
    fn completion_handles_negative_exit_code() {
        // SIGKILL-style exit codes can be negative (per std::process::ExitStatus).
        let c = completion(
            "error",
            Some(serde_json::json!({"result_class": "error", "exit_code": -9})),
        );
        let r = completion_to_dispatch_result(c);
        assert_eq!(r.exit_code, -9);
    }

    use tempfile::TempDir;

    fn with_roster_env<F: FnOnce(&PathBuf)>(f: F) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("fleet.json");
        let prev = std::env::var("DARKMUX_FLEET_FILE").ok();
        unsafe {
            std::env::set_var("DARKMUX_FLEET_FILE", &path);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&path)));
        match prev {
            Some(v) => unsafe {
                std::env::set_var("DARKMUX_FLEET_FILE", v);
            },
            None => unsafe {
                std::env::remove_var("DARKMUX_FLEET_FILE");
            },
        }
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    #[serial]
    fn load_missing_returns_empty_roster() {
        with_roster_env(|_| {
            let r = load_roster().unwrap();
            assert!(r.machines.is_empty());
            assert_eq!(r.version, "2");
        });
    }

    #[test]
    #[serial]
    fn add_then_load_round_trips() {
        with_roster_env(|_| {
            let mut r = FleetRoster::default();
            add_machine(&mut r, "studio", "100.74.208.36", Some("always-on m1 max")).unwrap();
            save_roster(&r).unwrap();

            let loaded = load_roster().unwrap();
            assert_eq!(loaded.machines.len(), 1);
            let entry = loaded.machines.get("studio").unwrap();
            assert_eq!(entry.address, "100.74.208.36");
            assert_eq!(entry.description.as_deref(), Some("always-on m1 max"));
            assert!(entry.added_unix_ms > 0);
        });
    }

    #[test]
    #[serial]
    fn add_preserves_added_ts_on_re_add() {
        // Idempotency: re-adding the same id mutates other fields but
        // preserves the original added_unix_ms. The roster's "fleet age"
        // signal stays honest.
        with_roster_env(|_| {
            let mut r = FleetRoster::default();
            add_machine(&mut r, "studio", "addr-1", None).unwrap();
            let first_added = r.machines.get("studio").unwrap().added_unix_ms;
            std::thread::sleep(std::time::Duration::from_millis(2));
            add_machine(&mut r, "studio", "addr-2", Some("updated desc")).unwrap();
            let entry = r.machines.get("studio").unwrap();
            assert_eq!(
                entry.added_unix_ms, first_added,
                "added_ts must be preserved"
            );
            assert_eq!(entry.address, "addr-2", "address must update");
            assert_eq!(entry.description.as_deref(), Some("updated desc"));
        });
    }

    #[test]
    fn add_rejects_empty_id() {
        let mut r = FleetRoster::default();
        let err = add_machine(&mut r, "", "addr", None).unwrap_err();
        assert!(err.to_string().contains("id must be non-empty"));
    }

    #[test]
    fn add_rejects_empty_address() {
        let mut r = FleetRoster::default();
        let err = add_machine(&mut r, "studio", "", None).unwrap_err();
        assert!(err.to_string().contains("address must be non-empty"));
    }

    #[test]
    fn remove_returns_entry_when_present() {
        let mut r = FleetRoster::default();
        add_machine(&mut r, "studio", "addr", None).unwrap();
        let removed = remove_machine(&mut r, "studio").expect("entry present");
        assert_eq!(removed.id, "studio");
        assert!(r.machines.is_empty());
    }

    #[test]
    fn remove_returns_none_when_absent() {
        let mut r = FleetRoster::default();
        assert!(remove_machine(&mut r, "ghost").is_none());
    }

    #[test]
    fn parse_address_handles_bare_ip() {
        // Bare IP gets DEFAULT_DAEMON_PORT appended.
        let a = parse_address("127.0.0.1").unwrap();
        assert_eq!(a.port(), DEFAULT_DAEMON_PORT);
    }

    #[test]
    fn parse_address_handles_ip_port() {
        let a = parse_address("127.0.0.1:9999").unwrap();
        assert_eq!(a.port(), 9999);
    }

    #[test]
    fn parse_address_rejects_empty() {
        assert!(parse_address("").is_err());
        assert!(parse_address("   ").is_err());
    }

    #[test]
    fn parse_address_returns_within_bounded_time_for_real_ip() {
        // Sanity for the Wave-E.10 DNS timeout wrapper: real IPs
        // resolve well under the 2s DNS_RESOLUTION_TIMEOUT cap and
        // certainly under 1s. Catches a regression where the
        // wrapper added ms-scale latency to the happy path.
        let start = std::time::Instant::now();
        let _ = parse_address("127.0.0.1:8765").expect("real IP resolves");
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "real-IP parse should be fast; took {elapsed:?}"
        );
    }

    #[test]
    fn parse_address_returns_bounded_for_invalid_format() {
        // A syntactically invalid input should bail fast (not wait the
        // full DNS_RESOLUTION_TIMEOUT). resolve_with_timeout converts
        // InvalidInput → Ok(None), so the caller's port-fallback path
        // runs; total bounded by 2 × DNS_RESOLUTION_TIMEOUT worst case.
        let start = std::time::Instant::now();
        let _ = parse_address("not::a::valid::addr");
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "invalid-format parse should not hang; took {elapsed:?}"
        );
    }

    #[test]
    fn parse_address_dns_timeout_is_bounded() {
        // Wave-E.10 invariant: even pathological-looking inputs
        // (e.g. a `.invalid` TLD per RFC 6761 — guaranteed NXDOMAIN)
        // must return within roughly DNS_RESOLUTION_TIMEOUT. The
        // resolver typically returns NXDOMAIN well under the cap;
        // this test asserts the WRAPPER bounds the worst case.
        let start = std::time::Instant::now();
        let _ = parse_address("definitely-not-a-real-hostname-12345.example.invalid");
        let elapsed = start.elapsed();
        // 2× DNS_RESOLUTION_TIMEOUT covers the host-then-host:port
        // double-attempt + scheduler jitter; still bounded.
        assert!(
            elapsed < std::time::Duration::from_secs(6),
            "DNS-failed parse should bounce within ~2 * DNS_RESOLUTION_TIMEOUT; took {elapsed:?}"
        );
    }

    #[test]
    fn probe_reachability_returns_true_for_listening_port() {
        // Bind a real listener on a free port; confirm probe sees it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let r = probe_reachability(&addr.to_string());
        assert!(
            r.reachable,
            "listening port must be reachable; got error: {:?}",
            r.error
        );
    }

    #[test]
    fn probe_reachability_returns_false_for_closed_port() {
        // Port 1 (tcpmux) is well-known and unbound on a normal system.
        let r = probe_reachability("127.0.0.1:1");
        assert!(!r.reachable);
        assert!(r.error.is_some());
    }

    #[test]
    fn probe_reachability_handles_unparseable() {
        let r = probe_reachability("not::a::valid::addr");
        assert!(!r.reachable);
        assert!(r.error.as_deref().unwrap().contains("unparseable"));
    }

    #[test]
    #[serial]
    fn save_roundtrip_preserves_pretty_json() {
        with_roster_env(|path| {
            let mut r = FleetRoster::default();
            add_machine(&mut r, "studio", "100.74.208.36:8765", None).unwrap();
            save_roster(&r).unwrap();
            let raw = std::fs::read_to_string(path).unwrap();
            // Pretty-print means newlines + indent — at least one newline.
            assert!(raw.contains('\n'), "expected pretty JSON; got: {raw}");
            assert!(raw.contains("\"studio\""));
        });
    }

    // ─── Work queue (PR-C.1) ──────────────────────────────────────────

    #[test]
    fn work_stream_is_single_global_stream() {
        // #590 — the per-tier stream prefix is gone; all fleet work
        // routes onto one stream.
        assert_eq!(crate::queue::WORK_STREAM, "darkmux:work");
    }

    #[test]
    fn work_job_serde_round_trips() {
        let job = build_work_job(
            Some("laptop".to_string()),
            "coder".to_string(),
            "implement the feature".to_string(),
            "session-2026-05-20-abc".to_string(),
            None,
            Some("/tmp/workspace".to_string()),
            None,
            darkmux_crew::dispatch::Runtime::Openclaw,
            600,
            Some("studio".to_string()),
            Some("claude-opus-4-7".to_string()),
        );
        let json = serde_json::to_string(&job).unwrap();
        let parsed: WorkJob = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, job);
        assert_eq!(parsed.attempt, 1, "new jobs publish with attempt=1");
        assert!(parsed.published_at_unix_ms > 0);
    }

    #[test]
    fn work_job_omits_none_fields_from_serialized() {
        // None-valued optional fields must be omitted from the wire form
        // so older runners (future-proof case) don't trip on
        // unexpected null values.
        let job = build_work_job(
            None, // target_machine None
            "scribe".to_string(),
            "draft a note".to_string(),
            "s-1".to_string(),
            None, // deliver None
            None, // workdir None
            None, // sprint_id None
            darkmux_crew::dispatch::Runtime::Openclaw,
            300,
            None, // published_by_machine None
            None, // published_by_orchestrator None
        );
        let json = serde_json::to_string(&job).unwrap();
        assert!(
            !json.contains("target_machine"),
            "None target_machine must be omitted: {json}"
        );
        assert!(
            !json.contains("deliver"),
            "None deliver must be omitted: {json}"
        );
        assert!(
            !json.contains("workdir"),
            "None workdir must be omitted: {json}"
        );
        assert!(
            !json.contains("sprint_id"),
            "None sprint_id must be omitted: {json}"
        );
        assert!(
            !json.contains("published_by_machine"),
            "None published_by_machine must be omitted: {json}"
        );
        assert!(
            !json.contains("published_by_orchestrator"),
            "None published_by_orchestrator must be omitted: {json}"
        );
    }

    /// Default runtime is `Internal` as of the runtime-default flip
    /// (in-house container-bounded Rust runtime is darkmux's default;
    /// openclaw shell-out stays as `--runtime openclaw` opt-in).
    /// A WorkJob with no `runtime` field serializes/deserializes
    /// against the Runtime enum's `#[default]` annotation.
    #[test]
    fn work_job_default_runtime_is_internal() {
        let json = r#"{
            "role_id": "scribe",
            "message": "hi",
            "session_id": "s-1",
            "timeout_seconds": 300,
            "published_at_unix_ms": 0,
            "attempt": 1
        }"#;
        let parsed: WorkJob = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.runtime, darkmux_crew::dispatch::Runtime::Internal);
    }

    /// Wave-E.14: lifting `runtime` from String to enum moves the
    /// "unknown runtime" check from `validate()` to JSON-parse time. A
    /// publisher that XADD's a job with `"runtime": "nuclear"` is
    /// rejected before the job ever enters the runner's WorkJob in-memory
    /// shape — the consumer's `serde_json::from_str` fails loud rather
    /// than going through validate.
    #[test]
    fn work_job_unknown_runtime_rejected_at_deserialize() {
        let json = r#"{
            "role_id": "scribe",
            "message": "hi",
            "session_id": "s-1",
            "runtime": "nuclear",
            "timeout_seconds": 300,
            "published_at_unix_ms": 0,
            "attempt": 1
        }"#;
        let err = serde_json::from_str::<WorkJob>(json)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("variant") || err.contains("runtime") || err.contains("nuclear"),
            "expected serde to name the unknown variant; got: {err}"
        );
    }

    /// Round-trip parity: Runtime::Openclaw serializes as "openclaw"
    /// (lowercase) and Runtime::Internal as "internal", matching the
    /// CLI-flag plumbing and the pre-enum String values so older
    /// JSON-on-disk records keep loading.
    #[test]
    fn runtime_enum_serdes_as_lowercase_string() {
        use darkmux_crew::dispatch::Runtime;
        let oc = serde_json::to_string(&Runtime::Openclaw).unwrap();
        assert_eq!(oc, "\"openclaw\"");
        let ic = serde_json::to_string(&Runtime::Internal).unwrap();
        assert_eq!(ic, "\"internal\"");
        let oc_back: Runtime = serde_json::from_str("\"openclaw\"").unwrap();
        assert_eq!(oc_back, Runtime::Openclaw);
        let ic_back: Runtime = serde_json::from_str("\"internal\"").unwrap();
        assert_eq!(ic_back, Runtime::Internal);
    }

    #[test]
    fn parse_xreadgroup_handles_nil() {
        // Timeout / no work case — Redis returns Nil.
        let result = parse_xreadgroup_response(&redis::Value::Nil).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parse_xreadgroup_handles_empty_bulk() {
        // Some redis-rs versions return Bulk(vec![]) for empty.
        let result = parse_xreadgroup_response(&redis::Value::Array(vec![])).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parse_xreadgroup_extracts_entry() {
        // Build the nested-array shape XREADGROUP returns:
        // [[stream_name, [[id, [k, v, k, v]]]]]
        use redis::Value as V;
        let job = build_work_job(
            None,
            "coder".to_string(),
            "do the thing".to_string(),
            "s-test".to_string(),
            None,
            None,
            None,
            darkmux_crew::dispatch::Runtime::Openclaw,
            600,
            None,
            None,
        );
        let job_json = serde_json::to_string(&job).unwrap();
        let entry_id = "1716192000000-0";
        let response = V::Array(vec![V::Array(vec![
            V::BulkString(b"darkmux:work".to_vec()),
            V::Array(vec![V::Array(vec![
                V::BulkString(entry_id.as_bytes().to_vec()),
                V::Array(vec![
                    V::BulkString(b"schema".to_vec()),
                    V::BulkString(b"1".to_vec()),
                    V::BulkString(b"record".to_vec()),
                    V::BulkString(job_json.as_bytes().to_vec()),
                ]),
            ])]),
        ])]);

        let claimed = parse_xreadgroup_response(&response).unwrap().unwrap();
        assert_eq!(claimed.work_id, entry_id);
        assert_eq!(claimed.job, job);
    }

    #[test]
    fn parse_xreadgroup_errors_on_missing_record_field() {
        use redis::Value as V;
        // Entry has fields but no `record` key — caller can't dispatch.
        let response = V::Array(vec![V::Array(vec![
            V::BulkString(b"darkmux:work".to_vec()),
            V::Array(vec![V::Array(vec![
                V::BulkString(b"1716192000000-0".to_vec()),
                V::Array(vec![
                    V::BulkString(b"schema".to_vec()),
                    V::BulkString(b"2".to_vec()),
                    // record field absent
                ]),
            ])]),
        ])]);
        let err = parse_xreadgroup_response(&response).unwrap_err();
        assert!(err.to_string().contains("missing `record`"));
    }

    #[test]
    fn extract_field_finds_value_by_key() {
        use redis::Value as V;
        let fields = vec![
            V::BulkString(b"schema".to_vec()),
            V::BulkString(b"1".to_vec()),
            V::BulkString(b"record".to_vec()),
            V::BulkString(b"{\"k\":\"v\"}".to_vec()),
        ];
        assert_eq!(extract_field(&fields, "schema").as_deref(), Some("1"));
        assert_eq!(
            extract_field(&fields, "record").as_deref(),
            Some("{\"k\":\"v\"}")
        );
        assert_eq!(extract_field(&fields, "absent"), None);
    }

    #[test]
    fn extract_field_handles_status_values() {
        // Some redis-rs versions return Status (SimpleString) for short
        // ASCII values.
        use redis::Value as V;
        let fields = vec![
            V::SimpleString("schema".to_string()),
            V::SimpleString("1".to_string()),
        ];
        assert_eq!(extract_field(&fields, "schema").as_deref(), Some("1"));
    }

    // ─── WorkJob::validate() (PR-C.2 boundary defense) ────────────────

    fn good_job() -> WorkJob {
        build_work_job(
            None,
            "coder".to_string(),
            "do a thing".to_string(),
            "s-1".to_string(),
            None,
            None,
            None,
            darkmux_crew::dispatch::Runtime::Openclaw,
            600,
            None,
            None,
        )
    }

    #[test]
    fn validate_accepts_well_formed_job() {
        assert!(good_job().validate().is_ok());
    }

    #[test]
    fn validate_rejects_path_traversal_in_role_id() {
        let mut j = good_job();
        j.role_id = "../../etc/passwd".to_string();
        let err = j.validate().unwrap_err().to_string();
        assert!(err.contains("invalid char") || err.contains("role_id"));
    }

    #[test]
    fn validate_rejects_uppercase_in_identifier() {
        let mut j = good_job();
        j.role_id = "Coder".to_string();
        assert!(j.validate().is_err());
    }

    #[test]
    fn validate_rejects_too_long_identifier() {
        let mut j = good_job();
        j.role_id = "a".repeat(MAX_WORK_IDENTIFIER_LEN + 1);
        let err = j.validate().unwrap_err().to_string();
        assert!(err.contains("exceeds") && err.contains("role_id"));
    }

    // Wave-E.14 (#255): `validate_rejects_unknown_runtime` /
    // `validate_rejects_empty_runtime` removed — `runtime` is now a
    // `Runtime` enum, so unknown/empty values are rejected at
    // `serde::Deserialize` time before the WorkJob exists. See
    // `work_job_unknown_runtime_rejected_at_deserialize` above.

    #[test]
    fn validate_rejects_oversize_message() {
        let mut j = good_job();
        j.message = "x".repeat(MAX_WORK_MESSAGE_BYTES + 1);
        let err = j.validate().unwrap_err().to_string();
        assert!(err.contains("message") && err.contains("exceeds"));
    }

    #[test]
    fn validate_accepts_message_at_cap() {
        let mut j = good_job();
        j.message = "x".repeat(MAX_WORK_MESSAGE_BYTES);
        assert!(j.validate().is_ok());
    }

    #[test]
    fn validate_rejects_oversize_workdir() {
        let mut j = good_job();
        j.workdir = Some("x".repeat(MAX_WORK_WORKDIR_BYTES + 1));
        let err = j.validate().unwrap_err().to_string();
        assert!(err.contains("workdir") && err.contains("exceeds"));
    }

    #[test]
    fn validate_rejects_target_machine_with_special_chars() {
        let mut j = good_job();
        j.target_machine = Some("studio$rm-rf".to_string());
        let err = j.validate().unwrap_err().to_string();
        assert!(err.contains("target_machine") || err.contains("invalid char"));
    }

    #[test]
    fn validate_accepts_target_machine_none() {
        let mut j = good_job();
        j.target_machine = None;
        assert!(j.validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_timeout() {
        let mut j = good_job();
        j.timeout_seconds = 0;
        let err = j.validate().unwrap_err().to_string();
        assert!(err.contains("timeout_seconds") && err.contains("non-zero"));
    }

    #[test]
    fn validate_rejects_oversize_timeout() {
        let mut j = good_job();
        j.timeout_seconds = MAX_WORK_TIMEOUT_SECONDS + 1;
        let err = j.validate().unwrap_err().to_string();
        assert!(err.contains("timeout_seconds") && err.contains("exceeds"));
    }

    #[test]
    fn validate_accepts_max_timeout() {
        let mut j = good_job();
        j.timeout_seconds = MAX_WORK_TIMEOUT_SECONDS;
        assert!(j.validate().is_ok());
    }

    // ─── match_completion (PR-C.3 --wait wrapper) ─────────────────────

    #[test]
    fn match_completion_matches_canonical_action() {
        // Canonical form today is "dispatch complete" (space) — every
        // production emit site uses this. PR-C.3 review HIGH-2 caught
        // the labels swapped in an earlier draft of this file.
        let line = r#"{
            "action": "dispatch complete",
            "session_id": "sess-A",
            "payload": {"result_class": "ok", "wall_ms": 12345}
        }"#;
        let result = match_completion(line, "sess-A").expect("matches");
        assert_eq!(result.session_id, "sess-A");
        assert_eq!(result.result_class, "ok");
        assert_eq!(result.wall_ms, Some(12345));
    }

    #[test]
    fn match_completion_matches_dotted_action_forward_compat() {
        // Forward-compat for a future emitter migration to the dotted
        // convention used by `dispatch.turn` / `dispatch.tool` / etc.
        // No production emit-site uses this today.
        let line = r#"{
            "action": "dispatch.complete",
            "session_id": "sess-B",
            "payload": {"result_class": "error"}
        }"#;
        let result = match_completion(line, "sess-B").expect("matches");
        assert_eq!(result.result_class, "error");
        assert_eq!(result.wall_ms, None);
    }

    #[test]
    fn match_completion_rejects_unrelated_session() {
        let line = r#"{
            "action": "dispatch complete",
            "session_id": "sess-A",
            "payload": {"result_class": "ok"}
        }"#;
        assert!(match_completion(line, "sess-B").is_none());
    }

    #[test]
    fn match_completion_rejects_dispatch_start() {
        let line = r#"{
            "action": "dispatch.start",
            "session_id": "sess-A"
        }"#;
        assert!(match_completion(line, "sess-A").is_none());
    }

    #[test]
    fn match_completion_handles_missing_payload() {
        let line = r#"{
            "action": "dispatch complete",
            "session_id": "sess-A"
        }"#;
        let result = match_completion(line, "sess-A").expect("matches");
        assert_eq!(result.result_class, "unknown");
        assert_eq!(result.wall_ms, None);
    }

    #[test]
    fn match_completion_ignores_malformed_line() {
        assert!(match_completion("not json", "sess-A").is_none());
        assert!(match_completion("{}", "sess-A").is_none());
        assert!(match_completion(r#"{"action": "dispatch complete"}"#, "sess-A").is_none());
    }

    // ─── scan_flow_entries_for_completion (PR-C.3 Redis-poll path) ────

    #[test]
    fn scan_flow_entries_handles_empty_stream() {
        // Empty XRANGE response = no entries yet, return None (not an error).
        let resp = redis::Value::Array(vec![]);
        let result = scan_flow_entries_for_completion(&resp, "sess-X").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn scan_flow_entries_handles_nil() {
        // Nil response (some redis-rs versions) — same as empty.
        let result = scan_flow_entries_for_completion(&redis::Value::Nil, "sess-X").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn scan_flow_entries_finds_completion_for_session() {
        use redis::Value as V;
        let record = r#"{
            "action": "dispatch complete",
            "session_id": "sess-target",
            "payload": {"result_class": "ok", "wall_ms": 5000}
        }"#;
        // Mock XRANGE response: Array([Array([id, Array([k,v,k,v])])])
        let resp = V::Array(vec![V::Array(vec![
            V::BulkString(b"1716192000000-0".to_vec()),
            V::Array(vec![
                V::BulkString(b"schema".to_vec()),
                V::BulkString(b"1.8.0".to_vec()),
                V::BulkString(b"record".to_vec()),
                V::BulkString(record.as_bytes().to_vec()),
            ]),
        ])]);
        let result = scan_flow_entries_for_completion(&resp, "sess-target").unwrap();
        let c = result.expect("matches");
        assert_eq!(c.session_id, "sess-target");
        assert_eq!(c.result_class, "ok");
        assert_eq!(c.wall_ms, Some(5000));
    }

    #[test]
    fn scan_flow_entries_skips_non_matching_sessions() {
        use redis::Value as V;
        let record_a = r#"{"action":"dispatch complete","session_id":"sess-A","payload":{"result_class":"ok"}}"#;
        let record_b = r#"{"action":"dispatch start","session_id":"sess-target"}"#;
        let resp = V::Array(vec![
            V::Array(vec![
                V::BulkString(b"1-0".to_vec()),
                V::Array(vec![
                    V::BulkString(b"record".to_vec()),
                    V::BulkString(record_a.as_bytes().to_vec()),
                ]),
            ]),
            V::Array(vec![
                V::BulkString(b"2-0".to_vec()),
                V::Array(vec![
                    V::BulkString(b"record".to_vec()),
                    V::BulkString(record_b.as_bytes().to_vec()),
                ]),
            ]),
        ]);
        let result = scan_flow_entries_for_completion(&resp, "sess-target").unwrap();
        // No `dispatch complete` for sess-target → None
        assert!(result.is_none());
    }

    // ─── #[serde(deny_unknown_fields)] (PR-C.2) ───────────────────────

    #[test]
    fn workjob_deserialize_rejects_unknown_field() {
        // A future-PR field smuggled by a malicious publisher must fail
        // to deserialize, not silently roundtrip.
        let json = r#"{
            "role_id": "coder",
            "message": "hi",
            "session_id": "s-1",
            "runtime": "openclaw",
            "timeout_seconds": 300,
            "published_at_unix_ms": 0,
            "attempt": 1,
            "future_priority_field": 999
        }"#;
        let result: Result<WorkJob, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "deny_unknown_fields must reject smuggled field; got: {:?}",
            result
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("future_priority_field") || err.contains("unknown field"),
            "error should name the unknown field: {err}"
        );
    }

    #[test]
    fn workjob_deserialize_accepts_known_fields_only() {
        // Sanity: the strict shape still accepts a valid job.
        let json = r#"{
            "role_id": "coder",
            "message": "hi",
            "session_id": "s-1",
            "runtime": "openclaw",
            "timeout_seconds": 300,
            "published_at_unix_ms": 0,
            "attempt": 1
        }"#;
        let parsed: WorkJob = serde_json::from_str(json).expect("valid job parses");
        assert_eq!(parsed.role_id, "coder");
        assert!(
            parsed.target_machine.is_none(),
            "advisory target_machine defaults to None when omitted"
        );
    }

    #[test]
    fn workjob_deserialize_rejects_legacy_target_tier_field() {
        // #590 wire break — the former `target_tier` routing key is gone.
        // A "1"-era publisher's job (carrying target_tier) must fail to
        // deserialize against the "2"-era shape rather than silently drop
        // the field. deny_unknown_fields enforces the non-interop.
        let json = r#"{
            "target_tier": "inference",
            "role_id": "coder",
            "message": "hi",
            "session_id": "s-1",
            "runtime": "openclaw",
            "timeout_seconds": 300,
            "published_at_unix_ms": 0,
            "attempt": 1
        }"#;
        let result: Result<WorkJob, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "legacy target_tier field must be rejected post-#590; got: {result:?}"
        );
    }
}
