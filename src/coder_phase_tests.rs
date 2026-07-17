    use super::*;
    use crew::types::{NodeStatus, Phase, PhaseStatus, Task};

    /// (#1402) `darkmux-serve`'s `mission_graph::mission_step_kind_display_name`
    /// is a STATIC literal table (that crate can't depend on this root
    /// binary crate — see its own doc for why) for the three `mission.*`
    /// Tier 3 kinds this module owns. This is the "conformance test in a
    /// crate that sees both" #1352's tiering doctrine asks for instead of
    /// unguarded duplication: THIS crate depends on `darkmux-serve` (to
    /// embed the daemon) AND owns these kinds, so it's the one place that
    /// can pin the static table against the REAL `StepKind::display_name()`
    /// each kind returns.
    #[test]
    fn mission_step_kind_display_names_match_this_crates_static_table() {
        let worktree = MissionWorktreeStepKind {
            repo_root: std::path::PathBuf::new(),
            wt_path: std::path::PathBuf::new(),
            branch: String::new(),
            base: String::new(),
            mission_id: String::new(),
            phase_id: String::new(),
            session_id: String::new(),
            role: String::new(),
        };
        let coder = MissionCoderStepKind {
            opts: Mutex::new(None),
            wt_path: std::path::PathBuf::new(),
            mission_id: String::new(),
            phase_id: String::new(),
            session_id: String::new(),
            role_id: String::new(),
            result_slot: Arc::new(Mutex::new(None)),
        };
        let verify = MissionVerifyStepKind {
            wt_path: std::path::PathBuf::new(),
            base: String::new(),
            phase_id: String::new(),
            result_slot: Arc::new(Mutex::new(None)),
        };
        for (id, display) in [
            (worktree.id(), worktree.display_name()),
            (coder.id(), coder.display_name()),
            (verify.id(), verify.display_name()),
        ] {
            assert_eq!(
                darkmux_serve::mission_graph::mission_step_kind_display_name(id),
                Some(display),
                "darkmux-serve's static mission_step_kind_display_name(\"{id}\") table has \
                 drifted from this crate's live StepKind::display_name()"
            );
        }
    }

    /// (#816) conventions_branch: template + ticket → conventioned ref;
    /// ticketless mission or invalid expansion → darkmux default (soft
    /// fallback, never an error).
    #[test]
    fn conventions_branch_expands_and_falls_back() {
        let s = phase("s1-fix", "m1", PhaseStatus::Planned);
        let mut m = mission("m1", "desc");
        let conv: crate::conventions::Conventions =
            serde_json::from_str(r#"{"branch_template":"{ticket}/{phase}"}"#).unwrap();
        // ticketless → default
        assert_eq!(conventions_branch(&s, &m, Some(&conv)), "darkmux/s1-fix");
        // ticketed → conventioned
        m.ticket = Some("SYS-2598".into());
        assert_eq!(conventions_branch(&s, &m, Some(&conv)), "SYS-2598/s1-fix");
        // no conventions at all → default
        assert_eq!(conventions_branch(&s, &m, None), "darkmux/s1-fix");
        // template expanding to an invalid ref → default
        let bad: crate::conventions::Conventions =
            serde_json::from_str(r#"{"branch_template":"-{phase}"}"#).unwrap();
        assert_eq!(conventions_branch(&s, &m, Some(&bad)), "darkmux/s1-fix");
    }

    /// (#815) With a mission-level source_input, the coder brief carries the
    /// compiled description AND the verbatim operator prose under the
    /// provenance-tagged block; without one (hand-authored / pre-#815
    /// missions) the brief is the bare description, unchanged.
    #[test]
    fn coder_brief_appends_verbatim_source_when_present() {
        let s = phase("s1", "m1", PhaseStatus::Planned);
        let mut m = mission("m1", "compiled summary");
        m.source_input = Some("EXACT placeholder: 'APIM Key Name'. Do NOT rename fields.".into());
        let brief = coder_brief(&s, &m, &[], &[], &[]);
        assert!(brief.starts_with("desc s1"), "compiled description leads");
        assert!(brief.contains("<operator-source-input>"), "provenance tag present");
        assert!(brief.contains("Do NOT rename fields."), "verbatim constraint survives");
        assert!(brief.contains("THIS text is authoritative"), "authority statement present");
        // The preamble must read as clean prose — no literal space-runs from
        // string-continuation mistakes (QA caught exactly this on the first
        // cut; the model-facing text is the product here).
        assert!(
            !brief.contains("  "),
            "brief preamble contains a literal space-run: {brief:?}"
        );
        assert!(brief.contains("unabridged request that produced this phase"));
    }

    #[test]
    fn coder_brief_is_bare_description_without_source() {
        let s = phase("s1", "m1", PhaseStatus::Planned);
        let m = mission("m1", "compiled summary");
        assert_eq!(coder_brief(&s, &m, &[], &[], &[]), "desc s1");
        // Whitespace-only source_input behaves as absent.
        let mut m2 = mission("m1", "compiled summary");
        m2.source_input = Some("   \n ".into());
        assert_eq!(coder_brief(&s, &m2, &[], &[], &[]), "desc s1");
    }

    #[test]
    fn coder_brief_injects_prior_corrections() {
        let s = phase("s1", "m1", PhaseStatus::Planned);
        let m = mission("m1", "compiled summary");
        let corrections = vec![
            "Do NOT rename the APIM key field.".to_string(),
            "The verify command is `cargo test -p foo`, not the workspace default.".to_string(),
        ];
        let brief = coder_brief(&s, &m, &[], &corrections, &[]);
        assert!(brief.starts_with("desc s1"), "base description leads: {brief:?}");
        assert!(
            brief.contains("<prior-adjudication-corrections>"),
            "corrections block present: {brief:?}"
        );
        assert!(brief.contains("- Do NOT rename the APIM key field."), "{brief:?}");
        assert!(brief.contains("- The verify command is `cargo test -p foo`"), "{brief:?}");
        // (#453) Corrections are framed as findings-to-verify, not directives —
        // the reframe of the prior "Honor them — do not re-make these mistakes"
        // anchoring framing. Assert the verify-against-workspace framing is present.
        assert!(
            brief.contains("not a fact about your current workspace"),
            "hypothesis-to-verify framing present: {brief:?}"
        );
        assert!(
            !brief.contains("Honor them"),
            "old anchoring framing must be gone: {brief:?}"
        );
        // Injected preamble prose must read clean — no literal space-runs from
        // string-continuation slips (the source-input block has the same guard;
        // the test notes here are space-run-free, so this covers the framing).
        assert!(!brief.contains("  "), "injected block has a space-run: {brief:?}");
        // Empty corrections leave the brief unchanged — the no-op the dispatch
        // hits on an honest first run with no prior adjudication.
        assert_eq!(coder_brief(&s, &m, &[], &[], &[]), "desc s1");
    }

    /// (#994 retrieve+inject) The detected-cautions block injects independently
    /// of the corrections block (either / both / neither), names the firing's
    /// file as the "where", carries the findings-to-verify framing (#453), and
    /// orders authored corrections before auto-detected cautions.
    #[test]
    fn coder_brief_injects_detected_cautions() {
        let s = phase("s1", "m1", PhaseStatus::Planned);
        let m = mission("m1", "compiled summary");
        let cautions = vec![
            "- [cycle] `edit` called 3× in the last 10 tool calls (in `src/x.rs`)".to_string(),
            "- [reasoning-loop] same reasoning repeated 3× in 6 turns".to_string(),
        ];

        // Cautions alone (no corrections).
        let brief = coder_brief(&s, &m, &[], &[], &cautions);
        assert!(brief.starts_with("desc s1"), "base description leads: {brief:?}");
        assert!(brief.contains("<detected-cautions>"), "cautions block present: {brief:?}");
        assert!(brief.contains("- [cycle] `edit` called 3×"), "{brief:?}");
        assert!(brief.contains("(in `src/x.rs`)"), "the file 'where' survives: {brief:?}");
        assert!(
            brief.contains("not facts about your current workspace"),
            "findings-to-verify framing present: {brief:?}"
        );
        assert!(
            !brief.contains("<prior-adjudication-corrections>"),
            "no corrections block when corrections are empty: {brief:?}"
        );
        // Model-facing prose must read clean — no string-continuation space-runs.
        assert!(!brief.contains("  "), "cautions block has a space-run: {brief:?}");

        // Both blocks coexist; authored corrections precede auto-detected cautions.
        let corrections = vec!["Do not rename the field.".to_string()];
        let both = coder_brief(&s, &m, &[], &corrections, &cautions);
        let corr_at = both.find("<prior-adjudication-corrections>").unwrap();
        let caut_at = both.find("<detected-cautions>").unwrap();
        assert!(corr_at < caut_at, "corrections (authored) precede detected cautions: {both:?}");

        // Neither → bare description.
        assert_eq!(coder_brief(&s, &m, &[], &[], &[]), "desc s1");
    }

    /// (#994) The lessons block injects independently, carries the FOLLOW
    /// framing (authoritative, not verify — distinct from cautions), and orders
    /// base → lessons → corrections → cautions.
    #[test]
    fn coder_brief_injects_lessons_authoritative_and_first() {
        let s = phase("s1", "m1", PhaseStatus::Planned);
        let m = mission("m1", "compiled summary");
        let lessons =
            vec!["- American English: house style across all work, no British spellings.".to_string()];
        let corrections = vec!["Do not rename the field.".to_string()];
        let cautions = vec!["- [cycle] looped on src/x.rs".to_string()];

        // Lessons alone.
        let brief = coder_brief(&s, &m, &lessons, &[], &[]);
        assert!(brief.starts_with("desc s1"), "task leads: {brief:?}");
        assert!(brief.contains("<lessons>"), "lessons block present: {brief:?}");
        assert!(brief.contains("American English"), "{brief:?}");
        assert!(
            brief.contains("Treat them as authoritative"),
            "FOLLOW framing present (distinct from cautions' verify framing): {brief:?}"
        );
        assert!(!brief.contains("  "), "lessons block has a space-run: {brief:?}");

        // All three present: base → lessons → corrections → cautions.
        let all = coder_brief(&s, &m, &lessons, &corrections, &cautions);
        let less_at = all.find("<lessons>").unwrap();
        let corr_at = all.find("<prior-adjudication-corrections>").unwrap();
        let caut_at = all.find("<detected-cautions>").unwrap();
        assert!(
            less_at < corr_at && corr_at < caut_at,
            "authored lessons lead, then corrections, then auto-detected cautions: {all:?}"
        );

        // Empty lessons → no block.
        assert!(!coder_brief(&s, &m, &[], &corrections, &cautions).contains("<lessons>"));
    }

    /// (#1256 frozen-text / #1426 ship-4) GOLDEN: the FULLY-ASSEMBLED coder
    /// brief with the operator source-input and ALL THREE injected blocks
    /// populated, byte-locked. The launch coder-phase path is now the ONLY
    /// producer of this model-facing text; per the frozen-model-facing-text
    /// contract, "frozen" means one hash — a drive-by rewording of any block's
    /// framing (or a lost block, the class of the retired-verb collapse's
    /// near-miss) fails here, not in production dispatches.
    #[test]
    fn coder_brief_fully_assembled_matches_the_golden_byte_for_byte() {
        let m = {
            let mut m = mission("m-golden", "improve the widget pipeline");
            m.source_input = Some("Original operator request: make widgets faster.".to_string());
            m
        };
        let p = phase("s1", "m-golden", PhaseStatus::Running);
        let brief = coder_brief(
            &p,
            &m,
            &["Always run the linter — CI enforces it.".to_string()],
            &["Do not rename the config field — downstream parses it.".to_string()],
            &["edit loop detected on src/widget.rs in an earlier dispatch".to_string()],
        );
        let golden = r#"desc s1

<operator-source-input>
The user's original, unabridged request that produced this phase. The summary above is derived from it; where this text adds constraints, exact strings, or scope limits beyond the summary, THIS text is authoritative.

Original operator request: make widgets faster.
</operator-source-input>

<lessons>
The user recorded these conventions and decisions for this codebase — the rules the team actually follows and the reasoning behind them. Treat them as authoritative: follow them, and prefer them over a generic default when they conflict. If one is clearly stale against the current code, say so in your final message rather than silently ignoring it:

Always run the linter — CI enforces it.
</lessons>

<prior-adjudication-corrections>
The user's reviewer recorded these corrections while reviewing earlier dispatches in this mission. Treat each as a finding from an earlier context, not a fact about your current workspace. If a correction names a concrete change (a renamed field, a config key, a command, an exact string), check it against the code or by running the command it names, and apply it if it holds. If it names a diagnosis (a race condition, a broken invariant, a failing test), reproduce the specific claim before changing anything: run the test or trace the code path it names. If a correction does not hold against your current workspace, say so in your final message and re-diagnose; if re-diagnosis does not converge quickly, surface the blocker and stop rather than looping:

- Do not rename the config field — downstream parses it.
</prior-adjudication-corrections>

<detected-cautions>
darkmux's loop detectors flagged these patterns in earlier dispatches in this mission — repeated tool calls, looping reasoning, tool-failure cascades. They are signals from earlier contexts, not facts about your current workspace: a pattern that fired earlier may be irrelevant now. Use them to avoid walking back into a known dead end — if you notice yourself about to repeat one, stop and change your approach. None of these is a required action:

edit loop detected on src/widget.rs in an earlier dispatch
</detected-cautions>"#;
        assert_eq!(brief, golden, "the assembled coder brief is frozen model-facing text (#1256)");
    }

    /// (#994) `engagement_lessons` reads the lessons.db store and formats
    /// entries as bullets — the file scope rendered as the "where", the why in
    /// the body. `#[serial]` — mutates DARKMUX_HOME (which collapses both
    /// lessons tiers to one db, so the read is exercised single-store).
    #[test]
    #[serial_test::serial]
    fn engagement_lessons_reads_lessons_db() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        // SAFETY: serialized via #[serial]; restored below.
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()) };

        {
            let conn = crew::lessons::open_at(&crew::lessons::repo_db_path()).unwrap();
            crew::lessons::add(&conn, "American English", "house style across all work", None, None)
                .unwrap();
            crew::lessons::add(
                &conn,
                "Bound retries",
                "the loop entrenches its first answer",
                Some("loop.rs"),
                None,
            )
            .unwrap();
        }
        let conv = engagement_lessons(&std::collections::HashSet::new());

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }

        assert_eq!(conv.len(), 2, "both entries injected (tiers collapse under DARKMUX_HOME): {conv:?}");
        assert!(
            conv.iter().any(|c| c.contains("American English") && c.contains("house style")),
            "{conv:?}"
        );
        assert!(
            conv.iter().any(|c| c.contains("Bound retries") && c.contains("(in `loop.rs`)")),
            "file scope rendered as the 'where': {conv:?}"
        );
    }

    /// (#1002) A lesson scoped to a file this dispatch will touch sorts ahead of
    /// an engagement-level one, even when the engagement-level lesson is newer
    /// (so the boost — not mere recency — is what flips the order). `#[serial]`
    /// — mutates DARKMUX_HOME.
    #[test]
    #[serial_test::serial]
    fn engagement_lessons_boosts_file_in_play() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()) };
        {
            let conn = crew::lessons::open_at(&crew::lessons::repo_db_path()).unwrap();
            // file-scoped lesson added FIRST (older updated_ts)...
            crew::lessons::add(&conn, "Target rule", "why", Some("./src/target.rs"), None).unwrap();
            // ...engagement-level lesson added SECOND (newer) — by recency this
            // would come first; the file-in-play boost must put the target first.
            crew::lessons::add(&conn, "House style", "applies everywhere", None, None).unwrap();
        }
        let intent: std::collections::HashSet<String> =
            ["src/target.rs"].iter().map(|s| s.to_string()).collect();
        let got = engagement_lessons(&intent);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        assert_eq!(got.len(), 2);
        assert!(
            got[0].contains("Target rule"),
            "file-in-play lesson ranks first despite being older: {got:?}"
        );
    }

    /// (#1004) The loop-lab A/B context builder wraps the repo's authored
    /// lessons in the real `<lessons>` block (no mission → no cautions). The
    /// block is the same one a coder dispatch would inject (shared
    /// `append_injected_blocks`). `#[serial]` — mutates DARKMUX_HOME.
    #[test]
    #[serial_test::serial]
    fn injected_context_for_lab_builds_the_lessons_block() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()) };
        {
            let conn = crew::lessons::open_at(&crew::lessons::repo_db_path()).unwrap();
            crew::lessons::add(&conn, "American English", "house style across all work", None, None)
                .unwrap();
        }
        let ctx = injected_context_for_lab(None, tmp.path(), None, None);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        assert!(ctx.starts_with("<lessons>"), "real lessons block, no leading blanks: {ctx:?}");
        assert!(ctx.contains("American English") && ctx.contains("house style"), "{ctx:?}");
        assert!(!ctx.contains("<detected-cautions>"), "no mission → no cautions block: {ctx:?}");
    }

    /// (#994 retrieve+inject) The caution reader: collects detector telemetry
    /// firings for a mission's EXACT dispatch session ids, deduped + ranked
    /// severity-then-recency, naming the firing's file. Excludes non-detector
    /// telemetry, non-telemetry categories, and sibling missions (same
    /// exact-set scope + sibling-bleed guard as the adjudication notes).
    /// `#[serial]` — mutates the shared DARKMUX_FLOWS_DIR.
    #[test]
    #[serial_test::serial]
    fn mission_cautions_filters_scopes_and_ranks() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("2026-06-22.jsonl"),
            concat!(
                // mission `auth`, s1 — a file-keyed cycle (warn)
                r#"{"ts":"2026-06-22T10:00:00Z","category":"telemetry","source":"detector","session_id":"mission-run-auth-s1","handle":"coder","payload":{"kind":"cycle","severity":"warn","detail":"`edit` called 3×","area":{"files":["src/x.rs"]}}}"#, "\n",
                // `auth`, s2 — an info-severity firing (must rank below warn)
                r#"{"ts":"2026-06-22T11:00:00Z","category":"telemetry","source":"detector","session_id":"mission-run-auth-s2","handle":"coder","payload":{"kind":"intra-turn-stall","severity":"info","detail":"runaway turn recovered"}}"#, "\n",
                // exact duplicate of the cycle — must not repeat
                r#"{"ts":"2026-06-22T11:30:00Z","category":"telemetry","source":"detector","session_id":"mission-run-auth-s1","handle":"coder","payload":{"kind":"cycle","severity":"warn","detail":"`edit` called 3×","area":{"files":["src/x.rs"]}}}"#, "\n",
                // SIBLING mission `auth-v2` — exact-set scope must NOT bleed it
                r#"{"ts":"2026-06-22T11:45:00Z","category":"telemetry","source":"detector","session_id":"mission-run-auth-v2-s1","handle":"coder","payload":{"kind":"cycle","severity":"warn","detail":"belongs to auth-v2"}}"#, "\n",
                // non-detector telemetry (source=runtime) — skip
                r#"{"ts":"2026-06-22T12:00:00Z","category":"telemetry","source":"runtime","session_id":"mission-run-auth-s1","handle":"coder","payload":{"kind":"context","detail":"context fill 40%"}}"#, "\n",
                // non-telemetry category, even with source=detector — skip
                r#"{"ts":"2026-06-22T12:05:00Z","category":"work","source":"detector","session_id":"mission-run-auth-s1","handle":"coder","payload":{"kind":"cycle","severity":"warn","detail":"wrong category"}}"#, "\n",
            ),
        )
        .unwrap();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        // SAFETY: serialized via #[serial]; restored below.
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path()) };

        let auth_ids: std::collections::HashSet<String> =
            ["mission-run-auth-s1", "mission-run-auth-s2"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        // Empty intent + a root with no matching files: no file-in-play boost
        // and (no code_hash on these records) no staleness reorder, so this
        // exercises the severity-then-recency fallthrough unchanged.
        let no_intent = std::collections::HashSet::new();
        let cautions = mission_cautions(&auth_ids, &no_intent, tmp.path());
        let unknown = mission_cautions(&std::collections::HashSet::new(), &no_intent, tmp.path());

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }

        assert_eq!(cautions.len(), 2, "two unique in-mission cautions: {cautions:?}");
        assert!(cautions[0].contains("[cycle]"), "warn outranks info: {cautions:?}");
        assert!(
            cautions[0].contains("(in `src/x.rs`)"),
            "the firing's file is named as the 'where': {cautions:?}"
        );
        assert!(cautions[1].contains("[intra-turn-stall]"), "info ranks last: {cautions:?}");
        assert!(
            !cautions.iter().any(|c| c.contains("auth-v2")),
            "sibling mission auth-v2 must not bleed: {cautions:?}"
        );
        assert!(
            !cautions.iter().any(|c| c.contains("context fill")),
            "non-detector telemetry excluded: {cautions:?}"
        );
        assert!(
            !cautions.iter().any(|c| c.contains("wrong category")),
            "non-telemetry category excluded: {cautions:?}"
        );
        assert!(unknown.is_empty(), "an empty session-id set reads as none");
    }

    // ─── (#1002) intent extraction + file-in-play / staleness ranking ────

    #[test]
    fn intent_files_extracts_path_like_tokens() {
        let got = intent_files(
            "Refactor the parser in `crates/darkmux-crew/src/lessons.rs` and bump Cargo.toml; \
             e.g. tidy up. Touch ./src/main.rs too.",
        );
        assert!(got.contains("crates/darkmux-crew/src/lessons.rs"));
        assert!(got.contains("Cargo.toml"));
        assert!(got.contains("src/main.rs"), "normalized ./src/main.rs: {got:?}");
        // Prose with a trailing dot is not a path.
        assert!(!got.iter().any(|f| f == "e.g" || f == "g"));
    }

    #[test]
    fn normalize_path_lexical_folds_equivalent_paths() {
        assert_eq!(normalize_path_lexical("./src/x.rs"), "src/x.rs");
        assert_eq!(normalize_path_lexical("src/../src/x.rs"), "src/x.rs");
        assert_eq!(normalize_path_lexical("src/x.rs/"), "src/x.rs");
    }

    /// (#1002) A caution about a file this dispatch will touch (intent match)
    /// outranks an engagement-level / other-file one, even when the other is
    /// newer. `#[serial]` — mutates the shared DARKMUX_FLOWS_DIR.
    #[test]
    #[serial_test::serial]
    fn mission_cautions_ranks_file_in_play_first() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("2026-06-22.jsonl"),
            concat!(
                // older caution on the file in play (normalized match for `./src/target.rs`)
                r#"{"ts":"2026-06-22T10:00:00Z","category":"telemetry","source":"detector","session_id":"mission-run-m-s1","payload":{"kind":"cycle","severity":"warn","detail":"on the target","area":{"files":["src/target.rs"]}}}"#, "\n",
                // NEWER, same-severity caution on an unrelated file
                r#"{"ts":"2026-06-22T12:00:00Z","category":"telemetry","source":"detector","session_id":"mission-run-m-s1","payload":{"kind":"cycle","severity":"warn","detail":"on something else","area":{"files":["src/other.rs"]}}}"#, "\n",
            ),
        )
        .unwrap();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path()) };

        let ids: std::collections::HashSet<String> =
            ["mission-run-m-s1"].iter().map(|s| s.to_string()).collect();
        let intent: std::collections::HashSet<String> =
            ["src/target.rs"].iter().map(|s| s.to_string()).collect();
        // workspace_root has no such files → no code_hash on records anyway → no
        // staleness reorder; this isolates the file-in-play boost.
        let cautions = mission_cautions(&ids, &intent, tmp.path());

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
        assert_eq!(cautions.len(), 2);
        assert!(
            cautions[0].contains("on the target"),
            "file-in-play caution ranks first despite being older: {cautions:?}"
        );
    }

    /// (#1002 + #1001) A caution whose firing-time `code_hash` no longer matches
    /// the file's current content (stale) is de-prioritized below a fresh one.
    /// `#[serial]` — mutates the shared DARKMUX_FLOWS_DIR.
    #[test]
    #[serial_test::serial]
    fn mission_cautions_ranks_fresh_over_stale() {
        let ws = tempfile::TempDir::new().unwrap();
        // The "fresh" file: its current content hashes to what we'll record.
        std::fs::write(ws.path().join("fresh.rs"), b"fn fresh() {}").unwrap();
        let fresh_hash = blake3::hash(b"fn fresh() {}").to_hex().to_string();
        // The "stale" file exists but its content differs from the recorded hash.
        std::fs::write(ws.path().join("stale.rs"), b"fn changed() {}").unwrap();
        let stale_recorded_hash = blake3::hash(b"fn ORIGINAL() {}").to_hex().to_string();

        let flows = tempfile::TempDir::new().unwrap();
        std::fs::write(
            flows.path().join("2026-06-22.jsonl"),
            format!(
                concat!(
                    // stale caution (recorded hash != current content), NEWER
                    r#"{{"ts":"2026-06-22T12:00:00Z","category":"telemetry","source":"detector","session_id":"mission-run-m-s1","payload":{{"kind":"cycle","severity":"warn","detail":"stale one","area":{{"files":["stale.rs"],"code_hash":"{stale}"}}}}}}"#, "\n",
                    // fresh caution (recorded hash == current content), older
                    r#"{{"ts":"2026-06-22T10:00:00Z","category":"telemetry","source":"detector","session_id":"mission-run-m-s1","payload":{{"kind":"cycle","severity":"warn","detail":"fresh one","area":{{"files":["fresh.rs"],"code_hash":"{fresh}"}}}}}}"#, "\n",
                ),
                stale = stale_recorded_hash,
                fresh = fresh_hash,
            ),
        )
        .unwrap();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", flows.path()) };

        let ids: std::collections::HashSet<String> =
            ["mission-run-m-s1"].iter().map(|s| s.to_string()).collect();
        // No intent match for either (neither file is in play) so the only
        // discriminator is freshness.
        let cautions = mission_cautions(&ids, &std::collections::HashSet::new(), ws.path());

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
        assert_eq!(cautions.len(), 2);
        assert!(
            cautions[0].contains("fresh one"),
            "fresh caution outranks the newer-but-stale one: {cautions:?}"
        );
    }

    // ─── (#1011) proportional injected-context budget + distribution ─────

    #[test]
    fn budget_chars_scales_with_window_and_floors_at_min() {
        // fraction × window × 4 chars/token.
        assert_eq!(budget_chars_for(Some(100_000), 0.15), (100_000f64 * 0.15) as usize * 4);
        // A bigger window → a bigger budget (auto-scaling from one fraction).
        assert!(budget_chars_for(Some(100_000), 0.15) > budget_chars_for(Some(8_000), 0.15));
        // A pathologically small window still floors at the minimum.
        assert_eq!(budget_chars_for(Some(10), 0.15), 2_000);
        // Unresolved window → the default fallback (non-zero, above the floor).
        assert!(budget_chars_for(None, 0.15) > 2_000);
    }

    fn bullets(prefix: &str, n: usize, len: usize) -> Vec<String> {
        // Each bullet is exactly `len` chars (prefix + padding) for predictable
        // budget math.
        (0..n)
            .map(|i| {
                let head = format!("{prefix}{i}:");
                format!("{head}{}", "x".repeat(len.saturating_sub(head.len())))
            })
            .collect()
    }

    #[test]
    fn allocate_gives_each_nonempty_category_its_floor() {
        // Generous budget: everything fits, nothing is dropped.
        let corr = bullets("c", 3, 50);
        let caut = bullets("a", 3, 50);
        let less = bullets("l", 3, 50);
        let (rc, rca, rl) =
            allocate_injected_context(corr.clone(), caut.clone(), less.clone(), 100_000);
        assert_eq!((rc.len(), rca.len(), rl.len()), (3, 3, 3), "all fit under a big budget");
    }

    #[test]
    fn allocate_caps_cautions_so_they_cannot_flood() {
        // Only cautions have content, and a LOT of it. The cautions cap (35% of
        // budget) bounds them even though the whole pool is otherwise free.
        let caut = bullets("a", 100, 100); // 100 bullets × 100 chars ≈ 10 100 chars demand
        let budget = 10_000;
        let (_, rca, _) = allocate_injected_context(Vec::new(), caut, Vec::new(), budget);
        let used: usize = rca.iter().map(|s| s.len() + 1).sum::<usize>().saturating_sub(1);
        // ≤ 35% of budget (+ one bullet's slack for the boundary item).
        assert!(used <= (budget * 35 / 100) + 101, "cautions capped near 35%: used={used}");
        assert!(!rca.is_empty(), "but still get their floor");
    }

    #[test]
    fn allocate_reallocates_empty_floors_and_prioritizes_corrections() {
        // No lessons, no cautions → their floors return to the pool, so the
        // high-authority corrections can use the whole budget.
        let corr = bullets("c", 50, 100); // 50 × 100 ≈ 5 049 chars demand
        let budget = 6_000;
        let (rc, rca, rl) =
            allocate_injected_context(corr, Vec::new(), Vec::new(), budget);
        assert!(rca.is_empty() && rl.is_empty());
        assert!(rc.len() >= 40, "corrections claim the freed pool: got {}", rc.len());
    }

    #[test]
    fn allocate_floor_protects_corrections_from_a_caution_flood() {
        // Cautions demand far exceeds budget, but corrections' floor guarantees
        // it lands regardless (the doom-loop's named failure: a correction
        // evaporating under a flood).
        let corr = bullets("c", 2, 100);
        let caut = bullets("a", 200, 100);
        let (rc, _, _) = allocate_injected_context(corr, caut, Vec::new(), 8_000);
        assert_eq!(rc.len(), 2, "both corrections survive the caution flood");
    }

    /// (#849 half 1) The brief-injection reader: collects adjudication notes
    /// for a mission's EXACT dispatch session ids, dedups, and excludes other
    /// sources + sibling missions. The load-bearing case is the sibling-mission
    /// regression QA caught: `auth-v2`'s notes must NOT bleed into `auth` (a
    /// prefix match would, since `mission-run-auth-v2-s1` starts with
    /// `mission-run-auth-`). `#[serial]` — mutates the shared DARKMUX_FLOWS_DIR.
    #[test]
    #[serial_test::serial]
    fn mission_adjudication_notes_reads_family_and_filters() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("2026-06-21.jsonl"),
            concat!(
                // mission `auth`, phase s1 — an adjudication correction
                r#"{"ts":"2026-06-21T10:00:00Z","action":"note","source":"adjudication","session_id":"mission-run-auth-s1","handle":"Do not rename the field."}"#, "\n",
                // `auth`, a LATER phase — same family, must be carried forward
                r#"{"ts":"2026-06-21T11:00:00Z","action":"note","source":"adjudication","session_id":"mission-run-auth-s2","handle":"Use cargo test -p foo."}"#, "\n",
                // exact duplicate of the first — must not repeat
                r#"{"ts":"2026-06-21T11:30:00Z","action":"note","source":"adjudication","session_id":"mission-run-auth-s1","handle":"Do not rename the field."}"#, "\n",
                // SIBLING mission `auth-v2` (id is a hyphen-extension of `auth`)
                // — a prefix match would bleed this in; the exact-set match must
                // NOT (the #849 QA regression).
                r#"{"ts":"2026-06-21T11:45:00Z","action":"note","source":"adjudication","session_id":"mission-run-auth-v2-s1","handle":"Belongs to auth-v2 ONLY."}"#, "\n",
                // an ORCHESTRATOR (dashboard) note for `auth` — wrong source, skip
                r#"{"ts":"2026-06-21T12:00:00Z","action":"note","source":"orchestrator","session_id":"mission-run-auth-s1","handle":"crew shipped it!"}"#, "\n",
            ),
        )
        .unwrap();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        // SAFETY: serialized via #[serial]; restored below.
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path()) };

        // `auth`'s EXACT dispatch session ids — as run() builds them from the
        // mission's phases. Note `auth-v2`'s session id is deliberately absent.
        let auth_ids: std::collections::HashSet<String> =
            ["mission-run-auth-s1", "mission-run-auth-s2"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let notes = mission_adjudication_notes(&auth_ids);
        let unknown = mission_adjudication_notes(&std::collections::HashSet::new());

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
        assert_eq!(notes.len(), 2, "two unique adjudication notes across the auth family: {notes:?}");
        assert!(notes.contains(&"Do not rename the field.".to_string()), "{notes:?}");
        assert!(notes.contains(&"Use cargo test -p foo.".to_string()), "{notes:?}");
        assert!(
            !notes.iter().any(|n| n.contains("auth-v2")),
            "sibling mission auth-v2 must NOT bleed into auth (the #849 prefix-bleed regression): {notes:?}"
        );
        assert!(!notes.iter().any(|n| n.contains("crew shipped")), "orchestrator note excluded: {notes:?}");
        assert!(unknown.is_empty(), "an empty session-id set reads as none");
    }

    /// (#1000) The debrief gather assembles a mission's review material from
    /// on-disk mission/phase state + the flow stream: the mission identity, its
    /// phases + how each ended, the detector cautions, and the reviewer's
    /// corrections — all scoped to THIS mission's exact dispatch sessions.
    /// `#[serial]` — mutates DARKMUX_HOME (mission/phase loaders) + the
    /// DARKMUX_FLOWS_DIR (the collectors read it live per-access).
    #[test]
    #[serial_test::serial]
    fn gather_debrief_assembles_mission_material() {
        let home = tempfile::TempDir::new().unwrap();
        let flows = tempfile::TempDir::new().unwrap();
        let mid = "m-debrief";
        let phases_dir = home.path().join("missions").join(mid).join("phases");
        std::fs::create_dir_all(&phases_dir).unwrap();
        std::fs::write(
            home.path().join("missions").join(mid).join("mission.json"),
            format!(
                r#"{{"id":"{mid}","description":"close the doom loop","status":"closed","phase_ids":["s1","s2"],"created_ts":1700000000}}"#
            ),
        )
        .unwrap();
        std::fs::write(
            phases_dir.join("s1.json"),
            format!(
                r#"{{"id":"s1","mission_id":"{mid}","description":"capture slice","status":"complete","depends_on":[],"created_ts":1700000200}}"#
            ),
        )
        .unwrap();
        std::fs::write(
            phases_dir.join("s2.json"),
            format!(
                r#"{{"id":"s2","mission_id":"{mid}","description":"index slice","status":"abandoned","depends_on":[],"created_ts":1700000300}}"#
            ),
        )
        .unwrap();
        // A detector caution + an adjudication correction, scoped to s1's session.
        std::fs::write(
            flows.path().join("2026-06-22.jsonl"),
            concat!(
                r#"{"ts":"2026-06-22T10:00:00Z","category":"telemetry","source":"detector","session_id":"mission-run-m-debrief-s1","handle":"coder","payload":{"kind":"cycle","severity":"warn","detail":"edit called 3x","area":{"files":["src/index.rs"]}}}"#, "\n",
                r#"{"ts":"2026-06-22T10:30:00Z","action":"note","source":"adjudication","session_id":"mission-run-m-debrief-s1","handle":"overrode SIGNOFF — verify never ran"}"#, "\n",
                // SIBLING mission session must NOT bleed in.
                r#"{"ts":"2026-06-22T10:45:00Z","category":"telemetry","source":"detector","session_id":"mission-run-m-debrief-v2-s1","handle":"coder","payload":{"kind":"cycle","severity":"warn","detail":"belongs to a sibling"}}"#, "\n",
            ),
        )
        .unwrap();

        let prev_home = std::env::var("DARKMUX_HOME").ok();
        let prev_flows = std::env::var("DARKMUX_FLOWS_DIR").ok();
        // SAFETY: serialized via #[serial]; restored below.
        unsafe {
            std::env::set_var("DARKMUX_HOME", home.path());
            std::env::set_var("DARKMUX_FLOWS_DIR", flows.path());
        }

        let report = gather_debrief(mid);
        let missing = gather_debrief("does-not-exist");

        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
            match prev_flows {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }

        let report = report.expect("mission found");
        assert_eq!(report.mission_id, mid);
        assert_eq!(report.mission_status, "closed");
        assert_eq!(report.phases.len(), 2, "both phases surfaced: {:?}", report.phases);
        assert!(
            report.phases.iter().any(|(id, _, st)| id == "s1" && *st == "complete"),
            "{:?}",
            report.phases
        );
        assert!(
            report.phases.iter().any(|(id, _, st)| id == "s2" && *st == "abandoned"),
            "{:?}",
            report.phases
        );
        assert_eq!(report.cautions.len(), 1, "one in-mission caution (sibling excluded): {:?}", report.cautions);
        assert!(report.cautions[0].contains("src/index.rs"), "{:?}", report.cautions);
        assert_eq!(report.corrections, vec!["overrode SIGNOFF — verify never ran".to_string()]);
        assert!(missing.is_err(), "an unknown mission errors");
    }

    /// (#1000) Closing a mission nudges the debrief — and that nudge emits a
    /// `Stage::Debrief` flow record (the variant's first real emission; #999
    /// added it unemitted). `#[serial]` — mutates DARKMUX_FLOWS_DIR.
    #[test]
    #[serial_test::serial]
    fn nudge_mission_debrief_emits_debrief_stage_record() {
        let flows = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        // SAFETY: serialized via #[serial]; restored below.
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", flows.path()) };

        nudge_mission_debrief("m-x");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }

        let mut found = false;
        for entry in std::fs::read_dir(flows.path()).unwrap() {
            let p = entry.unwrap().path();
            if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            for line in std::fs::read_to_string(&p).unwrap().lines() {
                let r: serde_json::Value = serde_json::from_str(line).unwrap();
                if r.get("stage").and_then(|v| v.as_str()) == Some("debrief")
                    && r.get("action").and_then(|v| v.as_str()) == Some("mission.debrief.prompt")
                    && r.get("mission_id").and_then(|v| v.as_str()) == Some("m-x")
                {
                    found = true;
                    // (#1436) The prompt joins the mission's lifecycle session
                    // bucket under the canonical HYPHEN form — the colon form
                    // (`mission:m-x`) is retired; a regression to it splits the
                    // viewer's session grouping between close and debrief.
                    assert_eq!(
                        r.get("session_id").and_then(|v| v.as_str()),
                        Some("mission-m-x"),
                        "debrief prompt must carry the canonical hyphen session id"
                    );
                }
            }
        }
        assert!(found, "the close nudge must emit a stage=debrief mission.debrief.prompt record");
    }

    /// (#817) The note-trail scan finds a session-scoped orchestrator note in
    /// the newest day files, and reads "no note" for other sessions, other
    /// sources, and a missing dir. `#[serial_test::serial]` — mutates the
    /// shared DARKMUX_FLOWS_DIR env (config_access reads it live per-access).
    #[test]
    #[serial_test::serial]
    fn session_note_scan_matches_session_and_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("2026-06-12.jsonl"),
            concat!(
                r#"{"ts":"2026-06-12T10:00:00Z","action":"note","source":"orchestrator","session_id":"mission-run-m1-s1","handle":"adjudicated"}"#, "\n",
                r#"{"ts":"2026-06-12T10:01:00Z","action":"note","source":"operator","session_id":"mission-run-m1-s2","handle":"not orchestrator"}"#, "\n",
                r#"{"ts":"2026-06-12T10:02:00Z","action":"note","source":"adjudication","session_id":"mission-run-m1-s3","handle":"audit-trail channel"}"#, "\n",
            ),
        )
        .unwrap();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        // SAFETY: serialized via #[serial]; restored below.
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path()) };

        let hit = session_has_orchestrator_note("mission-run-m1-s1");
        let wrong_session = session_has_orchestrator_note("mission-run-m1-sX");
        let wrong_source = session_has_orchestrator_note("mission-run-m1-s2");
        let adjudication_tag = session_has_orchestrator_note("mission-run-m1-s3");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
        assert!(hit, "session-scoped orchestrator note must be found");
        assert!(!wrong_session, "other sessions' notes must not match");
        assert!(!wrong_source, "non-adjudication/orchestrator notes must not match");
        assert!(adjudication_tag, "the adjudication audit-trail tag must satisfy the scan");
    }

    fn phase(id: &str, mission: &str, status: PhaseStatus) -> Phase {
        Phase {
            id: id.to_string(),
            mission_id: mission.to_string(),
            description: format!("desc {id}"),
            display_name: None,
            status,
            created_ts: 0,
            started_ts: None,
            completed_ts: None,
            abandoned_ts: None,
            task_ids: Vec::new(),
        }
    }

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// A no-assignment Task fixture for `StepKind::run` tests that don't
    /// exercise Task-sourced `role_id`/`profile_name`/`workdir`/`image`
    /// (#1230/#1341) — the assignment-carrying coder-phase graph shape is now
    /// covered by `darkmux-crew`'s `mission_config` interpret tests (which
    /// materialize the built-in `coder-phase.json` and assert the
    /// `mission.worktree`/`mission.coder`/`mission.verify` kinds).
    fn test_task(id: &str) -> Task {
        Task {
            id: id.to_string(),
            phase_id: "s1".to_string(),
            description: "test task".to_string(),
            display_name: None,
            step_ids: vec![format!("{id}-step")],
            depends_on: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        }
    }

    #[test]
    fn select_phase_explicit_must_belong_to_mission() {
        let phases = vec![phase("s1", "m1", PhaseStatus::Planned)];
        let err = select_phase(&phases, &ids(&["s1"]), "m2", Some("s1")).unwrap_err();
        assert!(err.to_string().contains("belongs to mission"), "{err}");
    }

    #[test]
    fn select_phase_explicit_rejects_complete() {
        let phases = vec![phase("s1", "m1", PhaseStatus::Complete)];
        let err = select_phase(&phases, &ids(&["s1"]), "m1", Some("s1")).unwrap_err();
        assert!(err.to_string().contains("already Complete"), "{err}");
    }

    #[test]
    fn select_phase_auto_picks_single_ready() {
        // (#1341) Phases are strictly linear — `s2` sits after `s1` in
        // `phase_ids` order, so it's blocked until `s1` completes; `s3`
        // belongs to a different mission (`phases`' own `mission_id`
        // scopes it out regardless of list order).
        let phases = vec![
            phase("s1", "m1", PhaseStatus::Planned),
            phase("s2", "m1", PhaseStatus::Planned),
            phase("s3", "m2", PhaseStatus::Planned),
        ];
        let chosen = select_phase(&phases, &ids(&["s1", "s2"]), "m1", None).unwrap();
        assert_eq!(chosen.id, "s1");
    }

    #[test]
    fn select_phase_auto_bails_on_zero_ready() {
        let phases = vec![phase("s1", "m1", PhaseStatus::Running)];
        let err = select_phase(&phases, &ids(&["s1"]), "m1", None).unwrap_err();
        assert!(err.to_string().contains("no ready phase"), "{err}");
    }

    /// (#1230 Packet 2, revised #1341) A phase whose predecessor in
    /// `phase_ids` order has already completed is auto-selected.
    #[test]
    fn select_phase_auto_picks_phase_whose_predecessor_is_complete() {
        let phases = vec![
            phase("s1", "m1", PhaseStatus::Complete),
            phase("s2", "m1", PhaseStatus::Planned),
        ];
        let chosen = select_phase(&phases, &ids(&["s1", "s2"]), "m1", None).unwrap();
        assert_eq!(chosen.id, "s2", "s2's predecessor (s1) is Complete — it's ready");
    }

    /// Companion negative case: same shape, but the predecessor is only
    /// `Running` (not yet `Complete`) — s2 must NOT be selected.
    #[test]
    fn select_phase_auto_excludes_phase_whose_predecessor_is_still_running() {
        let phases = vec![
            phase("s1", "m1", PhaseStatus::Running),
            phase("s2", "m1", PhaseStatus::Planned),
        ];
        let err = select_phase(&phases, &ids(&["s1", "s2"]), "m1", None).unwrap_err();
        assert!(err.to_string().contains("no ready phase"), "{err}");
    }

    #[test]
    fn branch_name_is_namespaced() {
        assert_eq!(branch_name("s1"), "darkmux/s1");
    }

    #[test]
    fn worktree_path_is_deterministic_under_repo_name() {
        let p = worktree_path(Path::new("/home/k/proj/darkmux-public"), "s1");
        assert!(p.ends_with("darkmux-public/s1"), "{}", p.display());
        // Recomputable: same inputs → same path.
        assert_eq!(p, worktree_path(Path::new("/home/k/proj/darkmux-public"), "s1"));
    }

    #[test]
    fn parse_main_worktree_picks_first_entry() {
        // The first `worktree` line is the main working tree; a linked
        // worktree follows. #846: ship from inside the linked one must still
        // resolve the repo name from the FIRST entry, not the current tree.
        let porcelain = "worktree /home/k/proj/darkmux-public\n\
                         HEAD 1111111111111111111111111111111111111111\n\
                         branch refs/heads/main\n\
                         \n\
                         worktree /home/k/.darkmux/worktrees/darkmux-public/s2-foo\n\
                         HEAD 2222222222222222222222222222222222222222\n\
                         branch refs/heads/darkmux/s2-foo\n";
        assert_eq!(
            parse_main_worktree(porcelain),
            Some(PathBuf::from("/home/k/proj/darkmux-public"))
        );
        // The repo-name component derived from it is stable regardless of which
        // tree `mission ship` was invoked from.
        let root = parse_main_worktree(porcelain).unwrap();
        assert!(worktree_path(&root, "s2-foo").ends_with("darkmux-public/s2-foo"));
    }

    #[test]
    fn parse_main_worktree_handles_empty_and_blank() {
        assert_eq!(parse_main_worktree(""), None);
        assert_eq!(parse_main_worktree("worktree \nHEAD abc\n"), None);
        assert_eq!(parse_main_worktree("HEAD abc\nbranch refs/heads/main\n"), None);
    }

    #[test]
    fn parse_main_worktree_unquoted_path_roundtrips_verbatim() {
        // No special chars → git emits the path unquoted; trailing space kept.
        assert_eq!(
            parse_main_worktree("worktree /home/me/repo \nHEAD abc\n"),
            Some(PathBuf::from("/home/me/repo "))
        );
    }

    #[test]
    fn parse_main_worktree_decodes_c_quoted_path() {
        // (#907) git C-quotes paths with special chars: a space-containing or
        // non-ASCII path is wrapped in quotes with escapes. The leading `"`
        // signals the quoted form.
        assert_eq!(
            parse_main_worktree("worktree \"/home/me/my repo\"\nHEAD abc\n"),
            Some(PathBuf::from("/home/me/my repo"))
        );
        // Escaped tab + backslash + quote.
        assert_eq!(
            parse_main_worktree("worktree \"/tmp/a\\tb\\\\c\\\"d\"\n"),
            Some(PathBuf::from("/tmp/a\tb\\c\"d"))
        );
        // Octal-escaped UTF-8 (é = 0xC3 0xA9 = \303\251).
        assert_eq!(
            parse_main_worktree("worktree \"/tmp/caf\\303\\251\"\n"),
            Some(PathBuf::from("/tmp/café"))
        );
    }

    #[test]
    fn git_lists_main_worktree_first_from_inside_a_linked_worktree() {
        // Locks the load-bearing #846 contract against REAL git: invoked from
        // INSIDE a linked worktree, `git worktree list --porcelain` still lists
        // the MAIN working tree first — so repo_root() (= this command +
        // parse_main_worktree) resolves the repo, not the phase dir. A future
        // git change or an output-ordering refactor that broke this is caught
        // here. No process-cwd mutation: git is invoked with `current_dir`.
        let tmp = tempfile::TempDir::new().unwrap();
        let main_repo = tmp.path().join("mainrepo");
        let linked = tmp.path().join("linked-phase");
        std::fs::create_dir_all(&main_repo).unwrap();

        let git = |dir: &Path, args: &[&str]| {
            let o = Command::new("git").current_dir(dir).args(args).output().unwrap();
            assert!(
                o.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        };
        git(&main_repo, &["init", "-q"]);
        git(&main_repo, &["config", "user.email", "t@example.com"]);
        git(&main_repo, &["config", "user.name", "t"]);
        git(&main_repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git(&main_repo, &["worktree", "add", "-q", linked.to_str().unwrap(), "-b", "phase-x"]);

        // Invoked FROM the linked worktree — the exact #846 scenario.
        let out = Command::new("git")
            .current_dir(&linked)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        assert!(out.status.success(), "git worktree list failed");
        let parsed = parse_main_worktree(&String::from_utf8_lossy(&out.stdout))
            .expect("parse a main worktree from porcelain");
        assert_eq!(
            parsed.canonicalize().unwrap(),
            main_repo.canonicalize().unwrap(),
            "expected the MAIN tree, got {}",
            parsed.display()
        );
    }

    fn mission(id: &str, desc: &str) -> crew::types::Mission {
        crew::types::Mission {
            id: id.to_string(),
            description: desc.to_string(),
            status: crew::types::MissionStatus::Active,
            phase_ids: vec![],
            created_ts: 0,
            started_ts: None,
            closed_ts: None,
            paused_ts: None,
            source_input: None,
            ticket: None,
        }
    }

    #[test]
    fn commit_subject_takes_first_line() {
        let s = phase("s1", "m1", PhaseStatus::Running);
        // phase() sets description = "desc s1"
        assert_eq!(commit_subject(&s), "desc s1");
    }

    #[test]
    fn commit_subject_truncates_long_and_takes_only_first_line() {
        let mut s = phase("s1", "m1", PhaseStatus::Running);
        s.description = format!("{}\nsecond line ignored", "x".repeat(100));
        let subj = commit_subject(&s);
        assert!(subj.chars().count() <= 72, "len {}", subj.chars().count());
        assert!(subj.ends_with("..."), "{subj}");
        assert!(!subj.contains("second line"), "only first line: {subj}");
    }

    #[test]
    fn commit_subject_falls_back_on_empty_description() {
        let mut s = phase("s1", "m1", PhaseStatus::Running);
        s.description = String::new();
        assert_eq!(commit_subject(&s), "darkmux phase s1");
    }

    #[test]
    fn pr_body_names_mission_and_phase_no_currency() {
        let m = mission("m1", "ship the thing");
        let s = phase("s1", "m1", PhaseStatus::Running);
        let body = pr_body(&m, &s);
        assert!(body.contains("`m1`"), "{body}");
        assert!(body.contains("`s1`"), "{body}");
        assert!(body.contains("mission ship"), "{body}");
        // Tokens-only doctrine: no currency leaks into shipped PR copy.
        assert!(!body.contains('$'), "no currency in PR body: {body}");
    }

    // (#799 part 2) parse_failed_verifiers — the verifier-fabrication backstop's
    // consumer-side parse. The governing discipline is FALSE-ALARM avoidance:
    // anything unparseable or absent must read as "nothing failed", never as a
    // failure — a soft signal that cries wolf is worse than one that stays quiet.
    fn envelope_with(failed: &str) -> String {
        format!(
            r#"{{"result":"stop","final_assistant":"done","metrics":{{}},"trajectory_path":"/x","failed_tool_invocations":{failed}}}"#
        )
    }

    #[test]
    fn parse_failed_verifiers_extracts_entries() {
        let env = envelope_with(
            r#"[{"command":"cargo test","reason":"command not found (exit 127) — the verifier never ran"}]"#,
        );
        let got = parse_failed_verifiers(&env);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].command, "cargo test");
        assert!(got[0].reason.contains("exit 127"), "{:?}", got[0].reason);
    }

    #[test]
    fn parse_failed_verifiers_empty_array_is_empty() {
        // An honest run stamps an empty array — the no-op case.
        assert!(parse_failed_verifiers(&envelope_with("[]")).is_empty());
    }

    #[test]
    fn parse_failed_verifiers_missing_field_is_empty() {
        // A pre-#799 runtime (or a non-success envelope) omits the field
        // entirely — must NOT be read as a failure.
        let env = r#"{"result":"stop","final_assistant":"done","metrics":{}}"#;
        assert!(parse_failed_verifiers(env).is_empty());
    }

    #[test]
    fn parse_failed_verifiers_malformed_json_is_empty() {
        // Garbage on stdout must fail OPEN to "nothing failed" — never a false
        // alarm that would hold a clean run's merge.
        assert!(parse_failed_verifiers("not json at all").is_empty());
        assert!(parse_failed_verifiers("").is_empty());
    }

    #[test]
    fn parse_failed_verifiers_last_line_fallback() {
        // Defense: if an unexpected leading line precedes the envelope, the
        // last-non-empty-line fallback still recovers the stamp.
        let env = envelope_with(r#"[{"command":"pytest","reason":"toolchain failed to load"}]"#);
        let stdout = format!("some stray log line\n{env}\n");
        let got = parse_failed_verifiers(&stdout);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].command, "pytest");
    }

    /// (#799 part 2) The ship-side reader round-trip — the run→ship handoff. The
    /// load-bearing case is RESUMED-PHASE latest-wins: a clean re-run's empty
    /// `mission.run.verification` record must OVERWRITE a prior dirty run's for
    /// the same session, so the documented fix-and-retry actually clears the
    /// hold. Also: a dirty-only session stays held, and other sessions don't
    /// bleed in. `#[serial]` — mutates the shared DARKMUX_FLOWS_DIR (read live
    /// per-access by config_access).
    #[test]
    #[serial_test::serial]
    fn session_failed_verifiers_latest_run_wins() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("2026-06-21.jsonl"),
            concat!(
                // session A: dirty run #1, then a clean re-run #2 (later line wins).
                r#"{"ts":"2026-06-21T10:00:00Z","action":"step result","session_id":"mission-run-mA-s1","payload":{"step_id":"s1-coder-step","kind":"mission.coder","failed_verifiers":[{"command":"cargo test","reason":"command not found (exit 127) — the verifier never ran"}],"count":1}}"#, "\n",
                r#"{"ts":"2026-06-21T10:30:00Z","action":"step result","session_id":"mission-run-mA-s1","payload":{"step_id":"s1-coder-step","kind":"mission.coder","failed_verifiers":[],"count":0}}"#, "\n",
                // session B: a single dirty run — stays held.
                r#"{"ts":"2026-06-21T11:00:00Z","action":"step result","session_id":"mission-run-mB-s1","payload":{"step_id":"s1-coder-step","kind":"mission.coder","failed_verifiers":[{"command":"pytest","reason":"toolchain failed to load"}],"count":1}}"#, "\n",
            ),
        )
        .unwrap();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        // SAFETY: serialized via #[serial]; restored below.
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path()) };

        let session_a = session_failed_verifiers("mission-run-mA-s1");
        let session_b = session_failed_verifiers("mission-run-mB-s1");
        let unknown = session_failed_verifiers("mission-run-mZ-s9");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
        assert!(
            session_a.is_empty(),
            "a clean re-run must clear a prior dirty record (latest-wins): {session_a:?}"
        );
        assert_eq!(session_b.len(), 1, "a dirty-only session stays held: {session_b:?}");
        assert_eq!(session_b[0].command, "pytest");
        assert!(unknown.is_empty(), "an unknown session reads as none");
    }

    // ─── (#1230 Packet 3) Task/Step graph migration ─────────────────────


    /// `MissionWorktreeStepKind` against a REAL git repo (no LMStudio, no
    /// Docker — pure git plumbing) — proves the migrated worktree step
    /// reproduces `add_worktree`'s two real-world outcomes: a clean
    /// creation, and the "already exists" bail on a second run for the
    /// same phase (the exact scenario a resumed/un-shipped coder-phase run
    /// hits).
    #[test]
    fn mission_worktree_step_kind_creates_then_rejects_duplicate() {
        let tmp = tempfile::TempDir::new().unwrap();
        let main_repo = tmp.path().join("repo");
        std::fs::create_dir_all(&main_repo).unwrap();
        let git = |args: &[&str]| {
            let o = Command::new("git").current_dir(&main_repo).args(args).output().unwrap();
            assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "init"]);

        let wt_path = tmp.path().join("worktrees").join("s1");
        let kind = MissionWorktreeStepKind {
            repo_root: main_repo.clone(),
            wt_path: wt_path.clone(),
            branch: "darkmux/s1".to_string(),
            base: "HEAD".to_string(),
            mission_id: "m1".to_string(),
            phase_id: "s1".to_string(),
            session_id: "mission-run-m1-s1".to_string(),
            role: "coder".to_string(),
        };
        let step = crew::types::Step {
            id: "s1-worktree-step".to_string(),
            task_id: "s1-worktree".to_string(),
            kind: "mission.worktree".to_string(),
            status: NodeStatus::Planned,
            config: serde_json::Value::Null,
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let task = test_task("s1-worktree");

        let outcome = kind.run(&step, &task, &std::collections::BTreeMap::new()).unwrap();
        assert!(wt_path.is_dir(), "worktree dir must exist after a clean run");
        assert_eq!(outcome.output, wt_path.display().to_string());

        // A second run against the SAME phase (the resumed-run case) must
        // fail loud, not silently clobber — same contract `add_worktree`
        // always had.
        let err = kind.run(&step, &task, &std::collections::BTreeMap::new()).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
    }

    /// `MissionVerifyStepKind` against a REAL git repo with an EMPTY diff
    /// (no coder changes made yet). `phase_review_output_at` short-
    /// circuits an empty diff to a canned "clean" `PhaseReviewOutput`
    /// with ZERO reviewer dispatch — so this exercises the real verify
    /// step end to end with no LMStudio/Docker involved, matching the
    /// operator's no-live-dispatch constraint. `#[serial]` — mutates the
    /// shared `DARKMUX_FLOWS_DIR` (the empty-diff path still emits a
    /// "phase review begin"/"verdict: clean" flow record pair).
    #[test]
    #[serial_test::serial]
    fn mission_verify_step_kind_clean_diff_needs_no_dispatch() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let o = Command::new("git").current_dir(&repo).args(args).output().unwrap();
            assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "init"]);
        git(&["branch", "-m", "main"]);

        let flows = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        // SAFETY: serialized via #[serial]; restored below.
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", flows.path()) };

        let slot: Arc<Mutex<Option<std::result::Result<crate::phase_cli::PhaseReviewOutput, String>>>> =
            Arc::new(Mutex::new(None));
        let kind = MissionVerifyStepKind {
            wt_path: repo.clone(),
            base: "main".to_string(),
            phase_id: "s1".to_string(),
            result_slot: slot.clone(),
        };
        let step = crew::types::Step {
            id: "s1-verify-step".to_string(),
            task_id: "s1-verify".to_string(),
            kind: "mission.verify".to_string(),
            status: NodeStatus::Planned,
            config: serde_json::Value::Null,
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let task = test_task("s1-verify");
        let outcome = kind.run(&step, &task, &std::collections::BTreeMap::new());

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }

        let outcome = outcome.unwrap();
        assert_eq!(outcome.output, "clean");
        let taken = slot.lock().unwrap().take();
        match taken {
            Some(Ok(review)) => {
                assert_eq!(review.verdict, "clean");
                assert_eq!(review.total_findings, 0);
            }
            Some(Err(e)) => panic!("expected Ok(clean review), got Err({e})"),
            None => panic!("expected Some(Ok(clean review)), got None"),
        }
    }

    /// `resolve_local_placement` — the best-effort role→profile→model
    /// classification `StepKind::residency` implementations use. A local
    /// model resolves to `Some(Placement)`; a remote (endpoint-bearing)
    /// model, or an unresolvable role/profile, fails OPEN to `None` (never
    /// an error — see the function's own doc). Uses an explicit
    /// `--profiles`-equivalent temp file path, so this never touches the
    /// real `~/.darkmux/profiles.json`.
    #[test]
    fn resolve_local_placement_classifies_local_vs_remote_vs_unresolvable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let profiles_path = tmp.path().join("profiles.json");
        std::fs::write(
            &profiles_path,
            r#"{
                "profiles": {
                    "test": {
                        "models": [
                            {"id": "local-model", "n_ctx": 32000},
                            {"id": "remote-model", "n_ctx": 32000, "endpoint": {"url": "https://example.com/v1"}}
                        ],
                        "default_model": "local-model"
                    }
                },
                "default_profile": "test"
            }"#,
        )
        .unwrap();
        let path_str = profiles_path.to_str().unwrap();

        // A known role ("coder" — built-in) on the default profile's
        // default (local) model resolves to a real Placement.
        let placement = resolve_local_placement("coder", None, Some(path_str), "test-seat");
        let placement = placement.expect("a local default model must resolve");
        assert_eq!(placement.model_key, "local-model");
        assert_eq!(placement.min_ctx, 32000);
        assert_eq!(placement.seat, "test-seat");
        assert!(placement.identifier.starts_with("darkmux:"), "{}", placement.identifier);

        // An explicit request for the remote-endpoint model — swap the
        // default so `select_model`'s no-vectors fallback picks it — must
        // classify `None` (Remote), never a Placement.
        std::fs::write(
            &profiles_path,
            r#"{
                "profiles": {
                    "test": {
                        "models": [
                            {"id": "remote-model", "n_ctx": 32000, "endpoint": {"url": "https://example.com/v1"}}
                        ],
                        "default_model": "remote-model"
                    }
                },
                "default_profile": "test"
            }"#,
        )
        .unwrap();
        assert!(
            resolve_local_placement("coder", None, Some(path_str), "test-seat").is_none(),
            "a remote-endpoint model must classify Remote (None), not a Placement"
        );

        // An unresolvable role fails open to None, not a panic/error.
        assert!(resolve_local_placement("no-such-role-xyz", None, Some(path_str), "seat").is_none());
    }

    // ── (#1426 ship-4 / #1433) ship/abort honest-finalize + workdir align ──

    /// Point `DARKMUX_CREW_DIR` + `DARKMUX_FLOWS_DIR` at fresh TempDirs and
    /// restore on drop. Every user MUST be `#[serial]` (global env mutation).
    struct CrewEnvGuard {
        _crew: tempfile::TempDir,
        _flows: tempfile::TempDir,
        prev_crew: Option<String>,
        prev_flows: Option<String>,
    }
    impl CrewEnvGuard {
        fn new() -> Self {
            let crew = tempfile::TempDir::new().unwrap();
            let flows = tempfile::TempDir::new().unwrap();
            let prev_crew = std::env::var("DARKMUX_CREW_DIR").ok();
            let prev_flows = std::env::var("DARKMUX_FLOWS_DIR").ok();
            // SAFETY: serialized via #[serial_test::serial] on every caller.
            unsafe {
                std::env::set_var("DARKMUX_CREW_DIR", crew.path());
                std::env::set_var("DARKMUX_FLOWS_DIR", flows.path());
            }
            Self { _crew: crew, _flows: flows, prev_crew, prev_flows }
        }
    }
    impl Drop for CrewEnvGuard {
        fn drop(&mut self) {
            // SAFETY: serialized via #[serial_test::serial] on every caller.
            unsafe {
                match &self.prev_crew {
                    Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                    None => std::env::remove_var("DARKMUX_CREW_DIR"),
                }
                match &self.prev_flows {
                    Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                    None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
                }
            }
        }
    }

    /// Seed an Active mission + its phases at the given statuses.
    fn seed_finalize_mission(mid: &str, phases: &[(&str, PhaseStatus)]) {
        let mut m = mission(mid, "finalize test");
        m.phase_ids = phases.iter().map(|(id, _)| id.to_string()).collect();
        crew::lifecycle::save_mission(&m).unwrap();
        for (id, status) in phases {
            let mut p = phase(id, mid, *status);
            match status {
                PhaseStatus::Complete => p.completed_ts = Some(1),
                PhaseStatus::Abandoned => p.abandoned_ts = Some(1),
                _ => {}
            }
            crew::lifecycle::save_phase(&p).unwrap();
        }
    }

    fn mission_status_now(mid: &str) -> crew::types::MissionStatus {
        crew::loader::load_missions()
            .unwrap()
            .into_iter()
            .find(|m| m.id == mid)
            .expect("mission on disk")
            .status
    }

    #[test]
    #[serial_test::serial]
    fn finalize_closes_mission_when_every_phase_is_terminal() {
        // ship completing the (single) phase leaves the mission all-terminal →
        // it closes honestly with a Clean envelope, not stranded Active.
        let _g = CrewEnvGuard::new();
        let mid = "fin-all-complete";
        seed_finalize_mission(mid, &[("p1", PhaseStatus::Complete)]);
        finalize_mission_if_complete(mid);
        assert_eq!(mission_status_now(mid), crew::types::MissionStatus::Closed);
        let env = crew::lifecycle::load_envelope(mid).unwrap().expect("envelope.json persisted");
        assert_eq!(env.status, crew::envelope::MissionOutcomeStatus::Clean);
    }

    #[test]
    #[serial_test::serial]
    fn finalize_abandoned_phase_closes_mission_degraded() {
        // abort abandoning the only phase → the mission closes (no strand), but
        // Degraded: real work happened yet no phase completed cleanly.
        let _g = CrewEnvGuard::new();
        let mid = "fin-abandoned";
        seed_finalize_mission(mid, &[("p1", PhaseStatus::Abandoned)]);
        finalize_mission_if_complete(mid);
        assert_eq!(mission_status_now(mid), crew::types::MissionStatus::Closed);
        let env = crew::lifecycle::load_envelope(mid).unwrap().expect("envelope.json persisted");
        assert_eq!(env.status, crew::envelope::MissionOutcomeStatus::Degraded);
    }

    #[test]
    #[serial_test::serial]
    fn finalize_leaves_mission_active_when_a_phase_is_still_open() {
        // A multi-phase mission where ship/abort closed only ONE phase: the
        // mission stays Active (the operator finishes the rest), no envelope.
        let _g = CrewEnvGuard::new();
        let mid = "fin-partial";
        seed_finalize_mission(mid, &[("p1", PhaseStatus::Complete), ("p2", PhaseStatus::Planned)]);
        finalize_mission_if_complete(mid);
        assert_eq!(
            mission_status_now(mid),
            crew::types::MissionStatus::Active,
            "an open phase keeps the mission Active"
        );
        assert!(
            crew::lifecycle::load_envelope(mid).unwrap().is_none(),
            "no finalize envelope while a phase is still open"
        );
    }

    #[test]
    #[serial_test::serial]
    fn resolve_run_workdir_reads_task_workdir_then_falls_back() {
        // A launched coder-phase run persists its worktree on Task.workdir;
        // ship/abort must target THAT, not the derived default.
        let _g = CrewEnvGuard::new();
        let mid = "wd-align";
        let mut t = test_task("p1-worktree");
        t.phase_id = "p1".to_string();
        t.workdir = Some(std::path::PathBuf::from("/custom/launch/wt"));
        crew::lifecycle::save_task(mid, &t).unwrap();

        let root = std::path::Path::new("/repo");
        assert_eq!(
            resolve_run_workdir(mid, "p1", root),
            std::path::PathBuf::from("/custom/launch/wt"),
            "reads the persisted launch workdir"
        );
        // A phase with no task record → the derived-path fallback.
        assert_eq!(
            resolve_run_workdir(mid, "p2", root),
            worktree_path(root, "p2"),
            "falls back to the derived worktree path"
        );
    }
