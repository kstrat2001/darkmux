    use super::*;
    // `NodeStatus` is used only by this test module as of #1284 Packet 3
    // (`build_review_graph` stopped constructing `Step` literals directly
    // once it became a thin `mission_config::interpret` launcher).
    use darkmux_crew::scheduler::STEP_LIFECYCLE_ACTIONS;
    use darkmux_crew::types::NodeStatus;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    // ── fixtures ────────────────────────────────────────────────────

    const DIFF: &str = "--- a/billing.ts\n+++ b/billing.ts\n@@ -1,3 +1,4 @@\n context line\n+const end = start.plus(30)\n+const total = base * rate\n more context\n";

    fn pm(id: &str) -> ProfileModel {
        ProfileModel { id: id.to_string(), n_ctx: Some(32_000), ..Default::default() }
    }

    fn staffing(profile: &str, model: &str, k: u32) -> ResolvedSeatStaffing {
        ResolvedSeatStaffing {
            name: profile.to_string(),
            pm: pm(model),
            k,
            // Default double-confirm — a test needing a different judge depth
            // sets `.passes` on the returned staffing (#1266).
            passes: 2,
            max_tokens: None,
            selector: None,
        }
    }

    fn crew_with(seats: Vec<(&str, Vec<ResolvedSeatStaffing>)>) -> ResolvedCrew {
        let mut m = BTreeMap::new();
        for (k, v) in seats {
            m.insert(k.to_string(), v);
        }
        ResolvedCrew { name: "test-crew".to_string(), seats: m, request_changes: false }
    }

    fn valid_crew() -> ResolvedCrew {
        crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 2)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ])
    }

    fn flag(bundle_id: &str, member: &str, draw: u32, charge_text: &str) -> ProbeFlag {
        ProbeFlag {
            bundle_id: bundle_id.to_string(),
            fact_family: "unscoped".to_string(),
            member: member.to_string(),
            draw,
            charge_text: charge_text.to_string(),
            anchor: None,
            also_flagged: Vec::new(),
        }
    }

    /// Recording [`ModelCycler`] mock: pushes `"load:<id>"` / `"release:<id>"`
    /// into a shared log so cycling ORDER is assertable.
    struct RecordingCycler {
        log: Vec<String>,
    }
    impl RecordingCycler {
        fn new() -> Self {
            Self { log: Vec::new() }
        }
    }
    impl ModelCycler for RecordingCycler {
        fn ensure_loaded(&mut self, pm: &ProfileModel) -> Result<()> {
            self.log.push(format!("load:{}", pm.id));
            Ok(())
        }
        fn release(&mut self, pm: &ProfileModel) -> Result<()> {
            self.log.push(format!("release:{}", pm.id));
            Ok(())
        }
    }

    fn reply(content: &str) -> SingleShotReply {
        SingleShotReply {
            content: content.to_string(),
            total_tokens: Some(10),
            prompt_tokens: None,
            completion_tokens: None,
            model: None,
        }
    }

    // ── review_token_telemetry_payload (#1361) ───────────────────────

    #[test]
    fn review_token_telemetry_payload_uses_prompt_and_completion_when_present() {
        let r = SingleShotReply {
            content: String::new(),
            total_tokens: Some(42),
            prompt_tokens: Some(30),
            completion_tokens: Some(12),
            model: None,
        };
        let payload = review_token_telemetry_payload(&r).expect("total_tokens present");
        assert_eq!(payload["prompt_tokens"], 30);
        assert_eq!(payload["completion_tokens"], 12);
        assert_eq!(payload["total_tokens"], 42);
    }

    #[test]
    fn review_token_telemetry_payload_defaults_missing_split_from_total() {
        // Real LMStudio/hosted responses always send prompt_tokens +
        // completion_tokens alongside total_tokens, but the fallback must
        // still produce an honest payload if a backend ever omits the split.
        let r = SingleShotReply {
            content: String::new(),
            total_tokens: Some(50),
            prompt_tokens: None,
            completion_tokens: None,
            model: None,
        };
        let payload = review_token_telemetry_payload(&r).expect("total_tokens present");
        assert_eq!(payload["prompt_tokens"], 0);
        assert_eq!(payload["completion_tokens"], 50);
        assert_eq!(payload["total_tokens"], 50);
    }

    #[test]
    fn review_token_telemetry_payload_none_when_no_total_tokens() {
        let r = SingleShotReply {
            content: String::new(),
            total_tokens: None,
            prompt_tokens: None,
            completion_tokens: None,
            model: None,
        };
        assert!(review_token_telemetry_payload(&r).is_none());
    }

    // ── judge ruling parser ──────────────────────────────────────────

    #[test]
    fn parse_judge_ruling_last_fence_wins() {
        let text = "Weighing the flag: the code quotes\n```\nconst days = Math.min(raw, 30)\n```\nwhich looks relevant.\n\n```json\n{\"ruling\": \"confirmed\", \"decisive_evidence\": \"the clamp is bypassed\", \"note_for_author\": \"real bug\"}\n```\n";
        let (ruling, evidence, note) = parse_judge_ruling(text).expect("parses");
        assert_eq!(ruling, JudgeRuling::Confirmed);
        assert_eq!(evidence, "the clamp is bypassed");
        assert_eq!(note, "real bug");
    }

    #[test]
    fn parse_judge_ruling_prose_wrapped_still_parses() {
        let text = "Some long reasoning about the code goes here, spanning several\nsentences before the verdict.\n```json\n{\"ruling\": \"false_positive\", \"decisive_evidence\": \"input is clamped upstream\", \"note_for_author\": \"no action needed\"}\n```";
        let (ruling, ..) = parse_judge_ruling(text).expect("parses");
        assert_eq!(ruling, JudgeRuling::FalsePositive);
    }

    #[test]
    fn parse_judge_ruling_needs_check_and_case_insensitive() {
        let text = "```json\n{\"ruling\": \"NEEDS_CHECK\", \"decisive_evidence\": \"outside the bundle\", \"note_for_author\": \"verify manually\"}\n```";
        let (ruling, ..) = parse_judge_ruling(text).expect("parses");
        assert_eq!(ruling, JudgeRuling::NeedsCheck);
    }

    #[test]
    fn parse_judge_ruling_unparsed_on_garbage() {
        assert!(parse_judge_ruling("I could not determine a verdict.").is_none());
        assert!(parse_judge_ruling("").is_none());
        // Off-contract ruling value never matches — falls through to None.
        assert!(parse_judge_ruling("```json\n{\"ruling\": \"maybe\"}\n```").is_none());
    }

    // ── dedup ─────────────────────────────────────────────────────────

    #[test]
    fn dedup_same_anchor_and_family_collapses_across_members_and_draws() {
        let flags = vec![
            flag("b1", "member-a", 0, "The clamp at `const end = start.plus(30)` double counts."),
            flag("b1", "member-b", 1, "`const end = start.plus(30)` double-counts the boundary day."),
            flag("b1", "member-a", 2, "`const end = start.plus(30)` looks off by one."),
        ];
        let (deduped, stats) = dedup_flags(flags, DIFF);
        assert_eq!(stats.raw, 3);
        assert_eq!(stats.deduped, 1);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].anchor.as_deref(), Some("const end = start.plus(30)"));
    }

    #[test]
    fn dedup_different_mechanism_family_survives() {
        let flags = vec![
            flag("b1", "member-a", 0, "`const end = start.plus(30)` double counts the boundary."),
            flag("b1", "member-b", 0, "`const end = start.plus(30)` — timezone handling is wrong here."),
        ];
        let (deduped, stats) = dedup_flags(flags, DIFF);
        assert_eq!(stats.deduped, 2, "different mechanism family must survive dedup");
        assert_eq!(deduped.len(), 2);
    }

    /// (#1299 recall guard) Two unanchored flags — no resolvable location —
    /// must NOT collapse even in the same family, because the dedup
    /// predicate requires a shared LOCATION and a shared SYMBOL, and neither
    /// is present. Under the asymmetric objective ("a leaked duplicate beats
    /// a false cut") a missing location keeps findings separate. This
    /// replaces the pre-#1299 family-only collapse, which was the over-cut
    /// path the location/symbol rules close.
    #[test]
    fn dedup_no_location_no_symbol_flags_do_not_collapse() {
        let flags = vec![
            flag("b1", "member-a", 0, "This is a null pointer risk on the branch."),
            flag("b1", "member-b", 0, "A null value can reach this path unchecked."),
        ];
        let (deduped, stats) = dedup_flags(flags, DIFF);
        assert_eq!(
            stats.deduped, 2,
            "no anchor + no symbol → no location/symbol overlap → both survive (recall-safe)"
        );
        assert!(deduped[0].anchor.is_none());
        assert!(deduped[1].anchor.is_none());
    }

    #[test]
    fn dedup_no_anchor_different_bundle_survives() {
        let flags = vec![
            flag("b1", "member-a", 0, "This is a null pointer risk."),
            flag("b2", "member-a", 0, "This is also a null pointer risk."),
        ];
        let (_deduped, stats) = dedup_flags(flags, DIFF);
        assert_eq!(stats.deduped, 2, "different bundle_id never collapses");
    }

    /// Frontier QA should-fix on this packet's PR: substring matching
    /// classified "tenant", "covenant", and "finance" as `null/bounds` (all
    /// contain "nan"), so two DISTINCT unanchored charges on a billing
    /// corpus keyed identically and one real defect was silently dropped
    /// in dedup. Word-boundary matching must not fire on those words.
    #[test]
    fn mechanism_family_does_not_substring_match_inside_words() {
        assert_eq!(
            mechanism_family("The tenant covenant check is skipped for finance accounts."),
            "other",
            "'tenant'/'covenant'/'finance' must not classify as null/bounds"
        );
        // The real keywords still classify as whole tokens.
        assert_eq!(mechanism_family("A null value reaches this branch."), "null/bounds");
        assert_eq!(mechanism_family("NaN propagates into the total."), "null/bounds");
        assert_eq!(mechanism_family("None is returned on the error path."), "null/bounds");
        // Punctuation-adjacent tokens still match (tokenizer strips it).
        assert_eq!(mechanism_family("Uses `Date.now()` for the cutoff."), "timezone/ambient-time");
        // "nonexistent" must not token-match "none".
        assert_eq!(mechanism_family("References a nonexistent column."), "other");
    }

    /// Two unanchored flags on the SAME bundle whose charges describe
    /// genuinely different mechanisms must both survive dedup — the
    /// substring bug collapsed them (both misclassified `null/bounds`) and
    /// silently dropped a real defect.
    #[test]
    fn dedup_distinct_mechanisms_same_bundle_both_survive() {
        let flags = vec![
            flag(
                "b1",
                "member-a",
                0,
                "The tenant covenant check is skipped when the finance flag is set.",
            ),
            flag("b1", "member-b", 0, "A null value reaches the accumulator unguarded."),
        ];
        let (deduped, stats) = dedup_flags(flags, DIFF);
        assert_eq!(
            stats.deduped, 2,
            "genuinely different mechanisms in one bundle must both survive"
        );
        assert_eq!(deduped.len(), 2);
    }

    // ── #1299: symbol extraction + the #396 production case ───────────

    #[test]
    fn referenced_symbols_extracts_code_identifiers_not_prose() {
        // camelCase, PascalCase, snake_case, and call sites are symbols;
        // plain English words (even in backticks) are NOT.
        let s = referenced_symbols(
            "The `docFileEntry` from FinancialStatement uses doc_file_entry and calls record(x).",
        );
        assert!(s.contains("docfileentry"), "camelCase is a symbol");
        assert!(s.contains("financialstatement"), "PascalCase is a symbol");
        assert!(s.contains("doc_file_entry"), "snake_case is a symbol");
        assert!(s.contains("record"), "a call site `record(` is a symbol");
        // Plain lowercase prose words are excluded — no false symbols that
        // could over-collapse two unrelated bugs.
        assert!(!s.contains("the"));
        assert!(!s.contains("from"));
        assert!(!s.contains("uses"));
        assert!(!s.contains("calls"));
        // A bare lowercase word not followed by `(` is not a symbol.
        assert!(referenced_symbols("the value is dropped").is_empty());
    }

    // The #396 diff — the new-side lines every golden charge quotes so its
    // anchor resolves to a real site.
    const DIFF_396: &str = "--- a/src/domain/extraction/financialStatementSpec.ts\n+++ b/src/domain/extraction/financialStatementSpec.ts\n@@ -10,2 +10,3 @@\n ctx\n+  if (isInThousands) recordDerived(value * 1000)\n--- a/src/services/ihsService.ts\n+++ b/src/services/ihsService.ts\n@@ -20,2 +20,10 @@\n ctx\n+  const docFileEntry = bankStatements[idx]\n+  const docFileEntry = invoices[idx]\n+  const docFileEntry = epfFiles[idx]\n+  const docFileEntry = payslips[idx]\n+  const docFileEntry = financialStatements[idx]\n+  writeDocumentInstance(docFileEntry)\n+  provenance.incorporatedDate = record.date\n";

    const SPEC_FILE: &str = "src/domain/extraction/financialStatementSpec.ts";
    const IHS_FILE: &str = "src/services/ihsService.ts";

    /// The 9 "confirmed" #396 findings — 3 distinct bugs stated many ways.
    fn flags_396() -> Vec<ProbeFlag> {
        vec![
            // Bug A — isInThousands drops the provenance source field. Three
            // restatements, all quoting the SAME recordDerived site.
            flag(SPEC_FILE, "gpt-4o", 0, "`recordDerived(value * 1000)` in the isInThousands branch drops the provenance source field."),
            flag(SPEC_FILE, "gpt-4o", 1, "`recordDerived(value * 1000)` is called unconditionally, losing the source mapping — a provenance defect."),
            flag(SPEC_FILE, "gpt-4o", 2, "`recordDerived(value * 1000)` records the derived value but omits the provenance source field."),
            // Bug B — docFileEntry undefined / out-of-bounds before
            // writeDocumentInstance. Five branches, five DISTINCT sites.
            flag(IHS_FILE, "gpt-4o", 0, "`docFileEntry = bankStatements[idx]` can be undefined before writeDocumentInstance — out of bounds on an empty array."),
            flag(IHS_FILE, "gpt-4o", 1, "`docFileEntry = invoices[idx]` may be undefined; the index can exceed the array length."),
            flag(IHS_FILE, "gpt-4o", 2, "`docFileEntry = epfFiles[idx]` is out of bounds when epfFiles is empty; undefined reaches writeDocumentInstance."),
            flag(IHS_FILE, "gpt-4o", 3, "`docFileEntry = payslips[idx]` — index-based selection can return undefined for the payslips branch."),
            flag(IHS_FILE, "gpt-4o", 4, "`docFileEntry = financialStatements[idx]` can be undefined / out of bounds in the financialStatements branch before writeDocumentInstance."),
            // Bug C — incorporatedDate recorded under the wrong field name.
            // Same FILE as B, but a DIFFERENT bug (provenance, not bounds).
            flag(IHS_FILE, "gpt-4o", 5, "`incorporatedDate` is recorded under the wrong field name, and there is no write-gate."),
        ]
    }

    /// The #396 golden case. Recall guards are HARD asserts; the exact
    /// collapse count is NOT pinned (the asymmetric objective — "a leaked
    /// duplicate beats a false cut"), only bounded to a range.
    #[test]
    fn dedup_396_collapses_duplicates_but_keeps_the_three_bugs_separate() {
        let (deduped, stats) = dedup_flags(flags_396(), DIFF_396);
        assert_eq!(stats.raw, 9);

        // HARD — Bug A's three same-site restatements collapse to ONE.
        let a: Vec<&ProbeFlag> = deduped.iter().filter(|f| f.bundle_id == SPEC_FILE).collect();
        assert_eq!(a.len(), 1, "Bug A (isInThousands provenance) collapses to one finding");
        assert_eq!(mechanism_family(&a[0].charge_text), "provenance/sibling");

        // HARD — every docFileEntry SITE survives (five distinct branches):
        // same symbol at different locations is NOT collapsed (recall).
        let b: Vec<&ProbeFlag> = deduped
            .iter()
            .filter(|f| {
                f.bundle_id == IHS_FILE && referenced_symbols(&f.charge_text).contains("docfileentry")
            })
            .collect();
        assert_eq!(b.len(), 5, "every docFileEntry branch keeps its own finding — no site hidden");
        let sites: std::collections::BTreeSet<Option<String>> =
            b.iter().map(|f| f.anchor.clone()).collect();
        assert_eq!(sites.len(), 5, "the five docFileEntry findings anchor to five distinct sites");
        assert!(
            b.iter().all(|f| mechanism_family(&f.charge_text) == "null/bounds"),
            "Bug B is the null-safety/bounds family"
        );

        // HARD (the recall guard) — Bug C is PRESENT, exactly once, and is
        // NOT merged into Bug B: different family AND different symbol, same
        // file notwithstanding.
        let c: Vec<&ProbeFlag> = deduped
            .iter()
            .filter(|f| referenced_symbols(&f.charge_text).contains("incorporateddate"))
            .collect();
        assert_eq!(c.len(), 1, "Bug C (incorporatedDate provenance) is present, exactly once");
        assert!(
            !referenced_symbols(&c[0].charge_text).contains("docfileentry"),
            "Bug C must not carry Bug B's symbol"
        );
        assert_eq!(
            mechanism_family(&c[0].charge_text),
            "provenance/sibling",
            "Bug C is provenance/field-name, a DIFFERENT family than Bug B (null/bounds)"
        );

        // SOFT — some collapse happened (A's three → one) and no over-merge:
        // a range, never a pinned count. 9 raw → 7 here (A collapses, B's
        // five distinct sites and C survive); anything in-range is a PASS.
        assert!(
            (3..=7).contains(&deduped.len()),
            "recall-safe collapse expected in 3..=7, got {}",
            deduped.len()
        );
    }

    /// Recall/negative guard: two GENUINELY DIFFERENT bugs in the same file
    /// and the same mechanism-family, but naming different symbols at
    /// different sites, must both survive — never over-collapsed.
    #[test]
    fn dedup_recall_same_file_family_different_symbol_stay_separate() {
        let diff = "--- a/svc.ts\n+++ b/svc.ts\n@@ -1,2 +1,3 @@\n ctx\n+  const a = parseAmount(row)\n+  const b = docFileEntry[idx]\n";
        let flags = vec![
            flag("svc.ts", "m", 0, "`parseAmount(row)` can return undefined for an empty row."),
            flag("svc.ts", "m", 1, "`docFileEntry[idx]` may be undefined / out of bounds."),
        ];
        let (deduped, stats) = dedup_flags(flags, diff);
        assert_eq!(
            stats.deduped, 2,
            "same file + same null/bounds family but different symbols → two distinct bugs, never merged"
        );
        assert_eq!(deduped.len(), 2);
    }

    /// Same symbol, same family, same file — but at DIFFERENT sites (the
    /// #396 docFileEntry shape). Location divergence keeps them separate:
    /// different sites can be different bugs.
    #[test]
    fn dedup_same_symbol_different_location_stays_separate() {
        let diff = "--- a/svc.ts\n+++ b/svc.ts\n@@ -1,2 +1,3 @@\n ctx\n+  const docFileEntry = a[idx]\n+  const docFileEntry = b[idx]\n";
        let flags = vec![
            flag("svc.ts", "m", 0, "`docFileEntry = a[idx]` can be undefined / out of bounds."),
            flag("svc.ts", "m", 1, "`docFileEntry = b[idx]` can be undefined / out of bounds."),
        ];
        let (_deduped, stats) = dedup_flags(flags, diff);
        assert_eq!(stats.deduped, 2, "same symbol at two different sites stays as two findings");
    }

    /// No resolvable location (the #396 frontier reality — 0/9 anchored)
    /// means NO collapse, even for obvious same-symbol restatements. The
    /// honest outcome is "more duplicates," never an over-merge.
    #[test]
    fn dedup_no_location_never_collapses_even_same_symbol() {
        // A diff that shares NO line with the charges → anchors stay None.
        let diff = "--- a/svc.ts\n+++ b/svc.ts\n@@ -1,1 +1,1 @@\n+ unrelated\n";
        let flags = vec![
            flag("svc.ts", "m", 0, "`docFileEntry` may be undefined here."),
            flag("svc.ts", "m", 1, "`docFileEntry` may be undefined here."),
        ];
        let (deduped, stats) = dedup_flags(flags, diff);
        assert!(deduped.iter().all(|f| f.anchor.is_none()), "no anchor resolved");
        assert_eq!(stats.deduped, 2, "no location → no collapse (recall-safe)");
    }

    /// (#1299 MUST_FIX 2) The adversarial shape the first golden test
    /// MISSED: a provenance / wrong-source bug and a bounds bug share a
    /// line, a symbol, AND an anchor, and the provenance bug's prose even
    /// mentions "array"/"index". It must NOT collapse into the bounds bug —
    /// bare generic tokens no longer classify `null/bounds`, and the
    /// specific `provenance/sibling` family is table-ordered first, so the
    /// two land in different families and stay separate.
    #[test]
    fn dedup_provenance_worded_with_index_does_not_merge_into_bounds() {
        let diff = "--- a/svc.ts\n+++ b/svc.ts\n@@ -1,2 +1,2 @@\n ctx\n+  const docFileEntry = sources[idx]\n";
        let flags = vec![
            flag("svc.ts", "m", 0, "`docFileEntry = sources[idx]` can be undefined / out of bounds when sources is empty."),
            flag("svc.ts", "m", 1, "`docFileEntry = sources[idx]` reads the wrong source at this array index — a provenance mismatch, not a bounds error."),
        ];
        // Same file + same symbol + same anchor, but DIFFERENT families.
        assert_eq!(mechanism_family(&flags[0].charge_text), "null/bounds");
        assert_eq!(mechanism_family(&flags[1].charge_text), "provenance/sibling");
        let (deduped, stats) = dedup_flags(flags, diff);
        assert_eq!(
            stats.deduped, 2,
            "a provenance bug worded with index/array must not merge into a co-located bounds bug"
        );
        assert!(
            deduped.iter().all(|f| f.also_flagged.is_empty()),
            "no false collapse → nothing absorbed"
        );
    }

    /// (#1299 MUST_FIX 2) Bare generic tokens (`index`/`array`/`bounds`) no
    /// longer classify `null/bounds` — they co-occur across unrelated defect
    /// classes. Only anchored phrases do; a provenance finding that also
    /// mentions index/array lands in provenance.
    #[test]
    fn mechanism_family_bare_index_array_bounds_are_not_null_bounds() {
        assert_eq!(mechanism_family("the loop reads the index into the array"), "other");
        assert_eq!(mechanism_family("a bounds concern on this record"), "other");
        assert_eq!(mechanism_family("this is out of bounds on an empty list"), "null/bounds");
        assert_eq!(mechanism_family("the value can be undefined here"), "null/bounds");
        assert_eq!(
            mechanism_family("reads the wrong source at this array index"),
            "provenance/sibling"
        );
    }

    /// (#1299 MUST_FIX 1) Collapse AGGREGATES, never discards: when Bug A's
    /// three same-site restatements collapse, the survivor retains its own
    /// framing AND carries the two absorbed ones in `also_flagged`, so a
    /// rendered finding can show every framing — a residual false cut can
    /// never vanish a defect's description.
    #[test]
    fn dedup_collapse_retains_absorbed_charge_texts() {
        let (deduped, _stats) = dedup_flags(flags_396(), DIFF_396);
        let a = deduped
            .iter()
            .find(|f| f.bundle_id == SPEC_FILE)
            .expect("Bug A survivor present");
        assert_eq!(
            a.also_flagged.len(),
            2,
            "the two absorbed Bug A restatements are retained, not dropped"
        );
        // The retained framings are the OTHER two, distinct from the survivor's own.
        assert!(a.also_flagged.iter().all(|t| *t != a.charge_text));
    }

    #[test]
    fn dedup_396_is_deterministic() {
        let (d1, s1) = dedup_flags(flags_396(), DIFF_396);
        let (d2, s2) = dedup_flags(flags_396(), DIFF_396);
        assert_eq!(s1.deduped, s2.deduped);
        let shape = |d: &[ProbeFlag]| -> Vec<(String, String, Option<String>)> {
            d.iter()
                .map(|f| (f.bundle_id.clone(), f.charge_text.clone(), f.anchor.clone()))
                .collect()
        };
        assert_eq!(shape(&d1), shape(&d2), "same input twice → identical dedup output");
    }

    // ── #1299: needs_check tier clustering ───────────────────────────

    fn nc_flag(bundle_id: &str, charge_text: &str) -> JudgedFlag {
        JudgedFlag {
            flag: flag(bundle_id, "gpt-4o", 0, charge_text),
            pass1: JudgeRecord {
                ruling: JudgeRuling::NeedsCheck,
                decisive_evidence: "e".into(),
                note_for_author: "n".into(),
                pass: 1,
                seconds: 0.0,
            },
            pass2: None,
            tier: Tier::NeedsCheck,
            demoted_by_pass2: false,
            verify: None,
            demoted_by_verify: false,
        }
    }

    #[test]
    fn cluster_needs_check_below_threshold_returns_empty() {
        let judged: Vec<JudgedFlag> = (0..NEEDS_CHECK_CLUSTER_THRESHOLD)
            .map(|_| nc_flag("f.ts", "possible undefined index"))
            .collect();
        assert!(
            cluster_needs_check(&judged).is_empty(),
            "at or below the threshold, needs_check renders raw"
        );
    }

    #[test]
    fn cluster_needs_check_396_caps_and_conserves_every_concern() {
        // ~25 heavily-duplicative needs_check items across files + families.
        let mut judged: Vec<JudgedFlag> = Vec::new();
        for _ in 0..12 {
            judged.push(nc_flag(IHS_FILE, "the partial-update DTO may drop a field"));
        }
        for _ in 0..8 {
            judged.push(nc_flag(IHS_FILE, "`incorporatedDate` recorded under the wrong field name"));
        }
        for _ in 0..5 {
            judged.push(nc_flag(SPEC_FILE, "index may be undefined / out of bounds"));
        }
        // Confirmed flags must be ignored by the clusterer.
        let mut confirmed = nc_flag(IHS_FILE, "a real confirmed bug");
        confirmed.tier = Tier::Confirmed;
        confirmed.pass1.ruling = JudgeRuling::Confirmed;
        judged.push(confirmed);

        let clusters = cluster_needs_check(&judged);
        assert!(!clusters.is_empty(), "25 needs_check > threshold → clustered");

        // NEVER a drop: the clusters' counts sum to the needs_check total.
        let total: usize = clusters.iter().map(|c| c.count).sum();
        assert_eq!(total, 25, "clustering conserves every concern — nothing hidden");

        // Deterministic — same input, identical clusters.
        assert_eq!(cluster_needs_check(&judged), clusters);

        // The rendered bullet names the count + file + mechanism.
        let biggest = clusters.iter().max_by_key(|c| c.count).unwrap();
        let bullet = biggest.bullet();
        assert!(bullet.contains("12 related concerns"), "bullet names the count: {bullet}");
        assert!(bullet.contains(IHS_FILE), "bullet names the file: {bullet}");
    }

    // ── double-confirm state machine ────────────────────────────────

    fn scripted_chat(
        script: RefCell<Vec<&'static str>>,
    ) -> impl FnMut(&ChatCall) -> Result<SingleShotReply> {
        move |_call: &ChatCall| {
            let mut s = script.borrow_mut();
            if s.is_empty() {
                return Ok(reply(""));
            }
            Ok(reply(s.remove(0)))
        }
    }

    const CONFIRM_JSON: &str = "```json\n{\"ruling\": \"confirmed\", \"decisive_evidence\": \"e\", \"note_for_author\": \"n\"}\n```";
    const FP_JSON: &str = "```json\n{\"ruling\": \"false_positive\", \"decisive_evidence\": \"e\", \"note_for_author\": \"n\"}\n```";
    const NEEDS_CHECK_JSON: &str = "```json\n{\"ruling\": \"needs_check\", \"decisive_evidence\": \"e\", \"note_for_author\": \"n\"}\n```";

    #[test]
    fn double_confirm_confirm_then_confirm_is_confirmed_tier() {
        let mut chat = scripted_chat(RefCell::new(vec![CONFIRM_JSON, CONFIRM_JSON]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.pass1.ruling, JudgeRuling::Confirmed);
        assert_eq!(o.pass2.unwrap().ruling, JudgeRuling::Confirmed);
        assert_eq!(o.tier, Tier::Confirmed);
        assert!(!o.demoted_by_pass2);
        assert_eq!(o.calls, 2, "one clean dispatch per pass");
    }

    #[test]
    fn double_confirm_confirm_then_false_positive_demotes_to_needs_check() {
        let mut chat = scripted_chat(RefCell::new(vec![CONFIRM_JSON, FP_JSON]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.pass1.ruling, JudgeRuling::Confirmed);
        assert_eq!(o.pass2.unwrap().ruling, JudgeRuling::FalsePositive);
        assert_eq!(o.tier, Tier::NeedsCheck, "disagreement demotes, never ships as confirmed");
        assert!(o.demoted_by_pass2);
    }

    #[test]
    fn double_confirm_pass1_needs_check_skips_pass2() {
        let mut chat = scripted_chat(RefCell::new(vec![NEEDS_CHECK_JSON]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.pass1.ruling, JudgeRuling::NeedsCheck);
        assert!(o.pass2.is_none());
        assert_eq!(o.tier, Tier::NeedsCheck);
        assert!(!o.demoted_by_pass2);
        assert_eq!(o.calls, 1);
        assert_eq!(o.pass2_ms, 0, "no pass-2 dispatch, no pass-2 wall time");
    }

    #[test]
    fn double_confirm_pass1_false_positive_archives_without_pass2() {
        let mut chat = scripted_chat(RefCell::new(vec![FP_JSON]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.pass1.ruling, JudgeRuling::FalsePositive);
        assert!(o.pass2.is_none());
        assert_eq!(o.tier, Tier::Archived);
    }

    #[test]
    fn double_confirm_unparsed_retries_then_archives() {
        // Two garbage replies: pass-1 attempt, retry — still unparsed.
        let mut chat = scripted_chat(RefCell::new(vec!["no verdict here", "still nothing"]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.pass1.ruling, JudgeRuling::Unparsed);
        assert!(o.pass2.is_none());
        assert_eq!(o.tier, Tier::Archived);
        assert!(!o.demoted_by_pass2);
        assert_eq!(o.calls, 2, "the unparsed retry is a real dispatch and is counted");
    }

    #[test]
    fn double_confirm_unparsed_retry_recovers() {
        // First attempt garbage, retry succeeds — the retry's ruling wins.
        let mut chat = scripted_chat(RefCell::new(vec!["garbage", CONFIRM_JSON, CONFIRM_JSON]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.pass1.ruling, JudgeRuling::Confirmed, "the retry's clean ruling survives");
        assert_eq!(o.pass2.unwrap().ruling, JudgeRuling::Confirmed);
        assert_eq!(o.tier, Tier::Confirmed);
        assert_eq!(o.calls, 3, "pass-1 attempt + retry + pass-2 = three real dispatches");
    }

    // ── passes knob (#1266): single pass (passes: 1) ─────────────────
    // pass-1's ruling IS the tier; no confirmation pass ever runs — the
    // frontier cost lever.

    #[test]
    fn passes_one_confirm_is_confirmed_with_a_single_call() {
        // A counting closure (not `scripted_chat`) so the "invoked exactly
        // once" claim is literal, not inferred from the outcome's own count.
        let mut calls = 0u32;
        let mut chat = |_call: &ChatCall| {
            calls += 1;
            Ok(reply(CONFIRM_JSON))
        };
        let o =
            judge_one_flag_with_passes(1, "prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.pass1.ruling, JudgeRuling::Confirmed);
        assert!(o.pass2.is_none(), "passes: 1 never runs a confirmation pass");
        assert_eq!(o.tier, Tier::Confirmed, "the single pass IS the tier directly");
        assert!(!o.demoted_by_pass2);
        assert_eq!(o.calls, 1);
        assert_eq!(o.pass2_ms, 0, "no confirmation pass, no confirmation wall time");
        assert_eq!(calls, 1, "the judge chat closure fired exactly once for this flag");
    }

    #[test]
    fn passes_one_needs_check_tiers_directly() {
        let mut calls = 0u32;
        let mut chat = |_call: &ChatCall| {
            calls += 1;
            Ok(reply(NEEDS_CHECK_JSON))
        };
        let o =
            judge_one_flag_with_passes(1, "prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.pass1.ruling, JudgeRuling::NeedsCheck);
        assert_eq!(o.tier, Tier::NeedsCheck, "pass-1's needs_check IS the tier");
        assert!(o.pass2.is_none());
        assert_eq!(calls, 1, "a non-confirmed pass-1 earns no second call under any passes");
    }

    #[test]
    fn passes_one_false_positive_archives_directly() {
        let mut chat = scripted_chat(RefCell::new(vec![FP_JSON]));
        let o =
            judge_one_flag_with_passes(1, "prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.pass1.ruling, JudgeRuling::FalsePositive);
        assert_eq!(o.tier, Tier::Archived, "pass-1's false_positive tiers out directly");
        assert!(o.pass2.is_none());
        assert_eq!(o.calls, 1);
    }

    // ── passes knob (#1266): N-pass unanimous consensus (passes: 3) ──
    // A flag stays Confirmed only if EVERY pass that runs confirms it; the
    // first non-confirm demotes and early-exits (N passes is never N× cost).

    #[test]
    fn passes_three_all_confirm_is_confirmed_after_three_calls() {
        let mut calls = 0u32;
        let mut chat = |_call: &ChatCall| {
            calls += 1;
            Ok(reply(CONFIRM_JSON))
        };
        let o =
            judge_one_flag_with_passes(3, "prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.tier, Tier::Confirmed, "unanimous confirms hold the bar");
        assert!(!o.demoted_by_pass2);
        assert_eq!(o.pass1.ruling, JudgeRuling::Confirmed);
        // The decisive `pass2` slot holds the LAST confirmation pass (pass-3),
        // carrying its real pass number.
        let last = o.pass2.as_ref().expect("a later confirmation pass survives into the slot");
        assert_eq!(last.ruling, JudgeRuling::Confirmed);
        assert_eq!(last.pass, 3, "the decisive slot carries the real pass number, not a hardcoded 2");
        assert_eq!(o.calls, 3);
        assert_eq!(calls, 3, "pass-1 + two confirmation passes");
    }

    #[test]
    fn passes_three_final_disagreement_demotes_after_three_calls() {
        // confirm → confirm → false_positive: unanimity breaks on the last
        // pass, so all three ran before the demotion landed.
        let mut calls = 0u32;
        let mut chat = |_call: &ChatCall| {
            calls += 1;
            Ok(reply(if calls < 3 { CONFIRM_JSON } else { FP_JSON }))
        };
        let o =
            judge_one_flag_with_passes(3, "prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.tier, Tier::NeedsCheck, "one disagreement breaks unanimity, never ships confirmed");
        assert!(o.demoted_by_pass2);
        assert_eq!(o.pass2.as_ref().unwrap().ruling, JudgeRuling::FalsePositive);
        assert_eq!(o.calls, 3);
        assert_eq!(calls, 3, "all three passes ran before the late disagreement");
    }

    #[test]
    fn passes_three_early_disagreement_exits_after_two_calls() {
        // confirm → false_positive: the unanimous early-exit fires at pass-2,
        // so pass-3 never runs — N passes is not N× cost.
        let mut calls = 0u32;
        let mut chat = |_call: &ChatCall| {
            calls += 1;
            Ok(reply(if calls < 2 { CONFIRM_JSON } else { FP_JSON }))
        };
        let o =
            judge_one_flag_with_passes(3, "prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.tier, Tier::NeedsCheck);
        assert!(o.demoted_by_pass2);
        assert_eq!(o.calls, 2, "early-exit — the third pass is skipped");
        assert_eq!(calls, 2, "the unanimous rule stops at the first non-confirm");
    }

    // ── passes knob (#1266): passes: 2 IS the historical double-confirm ─

    #[test]
    fn passes_two_reproduces_double_confirm_exactly() {
        // The explicit `passes: 2` path and the `double_confirm_*` wrapper
        // (which delegates passes=2) must agree — confirm→confirm Confirmed,
        // confirm→false_positive demoted.
        let mut chat = scripted_chat(RefCell::new(vec![CONFIRM_JSON, CONFIRM_JSON]));
        let ok =
            judge_one_flag_with_passes(2, "prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(ok.tier, Tier::Confirmed);
        assert_eq!(ok.pass2.as_ref().unwrap().pass, 2);
        assert_eq!(ok.calls, 2);

        let mut chat = scripted_chat(RefCell::new(vec![CONFIRM_JSON, FP_JSON]));
        let demoted =
            judge_one_flag_with_passes(2, "prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(demoted.tier, Tier::NeedsCheck);
        assert!(demoted.demoted_by_pass2);
        assert_eq!(demoted.calls, 2);
    }

    // ── empty probe draw ─────────────────────────────────────────────

    #[test]
    fn probe_one_draw_empty_content_retries_once_then_skips() {
        let mut calls = 0u32;
        let mut chat = |_call: &ChatCall| {
            calls += 1;
            Ok(reply(""))
        };
        let (content, tokens, _served) =
            probe_one_draw(&mut chat, "m", "sys", "user", 100, None).expect("no dispatch error");
        assert!(content.is_none(), "still empty after retry -> skipped, not a flag");
        assert_eq!(calls, 2, "exactly one retry (two total attempts)");
        // (#1260) BOTH empty attempts are billed — a hosted reasoning model
        // that burns its budget thinking and returns empty still spent real
        // tokens the caller must account.
        assert_eq!(tokens, 20, "empty-empty bills both attempts (10 + 10)");
    }

    #[test]
    fn probe_one_draw_recovers_on_retry() {
        let mut calls = 0u32;
        let mut chat = |_call: &ChatCall| {
            calls += 1;
            if calls == 1 {
                Ok(reply(""))
            } else {
                Ok(reply("a real defect description"))
            }
        };
        let (content, tokens, _served) = probe_one_draw(&mut chat, "m", "sys", "user", 100, None).unwrap();
        assert_eq!(content.unwrap(), "a real defect description");
        assert_eq!(calls, 2);
        // (#1260) The discarded empty attempt is still billed alongside the
        // recovering one.
        assert_eq!(tokens, 20, "empty-then-recover bills both attempts (10 + 10)");
    }

    #[test]
    fn probe_one_draw_propagates_dispatch_error() {
        let mut chat = |_call: &ChatCall| -> Result<SingleShotReply> { Err(anyhow!("network down")) };
        let err = probe_one_draw(&mut chat, "m", "sys", "user", 100, None).unwrap_err();
        assert!(err.to_string().contains("network down"));
    }

    // ── selector filtering ───────────────────────────────────────────

    #[test]
    fn selector_filters_by_fact_family() {
        let bundles = vec![
            BundleInput { id: "a".into(), fact_family: "auth".into(), code: String::new(), probe_code: String::new(), facts: vec![], manifest: vec![] },
            BundleInput { id: "b".into(), fact_family: "billing".into(), code: String::new(), probe_code: String::new(), facts: vec![], manifest: vec![] },
        ];
        let sel =
            BundleSelector { fact_families: vec!["auth".to_string()], ..Default::default() };
        let selected = select_bundles_for_staffing(&bundles, Some(&sel));
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, "a");
    }

    #[test]
    fn selector_no_selector_runs_every_bundle() {
        let bundles = vec![
            BundleInput { id: "a".into(), fact_family: "auth".into(), code: String::new(), probe_code: String::new(), facts: vec![], manifest: vec![] },
            BundleInput { id: "b".into(), fact_family: "billing".into(), code: String::new(), probe_code: String::new(), facts: vec![], manifest: vec![] },
        ];
        assert_eq!(select_bundles_for_staffing(&bundles, None).len(), 2);
    }

    #[test]
    fn selector_prioritizes_param_flow_and_respects_max_bundles() {
        let bundles = vec![
            BundleInput { id: "a".into(), fact_family: "other".into(), code: String::new(), probe_code: String::new(), facts: vec![], manifest: vec![] },
            BundleInput { id: "b".into(), fact_family: "param-flow".into(), code: String::new(), probe_code: String::new(), facts: vec![], manifest: vec![] },
            BundleInput { id: "c".into(), fact_family: "other".into(), code: String::new(), probe_code: String::new(), facts: vec![], manifest: vec![] },
        ];
        let sel = BundleSelector { max_bundles: Some(2), ..Default::default() };
        let selected = select_bundles_for_staffing(&bundles, Some(&sel));
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].id, "b", "param-flow bundle is prioritized first");
    }

    // ── crew seat-requirement validation ────────────────────────────

    #[test]
    fn validate_review_crew_happy_path() {
        let crew = valid_crew();
        let ReviewSeats { probes, judge, verify: _ } = validate_review_crew(&crew).expect("valid");
        assert_eq!(probes.len(), 1);
        assert_eq!(judge.pm.id, "judge-model");
    }

    #[test]
    fn validate_review_crew_missing_probe_seat_rejected() {
        let crew = crew_with(vec![("review-judge", vec![staffing("fast", "j", 1)])]);
        let err = validate_review_crew(&crew).unwrap_err();
        assert!(err.to_string().contains("review-probe"));
    }

    #[test]
    fn validate_review_crew_empty_probe_staffing_rejected() {
        let crew = crew_with(vec![
            ("review-probe", vec![]),
            ("review-judge", vec![staffing("fast", "j", 1)]),
        ]);
        let err = validate_review_crew(&crew).unwrap_err();
        assert!(err.to_string().contains("review-probe"));
    }

    #[test]
    fn validate_review_crew_missing_judge_seat_rejected() {
        let crew = crew_with(vec![("review-probe", vec![staffing("fast", "p", 1)])]);
        let err = validate_review_crew(&crew).unwrap_err();
        assert!(err.to_string().contains("review-judge"));
    }

    #[test]
    fn validate_review_crew_multiple_judge_staffings_rejected() {
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "p", 1)]),
            ("review-judge", vec![staffing("fast", "j1", 1), staffing("fast", "j2", 1)]),
        ]);
        let err = validate_review_crew(&crew).unwrap_err();
        assert!(err.to_string().contains("EXACTLY 1"));
    }

    // ── flow-record emission (#1247 Part 1) ───────────────────────────

    /// Recording [`ReviewEmitter`] mock — pushes every emitted record into
    /// a shared `Vec` so a test can assert the exact SEQUENCE (action +
    /// payload), same discipline as `RecordingCycler` above.
    struct RecordingEmitter {
        records: Vec<darkmux_flow::FlowRecord>,
    }
    impl RecordingEmitter {
        fn new() -> Self {
            Self { records: Vec::new() }
        }
    }
    impl ReviewEmitter for RecordingEmitter {
        fn emit(&mut self, record: darkmux_flow::FlowRecord) {
            self.records.push(record);
        }
    }

    // ── host telemetry sampler (#1247 doctrine surface) ─────────────────

    /// Deterministic fake sampler for the telemetry tests below — returns
    /// instantly with fixed values, so no test races real subprocess
    /// latency (`sample_host`'s `top -l 1` measured 600-900ms per call)
    /// against a scripted deadline on a shared CI runner. The REAL
    /// `sample_host` gets its own direct, macOS-gated coverage in
    /// `darkmux-crew`'s `telemetry_sampler` tests.
    fn fake_sample() -> HostSample {
        HostSample { cpu: Some(42), mem: Some(50), gpu: Some(7) }
    }

    /// (#1361 follow-up) Deterministic fake `lms_fn` for the telemetry
    /// tests below — the `lms_fn` twin of [`fake_sample`], same reason: an
    /// un-injected real `list_loaded` shells out to the `lms` CLI and
    /// raced/broke the fast-cadence tests' tight timing margin. Empty
    /// list — no diff, no `telemetry.lms` records — is a valid, honest
    /// "nothing resident" reading and keeps these tests focused on the
    /// `telemetry.process` family they actually assert on.
    fn fake_lms() -> anyhow::Result<Vec<darkmux_types::LoadedModel>> {
        Ok(Vec::new())
    }

    /// `HostTelemetrySampler` on its own, outside any guard: `drop` alone
    /// must stop and join the background thread. The join itself runs on
    /// a SPAWNED thread (not the test thread) and the test asserts via
    /// `recv_timeout` — a regression that makes the sampler ignore its
    /// stop flag then fails LOUD with a bounded timeout instead of
    /// wedging the whole `cargo test` run.
    #[test]
    fn host_telemetry_sampler_stops_and_joins_promptly_on_drop() {
        let sampler = HostTelemetrySampler::start(
            "case".to_string(),
            "crew".to_string(),
            Duration::from_millis(5),
            Duration::from_millis(2),
            fake_sample,
            fake_lms,
        );
        // Let at least one interval tick elapse so the thread is inside
        // its live sample-or-sleep loop, not still spinning up.
        thread::sleep(Duration::from_millis(20));
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            drop(sampler); // `HostTelemetrySampler::drop` -> stop() -> join()
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("sampler thread did not stop within 5s — thread leak");
    }

    /// `ReviewRunGuard` owns the sampler's whole-run lifecycle (see its
    /// doc). Clean finish: `task_started` -> `task_finished` -> the guard
    /// drops — the sampler thread must already be stopped by the time that
    /// drop returns. Same bounded-timeout discipline as the sampler-only
    /// test above (drop runs on a spawned thread; the test thread asserts
    /// via `recv_timeout` so a hang fails loud instead of wedging the run).
    #[test]
    fn bookend_guard_clean_finish_stops_telemetry_sampler_thread() {
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut emitter = RecordingEmitter::new();
            let mut sink = EmitterSink(&mut emitter);
            let mut guard = ReviewRunGuard::new_with_telemetry(
                &mut sink,
                "case-1",
                "crew-1",
                Duration::from_millis(5),
                Duration::from_millis(2),
                fake_sample,
                fake_lms,
            );
            guard.task_started(json!({"status": "started"}));
            let env = ReviewEnvelope {
                case_id: "case-1".to_string(),
                crew: "crew-1".to_string(),
                ..Default::default()
            };
            guard.task_finished(&env);
            drop(guard); // blocks until the sampler thread stops + joins
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("guard drop (clean finish) did not stop the telemetry sampler thread within 5s — thread leak");
    }

    /// The error-path mirror: `task_started` with no matching
    /// `task_finished` (an early `?`-return / panic unwind) — the guard's
    /// Drop path (still ARMED) closes open steps + emits a terminal error
    /// record AND must still stop the sampler thread, exactly like the
    /// clean-finish path above.
    #[test]
    fn bookend_guard_error_path_drop_stops_telemetry_sampler_thread() {
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut emitter = RecordingEmitter::new();
            {
                let mut sink = EmitterSink(&mut emitter);
                let mut guard = ReviewRunGuard::new_with_telemetry(
                    &mut sink,
                    "case-2",
                    "crew-2",
                    Duration::from_millis(5),
                    Duration::from_millis(2),
                    fake_sample,
                    fake_lms,
                );
                guard.task_started(json!({"status": "started"}));
                // No `task_finished` call — the guard drops here still
                // ARMED, exercising the error path.
            }
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("guard drop (error path) did not stop the telemetry sampler thread within 5s — thread leak");
    }

    // ── staffing snapshot (#1247 lab-view addition) ────────────────────

    #[test]
    fn staffing_snapshot_absent_field_on_an_older_envelope_deserializes_as_none() {
        // A pre-#1247 envelope has no `staffing` key at all — `default` +
        // `skip_serializing_if` must let it deserialize as `None`, never a
        // hard parse failure (the schema-lenience discipline every optional
        // envelope field in this module follows).
        let legacy = r#"{
            "case_id": "c1", "crew": "test-crew", "mode": "sequential",
            "members": [], "steps": [], "bundles": 1, "raw_flags": 0,
            "deduped_flags": 0, "flags": [], "judged": [],
            "confirmed": 0, "needs_check": 0, "archived": 0,
            "fingerprint": {}
        }"#;
        let env: ReviewEnvelope = serde_json::from_str(legacy).expect("legacy envelope without staffing parses");
        assert!(env.staffing.is_none());
    }

    // ── run_judge_only ────────────────────────────────────────────────

    #[test]
    fn run_judge_only_skips_probe_and_judges_supplied_flags() {
        let crew = valid_crew();
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
            bundles: None,
        };
        let flags = vec![flag("billing.ts", "member-a", 0, "`const end = start.plus(30)` double-counts")];
        let mut cycler = RecordingCycler::new();
        let mut chat = |_call: &ChatCall| Ok(reply(CONFIRM_JSON));
        let env = run_judge_only(flags, &inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");
        assert_eq!(env.raw_flags, 1);
        assert_eq!(env.judged.len(), 1);
        assert!(!cycler.log.iter().any(|s| s.contains("probe-model")), "probe never dispatched");
        assert_eq!(
            env.mode, "sequential",
            "the envelope records the caller's resolved mode, not a hardcoded label"
        );
    }

    #[test]
    fn run_judge_only_records_the_callers_parallel_mode() {
        let crew = valid_crew();
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Parallel,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
            bundles: None,
        };
        let flags = vec![flag("billing.ts", "member-a", 0, "`const end = start.plus(30)` off by one")];
        let mut cycler = RecordingCycler::new();
        let mut chat = |_call: &ChatCall| Ok(reply(FP_JSON));
        let env = run_judge_only(flags, &inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");
        assert_eq!(env.mode, "parallel", "a judge-only re-run of a parallel review keeps its provenance");
    }

    /// (#1355/#1357 review round) `finish_review`'s judge remote-budget
    /// honesty gates are STILL PRODUCTION CODE via the `--charges-file`
    /// path (`mission launch review --param charges_file=...` -> `run_judge_only` ->
    /// `finish_review`) — but every test that used to pin them routed
    /// through the deleted `run_review` driver, so the migration would have
    /// left them coverage-free. This ONE test pins all three gates through
    /// the surviving caller (the graph path lacks these gates entirely —
    /// that's the KNOWN GAP the migrated graph tests characterize):
    ///
    /// 1. the judge's per-pass budget rows reach `env.remote_budgets`;
    /// 2. bucket exhaustion (`skipped > 0`) degrades the run with the
    ///    reason named — never a silent pass (#1260);
    /// 3. a remote judge dispatch failure is named in `env.warnings`
    ///    UNCONDITIONALLY, whether or not the run also degrades (#1329's
    ///    loud-beats-quiet half).
    ///
    /// Scripted remote-call order (flag-major, one call per pass, no retry
    /// after a dispatch `Err` — same convention as the graph-path minority
    /// test): f1.p1 errs (503) -> f1 archives, dispatch_error counted;
    /// f2.p1 confirms at 600 tokens -> the 100-token pass-1 bucket is
    /// exhausted after the spend; f2.p2 confirms (its OWN pass-2 bucket —
    /// separate execution per #1260); f3.p1 is REFUSED by the exhausted
    /// pass-1 bucket -> ruled Error with the reason, no chat call.
    #[test]
    fn run_judge_only_remote_budget_exhaustion_rows_degrade_and_warn() {
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![remote_staffing("cloud", "gpt-judge", 1)]),
        ]);
        let mut inputs = ReviewInputs {
            case_id: "c-judge-only-budget".to_string(),
            crew: &crew,
            intent_title: "t",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 100, // one 600-token ruling exhausts a pass bucket
            bundles: None,
        };
        // Three flags in three different bundles (no anchors + distinct
        // bundle_id ⇒ all survive dedup, in input order).
        inputs.bundles = Some(vec![bundle_input("a.ts"), bundle_input("b.ts"), bundle_input("c.ts")]);
        let flags = vec![
            flag("a.ts", "member-a", 0, "charge one"),
            flag("b.ts", "member-a", 0, "charge two"),
            flag("c.ts", "member-a", 0, "charge three"),
        ];
        let mut cycler = RecordingCycler::new();
        let remote_calls = RefCell::new(0u32);
        let mut chat = |call: &ChatCall| {
            assert!(call.endpoint.is_some(), "judge-only + remote judge ⇒ every call is remote");
            let idx = *remote_calls.borrow();
            *remote_calls.borrow_mut() += 1;
            if idx == 0 {
                Err(anyhow!("endpoint 503"))
            } else {
                Ok(SingleShotReply {
                    content: CONFIRM_JSON.to_string(),
                    total_tokens: Some(600),
                    prompt_tokens: None,
                    completion_tokens: None,
                    model: None,
                })
            }
        };
        let env = run_judge_only(flags, &inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");
        assert_eq!(*remote_calls.borrow(), 3, "f1.p1(err) + f2.p1 + f2.p2 — f3.p1 never dispatched");

        // Gate 1: BOTH per-pass budget rows land in the envelope.
        let p1 = env.remote_budgets.iter().find(|r| r.stage == "judge-pass1").expect("judge-pass1 row");
        assert!(p1.exhausted);
        assert_eq!(p1.used_tokens, 600);
        assert_eq!(p1.skipped_calls, 1, "f3's pass-1 was refused by the exhausted bucket");
        let p2 = env.remote_budgets.iter().find(|r| r.stage == "judge-pass2").expect("judge-pass2 row — its own execution");
        assert_eq!(p2.skipped_calls, 0, "pass-2 drew from its own fresh allowance");

        // Gate 2: exhaustion degrades the run with the reason named.
        let reason = env.degenerate.as_deref().expect("judge bucket exhaustion degrades the run");
        assert!(reason.contains("remote judge token budget exhausted"), "got: {reason}");

        // Gate 3: the dispatch failure is named in env.warnings even though
        // the run ALSO degraded for the budget reason (#1329 — the warning
        // channel stays complete regardless of which degenerate gate fired).
        assert!(
            env.warnings.iter().any(|w| w.contains("remote judge dispatch failed on 1 of 3 flag")),
            "the #1329 warning must land unconditionally: {:?}",
            env.warnings
        );

        // Per-flag honesty (unchanged `judge_one_flag_with_passes` logic):
        // f1 archived on its dispatch error, f2 keeps its real double-confirm,
        // f3 is ruled Error with the budget reason — never silently confirmed.
        assert_eq!(env.judged.len(), 3);
        assert_eq!(env.judged[0].tier, Tier::Archived, "f1's dispatch error archives it");
        assert_eq!(env.judged[1].tier, Tier::Confirmed, "f2's real ruling survives");
        assert_eq!(env.judged[2].pass1.ruling, JudgeRuling::Error, "f3 was refused, not faked");
        assert!(env.judged[2].pass1.note_for_author.contains("remote token budget exhausted"));
    }

    // ── ExecMode auto-resolution (#1230 Packet 1: gestalt wave scheduler) ──

    /// A minimal, valid `WaveSchedule` for [`wave_schedule_to_exec_mode`]'s
    /// pure-projection tests — the wave PARTITIONING itself is already
    /// covered by `darkmux-gestalt`'s own `plan_waves` table tests; this
    /// only pins the wave-count → `ExecMode` mapping this module owns.
    fn schedule_with_waves(n: usize) -> darkmux_gestalt::WaveSchedule {
        let placement = |i: usize| darkmux_gestalt::Placement {
            model_key: format!("m{i}"),
            identifier: format!("darkmux:m{i}"),
            min_ctx: 8_000,
            seat: "probe".to_string(),
        };
        darkmux_gestalt::WaveSchedule {
            waves: (0..n).map(|i| vec![placement(i)]).collect(),
            refusals: Vec::new(),
            mode: darkmux_gestalt::WaveMode::Auto,
            effective_limit_bytes: None,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn wave_schedule_to_exec_mode_one_wave_is_parallel_more_is_sequential() {
        assert_eq!(wave_schedule_to_exec_mode(&schedule_with_waves(0)), ExecMode::Parallel);
        assert_eq!(wave_schedule_to_exec_mode(&schedule_with_waves(1)), ExecMode::Parallel);
        assert_eq!(wave_schedule_to_exec_mode(&schedule_with_waves(2)), ExecMode::Sequential);
        assert_eq!(wave_schedule_to_exec_mode(&schedule_with_waves(3)), ExecMode::Sequential);
    }

    #[test]
    fn resolve_auto_via_waves_empty_placements_is_parallel_without_touching_lms() {
        // No distinct local models (e.g. every probe + the judge are
        // remote) short-circuits to Parallel without any `LmsHost`/
        // `MacProbe` I/O — nothing to co-reside.
        assert_eq!(resolve_auto_via_waves(&[]), ExecMode::Parallel);
    }

    // ── judge_prompt shape ─────────────────────────────────────────────

    #[test]
    fn judge_prompt_includes_all_sections_when_present() {
        let p = judge_prompt(
            "Add billing window",
            "extends the retention window",
            "const end = start.plus(30)",
            &["fact one".to_string()],
            "the boundary is double-counted",
        );
        assert!(p.contains("Add billing window"));
        assert!(p.contains("extends the retention window"));
        assert!(p.contains("const end = start.plus(30)"));
        assert!(p.contains("## Fact sheet given to the flagging reviewer"));
        assert!(p.contains("fact one"));
        assert!(p.contains("the boundary is double-counted"));
        assert!(p.contains("```json"));
        assert!(p.contains("\"ruling\""));
    }

    #[test]
    fn judge_prompt_omits_bare_sections() {
        let p = judge_prompt("", "", "code", &[], "charge");
        assert!(p.contains("(no description provided)"));
        assert!(!p.contains("## Fact sheet given to the flagging reviewer"));
    }

    /// Phase A parity (#1256): a title present but an ABSENT body defaults
    /// only the body line — the title still renders. A single combined
    /// `intent: &str` field couldn't distinguish this from "everything
    /// blank"; separate `intent_title`/`intent_body` params can (and do,
    /// matching `judge-runner.py`'s `judge_one` per-field defaulting).
    #[test]
    fn judge_prompt_title_present_body_absent_still_renders_the_title() {
        let p = judge_prompt("Add billing window", "", "code", &[], "charge");
        assert!(p.contains("Add billing window"));
        assert!(p.contains("(no description provided)"));
    }

    // ── Phase A prompt-parity golden harness (#1256) ───────────────────
    //
    // Provenance: every golden constant below was captured by RUNNING the
    // Phase A python reference (NOT hand-transcribed) against a synthetic,
    // non-corpus fixture during development of this PR:
    //   - probe-runner.py's own `build_prompt()` + `read_code_excerpt()`,
    //     both real and unmodified, over a synthetic worktree containing
    //     the two-function `src/example.ts` fixture — so the probe goldens
    //     carry Phase A's OWN probe code format (``### `path` (lines
    //     a-b)`` + a ```` ```typescript ```` fence per block), which
    //     `bundle::slice_code_probe` ports and `BundleInput::probe_code`
    //     carries (per-seat formats — the judge's `// path` raw format
    //     lives in `BundleInput::code`).
    //   - judge-runner.py's real `slice_code()` against the same synthetic
    //     worktree, then `judge_one`'s exact `user` f-string template
    //     (copy-pasted verbatim, not paraphrased) fed with synthetic
    //     probe/bundle/label dicts — `judge_one` itself fires a live
    //     LMStudio call and can't be invoked directly.
    // The generating scripts are NOT checked into this repo (scratch,
    // depend on the private `pr-review-corpus` fixture tree on the
    // maintainer's machine) — this comment plus the fixture text below is
    // the durable record of how each golden was produced.

    /// The JUDGE-format fixture code slice — what `bundle::slice_code`
    /// emits for a single-ref bundle (`// path (lines a-b)` header, raw
    /// source lines, no fence), matching judge-runner.py's own
    /// `slice_code`. Synthetic, non-corpus.
    const GOLDEN_CODE: &str = "// src/example.ts (lines 1-4)\nexport function clampRetryDelay(attempt: number, base: number): number {\n  const delay = base * Math.pow(2, attempt);\n  return Math.min(delay, 30000);\n}";

    /// The PROBE-format fixture code slice — `read_code_excerpt`'s output
    /// for the same ref, captured verbatim from running the python
    /// reference (``### `path` (lines a-b)`` + ```` ```typescript ````
    /// fence); what `bundle::slice_code_probe` emits into
    /// `BundleInput::probe_code`.
    const GOLDEN_PROBE_CODE: &str = "### `src/example.ts` (lines 1-4)\n```typescript\nexport function clampRetryDelay(attempt: number, base: number): number {\n  const delay = base * Math.pow(2, attempt);\n  return Math.min(delay, 30000);\n}\n```";

    /// `probe-runner.py`'s hardcoded `STRONG_PRIOR` constant, copied
    /// verbatim — used ONLY as this golden test's `prior` argument, to
    /// prove `probe_user_message`'s ASSEMBLY is byte-identical to
    /// `build_prompt`'s. Production wiring passes `review-probe.md`'s text
    /// instead (close in spirit, not necessarily byte-identical wording —
    /// a persona-CONTENT question handled at the measurement layer, out of
    /// this issue's scope).
    const GOLDEN_STRONG_PRIOR: &str = "A thorough code review established that this code contains at least one real defect. Identify the most serious defect in the code below. Name the mechanism precisely, quote the exact line(s) responsible using backticks, and give a concrete example scenario: a specific input and what the code actually does versus what it should do.";

    fn golden_bundle(facts: Vec<String>) -> BundleInput {
        BundleInput {
            id: "src/example.ts".to_string(),
            fact_family: "unscoped".to_string(),
            code: GOLDEN_CODE.to_string(),
            probe_code: GOLDEN_PROBE_CODE.to_string(),
            facts,
            manifest: vec![],
        }
    }

    #[test]
    fn probe_prompt_matches_phase_a_golden_bare() {
        // Captured from probe-runner.py's real build_prompt(worktree,
        // [{path: "src/example.ts", start: 1, end: 4}], []) — including
        // read_code_excerpt's own fenced block format.
        let golden = "A thorough code review established that this code contains at least one real defect. Identify the most serious defect in the code below. Name the mechanism precisely, quote the exact line(s) responsible using backticks, and give a concrete example scenario: a specific input and what the code actually does versus what it should do.\n\nCode:\n\n### `src/example.ts` (lines 1-4)\n```typescript\nexport function clampRetryDelay(attempt: number, base: number): number {\n  const delay = base * Math.pow(2, attempt);\n  return Math.min(delay, 30000);\n}\n```";
        let bundle = golden_bundle(vec![]);
        assert_eq!(probe_user_message(GOLDEN_STRONG_PRIOR, &bundle), golden);
    }

    #[test]
    fn probe_prompt_matches_phase_a_golden_with_facts() {
        // Same build_prompt run with the two facts supplied.
        let golden = "A thorough code review established that this code contains at least one real defect. Identify the most serious defect in the code below. Name the mechanism precisely, quote the exact line(s) responsible using backticks, and give a concrete example scenario: a specific input and what the code actually does versus what it should do.\n\nCode:\n\n### `src/example.ts` (lines 1-4)\n```typescript\nexport function clampRetryDelay(attempt: number, base: number): number {\n  const delay = base * Math.pow(2, attempt);\n  return Math.min(delay, 30000);\n}\n```\n\nComputed facts about this code (mechanically extracted, not interpreted):\n\n- `attempt` is caller-controlled and unbounded\n- `base` defaults to 1000 in all call sites";
        let bundle = golden_bundle(vec![
            "`attempt` is caller-controlled and unbounded".to_string(),
            "`base` defaults to 1000 in all call sites".to_string(),
        ]);
        assert_eq!(probe_user_message(GOLDEN_STRONG_PRIOR, &bundle), golden);
    }

    #[test]
    fn judge_prompt_matches_phase_a_golden_with_facts_and_intent() {
        let golden = "## The author's stated case (the pull request description)\nBound retry backoff to a sane ceiling\nCaps the exponential backoff delay so a large attempt count cannot stall retries indefinitely.\n\n## The code under review\n```typescript\n// src/example.ts (lines 1-4)\nexport function clampRetryDelay(attempt: number, base: number): number {\n  const delay = base * Math.pow(2, attempt);\n  return Math.min(delay, 30000);\n}\n```\n\n## Fact sheet given to the flagging reviewer\n`attempt` is caller-controlled and unbounded\n`base` defaults to 1000 in all call sites\n\n## The flagged item to investigate\nThe delay calculation in `clampRetryDelay` never verifies `attempt` is non-negative — a negative attempt shrinks the delay below the intended floor.\n\nInvestigate the flagged item against the code above. End your reply with exactly one fenced JSON block:\n```json\n{\"ruling\": \"confirmed\" | \"needs_check\" | \"false_positive\", \"decisive_evidence\": \"<the specific code line or checked claim that decided it>\", \"note_for_author\": \"<one or two sentences the author reads>\"}\n```";
        let p = judge_prompt(
            "Bound retry backoff to a sane ceiling",
            "Caps the exponential backoff delay so a large attempt count cannot stall retries indefinitely.",
            GOLDEN_CODE,
            &[
                "`attempt` is caller-controlled and unbounded".to_string(),
                "`base` defaults to 1000 in all call sites".to_string(),
            ],
            "The delay calculation in `clampRetryDelay` never verifies `attempt` is non-negative — a negative attempt shrinks the delay below the intended floor.",
        );
        assert_eq!(p, golden);
    }

    #[test]
    fn judge_prompt_matches_phase_a_golden_bare_no_facts_no_intent() {
        let golden = "## The author's stated case (the pull request description)\n\n(no description provided)\n\n## The code under review\n```typescript\n// src/example.ts (lines 1-4)\nexport function clampRetryDelay(attempt: number, base: number): number {\n  const delay = base * Math.pow(2, attempt);\n  return Math.min(delay, 30000);\n}\n```\n\n## The flagged item to investigate\nThe delay calculation in `clampRetryDelay` never verifies `attempt` is non-negative — a negative attempt shrinks the delay below the intended floor.\n\nInvestigate the flagged item against the code above. End your reply with exactly one fenced JSON block:\n```json\n{\"ruling\": \"confirmed\" | \"needs_check\" | \"false_positive\", \"decisive_evidence\": \"<the specific code line or checked claim that decided it>\", \"note_for_author\": \"<one or two sentences the author reads>\"}\n```";
        let p = judge_prompt(
            "",
            "",
            GOLDEN_CODE,
            &[],
            "The delay calculation in `clampRetryDelay` never verifies `attempt` is non-negative — a negative attempt shrinks the delay below the intended floor.",
        );
        assert_eq!(p, golden);
    }

    // ── bundles_from_diff (provisional bundler) ────────────────────────

    #[test]
    fn bundles_from_diff_one_bundle_per_changed_file() {
        let bundles = bundles_from_diff(DIFF);
        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[0].id, "billing.ts");
        assert!(bundles[0].code.contains("const end = start.plus(30)"));
    }

    // ═══════════════════════════════════════════════════════════════
    // Phase B coverage packet (#1222) — protocol/dedup/telemetry edges
    // ═══════════════════════════════════════════════════════════════

    // ── judge ruling parser: multi-fence, extras, null values ─────────

    /// A judge reply can carry more than one fenced JSON block (e.g. a
    /// judge that reasons out loud, states a tentative verdict, then
    /// revises it). `judge_json_candidates` tries fences LAST-to-FIRST, so
    /// the LAST fenced block in the text must win — an earlier, superseded
    /// verdict must never leak through.
    #[test]
    fn parse_judge_ruling_multiple_valid_fences_last_wins() {
        let text = "```json\n{\"ruling\": \"confirmed\", \"decisive_evidence\": \"first pass\", \"note_for_author\": \"n1\"}\n```\nOn reflection, revising the verdict:\n```json\n{\"ruling\": \"false_positive\", \"decisive_evidence\": \"second pass\", \"note_for_author\": \"n2\"}\n```";
        let (ruling, evidence, note) = parse_judge_ruling(text).expect("parses");
        assert_eq!(ruling, JudgeRuling::FalsePositive, "the LAST fenced JSON wins, not the first");
        assert_eq!(evidence, "second pass", "the first fence's evidence must be ignored");
        assert_eq!(note, "n2");
    }

    /// `RawJudgeRuling` has no `deny_unknown_fields` — extra keys a judge
    /// bolts onto its ruling (confidence scores, nested detail) must not
    /// break parsing.
    #[test]
    fn parse_judge_ruling_tolerates_unknown_extra_fields() {
        let text = "```json\n{\"ruling\": \"confirmed\", \"decisive_evidence\": \"e\", \"note_for_author\": \"n\", \"confidence\": 0.87, \"extra\": {\"nested\": true}}\n```";
        let (ruling, evidence, note) = parse_judge_ruling(text).expect("unknown fields must not break parsing");
        assert_eq!(ruling, JudgeRuling::Confirmed);
        assert_eq!(evidence, "e");
        assert_eq!(note, "n");
    }

    /// `decisive_evidence`/`note_for_author` are `String`, not
    /// `Option<String>`, and `ruling` is a plain `String` matched against a
    /// closed set. A JSON `null` on any of these is a TYPE mismatch for
    /// serde (not a missing-field default), so every candidate in
    /// `judge_json_candidates` fails to deserialize and the whole reply
    /// falls through to `None` (Unparsed) rather than null silently
    /// standing in for an empty string or a bogus ruling.
    #[test]
    fn parse_judge_ruling_null_values_fail_to_parse_not_treated_as_empty() {
        let evidence_null = "```json\n{\"ruling\": \"confirmed\", \"decisive_evidence\": null, \"note_for_author\": \"n\"}\n```";
        assert!(
            parse_judge_ruling(evidence_null).is_none(),
            "null decisive_evidence must not silently parse as an empty string"
        );

        let ruling_null = "```json\n{\"ruling\": null, \"decisive_evidence\": \"e\", \"note_for_author\": \"n\"}\n```";
        assert!(
            parse_judge_ruling(ruling_null).is_none(),
            "a null ruling value must not silently match a variant"
        );
    }

    // ── dedup: whitespace-only anchor variance ─────────────────────────

    /// `extract_new_side_anchor` NORMALIZES (marker-strip + whitespace
    /// collapse) only to decide whether a quoted span is a legitimate
    /// anchor — the stored/returned anchor is the model's VERBATIM quote.
    /// Two flags whose backtick-quoted anchors are semantically identical
    /// but differ in internal whitespace both validate against the diff
    /// (via the collapsed fallback), yet the raw strings differ, so the
    /// dedup key `(bundle_id, anchor, family)` differs and they do NOT
    /// collapse. Characterizes current behavior — not asserted as a bug,
    /// since `dedup_flags`'s doc makes no whitespace-insensitivity promise
    /// on the key itself.
    #[test]
    fn dedup_anchors_differing_only_by_internal_whitespace_do_not_collapse() {
        let flags = vec![
            flag("b1", "member-a", 0, "The `const end = start.plus(30)` double counts."),
            flag("b1", "member-b", 0, "The `const  end = start.plus(30)` double counts."),
        ];
        let (deduped, stats) = dedup_flags(flags, DIFF);
        assert_eq!(
            stats.deduped, 2,
            "whitespace-differing anchors both validate against the diff but do not share a dedup key"
        );
        assert_eq!(deduped[0].anchor.as_deref(), Some("const end = start.plus(30)"));
        assert_eq!(
            deduped[1].anchor.as_deref(),
            Some("const  end = start.plus(30)"),
            "the stored anchor is the model's verbatim quote, not the normalized/collapsed form"
        );
    }

    // ── mechanism_family word-boundary regression suite (expanded) ─────

    /// Expands the substring-vs-token regression beyond the "tenant" case
    /// already covered: every table keyword must match as a whole token
    /// and must NOT fire on a longer/different word that merely contains
    /// it as a substring.
    #[test]
    fn mechanism_family_word_boundary_regression_suite() {
        // Real keywords match as standalone tokens.
        assert_eq!(mechanism_family("This has an async issue."), "async/await");
        assert_eq!(mechanism_family("Watch the dst transition."), "timezone/ambient-time");
        assert_eq!(mechanism_family("Provenance information is missing."), "provenance/sibling");
        assert_eq!(mechanism_family("Check the arg count."), "arity/param");

        // Longer/different words that merely CONTAIN a keyword as a
        // substring must not false-match — word-boundary, never substring.
        assert_eq!(
            mechanism_family("The function is asynchronous by design."),
            "other",
            "'asynchronous' must not token-match 'async'"
        );
        assert_eq!(
            mechanism_family("A windstorm knocked out power."),
            "other",
            "'windstorm' must not token-match 'dst'"
        );
        assert_eq!(
            mechanism_family("This proves the claim is unproven."),
            "other",
            "'proves'/'unproven' must not token-match 'provenance'"
        );
        assert_eq!(
            mechanism_family("The margarine recipe changed."),
            "other",
            "'margarine' must not token-match 'arg'"
        );
    }

    // ── double-confirm: pass-2 unparsed ─────────────────────────────────

    /// A `confirmed` pass-1 followed by a pass-2 that stays `Unparsed`
    /// (even after its own retry) is still ANY-other-than-confirmed —
    /// `judge_one_flag`'s doc is explicit this must demote, never silently
    /// promote to `Confirmed` on a garbled second call.
    #[test]
    fn double_confirm_confirm_then_pass2_unparsed_demotes_to_needs_check() {
        let mut chat = scripted_chat(RefCell::new(vec![CONFIRM_JSON, "no verdict here", "still nothing"]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.pass1.ruling, JudgeRuling::Confirmed);
        assert_eq!(o.pass2.as_ref().unwrap().ruling, JudgeRuling::Unparsed);
        assert_eq!(o.tier, Tier::NeedsCheck, "an unparsed pass-2 must demote, never silently confirm");
        assert!(o.demoted_by_pass2);
        assert_eq!(o.calls, 3, "pass-1 (1 call) + pass-2 attempt + pass-2's own unparsed-retry (2 calls)");
    }


    // ── LmsCycler residency reconciliation (#1271) ──────────────────────

    /// Write an executable shell stub standing in for `lms`, dispatching on
    /// `$1` the same subcommands `LmsCycler` issues: `ps --json` echoes the
    /// canned resident list from `$STUB_LMS_PS_JSON`; anything else (`load`,
    /// `unload`) appends its FULL argv to `$STUB_LMS_LOG` so cycling ORDER
    /// is assertable. Mirrors the `write_stub_script` pattern already used
    /// for the external-bundler subprocess seam (`lab::bundle::external`).
    #[cfg(unix)]
    fn write_stub_lms(dir: &std::path::Path) -> std::path::PathBuf {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("lms-stub.sh");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "case \"$1\" in").unwrap();
        writeln!(f, "  ps) cat \"$STUB_LMS_PS_JSON\" ;;").unwrap();
        writeln!(f, "  *) echo \"$*\" >> \"$STUB_LMS_LOG\" ;;").unwrap();
        writeln!(f, "esac").unwrap();
        writeln!(f, "exit 0").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    /// Stands up the stub + points `DARKMUX_LMS_BIN` (and its two auxiliary
    /// env vars) at it for the lifetime of one test. Env mutation means
    /// every test using this needs `#[serial_test::serial]`; `Drop` cleans
    /// the vars back up so a later, non-serial test never inherits a stale
    /// `DARKMUX_LMS_BIN`.
    #[cfg(unix)]
    struct LmsStubEnv {
        _dir: tempfile::TempDir,
        log_path: std::path::PathBuf,
    }

    #[cfg(unix)]
    impl LmsStubEnv {
        fn new(residents_json: &str) -> Self {
            let dir = tempfile::TempDir::new().unwrap();
            let script = write_stub_lms(dir.path());
            let ps_json_path = dir.path().join("ps.json");
            std::fs::write(&ps_json_path, residents_json).unwrap();
            let log_path = dir.path().join("log.txt");
            std::fs::write(&log_path, "").unwrap();
            unsafe {
                std::env::set_var("DARKMUX_LMS_BIN", &script);
                std::env::set_var("STUB_LMS_PS_JSON", &ps_json_path);
                std::env::set_var("STUB_LMS_LOG", &log_path);
            }
            Self { _dir: dir, log_path }
        }

        fn log(&self) -> String {
            std::fs::read_to_string(&self.log_path).unwrap()
        }
    }

    #[cfg(unix)]
    impl Drop for LmsStubEnv {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var("DARKMUX_LMS_BIN");
                std::env::remove_var("STUB_LMS_PS_JSON");
                std::env::remove_var("STUB_LMS_LOG");
            }
        }
    }

    /// (a) darkmux-owned resident sharing the modelKey but at an
    /// INSUFFICIENT ctx — reconcile: unload the stale instance, then load
    /// fresh at the required ctx. This is the exact #1271 repro shape
    /// (a resident from a DIFFERENT profile/crew, same underlying model,
    /// smaller ctx than this seat needs) — the old identifier-only check
    /// missed the collision and attempted a doomed second `lms load`.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn lms_cycler_darkmux_owned_wrong_ctx_reconciles_unload_then_reload() {
        let env = LmsStubEnv::new(
            r#"[{"identifier":"darkmux:devstral","modelKey":"devstral","status":"loaded","sizeBytes":14000000000,"contextLength":20000}]"#,
        );
        let mut cycler = LmsCycler;
        let model = ProfileModel { id: "devstral".to_string(), n_ctx: Some(32768), ..Default::default() };
        cycler.ensure_loaded(&model).expect("reconcile succeeds");
        let log = env.log();
        assert!(log.contains("unload darkmux:devstral"), "unload runs: {log}");
        assert!(
            log.contains("load devstral --context-length 32768 --identifier darkmux:devstral"),
            "reload runs at the required ctx: {log}"
        );
        let unload_pos = log.find("unload darkmux:devstral").unwrap();
        let load_pos = log.find("load devstral").unwrap();
        assert!(unload_pos < load_pos, "unload must precede the reload: {log}");
    }

    /// (b) darkmux-owned resident sharing the modelKey, ALREADY at a
    /// sufficient ctx — reuse, no load or unload issued. The pre-#1271
    /// "current skip-if-loaded behavior" this preserves.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn lms_cycler_darkmux_owned_right_ctx_skips_reload() {
        let env = LmsStubEnv::new(
            r#"[{"identifier":"darkmux:devstral","modelKey":"devstral","status":"loaded","sizeBytes":14000000000,"contextLength":40960}]"#,
        );
        let mut cycler = LmsCycler;
        let model = ProfileModel { id: "devstral".to_string(), n_ctx: Some(32768), ..Default::default() };
        cycler.ensure_loaded(&model).expect("reuse succeeds");
        assert_eq!(env.log(), "", "sufficient ctx already resident — no load/unload issued");
    }

    /// (c) a resident sharing the modelKey that is NOT darkmux-owned (no
    /// `darkmux:` prefix) — operator state. (#1230 Packet 1 cutover — a
    /// deliberate behavior change, see `darkmux_gestalt::planner`'s "Cutover
    /// behavior changes" doc): the cycler no longer hard-blocks around it.
    /// The foreign resident's load configuration is unknown (the #1135
    /// ghost) — never reused, never touched — but darkmux loads its OWN
    /// namespaced copy ALONGSIDE it (absolute namespace ownership, operator
    /// decision 2026-07-10, #1274) instead of refusing outright.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn lms_cycler_user_owned_same_model_key_loads_alongside_not_blocked() {
        let env = LmsStubEnv::new(
            r#"[{"identifier":"devstral-manual","modelKey":"devstral","status":"loaded","sizeBytes":14000000000,"contextLength":40960}]"#,
        );
        let mut cycler = LmsCycler;
        let model = ProfileModel { id: "devstral".to_string(), n_ctx: Some(32768), ..Default::default() };
        cycler.ensure_loaded(&model).expect("loads darkmux's own copy alongside the foreign resident");
        let log = env.log();
        assert!(
            log.contains("load devstral --context-length 32768 --identifier darkmux:devstral"),
            "darkmux's own copy loads: {log}"
        );
        assert!(!log.contains("unload"), "the foreign resident is never touched: {log}");
    }

    /// (d) no resident shares the modelKey — plain load, unchanged.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn lms_cycler_no_resident_loads_plain() {
        let env = LmsStubEnv::new("[]");
        let mut cycler = LmsCycler;
        let model = ProfileModel { id: "devstral".to_string(), n_ctx: Some(32768), ..Default::default() };
        cycler.ensure_loaded(&model).expect("plain load succeeds");
        let log = env.log();
        assert!(
            log.contains("load devstral --context-length 32768 --identifier darkmux:devstral"),
            "{log}"
        );
        assert!(!log.contains("unload"), "no unload without a resident: {log}");
    }

    /// (#1271 review round, REQUIRED fix) A resident under an EXPLICIT
    /// operator alias (`ProfileModel.identifier = Some(..)`, the documented
    /// namespace opt-out — `swap::namespaced_identifier` passes it through
    /// verbatim) is darkmux's OWN load for this profile and must classify as
    /// ours: sufficient ctx → Reuse, never Blocked.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn lms_cycler_explicit_alias_resident_right_ctx_reuses_not_blocked() {
        let env = LmsStubEnv::new(
            r#"[{"identifier":"custom","modelKey":"devstral","status":"loaded","sizeBytes":14000000000,"contextLength":32768}]"#,
        );
        let mut cycler = LmsCycler;
        let model = ProfileModel {
            id: "devstral".to_string(),
            n_ctx: Some(32768),
            identifier: Some("custom".to_string()),
            ..Default::default()
        };
        cycler.ensure_loaded(&model).expect("explicit-alias resident reuses, never Blocked");
        assert_eq!(env.log(), "", "no load or unload issued on reuse");
    }

    /// Explicit-alias resident at an INSUFFICIENT ctx — same reconcile path
    /// as the namespaced case: unload the alias instance, reload under the
    /// same alias at the required ctx.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn lms_cycler_explicit_alias_resident_wrong_ctx_reconciles() {
        let env = LmsStubEnv::new(
            r#"[{"identifier":"custom","modelKey":"devstral","status":"loaded","sizeBytes":14000000000,"contextLength":20000}]"#,
        );
        let mut cycler = LmsCycler;
        let model = ProfileModel {
            id: "devstral".to_string(),
            n_ctx: Some(32768),
            identifier: Some("custom".to_string()),
            ..Default::default()
        };
        cycler.ensure_loaded(&model).expect("explicit-alias reconcile succeeds");
        let log = env.log();
        assert!(log.contains("unload custom"), "stale alias instance unloads: {log}");
        assert!(
            log.contains("load devstral --context-length 32768 --identifier custom"),
            "reload keeps the operator's alias: {log}"
        );
        let unload_pos = log.find("unload custom").unwrap();
        let load_pos = log.find("load devstral").unwrap();
        assert!(unload_pos < load_pos, "unload precedes the reload: {log}");
    }

    /// (#1230 Packet 1 cutover — a deliberate behavior change) Multi-resident,
    /// user-owned listed AHEAD of a darkmux-stale instance: under gestalt's
    /// `decide_residency`, ownership partitions BEFORE position-matching (see
    /// `darkmux_gestalt::planner`'s "Cutover behavior changes" doc — "a
    /// foreign copy listed ahead of a darkmux copy also no longer shadows
    /// it"), so listing order no longer decides the outcome the way the old
    /// review-private `.find()` did. The owned-but-stale instance is found
    /// regardless of position → Reconcile, exactly like the mirror-ordering
    /// case below; the foreign resident is never touched either way.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn lms_cycler_multi_resident_user_owned_first_still_reconciles_owned_stale() {
        let env = LmsStubEnv::new(
            r#"[
                {"identifier":"devstral-manual","modelKey":"devstral","status":"loaded","sizeBytes":14000000000,"contextLength":40960},
                {"identifier":"darkmux:devstral","modelKey":"devstral","status":"loaded","sizeBytes":14000000000,"contextLength":20000}
            ]"#,
        );
        let mut cycler = LmsCycler;
        let model = ProfileModel { id: "devstral".to_string(), n_ctx: Some(32768), ..Default::default() };
        cycler.ensure_loaded(&model).expect("reconciles the owned-but-stale instance regardless of listing order");
        let log = env.log();
        assert!(log.contains("unload darkmux:devstral"), "the owned stale instance reconciles: {log}");
        assert!(!log.contains("unload devstral-manual"), "the foreign resident is never touched: {log}");
        assert!(
            log.contains("load devstral --context-length 32768 --identifier darkmux:devstral"),
            "{log}"
        );
    }

    /// Multi-resident, mirror ordering: darkmux-stale listed ahead of a
    /// user-owned instance → Reconcile, touching ONLY the darkmux instance —
    /// the user-owned one is never unloaded.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn lms_cycler_multi_resident_darkmux_stale_first_reconciles_only_darkmux_instance() {
        let env = LmsStubEnv::new(
            r#"[
                {"identifier":"darkmux:devstral","modelKey":"devstral","status":"loaded","sizeBytes":14000000000,"contextLength":20000},
                {"identifier":"devstral-manual","modelKey":"devstral","status":"loaded","sizeBytes":14000000000,"contextLength":40960}
            ]"#,
        );
        let mut cycler = LmsCycler;
        let model = ProfileModel { id: "devstral".to_string(), n_ctx: Some(32768), ..Default::default() };
        cycler.ensure_loaded(&model).expect("reconcile succeeds with a user-owned resident present");
        let log = env.log();
        assert!(log.contains("unload darkmux:devstral"), "darkmux instance reconciles: {log}");
        assert!(
            !log.contains("unload devstral-manual"),
            "user-owned instance is never touched: {log}"
        );
        assert!(
            log.contains("load devstral --context-length 32768 --identifier darkmux:devstral"),
            "{log}"
        );
    }

    // ── selector edge cases ──────────────────────────────────────────

    /// `max_bundles` is taken literally — `0` means the staffing gets ZERO
    /// bundles (a degenerate, silent no-op selection), not "unlimited".
    #[test]
    fn selector_max_bundles_zero_selects_nothing() {
        let bundles = vec![
            BundleInput { id: "a".into(), fact_family: "other".into(), code: String::new(), probe_code: String::new(), facts: vec![], manifest: vec![] },
            BundleInput { id: "b".into(), fact_family: "param-flow".into(), code: String::new(), probe_code: String::new(), facts: vec![], manifest: vec![] },
        ];
        let sel = BundleSelector { fact_families: vec![], max_bundles: Some(0), ..Default::default() };
        let selected = select_bundles_for_staffing(&bundles, Some(&sel));
        assert!(selected.is_empty(), "max_bundles: 0 must select nothing, not \"unlimited\"");
    }

    /// A `fact_families` restriction naming a family no bundle carries
    /// degrades to an empty selection (zero bundles for that staffing),
    /// never falls back to "no restriction matches everything."
    #[test]
    fn selector_fact_families_naming_unknown_family_selects_nothing() {
        let bundles = vec![
            BundleInput { id: "a".into(), fact_family: "auth".into(), code: String::new(), probe_code: String::new(), facts: vec![], manifest: vec![] },
            BundleInput { id: "b".into(), fact_family: "billing".into(), code: String::new(), probe_code: String::new(), facts: vec![], manifest: vec![] },
        ];
        let sel = BundleSelector {
            fact_families: vec!["nonexistent-family".to_string()],
            max_bundles: None,
            ..Default::default()
        };
        let selected = select_bundles_for_staffing(&bundles, Some(&sel));
        assert!(
            selected.is_empty(),
            "an unmatched fact_families restriction must select zero bundles, not fall back to 'no restriction'"
        );
    }

    // ── envelope serde round trip through a file ─────────────────────

    /// `ReviewEnvelope` derives `Serialize` only (no `Deserialize`), so a
    /// literal `ReviewEnvelope -> ReviewEnvelope` round trip isn't
    /// expressible. This writes a fully-populated envelope (covering all
    /// three `Tier` variants) to a real file, reads it back, and checks
    /// value-level equality through `serde_json::Value` — the strongest
    /// round-trip check available against the current shape.
    #[test]
    fn envelope_serde_round_trips_through_a_file_with_all_tier_variants() {
        use std::io::Write;

        let flag_confirmed = flag("b1", "member-a", 0, "confirmed charge");
        let flag_needs_check = flag("b1", "member-a", 1, "needs-check charge");
        let flag_archived = flag("b1", "member-a", 2, "archived charge");

        let judged = vec![
            JudgedFlag {
                flag: flag_confirmed.clone(),
                pass1: JudgeRecord {
                    ruling: JudgeRuling::Confirmed,
                    decisive_evidence: "e1".into(),
                    note_for_author: "n1".into(),
                    pass: 1,
                    seconds: 0.5,
                },
                pass2: Some(JudgeRecord {
                    ruling: JudgeRuling::Confirmed,
                    decisive_evidence: "e1b".into(),
                    note_for_author: "n1b".into(),
                    pass: 2,
                    seconds: 0.4,
                }),
                tier: Tier::Confirmed,
                demoted_by_pass2: false,
                verify: None,
                demoted_by_verify: false,
            },
            JudgedFlag {
                flag: flag_needs_check.clone(),
                pass1: JudgeRecord {
                    ruling: JudgeRuling::Confirmed,
                    decisive_evidence: "e2".into(),
                    note_for_author: "n2".into(),
                    pass: 1,
                    seconds: 0.3,
                },
                pass2: Some(JudgeRecord {
                    ruling: JudgeRuling::FalsePositive,
                    decisive_evidence: "e2b".into(),
                    note_for_author: "n2b".into(),
                    pass: 2,
                    seconds: 0.2,
                }),
                tier: Tier::NeedsCheck,
                demoted_by_pass2: true,
                verify: None,
                demoted_by_verify: false,
            },
            JudgedFlag {
                flag: flag_archived.clone(),
                pass1: JudgeRecord {
                    ruling: JudgeRuling::FalsePositive,
                    decisive_evidence: "e3".into(),
                    note_for_author: "n3".into(),
                    pass: 1,
                    seconds: 0.1,
                },
                pass2: None,
                tier: Tier::Archived,
                demoted_by_pass2: false,
                verify: None,
                demoted_by_verify: false,
            },
        ];

        let env = ReviewEnvelope {
            case_id: "case-42".to_string(),
            crew: "test-crew".to_string(),
            mode: "sequential".to_string(),
            members: vec![
                MemberRecord {
                    model: "darkmux:probe-model".to_string(),
                    seat: "review-probe".to_string(),
                    draws: 3,
                    wall_ms: 1200,
                    total_tokens: 900,
                    remote: false,
                    endpoint: None,
                    served_model: None,
                },
                MemberRecord {
                    model: "darkmux:judge-model".to_string(),
                    seat: "review-judge".to_string(),
                    draws: 5,
                    wall_ms: 800,
                    total_tokens: 600,
                    remote: false,
                    endpoint: None,
                    served_model: None,
                },
            ],
            steps: vec![
                StepRecord { step_id: "bundle".to_string(), kind: "procedural".to_string(), items_in: 1, items_out: 1, wall_ms: 2 },
                StepRecord { step_id: "probe".to_string(), kind: "dispatch".to_string(), items_in: 1, items_out: 3, wall_ms: 1200 },
                StepRecord { step_id: "dedup".to_string(), kind: "procedural".to_string(), items_in: 3, items_out: 3, wall_ms: 1 },
                StepRecord { step_id: "judge-pass1".to_string(), kind: "dispatch".to_string(), items_in: 3, items_out: 3, wall_ms: 500 },
                StepRecord { step_id: "judge-pass2".to_string(), kind: "dispatch".to_string(), items_in: 2, items_out: 2, wall_ms: 300 },
            ],
            bundles: 1,
            raw_flags: 3,
            deduped_flags: 3,
            flags: vec![flag_confirmed, flag_needs_check, flag_archived],
            judged,
            confirmed: 1,
            needs_check: 1,
            archived: 1,
            degenerate: None,
                        verified: 0,
            refuted: 0,
fingerprint: fingerprint("darkmux:judge-model", "judge sys"),
            staffing: None,
            warnings: Vec::new(),
            remote_budgets: Vec::new(),
            needs_check_clusters: Vec::new(),
        };

        let json = serde_json::to_string_pretty(&env).expect("serialize");

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("envelope.json");
        {
            let mut f = std::fs::File::create(&path).expect("create");
            f.write_all(json.as_bytes()).expect("write");
        }
        let read_back = std::fs::read_to_string(&path).expect("read");
        let value: serde_json::Value = serde_json::from_str(&read_back).expect("valid json");

        assert_eq!(value["case_id"], "case-42");
        assert_eq!(value["crew"], "test-crew");
        assert_eq!(value["mode"], "sequential");
        assert_eq!(value["bundles"], 1);
        assert_eq!(value["raw_flags"], 3);
        assert_eq!(value["deduped_flags"], 3);
        assert_eq!(value["confirmed"], 1);
        assert_eq!(value["needs_check"], 1);
        assert_eq!(value["archived"], 1);
        assert!(value.get("degenerate").is_none(), "a None degenerate must be omitted, not written as null");
        assert_eq!(value["fingerprint"]["protocol"], "double-confirm-v1");

        let tiers: Vec<String> = value["judged"]
            .as_array()
            .expect("judged array")
            .iter()
            .map(|j| j["tier"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            tiers,
            vec!["confirmed", "needs_check", "archived"],
            "all three Tier variants must survive the file round trip verbatim"
        );

        assert_eq!(value["members"].as_array().unwrap().len(), 2);
        assert_eq!(value["steps"].as_array().unwrap().len(), 5);
        assert_eq!(value["judged"][1]["demoted_by_pass2"], true);
        assert!(value["judged"][2]["pass2"].is_null(), "no pass-2 dispatch serializes pass2 as null, not omitted");
    }

    // ── manifest is dropped from the judge prompt (#1256) ──────────────

    /// `judge-runner.py`'s `judge_one` has no MANIFEST section at all —
    /// `bundler.py`'s bundles carry no such field. The Rust review's
    /// `BundleInput.manifest` is a Rust-only addition; per the "match
    /// Phase A exactly" operator decision it's dropped from the judge
    /// prompt entirely (not silently threaded through) even though the
    /// field itself still exists on `BundleInput` for a future consumer.
    /// Regression-tested at the `run_judge_only` integration level, not a
    /// `judge_prompt` unit test — the function no longer TAKES a manifest
    /// param, so there's nothing left to unit-test at that level; what's
    /// worth guarding is that a populated `BundleInput.manifest` never
    /// leaks into the dispatched prompt.
    #[test]
    fn manifest_never_reaches_the_dispatched_judge_prompt() {
        let crew = valid_crew();
        let bundles = vec![BundleInput {
            id: "billing.ts".to_string(),
            fact_family: "unscoped".to_string(),
            code: "const end = start.plus(30)".to_string(),
            probe_code: "const end = start.plus(30)".to_string(),
            facts: vec![],
            manifest: vec!["helperFn".to_string()],
        }];
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
            bundles: Some(bundles),
        };
        let flags = vec![flag("billing.ts", "member-a", 0, "`const end = start.plus(30)` double-counts")];
        let mut cycler = RecordingCycler::new();
        let seen_prompts = RefCell::new(Vec::new());
        let mut chat = |call: &ChatCall| {
            seen_prompts.borrow_mut().push(call.user.to_string());
            Ok(reply(CONFIRM_JSON))
        };
        let env = run_judge_only(flags, &inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");
        assert_eq!(env.judged.len(), 1);
        let prompts = seen_prompts.borrow();
        // A `confirmed` pass-1 (CONFIRM_JSON) earns a pass-2 (double-confirm
        // judge, module doc) — TWO dispatches over the SAME prompt text, not
        // one. Assert every dispatched prompt, not just the first.
        assert_eq!(prompts.len(), 2, "pass-1 confirmed -> pass-2 also dispatches");
        assert!(
            prompts.iter().all(|p| !p.contains("helperFn")),
            "the bundle's manifest entry must never reach the dispatched judge prompt: {prompts:?}"
        );
        assert!(
            prompts.iter().all(|p| !p.to_lowercase().contains("manifest") && !p.contains("Symbols referenced")),
            "no manifest section header at all, matching judge-runner.py: {prompts:?}"
        );
    }

    // ═══════════════════════════════════════════════════════════════
    // Remote (endpoint-staffed) seats (#1260/#1177) — routing,
    // provenance, per-execution token buckets, failure semantics
    // ═══════════════════════════════════════════════════════════════

    fn remote_pm(id: &str) -> ProfileModel {
        // No `n_ctx` — endpoint models have no local context (#1282). The
        // URL deliberately carries a deployment PATH so provenance tests can
        // prove only the HOST ever serializes.
        ProfileModel {
            id: id.to_string(),
            endpoint: Some(ModelEndpoint {
                url: Some(
                    "https://myorg.cognitiveservices.azure.com/openai/deployments/gpt-51"
                        .to_string(),
                ),
                api_version: Some("2025-01-01-preview".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn remote_staffing(profile: &str, model: &str, k: u32) -> ResolvedSeatStaffing {
        ResolvedSeatStaffing {
            name: profile.to_string(),
            pm: remote_pm(model),
            k,
            passes: 2,
            max_tokens: None,
            selector: None,
        }
    }

    fn bundle_input(id: &str) -> BundleInput {
        BundleInput {
            id: id.to_string(),
            fact_family: "unscoped".to_string(),
            code: "const x = 1".to_string(),
            probe_code: "const x = 1".to_string(),
            facts: vec![],
            manifest: vec![],
        }
    }

    // ═══════════════════════════════════════════════════════════════
    // The review-verify seat (#1260/#1177) — optional adjudication stage
    // ═══════════════════════════════════════════════════════════════

    const VERIFIED_JSON: &str = "```json\n{\"ruling\": \"verified\", \"decisive_evidence\": \"ve\", \"note_for_author\": \"vn\"}\n```";
    const REFUTED_JSON: &str = "```json\n{\"ruling\": \"refuted\", \"decisive_evidence\": \"re\", \"note_for_author\": \"rn\"}\n```";
    const UNCERTAIN_JSON: &str = "```json\n{\"ruling\": \"uncertain\", \"decisive_evidence\": \"ue\", \"note_for_author\": \"un\"}\n```";

    /// (contract 6) Byte-lock for the verify prompt — the full assembled
    /// string, mirroring `judge_prompt_matches_phase_a_golden_*`. The
    /// evidence sections are the judge's exact assembly (one shared
    /// implementation, `review_prompt_with_tail`); only the frozen tail
    /// differs, and this golden pins every byte of it.
    #[test]
    fn verify_prompt_matches_frozen_golden() {
        let golden = "## The author's stated case (the pull request description)\nBound retry backoff to a sane ceiling\nCaps the exponential backoff delay so a large attempt count cannot stall retries indefinitely.\n\n## The code under review\n```typescript\n// src/example.ts (lines 1-4)\nexport function clampRetryDelay(attempt: number, base: number): number {\n  const delay = base * Math.pow(2, attempt);\n  return Math.min(delay, 30000);\n}\n```\n\n## Fact sheet given to the flagging reviewer\n`attempt` is caller-controlled and unbounded\n\n## The flagged item to investigate\nThe delay calculation in `clampRetryDelay` never verifies `attempt` is non-negative — a negative attempt shrinks the delay below the intended floor.\n\nAdjudicate the confirmed finding against the code above. End your reply with exactly one fenced JSON block:\n```json\n{\"ruling\": \"verified\" | \"refuted\" | \"uncertain\", \"decisive_evidence\": \"<the specific code line or checked claim that decided it>\", \"note_for_author\": \"<one or two sentences the author reads>\"}\n```";
        let p = verify_prompt(
            "Bound retry backoff to a sane ceiling",
            "Caps the exponential backoff delay so a large attempt count cannot stall retries indefinitely.",
            GOLDEN_CODE,
            &["`attempt` is caller-controlled and unbounded".to_string()],
            "The delay calculation in `clampRetryDelay` never verifies `attempt` is non-negative — a negative attempt shrinks the delay below the intended floor.",
        );
        assert_eq!(p, golden);
    }

    #[test]
    fn parse_verify_ruling_vocabulary_and_rejections() {
        let (r, e, n) = parse_verify_ruling(VERIFIED_JSON).expect("parses");
        assert_eq!(r, VerifyRuling::Verified);
        assert_eq!((e.as_str(), n.as_str()), ("ve", "vn"));
        assert_eq!(parse_verify_ruling(REFUTED_JSON).unwrap().0, VerifyRuling::Refuted);
        assert_eq!(parse_verify_ruling(UNCERTAIN_JSON).unwrap().0, VerifyRuling::Uncertain);
        // Case-insensitive + trimmed, same as the judge parser.
        let upper = "```json\n{\"ruling\": \" VERIFIED \", \"decisive_evidence\": \"e\", \"note_for_author\": \"n\"}\n```";
        assert_eq!(parse_verify_ruling(upper).unwrap().0, VerifyRuling::Verified);
        // The JUDGE vocabulary is NOT the verify vocabulary — a verify seat
        // answering "confirmed" is off-contract and must read as Unparsed.
        assert!(parse_verify_ruling(CONFIRM_JSON).is_none());
        assert!(parse_verify_ruling("no verdict here").is_none());
    }

    /// The verify seat's staffing shape is validated like the judge's:
    /// exactly one staffing when declared; absent is fine (optional seat).
    #[test]
    fn validate_review_crew_verify_seat_shape() {
        let ok = crew_with(vec![
            ("review-probe", vec![staffing("fast", "a", 1)]),
            ("review-judge", vec![staffing("fast", "b", 1)]),
            ("review-verify", vec![staffing("frontier", "c", 1)]),
        ]);
        let seats = validate_review_crew(&ok).expect("verify seat accepted");
        assert!(seats.verify.is_some());

        let absent = valid_crew();
        assert!(validate_review_crew(&absent).expect("optional").verify.is_none());

        let two = crew_with(vec![
            ("review-probe", vec![staffing("fast", "a", 1)]),
            ("review-judge", vec![staffing("fast", "b", 1)]),
            ("review-verify", vec![staffing("frontier", "c", 1), staffing("frontier", "d", 1)]),
        ]);
        let err = validate_review_crew(&two).unwrap_err().to_string();
        assert!(err.contains("review-verify"), "{err}");
        assert!(err.contains("EXACTLY 1"), "{err}");
    }

    // ═══════════════════════════════════════════════════════════════
    // Review-round fixes (#1260) — bill every attempt, stage-scoped verify
    // degradation, remote-judge honest-fail, reasoning-aware floor
    // ═══════════════════════════════════════════════════════════════

    /// (FIX 5) Reasoning-aware completion floor: a REMOTE seat with NO
    /// explicit staffing `max_tokens` floors at 16384 (never the local-tuned
    /// probe default of 4000 — the reasoning-guillotine class); an explicit
    /// staffing `max_tokens` always wins verbatim; the floor never LOWERS an
    /// already-higher local default; LOCAL seats are unaffected.
    #[test]
    fn resolve_seat_max_tokens_remote_reasoning_floor() {
        let local = staffing("fast", "m", 1);
        assert_eq!(resolve_seat_max_tokens(&local, DEFAULT_PROBE_MAX_TOKENS), DEFAULT_PROBE_MAX_TOKENS);

        let remote = remote_staffing("cloud", "gpt", 1); // max_tokens: None
        assert_eq!(
            resolve_seat_max_tokens(&remote, DEFAULT_PROBE_MAX_TOKENS),
            REMOTE_REASONING_MAX_TOKENS_FLOOR,
            "a remote probe seat floors at 16384, not the 4000 local default"
        );
        assert_eq!(
            resolve_seat_max_tokens(&remote, DEFAULT_JUDGE_MAX_TOKENS),
            DEFAULT_JUDGE_MAX_TOKENS,
            "the floor never lowers an already-higher local default (a floor, not a clamp)"
        );

        let mut remote_explicit = remote_staffing("cloud", "gpt", 1);
        remote_explicit.max_tokens = Some(500);
        assert_eq!(
            resolve_seat_max_tokens(&remote_explicit, DEFAULT_PROBE_MAX_TOKENS),
            500,
            "an explicit staffing max_tokens always wins verbatim (operator sovereignty)"
        );
    }

    /// (CONSIDER c) `RemoteBucket::exhausted()` boundary: under < at == over.
    /// A mutation of `>=` to `>` must fail this table (the `at` row).
    #[test]
    fn remote_bucket_exhausted_boundary_table() {
        let mut under = RemoteBucket::new("s", 100);
        under.spend(99, 1);
        assert!(!under.exhausted(), "under budget: 99 < 100");

        let mut at = RemoteBucket::new("s", 100);
        at.spend(100, 1);
        assert!(at.exhausted(), "at budget: 100 >= 100 (a `>` mutation breaks here)");

        let mut over = RemoteBucket::new("s", 100);
        over.spend(101, 1);
        assert!(over.exhausted(), "over budget: 101 >= 100");
    }

    // ─── (#1230/#1341 DRY pass) Task/Step graph orchestration ───────────

    fn step_ctx(crew: &ResolvedCrew, bundles: Vec<BundleInput>) -> Arc<ReviewStepContext> {
        Arc::new(ReviewStepContext {
            case_id: "case-1".to_string(),
            crew: crew.clone(),
            intent_title: String::new(),
            intent_body: String::new(),
            diff: DIFF.to_string(),
            probe_system: "probe prior".to_string(),
            judge_system: "judge persona".to_string(),
            verify_system: "verify persona".to_string(),
            bundles,
            remote_max_tokens_per_execution: 500_000,
            timeout_seconds: 30,
            chat_override: None,
        })
    }

    /// `staffing()`'s graph-test twin: a LOCAL seat whose `ProfileModel`
    /// carries NO `n_ctx`. Every `StepKind::residency()` in this module
    /// (probe/judge/verify) reports `None` — i.e. `Residency::Remote` —
    /// whenever `n_ctx` is absent, exactly like a genuinely-remote seat.
    /// `run_bounded`'s Remote track never touches `host_factory` (the real
    /// `lms` CLI) at all, so a `run_review_graph` test built from these
    /// fixtures stays hermetic even with NON-EMPTY bundles — the whole
    /// point of the `chat_override` seam (#1355) is to exercise real
    /// dispatch-shaped step kinds without a live LMStudio, and a
    /// `Residency::Local` job would silently reach for one via
    /// `ensure_wave_loaded`. Production always sets `n_ctx` from the
    /// resolved profile; the missing `n_ctx` here is a deliberate
    /// test-only choice, not a shape a real profile would have.
    fn graph_pm(id: &str) -> ProfileModel {
        ProfileModel { id: id.to_string(), ..Default::default() }
    }
    fn graph_staffing(profile: &str, model: &str, k: u32) -> ResolvedSeatStaffing {
        ResolvedSeatStaffing {
            name: profile.to_string(),
            pm: graph_pm(model),
            k,
            passes: 2,
            max_tokens: None,
            selector: None,
        }
    }

    /// A crew of `graph_staffing` seats — the graph-hermetic equivalent of
    /// `valid_crew()`.
    fn graph_valid_crew() -> ResolvedCrew {
        crew_with(vec![
            ("review-probe", vec![graph_staffing("fast", "probe-model", 2)]),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
        ])
    }

    /// [`step_ctx`] with a mocked dispatch installed via the `chat_override`
    /// seam (#1355) — the graph-path analog of `run_review`'s injected
    /// `chat: &mut dyn FnMut` parameter. `chat_fn` must be `Send + Sync +
    /// 'static`: the graph's step kinds hold `Arc<ReviewStepContext>` and
    /// dispatch from inside `run_bounded`'s worker threads, not the calling
    /// thread — a plain `&mut dyn FnMut` (like `run_review`'s own seam)
    /// can't cross that boundary, which is exactly why `dispatch_chat`'s
    /// seam is an `Arc<dyn Fn + Send + Sync>` instead.
    fn step_ctx_with_chat(
        crew: &ResolvedCrew,
        bundles: Vec<BundleInput>,
        chat_fn: impl Fn(&ChatCall) -> Result<SingleShotReply> + Send + Sync + 'static,
    ) -> Arc<ReviewStepContext> {
        step_ctx_with_chat_and_budget(crew, bundles, 500_000, chat_fn)
    }

    /// [`step_ctx_with_chat`] with a caller-chosen remote per-execution
    /// token budget — for the budget-exhaustion tests below.
    fn step_ctx_with_chat_and_budget(
        crew: &ResolvedCrew,
        bundles: Vec<BundleInput>,
        remote_max_tokens_per_execution: u64,
        chat_fn: impl Fn(&ChatCall) -> Result<SingleShotReply> + Send + Sync + 'static,
    ) -> Arc<ReviewStepContext> {
        Arc::new(ReviewStepContext {
            case_id: "case-1".to_string(),
            crew: crew.clone(),
            intent_title: String::new(),
            intent_body: String::new(),
            diff: DIFF.to_string(),
            probe_system: "probe prior".to_string(),
            judge_system: "judge persona".to_string(),
            verify_system: "verify persona".to_string(),
            bundles,
            remote_max_tokens_per_execution,
            timeout_seconds: 30,
            chat_override: Some(Arc::new(chat_fn)),
        })
    }

    /// Build + run the graph in one call — the shared convenience wrapper
    /// every migrated `run_review_graph` test below uses, mirroring
    /// `run_review`'s single-call shape as closely as the graph API allows
    /// (`run_graph(&ctx, &mut emitter)` vs `run_review(&inputs, chat,
    /// cycler, emitter)`). `judge_concurrency: 1` is byte-identical dispatch
    /// ORDER to the historical sequential judge loop, per
    /// `build_review_graph`'s own doc.
    fn run_graph(ctx: &Arc<ReviewStepContext>, emitter: &mut dyn ReviewEmitter) -> Result<ReviewEnvelope> {
        let seats = validate_review_crew(&ctx.crew)?;
        let judge = seats.judge.clone();
        let verify = seats.verify.cloned();
        let probes: Vec<_> = seats.probes.clone();
        let fingerprint_val = fingerprint(&seat_identifier(&judge.pm), &ctx.judge_system);
        let staffing_snap = staffing_snapshot(&probes, &judge, verify.as_ref(), ctx.crew.request_changes);
        let graph =
            build_review_graph(ctx.clone(), judge, verify, &probes, "investigate", "adjudicate", "report", 1)?;
        let (env, _steps) = run_review_graph(
            ctx,
            &ctx.crew.name,
            ExecMode::Sequential,
            fingerprint_val,
            staffing_snap,
            graph,
            emitter,
            &mut |_step| {},
        )?;
        Ok(env)
    }

    /// The graph's SHAPE is fully knowable upfront (the redesign's whole
    /// point): three Phases, `depends_on` edges crossing Phase boundaries
    /// exactly like they cross Task boundaries within one, and every Step
    /// resolvable through the registry `build_review_graph` also builds.
    /// Pure structural assertion — no dispatch, no network.
    #[test]
    fn build_review_graph_has_three_phases_and_correct_dependencies() {
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model-a", 1), staffing("slow", "probe-model-b", 1)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ]);
        let seats = validate_review_crew(&crew).expect("valid crew");
        let ctx = step_ctx(&crew, vec![]);

        let graph = build_review_graph(
            ctx,
            seats.judge.clone(),
            seats.verify.cloned(),
            seats.probes,
            "investigate",
            "adjudicate",
            "report",
            1,
        )
        .expect("built-in review config builds cleanly");

        // bundle(1) + probe(2 seats) + dedup(1) = investigate's 4 tasks.
        let investigate_tasks: Vec<_> = graph.tasks.iter().filter(|t| t.phase_id == "investigate").collect();
        assert_eq!(investigate_tasks.len(), 4, "bundle + 2 probe seats + dedup");
        let adjudicate_tasks: Vec<_> = graph.tasks.iter().filter(|t| t.phase_id == "adjudicate").collect();
        assert_eq!(adjudicate_tasks.len(), 1, "judge only");
        let report_tasks: Vec<_> = graph.tasks.iter().filter(|t| t.phase_id == "report").collect();
        assert_eq!(report_tasks.len(), 2, "verify + synthesis");

        // (#1341) Cross-phase dependency now lives on `Task.depends_on`
        // (Steps have none of their own) — adjudicate's judge TASK depends
        // on investigate's dedup TASK, no special cross-phase mechanism.
        let tasks_by_id: std::collections::BTreeMap<&str, &Task> =
            graph.tasks.iter().map(|t| (t.id.as_str(), t)).collect();
        let dedup_step_id = "review-dedup-step";
        let judge_task = tasks_by_id["review-judge-task"];
        assert_eq!(judge_task.depends_on, vec!["review-dedup-task".to_string()]);
        assert_eq!(graph.phase_id_of_step[dedup_step_id], "investigate");
        assert_eq!(graph.phase_id_of_step["review-judge-step"], "adjudicate");

        // report's synthesis TASK depends on BOTH dedup (investigate) and
        // verify (report) — graph-native cross-phase data flow, not a side
        // channel.
        let synth_task = tasks_by_id["review-synthesis-task"];
        assert!(synth_task.depends_on.contains(&"review-dedup-task".to_string()));
        assert!(synth_task.depends_on.contains(&"review-verify-task".to_string()));
        assert_eq!(graph.phase_id_of_step["review-synthesis-step"], "report");

        // Every step's `kind` resolves through the SAME registry — the
        // scheduler contract this whole redesign hangs on.
        for step in graph.steps.values() {
            assert!(graph.registry.get(&step.kind).is_ok(), "step `{}` kind `{}` must resolve", step.id, step.kind);
        }

        // (#1349) The pre-rename `funnel.*` kind ids also resolve — a
        // `Step.kind` persisted before this rename shipped must not become
        // "unknown step kind" if anything ever re-reads it back through a
        // fresh registry (see `StepKindRegistry::register_alias`'s doc).
        for legacy in [
            "funnel.bundle",
            "funnel.probe:fast",
            "funnel.probe:slow",
            "funnel.dedup",
            "funnel.judge",
            "funnel.verify",
            "funnel.synthesis",
        ] {
            assert!(graph.registry.get(legacy).is_ok(), "legacy kind id `{legacy}` must still resolve");
        }

        // ONE call is the whole point: no separate driver loop needed to
        // reach every step — `depends_on` alone determines readiness.
        assert_eq!(graph.steps.len(), 7, "bundle + 2 probe + dedup + judge + verify + synthesis");
    }

    /// (#1402) Pins `review_step_kind_display_name` (the pure lookup
    /// `darkmux-serve`'s `mission_graph` module calls, since it can't
    /// construct a live `StepKind` instance from a persisted Step alone)
    /// against the REAL `StepKind::display_name()` every registered kind
    /// returns — the "conformance test in a crate that sees both" #1352's
    /// tiering doctrine asks for instead of unguarded duplication.
    #[test]
    fn review_step_kind_display_names_match_the_live_impls() {
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model-a", 1)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ]);
        let seats = validate_review_crew(&crew).expect("valid crew");
        let ctx = step_ctx(&crew, vec![]);
        let graph = build_review_graph(
            ctx,
            seats.judge.clone(),
            seats.verify.cloned(),
            seats.probes,
            "investigate",
            "adjudicate",
            "report",
            1,
        )
        .expect("built-in review config builds cleanly");

        for step in graph.steps.values() {
            let live = graph.registry.get(&step.kind).expect("every step kind resolves");
            let pure = review_step_kind_display_name(&step.kind);
            assert_eq!(
                pure,
                Some(live.display_name()),
                "review_step_kind_display_name(\"{}\") must match the live impl's display_name()",
                step.kind
            );
        }
    }

    /// (#1284 Packet 3, review round 2 MUST FIX 2) LAUNCHER-LEVEL
    /// conformance golden: the full serialized `(tasks, steps)` this
    /// launcher produces for a THREE-probe-seat crew (production
    /// review-deep's count) must be byte-equal (as JSON values) to the
    /// output CAPTURED FROM MAIN's pre-cutover hand-built
    /// `build_review_graph` — every task id, `phase_id`, description
    /// (em dashes included — the exact axis round 1's untouched tests
    /// missed), step id, `depends_on` set, kind id, `Step.config` payload,
    /// and `Vec<Task>` ORDER (a JSON array pins order under `Value`
    /// equality). The golden file was generated by running main's builder
    /// itself (commit c802f87, a temporary in-tree dump test) with these
    /// EXACT inputs, not transcribed by hand. Composed phase ids that
    /// differ from the document's own phase ids are deliberate — they pin
    /// that review's task/step ids are FIXED (no placeholder-prefix
    /// substitution applies to them). `judge_concurrency: 3` (non-default)
    /// pins the operator override into `Step.config`.
    ///
    /// (#1432 item 3) The golden also now pins each task's `display_name`
    /// (Bundle / Probe {index} / Dedup / Judge / Verify / Synthesis) — the
    /// phone-facing labels the config gained beyond main's pre-cutover
    /// output, threaded through `interpret` (task `display_name` +
    /// the probe expansion's `display_name_pattern`).
    #[test]
    fn build_review_graph_matches_the_pre_cutover_golden_exactly() {
        let crew = crew_with(vec![
            ("alpha", vec![staffing("alpha", "probe-model-a", 1)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ]);
        let probes = vec![
            staffing("alpha", "probe-model-a", 1),
            staffing("bravo", "probe-model-b", 2),
            staffing("charlie", "probe-model-c", 1),
        ];
        let judge = staffing("fast", "judge-model", 1);
        let ctx = step_ctx(&crew, vec![]);

        let graph = build_review_graph(
            ctx,
            judge,
            None,
            &probes,
            "pr-review-golden-investigate",
            "pr-review-golden-adjudicate",
            "pr-review-golden-report",
            3,
        )
        .expect("built-in review config builds cleanly");

        let actual = serde_json::json!({"tasks": graph.tasks, "steps": graph.steps});
        let golden: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/review_graph_3seat.json"
        )))
        .expect("golden parses");
        assert_eq!(
            actual,
            golden,
            "interpreted review graph diverged from main's pre-cutover builder output:\n{}",
            serde_json::to_string_pretty(&actual).unwrap()
        );
    }

    /// End-to-end through the REAL scheduler (`run_step_graph`, one call —
    /// see the module doc) with an EMPTY bundle set: every dispatch-shaped
    /// step (probe/judge/verify) iterates zero items and makes ZERO chat
    /// calls (probe's `select_bundles_for_staffing` returns empty; judge's
    /// deduped list is empty; verify's confirmed docket is empty) — so this
    /// exercises the full graph, all three Phases, without a live LMStudio
    /// or network. Confirms the degenerate reason ends up in the FINAL
    /// envelope regardless of which stage would have detected it.
    #[test]
    fn run_review_graph_with_empty_bundles_completes_with_zero_dispatches() {
        let crew = valid_crew();
        let seats = validate_review_crew(&crew).expect("valid crew");
        let judge = seats.judge.clone();
        let verify = seats.verify.cloned();
        let probes: Vec<_> = seats.probes.clone();
        let ctx = step_ctx(&crew, vec![]);

        let graph = build_review_graph(ctx.clone(), judge.clone(), verify.clone(), &probes, "investigate", "adjudicate", "report", 1)
            .expect("built-in review config builds cleanly");
        let fingerprint_val = fingerprint(&seat_identifier(&judge.pm), &ctx.judge_system);
        let staffing_snap = staffing_snapshot(&probes, &judge, verify.as_ref(), false);

        let mut emitter = RecordingEmitter::new();
        let (env, steps) = run_review_graph(
            &ctx,
            "test-crew",
            ExecMode::Sequential,
            fingerprint_val,
            staffing_snap,
            graph,
            &mut emitter,
            &mut |_step| {},
        )
        .expect("graph run completes even with zero bundles");

        assert_eq!(env.bundles, 0);
        assert_eq!(env.deduped_flags, 0);
        assert_eq!(env.confirmed, 0);
        assert_eq!(env.needs_check, 0);
        // (#1355) The "no bundles produced from the diff" degenerate reason
        // — the FIRST of #1355's two follow-up gates — must actually be the
        // reason named on a zero-bundle run, not just SOME degenerate
        // reason (a zero-flags reason winning here would be equally
        // "degenerate" but the wrong diagnosis for the operator).
        assert_eq!(env.degenerate.as_deref(), Some("no bundles produced from the diff"));
        // Every declared step reached a terminal status — the graph never
        // stalls on a "ready but never scheduled" node.
        for step in steps.values() {
            assert!(
                matches!(step.status, NodeStatus::Complete | NodeStatus::Error),
                "step `{}` (kind `{}`) must reach a terminal status, got {:?}",
                step.id,
                step.kind,
                step.status
            );
        }
        // The scheduler's own generic step-lifecycle bookends fired for
        // every step (free observability — see the module doc).
        let starts = emitter.records.iter().filter(|r| r.action == "step start").count();
        assert_eq!(starts, steps.len(), "every declared step got a lifecycle start record");
        // (#1399) The terminal bookend (complete OR error) fired for every
        // step too — zero step start/complete records was the exact bug
        // #1399 found live: the review path's own `step result` records
        // are a SUPPLEMENT to this vocabulary, never a replacement for it.
        let terminals = emitter
            .records
            .iter()
            .filter(|r| r.action == "step complete" || r.action == "step error")
            .count();
        assert_eq!(terminals, steps.len(), "every declared step got a terminal lifecycle record");
        // (#1399) Every step-lifecycle action this path emits is drawn from
        // the SAME canonical vocabulary constant the crew scheduler's own
        // conformance test asserts against — the two execution paths
        // (generic scheduler, review's Tier-3 driver) cannot silently grow
        // a competing vocabulary.
        for record in emitter.records.iter().filter(|r| r.action.starts_with("step ")) {
            assert!(
                STEP_LIFECYCLE_ACTIONS.contains(&record.action.as_str()) || record.action == "step result",
                "review path emitted a step-scoped action outside the canonical lifecycle \
                 vocabulary or the documented `step result` companion: {}",
                record.action
            );
        }
        // (#1349) `run_review_graph` itself must emit NO task-level bookend
        // at all — that liveness edge belongs entirely to the caller's
        // `with_dispatch_bookends` wrap (`src/pr_review.rs`), which brackets
        // the WHOLE call in the canonical `dispatch start`/`dispatch
        // complete` record. A `review.task` (or any `dispatch *`) record
        // emitted from inside this function would be the exact redundant,
        // competing-vocabulary bug #1349 retired.
        assert!(
            emitter.records.iter().all(|r| r.action != "review.task" && !r.action.starts_with("dispatch ")),
            "run_review_graph must not emit its own task-level bookend: {:?}",
            emitter.records.iter().map(|r| r.action.as_str()).collect::<Vec<_>>()
        );
    }

    /// (#1397) The review pipeline runs through the SAME `run_step_graph`
    /// call `coder_phase.rs`/`mission_launch.rs` use, so it gets the
    /// identical transition-time persistence hook — proven here the same
    /// way the crew scheduler's own `run_step_graph_persists_running_
    /// before_the_step_completes` test proves it: a `persist` closure that
    /// snapshots (clones) every step it's handed shows the FIRST recorded
    /// snapshot per step is already `Running` (not the pre-run `Planned`),
    /// and the LAST is terminal. This is what makes a `mission launch
    /// review` dispatch's mid-run graph page truthful instead of blind
    /// until the whole run finishes (composes with #1399 — the flow-record
    /// half of the same fix).
    #[test]
    fn run_review_graph_persists_running_before_terminal_for_every_step() {
        let crew = valid_crew();
        let seats = validate_review_crew(&crew).expect("valid crew");
        let judge = seats.judge.clone();
        let verify = seats.verify.cloned();
        let probes: Vec<_> = seats.probes.clone();
        let ctx = step_ctx(&crew, vec![]);

        let graph = build_review_graph(ctx.clone(), judge.clone(), verify.clone(), &probes, "investigate", "adjudicate", "report", 1)
            .expect("built-in review config builds cleanly");
        let fingerprint_val = fingerprint(&seat_identifier(&judge.pm), &ctx.judge_system);
        let staffing_snap = staffing_snapshot(&probes, &judge, verify.as_ref(), false);

        let mut emitter = RecordingEmitter::new();
        let mut persisted: Vec<Step> = Vec::new();
        let (_env, steps) = run_review_graph(
            &ctx,
            "test-crew",
            ExecMode::Sequential,
            fingerprint_val,
            staffing_snap,
            graph,
            &mut emitter,
            &mut |step| persisted.push(step.clone()),
        )
        .expect("graph run completes even with zero bundles");

        assert_eq!(
            persisted.len(),
            steps.len() * 2,
            "one Running persist + one terminal persist per step: {persisted:?}"
        );
        for step_id in steps.keys() {
            let mut snapshots = persisted.iter().filter(|s| &s.id == step_id);
            let first = snapshots.next().expect("at least one persisted snapshot per step");
            assert_eq!(first.status, NodeStatus::Running, "step `{step_id}`'s first persisted snapshot must be Running");
            let last = snapshots.next_back().unwrap_or(first);
            assert!(
                matches!(last.status, NodeStatus::Complete | NodeStatus::Error),
                "step `{step_id}`'s last persisted snapshot must be terminal, got {:?}",
                last.status
            );
        }
    }

    // ═══════════════════════════════════════════════════════════════
    // #1355/#1357: dispatch-level `run_review_graph` coverage
    //
    // Everything below drives `run_graph` (build + run the graph in one
    // call, using the `chat_override` seam added on `ReviewStepContext`)
    // instead of the deleted `run_review`/`run_review_impl`. Each test
    // preserves the INTENT of the `run_review`-driven test it replaces —
    // named in its own doc comment — rather than the literal old shape,
    // since the graph's observability surface (flow-record vocabulary,
    // `env.steps`, per-seat cycling order) genuinely differs from the old
    // sequential driver's. `graph_staffing`/`graph_valid_crew` (no `n_ctx`)
    // keep every dispatch on `Residency::Remote` so these tests never touch
    // the real `lms` CLI even with non-empty bundles — see `graph_pm`'s doc.
    //
    // ── #1357: tests retired outright (no graph-path equivalent needed) ──
    //
    // The following `run_review`-driven tests are DELETED, not migrated,
    // because what they locked down is either a mechanism the graph path
    // genuinely doesn't have, or vocabulary #1349 already retired. Listed
    // here (rather than left as dead stub functions) per #1357's own audit
    // requirement — each line names what was deleted and why no graph-path
    // equivalent is needed:
    //
    // - `sequential_cycling_loads_and_releases_each_member_before_the_next_
    //   then_judge_last` — asserted `ModelCycler` load/release ORDER, a
    //   mechanism unique to the old driver. The graph path loads local
    //   models through gestalt's wave planner instead (`ensure_wave_loaded`
    //   in `concurrent_dispatch.rs`, tested there and in `darkmux-gestalt`),
    //   which has no "cycler" abstraction and a different (co-residency,
    //   not strict per-seat sequential) loading model.
    // - `probe_phase_sequential_load_failure_aborts_remaining_members_and_
    //   drops_prior_flags` / `probe_phase_parallel_load_failure_aborts_
    //   before_any_dispatch` — directly called `probe_phase`, deleted
    //   alongside `run_review_impl` (its only caller, per #1357). Model-load
    //   failure handling is gestalt's `ensure_wave_loaded`/`plan_acquire`
    //   now, already covered in `concurrent_dispatch.rs`'s own tests.
    // - `bookend_guard_judge_release_failure_closes_judge_pass1_and_task` —
    //   exercised a `ModelCycler::release` failure; no `ModelCycler` in the
    //   graph's dispatch path at all (loading is gestalt's job).
    // - `flow_emission_records_the_expected_action_sequence_for_a_healthy_
    //   run` / `flow_emission_degenerate_zero_bundles_emits_only_task_and_
    //   bundle_step` / `bookend_guard_probe_dispatch_error_closes_open_
    //   steps_and_emits_terminal_task_record` / `bookend_guard_chat_error_
    //   mid_judge_docket_still_yields_terminal_task_record` — asserted the
    //   old driver's bespoke `review.task`/`review.step`/`review.ruling`
    //   emission vocabulary (`ReviewRunGuard`). #1349 retired that
    //   vocabulary from the graph path entirely: `run_review_graph` emits
    //   ONLY the scheduler's generic `step start`/`step complete`/`step
    //   error` records (already covered by `run_review_graph_with_empty_
    //   bundles_completes_with_zero_dispatches`, which now also pins the
    //   zero-bundle degenerate reason text) plus this module's own
    //   `emit_review_step_result` ("step result") records — the former
    //   `review.task` bookend now lives entirely in `src/pr_review.rs`'s
    //   `with_dispatch_bookends` wrap (see `run_review_graph`'s own doc).
    //   The GENUINE behavioral intent behind the two bookend-guard tests —
    //   a probe/judge dispatch error reaches a clean terminal envelope
    //   rather than hanging or panicking — is re-covered below by
    //   `probe_dispatch_error_reaches_a_terminal_degenerate_envelope` and
    //   `judge_dispatch_errors_are_swallowed_per_flag_not_aborted`.
    // - `flow_emission_includes_host_telemetry_when_sampler_cadence_is_
    //   fast` / `flow_emission_includes_lms_telemetry_when_sampler_cadence_
    //   is_fast` — exercised `run_review_with_telemetry`'s injectable
    //   `sample_fn`/`lms_fn` seam. `run_review_graph` hardcodes the real
    //   `sample_host`/`darkmux_profiles::lms::list_loaded` (adding an
    //   equivalent seam there is out of THIS packet's scope — see
    //   `ReviewStepContext::chat_override`'s own doc) at the PRODUCTION
    //   2-second cadence, which `HostTelemetrySampler::start`'s own doc
    //   explains is deliberately impossible to race a sub-millisecond
    //   mocked test into. `host_telemetry_sampler_stops_and_joins_promptly_
    //   on_drop` already covers the sampler's own inject-and-stop mechanism
    //   directly; the real graph-path integration is a live-dogfood concern
    //   (this repo's release-gate discipline), not a unit test.
    // - `step_telemetry_probe_wall_ms_encompasses_member_wall_ms` /
    //   `step_telemetry_judge_steps_sum_equals_judge_member_wall_ms` —
    //   asserted on `ReviewEnvelope.steps` (`Vec<StepRecord>`), which only
    //   the old driver (`finish_review`) ever populates; no graph step kind
    //   writes to it, so it stays empty end-to-end on the graph path. Timing
    //   observability now lives in the flow-record stream
    //   (`emit_review_step_result`'s `wall_ms` fields) instead.
    // - `remote_tokens_bookend_present_when_remote_absent_when_local` —
    //   asserted the old `review.task` bookend's `remote_tokens` field,
    //   which now lives in `src/pr_review.rs`'s `with_dispatch_bookends`
    //   payload (outside this module's crate boundary — see
    //   `run_review_graph`'s doc); an equivalent belongs in
    //   `src/pr_review.rs`'s own test suite, not here.
    //
    // ── a real, distinct gap found DURING the #1355/#1357 migration ─────
    //
    // FIXED by #1284 Packet 2 (#1373). `finish_review` (still alive via
    // `run_judge_only`) applied judge/verify remote-budget honesty gates
    // that `ReviewJudgeStepKind`/`ReviewVerifyStepKind`/
    // `ReviewSynthesisStepKind` did NOT reproduce — a judge/verify remote
    // bucket's exhaustion never reached `env.remote_budgets`, a
    // fully-exhausted judge bucket didn't degrade the run when at least
    // one flag got a real ruling first, a partial judge dispatch failure's
    // warning never reached `env.warnings`, and verify never skipped on an
    // already-doomed judge stage. Ported onto the step kinds via two
    // shared helpers (`judge_gate_outcome`, `verify_budget_outcome`) both
    // `finish_review` and the graph path now call, plus a `SharedReviewEnvelope`
    // handle threaded onto `ReviewDedupStepKind`/`ReviewJudgeStepKind`/
    // `ReviewVerifyStepKind` (see each kind's own doc). The tests below
    // — `graph_remote_judge_budget_exhaustion_is_an_honest_degraded_run`,
    // `graph_remote_verify_budget_exhaustion_degrades_the_stage_not_the_
    // run`, `graph_verify_stage_skipped_when_judge_already_degraded`, plus
    // the raw_flags (gate e) and minority-warning (gate c) pins elsewhere
    // in this module — were CHARACTERIZATION tests of the gap; they now
    // pin the FIXED (positive) behavior instead.

    /// Migrates `envelope_counts_and_steps_are_internally_consistent`'s
    /// INTENT (tier/count internal consistency) minus its `env.steps`
    /// assertions, which have no graph-path equivalent (see the retirement
    /// note above `step_telemetry_*`).
    #[test]
    fn graph_envelope_counts_are_internally_consistent() {
        let crew = graph_valid_crew();
        let call_n = std::sync::atomic::AtomicU32::new(0);
        let ctx = step_ctx_with_chat(&crew, bundles_from_diff(DIFF), move |_call: &ChatCall| {
            let n = call_n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < 2 {
                // two probe draws (k=2), both find the same defect
                Ok(reply("a real defect `const end = start.plus(30)`"))
            } else {
                Ok(reply(CONFIRM_JSON))
            }
        });
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");
        assert!(env.degenerate.is_none());
        assert_eq!(env.bundles, 1, "one changed file in the fixture diff");
        // (#1373 gate e, FIXED) `ReviewDedupStepKind` now writes the TRUE
        // pre-dedup count (2 draws here) into the shared envelope the
        // moment it's known, so `raw_flags` and `deduped_flags` diverge
        // again on the graph path — the "N raw collapsed to M"
        // observability signal the field name promises.
        assert_eq!(
            env.raw_flags, 2,
            "env.raw_flags reads the true pre-dedup draw count, not the deduped count"
        );
        assert_eq!(env.deduped_flags, 1, "identical anchor+family collapses to one");
        assert_eq!(env.flags.len(), env.deduped_flags);
        assert_eq!(env.judged.len(), env.deduped_flags);
        assert_eq!(
            env.confirmed + env.needs_check + env.archived,
            env.judged.len(),
            "every judged flag lands in exactly one tier"
        );
        assert!(!env.members.is_empty(), "probe + judge attribution present (#1355)");
        assert!(env.fingerprint.get("protocol").is_some());
    }

    /// Migrates the GENUINE behavioral intent of
    /// `bookend_guard_probe_dispatch_error_closes_open_steps_and_emits_
    /// terminal_task_record` (the old bespoke vocabulary it also asserted
    /// on is retired — see the note above): a LOCAL probe seat's dispatch
    /// error must not hang or panic the graph run — `run_review_graph`
    /// still returns `Ok`, with the failure named in `env.degenerate`, and
    /// `run_step_graph` marks the probe step (and everything downstream)
    /// terminal rather than dangling `Running` forever.
    #[test]
    fn probe_dispatch_error_reaches_a_terminal_degenerate_envelope() {
        let crew = graph_valid_crew();
        let ctx = step_ctx_with_chat(&crew, bundles_from_diff(DIFF), |_call: &ChatCall| -> Result<SingleShotReply> {
            Err(anyhow!("network down"))
        });
        let env = run_graph(&ctx, &mut NullEmitter)
            .expect("run_review_graph always returns Ok, even when a step errors");
        assert!(env.degenerate.is_some(), "a hard probe dispatch failure must be named, never silent");
        assert!(
            env.degenerate.as_deref().unwrap().contains("errored"),
            "got: {:?}",
            env.degenerate
        );
    }

    /// Migrates the GENUINE behavioral intent of
    /// `bookend_guard_chat_error_mid_judge_docket_still_yields_terminal_
    /// task_record`: a LOCAL judge's per-flag dispatch errors are swallowed
    /// (`JudgeRuling::Error` -> `Tier::Archived` — the SAME preserved
    /// `judge_one_flag_with_passes` both drivers call), so the graph run
    /// COMPLETES rather than aborting; since no flag got a usable ruling,
    /// the judge-dead honesty gate marks the envelope degenerate.
    #[test]
    fn judge_dispatch_errors_are_swallowed_per_flag_not_aborted() {
        let crew = graph_valid_crew();
        let ctx = step_ctx_with_chat(&crew, bundles_from_diff(DIFF), |call: &ChatCall| {
            if call.model.contains("probe-model") {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            } else {
                Err(anyhow!("lmstudio down"))
            }
        });
        let env = run_graph(&ctx, &mut NullEmitter)
            .expect("judge dispatch errors are swallowed per-flag, never abort the run");
        assert_eq!(env.judged.len(), 1, "the flag WAS judged (archived), not dropped");
        assert_eq!(env.archived, 1);
        let reason = env.degenerate.expect("a fully-dead judge marks the envelope degenerate");
        assert!(reason.contains("no usable ruling"), "{reason}");
    }

    // ── staffing_snapshot (#1247): migrated to direct pure-function tests ──
    //
    // `staffing_snapshot` is a pure function (`probes`, `judge`, `verify`,
    // `request_changes` in; `CrewStaffingSnapshot` out) — the three tests
    // below only ever routed through `run_review` to get an `env.staffing`
    // to inspect. Calling `staffing_snapshot` directly is a MORE direct
    // test of the thing actually under test, and needs no driver — graph
    // or sequential — at all. `run_review_graph` itself just stores the
    // caller-computed snapshot verbatim (`env.staffing = Some(staffing)` in
    // its own body), so there is nothing driver-specific left to migrate.

    /// Was `staffing_snapshot_round_trips_and_reflects_the_callers_
    /// resolved_k_not_a_registry_default`.
    #[test]
    fn graph_staffing_snapshot_reflects_the_callers_resolved_k_not_a_registry_default() {
        let probes = vec![staffing("fast", "probe-model", 9)];
        let judge = staffing("fast", "judge-model", 1);
        let snapshot = staffing_snapshot(&probes, &judge, None, false);

        assert_eq!(snapshot.probes.len(), 1);
        assert_eq!(snapshot.probes[0].k, 9, "the OVERRIDDEN k the caller resolved onto the crew");
        assert_eq!(snapshot.probes[0].name, "fast");
        assert_eq!(snapshot.probes[0].model, "darkmux:probe-model", "same namespaced form MemberRecord.model uses");
        let judge_snap = snapshot.judge.as_ref().expect("exactly one judge staffing");
        assert_eq!(judge_snap.model, "darkmux:judge-model");
        assert_eq!(judge_snap.k, 1);
        assert_eq!(snapshot.probes[0].n_ctx, Some(32_000));
        assert_eq!(judge_snap.n_ctx, Some(32_000));

        // The shape `reviews.json` persists, inside a full envelope — a
        // JSON round trip must preserve the snapshot exactly.
        let env = ReviewEnvelope { staffing: Some(snapshot), ..Default::default() };
        let json = serde_json::to_string(&env).expect("envelope serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("envelope parses back");
        assert_eq!(value["staffing"]["probes"][0]["k"], json!(9));
        assert_eq!(value["staffing"]["probes"][0]["model"], json!("darkmux:probe-model"));
        assert_eq!(value["staffing"]["probes"][0]["n_ctx"], json!(32_000));
        assert_eq!(value["staffing"]["judge"]["model"], json!("darkmux:judge-model"));
        assert_eq!(value["staffing"]["judge"]["n_ctx"], json!(32_000));
    }

    /// Was `staffing_snapshot_carries_the_judge_passes_knob`.
    #[test]
    fn graph_staffing_snapshot_carries_the_judge_passes_knob() {
        let probes = vec![staffing("fast", "probe-model", 2)];
        let mut judge = staffing("fast", "judge-model", 1);
        judge.passes = 3; // an N-pass consensus judge
        let snapshot = staffing_snapshot(&probes, &judge, None, false);

        assert_eq!(snapshot.judge.as_ref().unwrap().passes, 3, "the judge's resolved consensus depth is snapshotted");
        assert_eq!(snapshot.probes[0].passes, 2, "a probe seat omitting passes carries the visible default");

        let env = ReviewEnvelope { staffing: Some(snapshot), ..Default::default() };
        let json = serde_json::to_string(&env).expect("envelope serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("envelope parses back");
        assert_eq!(value["staffing"]["judge"]["passes"], json!(3));
        assert_eq!(value["staffing"]["probes"][0]["passes"], json!(2));
    }

    /// Was `staffing_snapshot_carries_the_request_changes_flag`.
    #[test]
    fn graph_staffing_snapshot_carries_the_request_changes_flag() {
        let probes = vec![staffing("fast", "probe-model", 2)];
        let judge = staffing("fast", "judge-model", 1);

        let blocking = staffing_snapshot(&probes, &judge, None, true);
        assert!(blocking.request_changes, "the crew's request_changes flag is snapshotted");
        let env = ReviewEnvelope { staffing: Some(blocking), ..Default::default() };
        let json = serde_json::to_string(&env).expect("envelope serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("envelope parses back");
        assert_eq!(value["staffing"]["request_changes"], json!(true));

        let advisory = staffing_snapshot(&probes, &judge, None, false);
        let env2 = ReviewEnvelope { staffing: Some(advisory), ..Default::default() };
        let json2 = serde_json::to_string(&env2).expect("envelope serializes");
        let value2: serde_json::Value = serde_json::from_str(&json2).expect("envelope parses back");
        assert!(
            value2["staffing"].get("request_changes").is_none(),
            "the advisory default is skipped on serialize"
        );
    }

    // ── the degenerate gates (#1355's own core finding) ─────────────────

    /// Was `degenerate_zero_bundles_never_silently_passes`.
    #[test]
    fn graph_degenerate_zero_bundles_never_silently_passes() {
        let crew = graph_valid_crew();
        let ctx = step_ctx_with_chat(&crew, Vec::new(), |_call: &ChatCall| Ok(reply("unused")));
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");
        assert!(env.degenerate.is_some());
        assert_eq!(env.bundles, 0);
        assert_eq!(env.confirmed, 0);
        assert_eq!(env.needs_check, 0);
        assert_eq!(env.archived, 0);
        assert!(
            env.fingerprint.get("protocol").is_some(),
            "a degenerate envelope still carries the comparability fingerprint"
        );
    }

    /// Was `degenerate_zero_flags_never_silently_passes`.
    #[test]
    fn graph_degenerate_zero_flags_never_silently_passes() {
        let crew = graph_valid_crew();
        // Every probe draw comes back empty — retried, then skipped.
        let ctx = step_ctx_with_chat(&crew, bundles_from_diff(DIFF), |_call: &ChatCall| Ok(reply("")));
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");
        assert!(env.degenerate.is_some());
        assert_eq!(env.raw_flags, 0);
        assert_eq!(env.judged.len(), 0);
        assert!(
            env.fingerprint.get("protocol").is_some(),
            "a zero-flag envelope still carries the comparability fingerprint"
        );
    }

    /// Was `degenerate_all_unparsed_judge_never_renders_as_a_clean_pass`.
    #[test]
    fn graph_degenerate_all_unparsed_judge_never_renders_as_a_clean_pass() {
        let crew = graph_valid_crew();
        let ctx = step_ctx_with_chat(&crew, bundles_from_diff(DIFF), |call: &ChatCall| {
            if call.model.contains("probe-model") {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            } else {
                // Every judge call (pass-1 AND its unparsed-retry) is
                // off-contract prose — no fenced JSON ruling.
                Ok(reply("I could not reach a verdict on this."))
            }
        });
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");
        assert_eq!(env.judged.len(), 1, "the flag WAS judged (archived), not dropped");
        assert_eq!(env.confirmed, 0);
        assert_eq!(env.needs_check, 0);
        assert_eq!(env.archived, 1);
        let note = env.degenerate.expect("all-unparsed judge must mark the envelope degenerate");
        assert!(note.contains("no usable ruling"), "{note}");
        assert!(note.contains("1 flags"), "names how many flags got nothing: {note}");
    }

    /// Was `genuine_all_false_positive_docket_is_not_degenerate`.
    #[test]
    fn graph_genuine_all_false_positive_docket_is_not_degenerate() {
        let crew = graph_valid_crew();
        let ctx = step_ctx_with_chat(&crew, bundles_from_diff(DIFF), |call: &ChatCall| {
            if call.model.contains("probe-model") {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            } else {
                Ok(reply(FP_JSON))
            }
        });
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");
        assert_eq!(env.confirmed, 0);
        assert_eq!(env.archived, 1);
        assert!(
            env.degenerate.is_none(),
            "a ruled-on docket is honest signal, never degenerate: {:?}",
            env.degenerate
        );
    }

    /// (#1418 route a) Every staffed probe seat's `selector` matches zero
    /// of the diff's bundles (e.g. a language-scoped crew reviewing a
    /// docs-only diff): `select_bundles_for_staffing` comes back empty for
    /// every seat, so zero draws happen anywhere in the run. Before #1418,
    /// this read as an authoritative Clean "no findings" review having
    /// examined nothing; the fix names it degenerate with a reason
    /// distinguishing the selector-starvation cause from generic
    /// zero-flags degeneracy.
    #[test]
    fn graph_degenerate_zero_draws_when_no_seat_matches_any_bundle() {
        let crew = crew_with(vec![
            (
                "review-probe",
                vec![ResolvedSeatStaffing {
                    name: "fast".to_string(),
                    pm: graph_pm("probe-model"),
                    k: 2,
                    passes: 2,
                    max_tokens: None,
                    selector: Some(BundleSelector {
                        fact_families: vec!["nonexistent-family".to_string()],
                        max_bundles: None,
                        ..Default::default()
                    }),
                }],
            ),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
        ]);
        let ctx = step_ctx_with_chat(&crew, bundles_from_diff(DIFF), |_call: &ChatCall| Ok(reply("unused")));
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");
        assert!(env.bundles > 0, "the diff DID produce bundles; this isn't the zero-bundle gate");
        assert!(env.members.is_empty(), "no seat ever placed a call");
        assert_eq!(env.confirmed, 0);
        let note = env.degenerate.expect("zero draws across every seat must never read as Clean");
        assert!(note.contains("no probe seat matched any bundle"), "{note}");
    }

    // ── remote seats: routing + provenance (#1260/#1177/#1355) ─────────

    /// Was `remote_seats_skip_cycler_route_endpoint_and_stamp_host_only_
    /// provenance`. The cycler-specific assertion (there is no `ModelCycler`
    /// in the graph's dispatch path — see the retirement note above) is
    /// dropped; the routing + provenance assertions, which are exactly
    /// #1355's territory, are kept.
    #[test]
    fn graph_remote_seats_route_endpoint_and_stamp_host_only_provenance() {
        let crew = crew_with(vec![
            (
                "review-probe",
                vec![graph_staffing("fast", "local-probe", 1), remote_staffing("cloud", "gpt-remote", 1)],
            ),
            ("review-judge", vec![remote_staffing("cloud-judge", "gpt-judge", 1)]),
        ]);
        let calls: std::sync::Mutex<Vec<(String, bool)>> = std::sync::Mutex::new(Vec::new());
        let ctx = step_ctx_with_chat(&crew, vec![bundle_input("a.ts")], move |call: &ChatCall| {
            calls.lock().unwrap().push((call.model.to_string(), call.endpoint.is_some()));
            if call.model == "gpt-judge" {
                Ok(reply(CONFIRM_JSON))
            } else {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            }
        });
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");

        // Local call: namespaced identifier, no endpoint. Remote calls: bare
        // profile id + endpoint.
        let probe = env.members.iter().find(|m| m.model == "gpt-remote").expect("remote member");
        assert!(probe.remote);
        assert_eq!(probe.endpoint.as_deref(), Some("myorg.cognitiveservices.azure.com"));
        let judge = env.members.iter().find(|m| m.seat == "review-judge").unwrap();
        assert!(judge.remote);
        let snap = env.staffing.as_ref().unwrap();
        assert!(snap
            .probes
            .iter()
            .any(|s| s.remote && s.endpoint.as_deref() == Some("myorg.cognitiveservices.azure.com")));
        assert!(snap.judge.as_ref().unwrap().remote);
        let local_snap = snap.probes.iter().find(|s| !s.remote).unwrap();
        assert!(local_snap.endpoint.is_none(), "local seats carry no endpoint field");
        // Never the full deployment path (and with it, never a key).
        let json = serde_json::to_string(&env).unwrap();
        assert!(
            !json.contains("/openai/deployments"),
            "the full deployment URL must never serialize into the envelope"
        );
    }

    /// Was `served_model_captured_distinct_from_requested_on_probe_and_
    /// judge`.
    #[test]
    fn graph_served_model_captured_distinct_from_requested_on_probe_judge_and_verify() {
        let crew = crew_with(vec![
            ("review-probe", vec![remote_staffing("cloud", "gpt-4o", 1)]),
            ("review-judge", vec![remote_staffing("cloud-judge", "gpt-4o", 1)]),
            ("review-verify", vec![remote_staffing("cloud-verify", "gpt-4o", 1)]),
        ]);
        let ctx = step_ctx_with_chat(&crew, vec![bundle_input("a.ts")], |call: &ChatCall| {
            let content = if call.system.contains("verify") {
                "```json\n{\"ruling\": \"verified\", \"decisive_evidence\": \"e\", \"note_for_author\": \"n\"}\n```"
                    .to_string()
            } else if call.model == "gpt-4o" && call.system.contains("judge") {
                CONFIRM_JSON.to_string()
            } else {
                "a real defect".to_string()
            };
            Ok(SingleShotReply {
                content,
                total_tokens: Some(10),
                prompt_tokens: None,
                completion_tokens: None,
                model: Some("gpt-4o-2026-08-01".to_string()),
            })
        });
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");

        let probe = env.members.iter().find(|m| m.seat == "review-probe").expect("probe member");
        assert_eq!(probe.model, "gpt-4o", "requested id is unchanged");
        assert_eq!(
            probe.served_model.as_deref(),
            Some("gpt-4o-2026-08-01"),
            "the probe's served model must be captured distinct from the requested id"
        );
        let judge = env.members.iter().find(|m| m.seat == "review-judge").expect("judge member");
        assert_eq!(judge.model, "gpt-4o");
        assert_eq!(
            judge.served_model.as_deref(),
            Some("gpt-4o-2026-08-01"),
            "the judge's served model must be captured distinct from the requested id"
        );
        let verify = env.members.iter().find(|m| m.seat == "review-verify").expect("verify member");
        assert_eq!(verify.model, "gpt-4o");
        assert_eq!(
            verify.served_model.as_deref(),
            Some("gpt-4o-2026-08-01"),
            "the verify seat's served model must be captured distinct from the requested id too"
        );
    }

    /// Was `served_model_absent_for_local_seats`.
    #[test]
    fn graph_served_model_absent_for_local_seats() {
        let crew = crew_with(vec![
            ("review-probe", vec![graph_staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
            ("review-verify", vec![graph_staffing("fast", "verify-model", 1)]),
        ]);
        let ctx = step_ctx_with_chat(&crew, vec![bundle_input("a.ts")], |call: &ChatCall| {
            let content = if call.system.contains("verify") {
                "```json\n{\"ruling\": \"verified\", \"decisive_evidence\": \"e\", \"note_for_author\": \"n\"}\n```"
                    .to_string()
            } else if call.model == "darkmux:judge-model" {
                CONFIRM_JSON.to_string()
            } else {
                "a real defect".to_string()
            };
            // (#1300 QA follow-up) The mock deliberately reports a served
            // model on the LOCAL calls too — exactly what a real LMStudio
            // response does. This proves the gate actually filters it out,
            // not that the mock happens never to set it.
            Ok(SingleShotReply { content, total_tokens: Some(10), prompt_tokens: None, completion_tokens: None, model: Some(call.model.to_string()) })
        });
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");

        assert_eq!(env.members.len(), 3, "probe + judge + verify all dispatched");
        for m in &env.members {
            assert!(
                m.served_model.is_none(),
                "a local seat must never report a served_model, even when the response body carries \
                 one (LMStudio's does): {m:?}"
            );
        }
    }

    /// Was `remote_probe_budget_exhaustion_is_reduced_coverage_not_a_dead_
    /// run`. The probe stage's remote bucket IS threaded through to
    /// `env.remote_budgets` on the graph path (`BuiltReviewGraph::
    /// probe_bucket`, merged post-run in `run_review_graph`) — unlike
    /// judge/verify, whose equivalent threading is the gap named above.
    #[test]
    fn graph_remote_probe_budget_exhaustion_is_reduced_coverage_not_a_dead_run() {
        let crew = crew_with(vec![
            ("review-probe", vec![remote_staffing("cloud", "gpt-remote", 3)]),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
        ]);
        let ctx = step_ctx_with_chat_and_budget(&crew, vec![bundle_input("a.ts")], 100, |call: &ChatCall| {
            if call.model == "darkmux:judge-model" {
                Ok(reply(CONFIRM_JSON))
            } else {
                Ok(SingleShotReply {
                    content: "a real defect `const end = start.plus(30)`".to_string(),
                    total_tokens: Some(600),
                    prompt_tokens: None,
                    completion_tokens: None,
                    model: None,
                })
            }
        });
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");

        assert!(env.degenerate.is_none(), "probe exhaustion never degrades the run");
        assert_eq!(env.raw_flags, 1, "only the pre-exhaustion draw landed");
        assert_eq!(env.confirmed, 1, "the surviving flag still went through the judge");
        let rec = env.remote_budgets.iter().find(|r| r.stage == "probe").expect("probe budget row");
        assert!(rec.exhausted);
        assert_eq!(rec.used_tokens, 600);
        assert_eq!(rec.skipped_calls, 2, "the remaining k-1 draws were skipped, not billed");
        assert!(
            env.warnings.iter().any(|w| w.contains("reduced coverage")),
            "the named reason lands in the envelope: {:?}",
            env.warnings
        );
    }

    /// Was `remote_judge_budget_exhaustion_is_an_honest_degraded_run`, then
    /// (temporarily) `graph_remote_judge_budget_exhaustion_gap_flag_level_
    /// is_honest_run_level_is_not` while #1373 gates a/b were an open,
    /// characterized gap. FIXED (#1373): `ReviewJudgeStepKind` now applies
    /// the SAME run-level honesty gate `finish_review` always has, via the
    /// shared `judge_gate_outcome` helper — a partially-exhausted remote
    /// judge bucket degrades the whole run, and its pass1/pass2 budget rows
    /// reach `env.remote_budgets`.
    #[test]
    fn graph_remote_judge_budget_exhaustion_is_an_honest_degraded_run() {
        let crew = crew_with(vec![
            ("review-probe", vec![graph_staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![remote_staffing("cloud", "gpt-judge", 1)]),
        ]);
        // Two bundles ⇒ two anchor-less flags in different bundles ⇒ both
        // survive dedup ⇒ the second flag's pass-1 hits the exhausted
        // bucket (one 600-token ruling exhausts a 100-token allowance).
        let bundles = vec![bundle_input("a.ts"), bundle_input("b.ts")];
        let ctx = step_ctx_with_chat_and_budget(&crew, bundles, 100, |call: &ChatCall| {
            if call.endpoint.is_some() {
                Ok(SingleShotReply {
                    content: CONFIRM_JSON.to_string(),
                    total_tokens: Some(600),
                    prompt_tokens: None,
                    completion_tokens: None,
                    model: None,
                })
            } else {
                Ok(reply("a real defect"))
            }
        });
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");

        // CORRECT (preserved per-flag logic): the pre-exhaustion flag still
        // carries a real ruling, and the post-exhaustion flag is honestly
        // marked Error, never silently confirmed.
        assert_eq!(env.judged.len(), 2);
        assert!(env.judged.iter().any(|j| j.tier == Tier::Confirmed), "the pre-exhaustion flag rules normally");
        let skipped = env
            .judged
            .iter()
            .find(|j| j.pass1.ruling == JudgeRuling::Error)
            .expect("the post-exhaustion flag is ruled Error, never silently confirmed");
        assert!(skipped.pass1.note_for_author.contains("remote token budget exhausted"));

        // (#1373 gate b, FIXED) ANY judge-bucket exhaustion degrades the
        // whole run (operator decision, #1260) — even though this scenario
        // has one flag that DID get a real ruling before the bucket ran
        // out (the "zero usable pass-1 rulings" gate alone would NOT have
        // caught this; the budget-exhaustion gate is the one that fires).
        let reason = env.degenerate.as_deref().expect("a partially-exhausted remote judge degrades the run");
        assert!(reason.contains("remote judge token budget exhausted"), "got: {reason}");
        // (#1373 gate a, FIXED) judge-pass1/pass2 budget rows now reach
        // `env.remote_budgets` alongside probe's own bucket row.
        assert!(
            env.remote_budgets.iter().any(|r| r.stage == "judge-pass1"),
            "judge-pass1 budget row must reach the envelope: {:?}",
            env.remote_budgets
        );
        assert!(
            env.remote_budgets.iter().any(|r| r.stage == "judge-pass2"),
            "judge-pass2 budget row must reach the envelope: {:?}",
            env.remote_budgets
        );
    }

    /// Was `remote_probe_failure_is_a_warning_and_the_run_continues`.
    #[test]
    fn graph_remote_probe_failure_is_a_warning_and_the_run_continues() {
        let crew = crew_with(vec![
            (
                "review-probe",
                vec![graph_staffing("fast", "local-probe", 1), remote_staffing("cloud", "gpt-remote", 2)],
            ),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
        ]);
        let ctx = step_ctx_with_chat(&crew, vec![bundle_input("a.ts")], |call: &ChatCall| {
            if call.endpoint.is_some() {
                Err(anyhow!("endpoint 401"))
            } else if call.model == "darkmux:judge-model" {
                Ok(reply(CONFIRM_JSON))
            } else {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            }
        });
        let env =
            run_graph(&ctx, &mut NullEmitter).expect("a remote probe failure must not abort the run");
        assert!(
            env.warnings.iter().any(|w| w.contains("reduced coverage") && w.contains("endpoint 401")),
            "the named failure lands as a warning: {:?}",
            env.warnings
        );
        assert_eq!(env.confirmed, 1, "the local seat's flag still confirmed");
        let remote = env.members.iter().find(|m| m.model == "gpt-remote").expect("remote member row");
        assert!(remote.remote);
        assert_eq!(remote.total_tokens, 0, "a failed seat billed nothing");
    }

    // ── the review-verify seat (#1260/#1177) ────────────────────────────

    /// Was `verify_stage_verified_refuted_uncertain_state_machine`. The
    /// residency-ordering assertion (cycler load/release order) and the old
    /// flow-record vocabulary assertions are dropped — see the retirement
    /// note above; the state-machine + envelope-accounting intent (the
    /// actual point of the test) is kept.
    #[test]
    fn graph_verify_stage_verified_refuted_uncertain_state_machine() {
        let crew = crew_with(vec![
            ("review-probe", vec![graph_staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
            ("review-verify", vec![graph_staffing("frontier", "verify-model", 1)]),
        ]);
        let bundles = vec![bundle_input("a.ts"), bundle_input("b.ts"), bundle_input("c.ts")];
        let verify_replies = std::sync::Mutex::new(vec![VERIFIED_JSON, REFUTED_JSON, UNCERTAIN_JSON]);
        let ctx = step_ctx_with_chat(&crew, bundles, move |call: &ChatCall| {
            if call.model == "darkmux:verify-model" {
                assert_eq!(call.system, "verify persona", "the verify seat gets its own persona");
                Ok(reply(verify_replies.lock().unwrap().remove(0)))
            } else if call.model == "darkmux:judge-model" {
                Ok(reply(CONFIRM_JSON))
            } else {
                Ok(reply("a real defect"))
            }
        });
        let mut emitter = RecordingEmitter::new();
        let env = run_graph(&ctx, &mut emitter).expect("graph run completes");

        assert_eq!(env.judged.len(), 3);
        // verified: stays confirmed, record present.
        let v = &env.judged[0];
        assert_eq!(v.tier, Tier::Confirmed);
        assert_eq!(v.verify.as_ref().unwrap().ruling, VerifyRuling::Verified);
        assert_eq!(v.verify.as_ref().unwrap().model, "darkmux:verify-model");
        assert!(!v.demoted_by_verify);
        // refuted: demoted to archived, demotion recorded.
        let r = &env.judged[1];
        assert_eq!(r.tier, Tier::Archived);
        assert!(r.demoted_by_verify);
        assert_eq!(r.verify.as_ref().unwrap().ruling, VerifyRuling::Refuted);
        assert_eq!(r.verify.as_ref().unwrap().note_for_author, "rn");
        // uncertain: stays confirmed (keeps the marker downstream).
        let u = &env.judged[2];
        assert_eq!(u.tier, Tier::Confirmed);
        assert_eq!(u.verify.as_ref().unwrap().ruling, VerifyRuling::Uncertain);
        assert!(!u.demoted_by_verify);
        // Envelope accounting.
        assert_eq!(env.confirmed, 2);
        assert_eq!(env.archived, 1);
        assert_eq!(env.verified, 1);
        assert_eq!(env.refuted, 1);
        let member = env.members.iter().find(|m| m.seat == "review-verify").expect("verify member");
        assert_eq!(member.draws, 3, "one adjudication per confirmed flag");
        assert!(!member.remote);
        assert!(env.staffing.as_ref().unwrap().verify.is_some(), "snapshot carries the verify seat");
        // Live observability: the scheduler's own generic step-lifecycle
        // bookend fired for the verify step, on the SAME injected emitter
        // every other record in this test's run rides (`emit_review_step_
        // result`'s own "step result" records go to the global
        // `darkmux_flow::record()` sink instead — see its own doc — so they
        // are NOT visible via `emitter` here; the scheduler's generic
        // bookend is the one signal this emitter actually carries).
        assert!(
            emitter
                .records
                .iter()
                .any(|r| r.action == "step complete" && r.handle == "review-verify-step"),
            "the verify step's generic lifecycle bookend must fire: {:?}",
            emitter.records.iter().map(|r| (r.action.as_str(), r.handle.as_str())).collect::<Vec<_>>()
        );
    }

    /// Was `crew_without_verify_seat_is_unchanged`.
    #[test]
    fn graph_crew_without_verify_seat_is_unchanged() {
        let crew = graph_valid_crew();
        let ctx = step_ctx_with_chat(&crew, vec![bundle_input("a.ts")], |call: &ChatCall| {
            if call.model == "darkmux:judge-model" {
                Ok(reply(CONFIRM_JSON))
            } else {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            }
        });
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");
        assert!(env.judged.iter().all(|j| j.verify.is_none()));
        assert!(!env.members.iter().any(|m| m.seat == "review-verify"));
        let value = serde_json::to_value(&env).unwrap();
        assert!(value.get("verified").is_none(), "zero verified never serializes");
        assert!(value.get("refuted").is_none());
        assert!(value["staffing"].get("verify").is_none());
        for j in value["judged"].as_array().unwrap() {
            assert!(j.get("verify").is_none());
            assert!(j.get("demoted_by_verify").is_none());
        }
    }

    /// Was `verify_stage_skips_entirely_on_zero_confirms`.
    #[test]
    fn graph_verify_stage_skips_entirely_on_zero_confirms() {
        let crew = crew_with(vec![
            ("review-probe", vec![graph_staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
            ("review-verify", vec![graph_staffing("frontier", "verify-model", 1)]),
        ]);
        let ctx = step_ctx_with_chat(&crew, vec![bundle_input("a.ts")], |call: &ChatCall| {
            assert_ne!(call.model, "darkmux:verify-model", "no confirms ⇒ no verify dispatch");
            if call.model == "darkmux:judge-model" {
                Ok(reply(FP_JSON))
            } else {
                Ok(reply("a real defect"))
            }
        });
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");
        assert_eq!(env.confirmed, 0);
        assert!(!env.members.iter().any(|m| m.seat == "review-verify"));
    }

    /// Was `remote_verify_budget_exhaustion_degrades_the_stage_not_the_
    /// run`, then (temporarily) `graph_remote_verify_budget_exhaustion_
    /// gap_flag_level_is_honest_bucket_row_is_not` while #1373 gates a/c's
    /// verify half were an open, characterized gap. FIXED (#1373):
    /// `ReviewVerifyStepKind` now applies the SAME warning + budget-row
    /// logic `run_verify_stage` always has, via the shared
    /// `verify_budget_outcome` helper.
    #[test]
    fn graph_remote_verify_budget_exhaustion_degrades_the_stage_not_the_run() {
        let crew = crew_with(vec![
            ("review-probe", vec![graph_staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
            ("review-verify", vec![remote_staffing("frontier", "gpt-verify", 1)]),
        ]);
        let bundles = vec![bundle_input("a.ts"), bundle_input("b.ts")];
        let ctx = step_ctx_with_chat_and_budget(&crew, bundles, 100, |call: &ChatCall| {
            if call.endpoint.is_some() {
                Ok(SingleShotReply {
                    content: VERIFIED_JSON.to_string(),
                    total_tokens: Some(600),
                    prompt_tokens: None,
                    completion_tokens: None,
                    model: None,
                })
            } else if call.model == "darkmux:judge-model" {
                Ok(reply(CONFIRM_JSON))
            } else {
                Ok(reply("a real defect"))
            }
        });
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");

        // CORRECT (preserved per-flag logic, `verify_pass_with_retry`): the
        // run itself is never marked degenerate by verify exhaustion — the
        // pre-exhaustion adjudication still counts, and the skipped one
        // keeps its Confirmed tier with the reason named per-flag.
        assert!(env.degenerate.is_none(), "verify exhaustion never degrades the whole run");
        assert_eq!(env.verified, 1, "the pre-exhaustion adjudication still counts");
        let skipped = env
            .judged
            .iter()
            .find(|j| matches!(&j.verify, Some(v) if v.ruling == VerifyRuling::Error))
            .expect("skipped adjudication recorded as Error");
        assert_eq!(skipped.tier, Tier::Confirmed);
        assert!(skipped.verify.as_ref().unwrap().note_for_author.contains("remote token budget exhausted"));

        // (#1373 gates a/c, FIXED) `env.warnings` now carries the loud
        // "verify budget exhausted after N of M adjudications" entry, and
        // `env.remote_budgets` carries the verify bucket's own row.
        assert!(
            env.warnings.iter().any(|w| w.contains("verify budget exhausted after 1 of 2 adjudications")),
            "the exhaustion warning must reach env.warnings: {:?}",
            env.warnings
        );
        assert!(
            env.remote_budgets.iter().any(|r| r.stage == "verify"),
            "the verify budget row must reach env.remote_budgets: {:?}",
            env.remote_budgets
        );
    }

    /// Was `verify_stage_skipped_when_judge_already_degraded`, then
    /// (temporarily) `graph_verify_stage_gap_still_dispatches_on_a_judge_
    /// doomed_run` while #1373 gate d was an open, characterized gap.
    /// FIXED (#1373): `ReviewVerifyStepKind` now gates on the shared
    /// envelope's `degenerate` state (set by `ReviewJudgeStepKind` before
    /// verify's task ever becomes ready, since `verify_task.depends_on ==
    /// [judge_task]`) — CONSIDER g, no frontier spend on a run the judge
    /// already doomed.
    #[test]
    fn graph_verify_stage_skipped_when_judge_already_degraded() {
        let crew = crew_with(vec![
            ("review-probe", vec![graph_staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![remote_staffing("cloud", "gpt-judge", 1)]),
            ("review-verify", vec![graph_staffing("frontier", "verify-model", 1)]),
        ]);
        let bundles = vec![bundle_input("a.ts"), bundle_input("b.ts")];
        let verify_dispatched = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let verify_dispatched_write = verify_dispatched.clone();
        let ctx = step_ctx_with_chat_and_budget(&crew, bundles, 100, move |call: &ChatCall| {
            if call.model == "darkmux:verify-model" {
                verify_dispatched_write.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(reply("```json\n{\"ruling\": \"verified\", \"decisive_evidence\": \"e\", \"note_for_author\": \"n\"}\n```"))
            } else if call.endpoint.is_some() {
                Ok(SingleShotReply {
                    content: CONFIRM_JSON.to_string(),
                    total_tokens: Some(600),
                    prompt_tokens: None,
                    completion_tokens: None,
                    model: None,
                })
            } else {
                Ok(reply("a real defect"))
            }
        });
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");
        // One flag confirmed before the judge's remote bucket exhausted —
        // the SAME preserved per-flag logic as
        // `graph_remote_judge_budget_exhaustion_is_an_honest_degraded_run`
        // — but the run is now degenerate (gate b), so verify's non-empty
        // docket never dispatches at all.
        assert_eq!(env.confirmed, 1, "the pre-exhaustion flag stays confirmed — verify never touches it");
        assert!(env.degenerate.is_some(), "the judge-bucket exhaustion still degrades the run (gate b)");
        assert!(
            !verify_dispatched.load(std::sync::atomic::Ordering::SeqCst),
            "no verify-model chat call must fire on a judge-doomed run"
        );
        assert!(
            !env.members.iter().any(|m| m.seat == "review-verify"),
            "no review-verify member row — verify never ran: {:?}",
            env.members
        );
    }

    // ── review-round fixes (#1260) still hold on the graph path ─────────

    /// Was `local_only_envelope_carries_no_remote_fields`.
    #[test]
    fn graph_local_only_envelope_carries_no_remote_fields() {
        let crew = graph_valid_crew();
        let ctx = step_ctx_with_chat(&crew, vec![bundle_input("a.ts")], |call: &ChatCall| {
            if call.model == "darkmux:judge-model" {
                Ok(reply(CONFIRM_JSON))
            } else {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            }
        });
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");
        let value = serde_json::to_value(&env).unwrap();
        assert!(value.get("warnings").is_none(), "empty warnings never serialize");
        assert!(value.get("remote_budgets").is_none(), "no budget rows on a local-only run");
        for m in value["members"].as_array().unwrap() {
            assert!(m.get("remote").is_none(), "local members carry no remote flag");
            assert!(m.get("endpoint").is_none());
        }
        for s in value["staffing"]["probes"].as_array().unwrap() {
            assert!(s.get("remote").is_none());
        }
    }

    /// Was `remote_probe_empty_draw_still_bills_both_attempts`.
    #[test]
    fn graph_remote_probe_empty_draw_still_bills_both_attempts() {
        let crew = crew_with(vec![
            ("review-probe", vec![remote_staffing("cloud", "gpt-remote", 1)]),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
        ]);
        // Every remote call is empty content but bills 600 tokens — the
        // draw retries once, so two 600-token attempts.
        let ctx = step_ctx_with_chat(&crew, vec![bundle_input("a.ts")], |call: &ChatCall| {
            if call.endpoint.is_some() {
                Ok(SingleShotReply { content: String::new(), total_tokens: Some(600), prompt_tokens: None, completion_tokens: None, model: None })
            } else {
                Ok(reply(CONFIRM_JSON))
            }
        });
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");

        // Zero content ⇒ zero flags ⇒ the run is a degenerate zero-flag
        // run, but the SPEND is still fully accounted.
        assert!(env.degenerate.is_some(), "no flags landed, so the run is degenerate");
        let member = env.members.iter().find(|m| m.model == "gpt-remote").expect("remote member");
        assert!(member.remote);
        assert_eq!(member.total_tokens, 1200, "both empty attempts billed to the member (600 + 600)");
        let rec = env.remote_budgets.iter().find(|r| r.stage == "probe").expect("probe budget row");
        assert_eq!(rec.used_tokens, 1200, "both empty attempts billed to the bucket");
    }

    /// Was `remote_judge_dispatch_failure_degrades_the_run`. The run still
    /// goes degenerate (the outcome #1260 requires). (#1373 reason-
    /// specificity fix) The reason TEXT now matches `finish_review`'s own
    /// wording exactly — `judge_gate_outcome` special-cases the
    /// all-remote-dispatch-error variant on BOTH paths, naming the failure
    /// shape ("remote judge dispatch failed on N of M flags") rather than
    /// the generic "no usable ruling" — so the operator sees WHY the judge
    /// went dead, not just THAT it did.
    #[test]
    fn graph_remote_judge_dispatch_failure_degrades_the_run() {
        let crew = crew_with(vec![
            ("review-probe", vec![graph_staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![remote_staffing("cloud", "gpt-judge", 1)]),
        ]);
        let ctx = step_ctx_with_chat(&crew, vec![bundle_input("a.ts")], |call: &ChatCall| {
            if call.endpoint.is_some() {
                Err(anyhow!("endpoint 503"))
            } else {
                Ok(reply("a real defect"))
            }
        });
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");
        let reason = env.degenerate.as_deref().expect("remote judge dispatch failure degrades the run");
        assert!(
            reason.contains("remote judge dispatch failed on 1 of 1 flag"),
            "got: {reason}"
        );
    }

    /// Was `remote_judge_dispatch_error_on_minority_of_flags_does_not_
    /// degrade_the_run` (#1329). The "does not degrade" + per-flag demotion
    /// behavior is preserved (both live in `judge_one_flag_with_passes`,
    /// unchanged). (#1373 gate c, FIXED) The "must be named in
    /// env.warnings" half — dropped as a KNOWN GAP during the #1355/#1357
    /// migration — is restored: `ReviewJudgeStepKind` now pushes the SAME
    /// unconditional #1329 warning `finish_review` always has.
    #[test]
    fn graph_remote_judge_dispatch_error_on_minority_of_flags_does_not_degrade_the_run() {
        let crew = crew_with(vec![
            ("review-probe", vec![graph_staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![remote_staffing("cloud", "gpt-judge", 1)]),
        ]);
        let bundles = vec![bundle_input("a.ts"), bundle_input("b.ts"), bundle_input("c.ts")];
        let judge_call_index = std::sync::atomic::AtomicU32::new(0);
        let ctx = step_ctx_with_chat(&crew, bundles, move |call: &ChatCall| {
            if call.endpoint.is_some() {
                let idx = judge_call_index.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // Calls land flag-major (judge_concurrency: 1 in `run_graph`
                // — byte-identical dispatch order to the historical
                // sequential loop): f1.p1, f1.p2, f2.p1, f2.p2, f3.p1, f3.p2.
                // Fail ONLY f2's pass-2 (call index 3).
                if idx == 3 {
                    Err(anyhow!("endpoint 503"))
                } else {
                    Ok(reply(CONFIRM_JSON))
                }
            } else {
                Ok(reply("a real defect"))
            }
        });
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");

        assert!(
            env.degenerate.is_none(),
            "a minority dispatch error with real usable signal must not degrade the run: {:?}",
            env.degenerate
        );
        assert_eq!(env.judged.len(), 3);
        assert_eq!(env.confirmed, 2, "the two clean flags stay confirmed");
        assert_eq!(env.needs_check, 1, "the dispatch-error flag demotes, it is not lost");
        assert_eq!(env.archived, 0);
        let demoted = &env.judged[1];
        assert_eq!(demoted.tier, Tier::NeedsCheck);
        assert!(demoted.demoted_by_pass2);
        // `finish_review` names this transient failure in `env.warnings`
        // even on an otherwise-healthy run (the loud-beats-quiet fix from
        // #1329) — the graph path now does too.
        assert!(
            env.warnings.iter().any(|w| w.contains("remote judge dispatch failed on 1 of 3 flag")),
            "a minority judge dispatch error must be named in env.warnings: {:?}",
            env.warnings
        );
    }

    /// Was `local_judge_dispatch_failure_keeps_today_behavior`.
    #[test]
    fn graph_local_judge_dispatch_failure_keeps_today_behavior() {
        let crew = graph_valid_crew();
        let ctx = step_ctx_with_chat(&crew, vec![bundle_input("a.ts")], |call: &ChatCall| {
            if call.model == "darkmux:judge-model" {
                Err(anyhow!("lmstudio down"))
            } else {
                Ok(reply("a real defect"))
            }
        });
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");
        let reason = env.degenerate.as_deref().expect("a fully-dead local judge is degenerate (judge-dead gate)");
        assert!(reason.contains("no usable ruling"), "local path uses the judge-dead gate: {reason}");
        assert!(!reason.contains("remote judge dispatch failed"), "the remote reason must not fire for a local judge");
    }

    /// Was `remote_probe_seat_sends_reasoning_floor_on_the_wire` (#1260 FIX
    /// 5, live).
    #[test]
    fn graph_remote_probe_seat_sends_reasoning_floor_on_the_wire() {
        let crew = crew_with(vec![
            ("review-probe", vec![remote_staffing("cloud", "gpt-remote", 1)]),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
        ]);
        let seen_cap = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let seen_cap_write = seen_cap.clone();
        let ctx = step_ctx_with_chat(&crew, vec![bundle_input("a.ts")], move |call: &ChatCall| {
            if call.endpoint.is_some() {
                seen_cap_write.store(call.max_tokens, std::sync::atomic::Ordering::SeqCst);
                Ok(reply("a real defect"))
            } else {
                Ok(reply(CONFIRM_JSON))
            }
        });
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");
        assert_eq!(env.raw_flags, 1, "sanity: the remote probe draw actually landed");
        assert_eq!(seen_cap.load(std::sync::atomic::Ordering::SeqCst), REMOTE_REASONING_MAX_TOKENS_FLOOR);
    }

    /// Was `verify_dispatch_error_and_unparsed_keep_confirmed_with_
    /// marker`.
    #[test]
    fn graph_verify_dispatch_error_and_unparsed_keep_confirmed_with_marker() {
        let crew = crew_with(vec![
            ("review-probe", vec![graph_staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
            ("review-verify", vec![graph_staffing("frontier", "verify-model", 1)]),
        ]);
        let bundles = vec![bundle_input("a.ts"), bundle_input("b.ts")];
        // Flag a: the verify call errors. Flag b: garbage both attempts (the
        // unparsed retry fires, then stays Unparsed).
        let verify_calls = std::sync::atomic::AtomicU32::new(0);
        let ctx = step_ctx_with_chat(&crew, bundles, move |call: &ChatCall| {
            if call.model == "darkmux:verify-model" {
                let n = verify_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                match n {
                    1 => Err(anyhow!("verify endpoint down")),
                    _ => Ok(reply("no verdict here")),
                }
            } else if call.model == "darkmux:judge-model" {
                Ok(reply(CONFIRM_JSON))
            } else {
                Ok(reply("a real defect"))
            }
        });
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");
        assert!(env.degenerate.is_none(), "an inconclusive verify never degrades the run");
        assert_eq!(env.confirmed, 2, "both stay confirmed (marker downstream)");
        assert_eq!(env.verified, 0, "an inconclusive adjudication never promotes");
        let errored = env
            .judged
            .iter()
            .find(|j| matches!(&j.verify, Some(v) if v.ruling == VerifyRuling::Error))
            .expect("dispatch-error adjudication recorded as Error");
        assert_eq!(errored.tier, Tier::Confirmed);
        let unparsed = env
            .judged
            .iter()
            .find(|j| matches!(&j.verify, Some(v) if v.ruling == VerifyRuling::Unparsed))
            .expect("garbage adjudication recorded as Unparsed after the retry");
        assert_eq!(unparsed.tier, Tier::Confirmed);
    }
