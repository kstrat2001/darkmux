    use super::*;

    fn lbl(kind: &str, anchor: Option<&str>) -> Label {
        Label {
            kind: kind.into(),
            intent_title: "t".into(),
            intent_body: String::new(),
            expect_verdict: if kind == "bug" { "flag".into() } else { "pass".into() },
            bug_class: None,
            anchor_contains: anchor.map(str::to_string),
            expected: Vec::new(),
            notes: None,
        }
    }

    fn ef(anchor: &str, access_gap: bool) -> ExpectedFinding {
        ExpectedFinding {
            anchor_contains: anchor.into(),
            match_contains: None,
            severity: None,
            bug_class: None,
            access_gap,
            required: true,
            notes: None,
        }
    }

    fn ef_opt(anchor: &str) -> ExpectedFinding {
        ExpectedFinding {
            required: false,
            ..ef(anchor, false)
        }
    }

    fn multi_lbl(kind: &str, expected: Vec<ExpectedFinding>) -> Label {
        Label {
            kind: kind.into(),
            intent_title: "t".into(),
            intent_body: String::new(),
            expect_verdict: if kind == "bug" { "flag".into() } else { "pass".into() },
            bug_class: None,
            anchor_contains: None,
            expected,
            notes: None,
        }
    }

    fn finding(sev: &str, anchor: &str, title: &str) -> Finding {
        Finding {
            severity: sev.into(),
            anchor: anchor.into(),
            title: title.into(),
        }
    }

    fn flagged(findings: Vec<Finding>) -> Review {
        Review {
            verdict: "flag".into(),
            findings,
            parsed: true,
        }
    }

    #[test]
    fn parse_review_plain_json() {
        let r = parse_review(r#"{"verdict":"pass","findings":[]}"#);
        assert!(r.parsed);
        assert_eq!(r.verdict, "pass");
        assert!(r.findings.is_empty());
    }

    #[test]
    fn parse_review_fenced_with_prose() {
        let r = parse_review("Here is my review:\n```json\n{\"verdict\":\"flag\",\"findings\":[{\"severity\":\"HIGH\",\"anchor\":\"let sql = format!(x)\",\"title\":\"SQLi\"}]}\n```\nDone.");
        assert!(r.parsed);
        assert_eq!(r.verdict, "flag");
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].severity, "high"); // lowercased
    }

    #[test]
    fn parse_review_findings_with_braces_in_strings() {
        // a suggestion/detail containing { } must not break extraction.
        let r = parse_review(r#"{"verdict":"flag","findings":[{"severity":"high","anchor":"a","title":"x","suggestion":"fn f() { ok }"}]}"#);
        assert!(r.parsed);
        assert_eq!(r.findings.len(), 1);
    }

    #[test]
    fn parse_review_degenerate_when_no_verdict() {
        assert!(!parse_review("").parsed);
        assert!(!parse_review("just some reasoning, no json").parsed);
        assert!(!parse_review(r#"{"summary":"thought about it"}"#).parsed);
    }

    // ── agentic mode (#1197 anchor experiment) ──

    fn tiny_case() -> Case {
        Case {
            id: "c1".into(),
            label: Label {
                kind: "bug".into(),
                intent_title: "t".into(),
                intent_body: String::new(),
                expect_verdict: "flag".into(),
                bug_class: None,
                anchor_contains: None,
                expected: Vec::new(),
                notes: None,
            },
            diff: "+ x".into(),
        }
    }

    /// The evidence sentence is mode-load-bearing: diff-only modes must SAY
    /// the diff is all there is; agentic mode must say the repo is checked
    /// out (telling a tool-wearing model it "has only this diff" would fight
    /// its explore-before-concluding role directives — and the reverse claim
    /// on a tool-less model invites fabricated "I read the file" prose).
    #[test]
    fn build_prompt_evidence_sentence_matches_mode() {
        let c = tiny_case();
        for mode in [BenchMode::Strict, BenchMode::FreeForm] {
            let p = build_prompt(&c, mode);
            assert!(
                p.contains("you have only this diff"),
                "{mode:?} must declare diff-only evidence"
            );
            assert!(!p.contains("checked out in your working directory"));
        }
        let p = build_prompt(&c, BenchMode::Agentic);
        assert!(p.contains("checked out in your working directory"));
        assert!(
            !p.contains("you have only this diff"),
            "agentic prompt must not contradict the mounted repo"
        );
    }

    #[test]
    fn bench_mode_role_and_label_mapping() {
        assert_eq!(BenchMode::Strict.role_id(), "pr-reviewer");
        assert_eq!(BenchMode::FreeForm.role_id(), "pr-reviewer-freeform");
        assert_eq!(BenchMode::Agentic.role_id(), "pr-reviewer-agentic");
        assert_eq!(BenchMode::Agentic.label(), "agentic");
        assert_eq!(BenchMode::Funnel.label(), "funnel");
    }

    #[test]
    #[should_panic(expected = "funnel dispatches")]
    fn bench_mode_funnel_role_id_is_unreachable() {
        // Funnel dispatches review-probe/review-judge seats directly — the
        // per-case loop branches to `run_funnel_case` before ever calling
        // `role_id()` on the mode. Pins that the panic message names why.
        let _ = BenchMode::Funnel.role_id();
    }

    /// The agentic role's marker dialect (`MUST FIX [path] `anchor``) parses
    /// through the same freeform parser — the bracket form has no colon, and
    /// `strip_marker` tolerates that. Pins the dialect compatibility the
    /// agentic mode depends on.
    #[test]
    fn freeform_parser_accepts_agentic_bracket_dialect() {
        let r = parse_freeform_review(
            "Traced the change.\n\n\
             MUST FIX [app/models/billing.ts] `billingEndAt.plus({ days: 1 })`\n\
             The boundary is off by one on the last day of the cycle.\n\n\
             VERDICT: flag",
        );
        assert_eq!(r.verdict, "flag");
        assert_eq!(r.findings.len(), 1);
        assert!(r.findings[0].anchor.contains("billingEndAt.plus"));
    }

    // ── free-form parsing (#1119 free-form mode) ──

    #[test]
    fn freeform_no_markers_is_a_real_pass_not_degenerate() {
        let r = parse_freeform_review(
            "I traced the changed function end to end. The guard correctly \
             handles the null case the PR description mentions. This looks sound.",
        );
        assert!(r.parsed, "non-empty prose with no markers is a real pass, not degenerate");
        assert_eq!(r.verdict, "pass");
        assert!(r.findings.is_empty());
    }

    #[test]
    fn freeform_empty_text_is_degenerate() {
        assert!(!parse_freeform_review("").parsed);
        assert!(!parse_freeform_review("   \n  ").parsed);
    }

    #[test]
    fn freeform_must_fix_line_is_high_and_flags() {
        let r = parse_freeform_review(
            "I looked through the diff.\n\n\
             MUST FIX: the call to .startOf('day') is not applied to both sides of \
             the range comparison in calc.ts:40, so results are undercounted near a \
             day boundary.\n\n\
             The rest of the change looks fine.",
        );
        assert_eq!(r.verdict, "flag");
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].severity, "high");
        assert!(r.findings[0].title.contains("startOf('day')"));
        assert!(r.findings[0].title.contains("calc.ts:40"));
    }

    #[test]
    fn freeform_consider_line_is_medium_and_does_not_flag() {
        let r = parse_freeform_review("CONSIDER: adding a test for the empty-array case.");
        assert_eq!(r.verdict, "pass", "CONSIDER alone must not flag (mirrors severity=medium)");
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].severity, "medium");
    }

    #[test]
    fn freeform_multiple_markers_each_become_a_finding() {
        let r = parse_freeform_review(
            "MUST FIX: bug one in foo.rs:1.\n\
             CONSIDER: a style nit in foo.rs:9.\n\
             MUST FIX: bug two in bar.rs:5.",
        );
        assert_eq!(r.verdict, "flag");
        assert_eq!(r.findings.len(), 3);
        assert_eq!(r.findings.iter().filter(|f| f.severity == "high").count(), 2);
        assert_eq!(r.findings.iter().filter(|f| f.severity == "medium").count(), 1);
    }

    #[test]
    fn freeform_tolerates_bullets_and_bold_markdown() {
        let cases = [
            "- MUST FIX: a bug",
            "* MUST FIX: a bug",
            "**MUST FIX:** a bug",
            "- **MUST FIX:** a bug",
        ];
        for c in cases {
            let r = parse_freeform_review(c);
            assert_eq!(r.findings.len(), 1, "failed on {c:?}");
            assert_eq!(r.findings[0].severity, "high", "failed on {c:?}");
            assert!(r.findings[0].title.contains("a bug"), "failed on {c:?}: {:?}", r.findings[0].title);
        }
    }

    #[test]
    fn freeform_continuation_lines_fold_into_the_finding() {
        let r = parse_freeform_review(
            "MUST FIX: the totals are wrong.\n\
             This is because dailyComplexCharges is never summed alongside\n\
             sumComplexCharges, so the dashboard undercounts.\n\n\
             CONSIDER: a follow-up test.",
        );
        assert_eq!(r.findings.len(), 2);
        assert!(r.findings[0].title.contains("dailyComplexCharges"));
        assert!(r.findings[0].title.contains("sumComplexCharges"));
    }

    #[test]
    fn freeform_non_ascii_prose_near_a_would_be_marker_does_not_panic() {
        // A line starting with multi-byte UTF-8 must never panic strip_marker's
        // byte slicing (str::get, not direct indexing) even when it's short or
        // lands mid-character relative to the marker's byte length.
        let r = parse_freeform_review("😀 the diff looks fine, no MUST FIX here.\n中文 CONSIDER test\n短");
        assert!(r.parsed);
        assert!(r.findings.is_empty(), "marker mid-line without a line-start match must not register");
    }

    #[test]
    fn freeform_scores_through_the_same_multi_finding_matcher() {
        // Proves the free-form path is a drop-in for the existing scorer: the
        // same expected-finding schema, matched via anchor/title substring.
        let label = multi_lbl("bug", vec![ef("calc.ts:40", false)]);
        let r = parse_freeform_review(
            "MUST FIX: calc.ts:40 undercounts near a day boundary because the \
             range comparison isn't day-aligned.",
        );
        let s = score(&label, &r);
        assert!(s.recall);
        assert_eq!(s.bugs_caught, 1);
        assert_eq!(s.tp, 1);
        assert_eq!(s.fp, 0);
    }

    #[test]
    fn score_clean_pass_is_correct() {
        let s = score(&lbl("clean", None), &Review { verdict: "pass".into(), findings: vec![], parsed: true });
        assert_eq!(s.fp, 0);
        assert!(s.correct);
    }

    #[test]
    fn score_clean_with_finding_is_false_positive() {
        let r = Review { verdict: "pass".into(), findings: vec![Finding { severity: "medium".into(), ..Default::default() }], parsed: true };
        let s = score(&lbl("clean", None), &r);
        assert_eq!(s.fp, 1);
        assert!(!s.correct); // a finding on a clean diff is wrong even if verdict=pass
    }

    #[test]
    fn score_bug_recall_via_anchor() {
        let r = Review { verdict: "flag".into(), findings: vec![Finding { severity: "high".into(), anchor: "let sql = format!(\"SELECT ...\", name)".into(), title: "SQLi".into() }], parsed: true };
        let s = score(&lbl("bug", Some("format!(\"SELECT")), &r);
        assert!(s.recall);
        assert!(s.anchor_ok);
        assert!(s.correct);
    }

    #[test]
    fn score_bug_recall_via_high_severity_without_anchor_match() {
        let r = Review { verdict: "flag".into(), findings: vec![Finding { severity: "high".into(), anchor: "wrong line".into(), title: "SQLi".into() }], parsed: true };
        let s = score(&lbl("bug", Some("format!")), &r);
        assert!(s.recall); // high severity counts as caught
        assert!(!s.anchor_ok); // but the anchor missed
    }

    #[test]
    fn score_bug_empty_flag_is_contract_violation_not_recall() {
        // verdict=flag with zero findings (the gpt-oss failure mode).
        let r = Review { verdict: "flag".into(), findings: vec![], parsed: true };
        let s = score(&lbl("bug", Some("format!")), &r);
        assert!(s.empty_flag);
        assert!(!s.recall);
        assert!(!s.correct);
    }

    #[test]
    fn score_degenerate_review() {
        let s = score(&lbl("clean", None), &Review::default());
        assert!(s.degenerate);
        assert!(!s.correct);
    }

    #[test]
    fn load_cases_rejects_unknown_kind() {
        let tmp = tempfile::TempDir::new().unwrap();
        let d = tmp.path();
        fs::write(
            d.join("x.label.json"),
            r#"{"kind":"Regression","intent_title":"t","expect_verdict":"flag"}"#,
        )
        .unwrap();
        fs::write(d.join("x.diff"), "diff --git a b\n").unwrap();
        let err = load_cases(d).unwrap_err();
        assert!(
            err.to_string().contains(r#"must be "clean" or "bug""#),
            "got: {err}"
        );
    }

    #[test]
    fn load_cases_loads_a_good_pair() {
        let tmp = tempfile::TempDir::new().unwrap();
        let d = tmp.path();
        fs::write(
            d.join("c.label.json"),
            r#"{"kind":"clean","intent_title":"t","expect_verdict":"pass"}"#,
        )
        .unwrap();
        fs::write(d.join("c.diff"), "diff --git a b\n").unwrap();
        let cases = load_cases(d).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].id, "c");
        assert_eq!(cases[0].label.kind, "clean");
    }

    // ── multi-finding path (#1119) ──

    #[test]
    fn multi_two_bugs_partial_recall() {
        let label = multi_lbl("bug", vec![ef("alpha.rs:10", false), ef("beta.rs:20", false)]);
        let r = flagged(vec![finding("high", "alpha.rs:10", "off-by-one")]);
        let s = score(&label, &r);
        assert_eq!(s.expected_bugs, 2);
        assert_eq!(s.bugs_caught, 1);
        assert!(!s.recall, "not all bugs caught");
        assert_eq!(s.tp, 1);
        assert_eq!(s.fp, 0);
    }

    #[test]
    fn multi_extra_finding_is_fp_on_bug_case() {
        // The new behavior: a junk finding on a BUG case is a false positive
        // (legacy scoring only counted FPs on clean cases).
        let label = multi_lbl("bug", vec![ef("alpha.rs:10", false)]);
        let r = flagged(vec![
            finding("high", "alpha.rs:10", "real bug"),
            finding("medium", "unrelated.rs:99", "reflexive null check"),
        ]);
        let s = score(&label, &r);
        assert_eq!(s.bugs_caught, 1);
        assert!(s.recall);
        assert_eq!(s.tp, 1);
        assert_eq!(s.fp, 1, "the unrelated finding is a false positive");
    }

    #[test]
    fn multi_access_vs_diff_split() {
        let label = multi_lbl("bug", vec![ef("comp.tsx:5", true), ef("calc.ts:40", false)]);
        let r = flagged(vec![
            finding("high", "comp.tsx:5", "block-in-span"),
            finding("high", "calc.ts:40", "undercount"),
        ]);
        let s = score(&label, &r);
        assert_eq!(s.bugs_caught, 2);
        assert_eq!(s.caught_access, 1);
        assert_eq!(s.caught_diff, 1);
        assert_eq!(s.expected_access, 1);
        assert_eq!(s.expected_diff, 1);
        assert!(s.recall);
    }

    #[test]
    fn multi_match_contains_recalls_without_precise_anchor() {
        let label = multi_lbl(
            "bug",
            vec![ExpectedFinding {
                anchor_contains: "calc.ts:40".into(),
                match_contains: Some("undercount".into()),
                severity: None,
                bug_class: None,
                access_gap: false,
                required: true,
                notes: None,
            }],
        );
        // Model flags the right bug (title matches) but anchors the wrong line.
        let r = flagged(vec![finding("high", "calc.ts:12", "day undercount off-by-one")]);
        let s = score(&label, &r);
        assert_eq!(s.bugs_caught, 1, "recalled via match_contains");
        assert!(s.recall);
        assert_eq!(s.anchors_ok, 0, "anchor was imprecise");
        assert!(!s.anchor_ok);
    }

    #[test]
    fn multi_pass_verdict_does_not_credit_recall() {
        // A matching finding under a `pass` verdict: precision credits the TP,
        // but recall does not (the model contradicted itself; it did not flag).
        let label = multi_lbl("bug", vec![ef("alpha.rs:10", false)]);
        let r = Review {
            verdict: "pass".into(),
            findings: vec![finding("high", "alpha.rs:10", "real bug")],
            parsed: true,
        };
        let s = score(&label, &r);
        assert_eq!(s.bugs_caught, 0, "pass verdict: not flagged");
        assert!(!s.recall);
        assert_eq!(s.tp, 1, "the finding still matches a real bug");
    }

    #[test]
    fn multi_all_caught_is_correct() {
        let label = multi_lbl("bug", vec![ef("a:1", false), ef("b:2", true)]);
        let r = flagged(vec![finding("high", "a:1", "x"), finding("medium", "b:2", "y")]);
        let s = score(&label, &r);
        assert!(s.recall);
        assert!(s.correct);
        assert_eq!(s.fp, 0);
    }

    #[test]
    fn multi_max_matching_beats_greedy() {
        // Frontier QA #1: overlapping match keys — a file-level bug + a
        // line-specific bug in the same file; the model flagged BOTH (one at the
        // exact line, one elsewhere in the file). Greedy would strand one bug as
        // a miss + a spurious FP; maximum matching credits both.
        let label = multi_lbl("bug", vec![ef("calc.ts", false), ef("calc.ts:40", false)]);
        let r = flagged(vec![
            finding("high", "calc.ts:40", "exact"),
            finding("high", "calc.ts:88", "elsewhere in the same file"),
        ]);
        let s = score(&label, &r);
        assert_eq!(s.bugs_caught, 2, "both bugs caught under max matching");
        assert_eq!(s.tp, 2);
        assert_eq!(s.fp, 0, "no spurious false positive");
        assert!(s.recall);
    }

    #[test]
    fn multi_duplicate_finding_is_fp() {
        // Two identical findings for one bug: one TP, one FP (max matching
        // preserves duplicate-as-FP).
        let label = multi_lbl("bug", vec![ef("alpha.rs:10", false)]);
        let r = flagged(vec![
            finding("high", "alpha.rs:10", "bug"),
            finding("high", "alpha.rs:10", "bug again"),
        ]);
        let s = score(&label, &r);
        assert_eq!(s.bugs_caught, 1);
        assert_eq!(s.tp, 1);
        assert_eq!(s.fp, 1, "the duplicate is a false positive");
    }

    #[test]
    fn multi_optional_finding_is_tp_not_recall() {
        // A required bug + an optional nit, both flagged. The optional match is a
        // TP (not an FP) but NOT in the recall denominator — keeps the control at
        // ~100% precision on its own labels.
        let label = multi_lbl("bug", vec![ef("a:1", false), ef_opt("nit.rs:9")]);
        let r = flagged(vec![
            finding("high", "a:1", "real"),
            finding("low", "nit.rs:9", "nit"),
        ]);
        let s = score(&label, &r);
        assert_eq!(s.expected_bugs, 1, "only the required bug counts for recall");
        assert_eq!(s.bugs_caught, 1);
        assert!(s.recall);
        assert_eq!(s.tp, 2, "both findings match an expected (required + optional)");
        assert_eq!(s.fp, 0, "the optional-nit match is not a false positive");
    }

    #[test]
    fn multi_clean_with_optional_only() {
        // A clean-ish case carrying only an optional nit: 0 required bugs (no
        // recall impact); flagging the nit is a TP, junk is an FP.
        let label = multi_lbl("clean", vec![ef_opt("nit.rs:9")]);
        let ok = score(&label, &flagged(vec![finding("low", "nit.rs:9", "nit")]));
        assert_eq!(ok.expected_bugs, 0);
        assert_eq!(ok.tp, 1);
        assert_eq!(ok.fp, 0);
        assert!(ok.correct, "no false positives ⇒ correct");
        let junk = score(&label, &flagged(vec![finding("high", "other.rs:1", "junk")]));
        assert_eq!(junk.fp, 1);
        assert!(!junk.correct);
    }

    #[test]
    fn load_cases_rejects_empty_expected_match_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        let d = tmp.path();
        fs::write(
            d.join("x.label.json"),
            r#"{"kind":"bug","intent_title":"t","expected":[{"anchor_contains":""}]}"#,
        )
        .unwrap();
        fs::write(d.join("x.diff"), "diff --git a b\n").unwrap();
        let err = load_cases(d).unwrap_err();
        assert!(err.to_string().contains("matches every finding"), "got: {err}");
    }

    #[test]
    fn load_cases_rejects_clean_with_required_bug() {
        let tmp = tempfile::TempDir::new().unwrap();
        let d = tmp.path();
        fs::write(
            d.join("x.label.json"),
            r#"{"kind":"clean","intent_title":"t","expected":[{"anchor_contains":"a:1"}]}"#,
        )
        .unwrap();
        fs::write(d.join("x.diff"), "diff --git a b\n").unwrap();
        let err = load_cases(d).unwrap_err();
        assert!(err.to_string().contains("must not carry a required"), "got: {err}");
    }

    #[test]
    fn multi_optional_does_not_steal_recall_from_required() {
        // Frontier QA (2nd pass) #1: a required + an optional expected share an
        // overlapping match key, and the model emits ONE finding that IS the
        // required bug. Max-cardinality over the full set could spend it on the
        // optional; the required-only matching must not. Recall must be TRUE
        // regardless of expected[] order.
        let req = ef("calc.ts:40", false); // required (default)
        let opt = ef_opt("calc.ts"); // optional, subsuming match key
        let f = finding("high", "calc.ts:40", "the required bug");
        for order in [vec![opt.clone(), req.clone()], vec![req.clone(), opt.clone()]] {
            let s = score(&multi_lbl("bug", order), &flagged(vec![f.clone()]));
            assert_eq!(s.expected_bugs, 1);
            assert!(s.recall, "required bug caught regardless of expected[] order");
            assert_eq!(s.bugs_caught, 1);
            assert_eq!(s.tp, 1, "the one finding is a true positive");
        }
    }

    #[test]
    fn load_cases_rejects_bug_with_no_required() {
        let tmp = tempfile::TempDir::new().unwrap();
        let d = tmp.path();
        fs::write(
            d.join("x.label.json"),
            r#"{"kind":"bug","intent_title":"t","expected":[{"anchor_contains":"a:1","required":false}]}"#,
        )
        .unwrap();
        fs::write(d.join("x.diff"), "diff --git a b\n").unwrap();
        let err = load_cases(d).unwrap_err();
        assert!(err.to_string().contains("at least one required"), "got: {err}");
    }

    // ─── #1198: scores.json emission ────────────────────────────────

    #[test]
    fn envelope_meta_extracts_model_and_tokens() {
        let stdout = "pulling image...\n{\"result\":\"stop\",\"metrics\":{\"model\":\"m-x\",\"prompt_tokens\":100,\"completion_tokens\":25}}";
        let m = envelope_meta(stdout);
        assert_eq!(m.model.as_deref(), Some("m-x"));
        assert_eq!(m.total_tokens, Some(125));
        // Garbage stdout degrades to None, never errors.
        let g = envelope_meta("not json at all");
        assert!(g.model.is_none() && g.total_tokens.is_none());
    }

    #[test]
    fn build_score_rows_maps_outcomes_and_aggregates() {
        use crate::lab::scores::{ArtifactKey, Outcome};
        let mk_case = |id: &str, kind: &str| Case {
            id: id.into(),
            label: Label {
                kind: kind.into(),
                intent_title: "t".into(),
                intent_body: String::new(),
                expect_verdict: String::new(),
                bug_class: None,
                anchor_contains: None,
                expected: vec![],
                notes: None,
            },
            diff: String::new(),
        };
        let clean_case = mk_case("c1", "clean");
        let bug_case = mk_case("b1", "bug");
        let clean_pass = CaseScore {
            correct: true,
            verdict: "pass".into(),
            ..Default::default()
        };
        let bug_degenerate = CaseScore {
            degenerate: true,
            ..Default::default()
        };
        let scored: Vec<(&Case, CaseScore)> =
            vec![(&clean_case, clean_pass), (&bug_case, bug_degenerate)];
        let meta = vec![
            EnvelopeMeta {
                model: Some("m-x".into()),
                total_tokens: Some(500),
            },
            EnvelopeMeta::default(),
        ];
        let artifact = ArtifactKey {
            model: "m-x".into(),
            ..Default::default()
        };
        let rows = build_score_rows(&scored, &meta, &artifact);

        // Two per-case rows + the clean_pass_rate aggregate (no multi schema).
        let case_rows: Vec<_> = rows.iter().filter(|r| r.axis == "case").collect();
        assert_eq!(case_rows.len(), 2);
        assert_eq!(case_rows[0].outcome, Outcome::Pass);
        assert_eq!(case_rows[0].tokens_to_solution, Some(500));
        // A degenerate review is a CAPABILITY failure (the dispatch ran).
        assert_eq!(case_rows[1].outcome, Outcome::CapabilityFail);
        let agg = rows.iter().find(|r| r.axis == "clean_pass_rate").unwrap();
        assert_eq!(agg.value, Some(1.0));
        assert!(
            !rows.iter().any(|r| r.axis == "recall"),
            "multi-schema aggregates only appear when the corpus uses them"
        );
        // Every row carries the artifact key + native source.
        assert!(rows.iter().all(|r| r.artifact.model == "m-x" && r.source == "native"));
    }

    /// (#1210) A degenerate case whose dispatch served ZERO tokens is an INFRA
    /// failure (rate-limited / unreachable endpoint / dead dispatch) — routed
    /// to `Outcome::InfraFail` and EXCLUDED from the capability denominators,
    /// never a `CapabilityFail` zero against the model. A degenerate case that
    /// served tokens (model ran, output unparseable) stays a capability fail.
    #[test]
    fn build_score_rows_zero_token_degenerate_is_infra_not_capability() {
        use crate::lab::scores::{ArtifactKey, Outcome};
        let mk_case = |id: &str, kind: &str| Case {
            id: id.into(),
            label: Label {
                kind: kind.into(),
                intent_title: "t".into(),
                intent_body: String::new(),
                expect_verdict: String::new(),
                bug_class: None,
                anchor_contains: None,
                expected: vec![],
                notes: None,
            },
            diff: String::new(),
        };
        let clean_ok = mk_case("c1", "clean");
        let clean_429 = mk_case("c2", "clean");
        let clean_ran = mk_case("c3", "clean");
        let pass = CaseScore { correct: true, verdict: "pass".into(), ..Default::default() };
        let degen = CaseScore { degenerate: true, ..Default::default() };
        let scored: Vec<(&Case, CaseScore)> = vec![
            (&clean_ok, CaseScore { correct: true, verdict: "pass".into(), ..Default::default() }),
            (&clean_429, CaseScore { degenerate: true, ..Default::default() }),
            (&clean_ran, CaseScore { degenerate: true, ..Default::default() }),
        ];
        let _ = (&pass, &degen);
        let meta = vec![
            EnvelopeMeta { model: Some("m-x".into()), total_tokens: Some(500) }, // ran + passed
            EnvelopeMeta { model: Some("m-x".into()), total_tokens: Some(0) },   // 429: zero served
            EnvelopeMeta { model: Some("m-x".into()), total_tokens: Some(200) }, // ran, unparseable
        ];
        let artifact = ArtifactKey { model: "m-x".into(), ..Default::default() };
        let rows = build_score_rows(&scored, &meta, &artifact);

        let case_rows: Vec<_> = rows.iter().filter(|r| r.axis == "case").collect();
        assert_eq!(case_rows.len(), 3);
        assert_eq!(case_rows[0].outcome, Outcome::Pass);
        // Zero tokens served → infra, not capability.
        assert_eq!(case_rows[1].outcome, Outcome::InfraFail);
        // Ran but produced unparseable output → still a capability failure.
        assert_eq!(case_rows[2].outcome, Outcome::CapabilityFail);

        // clean_pass_rate denominator EXCLUDES the infra case (c2): 1 pass of
        // the 2 clean cases that actually ran (c1 passed, c3 ran + degenerate),
        // never 1 of 3.
        let agg = rows.iter().find(|r| r.axis == "clean_pass_rate").unwrap();
        assert_eq!(agg.value, Some(0.5));
        let detail = &agg.detail;
        assert_eq!(detail["clean_cases"].as_u64(), Some(2));
    }

    /// (#1210 gate coverage) `is_infra_failure`'s three arms, the `None`
    /// tokens arm explicitly: only POSITIVE zero-token evidence reclassifies.
    /// The runtime envelope always emits numeric token fields (see the fn
    /// doc), so `None` means "no parseable envelope" — kept capability-side
    /// deliberately, never guessed into infra.
    #[test]
    fn is_infra_failure_requires_degenerate_and_positive_zero_token_evidence() {
        let degen = CaseScore { degenerate: true, ..Default::default() };
        let ran_fine = CaseScore { correct: true, ..Default::default() };
        let zero = EnvelopeMeta { model: None, total_tokens: Some(0) };
        let served = EnvelopeMeta { model: None, total_tokens: Some(250) };
        let unknown = EnvelopeMeta::default(); // total_tokens: None

        assert!(is_infra_failure(&degen, Some(&zero)), "degenerate + zero tokens = infra");
        assert!(!is_infra_failure(&degen, Some(&served)), "model ran = capability degenerate");
        assert!(!is_infra_failure(&degen, Some(&unknown)), "None tokens is NOT infra evidence");
        assert!(!is_infra_failure(&degen, None), "missing meta row is NOT infra evidence");
        assert!(!is_infra_failure(&ran_fine, Some(&zero)), "non-degenerate never reclassifies");
    }

    /// (#1210 gate coverage) `print_summary`'s partition: infra cases leave
    /// the capability set and are counted separately — via the shared pure
    /// `infra_partition` helper.
    #[test]
    fn infra_partition_splits_capability_from_infra() {
        let mk_case = |id: &str| Case {
            id: id.into(),
            label: Label {
                kind: "clean".into(),
                intent_title: "t".into(),
                intent_body: String::new(),
                expect_verdict: String::new(),
                bug_class: None,
                anchor_contains: None,
                expected: vec![],
                notes: None,
            },
            diff: String::new(),
        };
        let c1 = mk_case("c1");
        let c2 = mk_case("c2");
        let c3 = mk_case("c3");
        let scored: Vec<(&Case, CaseScore)> = vec![
            (&c1, CaseScore { correct: true, ..Default::default() }),
            (&c2, CaseScore { degenerate: true, ..Default::default() }), // 429: zero tokens
            (&c3, CaseScore { degenerate: true, ..Default::default() }), // ran, unparseable
        ];
        let meta = vec![
            EnvelopeMeta { model: None, total_tokens: Some(500) },
            EnvelopeMeta { model: None, total_tokens: Some(0) },
            EnvelopeMeta { model: None, total_tokens: Some(120) },
        ];
        let (capability, infra) = infra_partition(&scored, &meta);
        assert_eq!(infra, 1, "exactly the zero-token case");
        assert_eq!(capability.len(), 2);
        assert!(capability.iter().any(|(c, _)| c.id == "c1"));
        assert!(capability.iter().any(|(c, _)| c.id == "c3"), "capability-degenerate stays");
    }

    #[test]
    fn build_score_rows_emits_multi_schema_aggregates() {
        use crate::lab::scores::ArtifactKey;
        let bug_case = Case {
            id: "b1".into(),
            label: Label {
                kind: "bug".into(),
                intent_title: "t".into(),
                intent_body: String::new(),
                expect_verdict: String::new(),
                bug_class: None,
                anchor_contains: None,
                expected: vec![serde_json::from_str::<ExpectedFinding>(
                    r#"{"anchor_contains":"x"}"#,
                )
                .unwrap()],
                notes: None,
            },
            diff: String::new(),
        };
        let s = CaseScore {
            expected_bugs: 2,
            bugs_caught: 1,
            tp: 1,
            fp: 1,
            anchors_ok: 1,
            ..Default::default()
        };
        let scored: Vec<(&Case, CaseScore)> = vec![(&bug_case, s)];
        let rows = build_score_rows(
            &scored,
            &[EnvelopeMeta::default()],
            &ArtifactKey {
                model: "m".into(),
                ..Default::default()
            },
        );
        let recall = rows.iter().find(|r| r.axis == "recall").unwrap();
        assert_eq!(recall.value, Some(0.5));
        let precision = rows.iter().find(|r| r.axis == "precision").unwrap();
        assert_eq!(precision.value, Some(0.5)); // 1 tp / (1 tp + 1 fp)
        let anchor = rows.iter().find(|r| r.axis == "anchor_rate").unwrap();
        assert_eq!(anchor.value, Some(1.0));
    }

    // ── funnel mode (#1222 Phase B packet 7) ──────────────────────────

    fn probe_flag(anchor: Option<&str>) -> super::super::review::ProbeFlag {
        super::super::review::ProbeFlag {
            bundle_id: "billing.ts".into(),
            fact_family: "unscoped".into(),
            member: "darkmux:probe-model".into(),
            draw: 0,
            charge_text: "the clamp is bypassed".into(),
            anchor: anchor.map(str::to_string),
            also_flagged: Vec::new(),
        }
    }

    fn judge_record(ruling: super::super::review::JudgeRuling, note: &str, evidence: &str) -> super::super::review::JudgeRecord {
        super::super::review::JudgeRecord {
            ruling,
            decisive_evidence: evidence.to_string(),
            note_for_author: note.to_string(),
            pass: 1,
            seconds: 0.1,
        }
    }

    fn judged_flag(
        anchor: Option<&str>,
        tier: super::super::review::Tier,
        note: &str,
        evidence: &str,
    ) -> super::super::review::JudgedFlag {
        use super::super::review::JudgeRuling;
        let ruling = match tier {
            super::super::review::Tier::Confirmed => JudgeRuling::Confirmed,
            super::super::review::Tier::NeedsCheck => JudgeRuling::NeedsCheck,
            super::super::review::Tier::Archived => JudgeRuling::FalsePositive,
        };
        super::super::review::JudgedFlag {
            flag: probe_flag(anchor),
            pass1: judge_record(ruling, note, evidence),
            pass2: None,
            tier,
            demoted_by_pass2: false,
            verify: None,
            demoted_by_verify: false,
        }
    }

    fn funnel_env(judged: Vec<super::super::review::JudgedFlag>, degenerate: Option<&str>) -> super::super::review::ReviewEnvelope {
        use super::super::review::Tier;
        let confirmed = judged.iter().filter(|j| j.tier == Tier::Confirmed).count();
        let needs_check = judged.iter().filter(|j| j.tier == Tier::NeedsCheck).count();
        let archived = judged.iter().filter(|j| j.tier == Tier::Archived).count();
        super::super::review::ReviewEnvelope {
            case_id: "c1".into(),
            crew: "test-crew".into(),
            mode: "sequential".into(),
            bundles: 1,
            raw_flags: judged.len(),
            deduped_flags: judged.len(),
            confirmed,
            needs_check,
            archived,
            judged,
            degenerate: degenerate.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn review_from_funnel_confirmed_with_anchor_becomes_a_finding() {
        use super::super::review::Tier;
        let env = funnel_env(
            vec![judged_flag(
                Some("const end = start.plus(30)"),
                Tier::Confirmed,
                "the clamp is bypassed",
                "start.plus(30) skips the cap",
            )],
            None,
        );
        let r = review_from_funnel(&env);
        assert!(r.parsed);
        assert_eq!(r.verdict, "flag");
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].anchor, "const end = start.plus(30)");
        assert!(r.findings[0].title.contains("the clamp is bypassed"));
        assert!(r.findings[0].title.contains("start.plus(30) skips the cap"));
        assert_eq!(r.findings[0].severity, "high");
    }

    #[test]
    fn review_from_funnel_confirmed_without_anchor_still_becomes_a_finding_with_empty_anchor() {
        use super::super::review::Tier;
        let env = funnel_env(
            vec![judged_flag(None, Tier::Confirmed, "real bug", "evidence")],
            None,
        );
        let r = review_from_funnel(&env);
        assert_eq!(r.verdict, "flag");
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].anchor, "", "no dedup anchor ⇒ empty anchor, never dropped");
    }

    #[test]
    fn review_from_funnel_needs_check_and_archived_are_excluded_from_findings() {
        use super::super::review::Tier;
        let env = funnel_env(
            vec![
                judged_flag(Some("a"), Tier::NeedsCheck, "unsure", "ambiguous"),
                judged_flag(Some("b"), Tier::Archived, "false alarm", "ruled out"),
            ],
            None,
        );
        let r = review_from_funnel(&env);
        assert_eq!(r.verdict, "pass", "no confirmed flags ⇒ pass");
        assert!(r.findings.is_empty(), "needs_check/archived never become findings");
        assert!(r.parsed);
    }

    #[test]
    fn review_from_funnel_degenerate_envelope_is_not_parsed() {
        let env = funnel_env(vec![], Some("zero flags from all probe draws — never a silent pass"));
        let r = review_from_funnel(&env);
        assert!(!r.parsed, "a degenerate funnel run must score distinctly from a real pass");
    }

    #[test]
    fn review_from_funnel_scores_through_the_multi_finding_matcher() {
        use super::super::review::Tier;
        let label = multi_lbl("bug", vec![ef("const end = start.plus(30)", false)]);
        let env = funnel_env(
            vec![judged_flag(
                Some("const end = start.plus(30)"),
                Tier::Confirmed,
                "boundary bug",
                "off by one",
            )],
            None,
        );
        let r = review_from_funnel(&env);
        let s = score(&label, &r);
        assert!(s.recall);
        assert_eq!(s.bugs_caught, 1);
        assert_eq!(s.fp, 0);
    }

    // ── parse_exec_mode ─────────────────────────────────────────────

    #[test]
    fn parse_exec_mode_defaults_and_values() {
        use super::super::review::ExecMode;
        assert_eq!(parse_exec_mode(None).unwrap(), ExecMode::Auto);
        assert_eq!(parse_exec_mode(Some("auto")).unwrap(), ExecMode::Auto);
        assert_eq!(parse_exec_mode(Some("Sequential")).unwrap(), ExecMode::Sequential);
        assert_eq!(parse_exec_mode(Some("PARALLEL")).unwrap(), ExecMode::Parallel);
        assert!(parse_exec_mode(Some("bogus")).is_err());
    }

    // ── funnel coverage gap review (#1222 Phase B packet 7) ────────────
    //
    // Everything below characterizes wiring the packet's own unit tests
    // (above) didn't reach: score()'s treatment of a Confirmed flag with NO
    // dedup anchor, `write_scores_artifact`'s funnel-specific artifact
    // discipline (previously untested even for `debates.json` — no test in
    // this module ever constructed a full `ReviewBenchOpts`), the real
    // `run_funnel_case` pipeline's degenerate-envelope + `--bundler`
    // plumbing (both reachable offline because a zero-bundle/failed-bundle
    // run short-circuits BEFORE any chat dispatch), and `resolve_funnel_ctx`'s
    // crew-not-found + `--k`/`--exec-mode` plumbing.

    fn dummy_pm(id: &str) -> darkmux_types::ProfileModel {
        darkmux_types::ProfileModel {
            id: id.to_string(),
            n_ctx: Some(32_000),
            ..Default::default()
        }
    }

    fn funnel_case() -> Case {
        Case {
            id: "c1".into(),
            label: multi_lbl("bug", vec![ef("start.plus(30)", false)]),
            diff: "+ const end = start.plus(30)".into(),
        }
    }

    fn funnel_opts(scores_out: PathBuf, with_exec_mode_and_k: bool) -> ReviewBenchOpts {
        ReviewBenchOpts {
            cases_dir: PathBuf::from("."),
            role: "pr-reviewer".into(),
            profile_name: Some("test-profile".into()),
            config_path: None,
            timeout_seconds: 60,
            scores_out: Some(scores_out),
            mode: BenchMode::Funnel,
            workdirs: None,
            prosecutor_profile: None,
            defender_profile: None,
            judge_profile: None,
            roster_profile: Some("test-crew".into()),
            exec_mode: if with_exec_mode_and_k { Some("sequential".into()) } else { None },
            k_override: if with_exec_mode_and_k { Some(5) } else { None },
            bundler_cmd: None,
        }
    }

    // ── score() on a no-anchor Confirmed flag ("does score() treat it right?") ──

    #[test]
    fn score_of_funnel_review_empty_anchor_confirmed_flag_never_matches_anchor_only_expected() {
        use super::super::review::Tier;
        // A Confirmed flag with NO dedup anchor (`extract_new_side_anchor`
        // found no backtick-quoted span matching the diff) maps to
        // `Finding { anchor: "" }` (review_from_funnel's documented
        // behavior). When the label's expected finding relies on
        // `anchor_contains` alone (no `match_contains`),
        // `finding_matches_expected` falls back to `f.anchor.contains(...)`
        // — an empty anchor can never contain a non-empty substring, so this
        // confirmed flag scores as a pure false positive, never a catch.
        let label = multi_lbl("bug", vec![ef("start.plus(30)", false)]);
        let env = funnel_env(
            vec![judged_flag(None, Tier::Confirmed, "the clamp is bypassed", "start.plus(30) skips the cap")],
            None,
        );
        let r = review_from_funnel(&env);
        let s = score(&label, &r);
        assert!(!s.recall, "an anchor-only expected can never match an empty-anchor finding");
        assert_eq!(s.bugs_caught, 0);
        assert_eq!(s.fp, 1, "the unmatched confirmed flag is a false positive, not silently dropped");
    }

    #[test]
    fn score_of_funnel_review_empty_anchor_confirmed_flag_matches_via_match_contains_title() {
        use super::super::review::Tier;
        // Same no-anchor confirmed flag, but the label supplies
        // `match_contains` — `finding_matches_expected` then checks
        // `anchor + title` instead, so the flag's `note_for_author`/
        // `decisive_evidence`-derived title recovers the match even with an
        // empty anchor. Completes the "does score() treat it right?"
        // question: only when the label is written to allow it.
        let label = multi_lbl(
            "bug",
            vec![ExpectedFinding {
                match_contains: Some("clamp is bypassed".into()),
                ..ef("start.plus(30)", false)
            }],
        );
        let env = funnel_env(
            vec![judged_flag(None, Tier::Confirmed, "the clamp is bypassed", "start.plus(30) skips the cap")],
            None,
        );
        let r = review_from_funnel(&env);
        let s = score(&label, &r);
        assert!(s.recall, "match_contains recovers the catch even with no dedup anchor");
        assert_eq!(s.bugs_caught, 1);
        assert_eq!(s.fp, 0);
    }

    // ── LocalJsonlEmitter: file mechanics (#1247 review round) ──────────

    #[test]
    fn local_jsonl_emitter_appends_one_parseable_line_per_record() {
        use super::super::review::ReviewEmitter;

        fn rec(action: &str) -> darkmux_flow::FlowRecord {
            darkmux_flow::FlowRecord {
                ts: darkmux_flow::ts_utc_now(),
                level: darkmux_flow::Level::Info,
                category: darkmux_flow::Category::Work,
                tier: darkmux_flow::Tier::Local,
                stage: darkmux_flow::Stage::Dispatch,
                action: action.to_string(),
                handle: "test-crew".to_string(),
                phase_id: None,
                session_id: Some("c1".to_string()),
                source: Some("funnel".to_string()),
                model: None,
                reasoning: None,
                mission_id: None,
                machine_id: None,
                machine_uid: None,
                orchestrator: None,
                prev_hash: None,
                hash: None,
                payload: Some(serde_json::json!({"status": "started"})),
                work_id: None,
                attempt: None,
            }
        }

        let tmp = tempfile::TempDir::new().unwrap();
        // A nested, not-yet-existing parent proves the lazy create_dir_all.
        let path = tmp.path().join("run-dir").join("funnel-events.jsonl");
        let mut emitter = LocalJsonlEmitter::new(path.clone());

        // Construction alone must not touch the filesystem — a bench run in
        // a non-funnel mode constructs the emitter but never emits, and must
        // not leave an empty funnel-events.jsonl (or its dir) behind.
        assert!(!path.exists(), "no file before the first emit");
        assert!(!path.parent().unwrap().exists(), "no dir before the first emit");

        emitter.emit(rec("funnel.task"));
        assert!(path.is_file(), "the first emit creates dir + file");
        emitter.emit(rec("funnel.step"));

        let content = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "one JSONL line per emitted record");
        let first: darkmux_flow::FlowRecord = serde_json::from_str(lines[0]).expect("line 1 is a valid FlowRecord");
        let second: darkmux_flow::FlowRecord = serde_json::from_str(lines[1]).expect("line 2 is a valid FlowRecord");
        assert_eq!(first.action, "funnel.task");
        assert_eq!(second.action, "funnel.step");
        assert_eq!(first.session_id.as_deref(), Some("c1"));
    }

    // ── write_funnels_snapshot: per-case envelope streaming (#1247 Part 2) ──
    //
    // A killed 6-case bench must keep every COMPLETED case's envelope —
    // `run_review_bench`'s per-case loop calls `write_funnels_snapshot`
    // after every Funnel-mode case, not just at end-of-run. This exercises
    // the durability contract directly: case 1's snapshot must survive on
    // disk even when case 2 never gets a chance to write (simulating a
    // crash/timeout/error between the two cases).

    #[test]
    fn write_funnels_snapshot_survives_a_later_case_never_completing() {
        use super::super::review::Tier;
        let env1 = funnel_env(
            vec![judged_flag(Some("start.plus(30)"), Tier::Confirmed, "note1", "evidence1")],
            None,
        );

        let tmp = tempfile::TempDir::new().unwrap();
        let scores_path = tmp.path().join("scores.json");

        // Case 1 completes -> the driver loop streams its envelope.
        write_funnels_snapshot(&scores_path, std::slice::from_ref(&env1)).unwrap();
        let fpath = scores_path.with_file_name("funnels.json");
        assert!(fpath.is_file(), "case 1's snapshot must be on disk immediately");
        let after_case1: Vec<super::super::review::ReviewEnvelope> =
            serde_json::from_str(&fs::read_to_string(&fpath).unwrap()).unwrap();
        assert_eq!(after_case1.len(), 1, "only case 1 has completed so far");
        assert_eq!(after_case1[0].case_id, env1.case_id);

        // Case 2 "fails" — errors before it ever calls `write_funnels_snapshot`
        // again. The bench loop would propagate the error and stop (or, in
        // a real run, the process could be killed outright); either way, no
        // second snapshot write happens. funnels.json must still hold
        // exactly what case 1 wrote — never truncated, never corrupted.
        let after_case2_would_be_failure: Vec<super::super::review::ReviewEnvelope> =
            serde_json::from_str(&fs::read_to_string(&fpath).unwrap()).unwrap();
        assert_eq!(
            after_case2_would_be_failure.len(),
            1,
            "case 1's envelope survives a later case never completing"
        );
        assert_eq!(after_case2_would_be_failure[0].judged.len(), 1);
        assert_eq!(after_case2_would_be_failure[0].judged[0].tier, Tier::Confirmed);
    }

    #[test]
    fn write_funnels_snapshot_second_case_appends_atomically_via_temp_then_rename() {
        use super::super::review::Tier;
        let env1 = funnel_env(
            vec![judged_flag(Some("start.plus(30)"), Tier::Confirmed, "note1", "evidence1")],
            None,
        );
        let mut env2 = funnel_env(
            vec![judged_flag(Some("other.ts"), Tier::Archived, "note2", "evidence2")],
            None,
        );
        env2.case_id = "case-2".to_string();

        let tmp = tempfile::TempDir::new().unwrap();
        let scores_path = tmp.path().join("scores.json");
        write_funnels_snapshot(&scores_path, std::slice::from_ref(&env1)).unwrap();
        write_funnels_snapshot(&scores_path, &[env1, env2]).unwrap();

        let fpath = scores_path.with_file_name("funnels.json");
        // No leftover .tmp sibling after a successful rename.
        assert!(!fpath.with_extension("json.tmp").is_file(), "temp file must be renamed away, not left behind");
        let both: Vec<super::super::review::ReviewEnvelope> =
            serde_json::from_str(&fs::read_to_string(&fpath).unwrap()).unwrap();
        assert_eq!(both.len(), 2, "the second snapshot supersedes the first with BOTH cases");
        assert_eq!(both[1].case_id, "case-2");
    }

    // ── write_scores_artifact: funnels.json artifact discipline ────────
    //
    // No test in this module previously constructed a full `ReviewBenchOpts`
    // — `write_scores_artifact` (and its `debates.json`-first discipline)
    // had zero direct coverage. These tests exercise it for `funnels.json`.

    #[test]
    fn write_scores_artifact_funnel_needs_check_and_archived_survive_into_funnels_json_though_excluded_from_score_rows() {
        use super::super::review::Tier;
        let env = funnel_env(
            vec![
                judged_flag(Some("start.plus(30)"), Tier::Confirmed, "the clamp is bypassed", "evidence"),
                judged_flag(Some("other.ts"), Tier::NeedsCheck, "unsure", "ambiguous"),
                judged_flag(Some("third.ts"), Tier::Archived, "false alarm", "ruled out"),
            ],
            None,
        );
        let label = multi_lbl("bug", vec![ef("start.plus(30)", false)]);
        let r = review_from_funnel(&env);
        let s = score(&label, &r);
        // Only the Confirmed flag ever reaches the scoring surface.
        assert_eq!(s.findings, 1, "needs_check/archived never inflate the score row's finding count");

        let case = funnel_case();
        let scored: Vec<(&Case, CaseScore)> = vec![(&case, s)];
        let meta = vec![EnvelopeMeta { model: Some("darkmux:probe-model".into()), total_tokens: Some(100) }];
        let debates: Vec<super::super::dialectic::DebateEnvelope> = Vec::new();
        let funnels = vec![env];

        let tmp = tempfile::TempDir::new().unwrap();
        let scores_out = tmp.path().join("scores.json");
        let opts = funnel_opts(scores_out.clone(), true);

        let path = write_scores_artifact(&scored, &meta, &debates, &funnels, &opts, &scores_out, 0).unwrap();
        let fpath = path.with_file_name("funnels.json");
        let content = fs::read_to_string(&fpath).unwrap();
        let parsed: Vec<super::super::review::ReviewEnvelope> = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].judged.len(), 3, "all three tiers survive into the artifact");
        let tiers: Vec<Tier> = parsed[0].judged.iter().map(|j| j.tier).collect();
        assert!(tiers.contains(&Tier::NeedsCheck), "needs_check preserved even though it never became a finding");
        assert!(tiers.contains(&Tier::Archived), "archived preserved too — the full judge trail is the point");
    }

    #[test]
    fn write_scores_artifact_funnel_writes_funnels_json_even_when_scores_json_write_fails() {
        use super::super::review::Tier;
        let env = funnel_env(
            vec![judged_flag(Some("start.plus(30)"), Tier::Confirmed, "note", "evidence")],
            None,
        );
        let label = multi_lbl("bug", vec![ef("start.plus(30)", false)]);
        let r = review_from_funnel(&env);
        let s = score(&label, &r);
        let case = funnel_case();
        let scored: Vec<(&Case, CaseScore)> = vec![(&case, s)];
        let meta = vec![EnvelopeMeta::default()];
        let debates: Vec<super::super::dialectic::DebateEnvelope> = Vec::new();
        let funnels = vec![env];

        let tmp = tempfile::TempDir::new().unwrap();
        let scores_out = tmp.path().join("scores.json");
        // Force `scores::write_scores`'s atomic temp-write to fail: it
        // writes to `scores.json.tmp` before renaming into place —
        // pre-creating that exact path AS A DIRECTORY makes `std::fs::write`
        // fail with "Is a directory", without touching the funnels.json
        // sibling path at all.
        fs::create_dir_all(scores_out.with_extension("json.tmp")).unwrap();
        let opts = funnel_opts(scores_out.clone(), false);

        let err = write_scores_artifact(&scored, &meta, &debates, &funnels, &opts, &scores_out, 0).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("scores.json.tmp"), "expected the forced write failure to surface, got: {msg}");

        // funnels.json must still be on disk — written FIRST, independently
        // of the scores.json write succeeding, same discipline as
        // debates.json (#1222 packet 7's own doc comment on this function).
        let fpath = scores_out.with_file_name("funnels.json");
        assert!(fpath.is_file(), "funnels.json must survive a scores.json write failure");
        let content = fs::read_to_string(&fpath).unwrap();
        let parsed: Vec<super::super::review::ReviewEnvelope> = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].judged.len(), 1);
    }

    #[test]
    fn write_scores_artifact_funnel_extras_record_crew_and_k_and_the_envelopes_resolved_exec_mode() {
        use super::super::review::Tier;
        let mut env = funnel_env(
            vec![judged_flag(Some("start.plus(30)"), Tier::Confirmed, "note", "evidence")],
            None,
        );
        // The envelope's OWN resolved mode ("parallel") is what the extras
        // field should report — not the raw `--exec-mode` string, which may
        // be absent/"auto" when the crew resolved it dynamically.
        env.mode = "parallel".to_string();
        let label = multi_lbl("bug", vec![ef("start.plus(30)", false)]);
        let r = review_from_funnel(&env);
        let s = score(&label, &r);
        let case = funnel_case();
        let scored: Vec<(&Case, CaseScore)> = vec![(&case, s)];
        let meta = vec![EnvelopeMeta::default()];
        let debates: Vec<super::super::dialectic::DebateEnvelope> = Vec::new();
        let funnels = vec![env];

        let tmp = tempfile::TempDir::new().unwrap();
        let scores_out = tmp.path().join("scores.json");
        let mut opts = funnel_opts(scores_out.clone(), false);
        opts.roster_profile = Some("review-funnel".into());
        opts.k_override = Some(9);

        let path = write_scores_artifact(&scored, &meta, &debates, &funnels, &opts, &scores_out, 0).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(doc["crew"], serde_json::json!("review-funnel"));
        assert_eq!(
            doc["exec_mode"],
            serde_json::json!("parallel"),
            "reports the envelope's resolved mode, not the raw --exec-mode flag"
        );
        assert_eq!(doc["k"], serde_json::json!(9));
    }

    #[test]
    fn write_scores_artifact_funnel_extras_k_defaults_to_one_per_probe_role_label_when_unset() {
        use super::super::review::Tier;
        let env = funnel_env(
            vec![judged_flag(Some("start.plus(30)"), Tier::Confirmed, "note", "evidence")],
            None,
        );
        let label = multi_lbl("bug", vec![ef("start.plus(30)", false)]);
        let r = review_from_funnel(&env);
        let s = score(&label, &r);
        let case = funnel_case();
        let scored: Vec<(&Case, CaseScore)> = vec![(&case, s)];
        let meta = vec![EnvelopeMeta::default()];
        let debates: Vec<super::super::dialectic::DebateEnvelope> = Vec::new();
        let funnels = vec![env];

        let tmp = tempfile::TempDir::new().unwrap();
        let scores_out = tmp.path().join("scores.json");
        let opts = funnel_opts(scores_out.clone(), false); // k_override left None

        let path = write_scores_artifact(&scored, &meta, &debates, &funnels, &opts, &scores_out, 0).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(doc["k"], serde_json::json!("(one per probe role)"));
    }

    // (#1465) `role` is now an operator knob (was a `pr-reviewer` constant),
    // so the artifact must snapshot it — otherwise `lab eval coder` and
    // `lab eval pr-reviewer` emit indistinguishable scores.json. No-blind-runs
    // doctrine: every run self-describes its knobs.
    #[test]
    fn write_scores_artifact_extras_record_the_role_knob() {
        let label = multi_lbl("bug", vec![ef("start.plus(30)", false)]);
        let r = super::Review {
            verdict: "block".into(),
            parsed: true,
            ..Default::default()
        };
        let s = score(&label, &r);
        let case = funnel_case();
        let scored: Vec<(&Case, CaseScore)> = vec![(&case, s)];
        let meta = vec![EnvelopeMeta::default()];
        let debates: Vec<super::super::dialectic::DebateEnvelope> = Vec::new();
        let funnels: Vec<super::super::review::ReviewEnvelope> = Vec::new();

        let tmp = tempfile::TempDir::new().unwrap();
        let scores_out = tmp.path().join("scores.json");
        // A Strict-mode run of a NON-default role — the case `role` was
        // silently absent from the artifact before #1465.
        let opts = ReviewBenchOpts {
            cases_dir: PathBuf::from("."),
            role: "coder".into(),
            profile_name: Some("test-profile".into()),
            config_path: None,
            timeout_seconds: 60,
            scores_out: Some(scores_out.clone()),
            mode: BenchMode::Strict,
            workdirs: None,
            prosecutor_profile: None,
            defender_profile: None,
            judge_profile: None,
            roster_profile: None,
            exec_mode: None,
            k_override: None,
            bundler_cmd: None,
        };

        let path = write_scores_artifact(&scored, &meta, &debates, &funnels, &opts, &scores_out, 0).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(doc["role"], serde_json::json!("coder"), "the operator's role knob must ride in the artifact");
        assert_eq!(doc["mode"], serde_json::json!("strict"));
    }

    // (#1465/#1469) The experimental condition modes ignore the `role`
    // positional (they dispatch fixed pr-reviewer-variant roles). Naming a
    // role AND an experimental mode must bail LOUD before any dispatch — a
    // silent wrong-role run is the failure mode #1469 guards against.
    #[test]
    fn run_review_bench_bails_when_a_role_is_named_with_an_experimental_mode() {
        let tmp = tempfile::TempDir::new().unwrap();
        let d = tmp.path();
        fs::write(
            d.join("c.label.json"),
            r#"{"kind":"clean","intent_title":"t","expect_verdict":"pass"}"#,
        )
        .unwrap();
        fs::write(d.join("c.diff"), "diff --git a b\n").unwrap();

        let opts = ReviewBenchOpts {
            cases_dir: d.to_path_buf(),
            role: "coder".into(),
            profile_name: None,
            config_path: None,
            timeout_seconds: 30,
            scores_out: None,
            mode: BenchMode::FreeForm,
            workdirs: None,
            prosecutor_profile: None,
            defender_profile: None,
            judge_profile: None,
            roster_profile: None,
            exec_mode: None,
            k_override: None,
            bundler_cmd: None,
        };
        let err = run_review_bench(opts).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("pr-reviewer-specific"), "the bail must name why: {msg}");
        assert!(msg.contains("coder"), "the bail must echo the offending role: {msg}");
        assert!(msg.contains("freeform"), "the bail must name the mode: {msg}");
    }

    // `pr-reviewer` + any experimental mode still resolves past the guard —
    // no currently-valid invocation changed behavior (#1465). We only assert
    // the guard doesn't fire; the run itself needs live dispatch, out of scope
    // here, so we stop at the workdirs preflight (the NEXT loud failure).
    #[test]
    fn run_review_bench_default_role_passes_the_experimental_mode_guard() {
        let tmp = tempfile::TempDir::new().unwrap();
        let d = tmp.path();
        fs::write(
            d.join("c.label.json"),
            r#"{"kind":"clean","intent_title":"t","expect_verdict":"pass"}"#,
        )
        .unwrap();
        fs::write(d.join("c.diff"), "diff --git a b\n").unwrap();

        let opts = ReviewBenchOpts {
            cases_dir: d.to_path_buf(),
            role: "pr-reviewer".into(),
            profile_name: None,
            config_path: None,
            timeout_seconds: 30,
            scores_out: None,
            mode: BenchMode::Agentic,
            workdirs: None, // agentic requires this — the NEXT preflight bails here
            prosecutor_profile: None,
            defender_profile: None,
            judge_profile: None,
            roster_profile: None,
            exec_mode: None,
            k_override: None,
            bundler_cmd: None,
        };
        let err = run_review_bench(opts).unwrap_err();
        let msg = format!("{err:#}");
        // NOT the role guard — the run got PAST it to the workdirs preflight.
        assert!(!msg.contains("pr-reviewer-specific"), "default role must pass the role guard: {msg}");
        assert!(msg.contains("requires --workdirs"), "should reach the workdirs preflight: {msg}");
    }

    // ── run_funnel_case: the real pipeline, offline-testable ───────────
    //
    // `run_funnel_case`'s `chat` closure is hardcoded to the real
    // `single_shot_chat` (a live LMStudio call) — but a zero-bundle or
    // failed-bundle run short-circuits BEFORE `review::run_review` ever
    // reaches the probe phase, so both the degenerate-envelope path and the
    // `--bundler` wiring are reachable without any network dispatch.

    fn valid_funnel_crew() -> darkmux_crew::resourcing::ResolvedCrew {
        use darkmux_crew::resourcing::ResolvedSeatStaffing;
        use std::collections::BTreeMap;
        let mut seats = BTreeMap::new();
        seats.insert(
            "review-probe".to_string(),
            vec![ResolvedSeatStaffing {
                name: "fast".into(),
                role_id: None,
                pm: dummy_pm("probe-model"),
                k: 3,
                passes: 2,
                max_tokens: None,
                selector: None,
                provenance: None,
            }],
        );
        seats.insert(
            "review-judge".to_string(),
            vec![ResolvedSeatStaffing {
                name: "fast".into(),
                role_id: None,
                pm: dummy_pm("judge-model"),
                k: 1,
                passes: 2,
                max_tokens: None,
                selector: None,
                provenance: None,
            }],
        );
        darkmux_crew::resourcing::ResolvedCrew { name: "test-crew".into(), seats, request_changes: false }
    }

    fn test_funnel_ctx(bundler_cmd: Option<String>, exec_mode: super::super::review::ExecMode) -> FunnelCtx {
        FunnelCtx {
            crew: valid_funnel_crew(),
            exec_mode,
            probe_system: "probe system prompt".into(),
            judge_system: "judge system prompt".into(),
            verify_system: "verify system prompt".into(),
            bundler_cmd,
        }
    }

    #[test]
    fn run_funnel_case_zero_bundles_from_non_ts_diff_yields_a_degenerate_envelope_with_no_dispatch() {
        let case = Case {
            id: "c-docs".into(),
            label: lbl("clean", None),
            diff: "diff --git a/README.md b/README.md\n\
                   index 0000000..1111111 100644\n\
                   --- a/README.md\n\
                   +++ b/README.md\n\
                   @@ -1 +1,2 @@\n\
                    # Title\n\
                   +New line\n"
                .into(),
        };
        let workdir = tempfile::TempDir::new().unwrap();
        let ctx = test_funnel_ctx(None, super::super::review::ExecMode::Sequential);
        let (review, env) = run_funnel_case(&case, workdir.path(), &ctx, 30, &mut super::super::review::NullEmitter).unwrap();

        assert_eq!(env.bundles, 0, "README.md isn't a TS/TSX file — build_bundles finds nothing");
        assert!(env.degenerate.is_some(), "a zero-bundle run must be LOUD, never a silent pass");
        assert_eq!(
            env.mode, "sequential",
            "opts.exec_mode threads through FunnelCtx -> ReviewInputs -> the resolved envelope"
        );
        assert!(!review.parsed, "the mapped Review must not read as a real pass");

        let s = score(&case.label, &review);
        assert!(s.degenerate, "score() must classify a degenerate funnel run as degenerate, never clean-pass");
        assert!(!s.correct, "a degenerate funnel run can never score as correct");
    }

    /// (#1272) The lab/fleet sink boundary, characterized from the bench
    /// side: a bench run's `funnel.*` records go to the per-run-local
    /// `LocalJsonlEmitter` — never the fleet stream — and (the new
    /// assertion) carry no `dispatch start`/`dispatch complete`/`dispatch
    /// error` bookends. Honest scope note: the REAL enforcement is the
    /// crate graph — `mission launch review`'s `with_dispatch_bookends`
    /// wrapper lives in the binary's `mission_launch_review` module, which
    /// this crate cannot depend on, so `run_funnel_case` structurally cannot reach it and
    /// this test cannot fail from that wrapper specifically being wired
    /// in. What it DOES pin is the observable contract that the bench
    /// emitter's stream stays pure funnel vocabulary: a future bookend
    /// emission added anywhere on the bench path IN THIS CRATE would be
    /// caught before a corpus run started spamming synthetic "running
    /// dispatch" entries into the fleet-liveness surfaces.
    #[test]
    fn run_funnel_case_through_local_jsonl_emitter_emits_no_dispatch_bookends() {
        let case = Case {
            id: "c-docs-2".into(),
            label: lbl("clean", None),
            diff: "diff --git a/README.md b/README.md\n\
                   index 0000000..1111111 100644\n\
                   --- a/README.md\n\
                   +++ b/README.md\n\
                   @@ -1 +1,2 @@\n\
                    # Title\n\
                   +Another line\n"
                .into(),
        };
        let workdir = tempfile::TempDir::new().unwrap();
        let ctx = test_funnel_ctx(None, super::super::review::ExecMode::Sequential);
        let tmp = tempfile::TempDir::new().unwrap();
        let events_path = tmp.path().join("funnel-events.jsonl");
        let mut emitter = LocalJsonlEmitter::new(events_path.clone());

        let (_, env) = run_funnel_case(&case, workdir.path(), &ctx, 30, &mut emitter).unwrap();
        assert!(env.degenerate.is_some(), "zero-bundle case still emits a funnel.task terminal record");

        let content = fs::read_to_string(&events_path).expect("the degenerate run still emits funnel.task");
        let actions: Vec<String> = content
            .lines()
            .filter_map(|l| serde_json::from_str::<darkmux_flow::FlowRecord>(l).ok())
            .map(|r| r.action)
            .collect();
        assert!(!actions.is_empty(), "the bench path must still emit its own funnel.* records");
        assert!(
            actions.iter().all(|a| !a.starts_with("dispatch")),
            "the bench-path LocalJsonlEmitter must never see a dispatch.*/\"dispatch \" bookend \
             (lab-vs-fleet sink boundary): got {actions:?}"
        );
    }

    #[cfg(unix)]
    fn write_stub_bundler_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        f.write_all(body.as_bytes()).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn run_funnel_case_external_bundler_flows_through_bundle_external_bundles_and_names_the_case_on_failure() {
        // Mirrors `bundle::external::tests::write_stub_script` — an external
        // `--bundler` that produces an EMPTY bundle set is rejected loudly
        // by `bundle::external_bundles`'s own contract check. This proves
        // `run_funnel_case` actually reaches that real function (not a stub)
        // when `ctx.bundler_cmd` is set, and wraps the failure with the case
        // id — all offline, since the bundling failure short-circuits before
        // the crew/chat machinery is ever touched.
        let tmp = tempfile::TempDir::new().unwrap();
        let script = write_stub_bundler_script(tmp.path(), "empty-bundler.sh", "echo '{\"bundles\":[]}'\n");
        let case = Case {
            id: "c-ext".into(),
            label: lbl("clean", None),
            diff: "diff --git a/x.ts b/x.ts\n--- a/x.ts\n+++ b/x.ts\n@@ -1 +1,2 @@\n foo\n+bar\n".into(),
        };
        let workdir = tempfile::TempDir::new().unwrap();
        let ctx = test_funnel_ctx(
            Some(script.to_str().unwrap().to_string()),
            super::super::review::ExecMode::Sequential,
        );
        let err = run_funnel_case(&case, workdir.path(), &ctx, 30, &mut super::super::review::NullEmitter).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("external bundler for case c-ext"), "got: {msg}");
        assert!(msg.contains("empty bundle set"), "the underlying named error must survive the wrap: {msg}");
    }

    // ── resolve_funnel_ctx: roster resolution + --k / --exec-mode plumbing ───
    // (#1475) The funnel pins EVERY review seat to one profile (the
    // `--roster-profile`/`--profile` name, else default_profile) through packet
    // 3's per-run role→profile override — one canonical resolver shared with the
    // operator path. `--roster-profile` (#1465, renamed from `--crew`) names
    // that profile.

    fn write_test_registry(dir: &Path, roster: &str) -> PathBuf {
        use std::collections::BTreeMap;
        let mut profiles = BTreeMap::new();
        profiles.insert(
            roster.to_string(),
            darkmux_types::Profile {
                models: vec![dummy_pm("probe-model"), dummy_pm("judge-model")],
                ..Default::default()
            },
        );
        let registry = darkmux_types::ProfileRegistry {
            profiles,
            default_profile: Some(roster.to_string()),
            ..Default::default()
        };
        let path = dir.join("profiles.json");
        fs::write(&path, serde_json::to_string_pretty(&registry).unwrap()).unwrap();
        path
    }

    fn funnel_ctx_opts(config_path: PathBuf, roster: &str, exec_mode: Option<&str>, k_override: Option<u32>) -> ReviewBenchOpts {
        ReviewBenchOpts {
            cases_dir: PathBuf::from("."),
            role: "pr-reviewer".into(),
            profile_name: None,
            config_path: Some(config_path.to_str().unwrap().to_string()),
            timeout_seconds: 30,
            scores_out: None,
            mode: BenchMode::Funnel,
            workdirs: None,
            prosecutor_profile: None,
            defender_profile: None,
            judge_profile: None,
            roster_profile: Some(roster.to_string()),
            exec_mode: exec_mode.map(str::to_string),
            k_override,
            bundler_cmd: None,
        }
    }

    #[test]
    fn resolve_funnel_ctx_missing_roster_profile_names_available() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_test_registry(tmp.path(), "fast");
        let opts = funnel_ctx_opts(path, "ghost", None, None);

        // `Result::unwrap_err` requires `T: Debug`; `FunnelCtx` intentionally
        // isn't (it holds a `ResolvedCrew`, not a debug/display type) — `.err().unwrap()`
        // extracts the error without that bound.
        let err = resolve_funnel_ctx(&opts).err().unwrap();
        let msg = format!("{err:#}");
        // (#1475) The bench pins every seat to the roster via the per-run
        // override, so a bad roster surfaces the resolver's loud override error.
        assert!(msg.contains("ghost"), "names the missing roster: {msg}");
        assert!(msg.contains("fast"), "lists the available profile: {msg}");
    }

    #[test]
    fn resolve_funnel_ctx_k_override_applies_to_probe_and_exec_mode_parses() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_test_registry(tmp.path(), "fast");
        let opts = funnel_ctx_opts(path, "fast", Some("parallel"), Some(9));

        let ctx = resolve_funnel_ctx(&opts).unwrap();
        let probes = ctx.crew.seats.get("review-probe").unwrap();
        assert!(probes.iter().all(|s| s.k == 9), "the probe seat's draw count is the k override");
        let judges = ctx.crew.seats.get("review-judge").unwrap();
        assert_eq!(judges[0].k, 1, "the judge seat draws once regardless of the probe k override");
        assert_eq!(
            ctx.exec_mode,
            super::super::review::ExecMode::Parallel,
            "--exec-mode threads through into FunnelCtx"
        );
        assert!(!ctx.probe_system.is_empty(), "review-probe.md resolves via the embedded role loader");
        assert!(!ctx.judge_system.is_empty(), "review-judge.md resolves via the embedded role loader");
    }

    #[test]
    fn resolve_funnel_ctx_no_k_override_uses_default_probe_k_and_exec_mode_defaults_to_auto() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_test_registry(tmp.path(), "fast");
        let opts = funnel_ctx_opts(path, "fast", None, None);

        let ctx = resolve_funnel_ctx(&opts).unwrap();
        let probes = ctx.crew.seats.get("review-probe").unwrap();
        // (#1475) The flip staffs three distinct probe roles, one draw each —
        // the same total probe breadth (3) the old default `k=3` gave from one
        // seat, now role-borne rather than draw-borne.
        assert_eq!(probes.len(), 3, "three distinct probe roles staff by default");
        assert!(probes.iter().all(|s| s.k == 1), "no override ⇒ one draw per probe role");
        assert_eq!(
            ctx.exec_mode,
            super::super::review::ExecMode::Auto,
            "no --exec-mode ⇒ Auto, resolved later against local hardware"
        );
    }
