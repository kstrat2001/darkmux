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
            partial: false,
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
        let s = score(&lbl("clean", None), &Review { verdict: "pass".into(), findings: vec![], parsed: true, partial: false });
        assert_eq!(s.fp, 0);
        assert!(s.correct);
    }

    #[test]
    fn score_clean_with_finding_is_false_positive() {
        let r = Review { verdict: "pass".into(), findings: vec![Finding { severity: "medium".into(), ..Default::default() }], parsed: true, partial: false };
        let s = score(&lbl("clean", None), &r);
        assert_eq!(s.fp, 1);
        assert!(!s.correct); // a finding on a clean diff is wrong even if verdict=pass
    }

    #[test]
    fn score_bug_recall_via_anchor() {
        let r = Review { verdict: "flag".into(), findings: vec![Finding { severity: "high".into(), anchor: "let sql = format!(\"SELECT ...\", name)".into(), title: "SQLi".into() }], parsed: true, partial: false };
        let s = score(&lbl("bug", Some("format!(\"SELECT")), &r);
        assert!(s.recall);
        assert!(s.anchor_ok);
        assert!(s.correct);
    }

    #[test]
    fn score_bug_recall_via_high_severity_without_anchor_match() {
        let r = Review { verdict: "flag".into(), findings: vec![Finding { severity: "high".into(), anchor: "wrong line".into(), title: "SQLi".into() }], parsed: true, partial: false };
        let s = score(&lbl("bug", Some("format!")), &r);
        assert!(s.recall); // high severity counts as caught
        assert!(!s.anchor_ok); // but the anchor missed
    }

    #[test]
    fn score_bug_empty_flag_is_contract_violation_not_recall() {
        // verdict=flag with zero findings (the gpt-oss failure mode).
        let r = Review { verdict: "flag".into(), findings: vec![], parsed: true, partial: false };
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
            partial: false,
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

    // ── parse_exec_mode ─────────────────────────────────────────────

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

    fn funnel_case() -> Case {
        Case {
            id: "c1".into(),
            label: multi_lbl("bug", vec![ef("start.plus(30)", false)]),
            diff: "+ const end = start.plus(30)".into(),
        }
    }

    // ── score() on a no-anchor Confirmed flag ("does score() treat it right?") ──

    // ── LocalJsonlEmitter: file mechanics (#1247 review round) ──────────

    //
    // A killed 6-case bench must keep every COMPLETED case's envelope —
    // `run_review_bench`'s per-case loop calls `write_funnels_snapshot`
    // after every Funnel-mode case, not just at end-of-run. This exercises
    // the durability contract directly: case 1's snapshot must survive on
    // disk even when case 2 never gets a chance to write (simulating a
    // crash/timeout/error between the two cases).

    // ── write_scores_artifact ──────────────────────────────────────────
    //
    // No test in this module previously constructed a full `ReviewBenchOpts`
    // — `write_scores_artifact` (and its `debates.json`-first discipline)
    // had zero direct coverage. These tests exercise it for `funnels.json`.

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

        let path = write_scores_artifact(&scored, &meta, &debates, &opts, &scores_out, 0).unwrap();
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

    // ── resolve_funnel_ctx: roster resolution + --k / --exec-mode plumbing ───
    // (#1475) The funnel pins EVERY review seat to one profile (the
    // `--roster-profile`/`--profile` name, else default_profile) through packet
    // 3's per-run role→profile override — one canonical resolver shared with the
    // operator path. `--roster-profile` (#1465, renamed from `--crew`) names
    // that profile.

    // Every test below resolves `mission_config::load("review")`, which reads
    // the process-global DARKMUX_CREW_DIR. `#[serial_test::serial]` only
    // serializes against OTHER serial tests, so the gate test above (which
    // points that var at a tempdir holding a deliberately phase-less review
    // override) would otherwise race these and fail them with a dangling
    // `adjudicate` phase id. Config-resolving tests are all serial as a set.
    /// A bench run must write where the lab READER scans, and in a test build
    /// that is the isolated tmp root — never the operator's real
    /// `~/.darkmux/runs`.
    ///
    /// Both halves of this were live defects. `run_review_bench` resolved its
    /// default artifact path through `paths::resolve(Auto).runs` directly
    /// instead of `config_access::lab_dir()`, so:
    ///
    ///   1. every test that reached this path wrote real run directories into
    ///      the operator's `~/.darkmux/runs` (observed 2026-08-23: two
    ///      `review-bench-<ts>` dirs created by `cargo test`, which then
    ///      rendered as live RUNNING rows in the viewer's runs lens), and
    ///   2. it silently ignored `DARKMUX_LAB_DIR` / `config.dirs.lab`, so an
    ///      operator who configured a lab dir would have bench runs WRITTEN to
    ///      one place and SCANNED for in another — precisely the read/write
    ///      divergence `lab_dir_default`'s own docstring warns about.
    // `#[serial]` because `lab_dir()` reads `current_dir()` in test builds now,
    // and this crate's cwd-mutating tests are all serial. Measured before
    // adding it: 1 disagreement in 346,151 sampled call pairs — rare, real, and
    // one attribute to close.
    #[serial_test::serial]
    #[test]
    fn default_scores_path_resolves_through_the_lab_reader_not_the_real_home() {
        let path = super::default_scores_path(1_787_476_055_606);

        let reader_root = darkmux_types::config_access::lab_dir();
        assert!(
            path.starts_with(&reader_root),
            "bench artifacts must land under the same root the lab reader scans\n  \
             wrote:  {}\n  reader: {}",
            path.display(),
            reader_root.display(),
        );

        if let Some(home) = dirs::home_dir() {
            let real = home.join(".darkmux").join("runs");
            assert!(
                !path.starts_with(&real),
                "a test build wrote into the operator's real run store: {}",
                path.display(),
            );
        }
    }
