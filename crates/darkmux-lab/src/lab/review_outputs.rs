//! (#2310 P2) Typed step-output BODIES for the review pipeline's remaining
//! hand-offs — bundles, probe flags (one per seat), deduped flags, judged
//! flags, verify results, and the final envelope. Mirrors
//! `review_context.rs`'s shape exactly (see that module's own doc for the
//! full design rationale this repeats): DATA only, wrapped by the
//! producing step kind via `darkmux_crew::step_output::Output::wrap`, read
//! back by a per-body helper in `review.rs` (`bundles_from_input`/
//! `probe_flags_from_inputs`/`deduped_from_input`/`judged_from_input`/
//! `verify_from_input`) that kind-checks BEFORE touching a single field —
//! a mis-wired `review.json` fails loudly by name, never a silent zero.
//!
//! **Why `ReviewEnvelope` itself is not duplicated here.** The envelope
//! (`review.envelope`, `ReviewEnvelope` in `review.rs`) is ALREADY exactly
//! the shape a typed body needs to be — `Serialize`/`Deserialize`/
//! `PartialEq` (added this packet) — so `ReviewSynthesisStepKind` wraps it
//! directly (`Output::wrap(REVIEW_ENVELOPE_OUTPUT_KIND, env, producer)`)
//! rather than through a second wrapper struct defined here.
//!
//! **`BundleSetOutput` is a small, deliberate widening of the brief's
//! literal `Output<Vec<BundleInput>>`.** The bundle step's run-time work
//! (`ReviewBundleStepKind::run_streaming`) resolves three facts together —
//! the bundle set itself, the bundler's own per-file decline accounting
//! (`BundleSkipReport`), and whether a pinned `--bundler` plugin declined
//! and fell back to the built-in one — and `ReviewSynthesisStepKind`'s
//! zero-bundle degenerate gate (`classify_zero_bundle_degenerate`) needs
//! ALL THREE, not just the bundle count. A bare `Output<Vec<BundleInput>>`
//! has nowhere for the other two to ride once the shared envelope artifact
//! is gone, so this wraps all three in one small struct instead — the
//! `bundles` FIELD is the identical `Vec<BundleInput>` body the P0 golden
//! already pins (see this module's own tests + `bundles.json`'s diff in
//! the P2 report for why this reads as "the same body, one level deeper,"
//! not a behavior change).

use super::bundle::BundleSkipReport;
use super::review::{BundleInput, DedupStats, ProbeFlag, VerifyRecord};
use darkmux_crew::remote_budget::RemoteBudgetRecord;
use darkmux_crew::run_record::MemberRecord;
use serde::{Deserialize, Serialize};

/// [`BundleSetOutput`]'s content id.
pub const BUNDLE_SET_OUTPUT_KIND: &str = "review.bundles";
/// [`ProbeSeatOutput`]'s content id — one per probe TASK (#1512: one role,
/// one task, one dispatch), fanned in by dedup.
pub const PROBE_SEAT_OUTPUT_KIND: &str = "review.probe-flags";
/// [`DedupOutput`]'s content id.
pub const DEDUP_OUTPUT_KIND: &str = "review.deduped-flags";
/// [`JudgeOutput`]'s content id.
pub const JUDGE_OUTPUT_KIND: &str = "review.judged-flags";
/// [`VerifyOutput`]'s content id.
pub const VERIFY_OUTPUT_KIND: &str = "review.verify-results";
/// The final [`super::review::ReviewEnvelope`]'s content id when wrapped as
/// a typed step output by `ReviewSynthesisStepKind`.
pub const REVIEW_ENVELOPE_OUTPUT_KIND: &str = "review.envelope";

/// This module's bodies all share one schema version — every field on
/// every one of them is either a plain scalar or a type this crate already
/// owns and versions on its own terms (`ProbeFlag`/`JudgedFlag`/
/// `MemberRecord`/`RemoteBudgetRecord`/…), so there is no independent
/// evolution story that would justify per-body constants yet. Split this
/// the day one body's shape needs to move without the others.
pub const REVIEW_OUTPUTS_SCHEMA_VERSION: &str = "1.0";

/// `review.bundle`'s typed output — see this module's own doc for why this
/// is a small wrapper around the bundle set rather than a bare
/// `Output<Vec<BundleInput>>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct BundleSetOutput {
    pub schema_version: String,
    #[cfg_attr(feature = "ts-export", ts(type = "unknown"))]
    pub bundles: Vec<BundleInput>,
    #[serde(default)]
    #[cfg_attr(feature = "ts-export", ts(type = "unknown"))]
    pub skip: Option<BundleSkipReport>,
    #[serde(default)]
    pub bundler_fallback: Option<String>,
}

/// One probe TASK's typed output — `review.probe-collect`'s body. Carries
/// this seat's raw flags (already attributed to a bundle_id/fact_family —
/// see `crate::lab::review::reconstruct_probe_seat`) plus every per-seat
/// fact that used to accumulate on the run-scoped `ArtifactBus`
/// (`MemberRecord`, a dispatch-failure warning, the fired/error counts an
/// all-draws-failed degenerate gate needs, and this seat's own contribution
/// to the probe stage's ONE shared remote-token bucket). `ReviewDedupStepKind`
/// fans in every probe task's output and folds these exactly as
/// `reconstruct_probe_stage` (retired this packet) used to — see
/// `crate::lab::review::fold_probe_seats`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct ProbeSeatOutput {
    pub schema_version: String,
    pub seat: String,
    pub identifier: String,
    pub remote: bool,
    #[serde(default)]
    pub endpoint_host: Option<String>,
    #[cfg_attr(feature = "ts-export", ts(type = "unknown"))]
    pub flags: Vec<ProbeFlag>,
    #[serde(default)]
    #[cfg_attr(feature = "ts-export", ts(type = "unknown"))]
    pub member: Option<MemberRecord>,
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Draws that actually fired (a first-attempt remote-budget skip is
    /// never a draw — see `reconstruct_probe_seat`'s own doc).
    pub fired: u32,
    pub errors: u32,
    #[serde(default)]
    pub first_error: Option<String>,
    pub retries: u32,
    pub remote_tokens: u64,
    pub remote_calls: u32,
    pub remote_skips: u32,
}

/// `review.dedup`'s typed output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct DedupOutput {
    pub schema_version: String,
    #[cfg_attr(feature = "ts-export", ts(type = "unknown"))]
    pub flags: Vec<ProbeFlag>,
    #[cfg_attr(feature = "ts-export", ts(type = "unknown"))]
    pub stats: DedupStats,
    /// The probe stage's per-seat `MemberRecord`s, folded from every
    /// fanned-in [`ProbeSeatOutput`] — what `ReviewSynthesisStepKind` folds
    /// into the final envelope's `members` alongside the judge/verify rows.
    #[serde(default)]
    #[cfg_attr(feature = "ts-export", ts(type = "unknown"))]
    pub members: Vec<MemberRecord>,
    #[serde(default)]
    pub warnings: Vec<String>,
    /// The probe stage's ONE combined remote-budget row (`stage: "probe"`),
    /// folded across every seat that drew from the shared per-execution
    /// bucket — `None` when no probe seat is remote.
    #[serde(default)]
    #[cfg_attr(feature = "ts-export", ts(type = "unknown"))]
    pub remote_budget: Option<RemoteBudgetRecord>,
    /// The all-draws-failed honesty gate, folded across every seat — see
    /// `fold_probe_seats`'s doc.
    #[serde(default)]
    pub degenerate: Option<String>,
    #[serde(default)]
    pub probe_retries: usize,
    pub raw_flags: usize,
}

/// `review.judge`'s typed output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct JudgeOutput {
    pub schema_version: String,
    #[cfg_attr(feature = "ts-export", ts(type = "unknown"))]
    pub judged: Vec<super::review::JudgedFlag>,
    #[serde(default)]
    #[cfg_attr(feature = "ts-export", ts(type = "unknown"))]
    pub member: Option<MemberRecord>,
    #[serde(default)]
    #[cfg_attr(feature = "ts-export", ts(type = "unknown"))]
    pub remote_budget_rows: Vec<RemoteBudgetRecord>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub degenerate: Option<String>,
}

/// `review.verify-collect`'s typed output. `results` is one
/// [`VerifyRecord`] per CONFIRMED flag, in judged-docket confirmed order —
/// empty on every no-dispatch path (no verify seat staffed, a doomed run,
/// zero confirmed findings), matching the render step's own empty-collection
/// short-circuit. `ReviewSynthesisStepKind` zips this back onto the judged
/// docket's confirmed flags (see `apply_verify_records`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct VerifyOutput {
    pub schema_version: String,
    #[cfg_attr(feature = "ts-export", ts(type = "unknown"))]
    pub results: Vec<VerifyRecord>,
    #[serde(default)]
    #[cfg_attr(feature = "ts-export", ts(type = "unknown"))]
    pub member: Option<MemberRecord>,
    #[serde(default)]
    pub warning: Option<String>,
    #[serde(default)]
    #[cfg_attr(feature = "ts-export", ts(type = "unknown"))]
    pub budget_row: Option<RemoteBudgetRecord>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle() -> BundleInput {
        BundleInput {
            id: "b1".into(),
            fact_family: "ff".into(),
            code: "code".into(),
            probe_code: "probe".into(),
            facts: vec!["fact".into()],
            manifest: vec![],
        }
    }

    /// (#2310 P2 mutation test) Missing a required field on EVERY body must
    /// fail the typed read by field name — the whole point of a typed step
    /// output over a JSON blob (see `darkmux_crew::step_output`'s module
    /// doc, and `review_context.rs`'s identical test for the P1 body).
    #[test]
    fn missing_required_field_fails_by_name_on_every_body() {
        let bundles = BundleSetOutput {
            schema_version: REVIEW_OUTPUTS_SCHEMA_VERSION.to_string(),
            bundles: vec![bundle()],
            skip: None,
            bundler_fallback: None,
        };
        let mut v = serde_json::to_value(&bundles).unwrap();
        v.as_object_mut().unwrap().remove("bundles");
        let err = serde_json::from_value::<BundleSetOutput>(v).unwrap_err().to_string();
        assert!(err.contains("bundles"), "{err}");

        let probe = ProbeSeatOutput {
            schema_version: REVIEW_OUTPUTS_SCHEMA_VERSION.to_string(),
            seat: "review-probe-high".into(),
            identifier: "m".into(),
            remote: false,
            endpoint_host: None,
            flags: vec![],
            member: None,
            warnings: vec![],
            fired: 0,
            errors: 0,
            first_error: None,
            retries: 0,
            remote_tokens: 0,
            remote_calls: 0,
            remote_skips: 0,
        };
        let mut v = serde_json::to_value(&probe).unwrap();
        v.as_object_mut().unwrap().remove("identifier");
        let err = serde_json::from_value::<ProbeSeatOutput>(v).unwrap_err().to_string();
        assert!(err.contains("identifier"), "{err}");

        let dedup = DedupOutput {
            schema_version: REVIEW_OUTPUTS_SCHEMA_VERSION.to_string(),
            flags: vec![],
            stats: DedupStats { raw: 0, deduped: 0 },
            members: vec![],
            warnings: vec![],
            remote_budget: None,
            degenerate: None,
            probe_retries: 0,
            raw_flags: 0,
        };
        let mut v = serde_json::to_value(&dedup).unwrap();
        v.as_object_mut().unwrap().remove("stats");
        let err = serde_json::from_value::<DedupOutput>(v).unwrap_err().to_string();
        assert!(err.contains("stats"), "{err}");

        // (#2310 P2 review finding I3) `JudgeOutput` was missing from this
        // otherwise-exhaustive "every body" test — added so a regression on
        // its own required field (`judged`) fails here rather than only
        // being caught (or missed) downstream at `find_by_kind`'s peek.
        let judge = JudgeOutput {
            schema_version: REVIEW_OUTPUTS_SCHEMA_VERSION.to_string(),
            judged: vec![],
            member: None,
            remote_budget_rows: vec![],
            warnings: vec![],
            degenerate: None,
        };
        let mut v = serde_json::to_value(&judge).unwrap();
        v.as_object_mut().unwrap().remove("judged");
        let err = serde_json::from_value::<JudgeOutput>(v).unwrap_err().to_string();
        assert!(err.contains("judged"), "{err}");

        let verify = VerifyOutput {
            schema_version: REVIEW_OUTPUTS_SCHEMA_VERSION.to_string(),
            results: vec![],
            member: None,
            warning: None,
            budget_row: None,
        };
        let mut v = serde_json::to_value(&verify).unwrap();
        v.as_object_mut().unwrap().remove("results");
        let err = serde_json::from_value::<VerifyOutput>(v).unwrap_err().to_string();
        assert!(err.contains("results"), "{err}");
    }

    #[test]
    fn round_trips_through_json() {
        let bundles =
            BundleSetOutput { schema_version: "1.0".into(), bundles: vec![bundle()], skip: None, bundler_fallback: None };
        let back: BundleSetOutput = serde_json::from_str(&serde_json::to_string(&bundles).unwrap()).unwrap();
        assert_eq!(bundles, back);
    }
}
