    use super::*;
    use serial_test::serial;

    /// RAII guard: sets `DARKMUX_CREW_DIR` to a TempDir for the test's duration,
    /// then restores (or unsets) on drop.  Mirrors `CrewDirGuard` in the sibling
    /// `tests` module — kept local to this module to avoid cross-module coupling.
    struct TestCrewRoot {
        prev: Option<String>,
        _tmp: tempfile::TempDir,
    }

    impl TestCrewRoot {
        fn new() -> Self {
            let tmp = tempfile::TempDir::new().unwrap();
            let prev = std::env::var("DARKMUX_CREW_DIR").ok();
            // SAFETY: serialized via #[serial] on every caller.
            unsafe { std::env::set_var("DARKMUX_CREW_DIR", tmp.path()); }
            Self { prev, _tmp: tmp }
        }

        fn path(&self) -> &std::path::Path {
            self._tmp.path()
        }
    }

    impl Drop for TestCrewRoot {
        fn drop(&mut self) {
            // SAFETY: serialized via #[serial] on every caller.
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                    None => std::env::remove_var("DARKMUX_CREW_DIR"),
                }
            }
        }
    }

    fn seed_mission(root: &std::path::Path, id: &str) {
        let dir = root.join("missions").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let mission = serde_json::json!({
            "id": id,
            "description": format!("test mission {id}"),
            "phase_ids": [],
            "created_ts": 1_700_000_000u64,
        });
        std::fs::write(
            dir.join("mission.json"),
            serde_json::to_string_pretty(&mission).unwrap(),
        )
        .unwrap();
    }

    fn seed_phase(root: &std::path::Path, mission_id: &str, phase_id: &str) {
        let sdir = root.join("missions").join(mission_id).join("phases");
        std::fs::create_dir_all(&sdir).unwrap();
        let phase = serde_json::json!({
            "id": phase_id,
            "mission_id": mission_id,
            "description": format!("test phase {phase_id}"),
            "depends_on": [],
            "created_ts": 1_700_000_000u64,
        });
        std::fs::write(
            sdir.join(format!("{phase_id}.json")),
            serde_json::to_string_pretty(&phase).unwrap(),
        )
        .unwrap();
    }

    #[test]
    #[serial]
    fn load_missions_empty_root() {
        let _guard = TestCrewRoot::new();
        assert_eq!(load_missions().unwrap().len(), 0);
    }

    #[test]
    #[serial]
    fn load_phases_empty_root() {
        let _guard = TestCrewRoot::new();
        assert_eq!(load_phases().unwrap().len(), 0);
    }

    #[test]
    #[serial]
    fn load_missions_two_missions_one_phase_each() {
        let guard = TestCrewRoot::new();
        let root = guard.path();

        seed_mission(root, "alpha");
        seed_mission(root, "beta");
        seed_phase(root, "alpha", "s1");
        seed_phase(root, "beta", "s2");

        let missions = load_missions().unwrap();
        assert_eq!(missions.len(), 2);
        // Sorted by id.
        assert_eq!(missions[0].id, "alpha");
        assert_eq!(missions[1].id, "beta");

        let phases = load_phases().unwrap();
        assert_eq!(phases.len(), 2);
        let alpha_phase = phases.iter().find(|s| s.id == "s1").expect("phase s1 missing");
        let beta_phase = phases.iter().find(|s| s.id == "s2").expect("phase s2 missing");
        assert_eq!(alpha_phase.mission_id, "alpha");
        assert_eq!(beta_phase.mission_id, "beta");
    }

    #[test]
    #[serial]
    fn load_missions_ignores_legacy_flat_files() {
        let guard = TestCrewRoot::new();
        let root = guard.path();

        // New layout mission.
        seed_mission(root, "current");

        // Legacy flat mission file at <crew_root>/missions/old.json — should be IGNORED.
        let missions_root = root.join("missions");
        std::fs::write(
            missions_root.join("old.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "id": "old",
                "description": "legacy",
                "phase_ids": [],
                "created_ts": 1u64,
            }))
            .unwrap(),
        )
        .unwrap();

        // Legacy flat phase at <crew_root>/phases/x.json — should be IGNORED.
        let legacy_phases = root.join("phases");
        std::fs::create_dir_all(&legacy_phases).unwrap();
        std::fs::write(
            legacy_phases.join("x.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "id": "x",
                "mission_id": "old",
                "description": "legacy",
                "depends_on": [],
                "created_ts": 1u64,
            }))
            .unwrap(),
        )
        .unwrap();

        let missions = load_missions().unwrap();
        assert_eq!(missions.len(), 1, "legacy flat file must be ignored; got {:?}", missions.iter().map(|m| &m.id).collect::<Vec<_>>());
        assert_eq!(missions[0].id, "current");

        // current has no phases; legacy x.json under <crew_root>/phases/ is ignored.
        let phases = load_phases().unwrap();
        assert_eq!(phases.len(), 0, "legacy flat phase must be ignored; got {:?}", phases.iter().map(|s| &s.id).collect::<Vec<_>>());
    }

    #[test]
    #[serial]
    fn load_missions_skips_subdir_without_mission_json() {
        let guard = TestCrewRoot::new();
        let root = guard.path();

        // Create a subdir with no mission.json inside it.
        let partial = root.join("missions").join("partial");
        std::fs::create_dir_all(&partial).unwrap();

        // And a fully-formed mission alongside it.
        seed_mission(root, "complete");

        let missions = load_missions().unwrap();
        assert_eq!(missions.len(), 1);
        assert_eq!(missions[0].id, "complete");
    }

    #[test]
    #[serial]
    fn load_phases_skips_non_json_files() {
        let guard = TestCrewRoot::new();
        let root = guard.path();

        seed_mission(root, "mymission");
        seed_phase(root, "mymission", "s-real");

        // Drop a non-JSON file into the phases dir — should be ignored.
        let phases_dir = root.join("missions").join("mymission").join("phases");
        std::fs::write(phases_dir.join("notes.txt"), "just a note").unwrap();

        let phases = load_phases().unwrap();
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].id, "s-real");
    }

    // ─── Beat-33 dual-read fallback tests ────────────────────────────────
    //
    // `resolve_user_subdir` prefers the post-flatten canonical path
    // (`<root>/<subdir>/`) and falls back to the legacy pre-flatten path
    // (`<root>/crew/<subdir>/`) for operators who haven't migrated. These
    // tests pin the resolution table so a regression that silently flips
    // preference (e.g., always prefer legacy) fails loudly.

    #[serial]
    #[test]
    fn resolve_user_subdir_prefers_canonical_when_only_canonical_exists() {
        let guard = TestCrewRoot::new();
        let canonical = guard.path().join("roles");
        std::fs::create_dir_all(&canonical).unwrap();
        assert_eq!(roles_dir(), canonical);
    }

    #[serial]
    #[test]
    fn resolve_user_subdir_falls_back_to_legacy_when_only_legacy_exists() {
        let guard = TestCrewRoot::new();
        let legacy = guard.path().join("crew").join("roles");
        std::fs::create_dir_all(&legacy).unwrap();
        assert_eq!(roles_dir(), legacy);
    }

    #[serial]
    #[test]
    fn resolve_user_subdir_prefers_canonical_when_both_exist() {
        // Operator partway through a migration — canonical should win so
        // future reads + writes consolidate at the new layout (legacy
        // becomes operator-visible-but-not-touched, doctor PR-3b
        // surfaces it for cleanup).
        let guard = TestCrewRoot::new();
        let canonical = guard.path().join("missions");
        let legacy = guard.path().join("crew").join("missions");
        std::fs::create_dir_all(&canonical).unwrap();
        std::fs::create_dir_all(&legacy).unwrap();
        assert_eq!(missions_dir(), canonical);
    }

    #[serial]
    #[test]
    fn resolve_user_subdir_returns_canonical_when_neither_exists() {
        // Fresh install — neither layout exists. The canonical path is
        // returned so a subsequent write creates the new layout
        // (operator-sovereignty: no silent migration of legacy state).
        let guard = TestCrewRoot::new();
        let canonical = guard.path().join("phases");
        assert!(!canonical.exists());
        assert!(!guard.path().join("crew").join("phases").exists());
        assert_eq!(phases_dir(), canonical);
    }

    #[serial]
    #[test]
    fn loader_finds_user_role_override_in_legacy_layout() {
        // End-to-end: an operator with the legacy `<root>/crew/roles/`
        // layout still gets their override picked up by load_roles().
        let guard = TestCrewRoot::new();
        let legacy_roles = guard.path().join("crew").join("roles");
        std::fs::create_dir_all(&legacy_roles).unwrap();
        let user_json = r#"{"id":"coder","description":"legacy-layout override","skills":[],"tool_palette":{"allow":["read"],"deny":[]},"escalation_contract":"bail-with-explanation"}"#;
        std::fs::write(legacy_roles.join("coder.json"), user_json).unwrap();

        let roles = load_roles().unwrap();
        let coder = roles.iter().find(|r| r.id == "coder").expect("coder must load");
        assert_eq!(coder.description, "legacy-layout override");
    }

    // ─── #425: autonomous-dispatch preamble ─────────────────────────────

    #[test]
    fn embedded_preamble_contains_load_bearing_directives() {
        // Smoke-check that the embedded preamble file has the
        // directives it's meant to carry. A future edit that
        // accidentally truncated the file or removed key phrasing
        // would fail this test.
        let preamble = AUTONOMOUS_DISPATCH_PREAMBLE;
        assert!(preamble.contains("autonomous dispatch mode"));
        assert!(preamble.contains("cannot ask questions"));
        assert!(preamble.contains("cannot pause"));
        assert!(preamble.contains("BLOCKED:"));
    }

    /// (#442) Budget-aware prompt-engineering section pins the four
    /// bounds, their failure modes, and the floor-not-ceiling framing.
    /// Future edits that silently revert these load-bearing phrases
    /// would dilute the budget-awareness signal and reopen the
    /// Parkinson's-law concern.
    ///
    /// Deliberately does NOT assert specific numeric values — the
    /// preamble describes the bounds conceptually; the live values
    /// surface in the dynamic Dispatch budget block at compaction
    /// time. Pinning numerics here would couple the prompt to the
    /// runtime's current tuning and silently lie after a retune.
    #[test]
    fn embedded_preamble_carries_bounded_dispatch_prompt_engineering() {
        let preamble = AUTONOMOUS_DISPATCH_PREAMBLE;
        // Four-bound enumeration — by concept, not value.
        assert!(
            preamble.contains("Turn cap"),
            "preamble must name the turn cap"
        );
        assert!(
            preamble.contains("Per-turn token cap"),
            "preamble must name the per-turn cap"
        );
        assert!(
            preamble.contains("Cumulative completion-token cap"),
            "preamble must name the cumulative-token cap"
        );
        assert!(
            preamble.contains("Wall-clock deadline"),
            "preamble must name the wall-clock deadline"
        );
        // Failure-mode names each bound emits — these are operator-
        // visible JSON envelope strings, stable contract.
        assert!(
            preamble.contains("result: \"max_turns\""),
            "preamble must name the max_turns terminal"
        );
        assert!(
            preamble.contains("escalation_intra_turn_stall_exhausted"),
            "preamble must name the intra-turn-stall escalation"
        );
        assert!(
            preamble.contains("escalation_cumulative_tokens_exceeded"),
            "preamble must name the cumulative-tokens escalation"
        );
        // Floor-not-ceiling framing — the key prompt-engineering move
        // that guards against Parkinson's-law expansion.
        assert!(
            preamble.contains("*floor*, not a *ceiling*"),
            "preamble must use floor-not-ceiling framing"
        );
        // Behavioral guidance must mention reasoning tokens count
        // (the dominant failure mode from Beat 47).
        assert!(
            preamble.contains("Reasoning tokens count"),
            "preamble must warn that reasoning tokens count toward the caps"
        );
        // Operator-tunable signal — names that values live in the
        // dynamic Dispatch budget block, not in the preamble.
        assert!(
            preamble.contains("operator-configurable"),
            "preamble must signal that cap values are operator-tunable, \
             not hardcoded into the prompt"
        );
    }

    #[serial]
    #[test]
    fn load_preamble_returns_embedded_when_no_user_override() {
        let _guard = TestCrewRoot::new();
        let preamble = load_autonomous_dispatch_preamble();
        assert!(preamble.contains("autonomous dispatch mode"));
    }

    #[serial]
    #[test]
    fn load_preamble_returns_user_override_when_present() {
        let guard = TestCrewRoot::new();
        let override_path = guard.path().join("AUTONOMOUS_DISPATCH_PREAMBLE.md");
        std::fs::write(&override_path, "# CUSTOM OPERATOR PREAMBLE\nbe brief.").unwrap();
        let preamble = load_autonomous_dispatch_preamble();
        assert!(preamble.contains("CUSTOM OPERATOR PREAMBLE"));
        assert!(!preamble.contains("autonomous dispatch mode"));
    }

    #[test]
    fn role_family_default_is_specialist() {
        // Field absent on Role manifests defaults to specialist
        // (preventive: better to prepend an unneeded preamble than to
        // miss prepending a needed one).
        let role = Role {
            output_schema: None,
            id: "test-role".into(),
            description: "A test role".into(),
            skills: vec![],
            tool_palette: ToolPalette { allow: vec!["read".into()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: None,
            feedback_templates: None,
        };
        assert!(role.is_specialist());
    }

    #[test]
    fn role_family_utility_opts_out_of_specialist() {
        let role = Role {
            output_schema: None,
            id: "test-role".into(),
            description: "A test role".into(),
            skills: vec![],
            tool_palette: ToolPalette { allow: vec!["read".into()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: Some("utility".into()),
            feedback_templates: None,
        };
        assert!(!role.is_specialist());
    }

    /// Legacy `"admin"` value (renamed to `"utility"` in the
    /// admin→utility nomenclature transition) is treated as
    /// specialist by `is_specialist()` itself — silent fallthrough
    /// at the matcher layer. The loud-fail lives at the loader
    /// boundary (`validate_role_family`) so the matcher stays
    /// branch-free. See `validate_rejects_legacy_admin_role_family`.
    #[test]
    fn legacy_admin_value_is_silently_specialist_at_matcher_layer() {
        let role = Role {
            output_schema: None,
            id: "test-role".into(),
            description: "A test role".into(),
            skills: vec![],
            tool_palette: ToolPalette { allow: vec!["read".into()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: Some("admin".into()),
            feedback_templates: None,
        };
        // Matcher only recognizes "utility" — legacy "admin" falls
        // through to specialist (preventive default). Production paths
        // rely on the loader's validator catching this before the
        // matcher ever sees it.
        assert!(role.is_specialist());
    }

    #[test]
    fn role_family_explicit_specialist_matches_default() {
        let role = Role {
            output_schema: None,
            id: "test-role".into(),
            description: "A test role".into(),
            skills: vec![],
            tool_palette: ToolPalette { allow: vec!["read".into()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: Some("specialist".into()),
            feedback_templates: None,
        };
        assert!(role.is_specialist());
    }

    #[serial]
    #[test]
    fn mission_compiler_manifest_declares_role_family_utility() {
        // A canonical utility role (#590 also tags scribe, and will add
        // the compactor). Pins the manifest's role_family value so a
        // future edit that accidentally removed it would fail loudly.
        let _guard = TestCrewRoot::new();
        let roles = load_roles().expect("load_roles must succeed against embedded defaults");
        let mc = roles.iter()
            .find(|r| r.id == "mission-compiler")
            .expect("mission-compiler must be in the embedded role registry");
        assert_eq!(mc.role_family.as_deref(), Some("utility"),
            "mission-compiler must be tagged role_family: utility — it's the canonical bounded-I/O role; \
             retagging requires explicit operator decision since it changes preamble behavior");
        assert!(!mc.is_specialist());
    }

    #[serial]
    #[test]
    fn embedded_roles_declare_expected_families() {
        // Pin the specialist/utility split across the built-in roster (#590):
        // utility = supports the runtime outside mission scope; specialist =
        // works the mission/phases. Every built-in declares the family
        // EXPLICITLY (no implicit default), and a retag must be a conscious
        // edit here. The preamble-prepend logic depends on this split.
        let _guard = TestCrewRoot::new();
        let roles = load_roles().expect("load_roles must succeed against embedded defaults");
        let utility: std::collections::BTreeSet<&str> =
            ["mission-compiler", "scribe"].into_iter().collect();
        for r in &roles {
            assert!(
                r.role_family.is_some(),
                "built-in role `{}` must declare role_family explicitly (#590)",
                r.id
            );
            if utility.contains(r.id.as_str()) {
                assert_eq!(
                    r.role_family.as_deref(),
                    Some("utility"),
                    "role `{}` must be utility (supports the runtime outside mission scope)",
                    r.id
                );
                assert!(!r.is_specialist());
            } else {
                assert_eq!(
                    r.role_family.as_deref(),
                    Some("specialist"),
                    "role `{}` must be specialist (works the mission/phases)",
                    r.id
                );
                assert!(r.is_specialist());
            }
        }
    }

    /// Loader rejects the legacy `"admin"` role_family value from a
    /// user-authored manifest with an operator-actionable error
    /// message that names the rename, points at the offending file
    /// path, and tells the operator what to change. Pre-1.0 no-compat
    /// doctrine — no silent rewrite, no env-var alias.
    #[test]
    fn validate_rejects_legacy_admin_role_family_user_source() {
        let legacy_role = Role {
            output_schema: None,
            id: "legacy-role".into(),
            description: "A role using the pre-rename admin value".into(),
            skills: vec![],
            tool_palette: ToolPalette { allow: vec!["read".into()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: Some("admin".into()),
            feedback_templates: None,
        };
        let err = super::validate_role_family(&legacy_role, super::RoleSource::User).expect_err(
            "validator must reject the legacy `admin` value on user-authored roles"
        );
        let msg = format!("{err}");
        assert!(msg.contains("legacy-role"), "msg names the offending role: {msg}");
        assert!(msg.contains("\"admin\""), "msg names the legacy value: {msg}");
        assert!(msg.contains("\"utility\""), "msg names the new value: {msg}");
        // User-source message points at the editable file path.
        assert!(msg.contains("legacy-role.json"), "msg points at the file to edit: {msg}");
    }

    /// Builtin-source rejection produces a different message — operator
    /// can't edit embedded manifests, so the actionable repair is
    /// "please file an issue" rather than "edit your manifest."
    #[test]
    fn validate_rejects_legacy_admin_role_family_builtin_source() {
        let legacy_role = Role {
            output_schema: None,
            id: "broken-builtin".into(),
            description: "Simulates a builtin manifest that drifted back to admin".into(),
            skills: vec![],
            tool_palette: ToolPalette { allow: vec!["read".into()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: Some("admin".into()),
            feedback_templates: None,
        };
        let err = super::validate_role_family(&legacy_role, super::RoleSource::Builtin)
            .expect_err("validator must reject the legacy `admin` value on builtin roles");
        let msg = format!("{err}");
        assert!(msg.contains("broken-builtin"), "msg names the offending role: {msg}");
        assert!(msg.contains("internal regression"), "msg flags this is not operator-actionable: {msg}");
        assert!(msg.contains("file an issue"), "msg points to the right repair path: {msg}");
    }

    #[test]
    fn validate_accepts_utility_specialist_and_none() {
        let mut r = Role {
            output_schema: None,
            id: "test-role".into(),
            description: "A test role".into(),
            skills: vec![],
            tool_palette: ToolPalette { allow: vec!["read".into()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: None,
            feedback_templates: None,
        };
        // Source doesn't matter for the accept path — assert both.
        assert!(super::validate_role_family(&r, super::RoleSource::User).is_ok(), "None+User must pass");
        assert!(super::validate_role_family(&r, super::RoleSource::Builtin).is_ok(), "None+Builtin must pass");
        r.role_family = Some("utility".into());
        assert!(super::validate_role_family(&r, super::RoleSource::User).is_ok(), "utility+User must pass");
        r.role_family = Some("specialist".into());
        assert!(super::validate_role_family(&r, super::RoleSource::User).is_ok(), "specialist+User must pass");
    }

    /// Unknown role_family values (a typo like `"worker"`) are now rejected
    /// rather than silently treated as specialist (#590: validated two-value
    /// axis). User source → actionable; builtin source → regression framing.
    #[test]
    fn validate_rejects_unknown_role_family() {
        let r = Role {
            output_schema: None,
            id: "test-role".into(),
            description: "A role with a typo'd family".into(),
            skills: vec![],
            tool_palette: ToolPalette { allow: vec!["read".into()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: Some("worker".into()),
            feedback_templates: None,
        };
        let err = super::validate_role_family(&r, super::RoleSource::User)
            .expect_err("unknown role_family must be rejected on user roles");
        let msg = format!("{err}");
        assert!(msg.contains("\"worker\""), "names the bad value: {msg}");
        assert!(msg.contains("not a recognized family"), "explains the problem: {msg}");
        assert!(msg.contains("test-role.json"), "points at the file to edit: {msg}");

        let err_b = super::validate_role_family(&r, super::RoleSource::Builtin)
            .expect_err("unknown role_family must be rejected on builtin roles");
        let msg_b = format!("{err_b}");
        assert!(msg_b.contains("internal regression"), "flags not operator-actionable: {msg_b}");
        assert!(msg_b.contains("file an issue"), "points to the repair path: {msg_b}");
    }

    #[test]
    #[serial]
    fn load_skills_user_override_keys_on_body_id_not_filename() {
        // #892: load_skills keyed the dedup map on the filename stem, so a user
        // skill filed under a name != its body id failed to override the
        // builtin of the same id (both lingered). Keying on the body id fixes
        // the override. Goes red pre-fix (two 'coding' skills survive).
        let guard = TestCrewRoot::new();
        let skills_dir = guard.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        // A user override for the builtin "coding" skill, filed under a
        // mismatched filename.
        std::fs::write(
            skills_dir.join("wrong-name.json"),
            r#"{"id":"coding","description":"USER OVERRIDE","keywords":[]}"#,
        )
        .unwrap();

        let coding: Vec<_> = load_skills()
            .unwrap()
            .into_iter()
            .filter(|s| s.id == "coding")
            .collect();
        assert_eq!(
            coding.len(),
            1,
            "exactly one 'coding' skill — the user file must override the builtin by body id"
        );
        assert_eq!(
            coding[0].description, "USER OVERRIDE",
            "the user manifest must win the override"
        );
    }
