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
    build_review_graph, fingerprint, render_and_emit_review, run_review_graph, seat_identifier, staffing_snapshot,
    BundleBuildSpec, BundleInput, BundleSourceSpec, ChatCall, ExecMode, NullEmitter, ReviewContextStepKind,
    ReviewContextTestOverrides, ReviewStepContext,
};
use darkmux_crew::single_shot::SingleShotReply;
use darkmux_crew::step_kinds::StepKind as _;
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
// Four files, three REAL planted defects, one probe false positive:
//   src/billing.ts  — real defect 1: `const end = start.plus(30)` (string
//                      concatenation where numeric addition was intended).
//                      The judge CONFIRMS this one on both passes, and the
//                      SAME charge (same anchor + a shared `plus` symbol) is
//                      ALSO independently flagged by a second probe seat
//                      (Hole 2, #2336 review) — a real dedup collapse:
//                      `raw_flags == 5`, `deduped_flags == 4`, and the
//                      SURVIVING flag's `member` attribution stays the
//                      first-seen seat's (`review-conformance-probe-high`).
//   src/auth.ts     — real defect 2: `if (user.role = "admin")` (assignment
//                      instead of comparison). The judge's pass-1 confirms
//                      it, but pass-2 disagrees (needs_check) — a demoted
//                      finding, `demoted_by_pass2 == true`.
//   docs/setup.md   — no real defect. The low-recall probe seat flags it
//                      anyway (a false positive); the judge's single pass
//                      rules `false_positive` directly (tier Archived, no
//                      pass-2 dispatched at all — matches production: a
//                      pass-2 only fires when pass-1 is `Confirmed`).
//   src/config.ts   — real defect 3 (Hole 3, #2336 review): `const port =
//                      env.PORT + 1` (the SAME string-concatenation-vs-
//                      numeric-add shape as billing.ts, on a different
//                      symbol). The judge CONFIRMS this one on both passes
//                      too — a SECOND confirmed finding — but the verify
//                      seat REFUTES it (`env.refuted == 1`,
//                      `demoted_by_verify == true`, tier demotes back to
//                      Archived). This is also the run's one
//                      endpoint-bearing seat (the verify seat itself —
//                      `crew()`'s `verify_seat` below), the hermetic
//                      "remote" trick `review_tests.rs`'s own
//                      `remote_pm`/`remote_staffing` use: an endpoint with
//                      no `n_ctx` reports `Residency::Remote`, but
//                      `chat_override` still intercepts the call, so
//                      `MemberRecord::remote == true` and a `remote_budgets`
//                      row appear WITHOUT ever touching a real network.
//
// Two CONFIRMED findings reach the verify seat: billing.ts is ruled
// `verified` (tier stays Confirmed) and config.ts is ruled `refuted` (tier
// demotes to Archived) — `env.verified == 1`, `env.refuted == 1`.
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

/// (Hole 3, #2336 review) The second confirmed-then-refuted finding — see
/// this file's module doc for the full shape.
fn config_bundle() -> BundleInput {
    BundleInput {
        id: "config@src/config.ts".to_string(),
        fact_family: "config".to_string(),
        code: "// src/config.ts (lines 1-4)\nfunction loadPort(env) {\n  const port = env.PORT + 1\n  return port\n}".to_string(),
        probe_code: "### `src/config.ts` (lines 1-4)\n```typescript\nfunction loadPort(env) {\n  const port = env.PORT + 1\n  return port\n}\n```".to_string(),
        facts: vec!["config.ts: loadPort".to_string()],
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

/// (Docs, #2336 review) Shared by probe/judge seats via `crew()` below.
/// `passes` is meaningful ONLY for the judge seat — `ResolvedSeatStaffing
/// .passes` is read exactly twice in `review.rs` (the judge's own
/// double-confirm dispatch and the judge step's config stamp) and never
/// once for a probe seat. `crew()` still passes `2` for every probe seat
/// below (matching production's own `review.json`-resolved staffing, which
/// carries the SAME field on every seat regardless of role — the type
/// doesn't distinguish "probe" from "judge" staffing), so this harness
/// keeps it rather than diverging from a real resolved-staffing shape; it
/// is simply inert for the seats that carry it here.
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

/// (Hole 3, #2336 review) The one endpoint-bearing seat — byte-identical
/// shape to `review_tests.rs`'s own `remote_pm`: no `n_ctx` (endpoint models
/// have no local context, #1282), an `endpoint` present. The verify seat is
/// the natural pick — it's the ONLY seat production ever treats as
/// optional, so making just this one remote costs nothing structurally.
/// `chat_override` still intercepts every call regardless of local/remote
/// (see `review_dispatch_override`'s own doc), so this is exactly as
/// hermetic as the rest of the harness — it never reaches a real endpoint.
fn remote_pm(id: &str) -> ProfileModel {
    ProfileModel {
        id: id.to_string(),
        endpoint: Some(darkmux_types::ModelEndpoint {
            url: Some("https://review-conformance.example.invalid/openai/deployments/verify".to_string()),
            api_version: Some("2025-01-01-preview".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn remote_seat(role_id: &str, model_id: &str, passes: u32) -> ResolvedSeatStaffing {
    ResolvedSeatStaffing {
        name: "review-conformance".to_string(),
        role_id: Some(role_id.to_string()),
        pm: remote_pm(model_id),
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
        verify: Some(remote_seat("review-verify", "review-conformance-verify", 2)),
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
const REFUTED_JSON: &str =
    "```json\n{\"ruling\": \"refuted\", \"decisive_evidence\": \"the increment is applied to a numeric env value\", \"note_for_author\": \"does not reproduce\"}\n```";

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
///   OTHER (seat, bundle) pairing returns an EMPTY reply, which
///   `reconstruct_probe_stage` treats as "this seat drew nothing on this
///   bundle" (no flag), not an error. (Hole 2, #2336 review) `probe-mid`
///   ALSO flags `src/billing.ts` — the SAME charge text `probe-high` flags
///   it with, so the SAME anchor + a shared `plus` symbol on the SAME
///   bundle collapses the two under `dedup_flags`'s `MechanismFamilyDedup`
///   (review.rs:1558): `raw_flags == 5`, `deduped_flags == 4`.
/// - **judge pass 1 vs pass 2, and which bundle**: judge's `ChatCall`
///   carries no explicit pass number (`judge_pass_with_retry` dispatches
///   both passes with the IDENTICAL model/system/user), so this
///   discriminates by BUNDLE first — `judge_prompt`'s `code` parameter
///   (review.rs:1673) embeds the bundle's `// <path> (lines ...)` comment
///   verbatim into `call.user`, so a substring match on the file path picks
///   out the bundle regardless of dispatch order — then by an independent
///   per-bundle `AtomicU32` for pass 1 vs pass 2 (rather than one shared
///   global counter across every bundle, which would silently depend on
///   cross-bundle dispatch order the moment a fourth bundle was added; see
///   Hole 3's own history in the #2336 review for exactly that fragility).
fn chat_fn(
    billing_pass: Arc<AtomicU32>,
    auth_pass: Arc<AtomicU32>,
    config_pass: Arc<AtomicU32>,
    total_calls: Arc<AtomicU32>,
) -> impl Fn(&ChatCall) -> Result<SingleShotReply> + Send + Sync + 'static {
    move |call: &ChatCall| {
        total_calls.fetch_add(1, Ordering::SeqCst);
        if call.system.contains("VERIFY seat") {
            if call.user.contains("src/billing.ts") {
                return Ok(reply(VERIFIED_JSON));
            }
            if call.user.contains("src/config.ts") {
                return Ok(reply(REFUTED_JSON));
            }
            panic!("review-conformance: unexpected VERIFY call: {}", call.user);
        }
        if call.system.contains("JUDGE seat") {
            if call.user.contains("src/billing.ts") {
                let idx = billing_pass.fetch_add(1, Ordering::SeqCst);
                // billing: pass1 confirmed, pass2 confirmed. Any FURTHER pass
                // only exists when dedup failed to collapse the second billing
                // flag — answer it too, so that regression is reported by the
                // collapse assertion (raw 5 / deduped 4), not by a panic here.
                let _ = idx;
                return Ok(reply(CONFIRM_JSON));
            }
            if call.user.contains("src/config.ts") {
                let idx = config_pass.fetch_add(1, Ordering::SeqCst);
                return Ok(reply(match idx {
                    0 | 1 => CONFIRM_JSON, // config: pass1 confirmed, pass2 confirmed
                    other => panic!("review-conformance: unexpected config judge pass {other}"),
                }));
            }
            if call.user.contains("docs/setup.md") {
                return Ok(reply(FALSE_POSITIVE_JSON)); // docs: pass1 false_positive, no pass2
            }
            if call.user.contains("src/auth.ts") {
                let idx = auth_pass.fetch_add(1, Ordering::SeqCst);
                return Ok(reply(match idx {
                    0 => CONFIRM_JSON,        // auth: pass1 confirmed
                    1 => NEEDS_CHECK_JSON,    // auth: pass2 disagrees -> demoted to needs_check
                    other => panic!("review-conformance: unexpected auth judge pass {other}"),
                }));
            }
            panic!("review-conformance: unexpected JUDGE call: {}", call.user);
        }
        // Probe call. Discriminate by seat (call.model) x bundle (a
        // substring of call.user unique to that bundle's probe_code).
        if call.model.contains("probe-high") && call.user.contains("src/billing.ts") {
            return Ok(reply("a real defect: `const end = start.plus(30)`"));
        }
        // (Hole 2, #2336 review) A SECOND probe seat flags the SAME billing
        // hunk with the SAME charge text — same anchor, overlapping `plus`
        // symbol, same bundle_id — so `dedup_flags` collapses it into the
        // `probe-high` survivor above.
        if call.model.contains("probe-mid") && call.user.contains("src/billing.ts") {
            return Ok(reply("a real defect: `const end = start.plus(30)`"));
        }
        if call.model.contains("probe-mid") && call.user.contains("src/auth.ts") {
            return Ok(reply("a real defect: `if (user.role = \"admin\")`"));
        }
        if call.model.contains("probe-low") && call.user.contains("docs/setup.md") {
            return Ok(reply("a suspicious change: `A new heading appears here`"));
        }
        // (Hole 3, #2336 review) The second confirmed-then-refuted finding.
        if call.model.contains("probe-low") && call.user.contains("src/config.ts") {
            return Ok(reply("a real defect: `const port = env.PORT + 1`"));
        }
        // Every other (seat, bundle) pairing draws nothing.
        Ok(reply(""))
    }
}

fn step_ctx(
    billing_pass: Arc<AtomicU32>,
    auth_pass: Arc<AtomicU32>,
    config_pass: Arc<AtomicU32>,
    total_calls: Arc<AtomicU32>,
) -> Arc<ReviewStepContext> {
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
        chat_override: Some(Arc::new(chat_fn(billing_pass, auth_pass, config_pass, total_calls))),
        bundle_override: Some(Arc::new(|| {
            Ok(vec![billing_bundle(), auth_bundle(), docs_bundle(), config_bundle()])
        })),
        mission_id: None,
        crew_name: None,
        mode_label: None,
        fingerprint: None,
        staffing: None,
        interpret_warnings: Vec::new(),
        // (#2310 P1 fix) `review-context-step` now reads its test overrides
        // off this bus seam, not step config — mirror the three fixture
        // prompts + two budget knobs above so a full `run_review_graph` run
        // resolves the SAME text `chat_fn` discriminates on.
        diff_file: None,
        intent_file: None,
        context_test_overrides: ReviewContextTestOverrides {
            probe_system: Some("You are the review-conformance PROBE seat.".to_string()),
            judge_system: Some("You are the review-conformance JUDGE seat.".to_string()),
            verify_system: Some("You are the review-conformance VERIFY seat.".to_string()),
            remote_max_tokens_per_execution: Some(500_000),
            judge_exhaustion_strict: Some(false),
        },
    })
}

// ─── Hole 1 (#2336 review): residency hints are invisible on hermetic seats ─
//
// Every seat in `crew()` above is `n_ctx: None` (the harness's own
// hermeticity trick — see the module doc), so `judge.pm.n_ctx` and its
// probe/verify siblings are never actually READ by `build_review_graph_
// from_config`'s residency-stamping — the reviewer proved this by replacing
// the judge stamp with `let _ = judge.pm.n_ctx;` at review.rs:6967 and
// watching the suite stay green. A `ResolvedSeatStaffing` WITH a real
// `n_ctx` (and a `model_key`) is what makes that stamp observable: this
// builds a SECOND graph (never run — `run_review_graph` hardcodes the real
// `lms_host_factory`, so running an n_ctx-bearing local seat would try to
// touch a real host) with such seats, and pins the five dispatching steps'
// stamped `config` objects as their own golden. `residency()`
// (`ReviewJudgeStepKind::residency`, review.rs:5653, and the generic
// `dispatch.map` builtin's own residency read for probe/verify) reads
// exactly `model_key`/`identifier`/`n_ctx` off these configs — this golden
// is the byte-level proof those three keys are actually stamped from the
// staffing, not merely declared as struct fields nobody reads.
fn pm_with_ctx(id: &str, n_ctx: u32) -> ProfileModel {
    ProfileModel { id: id.to_string(), n_ctx: Some(n_ctx), ..Default::default() }
}

fn seat_with_ctx(role_id: &str, model_id: &str, n_ctx: u32, passes: u32) -> ResolvedSeatStaffing {
    ResolvedSeatStaffing {
        name: "review-conformance-residency".to_string(),
        role_id: Some(role_id.to_string()),
        pm: pm_with_ctx(model_id, n_ctx),
        k: 1,
        passes,
        max_tokens: None,
        selector: None,
        provenance: None,
    }
}

fn graph_config_golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/review-conformance/graph-config.json")
}

/// Builds (never runs — see this section's own doc) a graph from
/// n_ctx-bearing local seats and pins the five dispatching steps' stamped
/// `config` onto a golden. `DARKMUX_HOME` still needs scoping: `interpret`
/// walks through `mission_config::load("review")`, same as the main
/// scenario.
#[test]
#[serial_test::serial]
fn graph_config_stamps_residency_hints_from_n_ctx_bearing_seats() {
    let home = tempfile::tempdir().expect("tempdir");
    let _guard = HomeGuard::set(home.path());

    let ctx = step_ctx(
        Arc::new(AtomicU32::new(0)),
        Arc::new(AtomicU32::new(0)),
        Arc::new(AtomicU32::new(0)),
        Arc::new(AtomicU32::new(0)),
    );

    let judge = seat_with_ctx("review-judge", "review-conformance-judge", 32_768, 2);
    let verify = seat_with_ctx("review-verify", "review-conformance-verify", 16_384, 2);
    let probes = vec![
        seat_with_ctx("review-probe-high", "review-conformance-probe-high", 65_536, 2),
        seat_with_ctx("review-probe-mid", "review-conformance-probe-mid", 24_576, 2),
        seat_with_ctx("review-probe-low", "review-conformance-probe-low", 8_192, 2),
    ];

    let graph = build_review_graph(
        ctx,
        &dummy_bundle_spec(),
        judge,
        Some(verify),
        &probes,
        "investigate",
        "adjudicate",
        "report",
        1,
    )
    .expect("the shipped review.json builds cleanly with n_ctx-bearing seats");

    let step_ids = [
        "review-probe-high-step",
        "review-probe-mid-step",
        "review-probe-low-step",
        "review-judge-step",
        "review-verify-step",
    ];
    let mut configs = std::collections::BTreeMap::new();
    for id in step_ids {
        let step = graph.steps.get(id).unwrap_or_else(|| panic!("graph must have step `{id}`"));
        configs.insert(id.to_string(), step.config.clone());
    }
    let actual = canonicalize_graph_config(serde_json::to_value(&configs).expect("configs serialize"));

    // Sanity, before the golden compare: every one of the five configs must
    // actually carry the three residency-hint keys `residency()` reads.
    for id in step_ids {
        let cfg = &configs[id];
        assert!(cfg.get("model_key").is_some(), "step `{id}` config missing model_key: {cfg}");
        assert!(cfg.get("identifier").is_some(), "step `{id}` config missing identifier: {cfg}");
        assert!(cfg.get("n_ctx").is_some(), "step `{id}` config missing n_ctx: {cfg}");
    }

    let pretty = serde_json::to_string_pretty(&actual).expect("pretty-print");
    if std::env::var("DARKMUX_REVIEW_CONFORMANCE_UPDATE_GOLDEN").is_ok() {
        std::fs::write(graph_config_golden_path(), format!("{pretty}\n")).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(graph_config_golden_path()).unwrap_or_else(|_| {
        panic!(
            "missing golden at {} — run with DARKMUX_REVIEW_CONFORMANCE_UPDATE_GOLDEN=1 to generate it",
            graph_config_golden_path().display()
        )
    });
    assert_eq!(
        pretty.trim_end(),
        expected.trim_end(),
        "residency-hint stamping onto the probe/judge/verify step configs drifted from the \
         committed golden at {} — this is Hole 1's regression net (#2336 review): the \
         `model_key`/`identifier`/`n_ctx` keys `residency()` reads must actually be stamped from \
         the seat staffing.",
        graph_config_golden_path().display()
    );
}

/// [`canonicalize`]'s sibling for the graph-config golden — no `wall_ms`/
/// `seconds` fields exist here (nothing was ever run), so this is just the
/// sorted-keys canonicalization.
fn canonicalize_graph_config(v: serde_json::Value) -> serde_json::Value {
    sort_keys(v)
}

fn dummy_bundle_spec() -> BundleBuildSpec {
    BundleBuildSpec {
        source: BundleSourceSpec::Worktree { path: PathBuf::from("/nonexistent-review-conformance-worktree") },
        bundler: None,
        diff_file: PathBuf::from("/nonexistent-review-conformance-diff"),
    }
}

/// (#2345 minor) `review-report-step`'s config stays `null` (unstamped) in
/// every hermetic harness fixture in this file — a real `mission launch
/// review` always stamps `emit` (`src/mission_launch_review.rs::run_
/// dispatch`), but this harness never mints a launch, so nothing stamps it
/// here. `null` config means `emit_path: None`, which `emit_rendered`'s own
/// contract reads as "write to stdout" — so any scenario whose graph
/// reaches a COMPLETED `review-report-step` (the two golden-comparison
/// scenarios and the real-bundler scenario; the errored scenario's report
/// step never runs at all) prints the whole rendered `{mode, review,
/// comment}` payload to the TEST process's own stdout on every `cargo
/// test` run. Call this right alongside the `context_out` stamp each of
/// those scenarios already applies, so the report step's rendered output
/// goes to a tempdir file instead — same "give it a real destination"
/// discipline, one more step.
fn stamp_report_emit(graph: &mut darkmux_lab::lab::review::BuiltReviewGraph, home: &Path) {
    if let Some(report_step) = graph.steps.get_mut("review-report-step") {
        report_step.config = serde_json::json!({
            "emit": home.join("rendered-report.json").display().to_string()
        });
    }
}

// ─── Hole 4 (#2336 review): the bundle step's real path is out of the net ──
//
// Every OTHER test in this file sets `bundle_override: Some(...)`, which
// skips `ReviewBundleStepKind::run_streaming`'s `else` branch entirely
// (`file_source_from_step_config`, `build_bundles`, `bundle_skip`,
// `bundle_inputs_from_set` — review.rs:4442) — exactly the branch
// PRODUCTION takes on every real `mission launch review`. This scenario is
// the one place in the harness that leaves `bundle_override: None` and
// hands the graph a REAL worktree + a REAL diff file, so the actual bundler
// runs. `chat_override` stays installed (every probe draw comes back
// empty), so this stays exactly as hermetic as the rest of the harness —
// zero findings means the judge/verify stages never dispatch at all, and
// the run still completes as a clean, ruled-on (non-degenerate) docket with
// `bundles > 0` (never the "no bundles produced" degenerate gate).
const BUNDLE_STEP_BILLING_NEW: &str = "function computeTotal(start) {\n  const end = start.plus(30)\n  return end\n}\n\nfunction applyDiscount(total, pct) {\n  const rate = pct / 100\n  return total - rate\n}\n";
const BUNDLE_STEP_AUTH_NEW: &str =
    "function checkAccess(user) {\n  if (user.role = \"admin\") {\n    return true\n  }\n}\n";
const BUNDLE_STEP_LOCKFILE_NEW: &str = "{\"lockfileVersion\": 3}\n";

/// The diff driving [`review_bundle_step_runs_the_real_bundler_over_a_worktree`].
/// `src/billing.ts` carries TWO separate `@@` hunks (the first with both a
/// removed and an added line, the second a pure line replacement) so this
/// exercises multi-hunk splitting within one file, not just a single-hunk
/// happy path. `package-lock.json` is the excluded-non-code file — its
/// extension alone (not its content) is what `SkipReason::NonCodeExtension`
/// keys on.
const BUNDLE_STEP_DIFF: &str = "diff --git a/src/billing.ts b/src/billing.ts\n\
--- a/src/billing.ts\n\
+++ b/src/billing.ts\n\
@@ -1,3 +1,4 @@\n\
 function computeTotal(start) {\n\
+  const end = start.plus(30)\n\
-  return start\n\
+  return end\n\
 }\n\
@@ -6,1 +7,1 @@\n\
-  const rate = pct\n\
+  const rate = pct / 100\n\
diff --git a/src/auth.ts b/src/auth.ts\n\
--- a/src/auth.ts\n\
+++ b/src/auth.ts\n\
@@ -1,3 +1,5 @@\n\
 function checkAccess(user) {\n\
+  if (user.role = \"admin\") {\n\
+    return true\n\
+  }\n\
-  return true\n\
 }\n\
diff --git a/package-lock.json b/package-lock.json\n\
--- a/package-lock.json\n\
+++ b/package-lock.json\n\
@@ -1,1 +1,1 @@\n\
-{\"lockfileVersion\": 2}\n\
+{\"lockfileVersion\": 3}\n";

fn bundle_step_golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/review-conformance/bundles.json")
}

/// This test is about the BUNDLE step, not the probe/judge/verify stages —
/// but a docket with ZERO flags reads as degenerate ("zero flags from all
/// probe draws — never a silent pass", `run_review_graph`'s own gate), so
/// this can't just be all-empty. The cheapest non-degenerate shape: exactly
/// ONE probe seat flags ONE bundle, the judge rules `needs_check` on pass 1
/// (which never dispatches a pass 2 — module doc, `chat_fn`'s own note),
/// and no verify call ever fires. Every OTHER (seat, bundle) pairing stays
/// empty.
fn bundle_step_chat_fn(call: &ChatCall) -> Result<SingleShotReply> {
    if call.system.contains("JUDGE seat") {
        return Ok(reply(NEEDS_CHECK_JSON));
    }
    // (billing.ts's real changed lines land in TWO separate function
    // bundles — `computeTotal` and `applyDiscount` — so matching on the
    // bare file path would flag both; narrow to the specific function's
    // own source text to keep this scenario at exactly one flag.)
    if call.model.contains("probe-high") && call.user.contains("computeTotal") {
        return Ok(reply("a real defect: `const end = start.plus(30)`"));
    }
    Ok(reply(""))
}

#[test]
#[serial_test::serial]
fn review_bundle_step_runs_the_real_bundler_over_a_worktree() {
    let home = tempfile::tempdir().expect("tempdir");
    let _guard = HomeGuard::set(home.path());

    let worktree = tempfile::tempdir().expect("worktree tempdir");
    std::fs::create_dir_all(worktree.path().join("src")).expect("mkdir src");
    std::fs::write(worktree.path().join("src/billing.ts"), BUNDLE_STEP_BILLING_NEW).expect("write billing.ts");
    std::fs::write(worktree.path().join("src/auth.ts"), BUNDLE_STEP_AUTH_NEW).expect("write auth.ts");
    std::fs::write(worktree.path().join("package-lock.json"), BUNDLE_STEP_LOCKFILE_NEW).expect("write lockfile");

    let diff_dir = tempfile::tempdir().expect("diff tempdir");
    let diff_file = diff_dir.path().join("pr.diff");
    std::fs::write(&diff_file, BUNDLE_STEP_DIFF).expect("write diff file");

    let ctx = Arc::new(ReviewStepContext {
        case_id: "review-conformance-bundle-case".to_string(),
        roles: crew(),
        intent_title: "Fix billing rounding".to_string(),
        intent_body: "Cleans up a couple of billing/auth edge cases.".to_string(),
        diff: BUNDLE_STEP_DIFF.to_string(),
        probe_system: "You are the review-conformance PROBE seat.".to_string(),
        probe_role_prompts: std::collections::BTreeMap::new(),
        judge_system: "You are the review-conformance JUDGE seat.".to_string(),
        verify_system: "You are the review-conformance VERIFY seat.".to_string(),
        remote_max_tokens_per_execution: 500_000,
        judge_exhaustion_strict: false,
        timeout_seconds: 30,
        chat_override: Some(Arc::new(bundle_step_chat_fn)),
        // Hole 4's whole point: NOT overridden, so `ReviewBundleStepKind::
        // run_streaming`'s real `else` branch runs.
        bundle_override: None,
        mission_id: None,
        crew_name: None,
        mode_label: None,
        fingerprint: None,
        staffing: None,
        interpret_warnings: Vec::new(),
        // (#2310 P1 fix) `review-context-step` now reads its test overrides
        // off this bus seam, not step config — mirror the three fixture
        // prompts above so this hermetic scenario's `chat_fn` (which
        // discriminates by system-prompt/bundle-content substring) still
        // sees the fixture text it was written against.
        diff_file: None,
        intent_file: None,
        context_test_overrides: ReviewContextTestOverrides {
            probe_system: Some("You are the review-conformance PROBE seat.".to_string()),
            judge_system: Some("You are the review-conformance JUDGE seat.".to_string()),
            verify_system: Some("You are the review-conformance VERIFY seat.".to_string()),
            remote_max_tokens_per_execution: Some(500_000),
            judge_exhaustion_strict: Some(false),
        },
    });

    let bundle_spec = BundleBuildSpec {
        source: BundleSourceSpec::Worktree { path: worktree.path().to_path_buf() },
        bundler: None,
        diff_file: diff_file.clone(),
    };

    let judge = ctx.roles.judge.clone();
    let verify = ctx.roles.verify.clone();
    let probes = ctx.roles.probes.clone();
    let fingerprint_val = fingerprint(&seat_identifier(&judge.pm), &ctx.judge_system);
    let staffing_snap = staffing_snapshot(&probes, &judge, verify.as_ref(), ctx.roles.request_changes);
    let crew_name = ctx.roles.distinct_profile_names();

    let mut graph = build_review_graph(
        ctx.clone(),
        &bundle_spec,
        judge,
        verify,
        &probes,
        "investigate",
        "adjudicate",
        "report",
        1,
    )
    .expect("the shipped review.json builds cleanly over a real worktree bundle spec");
    // (#2310 P1) `review.context` resolves its OWN mission dir from the
    // task's phase record (`default_context_path`) — a real
    // `mission launch review` mints one; this hermetic test builds the
    // graph directly with no Mission/Phase ever written to disk, so it
    // uses the same `context_out` escape hatch `crawl.plan`'s tests use
    // (`plan_out`) instead of minting one just to satisfy the lookup.
    {
        let context_step = graph
            .steps
            .get_mut("review-context-step")
            .expect("the shipped review.json declares a review-context-step");
        let mut cfg = context_step.config.clone();
        cfg["context_out"] = serde_json::json!(home.path().join("review-context.json").display().to_string());
        // (#2310 P1 fix) The three fixture prompts + two budget knobs are
        // now carried on `ctx.context_test_overrides` (set at construction
        // above) — the bus seam `run_review_graph` seeds from `ctx`, not a
        // step-config key.
        context_step.config = cfg;
    }
    stamp_report_emit(&mut graph, home.path());

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
    .expect("review-conformance real-bundler graph run completes");

    // Sanity BEFORE the golden compare.
    assert!(env.degenerate.is_none(), "a docket with real bundles must never read as degenerate: {:?}", env.degenerate);
    assert!(env.bundles > 0, "the real bundler must produce at least one bundle from the worktree diff");
    assert_eq!(env.confirmed, 0, "the one flag is ruled needs_check on pass 1, never confirmed");
    assert_eq!(env.needs_check, 1, "the one real probe flag, ruled needs_check directly (no pass 2)");
    let skip = env.bundle_skip.as_ref().expect("the real bundler path must stamp a skip report");
    assert!(
        skip.files_skipped.iter().any(|f| f.path == "package-lock.json"),
        "package-lock.json must be recorded as skipped: {skip:?}"
    );
    assert_eq!(
        skip.files_skipped.iter().find(|f| f.path == "package-lock.json").map(|f| f.reason),
        Some(darkmux_lab::lab::bundle::SkipReason::NonCodeExtension),
        "a lockfile is skipped for its EXTENSION, not its content: {skip:?}"
    );

    let bundle_step = steps.get("review-bundle-step").expect("graph must have review-bundle-step");
    assert_eq!(
        bundle_step.status,
        darkmux_crew::types::NodeStatus::Complete,
        "output: {:?}",
        bundle_step.output
    );
    // (#2310 P2) `review-bundle-step`'s output is now a typed
    // `Output<BundleSetOutput>` envelope — the golden compares the BODY's
    // `bundles` array only (the identical data the pre-P2 bare array
    // held), never the envelope's own producer/hash/timestamp fields
    // (none of which are reproducible byte-for-byte between runs).
    let wrapped: serde_json::Value =
        serde_json::from_str(bundle_step.output.as_deref().expect("review-bundle-step produced output"))
            .expect("review-bundle-step output is valid JSON");
    let raw_output = wrapped["body"]["bundles"].clone();
    let bundle_inputs = raw_output.as_array().expect("review-bundle-step output body carries a JSON array of bundles");
    assert!(!bundle_inputs.is_empty(), "must actually carry bundle content, not just a nonzero count");
    let ids: Vec<&str> = bundle_inputs.iter().filter_map(|b| b["id"].as_str()).collect();
    assert!(
        ids.iter().any(|id| id.contains("src/billing.ts")),
        "billing.ts's real changed function must produce a bundle: {ids:?}"
    );
    assert!(
        ids.iter().any(|id| id.contains("src/auth.ts")),
        "auth.ts's real changed function must produce a bundle: {ids:?}"
    );
    assert!(
        !ids.iter().any(|id| id.contains("package-lock.json")),
        "the skipped lockfile must never surface as a bundle: {ids:?}"
    );

    let actual = sort_keys(raw_output);
    let pretty = serde_json::to_string_pretty(&actual).expect("pretty-print");
    if std::env::var("DARKMUX_REVIEW_CONFORMANCE_UPDATE_GOLDEN").is_ok() {
        std::fs::write(bundle_step_golden_path(), format!("{pretty}\n")).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(bundle_step_golden_path()).unwrap_or_else(|_| {
        panic!(
            "missing golden at {} — run with DARKMUX_REVIEW_CONFORMANCE_UPDATE_GOLDEN=1 to generate it",
            bundle_step_golden_path().display()
        )
    });
    assert_eq!(
        pretty.trim_end(),
        expected.trim_end(),
        "the real bundler's output over the worktree fixture drifted from the committed golden at \
         {} — this is Hole 4's regression net (#2336 review): the ids/files/facts a REAL bundler \
         run produces, byte-pinned.",
        bundle_step_golden_path().display()
    );
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

    let total_calls = Arc::new(AtomicU32::new(0));
    let ctx = step_ctx(
        Arc::new(AtomicU32::new(0)),
        Arc::new(AtomicU32::new(0)),
        Arc::new(AtomicU32::new(0)),
        total_calls.clone(),
    );
    let judge = ctx.roles.judge.clone();
    let verify = ctx.roles.verify.clone();
    let probes = ctx.roles.probes.clone();

    let fingerprint_val = fingerprint(&seat_identifier(&judge.pm), &ctx.judge_system);
    let staffing_snap = staffing_snapshot(&probes, &judge, verify.as_ref(), ctx.roles.request_changes);
    let crew_name = ctx.roles.distinct_profile_names();

    let mut graph = build_review_graph(
        ctx.clone(),
        &dummy_bundle_spec(),
        judge,
        verify,
        &probes,
        "investigate",
        "adjudicate",
        "report",
        // judge_concurrency: 1 — byte-identical dispatch order to the
        // historical sequential judge loop. `chat_fn` discriminates by
        // BUNDLE (a substring of `call.user`) rather than call order, so
        // this no longer gates correctness the way it once did — kept at 1
        // anyway to keep the golden's `members[].wall_ms`-adjacent ordering
        // (pre-`canonicalize`) matching production's own default.
        1,
    )
    .expect("the shipped review.json builds cleanly");
    // (#2310 P1) See the identical comment in
    // `review_bundle_step_runs_the_real_bundler_over_a_worktree` — this
    // hermetic test mints no Mission/Phase, so `review.context`'s config
    // needs the `context_out` escape hatch.
    {
        let context_step = graph
            .steps
            .get_mut("review-context-step")
            .expect("the shipped review.json declares a review-context-step");
        let mut cfg = context_step.config.clone();
        cfg["context_out"] = serde_json::json!(home.path().join("review-context.json").display().to_string());
        // (#2310 P1 fix) The three fixture prompts + two budget knobs are
        // now carried on `ctx.context_test_overrides` (set at construction
        // above) — the bus seam `run_review_graph` seeds from `ctx`, not a
        // step-config key.
        context_step.config = cfg;
    }
    stamp_report_emit(&mut graph, home.path());

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
    assert_eq!(env.bundles, 4, "four synthetic bundles");
    assert_eq!(
        env.raw_flags, 5,
        "billing(high) + billing(mid, dup) + auth(mid) + docs(low) + config(low)"
    );
    assert_eq!(
        env.deduped_flags, 4,
        "billing's two raw flags collapse into one (Hole 2, #2336 review); auth/docs/config stay distinct"
    );
    assert_eq!(env.confirmed, 1, "only billing.ts stays Confirmed (config.ts is refuted back to Archived)");
    assert_eq!(env.needs_check, 1, "auth.ts demoted by pass-2 disagreement");
    assert_eq!(
        env.archived, 2,
        "docs/setup.md ruled false_positive on pass-1, config.ts demoted by verify (Hole 3, #2336 review)"
    );
    assert_eq!(env.verified, 1, "billing.ts reaches verify and is verified");
    assert_eq!(env.refuted, 1, "config.ts reaches verify and is refuted (Hole 3, #2336 review)");
    assert!(env.degenerate.is_none(), "a ruled-on docket must never read as degenerate: {:?}", env.degenerate);
    let demoted = env.judged.iter().find(|j| j.flag.bundle_id == "auth@src/auth.ts").expect("auth flag present");
    assert!(demoted.demoted_by_pass2, "auth.ts must be recorded as demoted by pass-2");
    let confirmed = env.judged.iter().find(|j| j.flag.bundle_id == "billing@src/billing.ts").expect("billing flag present");
    assert_eq!(confirmed.flag.anchor.as_deref(), Some("const end = start.plus(30)"), "anchor must resolve against the diff");
    // (Hole 2, #2336 review) The billing survivor keeps the FIRST-SEEN
    // seat's attribution (`review-conformance-probe-high`, not `-mid`) and
    // records the absorbed duplicate's own charge text.
    assert_eq!(
        confirmed.flag.member,
        seat_identifier(&ctx.roles.probes[0].pm),
        "the surviving billing flag keeps probe-high's attribution, not probe-mid's"
    );
    assert_eq!(confirmed.flag.also_flagged.len(), 1, "the absorbed probe-mid duplicate is folded in, not dropped");
    let refuted_finding =
        env.judged.iter().find(|j| j.flag.bundle_id == "config@src/config.ts").expect("config flag present");
    assert!(refuted_finding.demoted_by_verify, "config.ts must be recorded as demoted by verify");
    assert_eq!(
        refuted_finding.verify.as_ref().map(|v| v.ruling),
        Some(darkmux_lab::lab::review::VerifyRuling::Refuted)
    );
    // (Hole 3, #2336 review) The verify seat is endpoint-bearing (`crew()`'s
    // `remote_seat`) — proves `MemberRecord::remote` and `remote_budgets`
    // actually populate from a real endpoint-shaped seat, not just from the
    // struct's own `Default`.
    let verify_member = env
        .members
        .iter()
        .find(|m| m.seat == "review-verify")
        .expect("verify member row present");
    assert!(verify_member.remote, "the verify seat is endpoint-bearing and must report remote == true");
    assert!(
        env.remote_budgets.iter().any(|b| b.stage == "verify"),
        "an endpoint-bearing verify seat must produce a remote_budgets row: {:?}",
        env.remote_budgets
    );
    // (Docs, #2336 review) The module doc used to claim 9 probe calls; the
    // real total (instrumented here, not inferred) is 19 — 3 probe seats x
    // 4 bundles = 12 first attempts, of which 5 succeed (the raw flags) and
    // 7 come back empty and each retry once (`retry_on_empty: 1`,
    // review.rs:6857): 12 + 7 = 19. Judge fires 7 (billing 2 + auth 2 +
    // docs 1 + config 2) and verify fires 2 (billing + config): 19 + 7 + 2
    // = 28 total dispatch calls this run makes. `env.members[].total_tokens`
    // is where the hidden empty-retry accounting actually surfaces in the
    // envelope: `MapItemResult::total_tokens` sums EVERY attempt's reported
    // usage for an item, including a discarded empty first try, so a probe
    // seat's summed `total_tokens` can exceed its `draws` count (e.g.
    // probe-high: draws=4, total_tokens=7 — one 1-token success plus three
    // 2-token empty-then-retried items).
    assert_eq!(
        total_calls.load(Ordering::SeqCst),
        28,
        "19 probe (12 first attempts + 7 empty retries) + 7 judge + 2 verify"
    );
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

// ─── #2310 P3: harness scenarios beyond the one happy-path golden ─────────
//
// The P0 conformance golden pins ONE happy scenario — every probe/judge/
// verify dispatch succeeds, `env.warnings` is empty, and every step reaches
// `Complete`. Both critical findings on P2's own review lived in states that
// golden never carries (a non-empty `env.warnings`, a step that genuinely
// ERRORS). P3 adds two more scenarios BEFORE touching the `review.report`
// step kind, so that step's own render golden (added alongside it) has a
// warned/errored envelope to render against, not just the clean one.

/// Like [`reply`], with an explicit `total_tokens` — the verify-exhaustion
/// scenario below needs ONE call to report enough usage to actually spend
/// down a small remote-token bucket; [`reply`]'s fixed `total_tokens: 1` is
/// too small for `RemoteBudget::settle` to ever leave a bucket exhausted
/// (see this function's one call site for the exact arithmetic).
fn reply_with_tokens(content: &str, total_tokens: u64) -> SingleShotReply {
    SingleShotReply {
        content: content.to_string(),
        total_tokens: Some(total_tokens),
        prompt_tokens: Some(total_tokens / 2),
        completion_tokens: Some(total_tokens / 2),
        model: None,
    }
}

/// The "warned" scenario's crew: probes stay LOCAL (unchanged from
/// [`crew`]) — a probe dispatch failure needs no remote seat to produce its
/// own warning (`reconstruct_probe_seat` fires on `errors > 0` regardless of
/// `remote`). Judge AND verify are BOTH remote (mirroring `crew()`'s own
/// `remote_seat` for verify) — a judge warning's two possible sources
/// (`JudgeGateOutcome::dispatch_error_warning`/`coverage_warning`) both
/// require `is_remote`, so a local judge structurally cannot produce one.
fn warned_crew() -> ResolvedReviewRoles {
    ResolvedReviewRoles {
        probes: vec![
            seat("review-probe-high", "review-conformance-probe-high", 2),
            seat("review-probe-mid", "review-conformance-probe-mid", 2),
            seat("review-probe-low", "review-conformance-probe-low", 2),
        ],
        judge: remote_seat("review-judge", "review-conformance-judge", 2),
        verify: Some(remote_seat("review-verify", "review-conformance-verify", 2)),
        request_changes: false,
        warnings: vec![],
    }
}

/// The warned scenario's canned dispatch — [`chat_fn`] plus four
/// deliberate deviations, one per warning source this scenario exists to
/// pin:
///
/// - **probe**: `probe-high` x `src/config.ts` (a pairing [`chat_fn`]
///   answers with an empty reply — no flag either way) always ERRORS
///   instead, so `reconstruct_probe_seat` records `errors == 1` and pushes
///   its own "dispatch failed on 1 draw(s)" warning. `deduped_flags` stays
///   identical to the happy path (an empty reply produces no flag; an
///   errored one doesn't either).
/// - **judge**: `docs/setup.md`'s pass-1 call ERRORS instead of ruling
///   `false_positive`. `judge_pass_with_retry` retries once on
///   `Unparsed`, never on a dispatch `Err` (see that function's own doc),
///   so this fires the `JudgeGateOutcome::dispatch_error_warning` path
///   unconditionally (`is_remote && dispatch_errors > 0`) — independent of
///   any token budget. `docs` drops out of `usable` (an `Error` ruling
///   isn't `Confirmed`/`NeedsCheck`/`FalsePositive`), but billing/auth/
///   config still are, so the judge-dead gate never fires.
/// - **verify**: billing's VERIFIED reply reports `total_tokens: 2000` —
///   `warned_step_ctx`'s `remote_max_tokens_per_execution` is ALSO 2000, so
///   this single call's `RemoteBudget::settle` leaves the bucket fully
///   spent (see that scenario's own doc for the arithmetic). config's
///   verify call — the docket's second and last confirmed finding — is
///   never expected to reach this closure at all: the bucket denies it
///   BEFORE dispatch, so `call.user.contains("src/config.ts")` under
///   `VERIFY seat` panics here as a canary if that budget math ever drifts.
/// - every other (seat, bundle) pairing is unchanged from [`chat_fn`].
fn warned_chat_fn(
    billing_pass: Arc<AtomicU32>,
    auth_pass: Arc<AtomicU32>,
    config_pass: Arc<AtomicU32>,
    total_calls: Arc<AtomicU32>,
) -> impl Fn(&ChatCall) -> Result<SingleShotReply> + Send + Sync + 'static {
    move |call: &ChatCall| {
        total_calls.fetch_add(1, Ordering::SeqCst);
        if call.system.contains("VERIFY seat") {
            if call.user.contains("src/billing.ts") {
                return Ok(reply_with_tokens(VERIFIED_JSON, 2_000));
            }
            if call.user.contains("src/config.ts") {
                panic!(
                    "review-conformance warned scenario: the config verify call reached \
                     chat_fn — the billing call's 2000-token settle should have exhausted the \
                     2000-token bucket first (budget math drifted)"
                );
            }
            panic!("review-conformance warned scenario: unexpected VERIFY call: {}", call.user);
        }
        if call.system.contains("JUDGE seat") {
            if call.user.contains("docs/setup.md") {
                anyhow::bail!("review-conformance warned scenario: synthetic judge dispatch failure");
            }
            if call.user.contains("src/billing.ts") {
                billing_pass.fetch_add(1, Ordering::SeqCst);
                return Ok(reply(CONFIRM_JSON));
            }
            if call.user.contains("src/config.ts") {
                let idx = config_pass.fetch_add(1, Ordering::SeqCst);
                return Ok(reply(match idx {
                    0 | 1 => CONFIRM_JSON,
                    other => panic!("review-conformance warned scenario: unexpected config judge pass {other}"),
                }));
            }
            if call.user.contains("src/auth.ts") {
                let idx = auth_pass.fetch_add(1, Ordering::SeqCst);
                return Ok(reply(match idx {
                    0 => CONFIRM_JSON,
                    1 => NEEDS_CHECK_JSON,
                    other => panic!("review-conformance warned scenario: unexpected auth judge pass {other}"),
                }));
            }
            panic!("review-conformance warned scenario: unexpected JUDGE call: {}", call.user);
        }
        // Probe call — identical to `chat_fn` EXCEPT probe-high x
        // src/config.ts, which always errors (see this function's own doc).
        if call.model.contains("probe-high") && call.user.contains("src/config.ts") {
            anyhow::bail!("review-conformance warned scenario: synthetic probe dispatch failure");
        }
        if call.model.contains("probe-high") && call.user.contains("src/billing.ts") {
            return Ok(reply("a real defect: `const end = start.plus(30)`"));
        }
        if call.model.contains("probe-mid") && call.user.contains("src/billing.ts") {
            return Ok(reply("a real defect: `const end = start.plus(30)`"));
        }
        if call.model.contains("probe-mid") && call.user.contains("src/auth.ts") {
            return Ok(reply("a real defect: `if (user.role = \"admin\")`"));
        }
        if call.model.contains("probe-low") && call.user.contains("docs/setup.md") {
            return Ok(reply("a suspicious change: `A new heading appears here`"));
        }
        if call.model.contains("probe-low") && call.user.contains("src/config.ts") {
            return Ok(reply("a real defect: `const port = env.PORT + 1`"));
        }
        Ok(reply(""))
    }
}

fn warned_step_ctx(
    billing_pass: Arc<AtomicU32>,
    auth_pass: Arc<AtomicU32>,
    config_pass: Arc<AtomicU32>,
    total_calls: Arc<AtomicU32>,
) -> Arc<ReviewStepContext> {
    Arc::new(ReviewStepContext {
        case_id: "review-conformance-warned-case".to_string(),
        roles: warned_crew(),
        intent_title: "Fix billing rounding".to_string(),
        intent_body: "Cleans up a couple of billing/auth edge cases.".to_string(),
        diff: DIFF.to_string(),
        probe_system: "You are the review-conformance PROBE seat.".to_string(),
        probe_role_prompts: std::collections::BTreeMap::new(),
        judge_system: "You are the review-conformance JUDGE seat.".to_string(),
        verify_system: "You are the review-conformance VERIFY seat.".to_string(),
        // (see `warned_chat_fn`'s own doc) — deliberately small: billing's
        // 2000-token verify reply exhausts a 2000-token bucket exactly.
        // Judge's OWN pass1/pass2 buckets get this SAME ceiling (each a
        // SEPARATE `RemoteBudget` instance, not a shared pool — see
        // `ReviewJudgeStepKind::run_streaming`'s construction) but never
        // approach it: every judge reply in this scenario reports
        // `total_tokens: 1` (`reply`'s fixed value), so cumulative real
        // spend across every pass stays in the single digits.
        remote_max_tokens_per_execution: 2_000,
        judge_exhaustion_strict: false,
        timeout_seconds: 30,
        chat_override: Some(Arc::new(warned_chat_fn(billing_pass, auth_pass, config_pass, total_calls))),
        bundle_override: Some(Arc::new(|| {
            Ok(vec![billing_bundle(), auth_bundle(), docs_bundle(), config_bundle()])
        })),
        mission_id: None,
        crew_name: None,
        mode_label: None,
        fingerprint: None,
        staffing: None,
        interpret_warnings: Vec::new(),
        diff_file: None,
        intent_file: None,
        context_test_overrides: ReviewContextTestOverrides {
            probe_system: Some("You are the review-conformance PROBE seat.".to_string()),
            judge_system: Some("You are the review-conformance JUDGE seat.".to_string()),
            verify_system: Some("You are the review-conformance VERIFY seat.".to_string()),
            remote_max_tokens_per_execution: Some(2_000),
            judge_exhaustion_strict: Some(false),
        },
    })
}

fn warned_golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/review-conformance/envelope-warned.json")
}

/// (#2310 P3) Every warning SOURCE fires at once, on an otherwise
/// ruled-on (non-degenerate) docket — the shape #2310 P2's own review found
/// missing from the P0 golden. `env.warnings`' pinned order is
/// interpret -> judge -> verify -> probe (`fold_review_outputs_into_envelope`'s
/// own doc); this test's `canonicalize`/golden-compare pins that order
/// byte-for-byte, same as every other field.
#[test]
#[serial_test::serial]
fn review_pipeline_warned_scenario_matches_the_committed_golden() {
    let home = tempfile::tempdir().expect("tempdir");
    let _guard = HomeGuard::set(home.path());

    let total_calls = Arc::new(AtomicU32::new(0));
    let ctx = warned_step_ctx(
        Arc::new(AtomicU32::new(0)),
        Arc::new(AtomicU32::new(0)),
        Arc::new(AtomicU32::new(0)),
        total_calls.clone(),
    );
    let judge = ctx.roles.judge.clone();
    let verify = ctx.roles.verify.clone();
    let probes = ctx.roles.probes.clone();

    let fingerprint_val = fingerprint(&seat_identifier(&judge.pm), &ctx.judge_system);
    let staffing_snap = staffing_snapshot(&probes, &judge, verify.as_ref(), ctx.roles.request_changes);
    let crew_name = ctx.roles.distinct_profile_names();

    let mut graph = build_review_graph(
        ctx.clone(),
        &dummy_bundle_spec(),
        judge,
        verify,
        &probes,
        "investigate",
        "adjudicate",
        "report",
        1,
    )
    .expect("the shipped review.json builds cleanly");
    {
        let context_step = graph
            .steps
            .get_mut("review-context-step")
            .expect("the shipped review.json declares a review-context-step");
        let mut cfg = context_step.config.clone();
        cfg["context_out"] = serde_json::json!(home.path().join("review-context.json").display().to_string());
        context_step.config = cfg;
    }
    // (#2310 P3) "seed at the real site" — `graph.interpret_warnings` is the
    // EXACT field `run_review_graph` reads at its own top (`let
    // BuiltReviewGraph { .., interpret_warnings, .. } = graph;`) and folds
    // into `ReviewStepContext::interpret_warnings` for
    // `ReviewSynthesisStepKind` to read — production populates it from
    // `mission_config::interpret`'s own pruning warnings inside
    // `build_review_graph_from_config`; this scenario's own probe claim
    // needs no pruning (all three declared probe tasks are claimed, same as
    // every other scenario in this file), so this stands in for that real
    // production source without contriving an actual pruned task.
    graph.interpret_warnings.push(
        "review-conformance: synthetic interpret-time warning (#2310 P3 harness)".to_string(),
    );
    stamp_report_emit(&mut graph, home.path());

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
    .expect("review-conformance warned-scenario graph run completes");

    // Sanity checks BEFORE the golden compare (see the happy-path test's
    // identical discipline).
    assert!(
        env.degenerate.is_none(),
        "a partially-warned but still ruled-on docket must never read as degenerate: {:?}",
        env.degenerate
    );
    assert_eq!(env.bundles, 4);
    assert_eq!(env.deduped_flags, 4, "the probe-high x config.ts error produces no flag either way");
    assert_eq!(
        env.warnings.len(),
        4,
        "one warning per source (interpret, judge, verify, probe): {:?}",
        env.warnings
    );
    assert!(env.warnings[0].contains("synthetic interpret-time warning"), "{:?}", env.warnings);
    assert!(
        env.warnings[1].contains("remote judge dispatch failed"),
        "judge's warning must be second (interpret -> judge -> verify -> probe): {:?}",
        env.warnings
    );
    assert!(
        env.warnings[2].contains("verify budget exhausted"),
        "verify's warning must be third: {:?}",
        env.warnings
    );
    assert!(
        env.warnings[3].contains("dispatch failed on 1 draw"),
        "probe's warning must be last (dedup.warnings folds in last): {:?}",
        env.warnings
    );
    let config_flag = env.judged.iter().find(|j| j.flag.bundle_id == "config@src/config.ts").expect("config flag present");
    assert_eq!(
        config_flag.tier,
        darkmux_lab::lab::review::Tier::Confirmed,
        "config keeps the manual-verification marker (a budget SKIP, not a Refuted ruling) — \
         never silently demoted"
    );
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
        std::fs::write(warned_golden_path(), format!("{pretty}\n")).expect("write golden");
        return;
    }

    let expected = std::fs::read_to_string(warned_golden_path()).unwrap_or_else(|_| {
        panic!(
            "missing golden at {} — run with DARKMUX_REVIEW_CONFORMANCE_UPDATE_GOLDEN=1 to generate it",
            warned_golden_path().display()
        )
    });
    assert_eq!(
        pretty.trim_end(),
        expected.trim_end(),
        "the warned-scenario envelope drifted from the committed conformance golden at {}. \
         Regenerate with DARKMUX_REVIEW_CONFORMANCE_UPDATE_GOLDEN=1 cargo test -p darkmux-lab \
         --test review_conformance, then review the diff before committing.",
        warned_golden_path().display()
    );
}

fn errored_golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/review-conformance/envelope-errored.json")
}

/// (#2310 P3) The judge step itself ERRORS (not a per-flag dispatch
/// failure — a genuine `NodeStatus::Error` at the scheduler level), landing
/// `run_review_graph` on its `report.errored`-non-empty fallback path
/// (review.rs, the `else` branch after the successful-path `if
/// report.errored.is_empty()`). A plain `Err` returned from `chat_override`
/// is swallowed per-flag by `judge_pass_with_retry`/`run_judge_pass` (see
/// `warned_chat_fn`'s own doc) — it never reaches the scheduler as a step
/// failure. A PANIC does: `ReviewJudgeStepKind::run_streaming` dispatches
/// each flag inside a `std::thread::scope` (review.rs's judge chunk loop),
/// and a panicking spawned thread re-panics the scope on join, which
/// `darkmux_crew::concurrent_dispatch::run_bounded` (proven by its own unit
/// test, `run_bounded_reconciles_a_panicking_local_job_to_a_terminal_error`)
/// reconciles to a terminal `NodeStatus::Error` rather than crashing the
/// process.
fn errored_chat_fn(
) -> impl Fn(&ChatCall) -> Result<SingleShotReply> + Send + Sync + 'static {
    move |call: &ChatCall| {
        if call.system.contains("JUDGE seat") {
            panic!("review-conformance errored scenario: synthetic judge step panic");
        }
        if call.system.contains("VERIFY seat") {
            panic!(
                "review-conformance errored scenario: the judge step erroring must block the \
                 verify task (unmet `depends_on`) — a verify call reaching this closure means \
                 that no longer holds"
            );
        }
        // Every probe call succeeds normally — the bundle/probe/dedup
        // stages must all COMPLETE before the judge step's own panic is the
        // interesting event; only the judge stage is meant to fail here.
        if call.model.contains("probe-high") && call.user.contains("src/billing.ts") {
            return Ok(reply("a real defect: `const end = start.plus(30)`"));
        }
        if call.model.contains("probe-mid") && call.user.contains("src/auth.ts") {
            return Ok(reply("a real defect: `if (user.role = \"admin\")`"));
        }
        if call.model.contains("probe-low") && call.user.contains("docs/setup.md") {
            return Ok(reply("a suspicious change: `A new heading appears here`"));
        }
        if call.model.contains("probe-low") && call.user.contains("src/config.ts") {
            return Ok(reply("a real defect: `const port = env.PORT + 1`"));
        }
        Ok(reply(""))
    }
}

fn errored_step_ctx() -> Arc<ReviewStepContext> {
    Arc::new(ReviewStepContext {
        case_id: "review-conformance-errored-case".to_string(),
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
        chat_override: Some(Arc::new(errored_chat_fn())),
        bundle_override: Some(Arc::new(|| {
            Ok(vec![billing_bundle(), auth_bundle(), docs_bundle(), config_bundle()])
        })),
        mission_id: None,
        crew_name: None,
        mode_label: None,
        fingerprint: None,
        staffing: None,
        interpret_warnings: Vec::new(),
        diff_file: None,
        intent_file: None,
        context_test_overrides: ReviewContextTestOverrides {
            probe_system: Some("You are the review-conformance PROBE seat.".to_string()),
            judge_system: Some("You are the review-conformance JUDGE seat.".to_string()),
            verify_system: Some("You are the review-conformance VERIFY seat.".to_string()),
            remote_max_tokens_per_execution: Some(500_000),
            judge_exhaustion_strict: Some(false),
        },
    })
}

/// (#2310 P3) Red-proof for this scenario lives in the assertions below,
/// not a separate mutation test: `env.degenerate` MUST be `Some`
/// (naming the judge step by id) and MUST carry forward whatever the
/// bundle/dedup stages already completed (`bundles == 4`, `raw_flags`/
/// `deduped_flags` from the three probe seats that DID finish) — a
/// regression that dropped the "rebuild from completed typed outputs"
/// half of the fallback (#2310 P2 review finding C2) would silently
/// revert to `fallback_env` alone (identity + reason, nothing else),
/// which this test's own `bundles`/`deduped_flags` assertions would catch
/// by reading back as zero.
#[test]
#[serial_test::serial]
fn review_pipeline_errored_scenario_matches_the_committed_golden() {
    let home = tempfile::tempdir().expect("tempdir");
    let _guard = HomeGuard::set(home.path());

    let ctx = errored_step_ctx();
    let judge = ctx.roles.judge.clone();
    let verify = ctx.roles.verify.clone();
    let probes = ctx.roles.probes.clone();

    let fingerprint_val = fingerprint(&seat_identifier(&judge.pm), &ctx.judge_system);
    let staffing_snap = staffing_snapshot(&probes, &judge, verify.as_ref(), ctx.roles.request_changes);
    let crew_name = ctx.roles.distinct_profile_names();

    let mut graph = build_review_graph(
        ctx.clone(),
        &dummy_bundle_spec(),
        judge,
        verify,
        &probes,
        "investigate",
        "adjudicate",
        "report",
        1,
    )
    .expect("the shipped review.json builds cleanly");
    {
        let context_step = graph
            .steps
            .get_mut("review-context-step")
            .expect("the shipped review.json declares a review-context-step");
        let mut cfg = context_step.config.clone();
        cfg["context_out"] = serde_json::json!(home.path().join("review-context.json").display().to_string());
        context_step.config = cfg;
    }

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
    .expect("run_review_graph itself must still return Ok — a step ERROR is not a hard Err out of this call");

    assert!(env.degenerate.is_some(), "an errored judge step must read as a named degenerate run");
    assert_eq!(
        env.degenerate_kind,
        Some(darkmux_lab::lab::review::DegenerateKind::Error)
    );
    assert_eq!(env.bundles, 4, "the bundle step completed before the judge step failed");
    assert_eq!(env.raw_flags, 4, "the probe stage completed before the judge step failed (no duplicate flag in this scenario's fixture)");
    // (#2310 P2 review finding C2's own fallback, read empirically) The
    // errored-run fallback folds `dedup.raw_flags`/`members`/`warnings`/
    // `remote_budget`/`degenerate` via `fold_review_outputs_into_envelope`,
    // but never assigns `env.deduped_flags`/`env.flags` — those two are
    // set ONLY on the ordinary synthesis path (`ReviewSynthesisStepKind::
    // run_streaming`'s own `env.deduped_flags = flags.len(); env.flags =
    // flags;`, after the shared fold returns). A step ERROR never reaches
    // that step, so both stay at `ReviewEnvelope::default()`'s zero value —
    // `raw_flags` (the dedup stage's own count) is what actually survives.
    assert_eq!(env.deduped_flags, 0, "deduped_flags/flags are synthesis-only fields, never set by the fallback");
    assert!(env.judged.is_empty(), "the judge step never produced a typed output to fold in");
    let judge_step = steps.values().find(|s| s.kind == "review.judge").expect("a review.judge step exists");
    assert_eq!(
        judge_step.status,
        darkmux_crew::types::NodeStatus::Error,
        "the judge step itself must be the one that reached NodeStatus::Error"
    );
    let verify_task_ran = steps.values().any(|s| {
        s.kind.starts_with("review.verify") && s.status == darkmux_crew::types::NodeStatus::Complete
    });
    assert!(!verify_task_ran, "the verify task depends on the judge task and must never run");

    let actual = canonicalize(serde_json::to_value(&env).expect("ReviewEnvelope serializes"));
    let pretty = serde_json::to_string_pretty(&actual).expect("pretty-print");

    if std::env::var("DARKMUX_REVIEW_CONFORMANCE_UPDATE_GOLDEN").is_ok() {
        std::fs::write(errored_golden_path(), format!("{pretty}\n")).expect("write golden");
        return;
    }

    let expected = std::fs::read_to_string(errored_golden_path()).unwrap_or_else(|_| {
        panic!(
            "missing golden at {} — run with DARKMUX_REVIEW_CONFORMANCE_UPDATE_GOLDEN=1 to generate it",
            errored_golden_path().display()
        )
    });
    assert_eq!(
        pretty.trim_end(),
        expected.trim_end(),
        "the errored-scenario envelope drifted from the committed conformance golden at {}. \
         Regenerate with DARKMUX_REVIEW_CONFORMANCE_UPDATE_GOLDEN=1 cargo test -p darkmux-lab \
         --test review_conformance, then review the diff before committing.",
        errored_golden_path().display()
    );
}

/// (#2310 P4a review fix M1/M2) The OPERATOR-FACING artifact test: reads
/// the EMITTED payload `review-report-step` itself writes (the comment
/// file + the envelope_out JSON), never `run_review_graph`'s in-memory
/// return value directly — that's what an operator/CI actually sees.
/// Before this fix, `report_ran` flipping `true` (the step now runs on
/// this errored graph, #2310 P4a) silently regressed the emitted
/// artifact: the step's own hard-fail-on-missing-synthesis-output path
/// produced a payload naming a "missing depends_on edge" config excuse
/// instead of the judge panic that actually happened, and the launcher's
/// `report_ran` fallback (which USED to render the rich envelope) never
/// fired because the step had already (wrongly) "succeeded".
#[test]
#[serial_test::serial]
fn errored_scenario_emitted_payload_carries_the_real_failure_not_a_config_excuse() {
    let home = tempfile::tempdir().expect("tempdir");
    let _guard = HomeGuard::set(home.path());

    let ctx = errored_step_ctx();
    let judge = ctx.roles.judge.clone();
    let verify = ctx.roles.verify.clone();
    let probes = ctx.roles.probes.clone();

    let fingerprint_val = fingerprint(&seat_identifier(&judge.pm), &ctx.judge_system);
    let staffing_snap = staffing_snapshot(&probes, &judge, verify.as_ref(), ctx.roles.request_changes);
    let crew_name = ctx.roles.distinct_profile_names();

    let mut graph = build_review_graph(
        ctx.clone(),
        &dummy_bundle_spec(),
        judge,
        verify,
        &probes,
        "investigate",
        "adjudicate",
        "report",
        1,
    )
    .expect("the shipped review.json builds cleanly");
    {
        let context_step = graph
            .steps
            .get_mut("review-context-step")
            .expect("the shipped review.json declares a review-context-step");
        let mut cfg = context_step.config.clone();
        cfg["context_out"] = serde_json::json!(home.path().join("review-context.json").display().to_string());
        context_step.config = cfg;
    }

    let emit_path = home.path().join("comment.json");
    let envelope_out_path = home.path().join("envelope.json");
    {
        let report_step = graph
            .steps
            .get_mut("review-report-step")
            .expect("the shipped review.json declares a review-report-step");
        report_step.config = serde_json::json!({
            "emit": emit_path.display().to_string(),
            "envelope_out": envelope_out_path.display().to_string(),
        });
    }

    let (driver_env, steps) = run_review_graph(
        &ctx,
        &crew_name,
        ExecMode::Sequential,
        fingerprint_val,
        staffing_snap,
        graph,
        &mut NullEmitter,
        &mut |_step| {},
    )
    .expect("run_review_graph itself must still return Ok — a step ERROR is not a hard Err out of this call");

    // The step itself must have RUN (this is #2310 P4a's whole point) and
    // COMPLETED — not stayed Planned, not itself errored.
    let report_step = steps.get("review-report-step").expect("review.json declares review-report-step");
    assert_eq!(report_step.status, darkmux_crew::types::NodeStatus::Complete);

    // (a) The EMITTED comment names the judge's real failure text, never
    // a "must depends_on/reads review-synthesis-task" config excuse.
    let comment_json = std::fs::read_to_string(&emit_path).expect("the step must have written the emit file");
    let comment_payload: serde_json::Value =
        serde_json::from_str(&comment_json).expect("emitted payload must be valid JSON");
    let comment_text = comment_payload["comment"].as_str().expect("payload has a comment field");
    // The judge step PANICS inside a spawned thread (`errored_chat_fn`'s
    // own doc) — `run_bounded` reconciles a panicking job to a terminal
    // `NodeStatus::Error` whose recorded `output` is its OWN wrapping
    // message (the raw panic payload goes to stderr, per #1452 — see
    // `run_step_graph_panicking_step_persists_terminal_error_never_
    // running`, darkmux-crew). THIS is the real failure text that must
    // travel all the way from the judge step, through `cascade_abandon`'s
    // origin-propagation and `gather_inputs`'s run_on-aware forwarding,
    // into the report step's own rendered comment — the SAME text the
    // committed golden's `degenerate` field already names.
    assert!(
        comment_text.contains("review-judge-step:")
            && comment_text.contains("a dispatch job panicked mid-wave")
            && comment_text.contains("#1452"),
        "the emitted comment must name the judge step's REAL recorded failure text, got: {comment_text}"
    );
    assert!(
        !comment_text.contains("depends_on") && !comment_text.contains("no `review.envelope` output"),
        "the emitted comment must never fall back to naming a config-shaped excuse: {comment_text}"
    );

    // (b) mode == "degraded".
    assert_eq!(comment_payload["mode"], "degraded");

    // (c) crew/staffing/bundle count present in the emitted envelope_out.
    let envelope_json =
        std::fs::read_to_string(&envelope_out_path).expect("the step must have written envelope_out");
    let emitted_env: serde_json::Value =
        serde_json::from_str(&envelope_json).expect("envelope_out must be valid JSON");
    assert_eq!(emitted_env["crew"], serde_json::json!(crew_name), "crew must survive into the emitted envelope");
    assert!(emitted_env["staffing"].is_object(), "staffing must survive into the emitted envelope");
    assert_eq!(emitted_env["bundles"], serde_json::json!(4), "the bundle count must survive into the emitted envelope");
    // (M2) case_id is pinned, not left at a hardcoded/default placeholder.
    assert_eq!(emitted_env["case_id"], serde_json::json!("review-conformance-errored-case"));

    // The emitted envelope equals the driver's OWN `run_review_graph`
    // return value (`driver_env`) BYTE-FOR-BYTE, including `degenerate`
    // — the step's independent, input-scoped reconstruction and the
    // driver's full-`steps`-map reconstruction agree exactly, because
    // `ReviewReportStepKind::run_streaming` wraps its propagated origin
    // reason in the SAME "review graph: N step(s) errored, zero usable
    // signal --" framing `errored_steps_degenerate_reason` (the driver's
    // own source) uses, rather than leaving it bare.
    let driver_value = canonicalize(serde_json::to_value(&driver_env).expect("driver env serializes"));
    let emitted_value = canonicalize(emitted_env);
    assert_eq!(
        driver_value, emitted_value,
        "the step's independently-reconstructed envelope must match the driver's own envelope byte-for-byte"
    );
    let degenerate = driver_value["degenerate"].as_str().unwrap_or_default();
    assert!(
        degenerate.contains("review-judge-step:") && degenerate.contains("a dispatch job panicked mid-wave") && degenerate.contains("#1452"),
        "degenerate text must name the judge step's real recorded failure, got: {degenerate}"
    );
}

/// (#2310 P4a, supersedes #2345 C1's premise) `review-report-task`
/// declares `run_on: ["complete", "error"]` (review.json) and the
/// scheduler's `cascade_abandon` (darkmux-crew) rolls
/// `review-synthesis-task` to `Abandoned` the moment the judge step
/// errors — `review-report-step` is READY the same scheduler pass, runs,
/// finds no `review-synthesis-task` output, and degrades HONESTLY through
/// its own `degraded_report_envelope` fallback (this crate,
/// `lab::review`) instead of hard-failing. Pre-#2310-P4a, this same
/// scenario left the step `Planned` forever (this test's old name/doc,
/// preserved in git history) and only the LAUNCHER's own fallback
/// (`src/mission_launch_review.rs::run_dispatch`, #2345 C1) ever rendered
/// anything. That fallback still exists (untouched by P4a — steps 2-4 of
/// #2310 are superseded, not started) but is now a documented, guarded
/// no-op on this path: it checks `report_ran = steps.get("review-report-
/// step").is_some_and(|s| s.status == NodeStatus::Complete)` BEFORE
/// calling `render_and_emit_review` a second time — see that call site's
/// own comment ("A no-op on the ordinary path, where the step already
/// did this"). With the step now completing here, `report_ran` is `true`
/// and the fallback never fires — no double-render, no code change
/// needed there.
#[test]
#[serial_test::serial]
fn errored_scenario_report_step_now_runs_and_renders_the_degraded_envelope_through_the_step() {
    let home = tempfile::tempdir().expect("tempdir");
    let _guard = HomeGuard::set(home.path());

    let ctx = errored_step_ctx();
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
    .expect("run_review_graph itself must still return Ok — a step ERROR is not a hard Err out of this call");

    let report_step = steps.get("review-report-step").expect("review.json declares review-report-step");
    assert_eq!(
        report_step.status,
        darkmux_crew::types::NodeStatus::Complete,
        "(#2310 P4a) review-report-task's run_on: [\"complete\", \"error\"] plus the scheduler's \
         cascade-abandon make this step READY (its one dependency, review-synthesis-task, is \
         Abandoned — a terminal status \"error\" accepts) and it runs to completion, rendering a \
         degraded envelope through degraded_report_envelope rather than staying wedged Planned"
    );
    assert_eq!(
        report_step.output.as_deref(),
        Some("-"),
        "no emit path is stamped in this harness (that's the LAUNCHER's job, unexercised here), \
         so the step rendered to stdout — its own output records that destination, same as any \
         other run"
    );
    assert!(env.degenerate.is_some(), "sanity: this is the same errored scenario as the golden test above");

    // `render_and_emit_review` remains directly callable outside the graph
    // for the OTHER entry points that render a synthesis-only envelope
    // without ever building a graph at all (`charges_file` re-judge,
    // `--from-envelope`) — exercised here against a fresh emit path to
    // confirm it still produces the same degraded payload shape.
    let emit_path = home.path().join("rendered.json");
    let mode = render_and_emit_review(&env, &ctx.diff, None, Some(&emit_path), None)
        .expect("render_and_emit_review must be able to render + emit an errored run's envelope");
    assert_eq!(mode, "degraded", "a degenerate run with zero judged flags renders as degraded");

    let written =
        std::fs::read_to_string(&emit_path).expect("render_and_emit_review must have written to emit_path");
    let payload: serde_json::Value = serde_json::from_str(&written).expect("emitted payload must be valid JSON");
    assert_eq!(payload["mode"], "degraded", "the emitted payload's own mode field must agree");
}

// ─── #2310 P1: `review.context` step kind, standalone ─────────────────────

fn context_step_golden_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/review-conformance/context.json"))
}

fn context_fixture_staffing() -> ResolvedReviewRoles {
    ResolvedReviewRoles {
        probes: vec![ResolvedSeatStaffing {
            name: "fast".to_string(),
            role_id: Some("review-probe-only".to_string()),
            pm: ProfileModel { id: "probe-model".to_string(), n_ctx: Some(32_000), ..Default::default() },
            k: 1,
            passes: 1,
            max_tokens: None,
            selector: None,
            provenance: None,
        }],
        judge: ResolvedSeatStaffing {
            name: "fast".to_string(),
            role_id: None,
            pm: ProfileModel { id: "judge-model".to_string(), n_ctx: Some(32_000), ..Default::default() },
            k: 1,
            passes: 2,
            max_tokens: None,
            selector: None,
            provenance: None,
        },
        verify: None,
        request_changes: false,
        warnings: vec![],
    }
}

/// (#2310 P1) `review.context` in isolation — no graph, no launcher: a
/// fixture `Step`/`Task` in, an `Output<ReviewContext>` out, compared
/// key-for-key (sorted) against a committed golden. `context_out` names an
/// explicit tempdir path (the `plan_out`-style escape hatch — see
/// `ReviewContextStepConfig`'s own doc), so this needs no `DARKMUX_HOME`
/// scoping to stay off the operator's real `~/.darkmux`.
#[test]
fn review_context_step_matches_the_committed_golden() {
    let dir = tempfile::tempdir().expect("tempdir");
    let diff_file = dir.path().join("pr.diff");
    std::fs::write(&diff_file, "--- a/x.ts\n+++ b/x.ts\n@@ -1 +1 @@\n-old\n+new\n").expect("write diff");
    let out_path = dir.path().join("context.json");

    let step = darkmux_crew::types::Step {
        id: "review-context-step".to_string(),
        task_id: "review-context-task".to_string(),
        gate: None,
        kind: "review.context".to_string(),
        status: darkmux_crew::types::NodeStatus::default(),
        config: serde_json::json!({
            "diff_file": diff_file.display().to_string(),
            "intent_title": "Fix the thing",
            "intent_body": "A short description.",
            "case_id": "context-golden-case",
            "timeout_seconds": 45,
            "staffing": serde_json::to_value(context_fixture_staffing()).expect("staffing serializes"),
            "context_out": out_path.display().to_string(),
        }),
        started_ts: None,
        completed_ts: None,
        output: None,
    };
    let task = darkmux_crew::types::Task {
        run_on: darkmux_crew::types::default_run_on(),
        id: "review-context-task".to_string(),
        phase_id: "investigate".to_string(),
        description: "context".to_string(),
        display_name: None,
        step_ids: vec!["review-context-step".to_string()],
        depends_on: Vec::new(),
        reads: Vec::new(),
        role_id: None,
        profile_name: None,
        workdir: None,
        image: None,
    };
    let input = std::collections::BTreeMap::new();

    let kind = ReviewContextStepKind;
    let outcome = kind.run(&step, &task, &input).expect("review.context step completes on a valid fixture");

    // The step's own output is a `ref` pointer — read the file it names.
    let written = std::fs::read_to_string(&out_path).expect("reading the written context file");
    assert!(outcome.output.contains(&out_path.display().to_string()), "Step.output must ref the written path");
    let envelope: serde_json::Value = serde_json::from_str(&written).expect("context.json parses");
    assert_eq!(envelope["kind"], serde_json::json!("review.context"));
    let body = envelope.get("body").expect("envelope carries a body").clone();
    let body_sorted = sort_keys(body);

    let pretty = serde_json::to_string_pretty(&body_sorted).expect("pretty-print");
    if std::env::var("DARKMUX_REVIEW_CONFORMANCE_UPDATE_GOLDEN").is_ok() {
        std::fs::write(context_step_golden_path(), format!("{pretty}\n")).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(context_step_golden_path()).unwrap_or_else(|_| {
        panic!(
            "missing golden at {} — run with DARKMUX_REVIEW_CONFORMANCE_UPDATE_GOLDEN=1 to generate it",
            context_step_golden_path().display()
        )
    });
    assert_eq!(
        pretty.trim_end(),
        expected.trim_end(),
        "`review.context`'s output body drifted from the committed golden at {}",
        context_step_golden_path().display()
    );
}

/// (#2310 P1 fix, mutation/red-proof) A `judge_system_override` (or any of
/// its four siblings) named in `Step.config` — the shape an operator-tier
/// `~/.darkmux/mission-configs/review.json` could set — must have NO
/// effect. Before this fix these five keys were read straight off
/// `Step.config` and applied over the real `role_prompt("review-judge")`
/// text, so a user-tier config could silently replace the FROZEN judge
/// prompt (#1256). They now live ONLY on the bus-only
/// `ReviewContextTestOverrides` seam (`ReviewStepContext::
/// context_test_overrides`), reachable exclusively by in-process test code
/// that seeds the `ArtifactBus` directly — never by a config file. This
/// test calls `ReviewContextStepKind::run` (the ctx-free path with no bus
/// at all, so it can carry NO override regardless of what's stamped on
/// config) with all five `*_override` keys present in `Step.config`, and
/// asserts the resolved prompts/budget/policy are the REAL values
/// `resolve_review_context` computes — proving the config keys are dead.
#[test]
fn review_context_step_config_level_overrides_have_no_effect() {
    let dir = tempfile::tempdir().expect("tempdir");
    let diff_file = dir.path().join("pr.diff");
    std::fs::write(&diff_file, "--- a/x.ts\n+++ b/x.ts\n@@ -1 +1 @@\n-old\n+new\n").expect("write diff");
    let out_path = dir.path().join("context.json");

    let step = darkmux_crew::types::Step {
        id: "review-context-step".to_string(),
        task_id: "review-context-task".to_string(),
        gate: None,
        kind: "review.context".to_string(),
        status: darkmux_crew::types::NodeStatus::default(),
        config: serde_json::json!({
            "diff_file": diff_file.display().to_string(),
            "intent_title": "Fix the thing",
            "intent_body": "A short description.",
            "case_id": "context-override-red-proof-case",
            "timeout_seconds": 45,
            "staffing": serde_json::to_value(context_fixture_staffing()).expect("staffing serializes"),
            "context_out": out_path.display().to_string(),
            // THE ATTACK: a hand-authored `review.json` (or a user-tier
            // variant) trying to hijack the frozen prompts / budget /
            // policy through step config.
            "probe_system_override": "HIJACKED PROBE PROMPT",
            "judge_system_override": "HIJACKED JUDGE PROMPT",
            "verify_system_override": "HIJACKED VERIFY PROMPT",
            "remote_max_tokens_per_execution_override": 1,
            "judge_exhaustion_strict_override": true,
        }),
        started_ts: None,
        completed_ts: None,
        output: None,
    };
    let task = darkmux_crew::types::Task {
        run_on: darkmux_crew::types::default_run_on(),
        id: "review-context-task".to_string(),
        phase_id: "investigate".to_string(),
        description: "context".to_string(),
        display_name: None,
        step_ids: vec!["review-context-step".to_string()],
        depends_on: Vec::new(),
        reads: Vec::new(),
        role_id: None,
        profile_name: None,
        workdir: None,
        image: None,
    };
    let input = std::collections::BTreeMap::new();

    let kind = ReviewContextStepKind;
    let outcome = kind.run(&step, &task, &input).expect("review.context step completes despite the config attack");

    let written = std::fs::read_to_string(&out_path).expect("reading the written context file");
    assert!(outcome.output.contains(&out_path.display().to_string()), "Step.output must ref the written path");
    let envelope: serde_json::Value = serde_json::from_str(&written).expect("context.json parses");
    let body = envelope.get("body").expect("envelope carries a body");

    let real_judge_system = darkmux_crew::loader::role_prompt("review-judge")
        .expect("the shipped review-judge role always has a system prompt");
    let real_verify_system = darkmux_crew::loader::role_prompt("review-verify")
        .expect("the shipped review-verify role always has a system prompt");
    assert_eq!(
        body["judge_system"].as_str(),
        Some(real_judge_system.as_str()),
        "a config-level `judge_system_override` must not reach the resolved context — the frozen \
         judge prompt must survive unhijacked"
    );
    assert_ne!(body["judge_system"].as_str(), Some("HIJACKED JUDGE PROMPT"), "the attack must not land");
    assert_eq!(
        body["verify_system"].as_str(),
        Some(real_verify_system.as_str()),
        "a config-level `verify_system_override` must not reach the resolved context"
    );
    assert_ne!(body["verify_system"].as_str(), Some("HIJACKED VERIFY PROMPT"), "the attack must not land");
    assert_ne!(
        body["probe_system"].as_str(),
        Some("HIJACKED PROBE PROMPT"),
        "a config-level `probe_system_override` must not reach the resolved context"
    );
    assert_ne!(
        body["remote_max_tokens_per_execution"].as_u64(),
        Some(1),
        "a config-level `remote_max_tokens_per_execution_override` must not reach the resolved context — \
         the real `config_access` value must survive"
    );
    // `judge_exhaustion_strict_override: true` is a WEAKER red-proof (the
    // real default happens to also be resolvable as `true` in some
    // environments), so this only asserts the field type/shape round-trips
    // — the budget + prompt assertions above are what actually pin the fix.
    assert!(body["judge_exhaustion_strict"].is_boolean(), "the field must still be a real boolean, not hijacked");
}

/// (#2310 P1 mutation/red-proof) A step config missing BOTH `diff_file` and
/// `diff` must refuse by NAME — the #1269 "loud at consumption" contract —
/// never silently default to an empty diff.
#[test]
fn review_context_step_missing_diff_is_a_named_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let step = darkmux_crew::types::Step {
        id: "review-context-step".to_string(),
        task_id: "review-context-task".to_string(),
        gate: None,
        kind: "review.context".to_string(),
        status: darkmux_crew::types::NodeStatus::default(),
        config: serde_json::json!({
            "case_id": "context-missing-diff-case",
            "staffing": serde_json::to_value(context_fixture_staffing()).expect("staffing serializes"),
            "context_out": dir.path().join("context.json").display().to_string(),
        }),
        started_ts: None,
        completed_ts: None,
        output: None,
    };
    let task = darkmux_crew::types::Task {
        run_on: darkmux_crew::types::default_run_on(),
        id: "review-context-task".to_string(),
        phase_id: "investigate".to_string(),
        description: "context".to_string(),
        display_name: None,
        step_ids: vec!["review-context-step".to_string()],
        depends_on: Vec::new(),
        reads: Vec::new(),
        role_id: None,
        profile_name: None,
        workdir: None,
        image: None,
    };
    let input = std::collections::BTreeMap::new();

    let kind = ReviewContextStepKind;
    let err = kind.run(&step, &task, &input).expect_err("a config with neither diff_file nor diff must refuse");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("diff_file") && msg.contains("config.diff"),
        "the error must name BOTH accepted config keys so the fix is obvious, got: {msg}"
    );
}
