//! (#2310 P1) `ReviewContext` — the review pipeline's shared context as a
//! typed step OUTPUT, not a launcher-built bus object.
//!
//! Before this packet, `src/mission_launch_review.rs` built a
//! `ReviewStepContext` (`darkmux-lab`'s `lab::review` module) by hand —
//! reading `diff_file`, resolving the three system prompts via
//! `darkmux_crew::loader::role_prompt`, and reading two `config_access`
//! knobs — then handed the whole struct to `run_review_graph`, which seeded
//! it onto the run's `ArtifactBus` before any step ran. `ReviewContext` is
//! the DATA half of that struct — exactly the fields the `review.context`
//! step kind (`lab::review::ReviewContextStepKind`) now resolves and writes
//! as a real, hashed, provenance-stamped `Output<ReviewContext>` (see
//! `darkmux_crew::step_output`'s module doc for the envelope shape).
//!
//! **What's NOT here, and why.** `ReviewStepContext` carries two more
//! things this type deliberately excludes:
//!
//! - `chat_override`/`bundle_override` — test-only dispatch/bundling seams
//!   (`Arc<dyn Fn...>` closures). A closure cannot serialize, and a real
//!   step output must be inspectable on disk; these stay on the
//!   `ArtifactBus` under `REVIEW_CONTEXT_ARTIFACT`.
//! - `mission_id` — the run's OWN identity, minted by the launcher AFTER
//!   `mint_run_id` and threaded through as an opaque tag for this module's
//!   directly-emitted flow records. The step that PRODUCES `ReviewContext`
//!   runs BEFORE any of that has meaning to it (it doesn't emit records
//!   under a mission id itself), and a value this genuinely doesn't own
//!   has no business riding in its own typed output body.
//!
//! **Migration complete (2026-09-04, corrected from an earlier partial
//! pass).** `review.context` is the FIRST task of the investigate phase,
//! and every downstream review task (`review-bundle-task`, all three probe
//! tasks, `review-dedup-task`, `review-judge-task`, `review-verify-task`,
//! `review-synthesis-task`) formally `depends_on`/`reads`
//! `review-context-task` (`templates/builtin/mission-configs/review.json`).
//! All six review kinds (`review.bundle`/`review.probe-render`/
//! `review.dedup`/`review.judge`/`review.verify-render`/`review.synthesis`)
//! read the DATA half through `Output::<ReviewContext>::read` — via the
//! shared `lab::review::review_context_from_input` helper — rather than the
//! bus. The bus (`REVIEW_CONTEXT_ARTIFACT`) survives ONLY as the carrier of
//! the two closure test seams plus `mission_id`, exactly as this module's
//! doc above says they must; an unwired `reads` edge is now a loud,
//! by-name config error (`review_context_from_input` refuses it) rather
//! than a silent fallback to the whole bus-seeded `ReviewStepContext`. An
//! earlier pass here left five of the six kinds on the bus and reasoned
//! that wiring all five `reads` edges at once would grow the P0
//! `graph-config.json` residency golden past "one new entry" — that golden
//! only pins FIVE dispatching steps' stamped configs (model_key/identifier/
//! n_ctx), which this wiring never touches; the graph STRUCTURE golden
//! (`review_graph_3seat.json`, which does capture task `depends_on`/`reads`)
//! is the one that changed, additively, to carry the new edges.

use darkmux_crew::resourcing::ResolvedReviewRoles;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The content id [`darkmux_crew::step_output::Output::read`] checks before
/// deserializing the body — the `review.context` step kind's `provides()`
/// port label is the same string (#2301's "port labels ARE the wrapper
/// kinds" convention).
pub const REVIEW_CONTEXT_OUTPUT_KIND: &str = "review.context";

/// This step-output body's own schema version (independent of
/// [`darkmux_crew::step_output::OUTPUT_SCHEMA_VERSION`], the ENVELOPE's).
pub const REVIEW_CONTEXT_SCHEMA_VERSION: &str = "1.0";

/// Everything the review pipeline's downstream steps need to know about
/// THIS run's inputs — DATA only (see the module doc for what's
/// deliberately excluded and why). Produced once, by the `review.context`
/// step kind, from the run's raw config (`diff_file`, `intent_file`/
/// `intent_title`, `case_id`, `timeout_seconds`, the resolved seat
/// staffing) — never hand-assembled by a launcher again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct ReviewContext {
    pub schema_version: String,
    pub case_id: String,
    /// (#2310 P1) `ResolvedReviewRoles`/`ResolvedSeatStaffing` are not yet
    /// wired for TS export (`darkmux-types`' `ProfileModel`/
    /// `BundleSelector`/`ModelEndpoint` have no `ts-export` feature of
    /// their own — see this crate's Cargo.toml) — widening that is a
    /// separate, cross-crate piece of work, not P1's. The Rust type stays
    /// fully typed (`Serialize`/`Deserialize`/`PartialEq` — see
    /// `resourcing.rs`'s own doc note on the same packet); only the
    /// GENERATED TypeScript binding falls back to `unknown` for this one
    /// field until that follow-up lands.
    #[cfg_attr(feature = "ts-export", ts(type = "unknown"))]
    pub roles: ResolvedReviewRoles,
    pub intent_title: String,
    pub intent_body: String,
    pub diff: String,
    pub probe_system: String,
    #[serde(default)]
    pub probe_role_prompts: BTreeMap<String, String>,
    pub judge_system: String,
    pub verify_system: String,
    pub remote_max_tokens_per_execution: u64,
    #[serde(default)]
    pub judge_exhaustion_strict: bool,
    pub timeout_seconds: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ReviewContext {
        ReviewContext {
            schema_version: REVIEW_CONTEXT_SCHEMA_VERSION.to_string(),
            case_id: "case-1".to_string(),
            roles: ResolvedReviewRoles::default(),
            intent_title: "title".to_string(),
            intent_body: "body".to_string(),
            diff: "diff".to_string(),
            probe_system: "probe".to_string(),
            probe_role_prompts: BTreeMap::new(),
            judge_system: "judge".to_string(),
            verify_system: "verify".to_string(),
            remote_max_tokens_per_execution: 500_000,
            judge_exhaustion_strict: false,
            timeout_seconds: 3600,
        }
    }

    #[test]
    fn round_trips_through_json() {
        let ctx = sample();
        let json = serde_json::to_string(&ctx).expect("serialize");
        let back: ReviewContext = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(ctx, back, "a round trip must be lossless");
    }

    /// (#2310 P1 mutation test) Dropping a REQUIRED field from the JSON
    /// must fail the typed read BY NAME (serde's missing-field message),
    /// never silently default or surface later as a nil somewhere
    /// downstream — the whole point of a typed step output over a JSON
    /// blob (see `darkmux_crew::step_output`'s module doc).
    #[test]
    fn missing_required_field_fails_by_name() {
        let ctx = sample();
        let mut value = serde_json::to_value(&ctx).expect("serialize");
        value.as_object_mut().expect("object").remove("judge_system");
        let err = serde_json::from_value::<ReviewContext>(value).expect_err("must fail to deserialize");
        let msg = err.to_string();
        assert!(
            msg.contains("judge_system"),
            "the error must name the missing field `judge_system`, got: {msg}"
        );
    }
}
