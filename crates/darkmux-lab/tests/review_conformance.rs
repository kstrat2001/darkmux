//! (#2310 P0) Conformance harness for the CURRENT review pipeline —
//! `mission launch review`'s graph, built + run through the exact same
//! entry points `src/mission_launch_review.rs` calls
//! (`build_review_graph`, which loads the SHIPPED
//! `templates/builtin/mission-configs/review.json`, and `run_review_graph`)
//! — pinned as a byte-level golden. This is the regression net for the
//! #2310 refactor packets that follow (P1-P4: typed step outputs, retiring
//! the bespoke launcher); it makes NO production-code changes beyond
//! whatever seam this file's own comments call out as strictly required
//! (none, as it turns out — every seam this harness needs
//! (`ReviewStepContext::chat_override`/`bundle_override`,
//! `build_review_graph`/`run_review_graph` themselves) already exists,
//! added by #1355/#1530 for exactly this purpose).
//!
//! **Hermeticity.** No LMStudio, no Docker, no network, no
//! `~/.darkmux` writes:
//! - `HomeGuard` scopes `DARKMUX_HOME` to a per-test tempdir (same pattern
//!   as `crates/darkmux-lab/src/crawl/unit_step_tests.rs::HomeGuard`), so
//!   `build_review_graph`'s `mission_config::load("review")` never touches
//!   the operator's real config root (it still resolves the review
//!   document — user-tier is empty, so it falls to the on-disk template
//!   under this repo checkout or the binary-embedded copy; either way the
//!   CONTENT is byte-identical to `templates/builtin/mission-configs/
//!   review.json`, since both are sourced from that same file).
//! - Every probe/judge/verify seat is a LOCAL-shaped `ResolvedSeatStaffing`
//!   (a bare `ProfileModel { id, .. Default::default() }` — no `n_ctx`, no
//!   `endpoint`). No `n_ctx` reports `Residency::Remote` to the scheduler
//!   (see `ReviewStepContext::chat_override`'s own doc), so `run_bounded`'s
//!   Remote track never touches the real `host_factory` (`lms`) at all —
//!   the exact hermeticity trick `review_tests.rs`'s own `graph_pm`/
//!   `graph_staffing` helpers use. No `endpoint` also means every
//!   `MemberRecord::remote` comes back `false`, so zero remote budget is
//!   drawn (`ReviewStepContext::remote_max_tokens_per_execution` is set
//!   high but is never actually charged against).
//! - `ReviewStepContext::chat_override` replaces every model call with a
//!   canned reply (this file's `chat_fn`, keyed by seat — see its own doc).
//! - `ReviewStepContext::bundle_override` replaces the real bundler (which
//!   would need a real worktree/diff file) with three synthetic
//!   `BundleInput`s built in-process — the `bundle_spec` parameter is a
//!   dummy, nonexistent path, exactly `review_tests.rs::dummy_bundle_spec`.
//!
//! To regenerate the golden after a deliberate, reviewed behavior change:
//! `DARKMUX_REVIEW_CONFORMANCE_UPDATE_GOLDEN=1 cargo test -p darkmux-lab \
//!  --test review_conformance` then review the diff before committing.

use anyhow::Result;
use darkmux_crew::resourcing::{ResolvedReviewRoles, ResolvedSeatStaffing};
use darkmux_lab::lab::review::{
    build_review_graph, fingerprint, run_review_graph, seat_identifier, staffing_snapshot, BundleBuildSpec,
    BundleInput, BundleSourceSpec, ChatCall, ExecMode, NullEmitter, ReviewStepContext,
};
use darkmux_crew::single_shot::SingleShotReply;
use darkmux_types::ProfileModel;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// Scopes `DARKMUX_HOME` for one test and restores the prior value —
/// byte-identical to `crates/darkmux-lab/src/crawl/unit_step_tests.rs`'s
/// own `HomeGuard`. `DARKMUX_HOME` is a process global, so every test in
/// this file that constructs one is `#[serial_test::serial]`.
struct HomeGuard(Option<String>);
impl HomeGuard {
    fn set(p: &Path) -> Self {
        let prior = std::env::var("DARKMUX_HOME").ok();
        std::env::set_var("DARKMUX_HOME", p);
        Self(prior)
    }
}
impl Drop for HomeGuard {
    fn drop(&mut self) {
        match &self.0 {
            Some(v) => std::env::set_var("DARKMUX_HOME", v),
            None => std::env::remove_var("DARKMUX_HOME"),
        }
    }
}

// ─── the synthetic diff + planted defects ──────────────────────────────
//
// Three files, two REAL planted defects, one probe false positive:
//   src/billing.ts  — real defect 1: `const end = start.plus(30)` (string
//                      concatenation where numeric addition was intended).
//                      The judge CONFIRMS this one on both passes.
//   src/auth.ts     — real defect 2: `if (user.role = "admin")` (assignment
//                      instead of comparison). The judge's pass-1 confirms
//                      it, but pass-2 disagrees (needs_check) — a demoted
//                      finding, `demoted_by_pass2 == true`.
//   docs/setup.md   — no real defect. The low-recall probe seat flags it
//                      anyway (a false positive); the judge's single pass
//                      rules `false_positive` directly (tier Archived, no
//                      pass-2 dispatched at all — matches production: a
//                      pass-2 only fires when pass-1 is `Confirmed`).
//
// Only the CONFIRMED billing.ts finding reaches the verify seat, which
// rules it `verified` (env.verified == 1, tier stays Confirmed).
const DIFF: &str = include_str!("fixtures/review-conformance/diff.patch");

fn billing_bundle() -> BundleInput {
    BundleInput {
        id: "billing@src/billing.ts".to_string(),
        fact_family: "billing".to_string(),
        code: "// src/billing.ts (lines 1-4)\nfunction computeTotal(start) {\n  const end = start.plus(30)\n  return end\n}".to_string(),
        probe_code: "### `src/billing.ts` (lines 1-4)\n```typescript\nfunction computeTotal(start) {\n  const end = start.plus(30)\n  return end\n}\n```".to_string(),
        facts: vec!["billing.ts: computeTotal".to_string()],
        manifest: vec![],
    }
}

fn auth_bundle() -> BundleInput {
    BundleInput {
        id: "auth@src/auth.ts".to_string(),
        fact_family: "auth".to_string(),
        code: "// src/auth.ts (lines 1-4)\nfunction checkAccess(user) {\n  if (user.role = \"admin\") {\n    return true\n  }\n}".to_string(),
        probe_code: "### `src/auth.ts` (lines 1-4)\n```typescript\nfunction checkAccess(user) {\n  if (user.role = \"admin\") {\n    return true\n  }\n}\n```".to_string(),
        facts: vec!["auth.ts: checkAccess".to_string()],
        manifest: vec![],
    }
}

fn docs_bundle() -> BundleInput {
    BundleInput {
        id: "docs@docs/setup.md".to_string(),
        fact_family: "other".to_string(),
        code: "// docs/setup.md (lines 1-3)\n# Setup\nA new heading appears here\nmore docs".to_string(),
        probe_code: "### `docs/setup.md` (lines 1-3)\n```markdown\n# Setup\nA new heading appears here\nmore docs\n```".to_string(),
        facts: vec![],
        manifest: vec![],
    }
}

/// The three staffed seats — role ids match `review.json`'s three declared
/// probe tasks exactly (`review-probe-high`/`-mid`/`-low`), so
/// `build_review_graph`'s claim/prune boundary claims all three (no
/// pruning warning). `graph_pm`-style `ProfileModel` (id only, no
/// `n_ctx`, no `endpoint` — see the module doc's hermeticity note).
fn pm(id: &str) -> ProfileModel {
    ProfileModel { id: id.to_string(), ..Default::default() }
}

fn seat(role_id: &str, model_id: &str, passes: u32) -> ResolvedSeatStaffing {
    ResolvedSeatStaffing {
        name: "review-conformance".to_string(),
        role_id: Some(role_id.to_string()),
        pm: pm(model_id),
        k: 1,
        passes,
        max_tokens: None,
        selector: None,
        provenance: None,
    }
}

fn crew() -> ResolvedReviewRoles {
    ResolvedReviewRoles {
        probes: vec![
            seat("review-probe-high", "review-conformance-probe-high", 2),
            seat("review-probe-mid", "review-conformance-probe-mid", 2),
            seat("review-probe-low", "review-conformance-probe-low", 2),
        ],
        judge: seat("review-judge", "review-conformance-judge", 2),
        verify: Some(seat("review-verify", "review-conformance-verify", 2)),
        request_changes: false,
        warnings: vec![],
    }
}

fn reply(content: &str) -> SingleShotReply {
    SingleShotReply { content: content.to_string(), total_tokens: Some(1), prompt_tokens: Some(1), completion_tokens: Some(1), model: None }
}

const CONFIRM_JSON: &str =
    "```json\n{\"ruling\": \"confirmed\", \"decisive_evidence\": \"the value is treated as a string\", \"note_for_author\": \"looks like a numeric-add bug\"}\n```";
const NEEDS_CHECK_JSON: &str =
    "```json\n{\"ruling\": \"needs_check\", \"decisive_evidence\": \"could not confirm on recheck\", \"note_for_author\": \"worth a second look\"}\n```";
const FALSE_POSITIVE_JSON: &str =
    "```json\n{\"ruling\": \"false_positive\", \"decisive_evidence\": \"docs have no execution semantics\", \"note_for_author\": \"not a defect\"}\n```";
const VERIFIED_JSON: &str =
    "```json\n{\"ruling\": \"verified\", \"decisive_evidence\": \"confirmed against the diff\", \"note_for_author\": \"holds up\"}\n```";

/// The canned dispatch — installed via `ReviewStepContext::chat_override`
/// (#1355's seam; the SAME injection discipline `review_tests.rs`'s
/// `step_ctx_with_chat` uses). Seat discrimination:
///
/// - **verify vs judge vs probe**: by `call.system` — this harness sets
///   `ctx.judge_system`/`ctx.verify_system` to distinct, self-identifying
///   strings ("... JUDGE seat." / "... VERIFY seat.") that never appear in
///   `ctx.probe_system`, mirroring `review_tests.rs`'s own
///   `call.system.contains("judge"/"verify")` convention
///   (`graph_served_model_captured_distinct_from_requested_on_probe_judge_
///   and_verify`).
/// - **which probe seat, which bundle**: `call.model` (each seat's
///   `ProfileModel.id` is distinct) crossed with a substring unique to
///   that bundle's `probe_code` (the file path) present in `call.user`
///   (`probe_user_message` embeds `bundle.probe_code` verbatim). Every
///   OTHER (seat, bundle) pairing — 6 of the 9 total probe calls, since 3
///   seats each see all 3 bundles with no `selector` — returns an EMPTY
///   reply, which `reconstruct_probe_stage` treats as "this seat drew
///   nothing on this bundle" (no flag), not an error.
/// - **judge pass 1 vs pass 2**: judge's `ChatCall` carries no explicit
///   pass number (`judge_pass_with_retry` dispatches both passes with the
///   IDENTICAL model/system/user), so this mirrors
///   `review_tests.rs::graph_remote_judge_dispatch_error_on_minority_of_
///   flags_does_not_degrade_the_run`'s technique exactly: an `AtomicU32`
///   counts judge calls, and dispatch order is byte-identical to the
///   historical sequential loop when `judge_concurrency: 1` (passed to
///   `build_review_graph` below) — flag-major, `f.pass1` before `f.pass2`,
///   deduped-flag order (which is raw-flag order here, since none of the
///   three flags share a `bundle_id` and so none of them collapse):
///   billing(pass1, pass2) — both `confirmed` — then auth(pass1, pass2) —
///   `confirmed` then `needs_check`, demoting it — then docs(pass1 only,
///   since pass-1 `false_positive` never dispatches a pass-2) —
///   `false_positive`. 5 calls total, indices 0..=4.
fn chat_fn(judge_call: Arc<AtomicU32>) -> impl Fn(&ChatCall) -> Result<SingleShotReply> + Send + Sync + 'static {
    move |call: &ChatCall| {
        if call.system.contains("VERIFY seat") {
            return Ok(reply(VERIFIED_JSON));
        }
        if call.system.contains("JUDGE seat") {
            let idx = judge_call.fetch_add(1, Ordering::SeqCst);
            let content = match idx {
                0 | 1 => CONFIRM_JSON,      // billing: pass1 confirmed, pass2 confirmed
                2 => CONFIRM_JSON,          // auth: pass1 confirmed
                3 => NEEDS_CHECK_JSON,      // auth: pass2 disagrees -> demoted to needs_check
                4 => FALSE_POSITIVE_JSON,   // docs: pass1 false_positive, no pass2
                other => panic!("review-conformance: unexpected judge call index {other}"),
            };
            return Ok(reply(content));
        }
        // Probe call. Discriminate by seat (call.model) x bundle (a
        // substring of call.user unique to that bundle's probe_code).
        if call.model.contains("probe-high") && call.user.contains("src/billing.ts") {
            return Ok(reply("a real defect: `const end = start.plus(30)`"));
        }
        if call.model.contains("probe-mid") && call.user.contains("src/auth.ts") {
            return Ok(reply("a real defect: `if (user.role = \"admin\")`"));
        }
        if call.model.contains("probe-low") && call.user.contains("docs/setup.md") {
            return Ok(reply("a suspicious change: `A new heading appears here`"));
        }
        // Every other (seat, bundle) pairing draws nothing.
        Ok(reply(""))
    }
}

fn step_ctx(judge_call: Arc<AtomicU32>) -> Arc<ReviewStepContext> {
    Arc::new(ReviewStepContext {
        case_id: "review-conformance-case".to_string(),
        roles: crew(),
        intent_title: "Fix billing rounding".to_string(),
        intent_body: "Cleans up a couple of billing/auth edge cases.".to_string(),
        diff: DIFF.to_string(),
        probe_system: "You are the review-conformance PROBE seat.".to_string(),
        probe_role_prompts: std::collections::BTreeMap::new(),
        judge_system: "You are the review-conformance JUDGE seat.".to_string(),
        verify_system: "You are the review-conformance VERIFY seat.".to_string(),
        remote_max_tokens_per_execution: 500_000,
        judge_exhaustion_strict: false,
        timeout_seconds: 30,
        chat_override: Some(Arc::new(chat_fn(judge_call))),
        bundle_override: Some(Arc::new(|| Ok(vec![billing_bundle(), auth_bundle(), docs_bundle()]))),
        mission_id: None,
    })
}

fn dummy_bundle_spec() -> BundleBuildSpec {
    BundleBuildSpec {
        source: BundleSourceSpec::Worktree { path: PathBuf::from("/nonexistent-review-conformance-worktree") },
        bundler: None,
        diff_file: PathBuf::from("/nonexistent-review-conformance-diff"),
    }
}

/// Every field this test zeroes before comparing against the committed
/// golden — wall-clock/`Instant`-derived, never reproducible byte-for-byte
/// between runs:
///   - `members[].wall_ms`   (`MemberRecord`, per-seat wall time)
///   - `judged[].pass1.seconds` / `.pass2.seconds` (`JudgeRecord`)
///   - `judged[].verify.seconds` (`VerifyRecord`)
///
/// `case_id`/`crew`/`fingerprint` etc. are already fully deterministic
/// (constructed from fixed strings above), so nothing else needs
/// normalizing. Also re-serializes with SORTED object keys (`BTreeMap`
/// round-trip) for a canonical, diff-stable golden — `ReviewEnvelope`'s own
/// `Serialize` impl uses declaration order, which is stable across runs but
/// not alphabetical, so this makes the committed file's ordering
/// independent of field-declaration order too.
fn canonicalize(mut v: serde_json::Value) -> serde_json::Value {
    if let Some(members) = v.get_mut("members").and_then(|m| m.as_array_mut()) {
        for m in members {
            if let Some(obj) = m.as_object_mut() {
                obj.insert("wall_ms".to_string(), serde_json::json!(0));
            }
        }
    }
    if let Some(judged) = v.get_mut("judged").and_then(|j| j.as_array_mut()) {
        for flag in judged {
            for pass_key in ["pass1", "pass2"] {
                if let Some(pass) = flag.get_mut(pass_key).and_then(|p| p.as_object_mut()) {
                    pass.insert("seconds".to_string(), serde_json::json!(0.0));
                }
            }
            if let Some(verify) = flag.get_mut("verify").and_then(|v| v.as_object_mut()) {
                verify.insert("seconds".to_string(), serde_json::json!(0.0));
            }
        }
    }
    sort_keys(v)
}

fn sort_keys(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => {
            let sorted: std::collections::BTreeMap<String, serde_json::Value> =
                map.into_iter().map(|(k, val)| (k, sort_keys(val))).collect();
            serde_json::to_value(sorted).expect("BTreeMap<String, Value> always serializes")
        }
        serde_json::Value::Array(arr) => serde_json::Value::Array(arr.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/review-conformance/envelope.json")
}

/// The conformance harness itself: build the graph from the SHIPPED
/// `review.json` via `build_review_graph` (never a hand-built document —
/// this is the same call `src/mission_launch_review.rs` makes), run it
/// through `run_review_graph` with the canned dispatch installed, and pin
/// the resulting `ReviewEnvelope` byte-for-byte (after normalizing the
/// volatile timing fields named on `canonicalize`'s own doc) against the
/// committed golden.
#[test]
#[serial_test::serial]
fn review_pipeline_matches_the_committed_golden() {
    let home = tempfile::tempdir().expect("tempdir");
    let _guard = HomeGuard::set(home.path());

    let judge_call = Arc::new(AtomicU32::new(0));
    let ctx = step_ctx(judge_call);
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
        // judge_concurrency: 1 — byte-identical dispatch ORDER to the
        // historical sequential judge loop (see `chat_fn`'s own doc for
        // why this harness's call-index discrimination depends on it).
        1,
    )
    .expect("the shipped review.json builds cleanly");

    let (env, steps) = run_review_graph(
        &ctx,
        &crew_name,
        ExecMode::Sequential,
        fingerprint_val,
        staffing_snap,
        graph,
        &mut NullEmitter,
        &mut |_step| {},
    )
    .expect("review-conformance graph run completes");

    // Sanity checks BEFORE the golden compare — these fail loud (with a
    // legible message) if the fixture stops exercising what it claims to,
    // rather than surfacing as an opaque JSON diff.
    assert_eq!(env.bundles, 3, "three synthetic bundles");
    assert_eq!(env.raw_flags, 3, "one flag per seat: billing, auth, docs false-positive");
    assert_eq!(env.deduped_flags, 3, "none collapse -- distinct bundle_ids");
    assert_eq!(env.confirmed, 1, "only billing.ts survives both judge passes");
    assert_eq!(env.needs_check, 1, "auth.ts demoted by pass-2 disagreement");
    assert_eq!(env.archived, 1, "docs/setup.md ruled false_positive on pass-1");
    assert_eq!(env.verified, 1, "the one confirmed finding reaches verify and is verified");
    assert_eq!(env.refuted, 0);
    assert!(env.degenerate.is_none(), "a ruled-on docket must never read as degenerate: {:?}", env.degenerate);
    let demoted = env.judged.iter().find(|j| j.flag.bundle_id == "auth@src/auth.ts").expect("auth flag present");
    assert!(demoted.demoted_by_pass2, "auth.ts must be recorded as demoted by pass-2");
    let confirmed = env.judged.iter().find(|j| j.flag.bundle_id == "billing@src/billing.ts").expect("billing flag present");
    assert_eq!(confirmed.flag.anchor.as_deref(), Some("const end = start.plus(30)"), "anchor must resolve against the diff");
    for step in steps.values() {
        assert!(
            matches!(step.status, darkmux_crew::types::NodeStatus::Complete | darkmux_crew::types::NodeStatus::Error),
            "step `{}` must reach a terminal status, got {:?}",
            step.id,
            step.status
        );
    }

    let actual = canonicalize(serde_json::to_value(&env).expect("ReviewEnvelope serializes"));
    let pretty = serde_json::to_string_pretty(&actual).expect("pretty-print");

    if std::env::var("DARKMUX_REVIEW_CONFORMANCE_UPDATE_GOLDEN").is_ok() {
        std::fs::write(golden_path(), format!("{pretty}\n")).expect("write golden");
        return;
    }

    let expected = std::fs::read_to_string(golden_path()).unwrap_or_else(|_| {
        panic!(
            "missing golden at {} — run with DARKMUX_REVIEW_CONFORMANCE_UPDATE_GOLDEN=1 to generate it",
            golden_path().display()
        )
    });
    assert_eq!(
        pretty.trim_end(),
        expected.trim_end(),
        "the review pipeline's envelope drifted from the committed conformance golden at {}.\n\
         If this drift is an intended, reviewed behavior change (this is the #2310 refactor's \
         regression net — a drift here is exactly what it exists to catch), regenerate with:\n\
         DARKMUX_REVIEW_CONFORMANCE_UPDATE_GOLDEN=1 cargo test -p darkmux-lab --test review_conformance\n\
         then review the diff before committing.",
        golden_path().display()
    );
}
