//! darkmux-fleet — fleet topology (the roster), secure work submission
//! between machines (#2916), running a received job, and dispatch routing.
//! Split by concern into submodules (#508); this file is the crate facade.

mod identity;
mod identity_knowledge;
mod job;
mod peer;
mod roster;
mod routing;
mod runner;
mod seats;
mod submission;

pub use identity::*;
pub use identity_knowledge::*;
pub use job::*;
pub use peer::*;
pub use roster::*;
pub use routing::*;
pub use runner::*;
pub use seats::*;
pub use submission::*;

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::path::PathBuf;

    use crate::roster::parse_address;

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

    /// (#2916 re-review C7) Case-insensitive lookup: exact wins, one
    /// case-variant is found, two are an error, never a pick by map order.
    #[test]
    fn roster_lookup_is_case_insensitive_and_never_guesses() {
        let mut r = FleetRoster::default();
        add_machine(&mut r, "MacBook-Pro", "laptop", None, None).unwrap();
        assert_eq!(find_machine_key(&r, "macbook-pro").unwrap().as_deref(), Some("MacBook-Pro"));
        assert_eq!(find_machine(&r, "MACBOOK-PRO").unwrap().unwrap().address, "laptop");
        assert_eq!(find_machine_key(&r, "studio").unwrap(), None);
        add_machine(&mut r, "macbook-pro", "laptop2", None, None).unwrap();
        assert_eq!(find_machine_key(&r, "macbook-pro").unwrap().as_deref(), Some("macbook-pro"), "an exact key wins");
        assert!(find_machine_key(&r, "MACBOOK-PRO").is_err(), "two case-variants are ambiguous");
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

    /// (#2924 C-c) A field this binary does not know (a newer binary's
    /// `loopback_intended`, an operator's hand-added note) survives a
    /// load -> add -> save cycle, on the entry being updated and on every
    /// other entry. Without this, an older binary rewriting the roster
    /// silently dropped `loopback_intended`.
    #[test]
    #[serial]
    fn unknown_entry_fields_survive_a_rewrite() {
        with_roster_env(|path| {
            std::fs::write(
                path,
                r#"{"version":"2","machines":{
                    "a":{"id":"a","address":"a.example","added_unix_ms":1,"future_field":{"x":1}},
                    "b":{"id":"b","address":"b.example","added_unix_ms":2,"note":"hand-added"}}}"#,
            )
            .unwrap();
            mutate_roster(|r| add_machine(r, "a", "a2.example", None, None)).unwrap();
            let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            assert_eq!(raw["machines"]["a"]["future_field"]["x"], 1, "{raw}");
            assert_eq!(raw["machines"]["a"]["address"], "a2.example");
            assert_eq!(raw["machines"]["b"]["note"], "hand-added", "{raw}");
        });
    }

    #[test]
    #[serial]
    fn add_then_load_round_trips() {
        with_roster_env(|_| {
            let mut r = FleetRoster::default();
            add_machine(&mut r, "studio", "100.64.0.2", Some("always-on m1 max"), None).unwrap();
            save_roster(&r).unwrap();

            let loaded = load_roster().unwrap();
            assert_eq!(loaded.machines.len(), 1);
            let entry = loaded.machines.get("studio").unwrap();
            assert_eq!(entry.address, "100.64.0.2");
            assert_eq!(entry.description.as_deref(), Some("always-on m1 max"));
            assert!(entry.added_unix_ms > 0);
            assert_eq!(entry.machine_uid, None);
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
            add_machine(&mut r, "studio", "addr-1", None, None).unwrap();
            let first_added = r.machines.get("studio").unwrap().added_unix_ms;
            std::thread::sleep(std::time::Duration::from_millis(2));
            add_machine(&mut r, "studio", "addr-2", Some("updated desc"), None).unwrap();
            let entry = r.machines.get("studio").unwrap();
            assert_eq!(
                entry.added_unix_ms, first_added,
                "added_ts must be preserved"
            );
            assert_eq!(entry.address, "addr-2", "address must update");
            assert_eq!(entry.description.as_deref(), Some("updated desc"));
        });
    }

    // (#2768) `uid: Some(...)` always wins — the self-registration path
    // re-resolves this host's own identity fresh on every call.
    #[test]
    fn add_uid_some_overwrites_on_re_add() {
        let mut r = FleetRoster::default();
        add_machine(&mut r, "laptop", "127.0.0.1:8765", None, Some("UID-OLD")).unwrap();
        add_machine(&mut r, "laptop", "127.0.0.1:8765", None, Some("UID-NEW")).unwrap();
        assert_eq!(
            r.machines.get("laptop").unwrap().machine_uid.as_deref(),
            Some("UID-NEW")
        );
    }

    // (#2768, the "decide and document" case for a remote peer) `uid: None`
    // must NOT clobber a uid this entry already carried — a plain
    // `--description` update on a peer must not silently erase a prior
    // resolution or an operator's hand-edit of the roster JSON.
    #[test]
    fn add_uid_none_preserves_existing_uid() {
        let mut r = FleetRoster::default();
        add_machine(&mut r, "peer1", "100.64.0.2:8765", None, Some("UID-PEER")).unwrap();
        add_machine(&mut r, "peer1", "100.64.0.2:8765", Some("new desc"), None).unwrap();
        let entry = r.machines.get("peer1").unwrap();
        assert_eq!(entry.machine_uid.as_deref(), Some("UID-PEER"));
        assert_eq!(entry.description.as_deref(), Some("new desc"));
    }

    // Inverted case: a fresh entry with `uid: None` throughout (the ordinary
    // remote-peer shape) stays `None` — nothing invents a value.
    #[test]
    fn add_uid_none_on_fresh_entry_stays_none() {
        let mut r = FleetRoster::default();
        add_machine(&mut r, "peer1", "100.64.0.2:8765", None, None).unwrap();
        assert_eq!(r.machines.get("peer1").unwrap().machine_uid, None);
    }

    #[test]
    fn add_rejects_empty_id() {
        let mut r = FleetRoster::default();
        let err = add_machine(&mut r, "", "addr", None, None).unwrap_err();
        assert!(err.to_string().contains("id must be non-empty"));
    }

    #[test]
    fn add_rejects_empty_address() {
        let mut r = FleetRoster::default();
        let err = add_machine(&mut r, "studio", "", None, None).unwrap_err();
        assert!(err.to_string().contains("address must be non-empty"));
    }

    #[test]
    fn remove_returns_entry_when_present() {
        let mut r = FleetRoster::default();
        add_machine(&mut r, "studio", "addr", None, None).unwrap();
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
            add_machine(&mut r, "studio", "100.64.0.2:8765", None, None).unwrap();
            save_roster(&r).unwrap();
            let raw = std::fs::read_to_string(path).unwrap();
            // Pretty-print means newlines + indent — at least one newline.
            assert!(raw.contains('\n'), "expected pretty JSON; got: {raw}");
            assert!(raw.contains("\"studio\""));
        });
    }

    // ─── WorkJob::validate() (PR-C.2 boundary defense) ────────────────

    fn good_job() -> WorkJob {
        build_work_job(
            "studio".to_string(),
            "coder".to_string(),
            "do a thing".to_string(),
            "s-1".to_string(),
            None,
            None,
            None,
            None,
            600,
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

    // (#1426 ship-3) The `runtime` field (a single-variant enum after
    // #1405/#1409) retired entirely in the WORK_JOB_SCHEMA_VERSION 3 to 4
    // bump. A pre-4 peer's `runtime` key is now an unknown field, see
    // `work_job_retired_runtime_key_rejected_at_deserialize` and the
    // version-first claim gate tested in `parse_xreadgroup_version_mismatch_*`.

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
        j.target_machine = "studio$rm-rf".to_string();
        let err = j.validate().unwrap_err().to_string();
        assert!(err.contains("target_machine") || err.contains("invalid char"));
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

    // ─── #[serde(deny_unknown_fields)] (PR-C.2) ───────────────────────

    #[test]
    fn workjob_deserialize_rejects_unknown_field() {
        // A future-PR field smuggled by a malicious publisher must fail
        // to deserialize, not silently roundtrip.
        let json = r#"{
            "role_id": "coder",
            "message": "hi",
            "session_id": "s-1",
            "timeout_seconds": 300,
            "published_at_unix_ms": 0,
            "target_machine": "studio",
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
            "timeout_seconds": 300,
            "published_at_unix_ms": 0,
            "target_machine": "studio"
        }"#;
        let parsed: WorkJob = serde_json::from_str(json).expect("valid job parses");
        assert_eq!(parsed.role_id, "coder");
        assert_eq!(parsed.target_machine, "studio");
        // (#2916) The job is addressed: `target_machine` is required.
        let unaddressed = json.replace(r#","target_machine": "studio""#, "").replace(",\n            \"target_machine\": \"studio\"", "");
        assert!(serde_json::from_str::<WorkJob>(&unaddressed).is_err(), "{unaddressed}");
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
            "timeout_seconds": 300,
            "published_at_unix_ms": 0,
            "target_machine": "studio"
        }"#;
        let result: Result<WorkJob, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "legacy target_tier field must be rejected post-#590; got: {result:?}"
        );
    }
}
