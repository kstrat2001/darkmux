    use super::*;
    // `NodeStatus` is used only by this test module as of #1284 Packet 3
    // (`build_review_graph` stopped constructing `Step` literals directly
    // once it became a thin `mission_config::interpret` launcher).
    use darkmux_crew::scheduler::{STEP_LIFECYCLE_ACTIONS, STEP_TIMING_ACTION};
    // (#1877 item 2) `HostSample` — test-only, same reasoning as
    // `ArtifactBus` below: production review.rs code never names this type
    // directly any more (only passes `sample_host`'s value through), so
    // this import stays out of review.rs's own `use` block.
    use darkmux_crew::telemetry_sampler::HostSample;
    use darkmux_crew::types::NodeStatus;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    // (#1877 item 2) `Ordering`/`mpsc`/`thread`/`Duration` were pulled in
    // via `super::*` from review.rs's own top-level imports before this PR
    // — that file no longer needs them in its production code (the
    // sampler thread/channel/stop-flag machinery moved to
    // `darkmux_crew::run_obs`), so the sampler + concurrency tests in this
    // module import them directly, same convention as `HostSample`/
    // `ArtifactBus`. `AtomicBool` itself stays out — this module's one use
    // (`verify_dispatched`, further down) already spells it out fully
    // qualified (`std::sync::atomic::AtomicBool`).
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;
    // (#1530 Packet 1) Only test code hand-builds an `ArtifactBus` — a
    // production `run_review_graph` call always builds its bus through
    // `run_step_graph`'s own pre-scan + caller-seed path. Imported here
    // (not review.rs's production `use` block) so a non-test build never
    // warns about it going unused.
    use darkmux_crew::step_kinds::ArtifactBus;
    // (#1605) Test-only — production review.rs code never constructs a
    // `SkippedFile` directly (only `build_bundles` does, in `bundle/mod.rs`).
    use super::super::bundle::SkippedFile;

    // ── fixtures ────────────────────────────────────────────────────

    const DIFF: &str = "--- a/billing.ts\n+++ b/billing.ts\n@@ -1,3 +1,4 @@\n context line\n+const end = start.plus(30)\n+const total = base * rate\n more context\n";

    fn pm(id: &str) -> ProfileModel {
        ProfileModel { id: id.to_string(), n_ctx: Some(32_000), ..Default::default() }
    }

    fn staffing(profile: &str, model: &str, k: u32) -> ResolvedSeatStaffing {
        ResolvedSeatStaffing {
            name: profile.to_string(),
            role_id: None,
            pm: pm(model),
            k,
            // Default double-confirm — a test needing a different judge depth
            // sets `.passes` on the returned staffing (#1266).
            passes: 2,
            max_tokens: None,
            selector: None,
            provenance: None,
        }
    }

    /// (#1512, #1513 review) Test-only fixture DSL: build a
    /// [`ResolvedReviewRoles`] from a `(family label, staffings)` list — the
    /// SAME literal shape the test suite has always used, so the ~60 call
    /// sites across this file don't need touching. This is purely a test
    /// fixture BUILDER's input format, not a production concept: production
    /// resolution (`darkmux_crew::resourcing::resolve_review_roles`) never
    /// reads a "family" string anywhere — it classifies roles structurally,
    /// by each task's own step kind. `"review-probe"` may repeat (each
    /// entry's staffings all become probes, in the order given);
    /// `"review-judge"`/`"review-verify"` take their entry's FIRST staffing.
    fn crew_with(seats: Vec<(&str, Vec<ResolvedSeatStaffing>)>) -> ResolvedReviewRoles {
        let mut probes = Vec::new();
        let mut judge = None;
        let mut verify = None;
        for (label, staffings) in seats {
            match label {
                "review-probe" => probes.extend(staffings),
                "review-judge" => judge = staffings.into_iter().next(),
                "review-verify" => verify = staffings.into_iter().next(),
                other => panic!("test fixture: unknown seat family `{other}`"),
            }
        }
        ResolvedReviewRoles {
            probes,
            judge: judge.expect("test fixture: crew_with needs a \"review-judge\" entry"),
            verify,
            request_changes: false,
            warnings: Vec::new(),
        }
    }

    fn valid_crew() -> ResolvedReviewRoles {
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
            absence_backstop: None,
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
    //
    // (#1442 ship-2b) `probe_one_draw`'s three unit tests retired WITH the
    // function. Successors — same behavioral intent on the generic block
    // that now owns the loop (`darkmux-crew::step_kinds::builtins`):
    //   probe_one_draw_empty_content_retries_once_then_skips
    //     -> dispatch_map_retry_on_empty_gives_up_honestly (both attempts
    //        billed, empty accepted as a non-flag)
    //   probe_one_draw_recovers_on_retry
    //     -> dispatch_map_retry_on_empty_retries_then_succeeds
    //   probe_one_draw_propagates_dispatch_error
    //     -> dispatch_map_local_per_item_error_isolation_continues_past_a_
    //        failure (deliberate semantic successor: per-item ISOLATION
    //        replaces hard propagation; the run-level honesty gate is
    //        graph_probe_dispatch_error's all-draws-failed reason below)

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

    // (#1512, #1513 review) `validate_review_crew`'s own seat-shape-rejection
    // tests RETIRED here — that validation dissolved into
    // `resolve_review_roles`'s OWN resolution loop (it enforces "at least
    // one probe role", "exactly one judge task", "verify optional" as part
    // of resolving them from the config, structurally, by step kind — a
    // config that violates any of these never produces an `Ok` value in the
    // first place). The equivalent coverage now lives in
    // `resourcing.rs`'s test module: `resolve_review_roles_with_no_probe_
    // tasks_errors_loudly`, `resolve_review_roles_with_no_judge_task_errors_
    // loudly`, `resolve_review_roles_verify_absent_from_the_document_
    // resolves_to_none`. This module's `crew_with` test DSL (a hand-built
    // fixture builder, not a production concept) no longer has a separate
    // "is this shape valid" check to test — a `ResolvedReviewRoles` value
    // built by hand is valid by construction (it's a plain struct, not a
    // family-keyed map that could be malformed).

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

    /// (#1434 — successor of `bookend_guard_clean_finish_stops_telemetry_
    /// sampler_thread`; the run-guard / per-run task bookend it drove
    /// was retired.) `ReviewObs` now owns the sampler's whole-run lifecycle
    /// (see its doc). Clean finish: emit a `step result` -> the obs drops —
    /// the sampler thread must already be stopped by the time that drop
    /// returns. Same bounded-timeout discipline as the sampler-only test
    /// above (drop runs on a spawned thread; the test thread asserts via
    /// `recv_timeout` so a hang fails loud instead of wedging the run).
    #[test]
    fn review_obs_clean_finish_stops_telemetry_sampler_thread() {
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut emitter = RecordingEmitter::new();
            let mut obs = ReviewObs::new_with_telemetry(
                &mut emitter,
                "case-1",
                "crew-1",
                "review",
                Duration::from_millis(5),
                Duration::from_millis(2),
                fake_sample,
                fake_lms,
            );
            obs.step_result("review.bundle", "bundle", json!({ "items_out": 0 }));
            drop(obs); // blocks until the sampler thread stops + joins
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("obs drop (clean finish) did not stop the telemetry sampler thread within 5s — thread leak");
    }

    /// (#1434 — successor of `bookend_guard_error_path_drop_stops_telemetry_
    /// sampler_thread`.) The abandoned-path mirror: an early `?`-return /
    /// panic unwind drops `ReviewObs` before it emits any completion record —
    /// its `Drop` must still stop the sampler thread, exactly like the
    /// clean-finish path above. (Run liveness on the real early-return path
    /// is the caller's `with_dispatch_bookends` wrap, not this struct.)
    #[test]
    fn review_obs_early_drop_stops_telemetry_sampler_thread() {
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut emitter = RecordingEmitter::new();
            {
                let _obs = ReviewObs::new_with_telemetry(
                    &mut emitter,
                    "case-2",
                    "crew-2",
                    "review",
                    Duration::from_millis(5),
                    Duration::from_millis(2),
                    fake_sample,
                    fake_lms,
                );
                // No emission — `_obs` drops here, exercising the early path.
            }
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("obs drop (early path) did not stop the telemetry sampler thread within 5s — thread leak");
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

    /// (#1475 / #44) The snapshot carries each seat's role→profile PROVENANCE
    /// verbatim from the resolver — the envelope records WHY a seat was staffed
    /// (role → profile → binding-source), not just what resolved — and a
    /// pre-provenance snapshot without the field still parses (`None`), per the
    /// module's schema-lenience discipline.
    #[test]
    fn staffing_snapshot_carries_provenance_and_reads_old_snapshots_leniently() {
        use darkmux_crew::resourcing::StaffingProvenance;
        let mut probe = staffing("fast", "probe-model", 2);
        probe.provenance =
            Some(StaffingProvenance::role_profile("review-probe-high", "fast", "role_profiles map"));
        let mut judge = staffing("fast", "judge-model", 1);
        judge.provenance =
            Some(StaffingProvenance::role_profile("review-judge", "big", "launch override"));
        let snap = staffing_snapshot(std::slice::from_ref(&probe), &judge, None, false);
        assert_eq!(snap.probes[0].provenance.as_ref().unwrap().kind, "role-profile");
        assert!(snap.probes[0].provenance.as_ref().unwrap().detail.contains("role_profiles map"));
        let judge_snap = snap.judge.as_ref().unwrap();
        assert_eq!(judge_snap.provenance.as_ref().unwrap().kind, "role-profile");
        assert!(judge_snap.provenance.as_ref().unwrap().detail.contains("launch override"));

        // Round-trips through JSON; an old snapshot WITHOUT the field parses
        // as None.
        let json = serde_json::to_string(&snap).unwrap();
        let back: StaffingSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.probes[0].provenance.as_ref().unwrap().kind, "role-profile");
        let old = r#"{"probes":[{"name":"fast","model":"m","k":2,"passes":2}],"judge":null}"#;
        let old_snap: StaffingSnapshot = serde_json::from_str(old).expect("pre-ship-2 snapshot parses");
        assert!(old_snap.probes[0].provenance.is_none());
    }

    // ── run_judge_only ────────────────────────────────────────────────

    #[test]
    fn run_judge_only_skips_probe_and_judges_supplied_flags() {
        let crew = valid_crew();
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            roles: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
            judge_exhaustion_strict: false,
            bundles: None,
            source: None,
            workspace: None,
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
            roles: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Parallel,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
            judge_exhaustion_strict: false,
            bundles: None,
            source: None,
            workspace: None,
        };
        let flags = vec![flag("billing.ts", "member-a", 0, "`const end = start.plus(30)` off by one")];
        let mut cycler = RecordingCycler::new();
        let mut chat = |_call: &ChatCall| Ok(reply(FP_JSON));
        let env = run_judge_only(flags, &inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");
        assert_eq!(env.mode, "parallel", "a judge-only re-run of a parallel review keeps its provenance");
    }

    /// (#1434) The `--charges-file` re-judge path (`run_judge_only`) emits the
    /// SAME generic `step result` companion vocabulary the graph path emits
    /// — never the retired bespoke per-run task/step/ruling `review.*`
    /// vocabulary. Drives `run_judge_only` through a `RecordingEmitter` and
    /// pins the record SHAPES: every emitted record is a `step result` (or a
    /// best-effort `telemetry.*` sample), and the step-result `kind`s cover
    /// the sequential pipeline's stages (`review.bundle`/`review.dedup`/
    /// `review.judge`). Run-level `dispatch start`/`dispatch complete`
    /// liveness bookends are the caller's `with_dispatch_bookends` wrap (per
    /// contract 2 — covered by `mission_launch_review`'s own bookend tests),
    /// NOT this driver, so they're deliberately absent here.
    #[test]
    fn run_judge_only_emits_step_result_vocabulary_not_legacy_review_actions() {
        let crew = valid_crew();
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            roles: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
            judge_exhaustion_strict: false,
            bundles: None,
            source: None,
            workspace: None,
        };
        let flags = vec![flag("billing.ts", "member-a", 0, "`const end = start.plus(30)` double-counts")];
        let mut cycler = RecordingCycler::new();
        let mut chat = |_call: &ChatCall| Ok(reply(CONFIRM_JSON));
        let mut emitter = RecordingEmitter::new();
        let env = run_judge_only(flags, &inputs, &mut chat, &mut cycler, &mut emitter).expect("runs");
        assert_eq!(env.judged.len(), 1);

        // Every emitted record is a generic `step result` companion or a
        // best-effort telemetry sample — NEVER a legacy per-run action.
        for r in &emitter.records {
            assert!(
                r.action == "step result" || r.action.starts_with("telemetry."),
                "run_judge_only emitted an unexpected action `{}` — the retired \
                 task/step/ruling vocabulary must not reappear",
                r.action
            );
        }
        // The step-result `kind`s cover the sequential pipeline's stages —
        // the SAME `kind` set the graph step kinds emit.
        let kinds: Vec<String> = emitter
            .records
            .iter()
            .filter(|r| r.action == "step result")
            .filter_map(|r| {
                r.payload
                    .as_ref()
                    .and_then(|p| p.get("kind"))
                    .and_then(|k| k.as_str())
                    .map(String::from)
            })
            .collect();
        assert!(kinds.iter().any(|k| k == "review.bundle"), "bundle step result present: {kinds:?}");
        assert!(kinds.iter().any(|k| k == "review.dedup"), "dedup step result present: {kinds:?}");
        assert!(kinds.iter().any(|k| k == "review.judge"), "judge step result present: {kinds:?}");
    }

    // ── #1748: absence-claim backstop ─────────────────────────────────
    //
    // Production incident: a `confirmed` finding claimed a line of code
    // was ABSENT ("does not assign process.exitCode", "there is no
    // .catch") when both were present in the file — the AI seat had been
    // shown a truncated bundle excerpt and reported honestly about its
    // own window; the pipeline promoted that into a claim about the
    // whole file. `apply_absence_backstop` is the cheap mechanical check
    // that would have caught it for zero tokens.

    /// A minimal `Tier::Confirmed` `JudgedFlag` whose decisive record's
    /// text is `text` — the shape `apply_absence_backstop` reads.
    fn confirmed_flag_with_text(bundle_id: &str, text: &str) -> JudgedFlag {
        JudgedFlag {
            flag: flag(bundle_id, "member-a", 0, "probe charge"),
            pass1: JudgeRecord {
                ruling: JudgeRuling::Confirmed,
                decisive_evidence: text.to_string(),
                note_for_author: String::new(),
                pass: 1,
                seconds: 0.0,
            },
            pass2: None,
            tier: Tier::Confirmed,
            demoted_by_pass2: false,
            verify: None,
            demoted_by_verify: false,
            absence_backstop: None,
        }
    }

    fn one_bundle(id: &str) -> Vec<BundleInput> {
        vec![BundleInput {
            id: id.to_string(),
            fact_family: "unscoped".to_string(),
            code: String::new(),
            probe_code: String::new(),
            facts: Vec::new(),
            manifest: Vec::new(),
        }]
    }

    // ── is_absence_claim / extract_claimed_absent_token / bundle_file_path ──

    #[test]
    fn is_absence_claim_matches_known_phrasing_case_insensitively() {
        assert!(is_absence_claim("This function does not assign `process.exitCode` on the error path"));
        assert!(is_absence_claim("THERE IS NO `.catch` handler attached here"));
        assert!(is_absence_claim("the code never calls `cleanup()` before returning"));
    }

    #[test]
    fn is_absence_claim_ignores_non_absence_wording() {
        assert!(
            !is_absence_claim("this loop is O(n^2) and could be slow on large inputs"),
            "a performance claim is not an absence claim"
        );
        assert!(
            !is_absence_claim("`total` is computed before `rate` is validated, which is a logic error"),
            "an ordering claim is not an absence claim"
        );
    }

    #[test]
    fn extract_claimed_absent_token_requires_exactly_one_backtick_span() {
        assert_eq!(
            extract_claimed_absent_token("does not assign `process.exitCode` on the error path"),
            Some("process.exitCode".to_string())
        );
        assert_eq!(extract_claimed_absent_token(".catch"), None, "no backtick span at all -> abstain");
        assert_eq!(
            extract_claimed_absent_token("does not call `foo` or `bar` anywhere"),
            None,
            "two spans is ambiguous -> abstain"
        );
    }

    #[test]
    fn bundle_file_path_splits_on_last_at_or_falls_back_to_the_bare_id() {
        assert_eq!(bundle_file_path("handleError@src/foo.ts"), Some("src/foo.ts"));
        assert_eq!(bundle_file_path("billing.ts"), Some("billing.ts"), "no `@` -> id IS the path");
        assert_eq!(bundle_file_path(""), None);
    }

    // ── apply_absence_backstop ───────────────────────────────────────

    /// The core failure mode: a `does not assign \`process.exitCode\``
    /// claim, where `process.exitCode` IS present in the whole file (just
    /// outside whatever excerpt the AI seat saw) -> demoted, with a note
    /// naming the token/file/line.
    #[test]
    fn absence_backstop_demotes_when_token_is_present_in_the_whole_file() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("cli.ts"),
            "function main() {\n  doWork();\n}\nprocess.exitCode = 1;\n",
        )
        .unwrap();
        let source = FileSource::worktree(dir.path());
        let bundles = one_bundle("cli.ts");
        let mut judged =
            vec![confirmed_flag_with_text("cli.ts", "does not assign `process.exitCode` on the error path")];

        apply_absence_backstop(&mut judged, &bundles, Some(&source));

        assert_eq!(judged[0].tier, Tier::NeedsCheck, "a contradicted absence claim is demoted, not deleted");
        let note = judged[0].absence_backstop.as_ref().expect("backstop note attached");
        assert_eq!(note.token, "process.exitCode");
        assert_eq!(note.file, "cli.ts");
        assert_eq!(note.line, Some(4), "the token is on line 4 of the fixture file");
        assert!(
            judged[0].pass1.note_for_author.contains("process.exitCode"),
            "the human-facing note also carries the mechanical explanation: {}",
            judged[0].pass1.note_for_author
        );
    }

    /// The MANDATORY inverted case: when the claimed-absent token is
    /// genuinely absent from the whole file too, the claim holds and the
    /// finding stays `Confirmed`, untouched. Without this case, a backstop
    /// that demoted EVERY absence claim would pass the test above too.
    #[test]
    fn absence_backstop_leaves_a_genuine_absence_confirmed() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("cli.ts"), "function main() {\n  doWork();\n}\n").unwrap();
        let source = FileSource::worktree(dir.path());
        let bundles = one_bundle("cli.ts");
        let mut judged =
            vec![confirmed_flag_with_text("cli.ts", "does not assign `process.exitCode` on the error path")];

        apply_absence_backstop(&mut judged, &bundles, Some(&source));

        assert_eq!(judged[0].tier, Tier::Confirmed, "a genuine absence claim is left standing");
        assert!(judged[0].absence_backstop.is_none());
    }

    /// A non-absence claim never even reaches token extraction / a file
    /// read — it is left completely untouched.
    #[test]
    fn absence_backstop_ignores_non_absence_claims() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("cli.ts"), "function main() { doWork(); }\n").unwrap();
        let source = FileSource::worktree(dir.path());
        let bundles = one_bundle("cli.ts");
        let mut judged =
            vec![confirmed_flag_with_text("cli.ts", "this loop is `O(n^2)` and could be slow on large inputs")];

        apply_absence_backstop(&mut judged, &bundles, Some(&source));

        assert_eq!(judged[0].tier, Tier::Confirmed);
        assert!(judged[0].absence_backstop.is_none());
    }

    /// An absence claim with no confidently-extractable token (zero, or
    /// ambiguous multiple, backtick spans) abstains rather than guessing.
    #[test]
    fn absence_backstop_abstains_without_a_confident_token() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("cli.ts"), "process.exitCode = 1;\n").unwrap();
        let source = FileSource::worktree(dir.path());
        let bundles = one_bundle("cli.ts");

        // Zero backtick spans.
        let mut judged_none =
            vec![confirmed_flag_with_text("cli.ts", "does not handle the error case at all")];
        apply_absence_backstop(&mut judged_none, &bundles, Some(&source));
        assert_eq!(judged_none[0].tier, Tier::Confirmed);
        assert!(judged_none[0].absence_backstop.is_none());

        // Two backtick spans — ambiguous which one is "the missing thing".
        let mut judged_two =
            vec![confirmed_flag_with_text("cli.ts", "does not call `setup` or `teardown` anywhere")];
        apply_absence_backstop(&mut judged_two, &bundles, Some(&source));
        assert_eq!(judged_two[0].tier, Tier::Confirmed);
        assert!(judged_two[0].absence_backstop.is_none());
    }

    /// The file being unreadable via `FileSource` (never written, or a
    /// bundle whose id names no real path) abstains — never fails the run.
    #[test]
    fn absence_backstop_abstains_when_the_file_is_unreadable() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        // `cli.ts` is deliberately never written into `dir`.
        let source = FileSource::worktree(dir.path());
        let bundles = one_bundle("cli.ts");
        let mut judged =
            vec![confirmed_flag_with_text("cli.ts", "does not assign `process.exitCode` on the error path")];

        apply_absence_backstop(&mut judged, &bundles, Some(&source));

        assert_eq!(judged[0].tier, Tier::Confirmed, "an unreadable file must never fail or falsely demote");
        assert!(judged[0].absence_backstop.is_none());
    }

    /// No `FileSource` at all (most of this module's own tests) makes the
    /// whole pass a no-op — never a panic, never a spurious demotion.
    #[test]
    fn absence_backstop_is_a_no_op_without_a_file_source() {
        let bundles = one_bundle("cli.ts");
        let mut judged =
            vec![confirmed_flag_with_text("cli.ts", "does not assign `process.exitCode` on the error path")];

        apply_absence_backstop(&mut judged, &bundles, None);

        assert_eq!(judged[0].tier, Tier::Confirmed);
        assert!(judged[0].absence_backstop.is_none());
    }

    /// Only `Tier::Confirmed` flags are ever inspected — a `NeedsCheck` or
    /// `Archived` flag naming a contradicted absence claim is left alone
    /// (nothing to demote it FROM; the backstop only ever tightens
    /// `Confirmed`, never re-litigates the judge's other tiers).
    #[test]
    fn absence_backstop_only_inspects_confirmed_flags() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("cli.ts"), "process.exitCode = 1;\n").unwrap();
        let source = FileSource::worktree(dir.path());
        let bundles = one_bundle("cli.ts");
        let mut flag = confirmed_flag_with_text("cli.ts", "does not assign `process.exitCode` on the error path");
        flag.tier = Tier::NeedsCheck;
        let mut judged = vec![flag];

        apply_absence_backstop(&mut judged, &bundles, Some(&source));

        assert_eq!(judged[0].tier, Tier::NeedsCheck, "unrelated to the backstop — never touched");
        assert!(judged[0].absence_backstop.is_none());
    }

    // ── MUST FIX 1 (PR #1765 merge-gate finding): operand-verb phrases ──
    //
    // "does not check `X`" / "does not handle `X`" name X as the SUBJECT
    // of the missing operation when the sentence reads "does not check
    // the return value of `X`" — X is guaranteed present (it's the thing
    // being called), so a bare presence check demoted a TRUE finding
    // every time. Fix: the claimed-absent token must IMMEDIATELY follow
    // the matched phrase (only whitespace between) — "does not assign
    // `process.exitCode`" keeps working; "does not check the return
    // value of `spawn_worker`" does not.

    /// This must STAY `Confirmed` — `spawn_worker` is the SUBJECT of the
    /// missing check, not the absent thing, and it's present in the file
    /// by construction of the finding (the file calls it). Before the
    /// fix, `content.contains("spawn_worker")` mechanically demoted this
    /// TRUE finding and attached a note asserting the absence claim
    /// "does not hold", which was false.
    #[test]
    fn absence_backstop_leaves_operand_verb_finding_confirmed() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("worker.ts"), "async function run() {\n  spawn_worker();\n}\n").unwrap();
        let source = FileSource::worktree(dir.path());
        let bundles = one_bundle("worker.ts");
        let mut judged = vec![confirmed_flag_with_text(
            "worker.ts",
            "does not check the return value of `spawn_worker` before using it",
        )];

        apply_absence_backstop(&mut judged, &bundles, Some(&source));

        assert_eq!(
            judged[0].tier,
            Tier::Confirmed,
            "the token is the SUBJECT of the missing check, not the absent thing — must not demote"
        );
        assert!(judged[0].absence_backstop.is_none());
    }

    // ── MUST FIX 2 (PR #1765 merge-gate finding): word-boundary containment ──
    //
    // `content.contains(token)` was a bare substring check with no word
    // boundary — it matched `id` inside `identifier`, `err` inside
    // `error`. That both mis-demoted a TRUE finding AND cited a line that
    // has nothing to do with the claim as fabricated evidence.

    /// `id` must NOT be considered present just because `identifier` is —
    /// a genuinely-absent `id` token must stay `Confirmed`.
    #[test]
    fn absence_backstop_word_boundary_rejects_substring_only_match() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("record.ts"),
            "function createRecord() {\n  return { identifier: makeId() };\n}\n",
        )
        .unwrap();
        let source = FileSource::worktree(dir.path());
        let bundles = one_bundle("record.ts");
        let mut judged =
            vec![confirmed_flag_with_text("record.ts", "does not set `id` on the created record")];

        apply_absence_backstop(&mut judged, &bundles, Some(&source));

        assert_eq!(
            judged[0].tier,
            Tier::Confirmed,
            "`id` only occurs as a substring of `identifier` — that is not a real match"
        );
        assert!(judged[0].absence_backstop.is_none());
    }

    /// THE REGRESSION GUARD: MUST FIX 1 and MUST FIX 2 must not blunt the
    /// backstop's whole reason for existing — the production incident's
    /// own shape (`does not assign process.exitCode`, token immediately
    /// follows a SAFE verb, present as a genuine boundary match) must
    /// still demote. If this test fails, the whole feature is inert.
    #[test]
    fn absence_backstop_still_demotes_the_production_incident_shape() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("cli.ts"),
            "function main() {\n  doWork();\n}\nprocess.exitCode = 1;\n",
        )
        .unwrap();
        let source = FileSource::worktree(dir.path());
        let bundles = one_bundle("cli.ts");
        let mut judged =
            vec![confirmed_flag_with_text("cli.ts", "does not assign `process.exitCode` on the error path")];

        apply_absence_backstop(&mut judged, &bundles, Some(&source));

        assert_eq!(
            judged[0].tier,
            Tier::NeedsCheck,
            "the production incident's own shape must still demote, or the whole feature is inert"
        );
        assert!(judged[0].absence_backstop.is_some());
    }

    /// `.catch` (short, leading-dot token) must still work as a
    /// claimed-absent token after the word-boundary fix — the PR
    /// deliberately waives a minimum-length floor for exactly this shape.
    #[test]
    fn absence_backstop_dot_catch_token_still_demotes() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("billing.ts"), "fetchThing().catch(handleError);\n").unwrap();
        let source = FileSource::worktree(dir.path());
        let bundles = one_bundle("billing.ts");
        let mut judged = vec![confirmed_flag_with_text("billing.ts", "there is no `.catch` around this call")];

        apply_absence_backstop(&mut judged, &bundles, Some(&source));

        assert_eq!(judged[0].tier, Tier::NeedsCheck, "`.catch` is present and must still demote");
        let note = judged[0].absence_backstop.as_ref().expect("backstop note attached");
        assert_eq!(note.token, ".catch");
    }

    // ── cheap findings (same area) ──────────────────────────────────

    /// "there is no longer" is a SUPERSESSION claim ("the old thing is
    /// gone"), not an absence claim — the "there is no " phrase must not
    /// prefix-match into "there is no longer" and pull an unrelated
    /// finding into the backstop's scope.
    #[test]
    fn absence_backstop_ignores_there_is_no_longer() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("legacy.ts"), "setLegacyMode(true);\nlegacy_mode = true;\n").unwrap();
        let source = FileSource::worktree(dir.path());
        let bundles = one_bundle("legacy.ts");
        let mut judged = vec![confirmed_flag_with_text(
            "legacy.ts",
            "there is no longer any writer that sets `legacy_mode`",
        )];

        apply_absence_backstop(&mut judged, &bundles, Some(&source));

        assert_eq!(
            judged[0].tier,
            Tier::Confirmed,
            "a supersession claim must never be treated as an absence claim"
        );
        assert!(judged[0].absence_backstop.is_none());
    }

    #[test]
    fn is_absence_claim_does_not_match_there_is_no_longer() {
        assert!(
            !is_absence_claim("there is no longer any writer that sets `legacy_mode`"),
            "a supersession claim is not an absence claim"
        );
        // The un-prefixed phrasing must still be caught.
        assert!(is_absence_claim("there is no writer that sets `legacy_mode`"));
    }

    /// The demotion note must say only what the mechanical check actually
    /// knows (presence, not scope) — "this absence claim does not hold"
    /// over-claims when the real situation could be e.g. "assigned on a
    /// different path than the one claimed". The check cannot tell those
    /// apart, so the wording must not pretend it can.
    #[test]
    fn absence_backstop_note_does_not_overclaim_the_finding_is_false() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("cli.ts"),
            "function main() {\n  doWork();\n}\nprocess.exitCode = 1;\n",
        )
        .unwrap();
        let source = FileSource::worktree(dir.path());
        let bundles = one_bundle("cli.ts");
        let mut judged =
            vec![confirmed_flag_with_text("cli.ts", "does not assign `process.exitCode` on the error path")];

        apply_absence_backstop(&mut judged, &bundles, Some(&source));

        let note = &judged[0].pass1.note_for_author;
        assert!(
            !note.contains("does not hold"),
            "the check knows presence, not scope — must not claim the finding is false: {note}"
        );
        assert!(note.contains("process.exitCode"), "still names the token: {note}");
    }

    // ── pipeline wiring: run_judge_only (sequential path) ─────────────

    /// End-to-end: the sequential `--charges-file` path (`run_judge_only`
    /// -> `finish_review`) applies the backstop BEFORE the tier counts are
    /// computed, so a mechanically-contradicted `confirmed` judge ruling
    /// lands in `env.needs_check`, not `env.confirmed`.
    #[test]
    fn run_judge_only_applies_the_absence_backstop_before_tier_counts() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("billing.ts"), "context line\nprocess.exitCode = 1;\nmore context\n")
            .unwrap();
        let source = FileSource::worktree(dir.path());
        let crew = valid_crew();
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            roles: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
            judge_exhaustion_strict: false,
            bundles: None,
            source: Some(&source),
            workspace: None,
        };
        let flags = vec![flag("billing.ts", "member-a", 0, "`const end = start.plus(30)` off by one")];
        let judge_reply = "```json\n{\"ruling\": \"confirmed\", \"decisive_evidence\": \"e\", \
             \"note_for_author\": \"does not assign `process.exitCode` on the error path\"}\n```";
        let mut cycler = RecordingCycler::new();
        let mut chat = |_call: &ChatCall| Ok(reply(judge_reply));
        let env = run_judge_only(flags, &inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");

        assert_eq!(env.judged.len(), 1);
        assert_eq!(
            env.judged[0].tier,
            Tier::NeedsCheck,
            "the mechanical backstop demoted the mechanically-contradicted claim"
        );
        assert!(env.judged[0].absence_backstop.is_some());
        assert_eq!(env.confirmed, 0);
        assert_eq!(env.needs_check, 1);
    }

    /// (#1442 sibling check for #1748) The absence backstop runs BEFORE the
    /// optional AI verify stage — a mechanically-demoted flag is no longer
    /// `Tier::Confirmed`, so `run_verify_stage`'s per-confirmed-flag filter
    /// skips it entirely: zero verify dispatches for a flag the mechanical
    /// check already caught, saving the AI spend.
    #[test]
    fn run_judge_only_absence_backstop_demotion_skips_the_verify_stage() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("billing.ts"), "fetchThing().catch(handleError);\n").unwrap();
        let source = FileSource::worktree(dir.path());
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
            ("review-verify", vec![staffing("frontier", "verify-model", 1)]),
        ]);
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            roles: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
            judge_exhaustion_strict: false,
            bundles: None,
            source: Some(&source),
            workspace: None,
        };
        let flags = vec![flag("billing.ts", "member-a", 0, "off by one")];
        let judge_reply = "```json\n{\"ruling\": \"confirmed\", \"decisive_evidence\": \"e\", \
             \"note_for_author\": \"there is no `.catch` around this call\"}\n```";
        let verify_calls = std::cell::RefCell::new(0u32);
        let mut cycler = RecordingCycler::new();
        let mut chat = |call: &ChatCall| {
            if call.system == "verify sys" {
                *verify_calls.borrow_mut() += 1;
            }
            Ok(reply(judge_reply))
        };
        let env = run_judge_only(flags, &inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");

        assert_eq!(env.judged[0].tier, Tier::NeedsCheck);
        assert_eq!(*verify_calls.borrow(), 0, "a mechanically-demoted flag never reaches the verify stage");
    }

    /// (#1442) The sequential `--charges-file` verify path (`run_judge_only`
    /// -> `finish_review` -> `run_verify_stage`) shares the graph path's
    /// dispatch.map semantics: a non-empty UNPARSEABLE verify reply is
    /// recorded as `Unparsed` on the FIRST attempt (NO re-dispatch), and the
    /// finding stays `Confirmed` with the manual-verification marker. This
    /// pins the retirement of the historical unparsed-RETRY, which had drifted
    /// from the graph path (whose `retry_on_empty` re-dispatches EMPTY replies
    /// only) — the #1373-class two-paths-diverge failure mode.
    #[test]
    fn sequential_verify_unparsed_reply_is_not_retried() {
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
            ("review-verify", vec![staffing("frontier", "verify-model", 1)]),
        ]);
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            roles: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
            judge_exhaustion_strict: false,
            bundles: None,
            source: None,
            workspace: None,
        };
        let flags = vec![flag("billing.ts", "member-a", 0, "`const end = start.plus(30)` double-counts")];
        let verify_calls = std::cell::RefCell::new(0u32);
        let mut cycler = RecordingCycler::new();
        let mut chat = |call: &ChatCall| {
            if call.system == "verify sys" {
                *verify_calls.borrow_mut() += 1;
                Ok(reply("no verdict here")) // non-empty, unparseable
            } else {
                Ok(reply(CONFIRM_JSON))
            }
        };
        let env = run_judge_only(flags, &inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");
        assert_eq!(
            *verify_calls.borrow(),
            1,
            "an unparseable non-empty reply is recorded on the first attempt, never re-dispatched"
        );
        assert_eq!(env.judged.len(), 1);
        assert_eq!(
            env.judged[0].tier,
            Tier::Confirmed,
            "an inconclusive adjudication keeps the confirmed tier (manual-verification marker downstream)"
        );
        assert_eq!(
            env.judged[0].verify.as_ref().expect("verify record present").ruling,
            VerifyRuling::Unparsed
        );
    }

    /// (#1442) The empty-content retry the graph's dispatch.map keeps
    /// (`retry_on_empty: 1`) IS preserved on the sequential path — an EMPTY
    /// verify reply is re-dispatched ONCE. Here the retry lands a real
    /// verdict, so the flag is `verified`.
    #[test]
    fn sequential_verify_empty_reply_retries_once() {
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
            ("review-verify", vec![staffing("frontier", "verify-model", 1)]),
        ]);
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            roles: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
            judge_exhaustion_strict: false,
            bundles: None,
            source: None,
            workspace: None,
        };
        let flags = vec![flag("billing.ts", "member-a", 0, "`const end = start.plus(30)` double-counts")];
        let verified_json =
            "```json\n{\"ruling\": \"verified\", \"decisive_evidence\": \"real defect confirmed\", \"note_for_author\": \"n\"}\n```";
        let verify_calls = std::cell::RefCell::new(0u32);
        let mut cycler = RecordingCycler::new();
        let mut chat = |call: &ChatCall| {
            if call.system == "verify sys" {
                let n = {
                    let mut c = verify_calls.borrow_mut();
                    *c += 1;
                    *c
                };
                if n == 1 {
                    Ok(reply("")) // empty content — must trigger exactly one retry
                } else {
                    Ok(reply(verified_json))
                }
            } else {
                Ok(reply(CONFIRM_JSON))
            }
        };
        let env = run_judge_only(flags, &inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");
        assert_eq!(*verify_calls.borrow(), 2, "an empty reply is re-dispatched exactly once");
        assert_eq!(env.judged.len(), 1);
        assert_eq!(env.judged[0].tier, Tier::Confirmed);
        assert_eq!(
            env.judged[0].verify.as_ref().expect("verify record present").ruling,
            VerifyRuling::Verified,
            "the retry's real verdict is the recorded ruling"
        );
        assert_eq!(env.verified, 1, "the retry's verdict counts toward the verified tally");
    }

    /// (#1355/#1357 review round; renamed + re-scoped #1876/#1877) `finish_
    /// review`'s judge remote-budget honesty gates are STILL PRODUCTION
    /// CODE via the `--charges-file` path (`mission launch review --param
    /// charges_file=...` -> `run_judge_only` -> `finish_review`) — but every
    /// test that used to pin them routed through the deleted `run_review`
    /// driver, so the migration would have left them coverage-free. This
    /// ONE test pins all three gates through the surviving caller (the
    /// graph path lacks these gates entirely — that's the KNOWN GAP the
    /// migrated graph tests characterize), under the STRICT policy
    /// (`judge_exhaustion_strict: true`, the operator opt-in that restores
    /// today's pre-#1876 "any skip is fatal" behavior):
    ///
    /// 1. the judge's per-pass budget rows reach `env.remote_budgets`;
    /// 2. bucket exhaustion (`skipped > 0`) degrades the run with the
    ///    reason named — never a silent pass (#1260) — because the STRICT
    ///    policy is set; the default (partial) policy's own version of this
    ///    scenario is the companion test right below.
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
    fn run_judge_only_remote_budget_exhaustion_strict_policy_degrades_and_warns() {
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![remote_staffing("cloud", "gpt-judge", 1)]),
        ]);
        let mut inputs = ReviewInputs {
            case_id: "c-judge-only-budget".to_string(),
            roles: &crew,
            intent_title: "t",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 100, // one 600-token ruling exhausts a pass bucket
            judge_exhaustion_strict: true,
            bundles: None,
            source: None,
            workspace: None,
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

    /// (#1876/#1877) The DEFAULT policy's version of the scenario above
    /// (`judge_exhaustion_strict: false`, byte-identical scripted dispatch
    /// order otherwise): one skipped call (f3's pass-1, refused by the
    /// exhausted bucket) alongside two real rulings — f2's confirm and f1's
    /// dispatch-error archive. Fixes the production incident's own shape at
    /// the mechanism that caused it: a skip must not degrade the run when
    /// USABLE rulings exist. "One skipped call out of a thousand" and "one
    /// skipped call out of three" are the same bug; this pins the smaller
    /// case directly, and `synthesize_review`'s own tests
    /// (`src/pr_review.rs`) pin the exact 134-flag production shape on the
    /// render side.
    #[test]
    fn run_judge_only_remote_budget_exhaustion_default_policy_is_partial_not_degenerate() {
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![remote_staffing("cloud", "gpt-judge", 1)]),
        ]);
        let mut inputs = ReviewInputs {
            case_id: "c-judge-only-budget-partial".to_string(),
            roles: &crew,
            intent_title: "t",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 100, // one 600-token ruling exhausts a pass bucket
            judge_exhaustion_strict: false,
            bundles: None,
            source: None,
            workspace: None,
        };
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

        // The budget row still reaches the envelope — coverage data is
        // NEVER dropped, only the VERDICT changes.
        let p1 = env.remote_budgets.iter().find(|r| r.stage == "judge-pass1").expect("judge-pass1 row");
        assert!(p1.exhausted);
        assert_eq!(p1.skipped_calls, 1, "f3's pass-1 was refused by the exhausted bucket, same as strict");

        // The fix: the run is NOT degenerate — real signal (f2's confirm)
        // exists, so the skip is a coverage fact, not a discard-everything
        // verdict.
        assert!(
            env.degenerate.is_none(),
            "a single skip must not degrade a run with usable rulings: {:?}",
            env.degenerate
        );
        assert_eq!(env.judged[1].tier, Tier::Confirmed, "f2's real ruling still renders");
        assert_eq!(env.judged[2].pass1.ruling, JudgeRuling::Error, "f3 is honestly Error, never faked confirmed");

        // The dispatch-error warning (a SEPARATE, always-independent
        // mechanism, #1329) is unaffected by the policy change.
        assert!(
            env.warnings.iter().any(|w| w.contains("remote judge dispatch failed on 1 of 3 flag")),
            "the #1329 warning is independent of the exhaustion policy: {:?}",
            env.warnings
        );
        // (#1876/#1877 QA follow-up) The non-strict Gate 1 skip ALSO lands
        // a coverage warning in `env.warnings` — this is what
        // `review_result_to_mission_envelope` (`src/mission_launch_review.rs`)
        // reads to classify the mission board / CLI exit code `Degraded`
        // instead of `Clean`. Without this, the fix only reached the
        // rendered PR comment, not the board — see
        // `a_partial_coverage_review_is_degraded_not_clean` for the
        // classifier-side pin.
        assert!(
            env.warnings.iter().any(|w| w.contains("remote judge token budget exhausted")),
            "the budget skip must also land a coverage warning, not just the remote_budgets row: {:?}",
            env.warnings
        );
        // (#1888) Pins the Gate 1 non-strict `coverage_warning`'s allowance
        // figure against this fixture's real `remote_max_tokens_per_execution`
        // (100) — a mutation that swaps the interpolated value for a stray
        // literal must fail here.
        assert!(
            env.warnings.iter().any(|w| w.contains("(100 tokens per stage)")),
            "the coverage warning's allowance must be the fixture's real remote_max_tokens_per_execution: {:?}",
            env.warnings
        );

        // `review_outcome` (the render-facing predicate) reads this
        // envelope as Partial, naming the real skip/total numbers.
        let outcome = review_outcome(&env);
        match outcome {
            RunOutcome::Partial { reasons } => {
                assert_eq!(reasons.len(), 1);
                assert!(reasons[0].contains("1 of 3 flags went unjudged"), "got: {}", reasons[0]);
                assert!(reasons[0].contains("judge-pass1"), "got: {}", reasons[0]);
                // (#1888) Same fixture, the OTHER site: `judge_budget_shortfall_reason`'s
                // allowance figure, pinned against the row's own max_tokens (100).
                assert!(
                    reasons[0].contains("100-token allowance"),
                    "the banner's allowance must be the row's own max_tokens, not a stray literal: {}",
                    reasons[0]
                );
            }
            other => panic!("expected Partial, got {other:?}"),
        }
    }

    /// (#1876/#1877 QA follow-up) Gate 2's own "no usable ruling" wording
    /// used to go generic ("all errored/unparsed") whenever the CAUSE was a
    /// non-strict budget exhaustion, because Gate 1 deliberately leaves
    /// `degen_reasons` empty in that policy — losing exactly the diagnosis
    /// an operator most needs (their `remote.max_tokens_per_execution` is
    /// too low for even one ruling, the single most likely misconfiguration
    /// shape) at the moment they need it. A budget of `0` refuses every
    /// call from the first one (`RemoteBudget`'s own doc: "exhausted from
    /// the FIRST call"), so both flags' pass-1 calls are skipped, `usable`
    /// stays 0, and Gate 2 fires — this pins that it fires with the
    /// budget-specific wording, not the generic fallback, regardless of
    /// the non-strict policy.
    #[test]
    fn run_judge_only_total_budget_exhaustion_under_default_policy_still_names_the_budget_in_gate_2() {
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![remote_staffing("cloud", "gpt-judge", 1)]),
        ]);
        let inputs = ReviewInputs {
            case_id: "c-judge-only-total-exhaustion".to_string(),
            roles: &crew,
            intent_title: "t",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 0, // exhausted from the first call, by construction
            judge_exhaustion_strict: false,
            bundles: Some(vec![bundle_input("a.ts"), bundle_input("b.ts")]),
            source: None,
            workspace: None,
        };
        let flags = vec![
            flag("a.ts", "member-a", 0, "charge one"),
            flag("b.ts", "member-a", 0, "charge two"),
        ];
        let mut cycler = RecordingCycler::new();
        let mut chat = |_call: &ChatCall| panic!("a budget of 0 must refuse every call before any dispatch fires");
        let env = run_judge_only(flags, &inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");

        let p1 = env.remote_budgets.iter().find(|r| r.stage == "judge-pass1").expect("judge-pass1 row");
        assert_eq!(p1.skipped_calls, 2, "both flags' pass-1 calls were refused");

        let reason = env
            .degenerate
            .as_deref()
            .expect("zero usable rulings must still degrade under the default policy (Gate 2)");
        assert!(
            reason.contains("remote judge token budget exhausted"),
            "Gate 2 must name the budget as the cause, not the generic wording: {reason}"
        );
        assert!(
            reason.contains("none of the flags that WERE judged produced a usable ruling"),
            "got: {reason}"
        );
        assert!(
            !reason.contains("all errored/unparsed"),
            "the generic fallback wording must not fire when the cause is known: {reason}"
        );
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

    // ── (#1959) review's optional workspace input: filter_bundles_by_workspace / resolve_bundles ──

    fn workspace_spec_with(include: Option<Vec<&str>>, exclude: Option<Vec<&str>>) -> WorkspaceSpec {
        WorkspaceSpec {
            schema_version: None,
            name: Some("test-workspace".to_string()),
            root: None,
            sources: Vec::new(),
            include: include.map(|v| v.into_iter().map(str::to_string).collect()),
            exclude: exclude.map(|v| v.into_iter().map(str::to_string).collect()),
            edges: Vec::new(),
            rules: Vec::new(),
            extras: Default::default(),
        }
    }

    #[test]
    fn filter_bundles_by_workspace_drops_files_the_spec_excludes() {
        let bundles = vec![
            BundleInput {
                id: "billing.ts".to_string(),
                fact_family: "unscoped".to_string(),
                code: "kept".to_string(),
                probe_code: "kept".to_string(),
                facts: Vec::new(),
                manifest: Vec::new(),
            },
            BundleInput {
                id: "vendor/generated.ts".to_string(),
                fact_family: "unscoped".to_string(),
                code: "dropped".to_string(),
                probe_code: "dropped".to_string(),
                facts: Vec::new(),
                manifest: Vec::new(),
            },
        ];
        let spec = workspace_spec_with(None, Some(vec!["vendor/**"]));
        let (kept, skipped) = filter_bundles_by_workspace(bundles, &spec);
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(kept[0].id, "billing.ts");
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].path, "vendor/generated.ts");
        assert_eq!(skipped[0].reason, SkipReason::ExcludedByWorkspaceSpec);
        assert!(skipped[0].function.is_none());
    }

    #[test]
    fn filter_bundles_by_workspace_keeps_everything_when_nothing_excluded() {
        let bundles = vec![BundleInput {
            id: "billing.ts".to_string(),
            fact_family: "unscoped".to_string(),
            code: "kept".to_string(),
            probe_code: "kept".to_string(),
            facts: Vec::new(),
            manifest: Vec::new(),
        }];
        let spec = workspace_spec_with(None, None);
        let (kept, skipped) = filter_bundles_by_workspace(bundles, &spec);
        assert_eq!(kept.len(), 1);
        assert!(skipped.is_empty());
    }

    #[test]
    fn resolve_bundles_is_byte_identical_when_workspace_is_absent() {
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            roles: &valid_crew(),
            intent_title: "",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "",
            judge_system: "",
            verify_system: "",
            remote_max_tokens_per_execution: 500_000,
            judge_exhaustion_strict: false,
            bundles: None,
            source: None,
            workspace: None,
        };
        let bundles = resolve_bundles(&inputs);
        assert_eq!(bundles.len(), 1, "unfiltered — same as bundles_from_diff alone");
        assert_eq!(bundles[0].id, "billing.ts");
    }

    #[test]
    fn resolve_bundles_drops_files_the_workspace_spec_excludes() {
        let spec = workspace_spec_with(None, Some(vec!["billing.ts"]));
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            roles: &valid_crew(),
            intent_title: "",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "",
            judge_system: "",
            verify_system: "",
            remote_max_tokens_per_execution: 500_000,
            judge_exhaustion_strict: false,
            bundles: None,
            source: None,
            workspace: Some(&spec),
        };
        let bundles = resolve_bundles(&inputs);
        assert!(bundles.is_empty(), "{bundles:?}");
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
                absence_backstop: None,
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
                absence_backstop: None,
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
                absence_backstop: None,
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
                StepRecord { step_id: "bundle".to_string(), kind: "procedural".to_string(), items_in: Some(1), items_out: Some(1), wall_ms: 2 },
                StepRecord { step_id: "probe".to_string(), kind: "dispatch".to_string(), items_in: Some(1), items_out: Some(3), wall_ms: 1200 },
                StepRecord { step_id: "dedup".to_string(), kind: "procedural".to_string(), items_in: Some(3), items_out: Some(3), wall_ms: 1 },
                StepRecord { step_id: "judge-pass1".to_string(), kind: "dispatch".to_string(), items_in: Some(3), items_out: Some(3), wall_ms: 500 },
                StepRecord { step_id: "judge-pass2".to_string(), kind: "dispatch".to_string(), items_in: Some(2), items_out: Some(2), wall_ms: 300 },
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
            bundle_skip: None,
            bundler_fallback: None,
            degenerate_kind: None,
            probe_retries: 0,
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
            roles: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
            judge_exhaustion_strict: false,
            bundles: Some(bundles),
            source: None,
            workspace: None,
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

    // ── (branch: bundler/coverage-gap-audit) the PROBE seat's blind spots
    //    ────────────────────────────────────────────────────────────────
    //
    // See `crates/darkmux-lab/src/lab/bundle/mod.rs`'s coverage-gap audit
    // report (findings #4 and #5) for the full writeup. Both tests below
    // are the review.rs half of that audit — they exercise the PROBE
    // seat's prompt builder directly, the seat that actually ORIGINATES a
    // finding (as opposed to `manifest_never_reaches_the_dispatched_judge_
    // prompt` just above, which is an EXISTING, deliberate, #1256-cited
    // exclusion on the JUDGE side).

    /// Finding #4 (#1754 fix). `bundle_inputs_from_set` renders the SAME
    /// code refs through two formatters: `slice_code` (-> `BundleInput.
    /// code`, what the judge sees) embeds an explicit "excerpt truncated"
    /// marker inline when a callee ref is a header-only stub of a longer
    /// body; `slice_code_probe` (-> `BundleInput.probe_code`, what the
    /// probe sees) now does too — a DELIBERATE DIVERGENCE from
    /// `probe-runner.py`'s `read_code_excerpt` (which has no notion of a
    /// truncated stub at all), not a port correction. The seat most
    /// likely to raise a "this looks incomplete" finding now gets the
    /// same textual signal the judge seat already had that its excerpt is
    /// a stub, not the whole function.
    #[test]
    fn probe_seat_now_sees_the_truncation_marker_the_judge_seat_already_got_inline() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut long_body = String::from("export function longHelper(x) {\n");
        for i in 0..50 {
            long_body.push_str(&format!("  console.log({i});\n"));
        }
        long_body.push_str("  return x;\n}\n");
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/helpers.ts"), &long_body).unwrap();
        std::fs::write(
            dir.path().join("src/caller.ts"),
            "import { longHelper } from './helpers';\nfunction useIt(x) {\n  return longHelper(x);\n}\n",
        )
        .unwrap();
        let diff = "+++ b/src/caller.ts\n\
@@ -0,0 +1,3 @@\n\
+import { longHelper } from './helpers';\n\
+function useIt(x) {\n\
+  return longHelper(x);\n\
+}\n";
        let source = FileSource::worktree(dir.path());
        let set = build_bundles(&source, diff).expect("build_bundles over the truncation fixture");
        assert!(!set.bundles.is_empty(), "expected at least one bundle for useIt");
        assert!(
            set.bundles[0].truncated,
            "fixture must actually exercise a truncated callee, or this test proves nothing: {:?}",
            set.bundles[0]
        );
        let inputs =
            bundle_inputs_from_set(&set, &source).expect("bundle_inputs_from_set over the truncation fixture");
        let b = &inputs[0];
        assert!(
            b.code.contains("excerpt truncated"),
            "the judge's rendering (`BundleInput.code`) must carry the truncation marker inline: {}",
            b.code
        );
        assert!(
            b.probe_code.to_lowercase().contains("truncat"),
            "the probe's rendering (`BundleInput.probe_code`) must ALSO carry a truncation marker — \
             the probe seat that raises the finding must learn the excerpt is a stub, same as the \
             judge seat: {}",
            b.probe_code
        );
    }

    /// Finding #5 (#1755 — DECIDED). Unlike the judge side (tested and
    /// cited above, #1256's "match Phase A exactly" operator decision),
    /// nothing previously pinned `probe_user_message`'s exclusion of
    /// `bundle.manifest` as deliberate — a future edit could have added
    /// it in one seat and not the other with zero signal either way.
    ///
    /// DECISION: the probe seat does NOT get the manifest either. Phase
    /// A's `probe-runner.py` never had a manifest field to drop in the
    /// first place (`bundler.py`'s bundles carry no such field), so this
    /// isn't quite "parity" the way the judge's exclusion is — but the
    /// practical effect the operator cares about is the same: the
    /// manifest is a Rust-only addition (#1222 packet 3) with no Phase A
    /// precedent, and it stays out of every model-facing prompt, not just
    /// the judge's, until a real consumer needs it. Kept conservative on
    /// purpose: injecting new content into a probe prompt is a
    /// model-facing-prompt change (see this repo's AI-convention/term-
    /// provenance doctrine), not something to do as a side effect of an
    /// audit finding with no operator request behind it.
    ///
    /// Pinned as a STRUCTURAL contract, not just a substring check:
    /// `probe_user_message`'s output must be byte-identical whether
    /// `bundle.manifest` is empty or populated — so this test fails if a
    /// future edit reads the field for ANYTHING, not only if it happens
    /// to leak the exact word "manifest" or a specific symbol name.
    #[test]
    fn manifest_never_reaches_the_probe_user_message() {
        let mut bundle = BundleInput {
            id: "billing.ts".to_string(),
            fact_family: "unscoped".to_string(),
            code: "const end = start.plus(30)".to_string(),
            probe_code: "### `billing.ts` (lines 1-1)\n```typescript\nconst end = start.plus(30)\n```"
                .to_string(),
            facts: vec![],
            manifest: vec![],
        };
        let without_manifest = probe_user_message("prior text", &bundle);
        bundle.manifest = vec!["referenced but not defined in bundle: helperFn <- unknown".to_string()];
        let with_manifest = probe_user_message("prior text", &bundle);
        assert_eq!(
            without_manifest, with_manifest,
            "the probe prompt must be byte-identical regardless of `bundle.manifest` — this is a \
             DECIDED contract (#1755), not an accident: a populated manifest must never leak into, \
             or otherwise affect, the dispatched probe prompt"
        );
        assert!(!with_manifest.contains("helperFn"), "{with_manifest}");
        assert!(!with_manifest.to_lowercase().contains("manifest"), "{with_manifest}");
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
            role_id: None,
            pm: remote_pm(model),
            k,
            passes: 2,
            max_tokens: None,
            selector: None,
            provenance: None,
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

    /// The verify seat is genuinely optional: a hand-built fixture that
    /// declares one is `Some`; one that doesn't is `None`. (#1512, #1513
    /// review: `validate_review_crew`'s own "exactly 1 verify staffing when
    /// declared" shape check retired with it — a `ResolvedReviewRoles`
    /// built by hand carries `verify: Option<ResolvedSeatStaffing>`
    /// directly, valid by construction; the equivalent production coverage
    /// is `resourcing.rs`'s `resolve_review_roles_verify_absent_from_the_
    /// document_resolves_to_none`.)
    #[test]
    fn crew_with_verify_seat_is_genuinely_optional() {
        let ok = crew_with(vec![
            ("review-probe", vec![staffing("fast", "a", 1)]),
            ("review-judge", vec![staffing("fast", "b", 1)]),
            ("review-verify", vec![staffing("frontier", "c", 1)]),
        ]);
        assert!(ok.verify.is_some());

        let absent = valid_crew();
        assert!(absent.verify.is_none());
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

    /// (CONSIDER c) `RemoteBudget::exhausted()` boundary: under < at == over.
    /// A mutation of `>=` to `>` must fail this table (the `at` row).
    #[test]
    fn remote_bucket_exhausted_boundary_table() {
        let mut under = RemoteBudget::with_stage("s", 100, MIN_VIABLE_JUDGE_GRANT);
        under.spend(99, 1);
        assert!(!under.exhausted(), "under budget: 99 < 100");

        let mut at = RemoteBudget::with_stage("s", 100, MIN_VIABLE_JUDGE_GRANT);
        at.spend(100, 1);
        assert!(at.exhausted(), "at budget: 100 >= 100 (a `>` mutation breaks here)");

        let mut over = RemoteBudget::with_stage("s", 100, MIN_VIABLE_JUDGE_GRANT);
        over.spend(101, 1);
        assert!(over.exhausted(), "over budget: 101 >= 100");
    }

    /// (#swarm-6) `admit_reserve` debits the granted cap AT ADMISSION, so
    /// concurrent judges sharing the bucket under a briefly-held lock can't
    /// all admit against the same untouched balance. Under the old
    /// `admit()`/`spend()` pair with the lock narrowed, two callers near the
    /// limit would BOTH admit unclamped and overshoot the #1260 ceiling by
    /// a full call each — which is exactly why the old code held the mutex
    /// across the whole dispatch (correct, but it serialized the
    /// `judge_concurrency` knob into a no-op). Reservation is what makes
    /// the narrow lock safe; this pins it.
    /// (#1610) A grant too small to buy a parseable ruling is a DENIAL.
    ///
    /// The bug: `admit_reserve` clamped without a floor, so a judge call
    /// requesting 20,000 tokens against 60 remaining was dispatched with
    /// `max_tokens: 60`. The reply truncates → parses as `Unparsed` →
    /// classifies as `Reject` → `multi_pass_confirm` archives the flag on
    /// pass 1 with an empty note and no confirmation pass. A real finding,
    /// deleted.
    ///
    /// And silently: `Some(60)` meant `skipped` never incremented, so no
    /// degraded gate fired and the envelope reported healthy. A flag that
    /// could not be judged must read as UNJUDGED, never as rejected.
    #[test]
    fn a_grant_too_small_to_buy_a_ruling_is_denied_and_counted_as_skipped() {
        let mut b = RemoteBudget::with_stage("s", 100_000, MIN_VIABLE_JUDGE_GRANT);
        // Spend down to a sliver.
        let g = b.admit_reserve(99_900).unwrap();
        b.settle(g, 99_900, 1);
        assert_eq!(b.remaining(), 100, "a sliver remains — not exhausted");
        assert!(!b.exhausted(), "the old code reached the clamp precisely here");

        // The old behavior was `Some(100)` — a cap that cannot close a JSON
        // ruling. It is now refused, and the refusal is REPORTED.
        let before = b.skipped();
        assert_eq!(
            b.admit_reserve(20_000),
            None,
            "a 100-token cap on a 20k ruling request must not be dispatched"
        );
        assert_eq!(b.skipped(), before + 1, "the denial is visible to the degraded gates");
        // Denying must not consume the sliver — a later, smaller-appetite
        // caller is still entitled to it.
        assert_eq!(b.remaining(), 100, "a denied request reserves nothing");
    }

    #[test]
    fn remote_bucket_admit_reserve_prevents_concurrent_overshoot() {
        // (#1610) Realistic units. The reservation property this pins is
        // scale-free, but `admit_reserve` now denies a grant too small to buy
        // a parseable ruling — so a fixture in tens of tokens would trip that
        // floor and test the wrong thing. Same arithmetic, real magnitudes.
        let mut b = RemoteBudget::with_stage("s", 100_000, MIN_VIABLE_JUDGE_GRANT);
        // First caller wants 80k — granted in full, and RESERVED.
        assert_eq!(b.admit_reserve(80_000), Some(80_000));
        // Second caller wants 80k — the reservation is already debited, so
        // the grant clamps to what genuinely remains, never a fresh 80k.
        assert_eq!(b.admit_reserve(80_000), Some(20_000));
        // Third caller: exhausted, refused, counted as skipped.
        assert_eq!(b.admit_reserve(10_000), None);

        // Settle releases the unspent part of a reservation back.
        let mut c = RemoteBudget::with_stage("s", 100_000, MIN_VIABLE_JUDGE_GRANT);
        let granted = c.admit_reserve(80_000).unwrap();
        c.settle(granted, 30_000, 1);
        assert_eq!(c.remaining(), 70_000, "unspent reservation returns to the pool");
        // …and an endpoint reporting ABOVE its cap pushes the bucket over —
        // the documented soft-ceiling overshoot, same reading as the map path.
        let granted2 = c.admit_reserve(80_000).unwrap();
        c.settle(granted2, 90_000, 1);
        assert!(c.exhausted(), "over-report lands as real spend: 30k+90k >= 100k");
    }

    /// (#1877 pre-move conformance) `admit()`/`spend()` — the SEQUENTIAL
    /// unshared pair the verify stage uses (never `admit_reserve`/`settle`).
    /// Pinned directly rather than only through the verify e2e tests, since
    /// this is the pair the #1877 extraction's unified type must keep
    /// byte-identical for the one caller that still uses it.
    #[test]
    fn remote_bucket_sequential_admit_then_spend_gates_and_accounts() {
        let mut b = RemoteBudget::with_stage("s", 100, MIN_VIABLE_JUDGE_GRANT);
        assert!(b.admit(), "a fresh bucket admits");
        b.spend(60, 1);
        assert!(!b.exhausted(), "60 < 100");
        assert!(b.admit(), "still under budget");
        b.spend(50, 1);
        assert!(b.exhausted(), "110 >= 100");
        // Exhausted: admit refuses and counts the refusal, spend is never
        // reached by a correct caller (verify's own loop gates on this).
        assert!(!b.admit(), "exhausted bucket refuses");
        assert_eq!(b.skipped(), 1, "the refusal is counted");
    }

    /// (#1877 pre-move conformance) `record()`'s emitted `RemoteBudgetRecord`
    /// — direct unit coverage of the fields the envelope actually carries,
    /// rather than only exercising them indirectly through a full pipeline
    /// run. Pinned before the #1877 move so the unified type's `record()`
    /// can be checked against the exact same fixture afterward.
    #[test]
    fn remote_bucket_record_reports_stage_budget_used_exhausted_and_skips() {
        let mut b = RemoteBudget::with_stage("judge-pass1", 1_000, MIN_VIABLE_JUDGE_GRANT);
        let g = b.admit_reserve(600).expect("first draw admits");
        b.settle(g, 600, 1);
        // Second draw is denied by the floor (400 remaining < 512 floor).
        assert!(b.admit_reserve(600).is_none(), "a starved grant is denied");
        let rec = b.record().expect("a bucket with calls must emit a row");
        assert_eq!(rec.stage, "judge-pass1");
        assert_eq!(rec.max_tokens, 1_000);
        assert_eq!(rec.used_tokens, 600);
        assert!(!rec.exhausted, "600 < 1000 is not exhausted");
        assert_eq!(rec.skipped_calls, 1);
    }

    /// (#1877 pre-move conformance) A bucket that never admitted or skipped
    /// a call emits no row — local-only envelopes carry no budget rows.
    #[test]
    fn remote_bucket_record_is_none_when_the_stage_never_touched_it() {
        let b = RemoteBudget::with_stage("verify", 1_000, MIN_VIABLE_JUDGE_GRANT);
        assert!(b.record().is_none(), "an untouched bucket emits no row");
    }

    // ─── (#1230/#1341 DRY pass) Task/Step graph orchestration ───────────

    /// (#1530) `bundles` no longer lands on `ReviewStepContext` directly
    /// (that field retired — bundling is now `review-bundle-step`'s own
    /// run-time work) — this helper instead wires it through
    /// `bundle_override`, so a full graph run through `run_review_graph`
    /// (its own `review-bundle-step` reads the override at run time,
    /// publishing straight onto `REVIEW_BUNDLES_ARTIFACT`) behaves exactly
    /// like every test did before this packet. Tests that call an isolated
    /// StepKind's `run_streaming`/`residency` directly (bypassing the bundle
    /// step entirely) additionally seed `REVIEW_BUNDLES_ARTIFACT` by hand —
    /// see those tests' own hand-built `ArtifactBus`es.
    fn step_ctx(crew: &ResolvedReviewRoles, bundles: Vec<BundleInput>) -> Arc<ReviewStepContext> {
        Arc::new(ReviewStepContext {
            case_id: "case-1".to_string(),
            roles: crew.clone(),
            intent_title: String::new(),
            intent_body: String::new(),
            diff: DIFF.to_string(),
            probe_system: "probe prior".to_string(),
            probe_role_prompts: BTreeMap::new(),
            judge_system: "judge persona".to_string(),
            verify_system: "verify persona".to_string(),
            remote_max_tokens_per_execution: 500_000,
            judge_exhaustion_strict: false,
            timeout_seconds: 30,
            chat_override: None,
            bundle_override: Some(Arc::new(move || Ok(bundles.clone()))),
            mission_id: None,
        })
    }

    /// [`step_ctx`]'s dummy [`BundleBuildSpec`] — every graph-test call site
    /// wires bundles through `bundle_override` (see `step_ctx`'s own doc),
    /// so `review-bundle-step`'s config is stamped from this but never
    /// actually consulted; it exists only because `build_review_graph`'s
    /// signature always needs one.
    fn dummy_bundle_spec() -> BundleBuildSpec {
        BundleBuildSpec {
            source: BundleSourceSpec::Worktree { path: PathBuf::from("/nonexistent-test-worktree") },
            bundler: None,
            diff_file: PathBuf::from("/nonexistent-test-diff"),
        }
    }

    /// (#1530) The launcher's `BundleBuildSpec` -> `Step.config` STAMP and the
    /// step's `file_source_from_step_config` READER agree only by two
    /// hand-written `json!`/`get()` literals in the same file. Nothing else
    /// pins them together, so a rename on one side is a silent break — and it
    /// breaks at RUN time, inside the graph, after the mission has minted.
    ///
    /// Covers the `github` shape specifically: that is what the real CI
    /// review uses (`--param github=<repo> --param head_sha=<sha>`), and it
    /// was the one source shape with ZERO coverage on the reconstruction path
    /// — the bench and the other new tests only ever build `worktree` specs.
    #[test]
    fn bundle_spec_stamp_and_reader_agree_for_every_source_shape() {
        for spec in [
            BundleBuildSpec {
                source: BundleSourceSpec::Github {
                    repo: "kstrat2001/darkmux".to_string(),
                    head_sha: "deadbeefcafe".to_string(),
                },
                bundler: None,
                diff_file: PathBuf::from("/tmp/some-diff.patch"),
            },
            BundleBuildSpec {
                source: BundleSourceSpec::Worktree { path: PathBuf::from("/tmp/some-worktree") },
                bundler: Some("my-bundler".to_string()),
                diff_file: PathBuf::from("/tmp/some-diff.patch"),
            },
        ] {
            let graph = build_review_graph(
                std::sync::Arc::new(ReviewStepContext::default()),
                &spec,
                graph_staffing("fast", "judge-model", 1),
                None,
                &[graph_staffing("fast", "probe-model", 1)],
                "investigate",
                "adjudicate",
                "report",
                1,
            )
            .expect("graph builds");
            let config = &graph
                .steps
                .get("review-bundle-step")
                .expect("the bundle step exists")
                .config;

            // The reader must accept what the stamper wrote — key names,
            // nesting, and all. A mismatch surfaces here instead of at run
            // time on a real review.
            file_source_from_step_config(config).unwrap_or_else(|e| {
                panic!("the bundle step's reader rejected the launcher's own stamp: {e:#}")
            });
            assert_eq!(
                config.get("bundler").and_then(|v| v.as_str()),
                spec.bundler.as_deref(),
                "the bundler command must survive the stamp verbatim"
            );
            assert_eq!(
                config.get("diff_file").and_then(|v| v.as_str()),
                Some(spec.diff_file.display().to_string().as_str()),
                "the diff path must survive the stamp verbatim (the external bundler reads it)"
            );
        }
    }

    /// (#1748) `review-judge-step` carries the SAME `"source"` block
    /// `review-bundle-step` does — the mechanical absence-claim backstop
    /// (`ReviewJudgeStepKind::run_streaming`) needs its own `FileSource` to
    /// check a confirmed finding against the whole file. Same
    /// stamp-and-reader-agree discipline as the bundle-step test above,
    /// applied to the judge step's copy of the block.
    #[test]
    fn judge_step_config_also_carries_source_for_the_absence_backstop() {
        let spec = BundleBuildSpec {
            source: BundleSourceSpec::Worktree { path: PathBuf::from("/tmp/some-worktree") },
            bundler: None,
            diff_file: PathBuf::from("/tmp/some-diff.patch"),
        };
        let graph = build_review_graph(
            std::sync::Arc::new(ReviewStepContext::default()),
            &spec,
            graph_staffing("fast", "judge-model", 1),
            None,
            &[graph_staffing("fast", "probe-model", 1)],
            "investigate",
            "adjudicate",
            "report",
            1,
        )
        .expect("graph builds");
        let config = &graph.steps.get("review-judge-step").expect("the judge step exists").config;

        let source = file_source_from_step_config(config).unwrap_or_else(|e| {
            panic!("the judge step's reader rejected the launcher's own stamp: {e:#}")
        });
        assert!(
            matches!(&source, FileSource::Worktree(p) if p.as_path() == Path::new("/tmp/some-worktree")),
            "the judge step's reconstructed FileSource must match the launcher's own bundle_spec"
        );
    }

    /// (#1530 Packet 3a follow-on) `ReviewDedupStepKind`/
    /// `ReviewSynthesisStepKind` gained NO constructor fields — the
    /// mint-time `probe_specs`/`remote_budget` (dedup) and
    /// `dedup_task_id`/`judge_task_id`/`verify_task_id`/`remote_budget`
    /// (synthesis) are stamped onto EACH step's own `Step.config` by
    /// `build_review_graph_from_config` and read back by
    /// [`dedup_config_from_step`]/[`synthesis_task_ids_from_step`] — two
    /// hand-written call sites (the stamper, the reader) with no compiler
    /// check keeping them in sync, exactly the risk
    /// `bundle_spec_stamp_and_reader_agree_for_every_source_shape` guards
    /// against on the bundle step. Covers both the no-verify-seat and
    /// verify-seat-staffed shapes (the synthesis step's config gains three
    /// extra keys only in the latter — see `build_review_graph_from_config`'s
    /// own doc), and a two-probe crew (so `probe_specs` round-trips more
    /// than one entry, in claim order).
    #[test]
    fn dedup_and_synthesis_config_stamp_and_reader_agree_with_and_without_verify_seat() {
        let probes = vec![graph_staffing("phigh", "probe-model-a", 1), graph_staffing("plow", "probe-model-b", 1)];
        for verify in [None, Some(graph_staffing("careful", "verify-model", 1))] {
            let graph = build_review_graph(
                // (#1530) A DISTINCTIVE budget, not `default()`'s 0 — otherwise
                // `synth == dedup` is `0 == 0` and would still pass if a future
                // change stamped a literal zero or read the wrong field.
                std::sync::Arc::new(ReviewStepContext {
                    remote_max_tokens_per_execution: 12_345,
                    ..Default::default()
                }),
                &dummy_bundle_spec(),
                graph_staffing("fast", "judge-model", 1),
                verify.clone(),
                &probes,
                "investigate",
                "adjudicate",
                "report",
                1,
            )
            .expect("graph builds");

            let dedup_config =
                &graph.steps.get("review-dedup-step").expect("the dedup step exists").config;
            let (probe_specs, dedup_remote_budget) = dedup_config_from_step(dedup_config)
                .unwrap_or_else(|e| panic!("the dedup step's reader rejected the stamper's own config: {e:#}"));
            assert_eq!(probe_specs.len(), probes.len(), "every claimed probe seat must round-trip a spec");
            assert_eq!(
                probe_specs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
                vec!["phigh", "plow"],
                "probe spec identity must survive the stamp, in claim order"
            );

            let synthesis_config =
                &graph.steps.get("review-synthesis-step").expect("the synthesis step exists").config;
            let (dedup_task_id, judge_task_id, verify_task_id, synth_remote_budget) =
                synthesis_task_ids_from_step(synthesis_config).unwrap_or_else(|e| {
                    panic!("the synthesis step's reader rejected the stamper's own config: {e:#}")
                });
            assert_eq!(dedup_task_id, "review-dedup-task", "task ids are FIXED, unaffected by phase-id args");
            assert_eq!(judge_task_id, "review-judge-task");
            assert_eq!(verify_task_id, "review-verify-task");
            assert_eq!(dedup_remote_budget, 12_345, "the dedup stamp must carry the ctx's real budget");
            assert_eq!(synth_remote_budget, 12_345, "the synthesis stamp must carry the same real budget");
            // (#1530) The join key attribution depends on (#1541) and the
            // seat identity the envelope reports — pinned here so the
            // round-trip is self-contained rather than leaning on the golden.
            assert_eq!(
                probe_specs.iter().map(|s| s.identifier.as_str()).collect::<Vec<_>>(),
                probes.iter().map(|p| seat_identifier(&p.pm)).collect::<Vec<_>>(),
                "each seat's dispatch identifier must survive the round trip"
            );
            assert!(
                probe_specs.iter().all(|s| s.draw_task_ids.len() == 1 && !s.draw_task_ids[0].is_empty()),
                "draw_task_ids is the key reconstruct_probe_stage joins on — it must survive: {probe_specs:?}"
            );
            assert_eq!(
                synth_remote_budget, dedup_remote_budget,
                "both steps stamp the SAME per-execution remote budget"
            );

            assert_eq!(
                synthesis_config.get("verify_identifier").is_some(),
                verify.is_some(),
                "verify_identifier (and its remote/endpoint_host siblings) is present iff a \
                 verify seat was staffed"
            );
        }
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
            role_id: None,
            pm: graph_pm(model),
            k,
            passes: 2,
            max_tokens: None,
            selector: None,
            provenance: None,
        }
    }

    /// A crew of `graph_staffing` seats — the graph-hermetic equivalent of
    /// `valid_crew()`.
    fn graph_valid_crew() -> ResolvedReviewRoles {
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
        crew: &ResolvedReviewRoles,
        bundles: Vec<BundleInput>,
        chat_fn: impl Fn(&ChatCall) -> Result<SingleShotReply> + Send + Sync + 'static,
    ) -> Arc<ReviewStepContext> {
        step_ctx_with_chat_and_budget(crew, bundles, 500_000, chat_fn)
    }

    /// [`step_ctx_with_chat`] with a caller-chosen remote per-execution
    /// token budget — for the budget-exhaustion tests below.
    fn step_ctx_with_chat_and_budget(
        crew: &ResolvedReviewRoles,
        bundles: Vec<BundleInput>,
        remote_max_tokens_per_execution: u64,
        chat_fn: impl Fn(&ChatCall) -> Result<SingleShotReply> + Send + Sync + 'static,
    ) -> Arc<ReviewStepContext> {
        Arc::new(ReviewStepContext {
            case_id: "case-1".to_string(),
            roles: crew.clone(),
            intent_title: String::new(),
            intent_body: String::new(),
            diff: DIFF.to_string(),
            probe_system: "probe prior".to_string(),
            probe_role_prompts: BTreeMap::new(),
            judge_system: "judge persona".to_string(),
            verify_system: "verify persona".to_string(),
            remote_max_tokens_per_execution,
            // (#1876/#1877) Default `false` (partial policy) — callers that
            // want the strict pin flip it after construction via
            // `Arc::get_mut` (this helper's single strong ref is still
            // uncloned at that point).
            judge_exhaustion_strict: false,
            timeout_seconds: 30,
            chat_override: Some(Arc::new(chat_fn)),
            // (#1530) See `step_ctx`'s own doc — same `bundle_override` wiring.
            bundle_override: Some(Arc::new(move || Ok(bundles.clone()))),
            mission_id: None,
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
        // (#1512, #1513 review) `ctx.roles` is already the validated,
        // resolved shape — no separate crew-validation step.
        let judge = ctx.roles.judge.clone();
        let verify = ctx.roles.verify.clone();
        let probes = ctx.roles.probes.clone();
        let fingerprint_val = fingerprint(&seat_identifier(&judge.pm), &ctx.judge_system);
        let staffing_snap = staffing_snapshot(&probes, &judge, verify.as_ref(), ctx.roles.request_changes);
        let crew_name = ctx.roles.distinct_profile_names();
        let graph = build_review_graph(
            ctx.clone(),
            &dummy_bundle_spec(),
            judge,
            verify,
            &probes,
            "investigate",
            "adjudicate",
            "report",
            1,
        )?;
        let (env, _steps) = run_review_graph(
            ctx,
            &crew_name,
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
        // (#1512) Two hand-built probe seats, no `role_id` — claimed
        // POSITIONALLY against review.json's three declared probe tasks
        // (review-probe-high-task, then -mid-task), pruning the unclaimed
        // third (-low-task). `k` no longer multiplies dispatch tasks (one
        // role, one task) — it's carried on the staffing purely for
        // envelope/back-compat reporting.
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model-a", 1), staffing("slow", "probe-model-b", 1)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ]);
        let ctx = step_ctx(&crew, vec![]);

        let graph = build_review_graph(
            ctx,
            &dummy_bundle_spec(),
            crew.judge.clone(),
            crew.verify.clone(),
            &crew.probes,
            "investigate",
            "adjudicate",
            "report",
            1,
        )
        .expect("built-in review config builds cleanly");

        // bundle(1) + probe(2 claimed, 1 pruned) + dedup(1) = investigate's 4 tasks.
        let investigate_tasks: Vec<_> = graph.tasks.iter().filter(|t| t.phase_id == "investigate").collect();
        assert_eq!(investigate_tasks.len(), 4, "bundle + 2 claimed probe tasks + dedup (the 3rd declared probe task is pruned, unclaimed)");
        // (#1530 follow-on, Packet A1) Each claimed probe task is now TWO
        // sequential steps — the Tier-3 `review.probe-render` step, then
        // the generic `dispatch.map` — mirroring the verify task's own
        // render/dispatch.map split (asserted below at
        // `review-verify-render-step`/`review-verify-step`).
        let probe_map_steps: Vec<_> = graph
            .steps
            .values()
            .filter(|s| s.id.starts_with("review-probe-") && s.kind == "dispatch.map")
            .collect();
        assert_eq!(probe_map_steps.len(), 2, "one dispatch.map step per CLAIMED probe seat — no k fan-out (#1512)");
        assert!(
            probe_map_steps.iter().all(|s| s.config["bucket_group"] == "probe"),
            "all probe map steps share ONE bucket_group"
        );
        let probe_render_steps: Vec<_> = graph
            .steps
            .values()
            .filter(|s| s.kind == "review.probe-render")
            .collect();
        assert_eq!(probe_render_steps.len(), 2, "one review.probe-render step per CLAIMED probe seat");
        let adjudicate_tasks: Vec<_> = graph.tasks.iter().filter(|t| t.phase_id == "adjudicate").collect();
        assert_eq!(adjudicate_tasks.len(), 1, "judge only");
        let report_tasks: Vec<_> = graph.tasks.iter().filter(|t| t.phase_id == "report").collect();
        assert_eq!(report_tasks.len(), 2, "verify (render + map) + synthesis");
        // (#1442 ship-2b) The verify task is two sequential steps: the
        // Tier-3 render step, then the generic dispatch.map.
        let verify_task = graph.tasks.iter().find(|t| t.id == "review-verify-task").unwrap();
        assert_eq!(
            verify_task.step_ids,
            vec!["review-verify-render-step".to_string(), "review-verify-step".to_string()],
            "render precedes the map within the verify task"
        );
        assert_eq!(graph.steps["review-verify-render-step"].kind, "review.verify-render");
        assert_eq!(graph.steps["review-verify-step"].kind, "dispatch.map");

        // (#1619) Cross-phase DATA now rides `Task.reads` (the output
        // ledger); `depends_on` is left for the ordering the graph should
        // DRAW. Judge still receives dedup's docket and still cannot start
        // early — `reads` orders identically in the scheduler — but the
        // investigate→adjudicate task connector no longer renders.
        let tasks_by_id: std::collections::BTreeMap<&str, &Task> =
            graph.tasks.iter().map(|t| (t.id.as_str(), t)).collect();
        let dedup_step_id = "review-dedup-step";
        let judge_task = tasks_by_id["review-judge-task"];
        assert_eq!(judge_task.depends_on, Vec::<String>::new());
        assert_eq!(judge_task.reads, vec!["review-dedup-task".to_string()]);
        assert_eq!(graph.phase_id_of_step[dedup_step_id], "investigate");
        assert_eq!(graph.phase_id_of_step["review-judge-step"], "adjudicate");

        // report's synthesis TASK still receives dedup (investigate), judge
        // (adjudicate — #1442: the judged docket arrives directly, since
        // verify's own output is now the generic map's result array), and
        // verify (report) — the cross-phase pair via the ledger, the
        // same-phase verify via the one remaining drawn edge.
        let synth_task = tasks_by_id["review-synthesis-task"];
        assert!(synth_task.reads.contains(&"review-dedup-task".to_string()));
        assert!(synth_task.reads.contains(&"review-judge-task".to_string()));
        assert_eq!(synth_task.depends_on, vec!["review-verify-task".to_string()]);
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
        // (#1442 ship-2b) `funnel.probe:<seat>` / `funnel.verify` retired
        // WITH their kinds — no live implementation remains to alias to;
        // persisted historical steps still LABEL via
        // `review_step_kind_display_name`'s read-path entries.
        for legacy in ["funnel.bundle", "funnel.dedup", "funnel.judge", "funnel.synthesis"] {
            assert!(graph.registry.get(legacy).is_ok(), "legacy kind id `{legacy}` must still resolve");
        }

        // ONE call is the whole point: no separate driver loop needed to
        // reach every step — `depends_on` alone determines readiness.
        // (#1530 follow-on, Packet A1) Each claimed probe task is now TWO
        // steps (render + map), mirroring the verify task's own split.
        assert_eq!(
            graph.steps.len(),
            10,
            "bundle + 2 claimed probe (render + map) tasks + dedup + judge + verify render + verify map + synthesis"
        );

        // (#1513 review C1) The SAME scenario above prunes the third,
        // unclaimed declared probe task — that pruning must be LOUD, not
        // silent, since a production run pruning a declared task is exactly
        // the reduced-coverage failure mode a Studio hand-edit can trigger.
        // (#1530 Packet 1) `BuiltReviewGraph` now carries the envelope's
        // build-time contents as a plain value (`initial_env`), not an
        // `Arc<Mutex<_>>` — the Arc is minted inside `run_review_graph`,
        // not here (see `BuiltReviewGraph::initial_env`'s doc).
        let warnings = graph.initial_env.warnings.clone();
        assert!(
            warnings.iter().any(|w| w.contains("pruned")),
            "pruning an unclaimed declared probe task must warn: {warnings:?}"
        );
    }

    /// (#1513 review C1) A task named in `review-dedup-task.depends_on`
    /// with no `role_id` is the "Studio hand-edit forgot a role_id"
    /// failure mode: before this fix, `resolve_review_roles` silently
    /// skipped it (never classified as a probe role) and the claim/prune
    /// step below silently dropped its dispatch. It must now bail loudly
    /// instead — a reduced-coverage run must never look like a clean pass.
    #[test]
    fn build_review_graph_bails_on_a_declared_probe_task_with_no_role_id() {
        let doc = serde_json::json!({
            "id": "review",
            "name": "PR Review",
            "phases": [
                {"id": "investigate", "tasks": [
                    {"id": "review-bundle-task", "depends_on": [], "steps": [{"id": "review-bundle-step", "kind": "review.bundle"}]},
                    {"id": "review-probe-only-task", "role_id": "review-probe-only", "depends_on": ["review-bundle-task"],
                     "steps": [{"id": "review-probe-only-step", "kind": "dispatch.map"}]},
                    // No role_id — the misconfiguration under test.
                    {"id": "review-probe-orphan-task", "depends_on": ["review-bundle-task"],
                     "steps": [{"id": "review-probe-orphan-step", "kind": "dispatch.map"}]},
                    {"id": "review-dedup-task", "depends_on": ["review-probe-only-task", "review-probe-orphan-task"],
                     "steps": [{"id": "review-dedup-step", "kind": "review.dedup"}]}
                ]},
                {"id": "adjudicate", "tasks": [
                    {"id": "review-judge-task", "role_id": "review-judge", "depends_on": ["review-dedup-task"],
                     "steps": [{"id": "review-judge-step", "kind": "review.judge", "config": {"concurrency": 1}}]}
                ]},
                {"id": "report", "tasks": [
                    {"id": "review-synthesis-task", "depends_on": ["review-dedup-task", "review-judge-task"],
                     "steps": [{"id": "review-synthesis-step", "kind": "review.synthesis"}]}
                ]}
            ]
        });
        let config: darkmux_crew::mission_config::MissionConfig =
            serde_json::from_value(doc).expect("hand-built config parses");

        let crew = crew_with(vec![
            ("review-probe", {
                let mut s = graph_staffing("fast", "probe-model", 1);
                s.role_id = Some("review-probe-only".to_string());
                vec![s]
            }),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
        ]);
        let ctx = step_ctx(&crew, vec![]);
        // (`BuiltReviewGraph` carries no `Debug` impl, so `unwrap_err` —
        // which formats the `Ok` side on panic — doesn't compile here;
        // match instead.)
        let result = build_review_graph_from_config(
            &config,
            "hand-built test config",
            ctx,
            &dummy_bundle_spec(),
            crew.judge.clone(),
            crew.verify.clone(),
            &crew.probes,
            "investigate",
            "adjudicate",
            "report",
            1,
        );
        let err = match result {
            Ok(_) => panic!("expected a bail on the roleless declared probe task"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("review-probe-orphan-task"), "names the roleless declared task: {err}");
        assert!(err.contains("role_id"), "{err}");
    }

    /// (#1475 packet 2, THE FLIP) End-to-end: a role→profile crew (built via the
    /// packet-2 resolver over a temp `role_profiles` map) flows through
    /// `build_review_graph` and each probe task lands (a) role-bound
    /// (`Task.role_id` = its probe role) and (b) with its dispatch config's
    /// `model` = the model that role's PROFILE resolves to — the whole
    /// task→role→profile→model rollup, not a roster-scored pick. Also pins the
    /// envelope snapshot recording the role→profile resolution per seat.
    #[test]
    fn role_profile_crew_binds_each_probe_task_to_its_role_resolved_model() {
        use darkmux_crew::resourcing::{resolve_review_roles, ReviewRoleStaffing};
        use darkmux_types::{Profile, ProfileModel, ProfileRegistry};

        let mk = |id: &str, n: u32| ProfileModel { id: id.into(), n_ctx: Some(n), ..Default::default() };
        let mut profiles = BTreeMap::new();
        profiles.insert("phigh".to_string(), Profile { models: vec![mk("m-high", 40000)], ..Default::default() });
        profiles.insert("pmid".to_string(), Profile { models: vec![mk("m-mid", 40000)], ..Default::default() });
        profiles.insert("plow".to_string(), Profile { models: vec![mk("m-low", 32000)], ..Default::default() });
        let reg = ProfileRegistry {
            profiles,
            default_profile: Some("plow".to_string()),
            ..Default::default()
        };
        let bindings: BTreeMap<String, String> = [
            ("review-probe-high", "phigh"),
            ("review-probe-mid", "pmid"),
            ("review-probe-low", "plow"),
        ]
        .iter()
        .map(|(r, p)| (r.to_string(), p.to_string()))
        .collect();
        // (#1512, #1513 review) The REAL embedded "review" document — role
        // discovery is `resolve_review_roles`'s own job now (structural, by
        // step kind), never a caller-supplied role-id list.
        let loaded = darkmux_crew::mission_config::load("review").expect("embedded review config loads");
        let roles = resolve_review_roles(&reg, &loaded.config, &ReviewRoleStaffing::default(), &|r| {
            match bindings.get(r) {
                Some(p) => darkmux_profiles::profiles::RoleBinding::Mapped(p.clone()),
                None => darkmux_profiles::profiles::RoleBinding::Unmapped,
            }
        })
        .unwrap();

        let probes = roles.probes.clone();
        // The envelope snapshot records role → profile → model per seat.
        let snap = staffing_snapshot(&probes, &roles.judge, roles.verify.as_ref(), roles.request_changes);
        assert_eq!(snap.probes[0].role_id.as_deref(), Some("review-probe-high"));
        assert_eq!(snap.probes[0].name, "phigh", "the high probe's profile is recorded");
        assert_eq!(snap.probes[0].provenance.as_ref().unwrap().kind, "role-profile");
        assert_eq!(snap.probes[2].role_id.as_deref(), Some("review-probe-low"));

        let ctx = step_ctx(&roles, vec![]);
        let graph = build_review_graph(
            ctx,
            &dummy_bundle_spec(),
            roles.judge.clone(),
            roles.verify.clone(),
            &probes,
            "investigate",
            "adjudicate",
            "report",
            1,
        )
        .expect("role-driven review graph builds");

        // Exactly three probe tasks, each bound to its distinct probe role.
        // (Found by ROLE, not by sorting task ids — "review-probe-low-task"
        // sorts before "review-probe-mid-task" alphabetically, so an
        // id-sort doesn't recover high/mid/low semantic order.)
        let probe_tasks: Vec<_> =
            graph.tasks.iter().filter(|t| t.id.starts_with("review-probe-") && t.id.ends_with("-task")).collect();
        assert_eq!(probe_tasks.len(), 3, "one role-bound probe task per probe role");
        for role in ["review-probe-high", "review-probe-mid", "review-probe-low"] {
            assert!(
                probe_tasks.iter().any(|t| t.role_id.as_deref() == Some(role)),
                "expected a probe task bound to role `{role}` among {:?}",
                probe_tasks.iter().map(|t| t.role_id.as_deref()).collect::<Vec<_>>()
            );
        }

        // (#1512, the law) One role per task, no hard-coded seats: each
        // probe task depends ONLY on the bundle task (no cross-probe
        // dependency), so parallelism is EMERGENT from `depends_on` — one
        // ready-batch, concurrent by construction, never a "parallel" flag.
        for t in &probe_tasks {
            assert_eq!(
                t.depends_on,
                vec!["review-bundle-task".to_string()],
                "probe task `{}` depends only on the bundle task — no cross-probe edge",
                t.id
            );
        }
        // dedup fans IN from all three — real three-way convergence, not a
        // template's single upstream.
        let dedup_task = graph.tasks.iter().find(|t| t.id == "review-dedup-task").unwrap();
        let mut dedup_deps = dedup_task.depends_on.clone();
        dedup_deps.sort();
        assert_eq!(
            dedup_deps,
            vec![
                "review-probe-high-task".to_string(),
                "review-probe-low-task".to_string(),
                "review-probe-mid-task".to_string(),
            ],
            "dedup depends on all three probe tasks"
        );

        // Each probe step's stamped dispatch model is the model its ROLE's
        // profile resolved to (namespaced local identifier) — the flip's
        // core. (#1512) Ids are the document's own literal ids now, not a
        // `review-probe-{index}` pattern.
        let model_of = |step_id: &str| graph.steps[step_id].config["model"].as_str().unwrap().to_string();
        assert!(model_of("review-probe-high-step").contains("m-high"), "{}", model_of("review-probe-high-step"));
        assert!(model_of("review-probe-mid-step").contains("m-mid"), "{}", model_of("review-probe-mid-step"));
        assert!(model_of("review-probe-low-step").contains("m-low"), "{}", model_of("review-probe-low-step"));
    }

    // ─── (#1512) config-driven probe count — the payoff, end to end ──────────

    /// A genuinely ONE-probe "review" mission config — the Studio 32GB case
    /// #1512 names: the SAME shape as the real `review.json`, minus two of
    /// the three probe tasks, with `review-dedup-task.depends_on` naming
    /// only the survivor. No Rust code change backs this — only the
    /// document differs from production's. Parsed directly into a
    /// `MissionConfig` (never written to disk / `DARKMUX_CREW_DIR`) so the
    /// test stays hermetic — no global env mutation to race every other
    /// concurrently-running test in this file that also calls
    /// `build_review_graph`.
    fn one_probe_review_config() -> darkmux_crew::mission_config::MissionConfig {
        let doc = serde_json::json!({
            "id": "review",
            "name": "PR Review",
            "schema_version": "1.3",
            "phases": [
                {
                    "id": "investigate",
                    "tasks": [
                        {
                            "id": "review-bundle-task",
                            "depends_on": [],
                            "steps": [{"id": "review-bundle-step", "kind": "review.bundle"}]
                        },
                        {
                            "id": "review-probe-only-task",
                            "role_id": "review-probe-only",
                            "depends_on": ["review-bundle-task"],
                            "steps": [
                                {"id": "review-probe-only-render-step", "kind": "review.probe-render"},
                                {"id": "review-probe-only-step", "kind": "dispatch.map"}
                            ]
                        },
                        {
                            "id": "review-dedup-task",
                            "depends_on": ["review-probe-only-task"],
                            "steps": [{"id": "review-dedup-step", "kind": "review.dedup"}]
                        }
                    ]
                },
                {
                    "id": "adjudicate",
                    "tasks": [
                        {
                            "id": "review-judge-task",
                            "role_id": "review-judge",
                            "depends_on": ["review-dedup-task"],
                            "steps": [{"id": "review-judge-step", "kind": "review.judge", "config": {"concurrency": 1}}]
                        }
                    ]
                },
                {
                    "id": "report",
                    "tasks": [
                        {
                            "id": "review-verify-task",
                            "role_id": "review-verify",
                            "depends_on": ["review-judge-task"],
                            "steps": [
                                {"id": "review-verify-render-step", "kind": "review.verify-render"},
                                {"id": "review-verify-step", "kind": "dispatch.map"}
                            ]
                        },
                        {
                            "id": "review-synthesis-task",
                            "depends_on": ["review-dedup-task", "review-judge-task", "review-verify-task"],
                            "steps": [{"id": "review-synthesis-step", "kind": "review.synthesis"}]
                        }
                    ]
                }
            ]
        });
        serde_json::from_value(doc).expect("hand-built one-probe review config parses")
    }

    /// [`run_graph`]'s twin for a CALLER-SUPPLIED config — everything
    /// `run_graph` does, except it builds the graph via
    /// `build_review_graph_from_config` (the hermetic inner seam, #1512)
    /// instead of the public `build_review_graph` (which always loads the
    /// real "review" document off disk/embedded).
    fn run_graph_against(
        config: &darkmux_crew::mission_config::MissionConfig,
        ctx: &Arc<ReviewStepContext>,
        emitter: &mut dyn ReviewEmitter,
    ) -> Result<ReviewEnvelope> {
        // (#1512, #1513 review) `ctx.roles` is already the validated,
        // resolved shape — no separate crew-validation step.
        let judge = ctx.roles.judge.clone();
        let verify = ctx.roles.verify.clone();
        let probes = ctx.roles.probes.clone();
        let fingerprint_val = fingerprint(&seat_identifier(&judge.pm), &ctx.judge_system);
        let staffing_snap = staffing_snapshot(&probes, &judge, verify.as_ref(), ctx.roles.request_changes);
        let crew_name = ctx.roles.distinct_profile_names();
        let graph = build_review_graph_from_config(
            config,
            "hand-built test config",
            ctx.clone(),
            &dummy_bundle_spec(),
            judge,
            verify,
            &probes,
            "investigate",
            "adjudicate",
            "report",
            1,
        )?;
        let (env, _steps) = run_review_graph(
            ctx,
            &crew_name,
            ExecMode::Sequential,
            fingerprint_val,
            staffing_snap,
            graph,
            emitter,
            &mut |_step| {},
        )?;
        Ok(env)
    }

    /// The #1512 payoff, proven end to end: a review.json declaring exactly
    /// ONE probe task builds a valid graph AND runs it to completion —
    /// bundle → 1 probe → dedup → judge → verify(unstaffed, no-op) →
    /// synthesis — with zero Rust changes. This is the Studio 32GB case the
    /// issue names: dropping from three probe tasks to one is purely the
    /// document edit `one_probe_review_config` performs.
    /// (#1538 follow-up) THE regression guard for silent wrong execution.
    ///
    /// #1538 made routing structural, so a differently-NAMED variant reaches
    /// the review launcher — but both consumers downstream still resolved the
    /// built-in by id (`mission_config::load("review")`). A `review-lean`
    /// launch therefore routed correctly and then executed the BUILT-IN
    /// three-probe graph, with its own probe tasks ignored and no warning
    /// anywhere. Before #1538 the same launch failed loudly on an unknown
    /// kind, so the change turned a loud refusal into a silent wrong answer.
    ///
    /// Routing tests can't catch this — `config_uses_review_kinds` was true
    /// the whole time. Only asserting on the BUILT GRAPH does. This is the
    /// review-side analogue of `task_overrides_reach_a_renamed_coder_phase_
    /// config`, which #1551 added for the coder-phase half.
    #[test]
    fn a_named_variant_builds_its_own_graph_not_the_builtin() {
        // A one-probe document under a DIFFERENT id, exactly what an operator
        // stores as `~/.darkmux/mission-configs/review-lean.json`.
        let mut doc = one_probe_review_config();
        doc.id = "review-lean".to_string();

        let probes = vec![{
            let mut s = graph_staffing("fast", "probe-model", 1);
            s.role_id = Some("review-probe-only".to_string());
            s
        }];
        let graph = build_review_graph_from_config(
            &doc,
            "the launched config `review-lean`",
            std::sync::Arc::new(ReviewStepContext::default()),
            &dummy_bundle_spec(),
            graph_staffing("fast", "judge-model", 1),
            None,
            &probes,
            "investigate",
            "adjudicate",
            "report",
            1,
        )
        .expect("a named variant must build its own graph");

        let probe_render_steps = graph
            .steps
            .values()
            .filter(|s| s.kind == "review.probe-render")
            .count();
        assert_eq!(
            probe_render_steps, 1,
            "the variant declares ONE probe task — building the built-in's three would mean the \
             launched document was ignored; got {probe_render_steps} render steps: {:?}",
            graph.steps.values().map(|s| (&s.id, &s.kind)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn build_review_graph_runs_a_config_driven_one_probe_review_json() {
        let config = one_probe_review_config();
        let crew = crew_with(vec![
            ("review-probe", {
                let mut s = graph_staffing("fast", "probe-model", 1);
                s.role_id = Some("review-probe-only".to_string());
                vec![s]
            }),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
        ]);
        let ctx = step_ctx_with_chat(&crew, bundles_from_diff(DIFF), |call: &ChatCall| {
            if call.model.contains("judge-model") {
                Ok(reply(CONFIRM_JSON))
            } else {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            }
        });
        let env = run_graph_against(&config, &ctx, &mut NullEmitter)
            .expect("a config-driven one-probe graph runs to completion");

        assert!(env.degenerate.is_none(), "a genuine one-probe review is a clean run, not degenerate: {:?}", env.degenerate);
        assert_eq!(env.raw_flags, 1, "exactly one probe dispatch fired — the document declares exactly one");
        assert_eq!(env.judged.len(), 1);
        assert_eq!(env.confirmed, 1);
        // (#1541) NEGATIVE guard: a HEALTHY run must emit ZERO attribution
        // warnings. The desync warning replaced a silent `continue`, so the
        // risk it introduces is the mirror of the bug it fixes — a spurious
        // warning on the release-gated path. Today the loud path is
        // unreachable on a healthy run (`dispatch.map` emits exactly one
        // result per collection item, and a short vector means the map step
        // never completed, in which case dedup never runs). This pins that as
        // a REGRESSION GUARD rather than a proof-by-reasoning — which matters
        // precisely when bundling becomes run-time work and the two sides
        // stop being the same pure function over the same input.
        assert!(
            !env.warnings.iter().any(|w| w.contains("attribution desync")),
            "a healthy run must emit no attribution-desync warnings, got: {:?}",
            env.warnings
        );
    }

    /// A five-probe config also builds and runs cleanly — the issue's other
    /// named example ("three, or five, likewise"), proven the same
    /// hermetic way.
    #[test]
    fn build_review_graph_runs_a_config_driven_five_probe_review_json() {
        let probe_ids: Vec<String> = (0..5).map(|i| format!("review-probe-{i}")).collect();
        let probe_tasks: Vec<serde_json::Value> = probe_ids
            .iter()
            .map(|id| {
                serde_json::json!({
                    "id": format!("{id}-task"),
                    "role_id": id,
                    "depends_on": ["review-bundle-task"],
                    "steps": [
                        {"id": format!("{id}-render-step"), "kind": "review.probe-render"},
                        {"id": format!("{id}-step"), "kind": "dispatch.map"}
                    ]
                })
            })
            .collect();
        let mut tasks = vec![serde_json::json!({
            "id": "review-bundle-task",
            "depends_on": [],
            "steps": [{"id": "review-bundle-step", "kind": "review.bundle"}]
        })];
        tasks.extend(probe_tasks);
        tasks.push(serde_json::json!({
            "id": "review-dedup-task",
            "depends_on": probe_ids.iter().map(|id| format!("{id}-task")).collect::<Vec<_>>(),
            "steps": [{"id": "review-dedup-step", "kind": "review.dedup"}]
        }));
        let doc = serde_json::json!({
            "id": "review",
            "name": "PR Review",
            "phases": [
                {"id": "investigate", "tasks": tasks},
                {"id": "adjudicate", "tasks": [
                    {"id": "review-judge-task", "role_id": "review-judge", "depends_on": ["review-dedup-task"],
                     "steps": [{"id": "review-judge-step", "kind": "review.judge", "config": {"concurrency": 1}}]}
                ]},
                {"id": "report", "tasks": [
                    {"id": "review-verify-task", "role_id": "review-verify", "depends_on": ["review-judge-task"],
                     "steps": [
                        {"id": "review-verify-render-step", "kind": "review.verify-render"},
                        {"id": "review-verify-step", "kind": "dispatch.map"}
                     ]},
                    {"id": "review-synthesis-task", "depends_on": ["review-dedup-task", "review-judge-task", "review-verify-task"],
                     "steps": [{"id": "review-synthesis-step", "kind": "review.synthesis"}]}
                ]}
            ]
        });
        let config: darkmux_crew::mission_config::MissionConfig =
            serde_json::from_value(doc).expect("hand-built five-probe review config parses");

        let probes: Vec<ResolvedSeatStaffing> = probe_ids
            .iter()
            .map(|id| {
                let mut s = graph_staffing("fast", "probe-model", 1);
                s.role_id = Some(id.clone());
                s
            })
            .collect();
        let crew = crew_with(vec![
            ("review-probe", probes),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
        ]);
        let ctx = step_ctx_with_chat(&crew, bundles_from_diff(DIFF), |call: &ChatCall| {
            if call.model.contains("judge-model") {
                Ok(reply(CONFIRM_JSON))
            } else {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            }
        });
        let env = run_graph_against(&config, &ctx, &mut NullEmitter)
            .expect("a config-driven five-probe graph runs to completion");

        assert!(env.degenerate.is_none(), "{:?}", env.degenerate);
        assert_eq!(env.raw_flags, 5, "five probe roles, all sharing the same bundle => 5 raw flags");
        assert_eq!(env.judged.len(), 1, "identical defect text from all five collapses to one flag");
        assert_eq!(env.confirmed, 1);
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
        let ctx = step_ctx(&crew, vec![]);
        let graph = build_review_graph(
            ctx,
            &dummy_bundle_spec(),
            crew.judge.clone(),
            crew.verify.clone(),
            &crew.probes,
            "investigate",
            "adjudicate",
            "report",
            1,
        )
        .expect("built-in review config builds cleanly");

        for step in graph.steps.values() {
            let live = graph.registry.get(&step.kind).expect("every step kind resolves");
            // (#1442 ship-2b) Generic Tier-1 kinds in the graph
            // (`dispatch.map` probe/verify steps) resolve their display
            // through the BUILTIN registry — `darkmux-serve` tries that
            // registry FIRST (see its `step_kind_display_name`), so the
            // review-specific pure lookup deliberately does not duplicate
            // them.
            if !step.kind.starts_with("review.") {
                assert!(
                    review_step_kind_display_name(&step.kind).is_none(),
                    "non-review kind `{}` must not be duplicated in the review lookup",
                    step.kind
                );
                continue;
            }
            let pure = review_step_kind_display_name(&step.kind);
            assert_eq!(
                pure,
                Some(live.display_name()),
                "review_step_kind_display_name(\"{}\") must match the live impl's display_name()",
                step.kind
            );
        }
    }

    /// (#1284 Packet 3, review round 2 MUST FIX 2, #1512) LAUNCHER-LEVEL
    /// conformance golden: the full serialized `(tasks, steps)` this
    /// launcher produces for the real three-role staffing (review.json's
    /// own declared roles, each with its own resolved model) must be
    /// byte-equal (as JSON values) to the golden — every task id,
    /// `phase_id`, description, step id, `depends_on` set, kind id,
    /// `Step.config` payload, and `Vec<Task>` ORDER (a JSON array pins
    /// order under `Value` equality).
    ///
    /// **REGENERATED DELIBERATELY for #1512** (the probe-role dissolution —
    /// the graph SHAPE is the feature). The old→new delta this golden now
    /// pins: probe tasks/steps carry the DOCUMENT's own literal ids
    /// (`review-probe-high-task`/`-mid-task`/`-low-task`) instead of a
    /// `review-probe-{index}-task` numeric pattern (the `expand` template
    /// that minted those retired with #1512), and there are always exactly
    /// three — one per role review.json declares, never `seats x k` (the
    /// `k` multiplier retired too; one role is one task is one dispatch).
    /// Composed phase ids that differ from the document's own phase ids
    /// are deliberate — they pin that review's task/step ids are FIXED (no
    /// placeholder-prefix substitution applies to them).
    /// `judge_concurrency: 3` (non-default) pins the operator override
    /// into `Step.config`.
    ///
    /// (#1432 item 3) The golden also pins each task's `display_name`
    /// (Bundle / Probe high/mid/low / Dedup / Judge / Verify / Synthesis) —
    /// the phone-facing labels the document declares directly now (#1512
    /// retired the probe expansion's `display_name_pattern` rendering, since
    /// there's no template left to render from).
    #[test]
    fn build_review_graph_matches_the_golden_exactly() {
        let probes = vec![
            {
                let mut s = staffing("phigh", "probe-model-a", 1);
                s.role_id = Some("review-probe-high".to_string());
                s
            },
            {
                let mut s = staffing("pmid", "probe-model-b", 1);
                s.role_id = Some("review-probe-mid".to_string());
                s
            },
            {
                let mut s = staffing("plow", "probe-model-c", 1);
                s.role_id = Some("review-probe-low".to_string());
                s
            },
        ];
        let crew = crew_with(vec![
            ("review-probe", probes.clone()),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ]);
        let judge = staffing("fast", "judge-model", 1);
        let ctx = step_ctx(&crew, vec![]);

        let graph = build_review_graph(
            ctx,
            &dummy_bundle_spec(),
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
            "interpreted review graph diverged from the golden:\n{}",
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
        let judge = crew.judge.clone();
        let verify = crew.verify.clone();
        let probes = crew.probes.clone();
        let ctx = step_ctx(&crew, vec![]);

        let graph = build_review_graph(ctx.clone(), &dummy_bundle_spec(), judge.clone(), verify.clone(), &probes, "investigate", "adjudicate", "report", 1)
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
        // (#1399/#1877) Every step-lifecycle action this path emits is drawn
        // from the SAME canonical vocabulary constants the crew scheduler's
        // own conformance test asserts against. The two execution paths
        // (generic scheduler, review's Tier-3 driver) cannot silently grow
        // a competing vocabulary. `STEP_TIMING_ACTION` ("step timing") is
        // the scheduler's own companion record: `apply_step_terminal`
        // streams one per `StepRecord` for EVERY graph-driven mission,
        // review's own graph path included, so this path now emits it
        // alongside its hand-built `"step result"` records for the SAME
        // steps without conflict (see `run_record.rs`'s module doc in
        // `darkmux-crew` for why the two stay distinct actions).
        for record in emitter.records.iter().filter(|r| r.action.starts_with("step ")) {
            assert!(
                STEP_LIFECYCLE_ACTIONS.contains(&record.action.as_str())
                    || record.action == "step result"
                    || record.action == STEP_TIMING_ACTION,
                "review path emitted a step-scoped action outside the canonical lifecycle \
                 vocabulary or the documented `step result`/`step timing` companions: {}",
                record.action
            );
        }
        // (#1349) `run_review_graph` itself must emit NO task-level bookend
        // at all — that liveness edge belongs entirely to the caller's
        // `with_dispatch_bookends` wrap (`src/mission_launch_review.rs`),
        // which brackets the WHOLE call in the canonical `dispatch start`/
        // `dispatch complete` record. Any `dispatch *` record emitted from
        // inside this function would be the exact redundant,
        // competing-vocabulary bug #1349 retired. (#1434 retired the bespoke
        // per-run task/step/ruling vocabulary the old driver emitted, so the
        // only remaining shapes here are the scheduler's generic `step *`
        // lifecycle records + this module's own `step result` companions.)
        assert!(
            emitter.records.iter().all(|r| !r.action.starts_with("dispatch ")),
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
        let judge = crew.judge.clone();
        let verify = crew.verify.clone();
        let probes = crew.probes.clone();
        let ctx = step_ctx(&crew, vec![]);

        let graph = build_review_graph(ctx.clone(), &dummy_bundle_spec(), judge.clone(), verify.clone(), &probes, "investigate", "adjudicate", "report", 1)
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
    //   old driver's bespoke per-run task/step/ruling `review.*` emission
    //   vocabulary (the retired run-guard). #1349 retired that
    //   vocabulary from the graph path entirely: `run_review_graph` emits
    //   ONLY the scheduler's generic `step start`/`step complete`/`step
    //   error` records (already covered by `run_review_graph_with_empty_
    //   bundles_completes_with_zero_dispatches`, which now also pins the
    //   zero-bundle degenerate reason text) plus this module's own
    //   `emit_review_step_result` ("step result") records — the former
    //   task-level bookend now lives entirely in `src/mission_launch_review.rs`'s
    //   `with_dispatch_bookends` wrap (see `run_review_graph`'s own doc).
    //   (#1434 extended the same retirement to the sequential
    //   `run_judge_only` path.)
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
    //   asserted the old task-level bookend's `remote_tokens` field,
    //   which now lives in `src/mission_launch_review.rs`'s `with_dispatch_bookends`
    //   payload (outside this module's crate boundary — see
    //   `run_review_graph`'s doc); an equivalent belongs in that binary
    //   crate's own test suite, not here.
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

    // ─── #1426 ship-2 operator decision: verify pre-wave short-circuit ───

    /// Recording `ModelHost` for the scheduler's `host_factory` seam — every
    /// `load` lands in a shared list so a test can assert exactly which
    /// models the residency wave loaded (or that it loaded none).
    struct RecordingHost {
        loads: Arc<StdMutex<Vec<String>>>,
    }

    impl darkmux_gestalt::ModelHost for RecordingHost {
        fn list_resident(
            &mut self,
        ) -> std::result::Result<Vec<darkmux_gestalt::ResidentFact>, darkmux_gestalt::HostError>
        {
            Ok(Vec::new())
        }
        fn list_catalog(
            &mut self,
        ) -> std::result::Result<Vec<darkmux_gestalt::CatalogFact>, darkmux_gestalt::HostError>
        {
            Ok(Vec::new())
        }
        fn load(
            &mut self,
            _model_key: &str,
            identifier: &str,
            _min_ctx: u32,
            _deadline: darkmux_gestalt::Deadline,
        ) -> std::result::Result<darkmux_gestalt::LoadReport, darkmux_gestalt::HostError> {
            self.loads.lock().unwrap().push(identifier.to_string());
            Ok(darkmux_gestalt::LoadReport::default())
        }
        fn unload(
            &mut self,
            _target: &darkmux_gestalt::OwnedTarget,
            _deadline: darkmux_gestalt::Deadline,
        ) -> std::result::Result<(), darkmux_gestalt::HostError> {
            Ok(())
        }
    }

    /// Build + run the review graph through the scheduler DIRECTLY, with a
    /// recording `host_factory` — the mock seam `run_review_graph` itself
    /// does not expose (it hardcodes the real `lms_host_factory`). Returns
    /// the recorded load identifiers plus the final step map, so tests can
    /// assert both the wave loader's behavior and each step's outcome.
    fn run_graph_recording_loads(
        ctx: &Arc<ReviewStepContext>,
        verify: ResolvedSeatStaffing,
    ) -> (Vec<String>, BTreeMap<String, Step>) {
        let judge = ctx.roles.judge.clone();
        let probes = ctx.roles.probes.clone();
        let graph = build_review_graph(
            ctx.clone(),
            &dummy_bundle_spec(),
            judge,
            Some(verify.clone()),
            &probes,
            "investigate",
            "adjudicate",
            "report",
            1,
        )
        .expect("graph builds");
        let BuiltReviewGraph { tasks, mut steps, registry, .. } = graph;
        let tasks_by_id: BTreeMap<String, Task> =
            tasks.into_iter().map(|t| (t.id.clone(), t)).collect();
        let loads: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let loads_for_factory = loads.clone();
        let host_factory = move || -> Box<dyn darkmux_gestalt::ModelHost> {
            Box::new(RecordingHost { loads: loads_for_factory.clone() })
        };
        // The verify model's estimate is small and fixed so the planner's
        // Load decision is deterministic on any test machine.
        let est = darkmux_gestalt::FixedEstimator(
            [(verify.pm.id.clone(), 1_000_000_000u64)].into_iter().collect(),
        );
        // (#1530 Packet 3a) This test drives `run_step_graph` directly
        // (bypassing `run_review_graph`) to inspect `RecordingHost`'s loads —
        // it used to rely on `ReviewDedupStepKind::provides()`'s context-free
        // ENVELOPE default (fine, since this test never asserts on envelope
        // content), but now that the review CONTEXT also lives on the bus
        // (`REVIEW_CONTEXT_ARTIFACT`), the judge/verify-render kinds' own
        // logic (bundle lookups, the judge's `residency()` skip-load check)
        // needs the REAL context, not `ReviewStepContext::default()`'s empty
        // bundles/diff — an empty default silently changes what this test
        // measures (residency wrongly reports no local need, dedup runs
        // against an empty diff). Seed it explicitly with the real `ctx`.
        let seed_artifacts: [(&'static str, Arc<dyn Any + Send + Sync>); 1] =
            [(REVIEW_CONTEXT_ARTIFACT, ctx.clone() as Arc<dyn Any + Send + Sync>)];
        darkmux_crew::scheduler::run_step_graph(
            &mut steps,
            &tasks_by_id,
            &registry,
            &darkmux_gestalt::Facts::default(),
            &est,
            8,
            &host_factory,
            &mut |_record| {},
            &mut |_step| {},
            None,
            // (#1442 ship-2b) The same ctx-mock adapter `run_review_graph`
            // threads — the verify dispatch.map step dispatches through the
            // test's `chat_override` on the worker thread.
            review_dispatch_override(ctx),
            &seed_artifacts,
        )
        .expect("graph run completes");
        let recorded = loads.lock().unwrap().clone();
        (recorded, steps)
    }

    /// (#1486) A `ModelHost` whose every `load` fails — the deterministic
    /// stand-in for the live repro (a 120B probe model that never fit the RAM
    /// budget). Lets a test drive the wholesale probe-stage model-load failure
    /// through the REAL scheduler + wave loader without a live LMStudio.
    struct FailingLoadHost {
        detail: String,
    }

    impl darkmux_gestalt::ModelHost for FailingLoadHost {
        fn list_resident(
            &mut self,
        ) -> std::result::Result<Vec<darkmux_gestalt::ResidentFact>, darkmux_gestalt::HostError>
        {
            Ok(Vec::new())
        }
        fn list_catalog(
            &mut self,
        ) -> std::result::Result<Vec<darkmux_gestalt::CatalogFact>, darkmux_gestalt::HostError>
        {
            Ok(Vec::new())
        }
        fn load(
            &mut self,
            _model_key: &str,
            _identifier: &str,
            _min_ctx: u32,
            _deadline: darkmux_gestalt::Deadline,
        ) -> std::result::Result<darkmux_gestalt::LoadReport, darkmux_gestalt::HostError> {
            Err(darkmux_gestalt::HostError::InsufficientResources { detail: self.detail.clone() })
        }
        fn unload(
            &mut self,
            _target: &darkmux_gestalt::OwnedTarget,
            _deadline: darkmux_gestalt::Deadline,
        ) -> std::result::Result<(), darkmux_gestalt::HostError> {
            Ok(())
        }
    }

    /// (#1486) The reason builder surfaces each errored step's OWN failure
    /// message (stored in its `output` by the scheduler's terminal
    /// transition), never just the bare step ids — the whole point of the
    /// fix. A run whose reason names only step ids swallowed the "could not
    /// load … for this wave" cause the operator needs.
    #[test]
    fn errored_steps_degenerate_reason_surfaces_each_steps_message() {
        let mut steps: BTreeMap<String, Step> = BTreeMap::new();
        let mk = |id: &str, output: Option<&str>| Step {
            id: id.to_string(),
            task_id: format!("{id}-task"),
            gate: None,
            kind: "dispatch.map".to_string(),
            status: NodeStatus::Error,
            config: serde_json::json!({}),
            started_ts: None,
            completed_ts: None,
            output: output.map(str::to_string),
        };
        steps.insert(
            "probe-a".to_string(),
            mk("probe-a", Some("darkmux: could not load \"gpt-oss-120b\" for this wave: host refused")),
        );
        // A step that reached Error with no recorded output must still read
        // loud, never blank.
        steps.insert("probe-b".to_string(), mk("probe-b", None));

        let reason = errored_steps_degenerate_reason(
            &["probe-a".to_string(), "probe-b".to_string()],
            &steps,
        );

        assert!(reason.contains("2 step(s) errored"), "names how many errored: {reason}");
        assert!(
            reason.contains("could not load \"gpt-oss-120b\""),
            "surfaces the real per-step failure message, not just step ids: {reason}"
        );
        assert!(reason.contains("probe-a:"), "attributes the reason to its step: {reason}");
        assert!(
            reason.contains("probe-b: (no failure reason recorded)"),
            "an outputless errored step still reads loud: {reason}"
        );
    }

    /// (#1486) The end-to-end model-load path: a review whose every LOCAL
    /// probe seat's model fails to load runs through the real scheduler + wave
    /// loader, marks every probe step `Error` with the load-failure reason in
    /// its `output`, and the degenerate reason the review graph finalizes with
    /// surfaces that reason — a LOUD, non-Clean outcome, never a silent
    /// `flags=0 members=0` pass. This is the exact #1486 shape (probe stage
    /// yields 0 members because nothing ever loaded), reproduced without a
    /// live LMStudio via `FailingLoadHost`.
    #[test]
    fn probe_stage_wholesale_model_load_failure_is_loud_with_reasons() {
        // LOCAL probe + judge (both have n_ctx → wave-loaded track), so the
        // probe map steps hit the wave loader where the load fails.
        let crew = valid_crew();
        let ctx = step_ctx_with_chat(&crew, bundles_from_diff(DIFF), |_call: &ChatCall| {
            panic!("no probe dispatch may fire when the model never loads");
        });
        let judge = ctx.roles.judge.clone();
        let probes = ctx.roles.probes.clone();
        let graph = build_review_graph(
            ctx.clone(),
            &dummy_bundle_spec(),
            judge,
            None,
            &probes,
            "investigate",
            "adjudicate",
            "report",
            1,
        )
        .expect("graph builds");
        let BuiltReviewGraph { tasks, mut steps, registry, .. } = graph;
        let tasks_by_id: BTreeMap<String, Task> =
            tasks.into_iter().map(|t| (t.id.clone(), t)).collect();
        let host_factory = || -> Box<dyn darkmux_gestalt::ModelHost> {
            Box::new(FailingLoadHost { detail: "gpt-oss-120b won't fit the RAM budget".to_string() })
        };
        // A small fixed estimate so the planner decides Load (which then
        // fails at the host), never Block — we want to exercise the
        // load-failure reason specifically.
        let est = darkmux_gestalt::FixedEstimator(
            [("probe-model".to_string(), 1_000u64), ("judge-model".to_string(), 1_000u64)]
                .into_iter()
                .collect(),
        );
        // (#1530 follow-on, Packet A1) Bypasses `run_review_graph`, same as
        // the sibling test above — but UNLIKE that test's original reasoning
        // ("every probe step errors before dedup, the first bus consumer,
        // ever runs"), the probe stage's OWN `review.probe-render` step is
        // now a bus consumer too, and it runs BEFORE the dispatch.map step
        // this test means to exercise. An unseeded bus hands it the
        // context-free `ReviewStepContext::default()` (empty bundles), which
        // renders an EMPTY prompt collection — `dispatch.map` then
        // short-circuits that empty collection as a completed no-op
        // (`residency() == None`, zero model loads, per its own documented
        // contract) instead of ever reaching the wave loader this test means
        // to exercise. Seed the REAL `ctx` (real bundles from `DIFF`) so the
        // render step produces a real, non-empty collection and the
        // dispatch.map step actually attempts (and fails) the load.
        let seed_artifacts: [(&'static str, Arc<dyn Any + Send + Sync>); 1] =
            [(REVIEW_CONTEXT_ARTIFACT, ctx.clone() as Arc<dyn Any + Send + Sync>)];
        let report = darkmux_crew::scheduler::run_step_graph(
            &mut steps,
            &tasks_by_id,
            &registry,
            &darkmux_gestalt::Facts::default(),
            &est,
            8,
            &host_factory,
            &mut |_record| {},
            &mut |_step| {},
            None,
            review_dispatch_override(&ctx),
            &seed_artifacts,
        )
        .expect("graph run completes even when every probe step errors");

        // Every probe step errored, and its OWN output carries the reason —
        // the scheduler propagated `run_bounded`'s synthesized per-job Err.
        assert!(!report.errored.is_empty(), "the failed probe stage must report errored steps");
        for id in &report.errored {
            let out = steps[id].output.as_deref().unwrap_or("");
            assert!(
                out.contains("could not load") && out.contains("won't fit the RAM budget"),
                "errored step `{id}` must carry the load-failure reason, not an empty err: {out:?}"
            );
        }

        // The degenerate reason the review graph finalizes with (else branch
        // of `run_review_graph`) surfaces that reason — loud, not a bare id
        // list, and never a silent Clean.
        let reason = errored_steps_degenerate_reason(&report.errored, &steps);
        assert!(
            reason.contains("could not load") && reason.contains("won't fit the RAM budget"),
            "the finalized degenerate reason names WHY the probe stage produced zero signal: {reason}"
        );
    }

    /// (#1426 ship-2 operator decision) ZERO confirmed findings + a pinned,
    /// DISTINCT local verify model: the verify step completes as a no-op and
    /// the residency wave loads NO model at all — `residency()` returns
    /// `None` from the confirmed-count check BEFORE the wave loader runs.
    #[test]
    fn verify_short_circuits_before_model_load_when_zero_confirmed_findings() {
        let crew = crew_with(vec![
            ("review-probe", vec![graph_staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
        ]);
        // Pinned distinct verify model, LOCAL with a declared n_ctx — the
        // exact shape whose placement the wave loader WOULD load absent the
        // short-circuit (probe/judge stay n_ctx-less → Remote track).
        let verify = staffing("fast", "verify-model", 1);
        let needs_check_json = "```json\n{\"ruling\": \"needs_check\", \"decisive_evidence\": \"cannot tell\", \"note_for_author\": \"check manually\"}\n```";
        let ctx = step_ctx_with_chat(&crew, bundles_from_diff(DIFF), move |call: &ChatCall| {
            assert_ne!(
                call.system, "verify persona",
                "verify must never dispatch on a zero-confirmed run"
            );
            if call.system == "probe prior" {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            } else {
                // Judge pass-1 rules needs_check → confirmed docket is 0.
                Ok(reply(needs_check_json))
            }
        });

        let (loads, steps) = run_graph_recording_loads(&ctx, verify);

        assert!(loads.is_empty(), "no model load may be issued: {loads:?}");
        // (#1442 ship-2b) The verify step is a generic `dispatch.map` now —
        // found by the document's FIXED step id. The render step upstream
        // emitted an EMPTY collection (zero confirmed), so the map
        // completed as a no-op ("[]") with `residency() == None` — the
        // empty-input residency short-circuit, now a property of the BLOCK.
        let verify_step = &steps["review-verify-step"];
        assert_eq!(verify_step.kind, "dispatch.map");
        assert_eq!(verify_step.status, NodeStatus::Complete, "completed no-op");
        assert_eq!(verify_step.output.as_deref(), Some("[]"), "zero items mapped");
        let render_step = &steps["review-verify-render-step"];
        assert_eq!(render_step.output.as_deref(), Some("[]"), "render emitted an empty collection");
        // Judged flags pass through untouched: still NeedsCheck, no verify
        // record — read from the synthesis step's final envelope (the
        // judged docket flows judge -> synthesis directly now).
        let synth = &steps["review-synthesis-step"];
        let env: ReviewEnvelope =
            serde_json::from_str(synth.output.as_deref().unwrap()).expect("envelope parses");
        assert_eq!(env.judged.len(), 1);
        assert_eq!(env.judged[0].tier, Tier::NeedsCheck);
        assert!(env.judged[0].verify.is_none());
    }

    /// The normal path is unaffected: a NONZERO confirmed docket loads the
    /// pinned verify model through the residency wave and dispatches the
    /// verify pass exactly as before.
    #[test]
    fn verify_dispatches_normally_with_confirmed_findings() {
        let crew = crew_with(vec![
            ("review-probe", vec![graph_staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
        ]);
        let verify = staffing("fast", "verify-model", 1);
        let verified_json = "```json\n{\"ruling\": \"verified\", \"decisive_evidence\": \"confirmed on the code\", \"note_for_author\": \"real\"}\n```";
        let ctx = step_ctx_with_chat(&crew, bundles_from_diff(DIFF), move |call: &ChatCall| {
            if call.system == "probe prior" {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            } else if call.system == "verify persona" {
                Ok(reply(verified_json))
            } else {
                Ok(reply(CONFIRM_JSON))
            }
        });

        let (loads, steps) = run_graph_recording_loads(&ctx, verify);

        assert_eq!(
            loads,
            vec!["darkmux:verify-model".to_string()],
            "the residency wave loads exactly the pinned verify model"
        );
        let verify_step = &steps["review-verify-step"];
        assert_eq!(verify_step.kind, "dispatch.map");
        assert_eq!(verify_step.status, NodeStatus::Complete);
        // The map step's own output is the generic per-item result array…
        let results: Vec<darkmux_crew::step_kinds::MapItemResult> =
            serde_json::from_str(verify_step.output.as_deref().unwrap()).expect("map output parses");
        assert_eq!(results.len(), 1, "one adjudication per confirmed finding");
        assert!(results[0].ok);
        // …and the APPLIED verdict lands on the synthesis envelope's docket.
        let synth = &steps["review-synthesis-step"];
        let env: ReviewEnvelope =
            serde_json::from_str(synth.output.as_deref().unwrap()).expect("envelope parses");
        assert_eq!(env.judged.len(), 1);
        assert_eq!(env.judged[0].tier, Tier::Confirmed);
        let vrec = env.judged[0].verify.as_ref().expect("verify record present — the pass dispatched");
        assert_eq!(vrec.ruling, VerifyRuling::Verified);
    }

    /// (#1374 gate coverage) The judge's per-flag global index is
    /// deterministic UNDER PERMUTED COMPLETION ORDER: with `concurrency: 2`
    /// and a chat stub that makes each chunk's FIRST flag finish LAST, the
    /// serialized `judged` output still follows deduped-docket order. The
    /// retired `results.lock().len()`-derived formula collided across
    /// offsets for chunks after the first exactly under this permutation
    /// (both flags of chunk 2 computed index 2), making output
    /// completion-order; `chunk_start + offset` pins docket order.
    #[test]
    fn judge_indices_are_deterministic_under_permuted_completion_order() {
        use std::sync::{Arc as StdArc, Mutex as TestMutex};
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![{
                let mut j = staffing("fast", "judge-model", 1);
                j.passes = 1; // single pass — one call per flag, timing stays simple
                j
            }]),
        ]);
        // Four deduped flags → chunks [f0,f1], [f2,f3] at concurrency 2.
        let flags: Vec<ProbeFlag> = (0..4)
            .map(|i| ProbeFlag {
                bundle_id: format!("b{i}"),
                fact_family: "unscoped".to_string(),
                member: "darkmux:probe-model".to_string(),
                draw: 1,
                charge_text: format!("charge-{i}"),
                anchor: None,
                also_flagged: Vec::new(),
            })
            .collect();
        // Chunk-first flags (charge-0, charge-2) sleep so they COMPLETE after
        // their chunk sibling — the exact permutation that collided the old
        // formula's indices.
        let ctx = step_ctx_with_chat(&crew, vec![], move |call: &ChatCall| {
            if call.user.contains("charge-0") || call.user.contains("charge-2") {
                std::thread::sleep(std::time::Duration::from_millis(120));
            }
            Ok(reply(CONFIRM_JSON))
        });
        let kind = ReviewJudgeStepKind;
        let judge = {
            let mut j = staffing("fast", "judge-model", 1);
            j.passes = 1;
            j
        };
        // (#1530 Packets 1/3a) `members`/`env`/the run-scoped `ctx` (context)
        // moved off `ReviewJudgeStepKind`'s own fields onto the run-scoped
        // `ArtifactBus` — a direct `run_streaming` call (bypassing the full
        // scheduler) hand-seeds the same bus a real `run_review_graph` call
        // would build, via `ArtifactBus::seed` + a bare `StepRunCtx` (no
        // emitter/bucket/override needed for this test). The judge seat's
        // staffing (model/passes/max_tokens) is likewise stamped onto the
        // step's own `config`, mirroring `build_review_graph_from_config`'s
        // production stamp.
        let members: StdArc<TestMutex<Vec<MemberRecord>>> = StdArc::new(TestMutex::new(Vec::new()));
        let env: StdArc<TestMutex<ReviewEnvelope>> = StdArc::new(TestMutex::new(ReviewEnvelope::default()));
        // (#1530) `ReviewJudgeStepKind::run_streaming` now reads its bundle
        // lookup off `REVIEW_BUNDLES_ARTIFACT` — this test's flags reference
        // no real bundle ids (`b0`..`b3`), so an EMPTY bundle set is the
        // exact pre-#1530 fixture behavior (`step_ctx_with_chat`'s own
        // `vec![]` above).
        let bundles: StdArc<TestMutex<Vec<BundleInput>>> = StdArc::new(TestMutex::new(Vec::new()));
        let mut bus = ArtifactBus::new();
        bus.seed(REVIEW_CONTEXT_ARTIFACT, ctx.clone() as StdArc<dyn Any + Send + Sync>);
        bus.seed(REVIEW_ENVELOPE_ARTIFACT, env.clone() as StdArc<dyn Any + Send + Sync>);
        bus.seed(REVIEW_MEMBERS_ARTIFACT, members.clone() as StdArc<dyn Any + Send + Sync>);
        bus.seed(REVIEW_BUNDLES_ARTIFACT, bundles.clone() as StdArc<dyn Any + Send + Sync>);
        let run_ctx = StepRunCtx::new(None, None, None, StdArc::new(bus));
        let mut judge_config = serde_json::json!({
            "concurrency": 2,
            "model": seat_identifier(&judge.pm),
            "passes": judge.passes,
            "max_tokens": resolve_seat_max_tokens(&judge, DEFAULT_JUDGE_MAX_TOKENS),
        });
        judge_config["model_key"] = serde_json::json!(judge.pm.id);
        judge_config["identifier"] = serde_json::json!(seat_identifier(&judge.pm));
        if let Some(n_ctx) = judge.pm.n_ctx {
            judge_config["n_ctx"] = serde_json::json!(n_ctx);
        }
        let step = darkmux_crew::types::Step {
            id: "judge-step".to_string(),
            task_id: "judge-task".to_string(),
            gate: None,
            kind: "review.judge".to_string(),
            status: NodeStatus::default(),
            config: judge_config,
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let task = darkmux_crew::types::Task {
            id: "judge-task".to_string(),
            phase_id: "adjudicate".to_string(),
            description: "judge".to_string(),
            display_name: None,
            step_ids: vec!["judge-step".to_string()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let mut input = BTreeMap::new();
        input.insert("dedup".to_string(), serde_json::to_string(&flags).unwrap());

        use darkmux_crew::step_kinds::StepKind as _;
        let outcome = kind.run_streaming(&step, &task, &input, &run_ctx).expect("judge step completes");
        let judged: Vec<JudgedFlag> = serde_json::from_str(&outcome.output).expect("judged parses");
        let order: Vec<&str> = judged.iter().map(|j| j.flag.charge_text.as_str()).collect();
        assert_eq!(
            order,
            vec!["charge-0", "charge-1", "charge-2", "charge-3"],
            "judged output follows deduped-docket order regardless of completion order (#1374)"
        );
    }

    /// (#1748) The graph path's judge step (`ReviewJudgeStepKind::
    /// run_streaming` — the ACTUAL production dispatch path for `mission
    /// launch review`, unlike the sequential `--charges-file` re-judge
    /// path) applies the SAME mechanical absence-claim backstop, once the
    /// concurrent judge dispatches above have joined (single-threaded, so
    /// no `FileSource` Send/Sync concern) and BEFORE `judged` is
    /// serialized as this step's output.
    #[test]
    fn graph_judge_step_applies_the_absence_backstop_when_source_is_stamped() {
        use std::sync::{Arc as StdArc, Mutex as TestMutex};
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("cli.ts"), "process.exitCode = 1;\n").unwrap();

        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![{
                let mut j = staffing("fast", "judge-model", 1);
                j.passes = 1;
                j
            }]),
        ]);
        let flags = vec![flag("cli.ts", "member-a", 0, "probe charge")];
        let judge_reply = "```json\n{\"ruling\": \"confirmed\", \"decisive_evidence\": \"e\", \
             \"note_for_author\": \"does not assign `process.exitCode` on the error path\"}\n```";
        let ctx = step_ctx_with_chat(&crew, vec![], move |_call: &ChatCall| Ok(reply(judge_reply)));
        let kind = ReviewJudgeStepKind;
        let judge = {
            let mut j = staffing("fast", "judge-model", 1);
            j.passes = 1;
            j
        };
        let members: StdArc<TestMutex<Vec<MemberRecord>>> = StdArc::new(TestMutex::new(Vec::new()));
        let env: StdArc<TestMutex<ReviewEnvelope>> = StdArc::new(TestMutex::new(ReviewEnvelope::default()));
        let bundles: StdArc<TestMutex<Vec<BundleInput>>> = StdArc::new(TestMutex::new(one_bundle("cli.ts")));
        let mut bus = ArtifactBus::new();
        bus.seed(REVIEW_CONTEXT_ARTIFACT, ctx.clone() as StdArc<dyn Any + Send + Sync>);
        bus.seed(REVIEW_ENVELOPE_ARTIFACT, env.clone() as StdArc<dyn Any + Send + Sync>);
        bus.seed(REVIEW_MEMBERS_ARTIFACT, members.clone() as StdArc<dyn Any + Send + Sync>);
        bus.seed(REVIEW_BUNDLES_ARTIFACT, bundles.clone() as StdArc<dyn Any + Send + Sync>);
        let run_ctx = StepRunCtx::new(None, None, None, StdArc::new(bus));
        let mut judge_config = serde_json::json!({
            "concurrency": 1,
            "model": seat_identifier(&judge.pm),
            "passes": judge.passes,
            "max_tokens": resolve_seat_max_tokens(&judge, DEFAULT_JUDGE_MAX_TOKENS),
            // The stamp `build_review_graph_from_config` now writes onto
            // this step's config too — see `bundle_source_spec_json`.
            "source": { "kind": "worktree", "path": dir.path().display().to_string() },
        });
        judge_config["model_key"] = serde_json::json!(judge.pm.id);
        judge_config["identifier"] = serde_json::json!(seat_identifier(&judge.pm));
        if let Some(n_ctx) = judge.pm.n_ctx {
            judge_config["n_ctx"] = serde_json::json!(n_ctx);
        }
        let step = darkmux_crew::types::Step {
            id: "judge-step".to_string(),
            task_id: "judge-task".to_string(),
            gate: None,
            kind: "review.judge".to_string(),
            status: NodeStatus::default(),
            config: judge_config,
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let task = darkmux_crew::types::Task {
            id: "judge-task".to_string(),
            phase_id: "adjudicate".to_string(),
            description: "judge".to_string(),
            display_name: None,
            step_ids: vec!["judge-step".to_string()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let mut input = BTreeMap::new();
        input.insert("dedup".to_string(), serde_json::to_string(&flags).unwrap());

        use darkmux_crew::step_kinds::StepKind as _;
        let outcome = kind.run_streaming(&step, &task, &input, &run_ctx).expect("judge step completes");
        let judged: Vec<JudgedFlag> = serde_json::from_str(&outcome.output).expect("judged parses");
        assert_eq!(judged.len(), 1);
        assert_eq!(
            judged[0].tier,
            Tier::NeedsCheck,
            "the graph path's judge step must apply the mechanical backstop too, \
             not just the sequential --charges-file path"
        );
        assert!(judged[0].absence_backstop.is_some());
    }

    /// (#1748) Backward compatibility: a hand-built `Step.config` with NO
    /// `"source"` key at all (every OTHER graph-path test in this file,
    /// plus any graph persisted before this packet) must never panic or
    /// error — the backstop is a no-op, the judge step runs exactly as it
    /// did before #1748.
    #[test]
    fn graph_judge_step_is_a_no_op_backstop_without_a_source_key() {
        use std::sync::{Arc as StdArc, Mutex as TestMutex};
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![{
                let mut j = staffing("fast", "judge-model", 1);
                j.passes = 1;
                j
            }]),
        ]);
        let flags = vec![flag("cli.ts", "member-a", 0, "probe charge")];
        let judge_reply = "```json\n{\"ruling\": \"confirmed\", \"decisive_evidence\": \"e\", \
             \"note_for_author\": \"does not assign `process.exitCode` on the error path\"}\n```";
        let ctx = step_ctx_with_chat(&crew, vec![], move |_call: &ChatCall| Ok(reply(judge_reply)));
        let kind = ReviewJudgeStepKind;
        let judge = {
            let mut j = staffing("fast", "judge-model", 1);
            j.passes = 1;
            j
        };
        let members: StdArc<TestMutex<Vec<MemberRecord>>> = StdArc::new(TestMutex::new(Vec::new()));
        let env: StdArc<TestMutex<ReviewEnvelope>> = StdArc::new(TestMutex::new(ReviewEnvelope::default()));
        // No real bundle for "cli.ts" either — this is the exact pre-#1748
        // fixture shape every other graph-judge test in this file uses.
        let bundles: StdArc<TestMutex<Vec<BundleInput>>> = StdArc::new(TestMutex::new(Vec::new()));
        let mut bus = ArtifactBus::new();
        bus.seed(REVIEW_CONTEXT_ARTIFACT, ctx.clone() as StdArc<dyn Any + Send + Sync>);
        bus.seed(REVIEW_ENVELOPE_ARTIFACT, env.clone() as StdArc<dyn Any + Send + Sync>);
        bus.seed(REVIEW_MEMBERS_ARTIFACT, members.clone() as StdArc<dyn Any + Send + Sync>);
        bus.seed(REVIEW_BUNDLES_ARTIFACT, bundles.clone() as StdArc<dyn Any + Send + Sync>);
        let run_ctx = StepRunCtx::new(None, None, None, StdArc::new(bus));
        let mut judge_config = serde_json::json!({
            "concurrency": 1,
            "model": seat_identifier(&judge.pm),
            "passes": judge.passes,
            "max_tokens": resolve_seat_max_tokens(&judge, DEFAULT_JUDGE_MAX_TOKENS),
            // Deliberately NO "source" key.
        });
        judge_config["model_key"] = serde_json::json!(judge.pm.id);
        judge_config["identifier"] = serde_json::json!(seat_identifier(&judge.pm));
        if let Some(n_ctx) = judge.pm.n_ctx {
            judge_config["n_ctx"] = serde_json::json!(n_ctx);
        }
        let step = darkmux_crew::types::Step {
            id: "judge-step".to_string(),
            task_id: "judge-task".to_string(),
            gate: None,
            kind: "review.judge".to_string(),
            status: NodeStatus::default(),
            config: judge_config,
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let task = darkmux_crew::types::Task {
            id: "judge-task".to_string(),
            phase_id: "adjudicate".to_string(),
            description: "judge".to_string(),
            display_name: None,
            step_ids: vec!["judge-step".to_string()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let mut input = BTreeMap::new();
        input.insert("dedup".to_string(), serde_json::to_string(&flags).unwrap());

        use darkmux_crew::step_kinds::StepKind as _;
        let outcome = kind.run_streaming(&step, &task, &input, &run_ctx).expect("judge step completes");
        let judged: Vec<JudgedFlag> = serde_json::from_str(&outcome.output).expect("judged parses");
        assert_eq!(judged.len(), 1);
        assert_eq!(
            judged[0].tier,
            Tier::Confirmed,
            "no \"source\" key -> the backstop never fires -> the judge's own ruling stands"
        );
        assert!(judged[0].absence_backstop.is_none());
    }

    /// Migrates `envelope_counts_and_steps_are_internally_consistent`'s
    /// INTENT (tier/count internal consistency) minus its `env.steps`
    /// assertions, which have no graph-path equivalent (see the retirement
    /// note above `step_telemetry_*`).
    #[test]
    fn graph_envelope_counts_are_internally_consistent() {
        // (#1512) Two DISTINCT probe seats, not one seat drawn twice (`k`
        // no longer multiplies dispatch tasks) — the same "2 raw collapse
        // to 1" shape, now role-borne. Positional claim (neither staffing
        // carries a `role_id`) binds them to the first two of the three
        // declared probe tasks; the third is pruned, unclaimed.
        let crew = crew_with(vec![
            ("review-probe", vec![graph_staffing("fast", "probe-model", 1), graph_staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
        ]);
        let call_n = std::sync::atomic::AtomicU32::new(0);
        let ctx = step_ctx_with_chat(&crew, bundles_from_diff(DIFF), move |_call: &ChatCall| {
            let n = call_n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < 2 {
                // two distinct probe roles, both find the same defect
                Ok(reply("a real defect `const end = start.plus(30)`"))
            } else {
                Ok(reply(CONFIRM_JSON))
            }
        });
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");
        assert!(env.degenerate.is_none());
        assert_eq!(env.bundles, 1, "one changed file in the fixture diff");
        // (#1373 gate e, FIXED) `ReviewDedupStepKind` now writes the TRUE
        // pre-dedup count (2 probe roles here) into the shared envelope the
        // moment it's known, so `raw_flags` and `deduped_flags` diverge
        // again on the graph path — the "N raw collapsed to M"
        // observability signal the field name promises.
        assert_eq!(
            env.raw_flags, 2,
            "env.raw_flags reads the true pre-dedup count (2 distinct probe roles), not the deduped count"
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
    // `request_changes` in; `StaffingSnapshot` out) — the three tests
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
                    role_id: None,
                    pm: graph_pm("probe-model"),
                    k: 2,
                    passes: 2,
                    max_tokens: None,
                    selector: Some(BundleSelector {
                        fact_families: vec!["nonexistent-family".to_string()],
                        max_bundles: None,
                        ..Default::default()
                    }),
                    provenance: None,
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
        assert!(note.contains("no probe seat placed a call"), "{note}");
        // (#1530) The message must name causes that CAN be true. It used to
        // send the operator after a per-seat `selector` (hardcoded `None` by
        // the only production constructor) and a "probe expansion" (retired
        // in #1512) — two knobs that cannot exist, costing a debugging
        // session at exactly the moment a review produced nothing.
        assert!(
            !note.contains("selector") && !note.contains("expansion"),
            "the degenerate note must not name unreachable causes: {note}"
        );
        assert!(
            note.contains("bundle(s)") && note.contains("staffed probe seat(s)"),
            "it should report the counts that distinguish the real causes: {note}"
        );
        // (#1530) The seat count must be the REAL one. `env.members` is empty
        // by construction in this branch (a member is pushed only when
        // `draws > 0`, and the guard is `total_draws == 0`), so sourcing it
        // there would render "across 0 staffed seat(s)" on a run that staffed
        // several — the same misdirection this message was rewritten to stop.
        assert!(
            !note.contains("across 0 staffed"),
            "the seat count must come from the staffing snapshot, not the (empty) member list: \
             {note}"
        );
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

    /// (#1605 cause 2 — "every probe draw errored") The probe stage's
    /// `dispatch.map` step opts into a bounded ONE-retry via
    /// `retry_on_error: 1` (stamped in `build_review_graph_from_config`);
    /// the verify stage does not. Both ride through the SAME
    /// `ctx.chat_override` transport (`review_dispatch_override` adapts one
    /// override for every `dispatch.map` step in the graph), so this test
    /// proves the DIFFERENCE is real config, not an accident of the mock:
    /// the probe seat's first dispatch errors and its retry succeeds (two
    /// calls, recovered), while the verify seat's dispatch errors ONCE and
    /// is never retried (one call, isolated) — exactly the "transient cause
    /// retries, the non-transient one doesn't" contract.
    #[test]
    fn graph_probe_retries_a_transient_error_once_but_verify_never_retries() {
        let crew = crew_with(vec![
            ("review-probe", vec![graph_staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
            ("review-verify", vec![graph_staffing("fast", "verify-model", 1)]),
        ]);
        let probe_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let verify_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probe_calls_inner = probe_calls.clone();
        let verify_calls_inner = verify_calls.clone();
        let ctx = step_ctx_with_chat(&crew, vec![bundle_input("a.ts")], move |call: &ChatCall| {
            if call.system.contains("verify") {
                // (#1605) The verify seat's own dispatch failure is the
                // NON-transient case in this test — it must never retry, no
                // matter how many times the graph would call back in here.
                verify_calls_inner.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("verify endpoint unavailable");
            } else if call.model == "darkmux:judge-model" {
                Ok(SingleShotReply {
                    content: CONFIRM_JSON.to_string(),
                    total_tokens: Some(10),
                    prompt_tokens: None,
                    completion_tokens: None,
                    model: None,
                })
            } else {
                // The probe seat's dispatch: FIRST call errors (a transient
                // blip), the RETRY succeeds.
                let n = probe_calls_inner.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    anyhow::bail!("transient: connection reset");
                }
                Ok(SingleShotReply {
                    content: "a real defect".to_string(),
                    total_tokens: Some(10),
                    prompt_tokens: None,
                    completion_tokens: None,
                    model: None,
                })
            }
        });
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");

        assert_eq!(
            probe_calls.load(Ordering::SeqCst),
            2,
            "the transient cause retries exactly once: the failed attempt + one retry"
        );
        assert_eq!(
            verify_calls.load(Ordering::SeqCst),
            1,
            "the non-transient cause (verify) never retries: exactly one call"
        );
        assert_eq!(
            env.probe_retries, 1,
            "the envelope records that a probe retry happened"
        );
        assert!(
            env.degenerate.is_none(),
            "the probe recovered on retry — this must not read as a degenerate run: {:?}",
            env.degenerate
        );
    }

    /// Was `remote_probe_budget_exhaustion_is_reduced_coverage_not_a_dead_
    /// run`. The probe stage's remote bucket IS threaded through to
    /// `env.remote_budgets` on the graph path (`BuiltReviewGraph::
    /// probe_bucket`, merged post-run in `run_review_graph`) — unlike
    /// judge/verify, whose equivalent threading is the gap named above.
    ///
    /// (#1512) Rewritten from one seat drawing k=3 (the retired
    /// multi-draw-per-role fan-out) to THREE DISTINCT probe roles sharing
    /// the SAME `bucket_group: "probe"` remote allowance — the same
    /// exhaustion shape, now role-borne: whichever role's dispatch fires
    /// first exhausts the shared bucket, and the other two get SKIPPED
    /// (never billed), never a dead/degenerate run. Aggregate counts only
    /// (fired=1, skipped=2) — which specific role wins the race isn't
    /// pinned, since sibling probe tasks have no ordering guarantee
    /// relative to each other.
    #[test]
    fn graph_remote_probe_budget_exhaustion_is_reduced_coverage_not_a_dead_run() {
        let crew = crew_with(vec![
            (
                "review-probe",
                vec![
                    remote_staffing("cloud", "gpt-remote", 1),
                    remote_staffing("cloud", "gpt-remote", 1),
                    remote_staffing("cloud", "gpt-remote", 1),
                ],
            ),
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
        assert_eq!(env.raw_flags, 1, "only the pre-exhaustion role's dispatch landed");
        assert_eq!(env.confirmed, 1, "the surviving flag still went through the judge");
        let rec = env.remote_budgets.iter().find(|r| r.stage == "probe").expect("probe budget row");
        assert!(rec.exhausted);
        assert_eq!(rec.used_tokens, 600);
        assert_eq!(rec.skipped_calls, 2, "the remaining two roles were skipped, not billed");
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
    ///
    /// Re-scoped (#1876/#1877): this is now the STRICT-policy pin
    /// (`ctx.judge_exhaustion_strict = true`) — the operator opt-in that
    /// restores exactly this pre-#1876 behavior. The companion test right
    /// below is the SAME scripted scenario under the default (partial)
    /// policy, which is what actually fixes #1876's own production shape.
    #[test]
    fn graph_remote_judge_budget_exhaustion_strict_policy_is_an_honest_degraded_run() {
        let crew = crew_with(vec![
            ("review-probe", vec![graph_staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![remote_staffing("cloud", "gpt-judge", 1)]),
        ]);
        // Two bundles ⇒ two anchor-less flags in different bundles ⇒ both
        // survive dedup ⇒ the second flag's pass-1 hits the exhausted
        // bucket (one 600-token ruling exhausts a 100-token allowance).
        let bundles = vec![bundle_input("a.ts"), bundle_input("b.ts")];
        let mut ctx = step_ctx_with_chat_and_budget(&crew, bundles, 100, |call: &ChatCall| {
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
        Arc::get_mut(&mut ctx).expect("sole strong ref before the graph runs").judge_exhaustion_strict = true;
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

        // (#1373 gate b, FIXED; #1876 STRICT policy) ANY judge-bucket
        // exhaustion degrades the whole run under the strict opt-in — even
        // though this scenario has one flag that DID get a real ruling
        // before the bucket ran out (the "zero usable pass-1 rulings" gate
        // alone would NOT have caught this; the budget-exhaustion gate is
        // the one that fires).
        let reason = env.degenerate.as_deref().expect("strict policy: a partially-exhausted remote judge degrades the run");
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

    /// (#1876/#1877) The DEFAULT policy's version of the test above — same
    /// scripted scenario, `ctx.judge_exhaustion_strict` left at its default
    /// `false`. This is the graph path's own pin of the #1876 fix: a
    /// partially-exhausted remote judge bucket with a real usable ruling
    /// alongside it must NOT degrade the run. The graph path is the one
    /// `mission launch review` (the real CI/production entry point) drives —
    /// this is the exact code path the production incident hit.
    #[test]
    fn graph_remote_judge_budget_exhaustion_default_policy_is_partial_not_degraded() {
        let crew = crew_with(vec![
            ("review-probe", vec![graph_staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![remote_staffing("cloud", "gpt-judge", 1)]),
        ]);
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

        assert_eq!(env.judged.len(), 2);
        assert!(env.judged.iter().any(|j| j.tier == Tier::Confirmed), "the pre-exhaustion flag rules normally");
        let skipped = env
            .judged
            .iter()
            .find(|j| j.pass1.ruling == JudgeRuling::Error)
            .expect("the post-exhaustion flag is ruled Error, never silently confirmed");
        assert!(skipped.pass1.note_for_author.contains("remote token budget exhausted"));

        // The #1876 fix, exercised on the real production entry point: the
        // run is NOT degenerate — the skip is a coverage fact, the real
        // ruling still renders.
        assert!(
            env.degenerate.is_none(),
            "default policy: a partially-exhausted remote judge with a usable ruling must not degrade the run: {:?}",
            env.degenerate
        );
        assert!(
            env.remote_budgets.iter().any(|r| r.stage == "judge-pass1" && r.skipped_calls > 0),
            "the budget row (with its skip) still reaches the envelope: {:?}",
            env.remote_budgets
        );
        // (#1876/#1877 QA follow-up) Same coverage-warning check as the
        // sequential path's counterpart test — this is what makes the
        // mission board / CLI exit code agree with the posted PR comment
        // on the graph path too (the actual production entry point).
        assert!(
            env.warnings.iter().any(|w| w.contains("remote judge token budget exhausted")),
            "the budget skip must also land a coverage warning on the graph path: {:?}",
            env.warnings
        );
        let outcome = review_outcome(&env);
        assert!(outcome.is_partial(), "expected Partial, got {outcome:?}");
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

        // CORRECT (preserved per-flag logic, `apply_verify_results`): the
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
        // (#1888 same class, verify stage) The allowance figure is the
        // real budget passed to `step_ctx_with_chat_and_budget` (100), never
        // a stray literal.
        assert!(
            env.warnings.iter().any(|w| w.contains("allowance of 100 tokens ran out")),
            "the verify budget warning's allowance must be the real budget passed in: {:?}",
            env.warnings
        );
        assert!(
            env.remote_budgets.iter().any(|r| r.stage == "verify"),
            "the verify budget row must reach env.remote_budgets: {:?}",
            env.remote_budgets
        );
    }

    /// (#1442 gate CONSIDER) A usage-OMITTING remote verify endpoint (a reply
    /// with no `total_tokens`) still EXHAUSTS the conservatively-metered
    /// `RemoteBudget` — the map settles each reply at its granted cap when
    /// usage is absent — so a later confirmed flag's adjudication is SKIPPED.
    /// The reconstructed verify budget row SUMS the endpoint-REPORTED usage
    /// (here 0, all omitted), which would read `exhausted: false` under a bare
    /// `used >= budget`; the `skipped_calls > 0` term keeps the row honest.
    /// (A skip is itself proof the bucket exhausted.)
    #[test]
    fn graph_verify_budget_row_is_exhausted_when_a_usage_omitting_endpoint_skips() {
        let crew = crew_with(vec![
            ("review-probe", vec![graph_staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
            ("review-verify", vec![remote_staffing("frontier", "gpt-verify", 1)]),
        ]);
        let bundles = vec![bundle_input("a.ts"), bundle_input("b.ts")];
        let ctx = step_ctx_with_chat_and_budget(&crew, bundles, 100, |call: &ChatCall| {
            if call.endpoint.is_some() {
                // The verify endpoint OMITS usage — the corner. Conservative
                // metering still exhausts the 100-token bucket at the granted
                // cap, so the second confirmed flag is skipped.
                Ok(SingleShotReply {
                    content: VERIFIED_JSON.to_string(),
                    total_tokens: None,
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

        let verify_row = env
            .remote_budgets
            .iter()
            .find(|r| r.stage == "verify")
            .expect("verify budget row present");
        assert!(
            verify_row.skipped_calls > 0,
            "a confirmed flag's adjudication was skipped on the exhausted bucket"
        );
        assert_eq!(
            verify_row.used_tokens, 0,
            "the usage-omitting endpoint reports zero — the exact corner this guards"
        );
        assert!(
            verify_row.exhausted,
            "the row reports `exhausted` truthfully despite summing to {} reported tokens",
            verify_row.used_tokens
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
    ///
    /// (#1876/#1877) `judge_exhaustion_strict: true` here on purpose: this
    /// test is about the CONSIDER-g "verify never spends on an already-
    /// doomed run" behavior, which needs a genuinely degenerate judge stage
    /// to exercise — the default (partial) policy would make this same
    /// scripted scenario NOT degenerate (see the sibling `..._default_
    /// policy_is_partial_not_degraded` test above), which would make this
    /// test's own premise false, not wrong.
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
        let mut ctx = step_ctx_with_chat_and_budget(&crew, bundles, 100, move |call: &ChatCall| {
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
        Arc::get_mut(&mut ctx).expect("sole strong ref before the graph runs").judge_exhaustion_strict = true;
        let env = run_graph(&ctx, &mut NullEmitter).expect("graph run completes");
        // One flag confirmed before the judge's remote bucket exhausted —
        // the SAME preserved per-flag logic as
        // `graph_remote_judge_budget_exhaustion_strict_policy_is_an_honest_degraded_run`
        // — but the run is now degenerate (gate b, strict policy), so
        // verify's non-empty docket never dispatches at all.
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
        // (#1513 review C1) A crew claiming all three of the built-in
        // review.json's declared probe roles — NOT `graph_valid_crew()`'s
        // single positionally-claimed probe, which would now leave the
        // other two declared probe tasks pruned (loudly, with a warning
        // per C1) and defeat this test's own "empty warnings never
        // serialize" assertion below for a reason unrelated to what the
        // test actually checks (remote-field omission).
        let crew = crew_with(vec![
            (
                "review-probe",
                vec![
                    {
                        let mut s = graph_staffing("fast", "probe-model-a", 1);
                        s.role_id = Some("review-probe-high".to_string());
                        s
                    },
                    {
                        let mut s = graph_staffing("fast", "probe-model-b", 1);
                        s.role_id = Some("review-probe-mid".to_string());
                        s
                    },
                    {
                        let mut s = graph_staffing("fast", "probe-model-c", 1);
                        s.role_id = Some("review-probe-low".to_string());
                        s
                    },
                ],
            ),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
        ]);
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
        // Flag a: the verify call errors. Flag b: non-empty garbage —
        // recorded Unparsed on the first attempt. (#1442 ship-2b: the old
        // verify unparsed-RETRY — a second attempt on an unparseable
        // non-empty reply — retired with `ReviewVerifyStepKind`; the
        // generic map's `retry_on_empty` covers the empty-reply case, and
        // an unparseable reply stays honestly inconclusive: tier preserved,
        // manual-verification marker kept.)
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
            .expect("garbage adjudication recorded as Unparsed");
        assert_eq!(unparsed.tier, Tier::Confirmed);
    }

    // ── (#1442 ship-2b) probe/verify on the generic dispatch.map block ──

    /// Decision 2's mandated byte-parity check: the render step's output
    /// collection is EXACTLY the frozen `verify_prompt` assembler's output,
    /// item for item — the map step substitutes each item verbatim
    /// (`user_template: "{item}"`), so render parity IS wire parity.
    #[test]
    fn verify_render_step_output_is_byte_identical_to_verify_prompt() {
        let crew = crew_with(vec![
            ("review-probe", vec![graph_staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
            ("review-verify", vec![graph_staffing("frontier", "verify-model", 1)]),
        ]);
        let bundles = vec![bundle_input("a.ts"), bundle_input("b.ts")];
        let ctx = step_ctx(&crew, bundles.clone());

        // Two confirmed flags (one per bundle) + one needs-check flag that
        // must NOT render.
        let mk = |bundle: &str, tier: Tier, charge: &str| JudgedFlag {
            flag: ProbeFlag {
                bundle_id: bundle.to_string(),
                fact_family: "unscoped".to_string(),
                member: "darkmux:probe-model".to_string(),
                draw: 0,
                charge_text: charge.to_string(),
                anchor: None,
                also_flagged: Vec::new(),
            },
            pass1: JudgeRecord {
                ruling: JudgeRuling::Confirmed,
                decisive_evidence: String::new(),
                note_for_author: String::new(),
                pass: 1,
                seconds: 0.0,
            },
            pass2: None,
            tier,
            demoted_by_pass2: false,
            verify: None,
            demoted_by_verify: false,
            absence_backstop: None,
        };
        let judged = vec![
            mk("a.ts", Tier::Confirmed, "charge one"),
            mk("b.ts", Tier::NeedsCheck, "never rendered"),
            mk("b.ts", Tier::Confirmed, "charge two"),
        ];

        let kind = ReviewVerifyRenderStepKind;
        // (#1530 Packets 1/3a) `env`/the run-scoped `ctx` (context) moved off
        // this kind's own fields onto the run-scoped `ArtifactBus` — see the
        // judge test above for the same hand-seeded-bus pattern. The verify
        // seat's ONLY thing this kind reads (`.is_none()`) is stamped onto
        // the step's own config as `verify_seat_staffed`, mirroring
        // `build_review_graph_from_config`'s production stamp.
        let env: Arc<StdMutex<ReviewEnvelope>> = Arc::new(StdMutex::new(ReviewEnvelope::default()));
        // (#1530) This test calls `run_streaming` directly, bypassing
        // `ReviewBundleStepKind` entirely, so `REVIEW_BUNDLES_ARTIFACT` needs
        // seeding by hand — same bundles `ctx`'s own `bundle_override` holds.
        let bundles_artifact: Arc<StdMutex<Vec<BundleInput>>> = Arc::new(StdMutex::new(bundles.clone()));
        let mut bus = ArtifactBus::new();
        bus.seed(REVIEW_CONTEXT_ARTIFACT, ctx.clone() as Arc<dyn Any + Send + Sync>);
        bus.seed(REVIEW_ENVELOPE_ARTIFACT, env.clone() as Arc<dyn Any + Send + Sync>);
        bus.seed(REVIEW_BUNDLES_ARTIFACT, bundles_artifact as Arc<dyn Any + Send + Sync>);
        let run_ctx = StepRunCtx::new(None, None, None, Arc::new(bus));
        let step = darkmux_crew::types::Step {
            id: "review-verify-render-step".to_string(),
            task_id: "review-verify-task".to_string(),
            gate: None,
            kind: "review.verify-render".to_string(),
            status: NodeStatus::default(),
            config: serde_json::json!({ "verify_seat_staffed": ctx.roles.verify.is_some() }),
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let task = darkmux_crew::types::Task {
            id: "review-verify-task".to_string(),
            phase_id: "report".to_string(),
            description: "verify".to_string(),
            display_name: None,
            step_ids: vec!["review-verify-render-step".to_string()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let mut input = BTreeMap::new();
        input.insert("review-judge-task".to_string(), serde_json::to_string(&judged).unwrap());

        use darkmux_crew::step_kinds::StepKind as _;
        let out = kind.run_streaming(&step, &task, &input, &run_ctx).expect("render completes");
        let prompts: Vec<String> = serde_json::from_str(&out.output).expect("collection parses");

        // Direct calls to the SAME #1256-frozen assembler, in confirmed
        // order — byte equality, never "similar".
        let expected: Vec<String> = judged
            .iter()
            .filter(|j| j.tier == Tier::Confirmed)
            .map(|j| {
                let bundle = bundles.iter().find(|b| b.id == j.flag.bundle_id).unwrap();
                verify_prompt(&ctx.intent_title, &ctx.intent_body, &bundle.code, &bundle.facts, &j.flag.charge_text)
            })
            .collect();
        assert_eq!(prompts.len(), 2, "one prompt per CONFIRMED flag only");
        assert_eq!(prompts, expected, "render output is byte-identical to direct verify_prompt calls");
    }

    /// (#1530 follow-on, Packet A1) The faithfulness pin this packet's own
    /// PR description promises: the probe render step's RUN-TIME output —
    /// selector applied to the bundle set off `REVIEW_BUNDLES_ARTIFACT`, then
    /// `probe_user_message` rendered per selected bundle — is byte-identical
    /// (same selection, same per-item text, same ORDER) to what the
    /// RETIRED build-time stamping loop in `build_review_graph_from_config`
    /// used to freeze into `config.collection` directly (`selected.iter()
    /// .map(|b| probe_user_message(&ctx.probe_system, b)).collect()`),
    /// mirroring `verify_render_step_output_is_byte_identical_to_verify_
    /// prompt` above for the verify stage's own render/dispatch split.
    /// `probe_user_message`/`select_bundles_for_staffing` themselves are
    /// UNCHANGED (only WHERE they're called moved) — this test calls them
    /// directly, independent of the render step, as the "old build-time
    /// stamping" reference.
    #[test]
    fn probe_render_step_output_is_byte_identical_to_probe_user_message() {
        let bundles = vec![
            BundleInput {
                id: "a.ts".into(),
                fact_family: "auth".into(),
                code: "const a = 1".into(),
                probe_code: "const a = 1 // probe".into(),
                facts: vec!["fact-a".to_string()],
                manifest: vec![],
            },
            BundleInput {
                id: "b.ts".into(),
                fact_family: "billing".into(),
                code: "const b = 2".into(),
                probe_code: "const b = 2 // probe".into(),
                facts: vec![],
                manifest: vec![],
            },
            BundleInput {
                id: "c.ts".into(),
                fact_family: "auth".into(),
                code: "const c = 3".into(),
                probe_code: "const c = 3 // probe".into(),
                facts: vec![],
                manifest: vec![],
            },
        ];
        let crew = crew_with(vec![
            ("review-probe", vec![graph_staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
        ]);
        let mut ctx_inner = (*step_ctx(&crew, bundles.clone())).clone();
        ctx_inner.probe_system = "shared fallback prior".to_string();
        ctx_inner
            .probe_role_prompts
            .insert("review-probe-high".to_string(), "high seat prior".to_string());
        let ctx = Arc::new(ctx_inner);

        let selector = BundleSelector { fact_families: vec!["auth".to_string()], ..Default::default() };
        let mut bus = ArtifactBus::new();
        bus.seed(REVIEW_CONTEXT_ARTIFACT, ctx.clone() as Arc<dyn Any + Send + Sync>);
        // (#1530) This test calls `run_streaming` directly, bypassing
        // `ReviewBundleStepKind`, so `REVIEW_BUNDLES_ARTIFACT` needs seeding
        // by hand.
        bus.seed(
            REVIEW_BUNDLES_ARTIFACT,
            Arc::new(StdMutex::new(bundles.clone())) as Arc<dyn Any + Send + Sync>,
        );
        // (#1541) Mirrors the scheduler's own `provides()` pre-scan for a
        // graph containing a `review.probe-render` step — this test calls
        // `run_streaming` directly, bypassing the scheduler, so it
        // materializes the artifact by hand the same way.
        bus.materialize(REVIEW_PROBE_SELECTION_ARTIFACT, make_review_probe_selection_artifact);
        let run_ctx = StepRunCtx::new(None, None, None, Arc::new(bus));
        let step = darkmux_crew::types::Step {
            id: "review-probe-high-render-step".to_string(),
            task_id: "review-probe-high-task".to_string(),
            gate: None,
            kind: "review.probe-render".to_string(),
            status: NodeStatus::default(),
            config: serde_json::json!({
                "selector": serde_json::to_value(&selector).unwrap(),
                "role_id": "review-probe-high",
            }),
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let task = darkmux_crew::types::Task {
            id: "review-probe-high-task".to_string(),
            phase_id: "investigate".to_string(),
            description: "probe high".to_string(),
            display_name: None,
            step_ids: vec!["review-probe-high-render-step".to_string(), "review-probe-high-step".to_string()],
            depends_on: vec!["review-bundle-task".to_string()],
            reads: Vec::new(),
            role_id: Some("review-probe-high".to_string()),
            profile_name: None,
            workdir: None,
            image: None,
        };
        let input = BTreeMap::new();

        use darkmux_crew::step_kinds::StepKind as _;
        let kind = ReviewProbeRenderStepKind;
        let out = kind.run_streaming(&step, &task, &input, &run_ctx).expect("probe render completes");
        let rendered: Vec<String> = serde_json::from_str(&out.output).expect("collection parses");

        // The "old build-time stamping" reference: the SAME two calls,
        // invoked directly here rather than through the render step.
        let selected = select_bundles_for_staffing(&bundles, Some(&selector));
        let expected: Vec<String> =
            selected.iter().map(|b| probe_user_message("high seat prior", b)).collect();

        assert_eq!(selected.len(), 2, "selector restricts to the two \"auth\" bundles");
        assert_eq!(rendered.len(), 2, "one prompt per selected bundle only");
        assert_eq!(rendered, expected, "render output is byte-identical to direct probe_user_message calls");
        // Order pin: `select_bundles_for_staffing`'s own stable-sort
        // ("param-flow" first, otherwise input order) means "a.ts" precedes
        // "c.ts" here — asserting the literal text (not just set equality)
        // pins that the render step preserves it.
        assert!(rendered[0].contains("const a = 1 // probe"));
        assert!(rendered[1].contains("const c = 3 // probe"));
    }

    /// (#1530 follow-on) The per-seat prompt fix's fallback half: a seat
    /// whose `role_id` has NO entry in `ctx.probe_role_prompts` (every
    /// hand-built fixture, and any operator install where a per-seat `.md`
    /// genuinely doesn't exist) falls through to the shared `probe_system`
    /// text — the exact pre-fix behavior, so the fix is a no-op unless the
    /// launcher actually populated a per-seat entry.
    #[test]
    fn probe_render_step_falls_back_to_shared_probe_system_when_role_has_no_specific_prompt() {
        let bundles = vec![bundle_input("a.ts")];
        let crew = crew_with(vec![
            ("review-probe", vec![graph_staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
        ]);
        // `step_ctx` populates NO `probe_role_prompts` entries — the default
        // empty map every hand-built fixture uses.
        let ctx = step_ctx(&crew, bundles.clone());
        assert!(ctx.probe_role_prompts.is_empty(), "test fixture carries no per-seat overrides");

        let mut bus = ArtifactBus::new();
        bus.seed(REVIEW_CONTEXT_ARTIFACT, ctx.clone() as Arc<dyn Any + Send + Sync>);
        // (#1530) This test calls `run_streaming` directly, bypassing
        // `ReviewBundleStepKind`, so `REVIEW_BUNDLES_ARTIFACT` needs seeding
        // by hand.
        bus.seed(
            REVIEW_BUNDLES_ARTIFACT,
            Arc::new(StdMutex::new(bundles.clone())) as Arc<dyn Any + Send + Sync>,
        );
        // (#1541) Mirrors the scheduler's own `provides()` pre-scan — see
        // the sibling test above for why this test needs it too.
        bus.materialize(REVIEW_PROBE_SELECTION_ARTIFACT, make_review_probe_selection_artifact);
        let run_ctx = StepRunCtx::new(None, None, None, Arc::new(bus));
        let step = darkmux_crew::types::Step {
            id: "review-probe-high-render-step".to_string(),
            task_id: "review-probe-high-task".to_string(),
            gate: None,
            kind: "review.probe-render".to_string(),
            status: NodeStatus::default(),
            config: serde_json::json!({ "selector": null, "role_id": "review-probe-high" }),
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let task = darkmux_crew::types::Task {
            id: "review-probe-high-task".to_string(),
            phase_id: "investigate".to_string(),
            description: "probe high".to_string(),
            display_name: None,
            step_ids: vec!["review-probe-high-render-step".to_string(), "review-probe-high-step".to_string()],
            depends_on: vec!["review-bundle-task".to_string()],
            reads: Vec::new(),
            role_id: Some("review-probe-high".to_string()),
            profile_name: None,
            workdir: None,
            image: None,
        };
        let input = BTreeMap::new();

        use darkmux_crew::step_kinds::StepKind as _;
        let kind = ReviewProbeRenderStepKind;
        let out = kind.run_streaming(&step, &task, &input, &run_ctx).expect("probe render completes");
        let rendered: Vec<String> = serde_json::from_str(&out.output).expect("collection parses");

        let expected: Vec<String> =
            bundles.iter().map(|b| probe_user_message(&ctx.probe_system, b)).collect();
        assert_eq!(rendered, expected, "no per-seat entry falls back to the shared probe_system prior");
    }

    // (#1512) `graph_seats_by_k_fan_out_sums_member_accounting_across_
    // sibling_steps` RETIRED here — it proved `build_review_graph` minted
    // multiple sibling map tasks per seat via `ResolvedSeatStaffing::k`
    // (2 seats x k=2 -> 4 sibling tasks). That graph-construction shape is
    // exactly what #1512 dissolves (one role, one task, one dispatch; probe
    // recall breadth is now a review.json edit, never a per-run draw
    // multiplier) — `build_review_graph` no longer mints sibling tasks for
    // k>1, so the scenario this test built can no longer occur. The
    // per-seat SUMMING mechanism it was also exercising (multiple
    // `draw_task_ids` collapsing into one `MemberRecord`) remains fully
    // covered independent of graph construction by
    // `reconstruct_probe_stage_accounts_skips_errors_and_flags` below,
    // which hand-builds a `ProbeSeatSpec` with two `draw_task_ids` and
    // calls `reconstruct_probe_stage` directly — so no coverage is lost,
    // only the now-impossible integration shape.

    /// `reconstruct_probe_stage` pure-function coverage: budget-skips are
    /// never draws, dispatch errors are, per-seat accounting sums across
    /// sibling draw tasks, and the all-draws-failed gate names its reason.
    #[test]
    fn reconstruct_probe_stage_accounts_skips_errors_and_flags() {
        use darkmux_crew::step_kinds::{MapItemResult, MAP_BUDGET_SKIP_ERROR};
        let spec = ProbeSeatSpec {
            name: "cloud".to_string(),
            identifier: "gpt-remote".to_string(),
            remote: true,
            endpoint_host: Some("example.com".to_string()),
            draw_task_ids: vec!["t-draw0".to_string(), "t-draw1".to_string()],
        };
        let item = |index: usize, ok: bool, content: &str, error: Option<&str>, tokens: Option<u64>| MapItemResult {
            index,
            ok,
            content: content.to_string(),
            error: error.map(String::from),
            total_tokens: tokens,
            prompt_tokens: None,
            completion_tokens: None,
            served_model: ok.then(|| "gpt-served".to_string()),
            wall_ms: 5,
            retried: 0,
        };
        // draw 0: b1 flags, b2 dispatch-errors. draw 1: b1 budget-skipped,
        // b2 empty-but-dispatched.
        let draw0 = vec![item(0, true, "a defect", None, Some(100)), item(1, false, "", Some("endpoint 503"), None)];
        let draw1 = vec![
            item(0, false, "", Some(MAP_BUDGET_SKIP_ERROR), None),
            item(1, true, "  ", None, Some(50)),
        ];
        let mut input = BTreeMap::new();
        input.insert("t-draw0".to_string(), serde_json::to_string(&draw0).unwrap());
        input.insert("t-draw1".to_string(), serde_json::to_string(&draw1).unwrap());
        // (#1541) The render step's published selection — one entry per
        // draw task id, replacing the retired `ProbeSeatSpec.bundles`
        // build-time snapshot.
        let mut selection: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        selection.insert(
            "t-draw0".to_string(),
            vec![("b1".to_string(), "unscoped".to_string()), ("b2".to_string(), "unscoped".to_string())],
        );
        selection.insert(
            "t-draw1".to_string(),
            vec![("b1".to_string(), "unscoped".to_string()), ("b2".to_string(), "unscoped".to_string())],
        );

        let recon =
            reconstruct_probe_stage(std::slice::from_ref(&spec), &input, &selection, 120).expect("parses");

        assert_eq!(recon.flags.len(), 1, "only the non-empty ok item flags");
        assert_eq!(recon.flags[0].bundle_id, "b1");
        assert_eq!(recon.flags[0].draw, 0);
        assert_eq!(recon.flags[0].charge_text, "a defect", "trimmed charge text");

        assert_eq!(recon.members.len(), 1);
        let m = &recon.members[0];
        assert_eq!(m.draws, 3, "fired = 4 items - 1 budget skip");
        assert_eq!(m.total_tokens, 150);
        assert_eq!(m.wall_ms, 15, "wall summed over FIRED items only");
        assert_eq!(m.served_model.as_deref(), Some("gpt-served"));

        let row = recon.budget_row.as_ref().expect("remote seat -> probe budget row");
        assert_eq!(row.stage, "probe");
        assert_eq!(row.used_tokens, 150);
        assert!(row.exhausted, "150 >= 120");
        assert_eq!(row.skipped_calls, 1);

        assert!(
            recon.warnings.iter().any(|w| w.contains("remote probe seat \"cloud\"") && w.contains("endpoint 503")),
            "dispatch error named: {:?}",
            recon.warnings
        );
        assert!(
            recon.warnings.iter().any(|w| w.contains("remote probe token budget exhausted — 1 draw(s) skipped")),
            "exhaustion named: {:?}",
            recon.warnings
        );
        // (#1888 same class, probe stage) The allowance figure is the
        // caller's real budget (120), never a stray literal.
        assert!(
            recon.warnings.iter().any(|w| w.contains("(120 tokens)")),
            "the probe budget warning's allowance must be the real budget passed in: {:?}",
            recon.warnings
        );
        assert!(recon.all_draws_failed.is_none(), "real signal landed — not the all-failed case");
    }

    #[test]
    fn reconstruct_probe_stage_all_fired_draws_erroring_names_the_degenerate_reason() {
        use darkmux_crew::step_kinds::MapItemResult;
        let spec = ProbeSeatSpec {
            name: "fast".to_string(),
            identifier: "darkmux:probe-model".to_string(),
            remote: false,
            endpoint_host: None,
            draw_task_ids: vec!["t-draw0".to_string()],
        };
        let items = vec![MapItemResult {
            index: 0,
            ok: false,
            content: String::new(),
            error: Some("network down".to_string()),
            total_tokens: None,
            prompt_tokens: None,
            completion_tokens: None,
            served_model: None,
            wall_ms: 3,
            retried: 1,
        }];
        let mut input = BTreeMap::new();
        input.insert("t-draw0".to_string(), serde_json::to_string(&items).unwrap());
        let mut selection: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        selection.insert("t-draw0".to_string(), vec![("b1".to_string(), "unscoped".to_string())]);
        let recon =
            reconstruct_probe_stage(std::slice::from_ref(&spec), &input, &selection, 500_000).expect("parses");
        assert!(recon.flags.is_empty());
        let reason = recon.all_draws_failed.expect("every fired draw errored");
        assert!(reason.contains("errored"), "{reason}");
        assert!(reason.contains("network down"), "the first error is named: {reason}");
        assert!(recon.budget_row.is_none(), "local-only stage carries no budget row");
        // (#1605) The item's own `retried: 1` (it consumed its one
        // `retry_on_error` attempt before still failing) sums into the
        // reconstruction's `retries` — the figure `run_review_graph` folds
        // into `ReviewEnvelope::probe_retries`.
        assert_eq!(recon.retries, 1, "the item's retried attempt must be summed, not dropped");
    }

    /// (#1541) THE no-op proof: what `ReviewProbeRenderStepKind::run_streaming`
    /// PUBLISHES onto `REVIEW_PROBE_SELECTION_ARTIFACT` is byte-identical to
    /// what the RETIRED build-time `ProbeSeatSpec.bundles` snapshot used to
    /// hold for the same inputs (`select_bundles_for_staffing(&ctx.bundles,
    /// selector).map(|b| (b.id, b.fact_family))` — the exact call
    /// `build_review_graph_from_config`'s probe loop used to make before this
    /// packet). Same selection, same order. Since attribution now keys
    /// entirely on the published pairs (`reconstruct_probe_stage`'s own
    /// `selection` parameter), this equality IS the claim that today's
    /// behavior is unchanged — the bug #1541 fixes is a divergence that can
    /// only appear once bundle selection itself becomes run-time work,
    /// which this packet does not do.
    #[test]
    fn probe_render_step_publishes_the_same_selection_the_retired_build_time_snapshot_held() {
        let bundles = vec![
            BundleInput {
                id: "a.ts".into(),
                fact_family: "auth".into(),
                code: "const a = 1".into(),
                probe_code: "const a = 1 // probe".into(),
                facts: vec![],
                manifest: vec![],
            },
            BundleInput {
                id: "b.ts".into(),
                fact_family: "billing".into(),
                code: "const b = 2".into(),
                probe_code: "const b = 2 // probe".into(),
                facts: vec![],
                manifest: vec![],
            },
            BundleInput {
                id: "c.ts".into(),
                fact_family: "auth".into(),
                code: "const c = 3".into(),
                probe_code: "const c = 3 // probe".into(),
                facts: vec![],
                manifest: vec![],
            },
        ];
        let crew = crew_with(vec![
            ("review-probe", vec![graph_staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![graph_staffing("fast", "judge-model", 1)]),
        ]);
        let ctx = step_ctx(&crew, bundles.clone());

        let selector = BundleSelector { fact_families: vec!["auth".to_string()], ..Default::default() };
        let mut bus = ArtifactBus::new();
        bus.seed(REVIEW_CONTEXT_ARTIFACT, ctx.clone() as Arc<dyn Any + Send + Sync>);
        // (#1530) This test calls `run_streaming` directly, bypassing
        // `ReviewBundleStepKind`, so `REVIEW_BUNDLES_ARTIFACT` needs seeding
        // by hand.
        bus.seed(
            REVIEW_BUNDLES_ARTIFACT,
            Arc::new(StdMutex::new(bundles.clone())) as Arc<dyn Any + Send + Sync>,
        );
        // Mirrors exactly what the scheduler's `provides()` pre-scan does
        // for a graph that contains a `review.probe-render` step — this
        // test bypasses the scheduler, so it materializes the artifact by
        // hand the same way.
        bus.materialize(REVIEW_PROBE_SELECTION_ARTIFACT, make_review_probe_selection_artifact);
        let run_ctx = StepRunCtx::new(None, None, None, Arc::new(bus));
        let step = darkmux_crew::types::Step {
            id: "review-probe-high-render-step".to_string(),
            task_id: "review-probe-high-task".to_string(),
            gate: None,
            kind: "review.probe-render".to_string(),
            status: NodeStatus::default(),
            config: serde_json::json!({
                "selector": serde_json::to_value(&selector).unwrap(),
                "role_id": "review-probe-high",
            }),
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let task = darkmux_crew::types::Task {
            id: "review-probe-high-task".to_string(),
            phase_id: "investigate".to_string(),
            description: "probe high".to_string(),
            display_name: None,
            step_ids: vec!["review-probe-high-render-step".to_string(), "review-probe-high-step".to_string()],
            depends_on: vec!["review-bundle-task".to_string()],
            reads: Vec::new(),
            role_id: Some("review-probe-high".to_string()),
            profile_name: None,
            workdir: None,
            image: None,
        };
        let input = BTreeMap::new();

        use darkmux_crew::step_kinds::StepKind as _;
        let kind = ReviewProbeRenderStepKind;
        kind.run_streaming(&step, &task, &input, &run_ctx).expect("probe render completes");

        let published: BTreeMap<String, Vec<(String, String)>> = run_ctx
            .artifact::<StdMutex<BTreeMap<String, Vec<(String, String)>>>>(REVIEW_PROBE_SELECTION_ARTIFACT)
            .expect("the render step materializes this artifact via its own provides()")
            .lock()
            .expect("probe selection mutex poisoned")
            .clone();
        let published_pairs = published.get(&task.id).expect("the render step publishes under its own task id");

        // The "old build-time stamping" reference: the SAME call
        // `build_review_graph_from_config`'s probe loop used to make before
        // #1541, invoked directly here as the retired snapshot's stand-in.
        let selected = select_bundles_for_staffing(&bundles, Some(&selector));
        let expected_pairs: Vec<(String, String)> =
            selected.iter().map(|b| (b.id.clone(), b.fact_family.clone())).collect();

        assert_eq!(expected_pairs.len(), 2, "selector restricts to the two \"auth\" bundles");
        assert_eq!(
            published_pairs, &expected_pairs,
            "the render step's published selection is byte-identical (same bundles, same order) \
             to the retired build-time snapshot for the same inputs"
        );
    }

    /// (#1541) The "fail loudly instead of silently" half of the fix: a
    /// draw whose published selection LENGTH doesn't match its dispatch
    /// result count (the desync run-time bundling would introduce) is a
    /// NAMED warning, and that draw's flags are DROPPED rather than
    /// misattributed via the old bug's positional `results.get(b_idx)`
    /// (which would have silently paired the wrong bundle to a result, or
    /// silently skipped a result past the snapshot's length).
    #[test]
    fn reconstruct_probe_stage_desynced_selection_warns_loudly_and_drops_the_draws_flags() {
        use darkmux_crew::step_kinds::MapItemResult;
        let spec = ProbeSeatSpec {
            name: "fast".to_string(),
            identifier: "darkmux:probe-model".to_string(),
            remote: false,
            endpoint_host: None,
            draw_task_ids: vec!["t-draw0".to_string()],
        };
        // Two dispatch results came back...
        let items = vec![
            MapItemResult {
                index: 0,
                ok: true,
                content: "a real defect".to_string(),
                error: None,
                total_tokens: Some(10),
                prompt_tokens: None,
                completion_tokens: None,
                served_model: None,
                wall_ms: 3,
                retried: 0,
            },
            MapItemResult {
                index: 1,
                ok: true,
                content: "another defect".to_string(),
                error: None,
                total_tokens: Some(10),
                prompt_tokens: None,
                completion_tokens: None,
                served_model: None,
                wall_ms: 3,
                retried: 0,
            },
        ];
        let mut input = BTreeMap::new();
        input.insert("t-draw0".to_string(), serde_json::to_string(&items).unwrap());
        // ...but the render step only published ONE selected bundle for
        // this task — a desync, exactly the shape run-time bundling could
        // introduce without this fix.
        let mut selection: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        selection.insert("t-draw0".to_string(), vec![("b1".to_string(), "unscoped".to_string())]);

        let recon =
            reconstruct_probe_stage(std::slice::from_ref(&spec), &input, &selection, 500_000).expect("parses");

        assert!(
            recon.flags.is_empty(),
            "a desynced draw's flags are DROPPED rather than misattributed: {:?}",
            recon.flags
        );
        assert!(
            recon.warnings.iter().any(|w| {
                w.contains("probe seat \"fast\"") && w.contains("desync") && w.contains("t-draw0")
            }),
            "the desync is named loudly, not swallowed: {:?}",
            recon.warnings
        );

        // The absent-selection half of the same loud-failure path: no entry
        // at all for the draw's task id (the render step never ran, or
        // never published for it).
        let empty_selection: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        let recon_absent =
            reconstruct_probe_stage(std::slice::from_ref(&spec), &input, &empty_selection, 500_000)
                .expect("parses");
        assert!(recon_absent.flags.is_empty(), "no published selection ⇒ no attributable flags");
        assert!(
            recon_absent
                .warnings
                .iter()
                .any(|w| w.contains("probe seat \"fast\"") && w.contains("no bundle selection published")),
            "the absence is named loudly, not swallowed: {:?}",
            recon_absent.warnings
        );
    }

    // ── (#1530) bundling becomes runtime graph work ──────────────────────

    /// THE faithfulness pin this packet's whole PR is built on: over a REAL
    /// worktree + diff (the `bundle_golden` integration test's own fixture —
    /// `tests/fixtures/bundle/`), `ReviewBundleStepKind::run_streaming`'s
    /// run-time output is byte-identical to what
    /// `src/mission_launch_review.rs`'s RETIRED pre-graph prelude used to
    /// compute (`build_bundles` + `bundle_inputs_from_set`, invoked directly
    /// here as that prelude's stand-in) for the SAME inputs — same bundles,
    /// same order, same content. Bundling is a pure function of (source,
    /// diff, bundler); this packet only relocates WHERE it's called, never
    /// WHAT it computes, and this test is the direct proof of that claim.
    /// `bundle_override` is `None` — this exercises the REAL reconstruction
    /// path (`file_source_from_step_config` + `build_bundles`), not the
    /// test-seam short-circuit every other graph test in this file uses.
    #[test]
    fn review_bundle_step_run_time_output_matches_the_retired_prelude_computation() {
        let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bundle");
        let worktree = fixture_dir.join("worktree");
        let diff_text = std::fs::read_to_string(fixture_dir.join("diff.patch"))
            .expect("read the bundle_golden integration test's own diff.patch fixture");

        // The RETIRED prelude's own computation, invoked directly — the
        // pre-#1530 `src/mission_launch_review.rs::run_dispatch` did exactly
        // this, before the graph was even built.
        let prelude_source = FileSource::worktree(&worktree);
        let prelude_set = build_bundles(&prelude_source, &diff_text)
            .expect("build_bundles over the bundle_golden fixture");
        let expected = bundle_inputs_from_set(&prelude_set, &prelude_source)
            .expect("bundle_inputs_from_set over the bundle_golden fixture");
        assert!(!expected.is_empty(), "the fixture must produce real bundles, or this test proves nothing");

        // The RUN-TIME path: `ReviewBundleStepKind::run_streaming`
        // reconstructs its OWN `FileSource` from `Step.config` — stamped
        // the same way `build_review_graph_from_config` stamps it from a
        // `BundleBuildSpec` — and calls the SAME two functions.
        let crew = valid_crew();
        let ctx = Arc::new(ReviewStepContext {
            case_id: "case-1".to_string(),
            roles: crew,
            intent_title: String::new(),
            intent_body: String::new(),
            diff: diff_text,
            probe_system: String::new(),
            probe_role_prompts: BTreeMap::new(),
            judge_system: String::new(),
            verify_system: String::new(),
            remote_max_tokens_per_execution: 500_000,
            judge_exhaustion_strict: false,
            timeout_seconds: 30,
            chat_override: None,
            bundle_override: None,
            mission_id: None,
        });
        let env: Arc<StdMutex<ReviewEnvelope>> = Arc::new(StdMutex::new(ReviewEnvelope::default()));
        let mut bus = ArtifactBus::new();
        bus.seed(REVIEW_CONTEXT_ARTIFACT, ctx.clone() as Arc<dyn Any + Send + Sync>);
        bus.seed(REVIEW_ENVELOPE_ARTIFACT, env.clone() as Arc<dyn Any + Send + Sync>);
        bus.materialize(REVIEW_BUNDLES_ARTIFACT, make_review_bundles_artifact);
        let run_ctx = StepRunCtx::new(None, None, None, Arc::new(bus));

        let step = darkmux_crew::types::Step {
            id: "review-bundle-step".to_string(),
            task_id: "review-bundle-task".to_string(),
            gate: None,
            kind: "review.bundle".to_string(),
            status: NodeStatus::default(),
            config: serde_json::json!({
                "source": { "kind": "worktree", "path": worktree.display().to_string() },
                "bundler": null,
                "diff_file": "/unused-when-bundler-is-null",
            }),
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let task = darkmux_crew::types::Task {
            id: "review-bundle-task".to_string(),
            phase_id: "investigate".to_string(),
            description: "bundle".to_string(),
            display_name: None,
            step_ids: vec!["review-bundle-step".to_string()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let input = BTreeMap::new();

        use darkmux_crew::step_kinds::StepKind as _;
        let kind = ReviewBundleStepKind;
        let out = kind.run_streaming(&step, &task, &input, &run_ctx).expect("bundle step completes");

        let published: Vec<BundleInput> = run_ctx
            .artifact::<StdMutex<Vec<BundleInput>>>(REVIEW_BUNDLES_ARTIFACT)
            .expect("this kind's own provides() materializes review.bundles")
            .lock()
            .expect("review bundles mutex poisoned")
            .clone();

        // Compared as JSON (BundleInput derives no PartialEq) — an exact
        // structural equality check, same discipline the golden tests in
        // this file already use.
        assert_eq!(
            serde_json::to_value(&published).unwrap(),
            serde_json::to_value(&expected).unwrap(),
            "the run-time bundle step's published output must be byte-identical to the retired \
             pre-graph prelude's own computation for the same inputs — that equality IS this \
             packet's faithfulness claim"
        );

        // `Step.output` and the shared envelope's `bundles` count are both
        // derived from the same run-time result — see this kind's own doc.
        let from_output: Vec<BundleInput> =
            serde_json::from_str(&out.output).expect("Step.output parses as the bundle list");
        assert_eq!(
            serde_json::to_value(&from_output).unwrap(),
            serde_json::to_value(&expected).unwrap(),
            "Step.output must carry the same bundles"
        );
        assert_eq!(
            env.lock().expect("shared review envelope mutex poisoned").bundles,
            expected.len(),
            "the shared envelope's bundle count is stamped from the real run-time result, not a \
             build-time snapshot"
        );
    }

    /// The degenerate empty-bundle-set behavior is UNCHANGED by this
    /// packet: an empty worktree/diff still publishes an empty
    /// `REVIEW_BUNDLES_ARTIFACT`, still yields a zero envelope count, and
    /// still leaves `ReviewJudgeStepKind::residency()`'s skip-load check
    /// (which reads the SAME artifact — see that method's own doc) with
    /// nothing to load, exactly like the pre-#1530 `ctx.bundles.is_empty()`
    /// check did.
    #[test]
    fn review_bundle_step_empty_diff_publishes_empty_bundles_and_zero_count() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let diff_text = ""; // no changed files at all

        let ctx = Arc::new(ReviewStepContext {
            case_id: "case-1".to_string(),
            roles: valid_crew(),
            intent_title: String::new(),
            intent_body: String::new(),
            diff: diff_text.to_string(),
            probe_system: String::new(),
            probe_role_prompts: BTreeMap::new(),
            judge_system: String::new(),
            verify_system: String::new(),
            remote_max_tokens_per_execution: 500_000,
            judge_exhaustion_strict: false,
            timeout_seconds: 30,
            chat_override: None,
            bundle_override: None,
            mission_id: None,
        });
        let env: Arc<StdMutex<ReviewEnvelope>> = Arc::new(StdMutex::new(ReviewEnvelope::default()));
        let mut bus = ArtifactBus::new();
        bus.seed(REVIEW_CONTEXT_ARTIFACT, ctx.clone() as Arc<dyn Any + Send + Sync>);
        bus.seed(REVIEW_ENVELOPE_ARTIFACT, env.clone() as Arc<dyn Any + Send + Sync>);
        bus.materialize(REVIEW_BUNDLES_ARTIFACT, make_review_bundles_artifact);
        let run_ctx = StepRunCtx::new(None, None, None, Arc::new(bus));

        let step = darkmux_crew::types::Step {
            id: "review-bundle-step".to_string(),
            task_id: "review-bundle-task".to_string(),
            gate: None,
            kind: "review.bundle".to_string(),
            status: NodeStatus::default(),
            config: serde_json::json!({
                "source": { "kind": "worktree", "path": dir.path().display().to_string() },
                "bundler": null,
                "diff_file": "/unused-when-bundler-is-null",
            }),
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let task = darkmux_crew::types::Task {
            id: "review-bundle-task".to_string(),
            phase_id: "investigate".to_string(),
            description: "bundle".to_string(),
            display_name: None,
            step_ids: vec!["review-bundle-step".to_string()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let input = BTreeMap::new();

        use darkmux_crew::step_kinds::StepKind as _;
        let kind = ReviewBundleStepKind;
        kind.run_streaming(&step, &task, &input, &run_ctx).expect("bundle step completes on an empty diff");

        let published = run_ctx
            .artifact::<StdMutex<Vec<BundleInput>>>(REVIEW_BUNDLES_ARTIFACT)
            .expect("this kind's own provides() materializes review.bundles")
            .lock()
            .expect("review bundles mutex poisoned")
            .clone();
        assert!(published.is_empty(), "an empty diff produces an empty bundle set");
        assert_eq!(env.lock().expect("shared review envelope mutex poisoned").bundles, 0);

        // `ReviewJudgeStepKind::residency()` skips loading a model whose
        // corresponding bundle set is empty — same downstream consumer this
        // packet re-pointed at `REVIEW_BUNDLES_ARTIFACT`.
        let judge_kind = ReviewJudgeStepKind;
        let judge_step = darkmux_crew::types::Step {
            id: "review-judge-step".to_string(),
            task_id: "review-judge-task".to_string(),
            gate: None,
            kind: "review.judge".to_string(),
            status: NodeStatus::default(),
            config: serde_json::json!({
                "model_key": "judge-model",
                "identifier": "judge-model",
                "n_ctx": 32_000,
            }),
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let judge_task = darkmux_crew::types::Task {
            id: "review-judge-task".to_string(),
            phase_id: "adjudicate".to_string(),
            description: "judge".to_string(),
            display_name: None,
            step_ids: vec!["review-judge-step".to_string()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        assert!(
            judge_kind.residency(&judge_step, &judge_task, &input, &run_ctx).is_none(),
            "an empty bundle set must still skip the judge model load, exactly like the retired \
             ctx.bundles.is_empty() check did"
        );
    }

    // ── (#2119) a pinned `--bundler` plugin declining a diff falls back to
    //    the built-in bundler instead of erroring the whole review ────────

    /// Write an executable shell stub standing in for a `--bundler` plugin
    /// that ignores its argv (same convention `lab::bundle::external`'s own
    /// `write_stub_script` uses) and just prints `stderr_line` then exits 1
    /// — this test only cares how `ReviewBundleStepKind::run_streaming`
    /// reacts to the exit, not that the stub actually bundles anything.
    #[cfg(unix)]
    fn write_stub_bundler_plugin(dir: &std::path::Path, stderr_line: &str) -> std::path::PathBuf {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("bundler-stub.sh");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "echo {} >&2", shell_quote(stderr_line)).unwrap();
        writeln!(f, "exit 1").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    /// Single-quote a shell word (the stub scripts' stderr lines are fixed
    /// test strings with no embedded single quotes, so the naive `'...'`
    /// wrap is sufficient — this exists only to keep `write_stub_bundler_
    /// plugin`'s call sites readable, not as a general-purpose shell
    /// escaper).
    #[cfg(unix)]
    fn shell_quote(s: &str) -> String {
        assert!(!s.contains('\''), "test fixture must not embed a single quote: {s}");
        format!("'{s}'")
    }

    /// The exact reference-plugin phrasing (`plugins/darkmux-bundler-rust/
    /// src/main.rs`'s `eprintln!`, darkmux#2119) — a fixture-local copy
    /// rather than a cross-crate import, since the plugin is a genuinely
    /// standalone Cargo project with zero path dependency on darkmux-lab
    /// (see that plugin's own module doc for why).
    const RUST_PLUGIN_DECLINE_MESSAGE: &str = "darkmux-bundler-rust: no .rs files with reviewable \
         hunks in this diff — nothing to bundle. (If this diff touches non-Rust files, the \
         built-in bundler or another --bundler plugin may be the right tool for it; this plugin \
         only handles .rs.)";

    /// THE #2119 fix, proven end to end: a pinned `--bundler` plugin that
    /// declines this diff (its own stderr matches the reference plugin's
    /// "nothing to bundle" phrasing, on a non-zero exit) must not fail the
    /// review graph — `ReviewBundleStepKind::run_streaming` falls back to
    /// the built-in bundler over the SAME diff, publishes exactly what an
    /// unpinned run would have, and names the fallback on the envelope so
    /// the posted review can say why. Uses the same `tests/fixtures/bundle/`
    /// worktree+diff the faithfulness test above uses, so "what the
    /// built-in bundler alone would have produced" is the same known-good
    /// `expected` value.
    #[cfg(unix)]
    #[test]
    fn review_bundle_step_falls_back_to_built_in_bundler_when_the_pinned_plugin_declines() {
        let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bundle");
        let worktree = fixture_dir.join("worktree");
        let diff_text = std::fs::read_to_string(fixture_dir.join("diff.patch"))
            .expect("read the bundle_golden integration test's own diff.patch fixture");

        let expected_source = FileSource::worktree(&worktree);
        let expected_set =
            build_bundles(&expected_source, &diff_text).expect("build_bundles over the bundle_golden fixture");
        let expected = bundle_inputs_from_set(&expected_set, &expected_source)
            .expect("bundle_inputs_from_set over the bundle_golden fixture");
        assert!(!expected.is_empty(), "the fixture must produce real bundles, or this test proves nothing");

        let stub_dir = tempfile::TempDir::new().expect("tempdir");
        let plugin = write_stub_bundler_plugin(stub_dir.path(), RUST_PLUGIN_DECLINE_MESSAGE);

        let ctx = Arc::new(ReviewStepContext {
            case_id: "case-1".to_string(),
            roles: valid_crew(),
            intent_title: String::new(),
            intent_body: String::new(),
            diff: diff_text,
            probe_system: String::new(),
            probe_role_prompts: BTreeMap::new(),
            judge_system: String::new(),
            verify_system: String::new(),
            remote_max_tokens_per_execution: 500_000,
            judge_exhaustion_strict: false,
            timeout_seconds: 30,
            chat_override: None,
            bundle_override: None,
            mission_id: None,
        });
        let env: Arc<StdMutex<ReviewEnvelope>> = Arc::new(StdMutex::new(ReviewEnvelope::default()));
        let mut bus = ArtifactBus::new();
        bus.seed(REVIEW_CONTEXT_ARTIFACT, ctx.clone() as Arc<dyn Any + Send + Sync>);
        bus.seed(REVIEW_ENVELOPE_ARTIFACT, env.clone() as Arc<dyn Any + Send + Sync>);
        bus.materialize(REVIEW_BUNDLES_ARTIFACT, make_review_bundles_artifact);
        let run_ctx = StepRunCtx::new(None, None, None, Arc::new(bus));

        let diff_file = stub_dir.path().join("d.diff");
        std::fs::write(&diff_file, "").expect("write a placeholder diff file (the stub plugin never reads it)");
        let step = darkmux_crew::types::Step {
            id: "review-bundle-step".to_string(),
            task_id: "review-bundle-task".to_string(),
            gate: None,
            kind: "review.bundle".to_string(),
            status: NodeStatus::default(),
            config: serde_json::json!({
                "source": { "kind": "worktree", "path": worktree.display().to_string() },
                "bundler": plugin.display().to_string(),
                "diff_file": diff_file.display().to_string(),
            }),
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let task = darkmux_crew::types::Task {
            id: "review-bundle-task".to_string(),
            phase_id: "investigate".to_string(),
            description: "bundle".to_string(),
            display_name: None,
            step_ids: vec!["review-bundle-step".to_string()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let input = BTreeMap::new();

        use darkmux_crew::step_kinds::StepKind as _;
        let kind = ReviewBundleStepKind;
        kind.run_streaming(&step, &task, &input, &run_ctx)
            .expect("a plugin decline must fall back to the built-in bundler, not error the step");

        let published: Vec<BundleInput> = run_ctx
            .artifact::<StdMutex<Vec<BundleInput>>>(REVIEW_BUNDLES_ARTIFACT)
            .expect("this kind's own provides() materializes review.bundles")
            .lock()
            .expect("review bundles mutex poisoned")
            .clone();
        assert_eq!(
            serde_json::to_value(&published).unwrap(),
            serde_json::to_value(&expected).unwrap(),
            "the fallback must publish exactly what an unpinned (built-in-bundler) run would have"
        );

        let env = env.lock().expect("shared review envelope mutex poisoned");
        assert_eq!(env.bundles, expected.len());
        assert!(
            env.bundle_skip.is_some(),
            "the fallback must carry the BUILT-IN bundler's real per-file skip accounting, not the \
             plugin's empty default"
        );
        let fallback = env
            .bundler_fallback
            .as_deref()
            .expect("bundler_fallback must be set when a plugin declined and the step fell back");
        assert!(
            fallback.starts_with("built-in (plugin declined: "),
            "unexpected bundler_fallback shape: {fallback}"
        );
        assert!(
            fallback.contains("nothing to bundle"),
            "the fallback reason must carry the plugin's own message: {fallback}"
        );
    }

    /// The converse of the fallback above: a pinned `--bundler` plugin that
    /// fails for a REAL reason (a crash, a message that does not match the
    /// reference plugin's decline phrasing) must still error the whole
    /// step, exactly as before #2119 — the fallback is scoped to the one
    /// named decline shape, never a blanket "any non-zero exit is fine."
    #[cfg(unix)]
    #[test]
    fn review_bundle_step_still_errors_when_the_pinned_plugin_fails_for_a_real_reason() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let stub_dir = tempfile::TempDir::new().expect("tempdir");
        let plugin = write_stub_bundler_plugin(stub_dir.path(), "some-bundler: panicked reading the diff file");

        let ctx = Arc::new(ReviewStepContext {
            case_id: "case-1".to_string(),
            roles: valid_crew(),
            intent_title: String::new(),
            intent_body: String::new(),
            diff: String::new(),
            probe_system: String::new(),
            probe_role_prompts: BTreeMap::new(),
            judge_system: String::new(),
            verify_system: String::new(),
            remote_max_tokens_per_execution: 500_000,
            judge_exhaustion_strict: false,
            timeout_seconds: 30,
            chat_override: None,
            bundle_override: None,
            mission_id: None,
        });
        let env: Arc<StdMutex<ReviewEnvelope>> = Arc::new(StdMutex::new(ReviewEnvelope::default()));
        let mut bus = ArtifactBus::new();
        bus.seed(REVIEW_CONTEXT_ARTIFACT, ctx.clone() as Arc<dyn Any + Send + Sync>);
        bus.seed(REVIEW_ENVELOPE_ARTIFACT, env.clone() as Arc<dyn Any + Send + Sync>);
        bus.materialize(REVIEW_BUNDLES_ARTIFACT, make_review_bundles_artifact);
        let run_ctx = StepRunCtx::new(None, None, None, Arc::new(bus));

        let diff_file = stub_dir.path().join("d.diff");
        std::fs::write(&diff_file, "").expect("write a placeholder diff file (the stub plugin never reads it)");
        let step = darkmux_crew::types::Step {
            id: "review-bundle-step".to_string(),
            task_id: "review-bundle-task".to_string(),
            gate: None,
            kind: "review.bundle".to_string(),
            status: NodeStatus::default(),
            config: serde_json::json!({
                "source": { "kind": "worktree", "path": dir.path().display().to_string() },
                "bundler": plugin.display().to_string(),
                "diff_file": diff_file.display().to_string(),
            }),
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let task = darkmux_crew::types::Task {
            id: "review-bundle-task".to_string(),
            phase_id: "investigate".to_string(),
            description: "bundle".to_string(),
            display_name: None,
            step_ids: vec!["review-bundle-step".to_string()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let input = BTreeMap::new();

        use darkmux_crew::step_kinds::StepKind as _;
        let kind = ReviewBundleStepKind;
        let err = kind
            .run_streaming(&step, &task, &input, &run_ctx)
            .expect_err("a genuine plugin failure must still error the step, unchanged from before #2119");
        let msg = format!("{err:#}");
        assert!(msg.contains("panicked reading the diff file"), "unexpected error message: {msg}");
        assert!(
            !msg.contains("nothing to bundle"),
            "a genuine failure must never be misread as a decline: {msg}"
        );
    }

    // ── #1641: this pipeline's OWN directly-emitted records carry a real
    //    mission identity instead of a hardcoded `None` ─────────────────

    /// (#1641) `review_step_result_record` — and by extension every
    /// `emit_review_step_result`/`apply_verify_results` call site that
    /// writes straight to the global flow sink (bypassing the caller-
    /// injected `ReviewEmitter` entirely, since those run on `run_bounded`
    /// worker threads — see `emit_review_step_result`'s own doc) — must
    /// carry a real `mission_id` when the caller's `ReviewStepContext`
    /// supplies one, rather than the pre-fix hardcoded `None`. This is the
    /// "review dispatch opts carry a phase/mission identity" requirement:
    /// asserting on `FlowRecord.mission_id` directly, not any downstream or
    /// coincidental field.
    #[test]
    fn review_step_result_record_carries_mission_id_when_the_context_supplies_one() {
        let with_mission = review_step_result_record(
            "review.judge",
            "review-judge-step",
            "case-1",
            Some("mission-xyz"),
            serde_json::json!({ "items_out": 3 }),
        );
        assert_eq!(
            with_mission.mission_id.as_deref(),
            Some("mission-xyz"),
            "a supplied mission_id must land on the record's own mission_id field, not be dropped"
        );

        // The `--charges-file`/lab-bench honest-`None` case (no Mission ever
        // minted) must still produce `None`, not a fabricated value.
        let without_mission = review_step_result_record(
            "review.judge",
            "review-judge-step",
            "case-1",
            None,
            serde_json::json!({ "items_out": 3 }),
        );
        assert_eq!(without_mission.mission_id, None);
    }

    /// (#1641) The same threading, exercised through the ACTUAL production
    /// call path: `dispatch_chat` -> `emit_review_token_telemetry` ->
    /// `build_telemetry_record`, driven by `ReviewStepContext::mission_id`
    /// rather than the function being called directly. Uses
    /// `chat_override` (the existing #1355 dispatch seam) so no real
    /// LMStudio call happens; `emit_review_token_telemetry` writes to the
    /// GLOBAL flow sink (it can't hold an injected emitter — see its own
    /// doc), so this test points `DARKMUX_FLOWS_DIR` at a fresh tempdir and
    /// reads the record back off disk, the same way the mission-graph
    /// viewer's backfill would.
    #[test]
    #[serial_test::serial]
    fn dispatch_chat_stamps_mission_id_onto_the_token_telemetry_record() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        // SAFETY: serialized via #[serial_test::serial].
        unsafe {
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        let crew = valid_crew();
        let ctx = ReviewStepContext {
            case_id: "case-mission-token-test".to_string(),
            roles: crew,
            intent_title: String::new(),
            intent_body: String::new(),
            diff: DIFF.to_string(),
            probe_system: String::new(),
            probe_role_prompts: BTreeMap::new(),
            judge_system: String::new(),
            verify_system: String::new(),
            remote_max_tokens_per_execution: 500_000,
            judge_exhaustion_strict: false,
            timeout_seconds: 30,
            chat_override: Some(Arc::new(|_call: &ChatCall| {
                Ok(SingleShotReply {
                    content: "ok".to_string(),
                    total_tokens: Some(15),
                    prompt_tokens: Some(10),
                    completion_tokens: Some(5),
                    model: None,
                })
            })),
            bundle_override: None,
            mission_id: Some("mission-token-test".to_string()),
        };
        let call = ChatCall {
            model: "test-model",
            system: "sys",
            user: "hello",
            temperature: 0.0,
            max_tokens: 100,
            endpoint: None,
        };
        dispatch_chat(&ctx, &call).expect("mocked dispatch_chat must succeed");

        // SAFETY: serialized via #[serial_test::serial].
        unsafe {
            match &prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }

        let mut found = false;
        for entry in std::fs::read_dir(tmp.path()).expect("read flows dir").filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let contents = std::fs::read_to_string(&path).unwrap_or_default();
            for line in contents.lines() {
                let Ok(rec) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                if rec.get("action").and_then(|v| v.as_str()) != Some("telemetry.tokens") {
                    continue;
                }
                // (#1544-family hermeticity) Scope to THIS test's records via
                // its own case id (the telemetry record's session_id). The
                // flow sink is env-var global and `#[serial]` only serializes
                // tests that OPT IN — a concurrent non-serial test that
                // dispatches writes ITS records into this test's tempdir
                // during the window, and an unscoped scan then asserts
                // against a foreign record. Observed live: the coverage CI
                // run failed this test on another test's `case-1` /
                // `darkmux:judge-model` record.
                if rec.get("session_id").and_then(|v| v.as_str())
                    != Some("case-mission-token-test")
                {
                    continue;
                }
                assert_eq!(
                    rec.get("mission_id").and_then(|v| v.as_str()),
                    Some("mission-token-test"),
                    "the telemetry.tokens record must carry the context's mission_id, got {rec:#?}"
                );
                found = true;
            }
        }
        assert!(found, "expected a telemetry.tokens record on disk after dispatch_chat");
    }

    // ── (#1605) classify_zero_bundle_degenerate: benign-empty vs error ──
    //
    // darkmux#1605 cause 1: `bundles: 0` plus a fixed string couldn't
    // distinguish "diff was entirely non-code" (benign — nothing to
    // review) from "bundler bug"/"internal limit hit" (a real degenerate
    // outcome). `ReviewEnvelope::degenerate_kind` is the typed field the
    // workflow/render layer branches on instead of string-matching
    // `degenerate`'s prose.

    fn skip_report(entries: Vec<(&str, SkipReason)>) -> BundleSkipReport {
        BundleSkipReport {
            files_considered: entries.len(),
            files_skipped: entries
                .into_iter()
                .map(|(path, reason)| SkippedFile { path: path.to_string(), reason, function: None })
                .collect(),
        }
    }

    #[test]
    fn a_pure_test_file_diff_is_benign_but_is_not_called_non_code() {
        // (#1605 QA finding) `scan::ts_file` rejects `tests/` and any
        // basename containing "test" — real TypeScript the bundler
        // deliberately excludes. Collapsing that into NonCodeExtension made
        // the no-op comment tell the author their test files were "fixtures,
        // lockfiles, or generated config". Benign is right; the LABEL was a
        // lie, in the one comment whose whole job is honesty about why
        // nothing was reviewed.
        let report = BundleSkipReport {
            files_considered: 2,
            files_skipped: vec![
                SkippedFile {
                    path: "src/foo.test.ts".to_string(),
                    reason: SkipReason::TestFileExcluded,
                    function: None,
                },
                SkippedFile {
                    path: "tests/bar.ts".to_string(),
                    reason: SkipReason::TestFileExcluded,
                    function: None,
                },
            ],
        };
        let (msg, kind) = classify_zero_bundle_degenerate(&Some(report));
        assert_eq!(kind, DegenerateKind::BenignEmpty, "a deliberate exclusion is benign");
        assert!(
            msg.contains("test file"),
            "the breakdown must name the real reason, not 'non-code extension': {msg}"
        );
        assert!(
            !msg.contains("non-code extension"),
            "a .ts test file must never be reported as a non-code extension: {msg}"
        );
    }

    #[test]
    fn classify_zero_bundle_degenerate_every_skip_non_code_extension_is_benign() {
        let skip = skip_report(vec![
            ("package-lock.json", SkipReason::NonCodeExtension),
            ("fixtures/sample.json", SkipReason::NonCodeExtension),
        ]);
        let (msg, kind) = classify_zero_bundle_degenerate(&Some(skip));
        assert_eq!(kind, DegenerateKind::BenignEmpty, "every skip was non-code — this is benign, not an error");
        assert!(msg.contains("2 file(s) considered"), "{msg}");
        assert!(msg.contains("2 skipped"), "{msg}");
        assert!(msg.contains("non-code extension"), "the summary must name the reason, not just a count: {msg}");
    }

    #[test]
    fn classify_zero_bundle_degenerate_mixed_reasons_stay_error() {
        // One benign skip, one that isn't (unreadable) — a MIX must never
        // be classified benign, because the non-benign file's absence is
        // unexplained-by-benignity even though the other one is fine.
        let skip = skip_report(vec![
            ("package-lock.json", SkipReason::NonCodeExtension),
            ("src/missing.ts", SkipReason::UnreadableInWorktree),
        ]);
        let (msg, kind) = classify_zero_bundle_degenerate(&Some(skip));
        assert_eq!(kind, DegenerateKind::Error, "a mix of benign and non-benign reasons must stay Error");
        assert!(msg.contains("unreadable in worktree"), "{msg}");
        assert!(msg.contains("non-code extension"), "{msg}");
    }

    #[test]
    fn classify_zero_bundle_degenerate_over_size_cap_is_error_not_benign() {
        // darkmux#1605's third distinguished case ("diff exceeded some
        // internal bound") is real code the bundler declined on policy —
        // never a benign "nothing here".
        let skip = skip_report(vec![("src/huge.ts", SkipReason::OverSizeCap)]);
        let (msg, kind) = classify_zero_bundle_degenerate(&Some(skip));
        assert_eq!(kind, DegenerateKind::Error);
        assert!(msg.contains("over the bundler's size cap"), "{msg}");
    }

    // ── (#1959) classify_zero_bundle_degenerate: workspace-spec exclusion ──

    #[test]
    fn classify_zero_bundle_degenerate_excluded_by_workspace_spec_is_benign() {
        // A review scoped to a workspace spec whose include/exclude drops
        // every touched file is a deliberate operator scoping decision,
        // same benign treatment as TestFileExcluded — never a bundler
        // failure.
        let skip = skip_report(vec![
            ("vendor/generated.ts", SkipReason::ExcludedByWorkspaceSpec),
            ("vendor/another.ts", SkipReason::ExcludedByWorkspaceSpec),
        ]);
        let (msg, kind) = classify_zero_bundle_degenerate(&Some(skip));
        assert_eq!(kind, DegenerateKind::BenignEmpty, "a workspace-spec exclusion is a deliberate scope, not a failure");
        assert!(msg.contains("excluded by the workspace spec"), "{msg}");
    }

    #[test]
    fn classify_zero_bundle_degenerate_workspace_excluded_mixed_with_unreadable_stays_error() {
        // A workspace-spec exclusion mixed with a genuine error reason
        // must never launder that error into benign.
        let skip = skip_report(vec![
            ("vendor/generated.ts", SkipReason::ExcludedByWorkspaceSpec),
            ("src/missing.ts", SkipReason::UnreadableInWorktree),
        ]);
        let (_msg, kind) = classify_zero_bundle_degenerate(&Some(skip));
        assert_eq!(kind, DegenerateKind::Error);
    }

    // ── (#1757) classify_zero_bundle_degenerate: unsupported-language ──

    #[test]
    fn a_sql_only_diff_is_unsupported_language_not_benign_and_not_error() {
        // The motivating case: a `.sql`-only PR is real source code, not
        // "nothing to review" — but the built-in TypeScript-only bundler
        // has no way to read it. This must NOT fail the check (it's not
        // `Error`) and must NOT read as benign (it's not `BenignEmpty`).
        let skip = skip_report(vec![("migrations/001_add_users.sql", SkipReason::SourceLanguageUnsupported)]);
        let (msg, kind) = classify_zero_bundle_degenerate(&Some(skip));
        assert_eq!(
            kind,
            DegenerateKind::UnsupportedLanguage,
            "real source in an unsupported language must classify neutrally, not benign and not error"
        );
        assert!(msg.contains("real source in an unsupported language"), "{msg}");
    }

    #[test]
    fn unsupported_language_mixed_with_benign_reasons_stays_unsupported_language() {
        // A `.css`-only PR alongside a lockfile bump: the benign file
        // doesn't change the classification once real unparseable source
        // is present — the run is still "bring your own bundler," not
        // "nothing here."
        let skip = skip_report(vec![
            ("package-lock.json", SkipReason::NonCodeExtension),
            ("src/styles/app.css", SkipReason::SourceLanguageUnsupported),
        ]);
        let (_msg, kind) = classify_zero_bundle_degenerate(&Some(skip));
        assert_eq!(kind, DegenerateKind::UnsupportedLanguage);
    }

    #[test]
    fn unsupported_language_mixed_with_a_real_error_reason_stays_error() {
        // A genuine bundler-limit decline (`OverSizeCap`) alongside an
        // unsupported-language file must NOT be swallowed into the neutral
        // outcome — a real error mixed in keeps the run loud.
        let skip = skip_report(vec![
            ("src/huge.ts", SkipReason::OverSizeCap),
            ("migrations/001_add_users.sql", SkipReason::SourceLanguageUnsupported),
        ]);
        let (_msg, kind) = classify_zero_bundle_degenerate(&Some(skip));
        assert_eq!(kind, DegenerateKind::Error, "a real error reason mixed in must stay Error, never neutral");
    }

    #[test]
    fn a_genuinely_benign_diff_stays_benign_never_unsupported_language() {
        // Inverted case: a diff whose ONLY skips are the deliberately
        // benign reasons (no unsupported-language file at all) must keep
        // reading as `BenignEmpty` — the new `UnsupportedLanguage` bucket
        // must never widen to swallow the existing benign classification.
        let skip = skip_report(vec![
            ("package-lock.json", SkipReason::NonCodeExtension),
            ("fixtures/sample.json", SkipReason::NonCodeExtension),
        ]);
        let (_msg, kind) = classify_zero_bundle_degenerate(&Some(skip));
        assert_eq!(
            kind,
            DegenerateKind::BenignEmpty,
            "a lockfile/json-only diff must stay benign, never reclassified as unsupported language"
        );
    }

    #[test]
    fn classify_zero_bundle_degenerate_no_skip_data_stays_error_never_guesses_benign() {
        // `bundle_override`/an external `--bundler` plugin carries no skip
        // bookkeeping at all — the honest default is "can't explain this",
        // never a guessed benign classification.
        let (msg, kind) = classify_zero_bundle_degenerate(&None);
        assert_eq!(kind, DegenerateKind::Error);
        assert_eq!(msg, "no bundles produced from the diff");
    }

    #[test]
    fn classify_zero_bundle_degenerate_empty_skip_list_stays_error() {
        // `files_considered > 0` but nothing recorded as skipped is
        // internally inconsistent (every considered file should have
        // either contributed a bundle or been skipped) — never guess
        // benign from an empty breakdown.
        let skip = BundleSkipReport { files_considered: 3, files_skipped: Vec::new() };
        let (_msg, kind) = classify_zero_bundle_degenerate(&Some(skip));
        assert_eq!(kind, DegenerateKind::Error);
    }

    // ── (#1876/#1877) review_outcome: the review-owned RunOutcome mapping ──

    /// A healthy envelope (no `degenerate`, no exhausted judge budget row)
    /// maps to `Complete`.
    #[test]
    fn review_outcome_healthy_envelope_is_complete() {
        let env = ReviewEnvelope { degenerate: None, ..Default::default() };
        assert_eq!(review_outcome(&env), RunOutcome::Complete);
    }

    /// `env.degenerate` set (Gate 2's zero-usable-rulings honesty gate, the
    /// strict-policy Gate 1, or any other reason review has ever set it
    /// for) always maps to `Empty` — the "no signal" wording's message
    /// carries through verbatim, unmodified by this function.
    #[test]
    fn review_outcome_degenerate_envelope_is_empty_with_the_reason_carried_verbatim() {
        let env = ReviewEnvelope {
            degenerate: Some("judge produced no usable ruling on any of 5 flags (all errored/unparsed)".to_string()),
            ..Default::default()
        };
        assert_eq!(
            review_outcome(&env),
            RunOutcome::Empty {
                reason: "judge produced no usable ruling on any of 5 flags (all errored/unparsed)".to_string()
            }
        );
    }

    /// The #1876 production shape, built directly against `review_outcome`
    /// (not through a real dispatch): no `degenerate`, but a `judge-pass1`
    /// remote-budget row with `skipped_calls > 0` — maps to `Partial`, with
    /// a reason built from the row's OWN numbers (never a fixed string).
    #[test]
    fn review_outcome_judge_budget_row_with_skips_and_no_degenerate_is_partial() {
        let env = ReviewEnvelope {
            degenerate: None,
            judged: (0..134).map(archived_flag_for_outcome_test).collect(),
            remote_budgets: vec![RemoteBudgetRecord {
                stage: "judge-pass1".to_string(),
                max_tokens: 500_000,
                used_tokens: 500_497,
                exhausted: true,
                skipped_calls: 11,
            }],
            ..Default::default()
        };
        match review_outcome(&env) {
            RunOutcome::Partial { reasons } => {
                assert_eq!(reasons.len(), 1);
                assert!(reasons[0].contains("11 of 134 flags went unjudged"), "got: {}", reasons[0]);
                assert!(reasons[0].contains("judge-pass1"), "got: {}", reasons[0]);
                assert!(reasons[0].contains("500497"), "the used-token count is the row's own number: {}", reasons[0]);
                // (#1888) The allowance figure is the row's own `max_tokens`,
                // never a stray constant — a mutation that swaps `r.max_tokens`
                // for a hardcoded literal in the format args must fail here.
                assert!(
                    reasons[0].contains("500000-token allowance"),
                    "the allowance is the row's own max_tokens, not a stray literal: {}",
                    reasons[0]
                );
            }
            other => panic!("expected Partial, got {other:?}"),
        }
    }

    /// A PROBE-stage budget row with skips (already its own correct,
    /// independently-tested "reduced coverage" warning treatment) must NOT
    /// also trigger `Partial` — scoped to judge stages only, per
    /// `review_outcome`'s own doc. Same for a VERIFY-stage row: it already
    /// renders normally with its own `env.warnings` note
    /// (`synthesize_verify_exhaustion_posts_verified_plus_header_marker_
    /// plus_warning` in `src/pr_review.rs` pins that treatment); folding it
    /// into `Partial` too would double-announce an already-correct outcome.
    #[test]
    fn review_outcome_ignores_non_judge_stage_budget_rows() {
        let env = ReviewEnvelope {
            degenerate: None,
            remote_budgets: vec![
                RemoteBudgetRecord {
                    stage: "probe".to_string(),
                    max_tokens: 1_000,
                    used_tokens: 1_000,
                    exhausted: true,
                    skipped_calls: 2,
                },
                RemoteBudgetRecord {
                    stage: "verify".to_string(),
                    max_tokens: 1_000,
                    used_tokens: 1_000,
                    exhausted: true,
                    skipped_calls: 1,
                },
            ],
            ..Default::default()
        };
        assert_eq!(review_outcome(&env), RunOutcome::Complete, "non-judge budget skips must not trigger Partial");
    }

    /// (#1876/#1877 QA follow-up, MUST FIX 2) A `judge-pass2` skip means
    /// something different from a `judge-pass1` skip — the flag WAS judged
    /// (pass-1 already confirmed it) and only its CONFIRMATION pass was
    /// skipped, conservatively demoting it to needs-check rather than
    /// leaving it unjudged. This pins `judge_budget_shortfall_reason`'s
    /// pass-2 wording end to end through `review_outcome`, since nothing
    /// else in this module ever constructs a `judge-pass2`
    /// `RemoteBudgetRecord` — the QA round's own headline correction
    /// (distinguishing "N went unjudged" from "N conservatively demoted")
    /// had zero coverage before this test: reintroducing the bug by
    /// collapsing `judge_budget_shortfall_reason`'s `if r.stage ==
    /// "judge-pass1"` branch to `if true` left `cargo test --workspace`
    /// fully green.
    #[test]
    fn review_outcome_judge_pass2_skip_reads_as_a_demotion_not_an_unjudged_count() {
        let env = ReviewEnvelope {
            degenerate: None,
            remote_budgets: vec![RemoteBudgetRecord {
                stage: "judge-pass2".to_string(),
                max_tokens: 500_000,
                used_tokens: 500_210,
                exhausted: true,
                skipped_calls: 4,
            }],
            ..Default::default()
        };
        match review_outcome(&env) {
            RunOutcome::Partial { reasons } => {
                assert_eq!(reasons.len(), 1);
                assert!(
                    reasons[0].contains("4 confirmed finding(s) were conservatively demoted to needs-check"),
                    "got: {}",
                    reasons[0]
                );
                assert!(reasons[0].contains("judge-pass2"), "got: {}", reasons[0]);
                assert!(reasons[0].contains("500210"), "the used-token count is the row's own number: {}", reasons[0]);
                // (#1888) Pins the pass-2 arm's allowance figure too — the
                // same `r.max_tokens` interpolation the pass-1 arm uses.
                assert!(
                    reasons[0].contains("500000-token allowance"),
                    "the allowance is the row's own max_tokens, not a stray literal: {}",
                    reasons[0]
                );
                assert!(
                    !reasons[0].contains("went unjudged"),
                    "a pass-2 skip (already judged, only demoted) must not read like a pass-1 \
                     unjudged count: {}",
                    reasons[0]
                );
            }
            other => panic!("expected Partial, got {other:?}"),
        }
    }

    // ── (#1877 item 2) review_mission_outcome: the MissionEnvelope-facing
    // ── RunOutcome mapping, deliberately distinct from `review_outcome` ──

    #[test]
    fn review_mission_outcome_healthy_envelope_is_complete() {
        let env = ReviewEnvelope { degenerate: None, warnings: Vec::new(), ..Default::default() };
        assert_eq!(review_mission_outcome(&env), RunOutcome::Complete);
    }

    /// The neutral carve-out (#1654/#1757): a `BenignEmpty` degenerate
    /// reason maps to `Complete`, NOT `Empty` — `review_outcome` (the
    /// banner-facing function) would call this `Empty`, and that
    /// divergence is the whole reason this is a separate function. Pins
    /// the exact regression this PR must not introduce: a benign-empty
    /// review flipping the mission board from `Clean` to `Degenerate`.
    #[test]
    fn review_mission_outcome_benign_empty_degenerate_is_complete_not_empty() {
        let env = ReviewEnvelope {
            degenerate: Some("no bundles produced from the diff — 2 skipped (2 non-code)".to_string()),
            degenerate_kind: Some(DegenerateKind::BenignEmpty),
            ..Default::default()
        };
        assert_eq!(
            review_mission_outcome(&env),
            RunOutcome::Complete,
            "a benign-empty review must not read Empty on the mission-envelope outcome"
        );
    }

    /// Same carve-out, the `UnsupportedLanguage` half (#1757).
    #[test]
    fn review_mission_outcome_unsupported_language_degenerate_is_complete_not_empty() {
        let env = ReviewEnvelope {
            degenerate: Some(
                "no bundles produced from the diff — 1 skipped (1 real source in an unsupported language)"
                    .to_string(),
            ),
            degenerate_kind: Some(DegenerateKind::UnsupportedLanguage),
            ..Default::default()
        };
        assert_eq!(review_mission_outcome(&env), RunOutcome::Complete);
    }

    /// The control: a genuine `Error`-kind degenerate run stays `Empty`.
    #[test]
    fn review_mission_outcome_error_kind_degenerate_is_empty() {
        let env = ReviewEnvelope {
            degenerate: Some("no bundles produced from the diff".to_string()),
            degenerate_kind: Some(DegenerateKind::Error),
            ..Default::default()
        };
        assert_eq!(
            review_mission_outcome(&env),
            RunOutcome::Empty { reason: "no bundles produced from the diff".to_string() }
        );
    }

    /// The other divergence from `review_outcome`: ANY non-empty
    /// `env.warnings` maps to `Partial`, not only a judge-stage remote
    /// budget skip. A probe-seat bounded-retry failure (no judge exhaustion
    /// at all) must still flag the mission board, matching the pre-#1877
    /// `!env.warnings.is_empty() -> Degraded` rule this function replaces.
    #[test]
    fn review_mission_outcome_any_warning_is_partial_not_only_judge_stage_skips() {
        let env = ReviewEnvelope {
            degenerate: None,
            warnings: vec!["remote probe seat failed after bounded retries".to_string()],
            remote_budgets: Vec::new(),
            ..Default::default()
        };
        assert_eq!(
            review_mission_outcome(&env),
            RunOutcome::Partial { reasons: vec!["remote probe seat failed after bounded retries".to_string()] }
        );
    }

    /// The #1876 production shape carries through this function too, with
    /// its real numbers intact.
    #[test]
    fn review_mission_outcome_judge_budget_warning_is_partial_with_real_numbers() {
        let env = ReviewEnvelope {
            degenerate: None,
            warnings: vec![
                "11 of 134 flags went unjudged on the `judge-pass1` stage — it exceeded its 500000-token \
                 allowance (500497 used)"
                    .to_string(),
            ],
            ..Default::default()
        };
        match review_mission_outcome(&env) {
            RunOutcome::Partial { reasons } => {
                assert_eq!(reasons.len(), 1);
                assert!(reasons[0].contains("11 of 134 flags went unjudged"));
                assert!(reasons[0].contains("500497"));
            }
            other => panic!("expected Partial, got {other:?}"),
        }
    }

    /// (#1877 QA ALSO FIX 1) `review_outcome` reads `env.remote_budgets`
    /// (structured), `review_mission_outcome` reads `env.warnings` (prose).
    /// They agree today ONLY because `judge_gate_outcome`'s two call sites
    /// push BOTH onto the envelope from the same gate result — a fact
    /// enforced by convention at each call site, not by any type. This
    /// test drives the REAL gate (not a hand-built envelope) with a
    /// starved judge-pass1 bucket and asserts both predicates land
    /// non-`Complete` off that one input, so a later cleanup that drops
    /// the coverage-warning string in favor of the structured rows alone
    /// cannot silently recreate #1876's shape (PR banner says "Incomplete
    /// review", mission board says `Clean`) without failing here first.
    #[test]
    fn judge_gate_outcome_keeps_review_outcome_and_review_mission_outcome_in_sync() {
        let mut pass1 = RemoteBudget::with_stage("judge-pass1", 1_000, MIN_VIABLE_JUDGE_GRANT);
        let g = pass1.admit_reserve(600).expect("first draw admits");
        pass1.settle(g, 600, 1);
        // Second draw: 400 remaining < the 512 floor — denied, counted as
        // a skip. This is the real starved-bucket shape #1876 fixed, not a
        // hand-set `skipped_calls` field.
        assert!(pass1.admit_reserve(600).is_none(), "the starved grant must be denied");
        let pass2 = RemoteBudget::with_stage("judge-pass2", 1_000, MIN_VIABLE_JUDGE_GRANT);
        let budgets = JudgeBudgets { pass1, pass2 };

        let gate = judge_gate_outcome(
            true, // is_remote
            2,    // judged_len
            1,    // usable — one flag still got a real ruling
            0,    // dispatch_errors
            Some(&budgets),
            1_000, // remote_max_tokens_per_execution
            false, // strict — the #1876 default policy
        );

        // Mirror the real call sites (`run_judge_only`, the graph judge
        // step): push both the structured rows and the coverage warning
        // onto the same envelope.
        let mut env = ReviewEnvelope {
            degenerate: None,
            judged: (0..2).map(archived_flag_for_outcome_test).collect(),
            ..Default::default()
        };
        env.remote_budgets.extend(gate.remote_budget_rows);
        if let Some(w) = gate.coverage_warning {
            env.warnings.push(w);
        }

        assert_ne!(
            review_outcome(&env),
            RunOutcome::Complete,
            "review_outcome must see the skipped judge-pass1 row"
        );
        assert_ne!(
            review_mission_outcome(&env),
            RunOutcome::Complete,
            "review_mission_outcome must see the coverage warning"
        );
    }

    /// A minimal `JudgedFlag` fixture for `review_outcome`'s own unit
    /// tests — distinct from `src/pr_review.rs`'s richer `archived_flag`
    /// (that one is `pub(crate)` to `pr_review`'s own test module, not
    /// reachable here), and this module only needs a flag that exists and
    /// carries a distinct bundle id.
    fn archived_flag_for_outcome_test(i: usize) -> JudgedFlag {
        JudgedFlag {
            flag: ProbeFlag {
                bundle_id: format!("fn{i}@src/x.ts"),
                fact_family: "unscoped".to_string(),
                member: "darkmux:probe-model".to_string(),
                draw: 0,
                charge_text: "a flagged concern".to_string(),
                anchor: None,
                also_flagged: Vec::new(),
            },
            pass1: JudgeRecord {
                ruling: JudgeRuling::FalsePositive,
                decisive_evidence: String::new(),
                note_for_author: String::new(),
                pass: 1,
                seconds: 0.0,
            },
            pass2: None,
            tier: Tier::Archived,
            demoted_by_pass2: false,
            verify: None,
            demoted_by_verify: false,
            absence_backstop: None,
        }
    }
