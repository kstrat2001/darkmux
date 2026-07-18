    use super::*;
    use tempfile::TempDir;

    /// (#1475 packet 3, contract 6) The four probe personas — `review-probe`
    /// and its three role→profile copies `review-probe-high`/`-mid`/`-low` — must
    /// be BYTE-EQUAL. Probe recall diversity lives entirely in each role's
    /// profile→model binding, never in the persona text (the frozen #1256
    /// persona). This lock makes "one hash, not one intention" structural: a
    /// future golden bump to `review-probe.md` that forgets to re-copy the three
    /// siblings fails HERE, before it can silently desync the measured prompts.
    #[test]
    fn probe_persona_copies_are_byte_equal() {
        let prompt_for = |id: &str| {
            BUILTIN_ROLE_PROMPTS
                .iter()
                .find(|(pid, _)| *pid == id)
                .map(|(_, c)| *c)
                .unwrap_or_else(|| panic!("{id} prompt must be embedded"))
        };
        let base = prompt_for("review-probe");
        for id in ["review-probe-high", "review-probe-mid", "review-probe-low"] {
            assert_eq!(
                prompt_for(id),
                base,
                "{id}.md must be byte-equal to review-probe.md — recall diversity is \
                 role→profile-borne, not persona-borne (#1256 frozen text / #1475 packet 3)"
            );
        }
    }

    /// (#1053) The pr-reviewer prompt must carry the intent-assessment
    /// directive — the cross-tier-validated lever against "restate-the-fix"
    /// false positives (a reviewer flagging the very bug a PR fixes). The
    /// self-review workflow supplies the PR title + description; this directive
    /// is what tells the model to judge the diff against that stated intent. A
    /// future edit that dropped it would silently regress the behavior the
    /// bench A/B (8B and 122B, with-intent → clean pass) proved.
    #[test]
    fn pr_reviewer_prompt_carries_intent_directive() {
        let prompt = BUILTIN_ROLE_PROMPTS
            .iter()
            .find(|(id, _)| *id == "pr-reviewer")
            .map(|(_, c)| *c)
            .expect("pr-reviewer prompt must be embedded");
        assert!(
            prompt.contains("stated intent"),
            "pr-reviewer.md must instruct assessing against the change's stated intent (#1053)"
        );
        assert!(
            prompt.contains("achieves its stated purpose"),
            "pr-reviewer.md must frame a correct change as one that achieves its stated purpose (#1053)"
        );
    }

    /// (#1113) The agentic PR reviewer's contract, pinned:
    /// - NO `output_schema` on the manifest — a grammar-constrained output
    ///   combined with tools makes the model skip tool-calling entirely and
    ///   fabricate plausible output (verified empirically 2026-07-04). The
    ///   freeform marker contract exists BECAUSE of that; a future edit that
    ///   "helpfully" adds a schema back would break the role silently.
    /// - The prompt must carry the marker + verdict-line contract the render
    ///   verb parses, the explore-before-concluding directive (the 2-turn
    ///   near-miss vs 5-turn access-gap-catch finding), the no-cap and
    ///   no-self-downgrade language, and the intent-assessment directive
    ///   (#1053, same lever as the tool-less reviewer).
    #[test]
    fn pr_reviewer_agentic_contract() {
        let role = load_roles()
            .expect("builtin roles load")
            .into_iter()
            .find(|r| r.id == "pr-reviewer-agentic")
            .expect("pr-reviewer-agentic must be embedded");
        assert!(
            role.output_schema.is_none(),
            "pr-reviewer-agentic must NOT declare an output_schema — schema+tools \
             makes the model fabricate instead of exploring (#1113)"
        );
        assert!(
            !role.tool_palette.allow.is_empty(),
            "pr-reviewer-agentic must grant tools — exploration is its whole point"
        );
        let prompt = BUILTIN_ROLE_PROMPTS
            .iter()
            .find(|(id, _)| *id == "pr-reviewer-agentic")
            .map(|(_, c)| *c)
            .expect("pr-reviewer-agentic prompt must be embedded");
        for needle in [
            "MUST FIX",
            "CONSIDER",
            "VERDICT:",
            "stated intent",
            "no cap on findings",
            "Never talk yourself out of a finding",
            "Explore before concluding",
        ] {
            assert!(
                prompt.contains(needle),
                "pr-reviewer-agentic.md must contain {needle:?} (#1113 contract)"
            );
        }
    }

    /// (#1222) The dialectic PR-review seats' contracts, pinned:
    /// - Advocates (prosecutor, defender) are agentic: tools granted, NO
    ///   output_schema (grammar + tools makes the model fabricate — the same
    ///   verified finding the agentic reviewer's contract pins), freeform
    ///   marker output (`CHARGE <n>` / `REBUTTAL <n>: <stance>`) with a
    ///   literal closing line. Charge numbering is EXPLICIT in the marker —
    ///   positional numbering derived at parse time would let one misparsed
    ///   body line shift every downstream number and desync
    ///   rebuttals ↔ verdicts (frontier QA finding on the P0 PR).
    /// - The judge is deliberately tool-less (rules on the presented record;
    ///   explicit deny list per the #1197 empty-palette rule) and has NO
    ///   output_schema (reason-freely-then-one-fenced-JSON keeps reasoning
    ///   room open — a JSON-only grammar suppresses it).
    #[test]
    fn dialectic_seats_contract() {
        let roles = load_roles().expect("builtin roles load");
        for seat in ["dialectic-prosecutor", "dialectic-defender"] {
            let role = roles
                .iter()
                .find(|r| r.id == seat)
                .unwrap_or_else(|| panic!("{seat} must be embedded"));
            assert!(
                role.output_schema.is_none(),
                "{seat} must NOT declare an output_schema — schema+tools makes \
                 the model fabricate instead of exploring (#1222)"
            );
            assert!(
                !role.tool_palette.allow.is_empty(),
                "{seat} must grant tools — evidence-gathering is its whole point"
            );
            for denied in ["edit", "write"] {
                assert!(
                    role.tool_palette.deny.iter().any(|t| t == denied),
                    "{seat} must deny {denied:?} — advocates are read-only"
                );
            }
        }
        let judge = roles
            .iter()
            .find(|r| r.id == "dialectic-judge")
            .expect("dialectic-judge must be embedded");
        assert!(
            judge.output_schema.is_none(),
            "dialectic-judge must NOT declare an output_schema — the contract \
             is reason-then-fenced-JSON so reasoning models keep their room (#1222)"
        );
        assert!(
            judge.tool_palette.allow.is_empty(),
            "dialectic-judge is tool-less by design — it rules on the record"
        );
        for denied in ["read", "write", "edit", "exec", "process"] {
            assert!(
                judge.tool_palette.deny.iter().any(|t| t == denied),
                "dialectic-judge must EXPLICITLY deny {denied:?} — an empty \
                 palette silently grants the full catalog (#1197 bench-role rule)"
            );
        }
        let prompt_for = |id: &str| {
            BUILTIN_ROLE_PROMPTS
                .iter()
                .find(|(pid, _)| *pid == id)
                .map(|(_, c)| *c)
                .unwrap_or_else(|| panic!("{id} prompt must be embedded"))
        };
        let prosecutor = prompt_for("dialectic-prosecutor");
        for needle in [
            "CHARGE 1 [",
            "CHARGE 2 [",
            "CHARGE <n>",
            "CASE: rested",
            "CASE: no-charges",
            "you are bad at those",
            "no cap on charges",
            "problem it is fixing",
        ] {
            assert!(
                prosecutor.contains(needle),
                "dialectic-prosecutor.md must contain {needle:?} (#1222 contract)"
            );
        }
        let defender = prompt_for("dialectic-defender");
        for needle in ["REBUTTAL", "DEFENSE: rests", "refute", "mitigate", "concede"] {
            assert!(
                defender.contains(needle),
                "dialectic-defender.md must contain {needle:?} (#1222 contract)"
            );
        }
        let judge_prompt = prompt_for("dialectic-judge");
        for needle in [
            "sustained",
            "dismissed",
            "decisive_evidence",
            "fenced JSON block",
            "no tools, by design",
            "every charge number presented",
        ] {
            assert!(
                judge_prompt.contains(needle),
                "dialectic-judge.md must contain {needle:?} (#1222 contract)"
            );
        }
    }

    /// (#1260/#1177) The review-verify seat's contract, pinned — following
    /// review-judge's pattern exactly:
    /// - deliberately TOOL-LESS with an EXPLICIT deny list (an empty
    ///   palette silently grants the full catalog — the #1197 bench-role
    ///   rule); verification is scoped to the provided bundle evidence.
    /// - NO output_schema (reason-freely-then-one-fenced-JSON keeps
    ///   reasoning room open — a JSON-only grammar suppresses it).
    /// - The prompt must carry the three-word ruling vocabulary
    ///   ({verified, refuted, uncertain}), the evidence-scope directive,
    ///   the refute-first posture, and the fenced-JSON contract's keys.
    #[test]
    fn review_verify_seat_contract() {
        let role = load_roles()
            .expect("builtin roles load")
            .into_iter()
            .find(|r| r.id == "review-verify")
            .expect("review-verify must be embedded");
        assert!(
            role.output_schema.is_none(),
            "review-verify must NOT declare an output_schema — the contract is \
             reason-then-fenced-JSON so reasoning models keep their room (#1260)"
        );
        assert!(
            role.tool_palette.allow.is_empty(),
            "review-verify is tool-less by design — it rules on the provided evidence"
        );
        for denied in ["read", "write", "edit", "exec", "process", "update_plan"] {
            assert!(
                role.tool_palette.deny.iter().any(|t| t == denied),
                "review-verify must EXPLICITLY deny {denied:?} — an empty palette \
                 silently grants the full catalog (#1197 bench-role rule)"
            );
        }
        let prompt = BUILTIN_ROLE_PROMPTS
            .iter()
            .find(|(id, _)| *id == "review-verify")
            .map(|(_, c)| *c)
            .expect("review-verify prompt must be embedded");
        for needle in [
            "\"verified\"",
            "\"refuted\"",
            "\"uncertain\"",
            "Verify ONLY what the provided evidence proves",
            "refute the finding first",
            "decisive_evidence",
            "note_for_author",
            "exactly one fenced JSON block",
        ] {
            assert!(
                prompt.contains(needle),
                "review-verify.md must contain {needle:?} (#1260 contract)"
            );
        }
    }

    /// (#1196) The tool-bench harness role's contract, pinned:
    /// - An EXPLICIT non-empty tool allow-list. An empty palette makes the
    ///   dispatch path expose the runtime's FULL catalog silently
    ///   (`compute_runtime_allowed_tools` returns `None`) — a bench role must
    ///   never rely on that, per the #1197 bench-role rule. The list must
    ///   cover the whole belt the bench measures (read brings search; exec
    ///   brings bash).
    /// - NO `output_schema` — schema+tools makes the model skip tool-calling
    ///   and fabricate; the ANSWER:/BLOCKED: line contract replaces it, and
    ///   fabrication under it is exactly what the bench scores.
    /// - SPECIALIST family — the autonomous-dispatch preamble carries the
    ///   `BLOCKED:` escalation convention the termination axis scores
    ///   against; a utility role would skip the preamble.
    /// - The prompt must carry the ANSWER/BLOCKED contract, the provenance
    ///   rule (only report tokens observed in tool results), and the
    ///   DMX- token definition the provider's scoring parses against.
    #[test]
    fn tool_bench_role_contract() {
        let role = load_roles()
            .expect("builtin roles load")
            .into_iter()
            .find(|r| r.id == "tool-bench")
            .expect("tool-bench must be embedded");
        assert!(
            role.output_schema.is_none(),
            "tool-bench must NOT declare an output_schema — schema+tools makes the \
             model fabricate instead of calling tools (#1196)"
        );
        for tool in ["read", "write", "edit", "exec"] {
            assert!(
                role.tool_palette.allow.iter().any(|t| t == tool),
                "tool-bench palette must EXPLICITLY allow {tool:?} — the full belt, \
                 never the empty-palette full-catalog fallback (#1197 bench-role rule)"
            );
        }
        assert!(
            role.is_specialist(),
            "tool-bench must be a specialist role so the autonomous-dispatch preamble \
             (the BLOCKED: escalation convention) is injected (#1196 termination axis)"
        );
        let prompt = BUILTIN_ROLE_PROMPTS
            .iter()
            .find(|(id, _)| *id == "tool-bench")
            .map(|(_, c)| *c)
            .expect("tool-bench prompt must be embedded");
        for needle in [
            "ANSWER:",
            "BLOCKED:",
            "DMX-",
            "actually observed in a tool result",
            "Never construct, guess",
        ] {
            assert!(
                prompt.contains(needle),
                "tool-bench.md must contain {needle:?} (#1196 contract)"
            );
        }
    }

    /// (#1053 quote-resolve) The pr-reviewer prompt must instruct the model to
    /// quote the line (`anchor`) rather than emit a line number — the workflow
    /// resolves the quote to a coordinate deterministically. The n=5 A/B showed
    /// model-emitted line numbers mis-localize (the construct it names is right,
    /// the integer is ~20 lines off); a regression back to line-numbers would
    /// silently bring that back. The output schema (`pr-reviewer.json`) must
    /// agree — it carries `anchor`, not `line`.
    #[test]
    fn pr_reviewer_prompt_and_schema_use_quote_anchor() {
        let prompt = BUILTIN_ROLE_PROMPTS
            .iter()
            .find(|(id, _)| *id == "pr-reviewer")
            .map(|(_, c)| *c)
            .expect("pr-reviewer prompt must be embedded");
        assert!(
            prompt.contains("anchor"),
            "pr-reviewer.md must describe the `anchor` (quote-the-line) field (#1053)"
        );
        assert!(
            prompt.contains("do not output a line number"),
            "pr-reviewer.md must tell the model NOT to emit a line number (#1053)"
        );
        let role_json = BUILTIN_ROLES
            .iter()
            .find(|(id, _)| *id == "pr-reviewer")
            .map(|(_, c)| *c)
            .expect("pr-reviewer manifest must be embedded");
        let role: Role = serde_json::from_str(role_json).expect("pr-reviewer manifest parses");
        let schema = role.output_schema.expect("pr-reviewer has an output_schema");
        let props = &schema["properties"]["findings"]["items"]["properties"];
        assert!(props.get("anchor").is_some(), "finding schema must carry `anchor` (#1053)");
        assert!(props.get("line").is_none(), "finding schema must NOT carry `line` (replaced by anchor, #1053)");
    }

    /// (#1119 free-form mode) `pr-reviewer-freeform` must NOT carry an
    /// `output_schema` — the whole point of the sibling role is no grammar
    /// lock on the output. A future edit that added one back would silently
    /// reintroduce the JSON muzzle this role exists to avoid.
    #[test]
    fn pr_reviewer_freeform_has_no_output_schema() {
        let role_json = BUILTIN_ROLES
            .iter()
            .find(|(id, _)| *id == "pr-reviewer-freeform")
            .map(|(_, c)| *c)
            .expect("pr-reviewer-freeform manifest must be embedded");
        let role: Role = serde_json::from_str(role_json).expect("pr-reviewer-freeform manifest parses");
        assert!(
            role.output_schema.is_none(),
            "pr-reviewer-freeform must stay grammar-unlocked (no output_schema) — that's the axis it exists to test"
        );
    }

    /// (2026-07-04 aggressive redesign) `pr-reviewer-freeform`'s prompt must
    /// instruct the model NEVER to downgrade a self-noticed defect to unmarked
    /// commentary. Live-dispatch evidence (gpt-5.1 on the pr-review-bench
    /// corpus, #1119): under the earlier "achieves its stated intent = correct"
    /// framing the model traced the EXACT labeled bug in its reasoning, then
    /// wrote "not a must-fix" and left it unmarked — a self-downgrade, not a
    /// detection failure. This directive is the fix; a future edit that
    /// softened it back would silently reintroduce the calibration failure.
    #[test]
    fn pr_reviewer_freeform_prompt_forbids_self_downgrade() {
        let prompt = BUILTIN_ROLE_PROMPTS
            .iter()
            .find(|(id, _)| *id == "pr-reviewer-freeform")
            .map(|(_, c)| *c)
            .expect("pr-reviewer-freeform prompt must be embedded");
        assert!(
            prompt.to_lowercase().contains("never talk yourself out"),
            "pr-reviewer-freeform.md must forbid self-downgrading a noticed defect"
        );
        assert!(
            prompt.contains("no cap") || prompt.contains("no artificial limit") || prompt.contains("**no cap**"),
            "pr-reviewer-freeform.md must not artificially cap the number of findings \
             (a human author triages downstream, per #1119)"
        );
    }

    /// (#1038) Every builtin role's `output_schema`, if present, must be
    /// LMStudio-grammar-safe — the runtime sends it with `strict: true`, and a
    /// non-conforming schema makes LMStudio reject the request on the FIRST real
    /// dispatch (a backend error that no unit test catches because nothing else
    /// loads these manifests AND calls LMStudio — only a live dogfood does).
    /// Two rules, learned the hard way. First, the OpenAI strict contract: every
    /// object schema sets `additionalProperties: false` and lists EVERY declared
    /// property in `required`. Second, `type` is ALWAYS a single string, never an
    /// array — LMStudio's grammar compiler rejects the union form
    /// `"type": ["string","null"]` with `ValueError: 'type' must be a string`
    /// (the regression that shipped in #1039 and broke every dispatch; nullable
    /// goes through `anyOf: [{type:string},{type:null}]` instead). Walks nested
    /// objects, array items, and anyOf/oneOf/allOf branches so a future schema
    /// edit can't silently drop either invariant.
    #[test]
    fn builtin_role_output_schemas_are_strict_safe() {
        fn assert_strict_safe(schema: &serde_json::Value, role: &str, path: &str) {
            let Some(obj) = schema.as_object() else { return };
            // Rule 2: `type` must be a single string, never an array (LMStudio).
            if let Some(ty) = obj.get("type") {
                assert!(
                    ty.is_string(),
                    "role `{role}` schema at `{path}`: `type` must be a single string, not {ty} — LMStudio rejects the union form (use anyOf for nullable)",
                );
            }
            // Recurse into array items + schema-composition branches.
            if let Some(items) = obj.get("items") {
                assert_strict_safe(items, role, &format!("{path}[]"));
            }
            for kw in ["anyOf", "oneOf", "allOf"] {
                if let Some(branches) = obj.get(kw).and_then(|b| b.as_array()) {
                    for (i, branch) in branches.iter().enumerate() {
                        assert_strict_safe(branch, role, &format!("{path}.{kw}[{i}]"));
                    }
                }
            }
            // Only object-typed schemas carry the properties/required contract.
            let Some(props) = obj.get("properties").and_then(|p| p.as_object()) else {
                return;
            };
            assert_eq!(
                obj.get("additionalProperties"),
                Some(&serde_json::Value::Bool(false)),
                "role `{role}` schema at `{path}`: object must set additionalProperties:false for strict:true",
            );
            let required: Vec<&str> = obj
                .get("required")
                .and_then(|r| r.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            for key in props.keys() {
                assert!(
                    required.contains(&key.as_str()),
                    "role `{role}` schema at `{path}`: property `{key}` must be in `required` for strict:true (optionals stay required, nullable via anyOf)",
                );
                assert_strict_safe(&props[key], role, &format!("{path}.{key}"));
            }
        }

        for (id, json) in BUILTIN_ROLES {
            let role: Role = serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("builtin role `{id}` must parse: {e}"));
            if let Some(schema) = &role.output_schema {
                assert_strict_safe(schema, id, "$");
            }
        }
    }

    /// RAII guard that points `DARKMUX_CREW_DIR` at a TempDir for the test's
    /// duration, then restores the previous value (or unsets it) on drop.
    /// Uses the existing env-var hook in `load_roles` rather than mutating
    /// the process cwd — cwd mutation crashes follow-up tests when the
    /// guarded TempDir is dropped while the cwd still points inside it.
    struct CrewDirGuard {
        prev: Option<String>,
        _tmp: TempDir,
    }

    impl CrewDirGuard {
        fn new(tmp: TempDir) -> Self {
            let prev = env::var("DARKMUX_CREW_DIR").ok();
            // SAFETY: serialized via #[serial_test::serial] on every caller.
            unsafe { env::set_var("DARKMUX_CREW_DIR", tmp.path()); }
            Self { prev, _tmp: tmp }
        }

        fn path(&self) -> &std::path::Path {
            self._tmp.path()
        }
    }

    impl Drop for CrewDirGuard {
        fn drop(&mut self) {
            // SAFETY: serialized via #[serial_test::serial] on every caller.
            unsafe {
                match &self.prev {
                    Some(v) => env::set_var("DARKMUX_CREW_DIR", v),
                    None => env::remove_var("DARKMUX_CREW_DIR"),
                }
            }
        }
    }

    #[test]
    fn roles_round_trip() {
        let role = Role {
            output_schema: None,
            id: "test".into(),
            description: "A test role".into(),
            skills: vec![String::from("coding")],
            tool_palette: ToolPalette { allow: vec!["read".to_string(), "edit".to_string()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: None,
            feedback_templates: None,
        };
        let json = serde_json::to_string(&role).unwrap();
        let back: Role = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "test");
        assert_eq!(back.description, "A test role");
        assert_eq!(back.skills, vec![String::from("coding")]);
    }

    #[test]
    fn skills_round_trip() {
        let cap = Skill {
            id: "coding".into(),
            description: "Writes code".into(),
            keywords: vec![KeywordWeight { keyword: "implement".into(), weight: 1.0 }],
            capabilities: Default::default(),
        };
        let json = serde_json::to_string(&cap).unwrap();
        let back: Skill = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "coding");
        assert_eq!(back.keywords[0].keyword, "implement");
    }

    #[test]
    fn escalation_contract_serializes() {
        let bail = EscalationContract::BailWithExplanation;
        assert_eq!(serde_json::to_string(&bail).unwrap(), "\"bail-with-explanation\"");

        let retry = EscalationContract::RetryWithHint;
        assert_eq!(serde_json::to_string(&retry).unwrap(), "\"retry-with-hint\"");

        let handoff = EscalationContract::HandOffTo("operator".into());
        assert_eq!(serde_json::to_string(&handoff).unwrap(), "{\"hand-off-to\":\"operator\"}");
    }

    #[test]
    fn position_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&Position::Lead).unwrap(), "\"lead\"");
        assert_eq!(serde_json::to_string(&Position::Support).unwrap(), "\"support\"");
    }

    #[serial_test::serial]
    #[test]
    fn loader_picks_user_file_over_builtin() {
        let guard = CrewDirGuard::new(TempDir::new().unwrap());
        let roles_dir = guard.path().join("roles");
        fs::create_dir_all(&roles_dir).unwrap();
        let user_json = r#"{"id":"coder","description":"user override","skills":[],"tool_palette":{"allow":["read"],"deny":[]},"escalation_contract":"bail-with-explanation"}"#;
        fs::write(roles_dir.join("coder.json"), user_json).unwrap();

        let roles = load_roles().unwrap();
        let coder = roles.iter().find(|r| r.id == "coder").expect("coder should be loaded");
        assert_eq!(coder.description, "user override");
    }

    #[serial_test::serial]
    #[test]
    fn loader_falls_through_to_builtin() {
        let _guard = CrewDirGuard::new(TempDir::new().unwrap());
        // No user files written — loader should fall through to builtin coder.
        let roles = load_roles().unwrap();
        let coder = roles.iter().find(|r| r.id == "coder").expect("builtin coder should load");
        assert_eq!(coder.description, "Implements features described by the user. Reads existing code first; follows patterns; runs tests before reporting done.");
    }

    #[serial_test::serial]
    #[test]
    fn coder_role_can_create_new_files() {
        // Regression guard for #124. Implementing features routinely requires
        // creating new files (new modules, new templates, new fixtures).
        // `edit` only modifies existing files; `write` is what the coder uses
        // to create them. A dispatch with the previous palette
        // (`read`/`edit`/`exec`/`process` — no `write`) dead-ends mid-task on
        // any new-file step. Verified empirically on Phase 2 of #113 (PR #123).
        let _guard = CrewDirGuard::new(TempDir::new().unwrap());
        let roles = load_roles().unwrap();
        let coder = roles.iter().find(|r| r.id == "coder").expect("builtin coder should load");
        assert!(
            coder.tool_palette.allow.iter().any(|t| t == "write"),
            "coder.tool_palette.allow must include 'write' (regression of #124); got {:?}",
            coder.tool_palette.allow
        );
    }

    #[serial_test::serial]
    #[test]
    fn prompt_path_resolved_when_md_present() {
        let guard = CrewDirGuard::new(TempDir::new().unwrap());
        let roles_dir = guard.path().join("roles");
        fs::create_dir_all(&roles_dir).unwrap();
        let role_json = r#"{"id":"custom","description":"a custom role","skills":[],"tool_palette":{"allow":["read"],"deny":[]},"escalation_contract":"bail-with-explanation"}"#;
        fs::write(roles_dir.join("custom.json"), role_json).unwrap();
        fs::write(roles_dir.join("custom.md"), "# Custom Role\nDo stuff.").unwrap();

        let roles = load_roles().unwrap();
        let custom = roles.iter().find(|r| r.id == "custom").expect("custom should load");
        assert!(
            custom.prompt_path.is_some(),
            "prompt_path should be Some when .md exists"
        );
    }

    #[serial_test::serial]
    #[test]
    fn prompt_path_none_when_md_absent() {
        let guard = CrewDirGuard::new(TempDir::new().unwrap());
        let roles_dir = guard.path().join("roles");
        fs::create_dir_all(&roles_dir).unwrap();
        let role_json = r#"{"id":"no-prompt","description":"no prompt file","skills":[],"tool_palette":{"allow":["read"],"deny":[]},"escalation_contract":"bail-with-explanation"}"#;
        fs::write(roles_dir.join("no-prompt.json"), role_json).unwrap();

        let roles = load_roles().unwrap();
        let np = roles.iter().find(|r| r.id == "no-prompt").expect("no-prompt should load");
        assert!(
            np.prompt_path.is_none(),
            "prompt_path should be None when .md doesn't exist"
        );
    }

    #[serial_test::serial]
    #[test]
    fn mission_compiler_role_loads_correctly() {
        let _guard = CrewDirGuard::new(TempDir::new().unwrap());
        // No user files written — loader should fall through to builtin mission-compiler.
        let roles = load_roles().unwrap();
        let mc = roles.iter().find(|r| r.id == "mission-compiler").expect("builtin mission-compiler should load");
        assert_eq!(mc.id, "mission-compiler");
        assert_eq!(mc.skills, vec!["mission-compiling".to_string()]);
        assert_eq!(mc.tool_palette.allow, vec!["read".to_string()]);
        assert_eq!(mc.tool_palette.deny, vec!["edit", "write", "exec", "process"]);
    }
