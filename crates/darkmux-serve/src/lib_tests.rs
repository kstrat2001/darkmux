    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use std::{fs, path::PathBuf};
    use tower::util::ServiceExt;
    use tempfile::TempDir;

    /// XSS guard, layer 1: the viewer must contain ZERO inline event-handler
    /// attributes. All clicks route through one delegated listener reading
    /// `data-act`/`data-arg` (dataset values are plain strings — no JS-string
    /// context for a malicious flow-record identifier to break out of). A new
    /// `onclick="…"` carrying a record-derived value is exactly the injection
    /// shape this PR removed; fail the build on any reintroduction. JS-side
    /// property assignment (`$("play").onclick=…`) is fine and not matched.
    #[test]
    fn viewer_has_no_inline_event_handlers() {
        let html = include_str!("../assets/viewer.html");
        // Scan the WHOLE inline-handler attribute class, not a fixed name list:
        // an HTML `on<event>=` attribute is leading whitespace, `on`, lowercase
        // letters, optional whitespace, `=`, then a quote. Matches onclick/
        // ontoggle/oninput/onkeydown/onsubmit/… alike, so reintroducing ANY of
        // them fails the build. (`$("x").onclick=` JS property assignment is NOT
        // matched — it has no leading-whitespace `on…="` attribute shape.)
        let bytes = html.as_bytes();
        for (i, w) in bytes.windows(3).enumerate() {
            if matches!(w[0], b' ' | b'\t' | b'\n') && w[1] == b'o' && w[2] == b'n' {
                let mut j = i + 3;
                while j < bytes.len() && bytes[j].is_ascii_lowercase() {
                    j += 1;
                }
                let mut k = j;
                while k < bytes.len() && matches!(bytes[k], b' ' | b'\t') {
                    k += 1;
                }
                if j > i + 3 && bytes.get(k) == Some(&b'=') {
                    let mut q = k + 1;
                    while q < bytes.len() && matches!(bytes[q], b' ' | b'\t') {
                        q += 1;
                    }
                    if matches!(bytes.get(q), Some(b'"') | Some(b'\'')) {
                        let attr = String::from_utf8_lossy(&bytes[i + 1..j]);
                        panic!(
                            "viewer.html contains inline event handler `{attr}=…` \
                             (byte {i}) — use data-act/data-arg + the delegated \
                             listener instead (XSS hardening)"
                        );
                    }
                }
            }
        }
        // No `javascript:` URL sinks either (an href/src carrying a
        // record-derived scheme would dodge the attribute scan above).
        assert!(
            !html.to_lowercase().contains("javascript:"),
            "viewer.html contains a javascript: URL — not allowed (XSS hardening)"
        );
    }

    /// XSS guard, layer 2: tripwire for raw (unescaped) interpolation of
    /// record-derived values into rendered templates. Curated needles — each
    /// is an exact pattern this hardening pass replaced with an `esc()`-wrapped
    /// form; matching one means an escape was dropped. NOT a proof of safety
    /// (string construction that's escaped downstream is legitimate and
    /// unmatched) — the proof is review + the malicious fixture walkthrough
    /// (tests/fixtures/xss-flow.jsonl).
    #[test]
    fn viewer_has_no_raw_record_interpolations() {
        let html = include_str!("../assets/viewer.html");
        for needle in [
            "${missionLabel()}", "${DATA_SOURCE}", "${nameOf(", "${specOf(",
            "${m}", "${sid}", "${pm}", "${mid}", "${q}", "${a}",
            "${role}", "${handle}", "${model}", "${mission}", "${tag}", "${turns}",
            "${state.machine}", "${state.session}",
            "${s.handle}", "${s.model}", "${s.mission_id}", "${s.role}", "${s.machine}",
            "${n.sid}", "${n.handle}", "${n.model}", "${n.mission}",
            "${r.category}", "${r.machine_id}", "${r.session_id}",
            "${f.k}", "${f.d}", "${f.f}", "${f.s}",
        ] {
            assert!(
                !html.contains(needle),
                "viewer.html interpolates `{needle}` without esc() — wrap it \
                 (record-derived values must be escaped at the template edge)"
            );
        }
    }

    /// (#794) The live SSE tail must be idempotent — a re-delivered record
    /// (reconnect / snapshot-stream overlap; the stream is at-least-once) must
    /// not be re-counted, or cumulative readouts (the savings hero) inflate
    /// past the persisted truth and "reset" on refresh. Lock the dedup so it
    /// can't be removed without this failing.
    #[test]
    fn live_tail_dedups_records() {
        let html = include_str!("../assets/viewer.html");
        assert!(
            html.contains("const SEEN_KEYS=new Set();") && html.contains("const recKey="),
            "viewer.html lost the live-tail dedup primitives (SEEN_KEYS / recKey)"
        );
        // The SSE onmessage handler must consult SEEN_KEYS before RAW.push so a
        // re-delivered record is dropped rather than re-counted.
        let onmsg = html
            .split("LIVE_ES.onmessage")
            .nth(1)
            .expect("viewer.html has no LIVE_ES.onmessage handler");
        let guard = onmsg.find("if(SEEN_KEYS.has(k))return;");
        let push = onmsg.find("RAW.push(rec);");
        assert!(
            matches!((guard, push), (Some(g), Some(p)) if g < p),
            "the live SSE onmessage must guard on SEEN_KEYS.has(k) BEFORE RAW.push(rec) \
             (idempotent append — #794)"
        );
    }

    /// (#803) The savings hero's token-class breakdown + hybrid note. Locks:
    /// (a) the educational class labels exist (generated / fresh / re-read —
    /// the decomposition IS the feature), (b) the hint stays TOKENS-ONLY
    /// (names rate classes, supplies no currency or rate figure), (c) the
    /// hybrid note's record-derived mission id is esc()-wrapped.
    #[test]
    fn savings_hero_breakdown_is_classed_and_currency_free() {
        let html = include_str!("../assets/viewer.html");
        for needle in ["generated", "fresh input", "re-read", "unclassified"] {
            assert!(
                html.contains(needle),
                "savings hero lost the `{needle}` token-class label (#803)"
            );
        }
        assert!(
            html.contains("${esc(mr.mission_id)}"),
            "hybridNote must esc() the mission id"
        );
        assert!(
            !html.contains("${mr.mission_id}"),
            "hybridNote interpolates mission_id without esc()"
        );
        // (#807) The real orchestrator-note channel: the tagged-note lookup
        // must exist, and note text (orchestrator-authored free text) must be
        // esc()-wrapped both on the hero line and in the history modal.
        assert!(
            html.contains("r.source===\"orchestrator\"&&!r.session_id"),
            "viewer lost the orchestrator-note source filter or its mission-level \
             (no session_id) restriction — session-scoped notes are adjudication-\
             shaped and never belong on the card (#807/#819)"
        );
        for (escaped, raw) in [
            ("${esc(last.handle)}", "${last.handle}"),
            ("${esc(n.handle)}", "${n.handle}"),
        ] {
            assert!(
                html.contains(escaped),
                "orchestrator note text lost its esc() wrap (`{escaped}`)"
            );
            assert!(
                !html.contains(raw),
                "orchestrator note text interpolated raw (`{raw}`)"
            );
        }
        assert!(
            html.contains("id=\"nmodalbg\"") && html.contains("data-act=\"notes\""),
            "notes-history modal or its dispatcher hook is missing (#807)"
        );
        // Tokens-only invariant: no currency or rate figure anywhere in the
        // hero region (hybridNote through the end of savingsHero, bounded by
        // the next top-level function) — the operator prices each class
        // themselves. Region-based extraction so a template reshuffle can't
        // silently shrink the scanned text (QA finding on the first cut).
        let start = html
            .find("function hybridNote(")
            .expect("viewer.html lost hybridNote");
        let end = html[start..]
            .find("function renderFleet(")
            .map(|i| start + i)
            .expect("renderFleet no longer follows the hero region");
        let hero = &html[start..end];
        for sym in ["USD", "$0", "$1", "€", "£", "/M", "per million"] {
            assert!(
                !hero.contains(sym),
                "savings hero must not carry a currency/rate figure (`{sym}`) — tokens only"
            );
        }
    }

    /// (#1387) The what-changed panel renders daemon-derived strings (the
    /// worktree path, the base ref) — it must stay output-encoded at the
    /// template edge and must never fetch outside live mode (playback and
    /// the static demo have no daemon). Lock both invariants in source so a
    /// refactor can't quietly drop them. Replaces the retired #756
    /// `diff_panel_is_live_gated_and_escaped` (the live-diff panel it
    /// covered no longer exists).
    #[test]
    fn wt_sum_panel_is_live_gated_and_escaped() {
        let html = include_str!("../assets/viewer.html");
        // The esc() implementation the assertions below depend on must itself
        // exist — if a refactor drops it, every wrapped interpolation throws
        // rather than escaping (QA finding, #756 viewer review).
        assert!(
            html.contains("const esc="),
            "viewer.html lost the esc() helper the output-encoding invariant rests on"
        );
        let panel = html
            .split("function wtSumPanel(")
            .nth(1)
            .expect("viewer.html lost the #1387 wtSumPanel function");
        assert!(
            panel.contains("if(!document.body.classList.contains('live-mode'))return \"\";"),
            "wtSumPanel must early-return outside live mode (static demo / playback never fetch)"
        );
        // Every daemon-derived value must pass through esc() — the base ref
        // label, the worktree path (both the `<code>` text and the zed href,
        // which is additionally URI-encoded first), and the numeric totals
        // (numbers today, but the panel's invariant is esc-at-the-template-
        // edge for everything daemon-derived — no unescaped drift).
        for needle in [
            "${esc(d.base)}",
            "${esc(path)}",
            "${esc(encodeURI(path))}",
            "${esc(files)}",
            "${esc(adds)}",
            "${esc(dels)}",
        ] {
            assert!(
                panel.contains(needle),
                "wtSumPanel lost the esc() wrap on `{needle}` — daemon-derived \
                 values must be output-encoded"
            );
        }
        for raw in ["${d.base}", "${path}", "${encodeURI(path)}", "${files}", "${adds}", "${dels}"] {
            assert!(
                !panel.contains(raw),
                "wtSumPanel interpolates `{raw}` without esc()"
            );
        }
    }

    /// The XSS regression fixture must stay a valid FLOW_SCHEMA day file —
    /// the manual walkthrough (copy to ~/.darkmux/flows/2026-01-01.jsonl,
    /// `darkmux serve`, open /play/2026-01-01, assert window.__xss is
    /// undefined) only proves anything if the records take the real ingest
    /// path rather than being skipped as malformed.
    #[test]
    fn xss_fixture_is_schema_valid() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/xss-flow.jsonl"
        );
        let raw = fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
        let mut n = 0;
        for (i, line) in raw.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            serde_json::from_str::<darkmux_flow::FlowRecord>(line).unwrap_or_else(|e| {
                panic!(
                    "xss-flow.jsonl line {} is not a valid FlowRecord: {e}\n  {line}",
                    i + 1
                )
            });
            n += 1;
        }
        assert!(n >= 10, "xss fixture suspiciously small ({n} records)");
        // The payloads themselves must be present, or the walkthrough is a no-op.
        assert!(raw.contains("window.__xss"), "fixture lost its XSS payloads");
        assert!(raw.contains("onerror="), "fixture lost its HTML-injection payloads");
    }

    /// The darkmux.com/demo dataset is a *real* FLOW_SCHEMA flow file: the demo
    /// page is the canonical viewer in playback mode loading this `.jsonl`,
    /// identical to a local `/play`. Guard that it stays an exact `FlowRecord`
    /// set — if a line drifts off-schema the demo would render wrong (or the
    /// playback path would choke), so it must deserialize line-for-line.
    #[test]
    fn demo_flow_dataset_is_schema_valid() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/demo/demo-flow.jsonl");
        let raw = fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
        let mut recs = Vec::new();
        for (i, line) in raw.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let r: darkmux_flow::FlowRecord = serde_json::from_str(line).unwrap_or_else(|e| {
                panic!("demo-flow.jsonl line {} is not a valid FlowRecord: {e}\n  {line}", i + 1)
            });
            recs.push(r);
        }
        assert!(!recs.is_empty(), "demo dataset is empty");
        // The demo flare is data-driven: the mission is named `demo`, so the
        // viewer's crumb reads `◆ demo` with zero viewer special-casing.
        assert!(
            recs.iter().any(|r| r.mission_id.as_deref() == Some("demo")),
            "demo dataset must carry mission_id=demo (the data-driven demo flare)"
        );
        assert!(
            recs.iter()
                .all(|r| r.mission_id.as_deref().map_or(true, |m| m == "demo")),
            "every mission-scoped demo record must be mission=demo"
        );
        let machines: std::collections::BTreeSet<_> =
            recs.iter().filter_map(|r| r.machine_id.as_deref()).collect();
        assert!(
            machines.len() >= 3,
            "demo should showcase a multi-machine fleet, got {machines:?}"
        );
    }

    #[tokio::test]
    async fn health_returns_200_with_versions() {
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);

        let bytes = to_bytes(response.into_body(), 1024)
            .await
            .unwrap();

        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(!json["darkmux_version"].as_str().unwrap().is_empty());
        assert!(!json["flow_schema_version"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn root_serves_viewer_html() {
        // #554: GET / serves the observability viewer (same-origin host
        // for the viewer's /flow/:date fetches).
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let ct = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(ct.starts_with("text/html"), "content-type was `{ct}`");

        let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        let lower = body.to_lowercase();
        assert!(
            lower.contains("<!doctype") || lower.contains("<html"),
            "GET / body is not HTML"
        );
        // #624 follow-up: GET / injects darkmux-mode=live so the viewer's
        // boot() knows to start the SSE tail. No darkmux-date meta on the
        // live route — the viewer falls back to URL-driven targetDate().
        assert!(
            body.contains(r#"<meta name="darkmux-mode" content="live">"#),
            "live meta absent from GET / body"
        );
        assert!(
            !body.contains(r#"<meta name="darkmux-date""#),
            "live route should not inject a date meta"
        );
    }

    #[tokio::test]
    async fn play_serves_viewer_html_with_playback_meta() {
        // #624 follow-up: GET /play/<date> serves the same viewer HTML with
        // darkmux-mode=play + darkmux-date=<date> so boot() skips the SSE
        // tail and stays in scrubber-replay mode for the captured day.
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/play/2026-06-03")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains(r#"<meta name="darkmux-mode" content="play">"#),
            "play meta absent from GET /play/<date> body"
        );
        assert!(
            body.contains(r#"<meta name="darkmux-date" content="2026-06-03">"#),
            "date meta absent from GET /play/<date> body"
        );
    }

    #[tokio::test]
    async fn play_rejects_malformed_date_with_404() {
        // The handler delegates date validation to `is_valid_date`, which
        // checks SHAPE (10 chars, dashes at positions 4 and 7, digits
        // elsewhere) but not calendar values. "2026-13-99" passes the shape
        // check; the daemon returns 200 + empty viewer for it because the
        // resulting /flow/<date> lookup returns no records — that's
        // acceptable. We test only shape-rejection cases here.
        let app = build_router(PathBuf::new());
        for bad in ["garbage", "26-6-3", "2026/06/03", "2026-06-3", "2026-6-03"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/play/{bad}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "expected 404 for /play/{bad}"
            );
        }
    }

    #[test]
    fn inject_mode_meta_inserts_after_head_open_tag() {
        let html = "<!doctype html><html><head>\n<title>x</title></head><body></body></html>";
        let live = inject_mode_meta(html, "live", None);
        assert!(live.contains(r#"<meta name="darkmux-mode" content="live">"#));
        assert!(!live.contains(r#"<meta name="darkmux-date""#));
        // Order matters: meta must come AFTER <head> open tag, BEFORE <title>
        let head_pos = live.find("<head>").unwrap();
        let meta_pos = live.find(r#"<meta name="darkmux-mode""#).unwrap();
        let title_pos = live.find("<title>").unwrap();
        assert!(head_pos < meta_pos && meta_pos < title_pos);

        let play = inject_mode_meta(html, "play", Some("2026-06-03"));
        assert!(play.contains(r#"<meta name="darkmux-mode" content="play">"#));
        assert!(play.contains(r#"<meta name="darkmux-date" content="2026-06-03">"#));
    }

    #[tokio::test]
    async fn root_route_does_not_shadow_api_routes() {
        // Adding GET / must not collide with the noun-prefixed API routes.
        // `/health` has no external deps, so a clean 200 here proves the
        // router still dispatches the API surface after `/` was added.
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "/health regressed after adding GET /");
    }

    #[tokio::test]
    async fn fleet_sessions_live_returns_json_array() {
        // GET /fleet/sessions/live (#638) must always answer 200 with a JSON
        // array — `[]` when Redis is unset/unreachable (CI), the live-session
        // beats when it is. The live viewer keys "running" on this, so it must
        // never 500 on a Redis blip. Asserting "200 + is_array" is robust in
        // both environments (content depends on whether a dispatch is live).
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/fleet/sessions/live")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body.is_array(), "live-sessions response must be a JSON array");
    }

    #[tokio::test]
    async fn fleet_machines_live_returns_json_array() {
        // GET /fleet/machines/live (#638) — the same robust contract as the
        // sessions endpoint: always 200 + JSON array (`[]` when Redis is
        // unset/unreachable, the presence beats when it is). The live viewer
        // unions this into machines(), so a Redis blip must degrade to "no
        // live machines", never a 500.
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/fleet/machines/live")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body.is_array(), "live-machines response must be a JSON array");
    }

    #[tokio::test]
    async fn machine_status_returns_200_with_structured_body() {
        // The handler calls into `lms::list_loaded()`, which shells out to
        // the `lms` binary. CI runners don't have it on PATH, so we expect
        // `lms_unreachable: true` and `models: []` rather than a 500. This
        // is the contract the `darkmux machine status <id>` peer read relies
        // on — degraded state shows up as a visible signal, not as a fetch
        // error. (#1426 — route renamed from /model/status.)
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/machine/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);

        let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        // Structural assertions — operator-facing fields the viewer reads.
        assert!(json.get("models").is_some(), "missing `models` array");
        assert!(json["models"].is_array());
        assert!(
            json.get("lms_unreachable").is_some(),
            "missing `lms_unreachable` flag"
        );
        assert!(json["lms_unreachable"].is_boolean());
        let generated = json["generated_at_ms"]
            .as_u64()
            .expect("`generated_at_ms` must be a u64 epoch-millis");
        // Sanity: timestamp should be after 2024 and before year 2100 — a
        // wide check, just to ensure it's actually populated.
        assert!(generated > 1_700_000_000_000);
        assert!(generated < 4_000_000_000_000);
    }

    // ─── #275 PR-A: /machine/specs endpoint ──────────────────────────

    /// GET /machine/specs returns a JSON object with the local machine's
    /// versioned spec sheet — version + machine_id + RAM + CPU + OS
    /// + loaded models + redacted Redis URL. Tested at the contract level
    /// rather than the value level so the test doesn't depend on the
    /// machine actually running it.
    #[tokio::test]
    #[serial_test::serial]
    async fn machine_specs_endpoint_returns_versioned_payload() {
        // Defensive — make sure no env from another test bleeds in.
        unsafe {
            std::env::remove_var("DARKMUX_REDIS_URL");
            std::env::remove_var("DARKMUX_MACHINE_ID");
        }
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/machine/specs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "expected 200 from /machine/specs");

        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON body");
        // Top-level contract — fields are present (values may be null on
        // non-macOS / missing-lms runners, but the keys must exist).
        for key in [
            "darkmux_version",
            "flow_schema_version",
            "machine_id",
            "os",
            "ram_total_bytes",
            "ram_free_for_ai_bytes",
            "cpu_brand",
            "loaded_models",
            "redis_url_redacted",
            "generated_at_ms",
        ] {
            assert!(
                json.get(key).is_some(),
                "/machine/specs response missing key `{key}`: {json}"
            );
        }
        // `darkmux_version` must equal the build's CARGO_PKG_VERSION.
        assert_eq!(
            json["darkmux_version"].as_str(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        // `loaded_models` is an array even when lms is unreachable.
        assert!(json["loaded_models"].is_array());
    }

    // ─── #1286: /machine/resources endpoint (the memory-ledger lens feed) ───

    /// GET /machine/resources returns the memory ledger with the fields the
    /// machine lens reads, plus the recorded cadence knob (`cache_ttl_ms`)
    /// and the observer-cost stamp (`gather_ms`) — the #1286 observer-effect
    /// constraints made visible in the payload. `lms` is pointed at a
    /// nonexistent binary so the test NEVER calls the operator's real
    /// LMStudio (and with no ls entries the arch reader opens no files);
    /// probe failures must degrade to `warnings`, never a 500.
    #[tokio::test]
    #[serial_test::serial]
    async fn machine_memory_endpoint_returns_ledger_with_observer_stamps() {
        let prev = std::env::var("DARKMUX_LMS_BIN").ok();
        unsafe {
            std::env::set_var("DARKMUX_LMS_BIN", "/nonexistent/darkmux-serve-test-lms");
        }
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/machine/resources")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_LMS_BIN", v),
                None => std::env::remove_var("DARKMUX_LMS_BIN"),
            }
        }
        assert_eq!(response.status(), StatusCode::OK, "expected 200 from /machine/resources");
        let bytes = to_bytes(response.into_body(), 256 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON body");
        for key in [
            "schema_version",
            "generated_at_ms",
            "gather_ms",
            "limit_bytes",
            "limit_source",
            "pool",
            "pressure",
            "models",
            "machine",
            "attribution",
            "attribution_note",
            "warnings",
            "cache_ttl_ms",
        ] {
            assert!(
                json.get(key).is_some(),
                "/machine/resources response missing key `{key}`: {json}"
            );
        }
        assert!(json["models"].is_array());
        assert!(json["machine"]["state"].is_string());
        // The nonexistent lms degrades loud — the payload documents it.
        assert!(
            json["warnings"].as_array().is_some_and(|w| !w.is_empty()),
            "missing-lms probes must surface as warnings: {json}"
        );
        assert_eq!(json["cache_ttl_ms"].as_u64(), Some(2_000));
    }

    /// /machine/specs MUST redact the Redis URL's password. Wide-open
    /// redaction is already in place via `flow::RawRedisUrl` (#216); this
    /// test pins that the new endpoint doesn't bypass it.
    #[tokio::test]
    #[serial_test::serial]
    async fn machine_specs_endpoint_redacts_redis_password() {
        unsafe {
            std::env::set_var("DARKMUX_REDIS_URL", "redis://user:s3cr3t-p4ss@example.com:6379");
        }
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/machine/specs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        unsafe { std::env::remove_var("DARKMUX_REDIS_URL"); }

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body_str = String::from_utf8_lossy(&bytes).into_owned();
        // The literal password must NOT appear anywhere in the body.
        assert!(
            !body_str.contains("s3cr3t-p4ss"),
            "Redis password leaked through /machine/specs: {body_str}"
        );
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let redacted = json["redis_url_redacted"]
            .as_str()
            .expect("redis_url_redacted must be a string when DARKMUX_REDIS_URL is set");
        // Sanity: redaction surface should mention example.com (it's the
        // host, not the secret) so the operator can confirm WHICH Redis
        // is being targeted without exposing creds.
        assert!(
            redacted.contains("example.com"),
            "redacted form should keep the host visible: {redacted}"
        );
    }

    /// /machine/specs reports operator-stamped provenance — `machine_id`
    /// from `DARKMUX_MACHINE_ID` (default: hostname). The former
    /// `machine_tier` field was dropped in #590 (machine-capacity tier
    /// no longer routes work).
    #[tokio::test]
    #[serial_test::serial]
    async fn machine_specs_endpoint_reports_machine_id_from_env() {
        unsafe {
            std::env::set_var("DARKMUX_MACHINE_ID", "test-laptop");
        }
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/machine/specs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        unsafe {
            std::env::remove_var("DARKMUX_MACHINE_ID");
        }
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            json["machine_id"].as_str(),
            Some("test-laptop"),
            "machine_id env not surfaced: {json}"
        );
        assert!(
            json.get("machine_tier").is_none(),
            "machine_tier must be absent post-#590: {json}"
        );
    }

    #[tokio::test]
    async fn flow_returns_empty_array_for_missing_date() {
        // Post-#701: a missing day is an empty JSON array (200), not a 404 —
        // "flow records for date X" is an empty collection, not an absent
        // resource. (The old `.jsonl` route returned 404 here; it's retired.)
        let tmp = TempDir::new().unwrap();
        let app = build_router(tmp.path().to_path_buf());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/flow/2999-01-01")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(bytes.as_ref(), b"[]");
    }

    #[tokio::test]
    async fn flow_since_filters_records_inclusively() {
        // (#1173) The live viewer's incremental reconcile passes `?since=<iso-ts>`
        // so a long-open tab stops re-parsing the whole day. Flow ts are uniform
        // second-precision UTC, so the filter is a plain lexical `>=` (inclusive).
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("2026-07-03.jsonl"),
            "{\"ts\":\"2026-07-03T00:00:10Z\",\"action\":\"a\",\"session_id\":\"S\"}\n\
             {\"ts\":\"2026-07-03T00:00:20Z\",\"action\":\"b\",\"session_id\":\"S\"}\n\
             {\"ts\":\"2026-07-03T00:00:30Z\",\"action\":\"c\",\"session_id\":\"S\"}\n",
        )
        .unwrap();
        let app = build_router(tmp.path().to_path_buf());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/flow/2026-07-03?since=2026-07-03T00:00:20Z")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
        let recs: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        let actions: Vec<&str> = recs.iter().filter_map(|r| r["action"].as_str()).collect();
        assert_eq!(actions, vec!["b", "c"], "since is inclusive; the earlier record drops");
    }

    #[tokio::test]
    async fn flow_without_since_returns_all_records() {
        // No `?since=` → the whole day, unchanged (backward-compatible).
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("2026-07-03.jsonl"),
            "{\"ts\":\"2026-07-03T00:00:10Z\",\"action\":\"a\"}\n\
             {\"ts\":\"2026-07-03T00:00:20Z\",\"action\":\"b\"}\n",
        )
        .unwrap();
        let app = build_router(tmp.path().to_path_buf());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/flow/2026-07-03")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
        let recs: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(recs.len(), 2, "no since → all records returned");
    }

    #[tokio::test]
    async fn flow_days_lists_days_newest_first_with_summaries() {
        // #691: the catalog endpoint scans the flows dir and summarizes each
        // day so the viewer's picker can offer historical playback by date.
        let tmp = TempDir::new().unwrap();
        // Two days; one with a mission + a dispatch, one bare. A schema header
        // line must not count as a record. A non-date file is ignored.
        fs::write(
            tmp.path().join("2026-05-14.jsonl"),
            "{\"_type\":\"schema\",\"version\":\"1.0.0\"}\n\
             {\"action\":\"dispatch.start\",\"handle\":\"coder\",\"session_id\":\"S1\",\"mission_id\":\"demo\"}\n\
             {\"action\":\"dispatch.turn\",\"handle\":\"coder\",\"session_id\":\"S1\",\"mission_id\":\"demo\"}\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("2026-05-12.jsonl"),
            "{\"action\":\"note\",\"handle\":\"x\"}\n",
        )
        .unwrap();
        fs::write(tmp.path().join("not-a-day.jsonl"), "garbage\n").unwrap();
        fs::write(tmp.path().join("README.txt"), "ignore me\n").unwrap();

        let app = build_router(tmp.path().to_path_buf());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/flow-days")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let days = json["days"].as_array().expect("days array");

        assert_eq!(days.len(), 2, "two valid date files (non-date files ignored)");
        // Newest first.
        assert_eq!(days[0]["date"], "2026-05-14");
        assert_eq!(days[1]["date"], "2026-05-12");
        // Summary: schema header excluded → 2 records; mission + 1 dispatch.
        assert_eq!(days[0]["records"], 2);
        assert_eq!(days[0]["dispatches"], 1);
        assert_eq!(days[0]["missions"], serde_json::json!(["demo"]));
        assert_eq!(days[1]["records"], 1);
        assert_eq!(days[1]["dispatches"], 0);
        assert_eq!(days[1]["missions"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn flow_days_empty_dir_is_empty_array() {
        let tmp = TempDir::new().unwrap();
        let app = build_router(tmp.path().to_path_buf());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/flow-days")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["days"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn flow_missions_rolls_up_a_mission_across_days() {
        // #691: a mission spanning two day files is ONE catalog entry, not two.
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("2026-05-12.jsonl"),
            "{\"_type\":\"schema\",\"version\":\"1.0.0\"}\n\
             {\"action\":\"dispatch.start\",\"session_id\":\"S1\",\"mission_id\":\"m1\",\"machine_id\":\"studio\",\"ts\":\"2026-05-12T10:00:00Z\"}\n\
             {\"action\":\"dispatch.turn\",\"session_id\":\"S1\",\"mission_id\":\"m1\",\"machine_id\":\"studio\",\"ts\":\"2026-05-12T10:01:00Z\"}\n",
        ).unwrap();
        fs::write(
            tmp.path().join("2026-05-14.jsonl"),
            "{\"action\":\"dispatch.start\",\"session_id\":\"S2\",\"mission_id\":\"m1\",\"machine_id\":\"laptop\",\"ts\":\"2026-05-14T09:00:00Z\"}\n\
             {\"action\":\"dispatch.turn\",\"session_id\":\"S2\",\"mission_id\":\"m1\",\"machine_id\":\"laptop\",\"ts\":\"2026-05-14T09:01:00Z\"}\n\
             {\"action\":\"dispatch.start\",\"session_id\":\"S3\",\"mission_id\":\"m2\",\"machine_id\":\"studio\",\"ts\":\"2026-05-14T09:05:00Z\"}\n",
        ).unwrap();

        let app = build_router(tmp.path().to_path_buf());
        let response = app
            .oneshot(Request::builder().uri("/flow-missions").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let missions = json["missions"].as_array().expect("missions array");
        assert_eq!(missions.len(), 2, "two distinct missions");
        let m1 = missions.iter().find(|m| m["mission_id"] == "m1").expect("m1 present");
        assert_eq!(m1["records"], 4, "m1's records span BOTH day files");
        assert_eq!(m1["dispatches"], 2, "S1 + S2");
        assert_eq!(m1["first_date"], "2026-05-12");
        assert_eq!(m1["last_date"], "2026-05-14");
        assert_eq!(m1["first_ts"], "2026-05-12T10:00:00Z");
        assert_eq!(m1["last_ts"], "2026-05-14T09:01:00Z");
        assert_eq!(m1["machines"], serde_json::json!(["laptop", "studio"]));
        // Newest activity first: m2's last dispatch (09:05) is newer than m1's (09:01).
        assert_eq!(missions[0]["mission_id"], "m2");
    }

    #[tokio::test]
    async fn flow_mission_returns_records_across_days_chronologically() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("2026-05-12.jsonl"),
            "{\"session_id\":\"S1\",\"mission_id\":\"m1\",\"ts\":\"2026-05-12T10:00:00Z\"}\n",
        ).unwrap();
        fs::write(
            tmp.path().join("2026-05-14.jsonl"),
            "{\"session_id\":\"S2\",\"mission_id\":\"m1\",\"ts\":\"2026-05-14T09:00:00Z\"}\n\
             {\"session_id\":\"S3\",\"mission_id\":\"m2\",\"ts\":\"2026-05-14T09:05:00Z\"}\n",
        ).unwrap();
        let app = build_router(tmp.path().to_path_buf());
        let response = app
            .oneshot(Request::builder().uri("/flow-mission/m1").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let records = json["records"].as_array().expect("records array");
        assert_eq!(records.len(), 2, "both m1 records across days; m2 excluded");
        assert_eq!(json["truncated"], false);
        // Chronological: the 05-12 record precedes the 05-14 record.
        assert_eq!(records[0]["ts"], "2026-05-12T10:00:00Z");
        assert_eq!(records[1]["ts"], "2026-05-14T09:00:00Z");
    }

    #[tokio::test]
    async fn flow_session_returns_only_that_sessions_records() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("2026-05-12.jsonl"),
            "{\"session_id\":\"S1\",\"mission_id\":\"m1\",\"ts\":\"2026-05-12T10:00:00Z\"}\n\
             {\"session_id\":\"S2\",\"mission_id\":\"m1\",\"ts\":\"2026-05-12T10:05:00Z\"}\n",
        ).unwrap();
        let app = build_router(tmp.path().to_path_buf());
        let response = app
            .oneshot(Request::builder().uri("/flow-session/S1").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let records = json["records"].as_array().expect("records array");
        assert_eq!(records.len(), 1, "only S1 (S2 excluded)");
        assert_eq!(records[0]["session_id"], "S1");
    }

    #[tokio::test]
    async fn flow_catalog_rejects_invalid_id() {
        let tmp = TempDir::new().unwrap();
        let app = build_router(tmp.path().to_path_buf());
        let long = "a".repeat(200); // > 128-char cap → invalid
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/flow-mission/{long}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn flow_returns_400_for_malformed_date() {
        let tmp = TempDir::new().unwrap();
        let app = build_router(tmp.path().to_path_buf());

        // Non-date shape → rejected by the format validator.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/flow/not-a-date")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // A trailing `.jsonl` is no longer a recognized shape (route retired
        // in #701) — it fails the bare-date validator → 400.
        let response2 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/flow/2026-05-14.jsonl")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response2.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn flow_returns_records_as_json_array_when_present() {
        // The bare-date route reads the local `<date>.jsonl` file (Redis
        // unset) and returns its records as a JSON array.
        // (#811) The config tier is empty by construction in this test build
        // (the test-support dev-dep), so config.json can't supply a Redis URL.
        // The ENV tier is separate: drop DARKMUX_REDIS_URL (serial-guarded so it
        // can't race the sibling tests that set it) so redis_url() is "off" and
        // the handler reads the tmp local file instead of the operator's Redis.
        unsafe { std::env::remove_var("DARKMUX_REDIS_URL"); }
        let tmp = TempDir::new().unwrap();
        let content = "{\"_type\":\"schema\",\"version\":\"1.0.0\"}\n{\"action\":\"x\",\"handle\":\"test\"}\n";
        fs::write(tmp.path().join("2026-05-14.jsonl"), content).unwrap();

        let app = build_router(tmp.path().to_path_buf());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/flow/2026-05-14")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let ct = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(ct.starts_with("application/json"), "content-type was `{ct}`");

        let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
        let arr: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // Every non-empty JSON line is parsed (schema header + the record);
        // the viewer's adapter ignores non-record entries downstream.
        let items = arr.as_array().expect("expected a JSON array");
        assert_eq!(items.len(), 2, "schema header line + one record");
        assert!(
            items.iter().any(|r| r["handle"] == "test"),
            "the dispatch record should be present: {arr}"
        );
    }

    /// Localhost-origin requests get CORS headers (topology viewer on any
    /// local port can read the response).
    #[tokio::test]
    /// Post-#270/#273: localhost dev-server origins are NOT allowed by
    /// default. The same browser tab that runs an operator's localhost
    /// app should not be able to exfiltrate fleet-wide flow data via
    /// CORS. Operators with a legitimate dev-server-origin need set
    /// `DARKMUX_DAEMON_CORS_ORIGINS` to opt that origin in.
    ///
    /// `#[serial]` because the sibling `cors_allows_origin_in_env_override`
    /// sets `DARKMUX_DAEMON_CORS_ORIGINS` and the default-deny assertion
    /// races against it under cargo's parallel runner. (Race surfaced
    /// post-merge in CI on main; not flagged on PR-level CI.)
    #[serial_test::serial]
    async fn cors_denies_localhost_origin_by_default() {
        // Defensive — make sure no inherited env from another test changes the default behavior.
        unsafe { std::env::remove_var("DARKMUX_DAEMON_CORS_ORIGINS"); }
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("Origin", "http://localhost:5173")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(
            !response.headers().contains_key("access-control-allow-origin"),
            "localhost origin must NOT receive CORS headers by default (#273)"
        );
    }

    /// 127.0.0.1 variant — same as localhost; denied by default post-#273.
    /// `#[serial]` for the same reason as `cors_denies_localhost_origin_by_default`.
    #[tokio::test]
    #[serial_test::serial]
    async fn cors_denies_loopback_ip_origin_by_default() {
        unsafe { std::env::remove_var("DARKMUX_DAEMON_CORS_ORIGINS"); }
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("Origin", "http://127.0.0.1:8765")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(
            !response.headers().contains_key("access-control-allow-origin"),
            "127.0.0.1 origin must NOT receive CORS headers by default (#273)"
        );
    }

    /// Operator-opt-in: `DARKMUX_DAEMON_CORS_ORIGINS` extends the
    /// default null-only allowlist. Comma-separated. Origins in the
    /// list receive CORS headers. (#273)
    #[tokio::test]
    #[serial_test::serial]
    async fn cors_allows_origin_in_env_override() {
        unsafe {
            std::env::set_var(
                "DARKMUX_DAEMON_CORS_ORIGINS",
                "http://localhost:5173,http://localhost:3000",
            );
        }
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("Origin", "http://localhost:5173")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        unsafe { std::env::remove_var("DARKMUX_DAEMON_CORS_ORIGINS"); }

        assert!(
            response.headers().contains_key("access-control-allow-origin"),
            "origin in DARKMUX_DAEMON_CORS_ORIGINS must receive CORS headers (#273)"
        );
    }

    // ─── (#881) serve daemon auth ─────────────────────────────────────
    // Under test-support the config tier is empty (#811), so a serve token
    // resolves ONLY from the DARKMUX_SERVE_TOKEN env var — set/scrub it,
    // #[serial] to avoid racing other env-mutating tests.

    const TEST_TOKEN: &str = "sek-test-12345";
    fn remote_peer() -> ConnectInfo<SocketAddr> {
        ConnectInfo("10.0.0.9:5555".parse::<SocketAddr>().unwrap())
    }

    /// (#1387) `/worktree-summary/:session_id` (the numbers-only replacement
    /// for the retired `/diff/:session_id`) must ride the SAME remote-only
    /// bearer gate as the rest of the read surface — there is no longer a
    /// route-specific always-on gate anywhere in this router. Mirrors
    /// `lab_runs_requires_token_from_remote_peer`.
    #[tokio::test]
    #[serial_test::serial]
    async fn worktree_summary_requires_token_from_remote_peer() {
        unsafe { std::env::set_var("DARKMUX_SERVE_TOKEN", TEST_TOKEN); }
        let app = build_router(PathBuf::new());
        let mut req = Request::builder()
            .uri("/worktree-summary/some-session")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(remote_peer());
        let resp = app.oneshot(req).await.unwrap();
        unsafe { std::env::remove_var("DARKMUX_SERVE_TOKEN"); }
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// Companion to the above: unlike the retired `/diff` gate,
    /// `/worktree-summary` is open on LOOPBACK even when a token is
    /// configured — a oneshot request has no `ConnectInfo` and is treated
    /// as loopback, same as every other route on the general gate.
    #[tokio::test]
    #[serial_test::serial]
    async fn worktree_summary_open_on_loopback_even_with_token() {
        unsafe { std::env::set_var("DARKMUX_SERVE_TOKEN", TEST_TOKEN); }
        let app = build_router(PathBuf::new());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/worktree-summary/some-session")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        unsafe { std::env::remove_var("DARKMUX_SERVE_TOKEN"); }
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn flow_open_on_loopback_even_with_token() {
        // The router-wide gate exempts loopback peers; a oneshot request has no
        // ConnectInfo and is treated as loopback.
        unsafe { std::env::set_var("DARKMUX_SERVE_TOKEN", TEST_TOKEN); }
        let app = build_router(PathBuf::new());
        let resp = app
            .oneshot(Request::builder().uri("/flow-days").body(Body::empty()).unwrap())
            .await
            .unwrap();
        unsafe { std::env::remove_var("DARKMUX_SERVE_TOKEN"); }
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn flow_requires_token_from_remote_peer() {
        unsafe { std::env::set_var("DARKMUX_SERVE_TOKEN", TEST_TOKEN); }
        let app = build_router(PathBuf::new());
        let mut req = Request::builder().uri("/flow-days").body(Body::empty()).unwrap();
        req.extensions_mut().insert(remote_peer());
        let resp = app.oneshot(req).await.unwrap();
        unsafe { std::env::remove_var("DARKMUX_SERVE_TOKEN"); }
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// (#1247 Part 3 / #1262 review round) The lab routes must ride the SAME
    /// remote-only bearer gate as the rest of the read surface — guards
    /// against a future refactor special-casing `/lab/*` out of `auth_mw`
    /// (they serve local run artifacts, no less sensitive than flow records).
    #[tokio::test]
    #[serial_test::serial]
    async fn lab_runs_requires_token_from_remote_peer() {
        unsafe { std::env::set_var("DARKMUX_SERVE_TOKEN", TEST_TOKEN); }
        let app = build_router(PathBuf::new());
        let mut req = Request::builder().uri("/lab/runs").body(Body::empty()).unwrap();
        req.extensions_mut().insert(remote_peer());
        let resp = app.oneshot(req).await.unwrap();
        unsafe { std::env::remove_var("DARKMUX_SERVE_TOKEN"); }
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn flow_accepts_remote_peer_with_token() {
        unsafe { std::env::set_var("DARKMUX_SERVE_TOKEN", TEST_TOKEN); }
        let app = build_router(PathBuf::new());
        let mut req = Request::builder()
            .uri("/flow-days")
            .header("Authorization", format!("Bearer {TEST_TOKEN}"))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(remote_peer());
        let resp = app.oneshot(req).await.unwrap();
        unsafe { std::env::remove_var("DARKMUX_SERVE_TOKEN"); }
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn health_exempt_from_remote_gate() {
        unsafe { std::env::set_var("DARKMUX_SERVE_TOKEN", TEST_TOKEN); }
        let app = build_router(PathBuf::new());
        let mut req = Request::builder().uri("/health").body(Body::empty()).unwrap();
        req.extensions_mut().insert(remote_peer());
        let resp = app.oneshot(req).await.unwrap();
        unsafe { std::env::remove_var("DARKMUX_SERVE_TOKEN"); }
        // /health must answer even an unauthenticated remote peer (doctor's
        // reachability probe + external liveness checks).
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn bind_gate_allows_loopback_without_token() {
        assert!(bind_requires_token("127.0.0.1", false).is_ok());
        assert!(bind_requires_token("::1", false).is_ok());
        assert!(bind_requires_token("127.0.0.5", false).is_ok());
    }

    #[test]
    fn bind_gate_refuses_nonloopback_without_token() {
        assert!(bind_requires_token("0.0.0.0", false).is_err());
        assert!(bind_requires_token("100.64.0.5", false).is_err()); // a Tailnet IP
        assert!(bind_requires_token("192.168.1.10", false).is_err());
        assert!(bind_requires_token("::", false).is_err()); // IPv6 unspecified
        assert!(bind_requires_token("2001:db8::1", false).is_err()); // global IPv6
    }

    #[test]
    fn bind_gate_allows_nonloopback_with_token() {
        assert!(bind_requires_token("0.0.0.0", true).is_ok());
        assert!(bind_requires_token("100.64.0.5", true).is_ok());
    }

    #[test]
    fn bind_gate_treats_unparseable_as_nonloopback() {
        // A bind string that isn't an IP is conservatively non-loopback.
        assert!(bind_requires_token("garbage", false).is_err());
        assert!(bind_requires_token("garbage", true).is_ok());
    }

    #[test]
    fn tokens_match_is_exact() {
        assert!(tokens_match(b"abc", b"abc"));
        assert!(!tokens_match(b"abc", b"abd"));
        assert!(!tokens_match(b"abc", b"abcd")); // length differs
        assert!(!tokens_match(b"", b"x"));
    }

    // ─── #294 try_send_or_drop_newest ─────────────────────────────────

    /// Bounded channel + producer outpaces consumer → SendOutcome::
    /// Dropped returned; the records that fit make it through. Pre-
    /// #294, the channel was unbounded and would grow indefinitely.
    #[tokio::test]
    async fn try_send_or_drop_newest_drops_when_channel_full() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(2);

        assert_eq!(
            try_send_or_drop_newest(&tx, "a".to_string()),
            SendOutcome::Sent
        );
        assert_eq!(
            try_send_or_drop_newest(&tx, "b".to_string()),
            SendOutcome::Sent
        );
        // Capacity is 2; third send should drop (newest-dropped).
        assert_eq!(
            try_send_or_drop_newest(&tx, "c".to_string()),
            SendOutcome::Dropped
        );
        assert_eq!(
            try_send_or_drop_newest(&tx, "d".to_string()),
            SendOutcome::Dropped
        );

        // The consumer pulls — gets the records that fit (a, b);
        // c and d were dropped (drop-newest semantics).
        assert_eq!(rx.recv().await, Some("a".to_string()));
        assert_eq!(rx.recv().await, Some("b".to_string()));
    }

    /// Receiver dropped → SendOutcome::Closed returned.
    #[tokio::test]
    async fn try_send_or_drop_newest_signals_closed_on_receiver_drop() {
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(16);
        drop(rx); // consumer leaves
        assert_eq!(
            try_send_or_drop_newest(&tx, "post-drop".to_string()),
            SendOutcome::Closed
        );
    }

    /// Consumer that drains keeps up — no drops.
    #[tokio::test]
    async fn try_send_or_drop_newest_no_drops_when_consumer_keeps_up() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(2);
        for i in 0..10 {
            assert_eq!(
                try_send_or_drop_newest(&tx, format!("r{i}")),
                SendOutcome::Sent
            );
            // Consumer drains immediately after each send.
            let _ = rx.recv().await;
        }
    }

    // ─── #288 normalize trailing-slash + case + reject wildcard ─────────

    /// Operator types `http://localhost:5173/` (trailing slash, what
    /// you copy out of a browser address bar) → must allow the
    /// browser's actual Origin `http://localhost:5173` (no trailing
    /// slash). Pre-#288 this was a silent deny.
    #[tokio::test]
    #[serial_test::serial]
    async fn cors_env_normalizes_trailing_slash_in_origin() {
        unsafe {
            std::env::set_var(
                "DARKMUX_DAEMON_CORS_ORIGINS",
                "http://localhost:5173/", // trailing slash
            );
        }
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("Origin", "http://localhost:5173") // no slash
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        unsafe { std::env::remove_var("DARKMUX_DAEMON_CORS_ORIGINS"); }

        assert!(
            response.headers().contains_key("access-control-allow-origin"),
            "trailing slash in env entry should still allow the canonical Origin (#288)"
        );
    }

    /// Operator types `HTTP://Localhost:5173` (uppercase scheme+host)
    /// → must allow the browser's lowercase Origin. Pre-#288 silent
    /// deny.
    #[tokio::test]
    #[serial_test::serial]
    async fn cors_env_normalizes_uppercase_scheme_and_host() {
        unsafe {
            std::env::set_var(
                "DARKMUX_DAEMON_CORS_ORIGINS",
                "HTTP://Localhost:5173",
            );
        }
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("Origin", "http://localhost:5173")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        unsafe { std::env::remove_var("DARKMUX_DAEMON_CORS_ORIGINS"); }

        assert!(
            response.headers().contains_key("access-control-allow-origin"),
            "uppercase scheme+host in env entry should still allow the canonical Origin (#288)"
        );
    }

    /// Literal `*` in the env value is non-functional (browsers don't
    /// send `*` as Origin) — the parser MUST reject it with a stderr
    /// warning so operators who paste a wildcard don't think it works.
    /// The end-to-end behavior: a request with arbitrary external
    /// Origin must still be denied.
    #[tokio::test]
    #[serial_test::serial]
    async fn cors_env_rejects_literal_wildcard() {
        unsafe { std::env::set_var("DARKMUX_DAEMON_CORS_ORIGINS", "*"); }
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("Origin", "https://malicious.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        unsafe { std::env::remove_var("DARKMUX_DAEMON_CORS_ORIGINS"); }

        assert!(
            !response.headers().contains_key("access-control-allow-origin"),
            "literal `*` in env must NOT wildcard-allow arbitrary origins (#288)"
        );
    }

    // ─── parse_cors_origins / normalize_cors_origin unit tests ──────────

    #[test]
    fn normalize_cors_origin_strips_trailing_slash() {
        assert_eq!(
            normalize_cors_origin("http://localhost:5173/"),
            "http://localhost:5173"
        );
        assert_eq!(
            normalize_cors_origin("http://localhost:5173"),
            "http://localhost:5173"
        );
    }

    #[test]
    fn normalize_cors_origin_lowercases_scheme_and_host() {
        assert_eq!(
            normalize_cors_origin("HTTP://Localhost:5173"),
            "http://localhost:5173"
        );
        assert_eq!(
            normalize_cors_origin("HTTPS://Example.COM"),
            "https://example.com"
        );
    }

    /// Realistic operator paste-from-browser-bar shape: both case +
    /// trailing slash. Each transform tested independently above;
    /// this guards the combined case (which is what operators
    /// actually paste).
    #[test]
    fn normalize_cors_origin_handles_uppercase_and_trailing_slash_together() {
        assert_eq!(
            normalize_cors_origin("HTTP://Localhost:5173/"),
            "http://localhost:5173"
        );
    }

    #[test]
    fn normalize_cors_origin_handles_no_scheme() {
        // Edge case: operator types just `localhost:5173` without scheme.
        // Defensible: lowercase as a string.
        assert_eq!(normalize_cors_origin("Localhost:5173"), "localhost:5173");
    }

    #[test]
    fn parse_cors_origins_filters_empty_and_normalizes() {
        let result = parse_cors_origins("HTTP://Localhost:5173/,, http://other:3000 ,");
        assert_eq!(result.len(), 2);
        assert!(result.contains(&"http://localhost:5173".to_string()));
        assert!(result.contains(&"http://other:3000".to_string()));
    }

    #[test]
    fn parse_cors_origins_rejects_literal_wildcard() {
        let result = parse_cors_origins("*,http://localhost:5173");
        assert_eq!(result, vec!["http://localhost:5173".to_string()]);
        // Wildcard entry filtered; non-wildcard entry preserved + normalized.
    }

    #[test]
    fn parse_cors_origins_empty_returns_empty_vec() {
        assert!(parse_cors_origins("").is_empty());
        assert!(parse_cors_origins("   ").is_empty());
        assert!(parse_cors_origins(",,,").is_empty());
    }

    /// Negative side of the env-override: a localhost port NOT in the
    /// override list still gets denied.
    #[tokio::test]
    #[serial_test::serial]
    async fn cors_denies_localhost_port_not_in_env_override() {
        unsafe {
            std::env::set_var(
                "DARKMUX_DAEMON_CORS_ORIGINS",
                "http://localhost:5173",
            );
        }
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("Origin", "http://localhost:9999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        unsafe { std::env::remove_var("DARKMUX_DAEMON_CORS_ORIGINS"); }

        assert!(
            !response.headers().contains_key("access-control-allow-origin"),
            "localhost port outside the env override list must still be denied (#273)"
        );
    }

    /// `null` origin = topology viewer opened directly from disk (file://).
    #[tokio::test]
    async fn cors_allows_file_protocol_origin() {
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("Origin", "null")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(
            response.headers().contains_key("access-control-allow-origin"),
            "file:// (null) origin must receive CORS headers for the topology viewer"
        );
    }

    /// Arbitrary external origins must NOT get CORS headers — this is the
    /// cross-origin exfiltration guard (#225). The response body is still
    /// returned (CORS is browser-enforced); the missing header causes the
    /// browser to block the script from reading it.
    #[tokio::test]
    async fn cors_denies_external_origin() {
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("Origin", "https://malicious.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(
            !response.headers().contains_key("access-control-allow-origin"),
            "external origin must NOT receive CORS headers"
        );
    }

    // ─── SSE / tail_lines tests ─────────────────────────────────────────
    //
    // Tests assert against `tail_lines` (the testable core of `build_tail_stream`)
    // so they verify actual line content. The thin `Event::default().data(...)`
    // wrapper in `build_tail_stream` is small enough that visual review suffices.

    use futures::StreamExt;
    use std::time::Duration;

    /// Pop the next line off a stream within `timeout`. Returns None if
    /// the stream produced nothing in the window.
    async fn next_line<S>(stream: &mut S, timeout: Duration) -> Option<String>
    where
        S: futures::Stream<Item = String> + Unpin,
    {
        tokio::time::timeout(timeout, stream.next()).await.ok().flatten()
    }

    #[tokio::test]
    async fn tail_stream_emits_appended_lines() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.jsonl");
        std::fs::File::create(&path).unwrap();

        let stream = tail_lines(path.clone(), 0);
        tokio::pin!(stream);

        // Append a line after the stream started.
        let written = r#"{"action":"hi"}"#;
        std::fs::write(&path, format!("{written}\n")).unwrap();

        let got = next_line(&mut stream, Duration::from_millis(1500)).await;
        assert_eq!(got.as_deref(), Some(written), "expected appended line verbatim");
    }

    #[tokio::test]
    async fn tail_stream_starts_from_current_eof_not_replay() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.jsonl");
        std::fs::write(&path, "{\"old\":1}\n{\"old\":2}\n{\"old\":3}\n").unwrap();

        let start = std::fs::metadata(&path).unwrap().len();
        let stream = tail_lines(path.clone(), start);
        tokio::pin!(stream);

        // 600ms of polling should yield nothing — old lines are below start_offset.
        let got = next_line(&mut stream, Duration::from_millis(600)).await;
        assert!(got.is_none(), "tail-from-current must not replay history; got {got:?}");

        // Append a new line — only the new line should be emitted.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        writeln!(file, "{{\"new\":1}}").unwrap();
        drop(file);

        let got2 = next_line(&mut stream, Duration::from_millis(1500)).await;
        assert_eq!(got2.as_deref(), Some(r#"{"new":1}"#));
    }

    #[tokio::test]
    async fn tail_stream_handles_missing_file_gracefully() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("not-yet.jsonl");
        assert!(!path.exists());

        let stream = tail_lines(path.clone(), 0);
        tokio::pin!(stream);

        // Create the file with content after the stream starts polling.
        let path2 = path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            std::fs::write(&path2, "{\"late\":1}\n").unwrap();
        });

        let got = next_line(&mut stream, Duration::from_millis(2000)).await;
        assert_eq!(got.as_deref(), Some(r#"{"late":1}"#));
    }

    #[tokio::test]
    async fn tail_stream_emits_multiple_lines_from_single_append() {
        // Regression: ensure pending VecDeque drains across multiple
        // unfold iterations — three lines in one write should produce
        // three sequential events.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.jsonl");
        std::fs::File::create(&path).unwrap();

        let stream = tail_lines(path.clone(), 0);
        tokio::pin!(stream);

        std::fs::write(&path, "a\nb\nc\n").unwrap();

        assert_eq!(
            next_line(&mut stream, Duration::from_millis(1500)).await.as_deref(),
            Some("a")
        );
        assert_eq!(
            next_line(&mut stream, Duration::from_millis(1500)).await.as_deref(),
            Some("b")
        );
        assert_eq!(
            next_line(&mut stream, Duration::from_millis(1500)).await.as_deref(),
            Some("c")
        );
    }

    #[tokio::test]
    async fn stream_returns_400_for_malformed_date() {
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/flow/not-a-date/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    // The is_addr_reachable / nudge probe tests moved with the helpers to
    // `darkmux_flow::daemon_probe` (#463 cycle-break).

    fn sample_addr() -> std::net::SocketAddr {
        "127.0.0.1:8765".parse().expect("addr parses")
    }

    #[test]
    fn startup_banner_contains_core_info() {
        let flows = PathBuf::from("/tmp/darkmux-flows-banner-test");
        let missions = PathBuf::from("/tmp/darkmux-missions-banner-test");
        let phases = PathBuf::from("/tmp/darkmux-phases-banner-test");
        let lines = build_startup_banner(
            &sample_addr(), &flows, true, &missions, true, &phases, true, 3, 9, None,
        );

        // Title carries the binary version that operators bump via cargo install.
        let joined = lines.join("\n");
        assert!(joined.contains("darkmux serve · v"), "title line present: {joined}");
        assert!(joined.contains("viewer:"), "viewer line present");
        assert!(
            joined.contains("http://127.0.0.1:8765/"),
            "viewer shows the addr with trailing slash"
        );
        assert!(joined.contains("flow schema:"), "schema line present");
        assert!(joined.contains(darkmux_flow::FLOW_SCHEMA_VERSION), "schema version shown");
        assert!(joined.contains("/tmp/darkmux-flows-banner-test"), "flows dir shown");
        assert!(joined.contains("missions:       3 loaded"), "mission count rendered");
        assert!(joined.contains("phases:        9 loaded"), "phase count rendered");
        assert!(joined.contains("ready"), "ready line present");
        assert!(joined.contains("Ctrl-C"), "Ctrl-C hint present");
    }

    #[test]
    fn startup_banner_warns_on_missing_flows_dir() {
        let flows = PathBuf::from("/tmp/darkmux-banner-missing-flows");
        let missions = PathBuf::from("/tmp/darkmux-banner-present-missions");
        let phases = PathBuf::from("/tmp/darkmux-banner-present-phases");
        let lines = build_startup_banner(
            &sample_addr(), &flows, false, &missions, true, &phases, true, 0, 0, None,
        );
        let joined = lines.join("\n");
        assert!(
            joined.contains("flows dir doesn't exist yet"),
            "expected flows-dir warning; got: {joined}"
        );
        assert!(
            !joined.contains("missions dir not found"),
            "should not warn about missions when missions dir exists"
        );
        assert!(
            !joined.contains("phases dir not found"),
            "should not warn about phases when phases dir exists"
        );
    }

    #[test]
    fn startup_banner_warns_on_missing_missions_dir() {
        let flows = PathBuf::from("/tmp/darkmux-banner-present-flows");
        let missions = PathBuf::from("/tmp/darkmux-banner-missing-missions");
        let phases = PathBuf::from("/tmp/darkmux-banner-present-phases");
        let lines = build_startup_banner(
            &sample_addr(), &flows, true, &missions, false, &phases, true, 0, 0, None,
        );
        let joined = lines.join("\n");
        assert!(
            joined.contains("missions dir not found"),
            "expected missions warning; got: {joined}"
        );
        assert!(
            joined.contains("/tmp/darkmux-banner-missing-missions"),
            "missions warning should include the path"
        );
        assert!(
            !joined.contains("phases dir not found"),
            "should not warn about phases when phases dir exists"
        );
    }

    #[test]
    fn startup_banner_warns_on_missing_phases_dir() {
        let flows = PathBuf::from("/tmp/darkmux-banner-present-flows");
        let missions = PathBuf::from("/tmp/darkmux-banner-present-missions");
        let phases = PathBuf::from("/tmp/darkmux-banner-missing-phases");
        let lines = build_startup_banner(
            &sample_addr(), &flows, true, &missions, true, &phases, false, 0, 0, None,
        );
        let joined = lines.join("\n");
        assert!(
            joined.contains("phases dir not found"),
            "expected phases warning; got: {joined}"
        );
        assert!(
            joined.contains("/tmp/darkmux-banner-missing-phases"),
            "phases warning should include the path"
        );
    }

    #[test]
    fn startup_banner_no_warnings_when_state_is_clean() {
        let flows = PathBuf::from("/some/flows");
        let missions = PathBuf::from("/some/missions");
        let phases = PathBuf::from("/some/phases");
        let lines = build_startup_banner(
            &sample_addr(), &flows, true, &missions, true, &phases, true, 1, 4, None,
        );
        let joined = lines.join("\n");
        assert!(!joined.contains("doesn't exist yet"), "no flows warning");
        assert!(!joined.contains("missions dir not found"), "no missions warning");
        assert!(!joined.contains("phases dir not found"), "no phases warning");
    }

    /// (#1247 Part 3) The banner names whichever lab-dir state is true —
    /// configured (shows the path) or unconfigured (shows the enable hint) —
    /// so the operator never has to wonder why the lab lens is empty.
    #[test]
    fn startup_banner_reports_lab_dir_state() {
        let flows = PathBuf::from("/some/flows");
        let missions = PathBuf::from("/some/missions");
        let phases = PathBuf::from("/some/phases");
        let unconfigured = build_startup_banner(
            &sample_addr(), &flows, true, &missions, true, &phases, true, 0, 0, None,
        )
        .join("\n");
        assert!(
            unconfigured.contains("lab dir:") && unconfigured.contains("--lab-dir"),
            "expected the enable hint when unconfigured: {unconfigured}"
        );

        let lab = PathBuf::from("/some/lab-runs");
        let configured = build_startup_banner(
            &sample_addr(), &flows, true, &missions, true, &phases, true, 0, 0, Some(&lab),
        )
        .join("\n");
        assert!(
            configured.contains("/some/lab-runs"),
            "expected the configured lab dir path: {configured}"
        );
    }

    // ─── #270 Redis aggregation tests ─────────────────────────────────
    //
    // Verify `GET /flow/<date>` (no `.jsonl`) returns a JSON array,
    // aggregating from Redis when `DARKMUX_REDIS_URL` is set + reachable
    // and falling back to the local file otherwise. POSIX-only because
    // the tests spawn a real `redis-server`. Tagged `#[serial]` because
    // they mutate `DARKMUX_REDIS_URL`.

    #[cfg(unix)]
    mod redis_aggregation {
        use super::*;
        use serial_test::serial;
        use std::process::{Child, Command, Stdio};
        use std::time::Instant;

        const REDIS_READY_TIMEOUT: Duration = Duration::from_secs(5);

        fn redis_server_available() -> bool {
            Command::new("redis-server")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }

        struct RedisFixture {
            child: Child,
            url: String,
        }

        impl Drop for RedisFixture {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }

        fn spawn_redis() -> RedisFixture {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")
                .expect("bind ephemeral port");
            let port = listener.local_addr().unwrap().port();
            drop(listener);

            // clippy's zombie-processes lint can't see through the
            // `Drop` impl below that kill+waits the child. Suppress at
            // the spawn site; the Drop guarantees no leaks.
            #[allow(clippy::zombie_processes)]
            let child = Command::new("redis-server")
                .args([
                    "--port", &port.to_string(),
                    "--save", "",
                    "--appendonly", "no",
                    "--bind", "127.0.0.1",
                    "--protected-mode", "no",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("redis-server spawn");

            let url = format!("redis://127.0.0.1:{port}");
            let client = redis::Client::open(url.as_str()).expect("redis client");
            let start = Instant::now();
            while start.elapsed() < REDIS_READY_TIMEOUT {
                if let Ok(mut conn) = client.get_connection() {
                    let ping: redis::RedisResult<String> = redis::cmd("PING").query(&mut conn);
                    if let Ok(s) = ping {
                        if s == "PONG" {
                            return RedisFixture { child, url };
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            panic!("redis-server failed to come ready within {REDIS_READY_TIMEOUT:?}");
        }

        fn xadd_flow_record(url: &str, record_json: &str) {
            let client = redis::Client::open(url).expect("redis client");
            let mut conn = client.get_connection().expect("conn");
            let _: String = redis::cmd("XADD")
                .arg("darkmux:flow")
                .arg("*")
                .arg("schema")
                .arg("1.8.0")
                .arg("record")
                .arg(record_json)
                .query(&mut conn)
                .expect("XADD");
        }

        fn today_utc_date() -> String {
            darkmux_flow::day_utc_now()
        }

        async fn body_as_array(
            response: axum::response::Response,
        ) -> Vec<serde_json::Value> {
            let bytes = to_bytes(response.into_body(), 1024 * 1024)
                .await
                .expect("body bytes");
            let body: serde_json::Value = serde_json::from_slice(&bytes)
                .expect("body parses as JSON");
            body.as_array().expect("body is JSON array").clone()
        }

        /// New behavior: GET /flow/<date> (no `.jsonl`) returns a JSON
        /// array sourced from Redis when DARKMUX_REDIS_URL is reachable.
        /// Records from other dates are filtered out.
        #[tokio::test]
        #[serial]
        async fn flow_endpoint_reads_from_redis_when_url_set() {
            if !redis_server_available() {
                eprintln!("skipping: redis-server not on PATH");
                return;
            }
            let redis = spawn_redis();
            let today = today_utc_date();

            let today_record = format!(
                r#"{{"ts":"{today}T12:00:00Z","action":"redis-today","machine_id":"laptop"}}"#
            );
            let other_record = r#"{"ts":"2020-01-01T12:00:00Z","action":"redis-other","machine_id":"laptop"}"#;
            xadd_flow_record(&redis.url, &today_record);
            xadd_flow_record(&redis.url, other_record);

            // SAFETY: serial-tagged test owns the env mutation window.
            unsafe { std::env::set_var("DARKMUX_REDIS_URL", &redis.url); }

            let tmp = TempDir::new().unwrap();
            let app = build_router(tmp.path().to_path_buf());
            let response = app
                .oneshot(
                    Request::builder()
                        .uri(format!("/flow/{today}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            unsafe { std::env::remove_var("DARKMUX_REDIS_URL"); }

            assert_eq!(response.status(), StatusCode::OK, "expected 200");
            let arr = body_as_array(response).await;
            assert!(
                arr.iter().any(|r| r.get("action").and_then(|v| v.as_str()) == Some("redis-today")),
                "expected `redis-today` record in response: {arr:?}"
            );
            assert!(
                arr.iter().all(|r| r.get("action").and_then(|v| v.as_str()) != Some("redis-other")),
                "expected `redis-other` (other date) filtered out: {arr:?}"
            );
        }

        /// Fallback path: DARKMUX_REDIS_URL set but pointing at an
        /// unreachable endpoint → daemon serves the local file's records
        /// as a JSON array.
        #[tokio::test]
        #[serial]
        async fn flow_endpoint_falls_back_to_file_when_redis_unreachable() {
            let today = today_utc_date();
            let tmp = TempDir::new().unwrap();
            let local_record = format!(
                r#"{{"ts":"{today}T12:00:00Z","action":"local-fallback","machine_id":"local"}}"#
            );
            fs::write(
                tmp.path().join(format!("{today}.jsonl")),
                format!("{local_record}\n"),
            )
            .unwrap();

            unsafe { std::env::set_var("DARKMUX_REDIS_URL", "redis://127.0.0.1:1"); }

            let app = build_router(tmp.path().to_path_buf());
            let response = app
                .oneshot(
                    Request::builder()
                        .uri(format!("/flow/{today}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            unsafe { std::env::remove_var("DARKMUX_REDIS_URL"); }

            assert_eq!(response.status(), StatusCode::OK);
            let arr = body_as_array(response).await;
            assert!(
                arr.iter().any(|r| r.get("action").and_then(|v| v.as_str()) == Some("local-fallback")),
                "expected fallback to local file when Redis unreachable: {arr:?}"
            );
        }

        /// Regression: DARKMUX_REDIS_URL unset → local file as JSON array.
        /// (Today this URL shape returns 400 because the handler only
        /// accepts `<date>.jsonl`. Post-fix it returns the array.)
        #[tokio::test]
        #[serial]
        async fn flow_endpoint_reads_local_file_when_redis_url_unset() {
            // (#811) The config tier is empty by construction in this test build
            // (the test-support dev-dep), so an unset DARKMUX_REDIS_URL means
            // Redis off — not the operator's config.json-assembled URL.
            let today = today_utc_date();
            let tmp = TempDir::new().unwrap();
            let local_record = format!(
                r#"{{"ts":"{today}T12:00:00Z","action":"local-only","machine_id":"local"}}"#
            );
            fs::write(
                tmp.path().join(format!("{today}.jsonl")),
                format!("{local_record}\n"),
            )
            .unwrap();

            unsafe { std::env::remove_var("DARKMUX_REDIS_URL"); }

            let app = build_router(tmp.path().to_path_buf());
            let response = app
                .oneshot(
                    Request::builder()
                        .uri(format!("/flow/{today}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::OK);
            let arr = body_as_array(response).await;
            assert!(
                arr.iter().any(|r| r.get("action").and_then(|v| v.as_str()) == Some("local-only")),
                "expected local-only record: {arr:?}"
            );
        }

        #[tokio::test]
        async fn flow_file_read_caps_at_max_records_keeping_newest() {
            // (#900) A day-file larger than the cap must yield only the newest
            // MAX_FLOW_FILE_RECORDS (chronological), bounding the daemon's
            // memory — pre-fix the whole file was read + every line parsed.
            let today = today_utc_date();
            let tmp = TempDir::new().unwrap();
            let total = MAX_FLOW_FILE_RECORDS + 5;
            let mut buf = String::with_capacity(total * 40);
            for i in 0..total {
                buf.push_str(&format!(r#"{{"ts":"{today}T00:00:00Z","n":{i}}}"#));
                buf.push('\n');
            }
            fs::write(tmp.path().join(format!("{today}.jsonl")), buf).unwrap();

            let records = read_flow_records_from_file(&today, tmp.path()).await;
            assert_eq!(records.len(), MAX_FLOW_FILE_RECORDS, "must cap at the max");
            // Oldest dropped, newest kept, chronological order preserved.
            assert_eq!(
                records.first().unwrap()["n"].as_u64().unwrap(),
                (total - MAX_FLOW_FILE_RECORDS) as u64
            );
            assert_eq!(records.last().unwrap()["n"].as_u64().unwrap(), (total - 1) as u64);
        }

        #[tokio::test]
        async fn flow_file_read_skips_a_pathological_oversize_line() {
            // (#925) A single line larger than MAX_FLOW_LINE_BYTES (a corrupt
            // or adversarial flows file with no newline) is skipped — drained
            // to its next newline — not read into one ballooning allocation.
            // The surrounding valid records are still returned, and empty /
            // non-JSON lines are tolerated (push_flow_line drops them).
            let today = today_utc_date();
            let tmp = TempDir::new().unwrap();
            let mut buf = String::new();
            buf.push_str(&format!(r#"{{"ts":"{today}T00:00:00Z","n":1}}"#));
            buf.push('\n');
            buf.push('\n'); // empty line — skipped
            buf.push_str("not valid json\n"); // non-JSON — skipped
            buf.push_str(&"x".repeat(MAX_FLOW_LINE_BYTES + 100)); // over-cap — skipped
            buf.push('\n');
            buf.push_str(&format!(r#"{{"ts":"{today}T00:00:00Z","n":2}}"#));
            buf.push('\n');
            fs::write(tmp.path().join(format!("{today}.jsonl")), buf).unwrap();

            let records = read_flow_records_from_file(&today, tmp.path()).await;
            assert_eq!(records.len(), 2, "oversize/empty/non-JSON lines skipped, valid kept");
            assert_eq!(records[0]["n"].as_u64().unwrap(), 1);
            assert_eq!(records[1]["n"].as_u64().unwrap(), 2);
        }

        // ─── #270 PR-B: redis_tail_lines (SSE Redis tail) ────────────────

        /// XADD'd records after the stream opens are emitted as lines.
        /// Parallel to `tail_stream_emits_appended_lines` for the file-tail
        /// path. Tail-from-now: existing entries in the stream are not
        /// replayed (semantics matches the file-tail handler).
        #[tokio::test]
        #[serial]
        async fn redis_tail_lines_emits_xadded_records_for_today() {
            if !redis_server_available() {
                eprintln!("skipping: redis-server not on PATH");
                return;
            }
            let redis = spawn_redis();
            let today = today_utc_date();

            // Pre-existing entry — must NOT be emitted (tail-from-now).
            let pre = format!(
                r#"{{"ts":"{today}T08:00:00Z","action":"pre-existing"}}"#
            );
            xadd_flow_record(&redis.url, &pre);

            let stream = redis_tail_lines(
                redis.url.clone(),
                "darkmux:flow".to_string(),
                today.clone(),
            );
            tokio::pin!(stream);

            // Give the stream a moment to read the current last-id before
            // we XADD the next one.
            tokio::time::sleep(Duration::from_millis(150)).await;

            let post = format!(
                r#"{{"ts":"{today}T12:00:00Z","action":"post-open"}}"#
            );
            xadd_flow_record(&redis.url, &post);

            let got = next_line(&mut stream, Duration::from_millis(3000)).await;
            assert!(
                got.as_ref().is_some_and(|s| s.contains("post-open")),
                "expected post-open record to arrive over the tail; got {got:?}"
            );
            assert!(
                !got.as_ref().unwrap().contains("pre-existing"),
                "tail-from-now must NOT replay history; got {got:?}"
            );
        }

        /// Records for OTHER dates are filtered out by the tail.
        #[tokio::test]
        #[serial]
        async fn redis_tail_lines_filters_records_for_other_dates() {
            if !redis_server_available() {
                eprintln!("skipping: redis-server not on PATH");
                return;
            }
            let redis = spawn_redis();
            let today = today_utc_date();

            let stream = redis_tail_lines(
                redis.url.clone(),
                "darkmux:flow".to_string(),
                today.clone(),
            );
            tokio::pin!(stream);

            tokio::time::sleep(Duration::from_millis(150)).await;

            // First XADD: other date — should be filtered out.
            xadd_flow_record(
                &redis.url,
                r#"{"ts":"2020-01-01T12:00:00Z","action":"other-date"}"#,
            );
            // Second XADD: today — should be emitted.
            let today_record = format!(
                r#"{{"ts":"{today}T12:00:00Z","action":"matching-date"}}"#
            );
            xadd_flow_record(&redis.url, &today_record);

            // First emit must be the matching-date one — other-date was
            // dropped on the way through.
            let got = next_line(&mut stream, Duration::from_millis(3000)).await;
            assert!(
                got.as_ref().is_some_and(|s| s.contains("matching-date")),
                "expected matching-date record; got {got:?}"
            );
            assert!(
                !got.as_ref().unwrap().contains("other-date"),
                "other-date record must be filtered: {got:?}"
            );
        }

        /// #293 — `redis_tail_lines` against a permanently-unreachable
        /// Redis MUST eventually emit a synthetic `stream.error` record
        /// and exit the spawned task. Pre-fix it retried forever (log
        /// spam + leaked spawned tasks per open SSE connection); the
        /// receiver-drop check only fired on a successful produce,
        /// which never happens on permanent failure.
        ///
        /// Contract: after MAX_CONSECUTIVE_XREAD_FAILURES (10) failures
        /// at 500ms backoff (≈5s wall-clock), the stream sends a
        /// synthetic record with `action: "stream.error"` and the
        /// receiver sees the channel close on its next `.next()`
        /// after that record.
        ///
        /// **Coverage caveat**: this exercises the TCP-refuses-fast
        /// path (127.0.0.1:1). The operator's actual Studio-offline
        /// path is structurally similar (Tailscale-routed peer,
        /// REDIS_CONNECT_TIMEOUT=500ms per connect → also ≈10s total
        /// budget) but takes 2× longer per iteration. Both are
        /// bounded by the 10s test budget, but only the
        /// refused-connect case is DIRECTLY verified here — would
        /// need a netem/tarpit fixture for the silent-timeout case.
        #[tokio::test]
        async fn redis_tail_lines_exits_after_persistent_xread_failures() {
            use futures::StreamExt;
            // 127.0.0.1:1 — privileged port; nothing listens. TCP
            // refuses fast → XREAD errors fast → ~10 failures × 500ms
            // ≈ 5s before the synthetic record + exit.
            let stream = redis_tail_lines(
                "redis://127.0.0.1:1".to_string(),
                "darkmux:flow".to_string(),
                "2026-05-23".to_string(),
            );
            tokio::pin!(stream);

            let read_synthetic = async {
                while let Some(line) = stream.next().await {
                    if line.contains("\"stream.error\"") {
                        return Some(line);
                    }
                }
                None
            };
            // Budget: MAX × 500ms backoff + slack = 10s wall.
            let synthetic = tokio::time::timeout(Duration::from_secs(10), read_synthetic)
                .await
                .expect("redis_tail_lines should emit synthetic + exit within 10s");
            assert!(
                synthetic.is_some(),
                "expected a stream.error synthetic record before the channel closed"
            );
            let line = synthetic.unwrap();
            assert!(
                line.contains("redis_tail_lines"),
                "synthetic record's handle should name the source; got: {line}"
            );
            assert!(
                line.contains("darkmux:flow"),
                "synthetic record's reasoning should name the stream; got: {line}"
            );

            // The next pull should yield None (channel closed because
            // the spawned task returned after emitting the synthetic).
            let post = tokio::time::timeout(Duration::from_secs(2), stream.next())
                .await
                .expect("channel close should be observable within 2s of synthetic");
            assert!(
                post.is_none(),
                "channel must close after the synthetic; got: {post:?}"
            );
        }

        /// Handler-level smoke: GET /flow/<date>/stream with Redis URL set
        /// returns 200 + content-type `text/event-stream` and surfaces an
        /// XADD'd record as an SSE `data: ...` line. End-to-end coverage
        /// for the integration path.
        #[tokio::test]
        #[serial]
        async fn flow_stream_handler_uses_redis_when_url_set() {
            if !redis_server_available() {
                eprintln!("skipping: redis-server not on PATH");
                return;
            }
            let redis = spawn_redis();
            let today = today_utc_date();
            unsafe { std::env::set_var("DARKMUX_REDIS_URL", &redis.url); }

            let tmp = TempDir::new().unwrap();
            let app = build_router(tmp.path().to_path_buf());
            let response = app
                .oneshot(
                    Request::builder()
                        .uri(format!("/flow/{today}/stream"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::OK);
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            assert!(
                content_type.starts_with("text/event-stream"),
                "expected SSE content-type; got {content_type}"
            );

            // Pull the SSE body as a stream; XADD; look for the line.
            use futures::StreamExt;
            let mut body = response.into_body().into_data_stream();

            // Give the handler a beat to probe Redis + start XREAD.
            tokio::time::sleep(Duration::from_millis(200)).await;

            let record = format!(
                r#"{{"ts":"{today}T12:00:00Z","action":"sse-redis-end-to-end"}}"#
            );
            xadd_flow_record(&redis.url, &record);

            // Read up to 3s waiting for the SSE chunk that includes the line.
            let read_fut = async {
                let mut acc = Vec::new();
                while let Some(Ok(chunk)) = body.next().await {
                    acc.extend_from_slice(&chunk);
                    let s = String::from_utf8_lossy(&acc);
                    if s.contains("sse-redis-end-to-end") {
                        return s.into_owned();
                    }
                }
                String::from_utf8_lossy(&acc).into_owned()
            };
            let got = tokio::time::timeout(Duration::from_secs(3), read_fut)
                .await
                .unwrap_or_default();

            unsafe { std::env::remove_var("DARKMUX_REDIS_URL"); }

            assert!(
                got.contains("sse-redis-end-to-end"),
                "expected XADD'd record to surface as SSE event; got: {got:?}"
            );
        }
    }

    // ─── #1387: worktree-summary endpoint tests (shared session-resolution
    // infra below predates it — kept from #756, still exercised by the
    // current handler) ──────────────────────────────────────────────────

    #[test]
    fn validate_base_ref_accepts_valid_refs() {
        assert!(validate_base_ref("main"));
        assert!(validate_base_ref("origin/main"));
        assert!(validate_base_ref("abc123"));
        assert!(validate_base_ref("v1.0.0"));
        assert!(validate_base_ref("feature/my-branch"));
        assert!(validate_base_ref("a_b.c-d"));
    }

    #[test]
    fn validate_base_ref_rejects_invalid_refs() {
        assert!(!validate_base_ref(""));
        assert!(!validate_base_ref("-rev"));
        assert!(!validate_base_ref("$(x)"));
        assert!(!validate_base_ref("a b")); // space
    }

    #[test]
    fn worktree_contained_accepts_path_under_base() {
        let tmp = TempDir::new().unwrap();
        // Create a real directory structure.
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        assert!(worktree_contained(
            &tmp.path().join("sub"),
            tmp.path(),
        ));
    }

    #[test]
    fn worktree_contained_rejects_path_outside_base() {
        let tmp = TempDir::new().unwrap();
        // A path outside the base dir.
        assert!(
            !worktree_contained(
                StdPath::new("/tmp"),
                tmp.path(),
            )
        );
    }

    #[test]
    fn worktree_contained_rejects_missing_worktree() {
        let tmp = TempDir::new().unwrap();
        // Non-existent worktree path.
        assert!(
            !worktree_contained(
                &tmp.path().join("does_not_exist"),
                tmp.path(),
            )
        );
    }

    #[test]
    fn resolve_session_finds_matching_record() {
        let tmp = TempDir::new().unwrap();
        let record = serde_json::json!({
            "action": "step result",
            "session_id": "abc123",
            "payload": {
                "step_id": "s1-worktree-step",
                "kind": "mission.worktree",
                "worktree": "/tmp/wt",
                "base": "main",
                "branch": "darkmux/abc123"
            }
        });
        fs::write(
            tmp.path().join("2026-01-15.jsonl"),
            format!("{}\n", record),
        )
        .unwrap();

        let result = resolve_session("abc123", tmp.path());
        assert!(result.is_some(), "expected session to resolve");
        let (wt, base, branch) = result.unwrap();
        assert_eq!(wt, "/tmp/wt");
        assert_eq!(base, "main");
        assert_eq!(branch, "darkmux/abc123");
    }

    #[test]
    fn resolve_session_returns_none_for_unknown() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("2026-01-15.jsonl"),
            r#"{"action":"step result","session_id":"other","payload":{"step_id":"s1-worktree-step","kind":"mission.worktree","worktree":"/tmp/wt","base":"main","branch":"x"}}\n"#,
        )
        .unwrap();

        let result = resolve_session("unknown", tmp.path());
        assert!(result.is_none(), "expected no match for unknown session");
    }

    /// `compute_diff_summary` must count files and sum additions/deletions
    /// correctly from `git diff --numstat` — including a BINARY file, whose
    /// numstat line is `-\t-\t<path>`: it counts as a changed FILE but
    /// contributes nothing to the add/del totals (the `parse_numstat_count`
    /// "-" arm, pinned here by a direct test). And (the #1387 invariant this
    /// replaces the old cap tests with) it must never read or return diff
    /// TEXT: only the three `u64` totals come back, whatever the change size.
    #[test]
    fn compute_diff_summary_counts_files_and_totals() {
        let wt = TempDir::new().unwrap();
        let dir = wt.path();
        let git = |args: &[&str]| {
            Command::new("git")
                .current_dir(dir)
                .args(args)
                .output()
                .expect("git command");
        };
        git(&["init"]);
        git(&["config", "user.email", "t@t.com"]);
        git(&["config", "user.name", "T"]);
        fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        fs::write(dir.join("b.txt"), "x\n").unwrap();
        // A binary file (NUL byte forces git's binary detection).
        fs::write(dir.join("blob.bin"), [0u8, 159, 146, 150]).unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "base"]);
        let base = String::from_utf8(
            Command::new("git")
                .current_dir(dir)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        // a.txt: +1/-2 lines (replace both with one new line); b.txt: +1/-0
        // (append a line); blob.bin: modified binary (numstat `-\t-\t...`).
        // Three files changed, still 2 additions and 2 deletions total.
        fs::write(dir.join("a.txt"), "three\n").unwrap();
        fs::write(dir.join("b.txt"), "x\ny\n").unwrap();
        fs::write(dir.join("blob.bin"), [0u8, 1, 2, 3, 4]).unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "change"]);

        let (files, adds, dels) =
            compute_diff_summary(dir, &base).expect("compute_diff_summary");
        assert_eq!(files, 3, "expected three changed files (two text + one binary)");
        assert_eq!(adds, 2, "expected 2 total additions (binary contributes none)");
        assert_eq!(dels, 2, "expected 2 total deletions (binary contributes none)");
    }

    #[tokio::test]
    // (#881) serial: DARKMUX_SERVE_TOKEN is set/scrubbed by the serial auth
    // tests — without this, that token can leak into this test's
    // build_router window and 401 the request (CI-only race).
    #[serial_test::serial]
    async fn worktree_summary_handler_returns_available_true_with_totals() {
        // Create a temp dir acting as the worktrees base containing a real git repo.
        let wt_base = TempDir::new().unwrap();

        // Create a real git repo inside the base.
        let wt_dir = wt_base.path().join("myrepo").join("phase1");
        std::fs::create_dir_all(&wt_dir).unwrap();

        // Initialize git repo.
        Command::new("git")
            .current_dir(&wt_dir)
            .args(["init"])
            .output()
            .expect("git init");

        // Configure git user for commits.
        Command::new("git")
            .current_dir(&wt_dir)
            .args(["config", "user.email", "test@test.com"])
            .output()
            .expect("git config");
        Command::new("git")
            .current_dir(&wt_dir)
            .args(["config", "user.name", "Test"])
            .output()
            .expect("git config");

        // Create and commit a file as base.
        let test_file = wt_dir.join("hello.txt");
        fs::write(&test_file, "original content\n").unwrap();

        Command::new("git")
            .current_dir(&wt_dir)
            .args(["add", "."])
            .output()
            .expect("git add");
        let _commit_out = Command::new("git")
            .current_dir(&wt_dir)
            .args(["commit", "-m", "initial"])
            .output()
            .expect("git commit");
        // Use git rev-parse to get just the hash.
        let base_ref = Command::new("git")
            .current_dir(&wt_dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git rev-parse")
            .stdout;
        let base_ref = String::from_utf8(base_ref).unwrap().trim().to_string();

        // Modify the file (no separate untracked file needed — the
        // #1387 response never enumerates untracked paths).
        fs::write(&test_file, "modified content\n").unwrap();
        Command::new("git")
            .current_dir(&wt_dir)
            .args(["add", "."])
            .output()
            .expect("git add");

        // Create a flows dir with a day file pointing at this worktree.
        let flows_dir = TempDir::new().unwrap();
        let record = serde_json::json!({
            "action": "step result",
            "session_id": "test-session-1",
            "payload": {
                "step_id": "s1-worktree-step",
                "kind": "mission.worktree",
                "worktree": wt_dir.to_str().unwrap(),
                "base": &base_ref,
                "branch": "darkmux/test"
            }
        });
        fs::write(
            flows_dir.path().join("2026-01-15.jsonl"),
            format!("{}\n", record),
        )
        .unwrap();

        let app = build_router_with_worktrees_base(
            flows_dir.path().to_path_buf(),
            wt_base.path().to_path_buf(),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/worktree-summary/test-session-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON body");

        assert!(json["available"].as_bool().unwrap_or(false), "expected available:true");
        assert_eq!(json["session_id"].as_str(), Some("test-session-1"));
        assert!(!json["base"].as_str().unwrap_or("").is_empty());
        assert!(!json["branch"].as_str().unwrap_or("").is_empty());
        assert_eq!(json["path"].as_str(), Some(wt_dir.to_str().unwrap()));

        // One file changed (hello.txt), one line added and one removed.
        assert_eq!(json["files"].as_u64(), Some(1), "expected one changed file");
        assert_eq!(json["adds"].as_u64(), Some(1), "expected one addition");
        assert_eq!(json["dels"].as_u64(), Some(1), "expected one deletion");

        // No diff TEXT and no per-file breakdown ever leave the server — the
        // whole point of #1387.
        assert!(json.get("diff").is_none(), "response must never carry diff text");
        assert!(json.get("untracked").is_none(), "response must never carry an untracked list");
    }

    #[tokio::test]
    // (#881) serial — see worktree_summary_handler_returns_available_true_with_totals:
    // the general remote gate keys on DARKMUX_SERVE_TOKEN set by the serial auth tests.
    #[serial_test::serial]
    async fn worktree_summary_handler_returns_available_false_for_unknown_session() {
        let flows_dir = TempDir::new().unwrap();
        // No records at all.

        let app = build_router(flows_dir.path().to_path_buf());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/worktree-summary/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON body");
        assert!(!json["available"].as_bool().unwrap_or(false));
    }

    // ─── #1247 Part 3: lab observer lens ───────────────────────────────────
    //
    // Every fixture below is SYNTHETIC — fabricated field-for-field to match
    // the real `ReviewEnvelope` / `FlowRecord` / `ScoresDoc` shapes (verified
    // against the actual types via `darkmux_lab::lab::{funnel,scores}`), but
    // with invented model names / case ids / crew names. No content from any
    // private corpus.

    /// Write a "finished" synthetic funnel run at `dir`: `funnels.json` (one
    /// envelope), `funnel-events.jsonl` (a couple of records), and
    /// `scores.json` (minimal valid doc). `mtime_offset_secs` lets a test
    /// stagger two runs' mtimes deterministically (rather than racing on
    /// filesystem timestamp resolution) — 0 for "now", a larger value for
    /// "older" by touching the file's mtime backward via `filetime`-free
    /// manual `set_file_mtime` (std has no stable API pre-1.75 filetime
    /// crate; sidestepped by writing older runs FIRST in test bodies instead
    /// — see the callers).
    fn write_synthetic_funnel_run(dir: &StdPath, case_id: &str, crew: &str) {
        fs::create_dir_all(dir).unwrap();
        let funnels = serde_json::json!([{
            "case_id": case_id,
            "crew": crew,
            "mode": "sequential",
            "members": [],
            "steps": [],
            "bundles": 5,
            "raw_flags": 8,
            "deduped_flags": 6,
            "flags": [],
            "judged": [],
            "confirmed": 2,
            "needs_check": 1,
            "archived": 3,
            "fingerprint": {},
            "staffing": {
                "probes": [
                    {"name": "demo-probe", "model": "darkmux:demo-probe-model", "k": 1, "n_ctx": 32768, "max_tokens": 3000}
                ],
                "judge": {"name": "demo-judge", "model": "darkmux:demo-judge-model", "k": 3, "n_ctx": 65536, "max_tokens": 20000}
            }
        }]);
        fs::write(dir.join("funnels.json"), serde_json::to_vec_pretty(&funnels).unwrap()).unwrap();

        let events = format!(
            "{}\n{}\n",
            serde_json::json!({
                "ts": "2026-01-01T00:00:00Z", "level": "info", "category": "work",
                "tier": "local", "stage": "dispatch", "action": "review.task",
                "handle": crew, "session_id": case_id, "source": "review",
                "payload": {"case_id": case_id, "crew": crew, "exec_mode": "sequential", "status": "started", "bundles": 5}
            }),
            serde_json::json!({
                "ts": "2026-01-01T00:05:00Z", "level": "info", "category": "work",
                "tier": "local", "stage": "dispatch", "action": "review.task",
                "handle": crew, "session_id": case_id, "source": "review",
                "payload": {"status": "finished", "confirmed": 2, "needs_check": 1, "archived": 3}
            }),
        );
        fs::write(dir.join("funnel-events.jsonl"), events).unwrap();

        let scores = serde_json::json!({
            "schema_version": "1.0.0",
            "provenance": {
                "run_id": format!("demo-run-{case_id}"),
                "ts": "2026-01-01T00:05:00Z",
                "machine": {"machine_id": "test-machine"},
                "profile": "demo-profile",
                "loaded_drift": false
            },
            "rows": [],
            "crew": crew,
            "exec_mode": "sequential",
            "mode": "funnel",
            "k": "(profile default)"
        });
        fs::write(dir.join("scores.json"), serde_json::to_vec_pretty(&scores).unwrap()).unwrap();
    }

    /// Write a LIVE synthetic run at `dir`: only `funnel-events.jsonl` with a
    /// single "started" `funnel.task` record — no `funnels.json`/
    /// `scores.json` yet, matching a run whose first case hasn't completed.
    fn write_synthetic_live_run(dir: &StdPath, case_id: &str, crew: &str) {
        fs::create_dir_all(dir).unwrap();
        let record = serde_json::json!({
            "ts": "2026-01-01T00:00:00Z", "level": "info", "category": "work",
            "tier": "local", "stage": "dispatch", "action": "review.task",
            "handle": crew, "session_id": case_id, "source": "review",
            "payload": {"case_id": case_id, "crew": crew, "exec_mode": "parallel", "status": "started", "bundles": 12}
        });
        fs::write(dir.join("funnel-events.jsonl"), format!("{record}\n")).unwrap();
    }

    #[test]
    fn resolve_lab_run_dir_accepts_valid_relative_subdir() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("case-a/run1")).unwrap();
        let resolved = resolve_lab_run_dir(tmp.path(), "case-a/run1");
        assert_eq!(resolved, Some(tmp.path().join("case-a/run1")));
    }

    #[test]
    fn resolve_lab_run_dir_rejects_absolute_path() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(resolve_lab_run_dir(tmp.path(), "/etc/passwd"), None);
    }

    #[test]
    fn resolve_lab_run_dir_rejects_parent_dir_traversal() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("case-a")).unwrap();
        assert_eq!(resolve_lab_run_dir(tmp.path(), "case-a/../../etc"), None);
        assert_eq!(resolve_lab_run_dir(tmp.path(), "../outside"), None);
    }

    #[test]
    fn resolve_lab_run_dir_empty_resolves_to_lab_dir_itself() {
        // QA finding (#1247 PR review round): a run matched AT `lab_dir`'s
        // own root gets `""` as its `dir` — that identifier must round-trip
        // back to `lab_dir`, or such a run is listed but permanently
        // un-drillable (a 400 on every detail/events request).
        let tmp = TempDir::new().unwrap();
        assert_eq!(resolve_lab_run_dir(tmp.path(), ""), Some(tmp.path().to_path_buf()));
    }

    #[test]
    fn resolve_lab_run_dir_rejects_nonexistent_subdir() {
        // worktree_contained canonicalizes both sides; a dir that was never
        // created can't resolve — the endpoint would 400 rather than 500 on
        // a typo'd `dir` param.
        let tmp = TempDir::new().unwrap();
        assert_eq!(resolve_lab_run_dir(tmp.path(), "does-not-exist"), None);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_lab_run_dir_rejects_symlink_escape() {
        // A symlink INSIDE lab_dir pointing OUTSIDE it must be rejected —
        // the string-shape checks (`..`, absolute) can't catch this; only
        // the canonicalize + prefix check in `worktree_contained` can, since
        // canonicalize resolves the link to its real (outside) target before
        // the prefix comparison runs.
        let lab = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir_all(outside.path().join("secret")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), lab.path().join("escape")).unwrap();
        assert_eq!(resolve_lab_run_dir(lab.path(), "escape"), None);
    }

    #[test]
    fn scan_lab_runs_finds_a_finished_run_and_computes_headline_counts() {
        let tmp = TempDir::new().unwrap();
        write_synthetic_funnel_run(&tmp.path().join("case-a/run1"), "demo-case-a", "demo-crew");
        let runs = scan_lab_runs(tmp.path());
        assert_eq!(runs.len(), 1, "expected exactly one run cluster: {runs:?}");
        let r = &runs[0];
        assert_eq!(r.dir, "case-a/run1");
        assert_eq!(r.case_ids, vec!["demo-case-a".to_string()]);
        assert_eq!(r.crew.as_deref(), Some("demo-crew"));
        assert_eq!(r.exec_mode.as_deref(), Some("sequential"));
        assert_eq!(r.bundles, 5);
        assert_eq!(r.raw_flags, 8);
        assert_eq!(r.deduped_flags, 6);
        assert_eq!(r.confirmed, 2);
        assert_eq!(r.needs_check, 1);
        assert_eq!(r.archived, 3);
        assert!(!r.degenerate);
        assert!(r.finished, "scores.json present => finished");
        assert!(r.has_funnels);
        assert!(r.has_events);
        let staffing = r.staffing.as_ref().expect("staffing snapshot present");
        assert_eq!(staffing.probes.len(), 1);
        assert_eq!(staffing.probes[0].model, "darkmux:demo-probe-model");
        assert_eq!(staffing.judge.as_ref().unwrap().model, "darkmux:demo-judge-model");
    }

    #[test]
    fn scan_lab_runs_finds_a_live_run_via_events_prefix_scan() {
        let tmp = TempDir::new().unwrap();
        write_synthetic_live_run(&tmp.path().join("case-b/gate-1"), "demo-case-b", "demo-gate-crew");
        let runs = scan_lab_runs(tmp.path());
        assert_eq!(runs.len(), 1);
        let r = &runs[0];
        assert_eq!(r.dir, "case-b/gate-1");
        assert_eq!(r.case_ids, vec!["demo-case-b".to_string()]);
        assert_eq!(r.crew.as_deref(), Some("demo-gate-crew"));
        assert_eq!(r.exec_mode.as_deref(), Some("parallel"));
        assert!(!r.finished, "no scores.json yet => not finished");
        assert!(!r.has_funnels, "no case has completed yet");
        assert!(r.has_events);
        assert!(r.staffing.is_none(), "no envelope snapshot exists yet");
    }

    #[test]
    fn scan_lab_runs_does_not_descend_into_a_matched_runs_worktrees() {
        // A matched run dir's own `worktrees/<case>` subtree is a full repo
        // checkout that could itself contain marker-file-shaped noise (e.g.
        // a nested fixture) — `LAB_SCAN_SKIP_DIRS` must keep the scan from
        // ever walking into it looking for more runs, unconditionally
        // (independent of whether the parent itself matched — see the next
        // test for why "stop entirely on match" is NOT the mechanism this
        // relies on).
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("probe-27b-527");
        write_synthetic_funnel_run(&run_dir, "demo-case-c", "demo-crew");
        // Plant a decoy inside the run's own worktrees/ subtree that WOULD
        // match the run-cluster criterion if the scanner ever walked there.
        let decoy = run_dir.join("worktrees/demo-case-c/nested");
        fs::create_dir_all(&decoy).unwrap();
        fs::write(decoy.join("scores.json"), "{}").unwrap();

        let runs = scan_lab_runs(tmp.path());
        assert_eq!(runs.len(), 1, "the decoy under worktrees/ must not surface as a second run: {runs:?}");
        assert_eq!(runs[0].dir, "probe-27b-527");
    }

    /// Regression (caught live against a real corpus during #1247 manual
    /// verification, NOT in unit tests originally): a stray marker file
    /// left directly at `lab_dir` — e.g. a leftover `funnels.json` from an
    /// earlier ad-hoc `--scores-out` pointed at the corpus root — must not
    /// swallow every run nested beneath it. An earlier version of this scan
    /// returned early the instant ANY directory matched, which made
    /// `lab_dir` itself matching hide the entire tree below it.
    #[test]
    fn scan_lab_runs_a_stray_marker_at_lab_dir_root_does_not_hide_nested_runs() {
        let tmp = TempDir::new().unwrap();
        // A stray funnels.json sitting directly at the scan root — matches
        // the run-cluster criterion on its own, with no sibling scores.json
        // or funnel-events.jsonl (exactly the leftover-artifact shape).
        fs::write(tmp.path().join("funnels.json"), "[]").unwrap();
        write_synthetic_funnel_run(&tmp.path().join("case-a/run1"), "demo-case-a", "demo-crew");
        write_synthetic_funnel_run(&tmp.path().join("case-b/run1"), "demo-case-b", "demo-crew");

        let runs = scan_lab_runs(tmp.path());
        let dirs: Vec<&str> = runs.iter().map(|r| r.dir.as_str()).collect();
        assert!(dirs.contains(&""), "the root's own stray marker still surfaces as a run: {dirs:?}");
        assert!(dirs.contains(&"case-a/run1"), "nested runs must survive a matching root: {dirs:?}");
        assert!(dirs.contains(&"case-b/run1"), "nested runs must survive a matching root: {dirs:?}");
        assert_eq!(runs.len(), 3, "{runs:?}");
    }

    #[test]
    fn scan_lab_runs_respects_max_depth() {
        let tmp = TempDir::new().unwrap();
        // 6 levels deep — past LAB_SCAN_MAX_DEPTH (4) from lab_dir.
        let deep = tmp
            .path()
            .join("a/b/c/d/e/f");
        write_synthetic_funnel_run(&deep, "too-deep", "demo-crew");
        let runs = scan_lab_runs(tmp.path());
        assert!(runs.is_empty(), "a run past the depth cap must not surface: {runs:?}");
    }

    #[test]
    fn scan_lab_runs_multiple_case_dirs_sorted_newest_first() {
        let tmp = TempDir::new().unwrap();
        write_synthetic_funnel_run(&tmp.path().join("older/run1"), "case-older", "crew-x");
        // Force a deterministic newer mtime on the second run's marker files
        // rather than racing filesystem timestamp resolution.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_synthetic_funnel_run(&tmp.path().join("newer/run1"), "case-newer", "crew-x");

        let runs = scan_lab_runs(tmp.path());
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].dir, "newer/run1", "newest run sorts first: {runs:?}");
        assert_eq!(runs[1].dir, "older/run1");
    }

    #[test]
    fn tail_lab_events_backfills_from_zero_then_returns_only_new_lines() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        fs::write(dir.join("funnel-events.jsonl"), "{\"a\":1}\n{\"a\":2}\n").unwrap();

        let first = tail_lab_events(dir, 0);
        assert_eq!(first.lines.len(), 2);
        assert!(!first.finished);
        assert!(first.next_offset > 0);

        // Nothing new yet — same offset, empty backfill.
        let stale = tail_lab_events(dir, first.next_offset);
        assert!(stale.lines.is_empty());
        assert_eq!(stale.next_offset, first.next_offset);

        // Append a third record and re-poll from the returned offset.
        use std::io::Write;
        let mut f = fs::OpenOptions::new().append(true).open(dir.join("funnel-events.jsonl")).unwrap();
        writeln!(f, "{{\"a\":3}}").unwrap();
        drop(f);

        let second = tail_lab_events(dir, first.next_offset);
        assert_eq!(second.lines.len(), 1, "only the newly-appended line: {second:?}");
        assert_eq!(second.lines[0]["a"], 3);
    }

    #[test]
    fn tail_lab_events_leaves_a_torn_trailing_line_for_the_next_poll() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        // A complete line followed by a partial one with NO trailing
        // newline — simulates reading mid-flush.
        fs::write(dir.join("funnel-events.jsonl"), "{\"a\":1}\n{\"a\":2").unwrap();

        let resp = tail_lab_events(dir, 0);
        assert_eq!(resp.lines.len(), 1, "the torn line must not be parsed yet: {resp:?}");
        assert_eq!(resp.lines[0]["a"], 1);
        // next_offset must land exactly after the complete line's newline,
        // not past the torn trailing bytes.
        assert_eq!(resp.next_offset, "{\"a\":1}\n".len() as u64);
    }

    #[test]
    fn tail_lab_events_reports_finished_once_scores_json_exists() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        fs::write(dir.join("funnel-events.jsonl"), "{\"a\":1}\n").unwrap();
        assert!(!tail_lab_events(dir, 0).finished);
        fs::write(dir.join("scores.json"), "{}").unwrap();
        assert!(tail_lab_events(dir, 0).finished);
    }

    #[test]
    fn tail_lab_events_missing_file_returns_empty_not_error() {
        let tmp = TempDir::new().unwrap();
        let resp = tail_lab_events(tmp.path(), 0);
        assert!(resp.lines.is_empty());
        assert_eq!(resp.next_offset, 0);
        assert!(!resp.finished);
    }

    #[tokio::test]
    async fn lab_runs_handler_reports_unconfigured_when_no_lab_dir() {
        let flows = TempDir::new().unwrap();
        let app = build_router_full(flows.path().to_path_buf(), worktrees_base_dir(), None);
        let response = app
            .oneshot(Request::builder().uri("/lab/runs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["configured"], false);
        assert_eq!(json["runs"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn lab_runs_handler_lists_a_configured_runs_directory() {
        let flows = TempDir::new().unwrap();
        let lab = TempDir::new().unwrap();
        write_synthetic_funnel_run(&lab.path().join("case-a/run1"), "demo-case-a", "demo-crew");
        let app = build_router_full(
            flows.path().to_path_buf(),
            worktrees_base_dir(),
            Some(lab.path().to_path_buf()),
        );
        let response = app
            .oneshot(Request::builder().uri("/lab/runs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["configured"], true);
        let runs = json["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["dir"], "case-a/run1");
        assert_eq!(runs[0]["crew"], "demo-crew");
    }

    #[tokio::test]
    async fn lab_run_detail_handler_returns_funnels_and_scores() {
        let flows = TempDir::new().unwrap();
        let lab = TempDir::new().unwrap();
        write_synthetic_funnel_run(&lab.path().join("case-a/run1"), "demo-case-a", "demo-crew");
        let app = build_router_full(
            flows.path().to_path_buf(),
            worktrees_base_dir(),
            Some(lab.path().to_path_buf()),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/lab/run/detail?dir=case-a%2Frun1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["dir"], "case-a/run1");
        let funnels = json["funnels"].as_array().unwrap();
        assert_eq!(funnels.len(), 1);
        assert_eq!(funnels[0]["case_id"], "demo-case-a");
        assert_eq!(json["scores"]["provenance"]["profile"], "demo-profile");
    }

    #[tokio::test]
    async fn lab_run_detail_handler_resolves_a_run_matched_at_lab_dir_root() {
        // QA finding (#1247 PR review round): `lab_dir` ITSELF matching the
        // run-cluster criteria (an operator pointing `--lab-dir` at one
        // run's folder, or a stray marker left at the root) gets `dir: ""`
        // from `/lab/runs` — that identifier must actually be drillable, not
        // a permanent 400.
        let flows = TempDir::new().unwrap();
        let lab = TempDir::new().unwrap();
        write_synthetic_funnel_run(lab.path(), "demo-case-root", "demo-crew");
        let app = build_router_full(
            flows.path().to_path_buf(),
            worktrees_base_dir(),
            Some(lab.path().to_path_buf()),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/lab/run/detail?dir=")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["funnels"].as_array().unwrap()[0]["case_id"], "demo-case-root");
    }

    #[tokio::test]
    async fn lab_run_detail_handler_rejects_traversal_dir() {
        let flows = TempDir::new().unwrap();
        let lab = TempDir::new().unwrap();
        write_synthetic_funnel_run(&lab.path().join("case-a/run1"), "demo-case-a", "demo-crew");
        let app = build_router_full(
            flows.path().to_path_buf(),
            worktrees_base_dir(),
            Some(lab.path().to_path_buf()),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/lab/run/detail?dir=..%2F..%2Fetc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn lab_run_detail_handler_404s_when_not_configured() {
        let flows = TempDir::new().unwrap();
        let app = build_router_full(flows.path().to_path_buf(), worktrees_base_dir(), None);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/lab/run/detail?dir=case-a%2Frun1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn lab_run_events_handler_backfills_then_deltas_across_two_polls() {
        let flows = TempDir::new().unwrap();
        let lab = TempDir::new().unwrap();
        let run_dir = lab.path().join("live/gate-1");
        write_synthetic_live_run(&run_dir, "demo-case-b", "demo-gate-crew");
        let app = build_router_full(
            flows.path().to_path_buf(),
            worktrees_base_dir(),
            Some(lab.path().to_path_buf()),
        );

        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/lab/run/events?dir=live%2Fgate-1&offset=0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let bytes = to_bytes(first.into_body(), 65536).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["lines"].as_array().unwrap().len(), 1);
        assert!(!json["finished"].as_bool().unwrap());
        let next_offset = json["next_offset"].as_u64().unwrap();
        assert!(next_offset > 0);

        // Append a second record directly (simulating the driver emitting
        // another step) and re-poll from next_offset.
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("funnel-events.jsonl"))
            .unwrap();
        writeln!(
            f,
            "{}",
            serde_json::json!({
                "ts": "2026-01-01T00:01:00Z", "level": "info", "category": "work",
                "tier": "local", "stage": "dispatch", "action": "review.step",
                "handle": "demo-gate-crew", "session_id": "demo-case-b", "source": "review",
                "payload": {"step_id": "bundle", "kind": "procedural", "status": "finished", "items_in": 1, "items_out": 12, "wall_ms": 0}
            })
        )
        .unwrap();
        drop(f);

        let second = app
            .oneshot(
                Request::builder()
                    .uri(format!("/lab/run/events?dir=live%2Fgate-1&offset={next_offset}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        let bytes = to_bytes(second.into_body(), 65536).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let lines = json["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 1, "only the newly-appended record: {lines:?}");
        assert_eq!(lines[0]["action"], "review.step");
    }

    #[tokio::test]
    async fn lab_run_events_handler_rejects_traversal_dir() {
        let flows = TempDir::new().unwrap();
        let lab = TempDir::new().unwrap();
        write_synthetic_live_run(&lab.path().join("live/gate-1"), "demo-case-b", "demo-gate-crew");
        let app = build_router_full(
            flows.path().to_path_buf(),
            worktrees_base_dir(),
            Some(lab.path().to_path_buf()),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/lab/run/events?dir=%2Fetc%2Fpasswd&offset=0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn lab_run_events_handler_404s_when_not_configured() {
        let flows = TempDir::new().unwrap();
        let app = build_router_full(flows.path().to_path_buf(), worktrees_base_dir(), None);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/lab/run/events?dir=live%2Fgate-1&offset=0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // ─── Mission graph lens (#1284 Packet 5) ────────────────────────────

    /// Redirects `darkmux-crew`'s mission/phase/task/step storage into a
    /// fresh temp dir for the lifetime of the guard, mirroring the
    /// `CrewDirGuard` pattern used throughout `darkmux-crew`'s own tests
    /// (`cli.rs`, `index.rs`) — `DARKMUX_CREW_DIR` is the top tier of
    /// `darkmux_types::config_access::crew_dir_override()`, read live per
    /// call, so no process restart is needed for the override to take
    /// effect. Callers of anything backed by this guard MUST be
    /// `#[serial_test::serial]` (env var mutation isn't thread-safe).
    struct CrewDirGuard {
        prev: Option<String>,
        // Held only to keep the temp dir alive for the guard's lifetime
        // (RAII) — never read after construction, hence the underscore.
        _tmp: TempDir,
    }
    impl CrewDirGuard {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let prev = std::env::var("DARKMUX_CREW_DIR").ok();
            unsafe {
                std::env::set_var("DARKMUX_CREW_DIR", tmp.path());
            }
            Self { prev, _tmp: tmp }
        }
    }
    impl Drop for CrewDirGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                    None => std::env::remove_var("DARKMUX_CREW_DIR"),
                }
            }
        }
    }

    fn now_unix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn save_test_mission(mission: &darkmux_crew::types::Mission) {
        darkmux_crew::lifecycle::save_mission(mission).expect("save mission");
    }
    fn save_test_phase(phase: &darkmux_crew::types::Phase) {
        darkmux_crew::lifecycle::save_phase(phase).expect("save phase");
    }

    fn minimal_mission(id: &str, phase_ids: Vec<String>) -> darkmux_crew::types::Mission {
        darkmux_crew::types::Mission {
            id: id.to_string(),
            description: format!("test mission {id}"),
            status: darkmux_crew::types::MissionStatus::Active,
            phase_ids,
            created_ts: now_unix(),
            started_ts: None,
            closed_ts: None,
            paused_ts: None,
            source_input: None,
            ticket: None,
        }
    }

    fn minimal_phase(id: &str, mission_id: &str) -> darkmux_crew::types::Phase {
        darkmux_crew::types::Phase {
            id: id.to_string(),
            mission_id: mission_id.to_string(),
            description: format!("phase {id}"),
            display_name: None,
            status: darkmux_crew::types::PhaseStatus::Running,
            created_ts: now_unix(),
            started_ts: Some(now_unix()),
            completed_ts: None,
            abandoned_ts: None,
            task_ids: Vec::new(),
        }
    }

    #[tokio::test]
    async fn mission_graph_html_serves_the_page() {
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/mission/any-id/graph")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/html; charset=utf-8"
        );
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let html = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(html.contains("/vendor/reactflow-bundle.min.js"));
        assert!(html.contains("/vendor/reactflow-bundle.min.css"));
        // Same XSS-hardening posture as viewer.html: no raw HTML injection
        // sink. React's `createElement` escapes text content by
        // construction; this page must never reach for the one API that
        // opts back OUT of that (`dangerouslySetInnerHTML`).
        assert!(!html.contains("dangerouslySetInnerHTML"));
        assert!(!html.to_lowercase().contains("javascript:"));
    }

    #[tokio::test]
    async fn vendor_reactflow_js_served_with_correct_content_type() {
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/vendor/reactflow-bundle.min.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/javascript; charset=utf-8"
        );
        let bytes = to_bytes(response.into_body(), 8 << 20).await.unwrap();
        assert!(bytes.len() > 10_000, "vendor bundle unexpectedly tiny: {} bytes", bytes.len());
        let js = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(js.contains("MissionGraphVendor"));
    }

    #[tokio::test]
    async fn vendor_reactflow_css_served_with_correct_content_type() {
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/vendor/reactflow-bundle.min.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get("content-type").unwrap(), "text/css; charset=utf-8");
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        assert!(!bytes.is_empty());
    }

    /// (#1403) The standalone-shell manifest serves at the well-known path with
    /// the manifest content type and declares `display: standalone` (an
    /// installed home-screen shortcut opens chromeless).
    #[tokio::test]
    async fn web_manifest_served_standalone() {
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(Request::builder().uri("/manifest.webmanifest").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let ct = response.headers().get("content-type").unwrap().to_str().unwrap().to_string();
        assert!(ct.starts_with("application/manifest+json"), "content-type was `{ct}`");
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["display"], "standalone");
        assert!(json["icons"].as_array().map(|a| !a.is_empty()).unwrap_or(false));
    }

    /// (#1403) Each app-icon route serves real PNG bytes (magic header) as
    /// `image/png` — the iOS home-screen icon + the manifest icons.
    #[tokio::test]
    async fn app_icons_served_as_png() {
        for path in ["/apple-touch-icon.png", "/icon-192.png", "/icon-512.png"] {
            let app = build_router(PathBuf::new());
            let response = app
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path} status");
            assert_eq!(response.headers().get("content-type").unwrap(), "image/png", "{path} ct");
            let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
            // PNG signature: 89 50 4E 47 0D 0A 1A 0A.
            assert!(bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]), "{path} not a PNG");
        }
    }

    /// (#1403) Both HTML shells (the viewer and the mission-graph page) declare
    /// the standalone-app shell — manifest link + apple-touch-icon — so a
    /// future edit that drops the shell fails a test rather than silently
    /// breaking the home-screen-shortcut experience.
    #[test]
    fn html_shells_declare_standalone_app() {
        for (name, html) in [("viewer", VIEWER_HTML), ("mission-graph", MISSION_GRAPH_HTML)] {
            assert!(
                html.contains("rel=\"manifest\"") && html.contains("/manifest.webmanifest"),
                "{name}.html lost its web-manifest link (#1403 standalone shell)"
            );
            assert!(
                html.contains("apple-touch-icon"),
                "{name}.html lost its apple-touch-icon link (#1403 standalone shell)"
            );
            assert!(
                html.contains("apple-mobile-web-app-capable"),
                "{name}.html lost the apple-mobile-web-app-capable meta (#1403)"
            );
        }
    }

    /// (#1403) The graph.json StepRow wire contract now carries the raw `kind`
    /// the page gates the per-step token/turn meter on. Pins it so a
    /// rename/removal fails here rather than silently disabling every step
    /// meter on the mission-graph page.
    #[test]
    fn step_row_serializes_raw_kind() {
        let row = crate::mission_graph::StepRow {
            id: "s1".to_string(),
            label: "Dispatch".to_string(),
            kind: "dispatch.internal".to_string(),
            status: "running",
            started_ts: None,
            completed_ts: None,
            tokens_final: None,
            turns_final: None,
            cloud: None,
        };
        let v = serde_json::to_value(&row).unwrap();
        assert_eq!(v["kind"], "dispatch.internal");
        assert_eq!(v["label"], "Dispatch");
        // (#1432 item 4) Backfill fields are absent when None (lenient wire).
        assert!(v.get("tokensFinal").is_none());
        assert!(v.get("turnsFinal").is_none());
        assert!(v.get("cloud").is_none());
    }

    /// (#881 parity) The graph.json route must ride the SAME remote-only
    /// bearer gate as `/missions`/`/phases`/`/lab/runs` — guards against a
    /// future refactor special-casing the mission-graph surface out of
    /// `auth_mw`. Mirrors `lab_runs_requires_token_from_remote_peer`
    /// exactly.
    #[tokio::test]
    #[serial_test::serial]
    async fn mission_graph_json_requires_token_from_remote_peer() {
        unsafe {
            std::env::set_var("DARKMUX_SERVE_TOKEN", TEST_TOKEN);
        }
        let app = build_router(PathBuf::new());
        let mut req = Request::builder()
            .uri("/mission/some-mission/graph.json")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(remote_peer());
        let resp = app.oneshot(req).await.unwrap();
        unsafe {
            std::env::remove_var("DARKMUX_SERVE_TOKEN");
        }
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// Same parity check for the HTML page and the vendor bundle routes —
    /// all three ride the same `timed` router group as `/`/`/play/:date`.
    #[tokio::test]
    #[serial_test::serial]
    async fn mission_graph_html_and_vendor_bundle_require_token_from_remote_peer() {
        unsafe {
            std::env::set_var("DARKMUX_SERVE_TOKEN", TEST_TOKEN);
        }
        let app = build_router(PathBuf::new());
        for uri in ["/mission/some-mission/graph", "/vendor/reactflow-bundle.min.js", "/vendor/reactflow-bundle.min.css"] {
            let mut req = Request::builder().uri(uri).body(Body::empty()).unwrap();
            req.extensions_mut().insert(remote_peer());
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{uri} should require a token from a remote peer");
        }
        unsafe {
            std::env::remove_var("DARKMUX_SERVE_TOKEN");
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn mission_graph_json_returns_404_for_unknown_mission() {
        let _guard = CrewDirGuard::new();
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/mission/does-not-exist/graph.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn mission_graph_json_rejects_invalid_id() {
        let _guard = CrewDirGuard::new();
        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/mission/..%2F..%2Fetc/graph.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// A mission with phase data but NO task/step graph underneath any
    /// phase (a legacy pre-registry instance, or a freeform hand-authored
    /// mission) renders phases-only with a note — never an error (item 6,
    /// graceful degradation).
    #[tokio::test]
    #[serial_test::serial]
    async fn mission_graph_json_legacy_mission_is_phases_only_with_a_note() {
        let _guard = CrewDirGuard::new();
        let mission = minimal_mission("legacy-mission", vec!["only-phase".to_string()]);
        save_test_mission(&mission);
        save_test_phase(&minimal_phase("only-phase", "legacy-mission"));

        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/mission/legacy-mission/graph.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["legacy"], true);
        assert!(json["note"].is_string(), "legacy mission must carry a note: {json}");
        let nodes = json["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 1, "phases-only: exactly the one phase node, no tasks/steps: {nodes:?}");
        assert_eq!(nodes[0]["kind"], "phase");
        assert_eq!(nodes[0]["id"], "only-phase");
        assert_eq!(json["edges"].as_array().unwrap().len(), 0);
    }

    /// Full shape test: a hand-built mission with a fan-in (two upstream
    /// tasks feeding one downstream task) exercises phase→task containment
    /// AND task-level `depends_on` edges (#1401: steps render as ROWS
    /// inside their owning task node, not separate nodes — the derived
    /// step-granularity fan-in edges retired along with the step nodes
    /// they connected; the fan-in now shows as two edges converging
    /// directly on the downstream TASK).
    #[tokio::test]
    #[serial_test::serial]
    async fn mission_graph_json_fan_in_shape() {
        let _guard = CrewDirGuard::new();
        let mission_id = "fanin-mission";
        let mission = minimal_mission(mission_id, vec!["p1".to_string()]);
        save_test_mission(&mission);
        save_test_phase(&minimal_phase("p1", mission_id));

        let task_a = darkmux_crew::types::Task {
            id: "task-a".to_string(),
            phase_id: "p1".to_string(),
            description: "task a".to_string(),
            display_name: None,
            step_ids: vec!["task-a-step".to_string()],
            depends_on: vec![],
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let task_b = darkmux_crew::types::Task {
            id: "task-b".to_string(),
            phase_id: "p1".to_string(),
            description: "task b".to_string(),
            display_name: None,
            step_ids: vec!["task-b-step".to_string()],
            depends_on: vec![],
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let task_c = darkmux_crew::types::Task {
            id: "task-c".to_string(),
            phase_id: "p1".to_string(),
            description: "task c (dedup-like)".to_string(),
            display_name: Some("Dedup".to_string()),
            step_ids: vec!["task-c-step".to_string()],
            depends_on: vec!["task-a".to_string(), "task-b".to_string()],
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        for t in [&task_a, &task_b, &task_c] {
            darkmux_crew::lifecycle::save_task(mission_id, t).unwrap();
        }
        let step_a = darkmux_crew::types::Step {
            id: "task-a-step".to_string(),
            task_id: "task-a".to_string(),
            kind: "procedural.noop".to_string(),
            status: darkmux_crew::types::NodeStatus::Complete,
            config: serde_json::Value::Null,
            started_ts: Some(1),
            completed_ts: Some(2),
            output: None,
        };
        let step_b = darkmux_crew::types::Step {
            id: "task-b-step".to_string(),
            task_id: "task-b".to_string(),
            kind: "procedural.noop".to_string(),
            status: darkmux_crew::types::NodeStatus::Running,
            config: serde_json::Value::Null,
            started_ts: Some(1),
            completed_ts: None,
            output: None,
        };
        let step_c = darkmux_crew::types::Step {
            id: "task-c-step".to_string(),
            task_id: "task-c".to_string(),
            kind: "procedural.noop".to_string(),
            status: darkmux_crew::types::NodeStatus::Planned,
            config: serde_json::Value::Null,
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        for s in [&step_a, &step_b, &step_c] {
            darkmux_crew::lifecycle::save_step(mission_id, "p1", s).unwrap();
        }

        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/mission/{mission_id}/graph.json"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["legacy"], false);

        let nodes = json["nodes"].as_array().unwrap();
        // (#1401) 1 phase + 3 tasks — NO separate step nodes anymore.
        assert_eq!(nodes.len(), 4, "{nodes:?}");
        let node = |id: &str| nodes.iter().find(|n| n["id"] == id).unwrap_or_else(|| panic!("missing node {id}: {nodes:?}"));
        assert_eq!(node("task-a")["status"], "complete");
        assert_eq!(node("task-b")["status"], "running");
        assert_eq!(node("task-c")["status"], "planned");

        // (#1398) label falls back to `id` when no display_name is set;
        // task-c's explicit display_name wins over its id.
        assert_eq!(node("task-a")["label"], "task-a");
        assert_eq!(node("task-c")["label"], "Dedup");

        // (#1401) The steps a task carries render as an array of rows on
        // ITS OWN node — kind display name resolved via `resolve_step_label`
        // ("procedural.noop" -> "No-op").
        let task_a_steps = node("task-a")["steps"].as_array().unwrap();
        assert_eq!(task_a_steps.len(), 1, "{task_a_steps:?}");
        assert_eq!(task_a_steps[0]["id"], "task-a-step");
        assert_eq!(task_a_steps[0]["label"], "No-op");
        assert_eq!(task_a_steps[0]["status"], "complete");
        assert_eq!(task_a_steps[0]["startedTs"], 1);
        assert_eq!(task_a_steps[0]["completedTs"], 2);
        let task_b_steps = node("task-b")["steps"].as_array().unwrap();
        assert_eq!(task_b_steps[0]["status"], "running");

        // ── Wire-casing CONTRACT (review-gate MF1). The page's JS reads
        // `n.parentId` in computeLayout — the first cut serialized
        // `parent_id` while the JS read `parentId`, so every task grouped
        // under a missing parent and the whole layout collapsed to the
        // origin. Pin the EXACT camelCase keys the JS reads on node
        // objects, the ABSENCE of the snake_case forms, and the envelope's
        // deliberate snake_case (the JS reads `g.mission_id`), so a rename
        // on either side fails here instead of silently flattening the
        // layout.
        assert_eq!(node("task-a")["parentId"], "p1");
        assert!(
            node("task-a")["parent_id"].is_null(),
            "snake_case parent_id must NOT be on the wire (JS reads parentId)"
        );
        assert!(
            task_a_steps[0]["started_ts"].is_null(),
            "snake_case started_ts must NOT be on a step row's wire form (camelCase contract)"
        );
        assert!(node("task-a")["depth"].is_number());
        assert!(json["mission_id"].is_string(), "envelope stays snake_case (JS reads g.mission_id)");
        assert!(json["mission_status"].is_string(), "envelope stays snake_case (JS reads g.mission_status)");

        let edges = json["edges"].as_array().unwrap();
        let has_edge = |source: &str, target: &str, kind: &str| {
            edges.iter().any(|e| e["source"] == source && e["target"] == target && e["kind"] == kind)
        };
        assert!(has_edge("p1", "task-a", "contains"));
        assert!(has_edge("p1", "task-b", "contains"));
        assert!(has_edge("p1", "task-c", "contains"));
        // The fan-in: BOTH upstream TASKS converge directly on the
        // downstream task — no step-level detour (#1401).
        assert!(has_edge("task-a", "task-c", "depends_on"));
        assert!(has_edge("task-b", "task-c", "depends_on"));
        // No "contains" edges into step ids — steps aren't nodes anymore.
        assert!(!edges.iter().any(|e| e["target"] == "task-a-step"));

        // (review-gate C7) No duplicate edge ids — React keys must be
        // unique; a duplicate depends_on entry on a Task must not emit
        // the same edge twice.
        let mut ids: Vec<&str> = edges.iter().filter_map(|e| e["id"].as_str()).collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate edge ids in {edges:?}");
    }

    /// (review-gate MF2a) The three production graph runners persist Step
    /// JSONs only AFTER `run_step_graph` returns — so a page opened DURING
    /// a run sees tasks whose `step_ids` name steps with no file on disk.
    /// The builder must synthesize `planned` step ROWS for those ids
    /// (#1401 — otherwise the SSE layer has nothing to flip when the
    /// scheduler's `step start` records arrive). This fixture is exactly
    /// that mid-run shape: tasks persisted with `step_ids` + `depends_on`,
    /// ZERO step files, and NO config-snapshot.json (a hand-built test
    /// fixture, not a config-launched mission) — so the kind-recovery path
    /// (#1402) has nothing to recover from, and the row's label falls back
    /// to the step id itself (the fallback chain's SECOND-to-last rung,
    /// not the deep "unknown" — see `resolve_step_label`'s doc).
    #[tokio::test]
    #[serial_test::serial]
    async fn mission_graph_json_synthesizes_planned_steps_for_unpersisted_step_files() {
        let _guard = CrewDirGuard::new();
        let mission_id = "midrun-mission";
        save_test_mission(&minimal_mission(mission_id, vec!["p1".to_string()]));
        save_test_phase(&minimal_phase("p1", mission_id));

        let task_a = darkmux_crew::types::Task {
            id: "task-a".to_string(),
            phase_id: "p1".to_string(),
            description: "task a".to_string(),
            display_name: None,
            step_ids: vec!["task-a-step".to_string()],
            depends_on: vec![],
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let task_b = darkmux_crew::types::Task {
            id: "task-b".to_string(),
            phase_id: "p1".to_string(),
            description: "task b".to_string(),
            display_name: None,
            // Two steps — both must synthesize a row, in order.
            step_ids: vec!["task-b-step-1".to_string(), "task-b-step-2".to_string()],
            depends_on: vec!["task-a".to_string()],
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        for t in [&task_a, &task_b] {
            darkmux_crew::lifecycle::save_task(mission_id, t).unwrap();
        }
        // Deliberately NO save_step calls — the mid-run disk state.

        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/mission/{mission_id}/graph.json"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["legacy"], false);

        let nodes = json["nodes"].as_array().unwrap();
        // (#1401) 1 phase + 2 tasks — NO separate step nodes, synthesized
        // or otherwise.
        assert_eq!(nodes.len(), 3, "{nodes:?}");
        let node = |id: &str| {
            nodes
                .iter()
                .find(|n| n["id"] == id)
                .unwrap_or_else(|| panic!("missing node {id}: {nodes:?}"))
        };
        assert_eq!(node("task-a")["status"], "planned");
        assert_eq!(node("task-b")["status"], "planned");

        let task_a_steps = node("task-a")["steps"].as_array().unwrap();
        assert_eq!(task_a_steps.len(), 1);
        assert_eq!(task_a_steps[0]["id"], "task-a-step");
        assert_eq!(task_a_steps[0]["status"], "planned", "synthesized step must be planned: {task_a_steps:?}");
        assert_eq!(
            task_a_steps[0]["label"], "task-a-step",
            "no config-snapshot to recover a kind from -> label falls back to the step id \
             itself, not the deep 'unknown' (#1402): {task_a_steps:?}"
        );

        let task_b_steps = node("task-b")["steps"].as_array().unwrap();
        assert_eq!(task_b_steps.len(), 2, "both synthesized rows present, in step_ids order");
        assert_eq!(task_b_steps[0]["id"], "task-b-step-1");
        assert_eq!(task_b_steps[1]["id"], "task-b-step-2");
        for row in task_b_steps {
            assert_eq!(row["status"], "planned");
        }

        // Every dependency edge's BOTH endpoints exist as (task) nodes.
        let node_ids: std::collections::BTreeSet<&str> =
            nodes.iter().filter_map(|n| n["id"].as_str()).collect();
        let edges = json["edges"].as_array().unwrap();
        for e in edges {
            for end in ["source", "target"] {
                let id = e[end].as_str().unwrap();
                assert!(node_ids.contains(id), "edge {e} references missing node {id}");
            }
        }
        // Cross-task dep edge connects the TASKS directly (#1401) — no
        // step-level detour, synthesized or otherwise.
        assert!(edges
            .iter()
            .any(|e| e["source"] == "task-a" && e["target"] == "task-b" && e["kind"] == "depends_on"));
    }

    /// (review-gate C2) Pin the SSE action-string contract: the page's
    /// STATUS_ACTIONS map must contain every action string the emitting
    /// side actually writes (`scheduler::step_lifecycle_record`'s
    /// "step start"/"step complete"/"step error";
    /// `lifecycle::emit_phase_transition_record`'s "phase start"/
    /// "phase complete"/"phase abandon"; the mission transition verbs).
    /// A rename on either side must fail a test, not silently kill the
    /// live animation. The emit side is pinned by darkmux-crew's own
    /// `run_step_graph_emits_step_start_and_step_complete_records`; this
    /// pins the page side against the same literals.
    #[test]
    fn mission_graph_page_pins_flow_action_strings() {
        for action in [
            "step start",
            "step complete",
            "step error",
            "phase start",
            "phase complete",
            "phase abandon",
            "mission start",
            "mission close",
            "mission pause",
            "mission resume",
        ] {
            assert!(
                MISSION_GRAPH_HTML.contains(&format!("\"{action}\"")),
                "mission-graph.html lost the \"{action}\" entry from its STATUS_ACTIONS map — \
                 the SSE delta layer silently stops animating that transition"
            );
        }
    }

    /// (F3, #1397/#1399 gate remediation) The MECHANICAL tie between the
    /// emit-side vocabulary constant and the page-side consumer: iterates
    /// `darkmux_crew::scheduler::STEP_LIFECYCLE_ACTIONS` itself (not
    /// re-typed literals) and asserts each string appears in the embedded
    /// mission-graph asset. The test above pins the page against literals
    /// (covering the phase/mission verbs too, which have no shared Rust
    /// constant yet); this one guarantees that if the SCHEDULER's canonical
    /// step vocabulary ever changes, this crate fails to build/pass until
    /// the page's STATUS_ACTIONS map catches up — the two hand-maintained
    /// lists (Rust constant, JS map) can no longer drift silently.
    #[test]
    fn mission_graph_page_contains_every_scheduler_step_lifecycle_action() {
        for action in darkmux_crew::scheduler::STEP_LIFECYCLE_ACTIONS {
            assert!(
                MISSION_GRAPH_HTML.contains(&format!("\"{action}\"")),
                "mission-graph.html's STATUS_ACTIONS map is missing scheduler \
                 STEP_LIFECYCLE_ACTIONS entry \"{action}\" — the graph lens would silently \
                 stop animating that step transition"
            );
        }
    }

    /// (item 7) Shape test against a REAL review-config-interpreted graph:
    /// loads the embedded `review` mission config, interprets it with a
    /// launcher supplying two probe seats (mirroring how `pr-review run`
    /// would resolve `staffing` at launch), persists the resulting
    /// Mission/Phase/Task/Step graph, and asserts the graph.json node
    /// counts match what `review.json`'s own shape (3 phases; investigate
    /// = bundle + N probe + dedup; adjudicate = judge; report = verify +
    /// synthesis) predicts for N=2 probe seats: 3 phase nodes, 7 task nodes
    /// (bundle, probe-0, probe-1, dedup, judge, verify, synthesis), 7 step
    /// nodes (every task here is single-step).
    #[tokio::test]
    #[serial_test::serial]
    async fn mission_graph_json_review_config_interpreted_shape() {
        let _guard = CrewDirGuard::new();
        let loaded = darkmux_crew::mission_config::load("review").expect("embedded review config loads");

        let mut expansions = std::collections::BTreeMap::new();
        expansions.insert(
            "probe_seats".to_string(),
            vec!["seat-a".to_string(), "seat-b".to_string()],
        );
        let params = darkmux_crew::mission_config::LaunchParams {
            phase_ids: std::collections::BTreeMap::new(),
            task_overrides: std::collections::BTreeMap::new(),
            step_config_overrides: std::collections::BTreeMap::new(),
            expansions,
        };
        let (tasks, steps, _warnings) = darkmux_crew::mission_config::interpret(&loaded.config, &params)
            .expect("interpret succeeds against the embedded review config");

        let mission_id = "review-shape-mission";
        let phase_ids: Vec<String> = loaded.config.phases.iter().map(|p| p.id.clone()).collect();
        save_test_mission(&minimal_mission(mission_id, phase_ids.clone()));
        for phase_id in &phase_ids {
            save_test_phase(&minimal_phase(phase_id, mission_id));
        }
        for task in &tasks {
            darkmux_crew::lifecycle::save_task(mission_id, task).unwrap();
        }
        for step in steps.values() {
            darkmux_crew::lifecycle::save_step(mission_id, &tasks.iter().find(|t| t.id == step.task_id).unwrap().phase_id, step).unwrap();
        }

        let app = build_router(PathBuf::new());
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/mission/{mission_id}/graph.json"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["legacy"], false);

        let nodes = json["nodes"].as_array().unwrap();
        let count_kind = |kind: &str| nodes.iter().filter(|n| n["kind"] == kind).count();
        assert_eq!(count_kind("phase"), 3, "{nodes:?}");
        assert_eq!(count_kind("task"), 7, "expected bundle+2 probes+dedup+judge+verify+synthesis: {nodes:?}");
        // (#1401) No separate "step" node kind anymore — every task's
        // steps render as rows on ITS OWN node instead. Sum the `steps`
        // array lengths across every task node: still 7 (every task in
        // review.json is single-step), just no longer a separate node per
        // step.
        assert_eq!(count_kind("step"), 0, "steps are rows now, not nodes (#1401): {nodes:?}");
        let total_step_rows: usize = nodes
            .iter()
            .filter(|n| n["kind"] == "task")
            .map(|n| n["steps"].as_array().map(|a| a.len()).unwrap_or(0))
            .sum();
        assert_eq!(total_step_rows, 7, "every task in review.json is single-step: {nodes:?}");
        assert_eq!(tasks.len(), 7);
        assert_eq!(steps.len(), 7);

        // (#1402) Every row's label resolves through the real StepKind
        // display-name fallback chain — a probe row (rendered per-seat
        // kind id "review.probe:seat-a") still reads "Probe", the base
        // label, not the raw suffixed id.
        let all_labels: Vec<String> = nodes
            .iter()
            .filter(|n| n["kind"] == "task")
            .flat_map(|n| n["steps"].as_array().cloned().unwrap_or_default())
            .filter_map(|row| row["label"].as_str().map(String::from))
            .collect();
        for expected in ["Bundle", "Probe", "Dedup", "Judge", "Verify", "Synthesis"] {
            assert!(
                all_labels.iter().any(|l| l == expected),
                "expected a \"{expected}\" row label among {all_labels:?}"
            );
        }
    }

    /// (#1432 item 4) graph.json backfills a COMPLETED step's finalized
    /// token/turn totals from the daemon's flow records at page load — so a
    /// page opened after the dispatch ran shows the real total, not the idle
    /// "· tok" placeholder (the SSE stream being tail-from-now). A step with
    /// NO folded record stays absent (honest "no data", never a wrong zero).
    #[tokio::test]
    #[serial_test::serial]
    async fn mission_graph_json_backfills_completed_step_totals_from_flow_records() {
        use darkmux_crew::types::{NodeStatus, Step, Task};
        let _guard = CrewDirGuard::new();
        let mission_id = "backfill-mission";
        let phase_id = "p1";
        save_test_mission(&minimal_mission(mission_id, vec![phase_id.to_string()]));
        save_test_phase(&minimal_phase(phase_id, mission_id));

        // Two steps: one that ran (has a flow record), one that never did.
        let mk_task = |id: &str, step: &str| Task {
            id: id.to_string(),
            phase_id: phase_id.to_string(),
            description: format!("task {id}"),
            display_name: None,
            step_ids: vec![step.to_string()],
            depends_on: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let mk_step = |id: &str, status: NodeStatus| Step {
            id: id.to_string(),
            task_id: format!("{id}-task"),
            kind: "dispatch.internal".to_string(),
            status,
            config: serde_json::Value::Null,
            started_ts: Some(now_unix()),
            completed_ts: if status == NodeStatus::Complete { Some(now_unix()) } else { None },
            output: None,
        };
        darkmux_crew::lifecycle::save_task(mission_id, &mk_task("ran-task", "ran-step")).unwrap();
        darkmux_crew::lifecycle::save_task(mission_id, &mk_task("idle-task", "idle-step")).unwrap();
        darkmux_crew::lifecycle::save_step(mission_id, phase_id, &mk_step("ran-step", NodeStatus::Complete)).unwrap();
        darkmux_crew::lifecycle::save_step(mission_id, phase_id, &mk_step("idle-step", NodeStatus::Planned)).unwrap();

        // A flows_dir carrying a dispatch-complete record for the ran step
        // only. Named for TODAY's day stem (#1445 gate): the backfill scans
        // only day files inside the mission's lifetime window, and this
        // mission was just minted with created_ts = now — a hardcoded date
        // would silently fall out of the window when run later.
        let flows = TempDir::new().unwrap();
        let rec = serde_json::json!({
            "action": "dispatch complete",
            "session_id": "step-ran-step",
            "payload": { "total_tokens": 12345, "total_turns": 7 }
        });
        let today = crate::mission_graph::epoch_days_to_stem((now_unix() / 86400) as i64);
        std::fs::write(
            flows.path().join(format!("{today}.jsonl")),
            format!("{}\n", serde_json::to_string(&rec).unwrap()),
        )
        .unwrap();

        let app = build_router(flows.path().to_path_buf());
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/mission/{mission_id}/graph.json"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let rows: Vec<serde_json::Value> = json["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["kind"] == "task")
            .flat_map(|n| n["steps"].as_array().cloned().unwrap_or_default())
            .collect();
        let ran = rows.iter().find(|r| r["id"] == "ran-step").expect("ran-step row");
        assert_eq!(ran["tokensFinal"], 12345, "completed step backfilled from flow records");
        assert_eq!(ran["turnsFinal"], 7);
        let idle = rows.iter().find(|r| r["id"] == "idle-step").expect("idle-step row");
        assert!(idle.get("tokensFinal").is_none(), "never-ran step has no backfill (honest absent)");
        assert!(idle.get("turnsFinal").is_none());
    }

    /// (#1432 item 4) A mixed-era mission (steps ran, but the daemon has NO
    /// flow files on disk — e.g. rotated away, or a fresh checkout) backfills
    /// nothing rather than erroring or inventing zeros. Same code path, empty
    /// flows_dir.
    #[tokio::test]
    #[serial_test::serial]
    async fn mission_graph_json_backfill_absent_when_no_flow_records() {
        use darkmux_crew::types::{NodeStatus, Step, Task};
        let _guard = CrewDirGuard::new();
        let mission_id = "no-records-mission";
        let phase_id = "p1";
        save_test_mission(&minimal_mission(mission_id, vec![phase_id.to_string()]));
        save_test_phase(&minimal_phase(phase_id, mission_id));
        let task = Task {
            id: "t1".to_string(),
            phase_id: phase_id.to_string(),
            description: "t1".to_string(),
            display_name: None,
            step_ids: vec!["s1".to_string()],
            depends_on: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        darkmux_crew::lifecycle::save_task(mission_id, &task).unwrap();
        darkmux_crew::lifecycle::save_step(
            mission_id,
            phase_id,
            &Step {
                id: "s1".to_string(),
                task_id: "t1".to_string(),
                kind: "dispatch.internal".to_string(),
                status: NodeStatus::Complete,
                config: serde_json::Value::Null,
                started_ts: Some(now_unix()),
                completed_ts: Some(now_unix()),
                output: None,
            },
        )
        .unwrap();

        // Empty flows_dir (like the test router's own PathBuf::new()).
        let empty = TempDir::new().unwrap();
        let app = build_router(empty.path().to_path_buf());
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/mission/{mission_id}/graph.json"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let row = json["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["kind"] == "task")
            .flat_map(|n| n["steps"].as_array().cloned().unwrap_or_default())
            .find(|r| r["id"] == "s1")
            .expect("s1 row");
        assert!(row.get("tokensFinal").is_none(), "no flow records -> no backfill");
    }
