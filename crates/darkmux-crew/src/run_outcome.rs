//! `RunOutcome` — a generic, reusable three-state summary of how completely
//! a run covered its own docket. Every mission that partitions its work into
//! independently-judgeable items (a review's flags, a future coder-phase's
//! own docket) can answer "did this run finish everything it set out to do"
//! the same way, instead of each mission inventing its own binary
//! degenerate/healthy flag.
//!
//! **#1877 item 5, motivated by #1876.** A budget-exhausted judge stage had
//! judged 123 of 134 flags — 7 confirmed findings and 67 needs-check
//! rulings, complete, with rulings and evidence on every one — and because
//! the only outcome shape available was binary (`degenerate: Option<String>`),
//! the run discarded ALL 123 completed judgements and posted "the review
//! produced no signal." That was a fully accurate [`RunOutcome::Partial`]
//! mislabeled [`RunOutcome::Empty`], because there was nowhere else for it
//! to go. `RunOutcome` gives every future mission that third state from day
//! one, instead of leaving the next mission to hit the same wall.
//!
//! Deliberately NOT opinionated about what counts as a "reason," or about
//! HOW a caller decides which variant its own run landed in — a mission
//! computes its own outcome via its own predicates over its own data (flags
//! judged, usable rulings, remote-budget rows, …) and constructs the
//! matching variant. This type only names the three shapes an outcome picks
//! from; the mapping stays with the mission that owns the data. See
//! `darkmux_lab::lab::review::review_outcome` for review's own mapping.
//!
//! **Gap closed (#1877 item 2):** `MissionEnvelope` (`crew::envelope`) now
//! carries an `outcome: Option<RunOutcome>` field, and its own
//! `MissionOutcomeStatus` (`Clean`/`Degraded`/`Degenerate`/`Error`) is
//! derived FROM `outcome` (`MissionOutcomeStatus::from_outcome`,
//! `MissionEnvelope::from_outcome`) rather than hand-computed a second time.
//! Review is the first (and, as of this PR, only) adopter —
//! `review_result_to_mission_envelope` (`src/mission_launch_review.rs`)
//! builds its `RunOutcome` via `darkmux_lab::lab::review::
//! review_mission_outcome` and hands it straight to `MissionEnvelope::
//! from_outcome`, so "partial" is a typed `RunOutcome::Partial { reasons }`
//! on the envelope, not only an untyped warning string. See
//! `crate::envelope`'s module doc (the "`outcome` — the typed source"
//! section) for the full mapping and why `MissionOutcomeStatus` itself
//! stays four-valued (`Partial` collapses into `Degraded` for every
//! existing consumer, a stated decision, not an oversight).
//!
//! **Still open, deliberately deferred (#1877 item 5, next step):**
//! coder-phase (`src/coder_phase.rs`) does not construct a `RunOutcome`
//! yet — it still builds `status` directly via `MissionEnvelope::new`. Its
//! own docket (files touched, QA findings addressed) is exactly the kind
//! of partitioned work this type exists to describe, but wiring it up is
//! its own acceptance test.
//!
//! # Forward-compat leniency (#1881)
//!
//! [`RunOutcome::Unknown`] closes a gap #1877 QA flagged but deliberately
//! left open (`crates/darkmux-crew/src/envelope.rs`'s
//! `MISSION_ENVELOPE_SCHEMA` doc, pre-#1881): before this variant existed,
//! a `state` value this binary didn't recognize failed the deserialize of
//! the ENTIRE `MissionEnvelope` — not just `outcome` — because
//! `#[serde(tag = "state")]` has no unknown-variant tolerance on its own.
//! `#[serde(other)]` on a unit fallback variant fixes this even though the
//! OTHER variants (`Partial`, `Empty`) carry data: serde buffers the whole
//! tagged value before dispatching on `state`, so a sibling's fields never
//! constrain what the fallback arm can accept. Verified directly by this
//! module's own `an_unrecognized_state_degrades_to_unknown_instead_of_failing_to_parse`
//! test (below), and again at the whole-`MissionEnvelope` level by
//! `envelope.rs`'s `an_unrecognized_outcome_variant_degrades_to_unknown_and_the_rest_of_the_document_still_parses`.
//!
//! **What this does NOT do:** the original `state` string is discarded
//! (serde's `other` catch-all buffers-then-drops unmatched content) — a
//! consumer that needs to know WHAT the unrecognized state actually was
//! has to go read the raw `envelope.json` off disk, this typed value can't
//! say. And `Unknown` is never read by `mission_run_status`'s `RunStatus`
//! derivation (`crates/darkmux-serve/src/runs.rs`) — `outcome` never has
//! been (#1877 item 4); the run's `status` field alone remains
//! authoritative for that. A known `status` paired with an unrecognized
//! `outcome` therefore still renders per that known status, honestly —
//! see `MissionOutcomeStatus`'s own forward-compat note in `envelope.rs`
//! for the field `RunStatus` DOES read leniently.
//!
//! **Load-then-save hazard, not yet reachable (#1881, QA-caught).** Because
//! `Unknown` re-serializes as `{"state":"unknown"}` (the ordinary
//! `Serialize` derive, unaware anything was discarded on read), a future
//! code path that LOADS an envelope and then SAVES it back out — a
//! `mission migrate`-style rewrite, a round-trip through an editing tool —
//! would silently overwrite a newer machine's real (if unrecognized) value
//! with this binary's own ignorance of it, permanently losing the original
//! `state`. No such path exists today: `finalize_mission`
//! (`crates/darkmux-crew/src/envelope.rs`) is the only `save_envelope`
//! caller, and every one of its callers hands it a freshly-built
//! `MissionEnvelope`, never a loaded one. Worth writing down before that
//! stops being true, rather than after.

use serde::{Deserialize, Serialize};

/// How completely a run covered its own docket — see the module doc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RunOutcome {
    /// Every unit of work in the docket produced a real outcome — nothing
    /// was skipped, refused, or left unjudged.
    Complete,
    /// SOME of the docket produced a usable outcome, but not all of it.
    /// `reasons` names each shortfall in plain language with REAL numbers
    /// drawn from the run's own data — never a fixed string, so a caller
    /// can't accidentally ship a canned reason that drifts from the count
    /// it describes. A partial outcome is never worthless (its usable
    /// portion is real signal, and renders normally) and never a clean pass
    /// either (the shortfall is loud, not folded into a healthy run).
    Partial { reasons: Vec<String> },
    /// Nothing in the docket produced a usable outcome — the run has no
    /// signal worth rendering. `reason` is the caller's own honest "why."
    Empty { reason: String },
    /// (#1881) A `state` this binary does not recognize — written by a
    /// NEWER darkmux than this one. Forward-compat READ-ONLY fallback: no
    /// writer in this codebase ever constructs this variant on purpose
    /// (every driver holds a real, freshly-computed `RunOutcome`; this only
    /// ever arises from deserializing someone else's envelope). See the
    /// module doc's "Forward-compat leniency" section.
    #[serde(other)]
    Unknown,
}

impl RunOutcome {
    pub fn is_complete(&self) -> bool {
        matches!(self, RunOutcome::Complete)
    }

    pub fn is_partial(&self) -> bool {
        matches!(self, RunOutcome::Partial { .. })
    }

    /// (#1881) `true` only for the deserialize-time forward-compat
    /// fallback — see [`RunOutcome::Unknown`].
    pub fn is_unknown(&self) -> bool {
        matches!(self, RunOutcome::Unknown)
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, RunOutcome::Empty { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_helpers_match_their_own_variant_only() {
        let complete = RunOutcome::Complete;
        assert!(complete.is_complete());
        assert!(!complete.is_partial());
        assert!(!complete.is_empty());

        let partial = RunOutcome::Partial { reasons: vec!["11 of 134 flags went unjudged".to_string()] };
        assert!(!partial.is_complete());
        assert!(partial.is_partial());
        assert!(!partial.is_empty());

        let empty = RunOutcome::Empty { reason: "no usable ruling on any flag".to_string() };
        assert!(!empty.is_complete());
        assert!(!empty.is_partial());
        assert!(empty.is_empty());

        let unknown = RunOutcome::Unknown;
        assert!(!unknown.is_complete());
        assert!(!unknown.is_partial());
        assert!(!unknown.is_empty());
        assert!(unknown.is_unknown());
        assert!(!complete.is_unknown());
    }

    /// (#1881) A `state` this binary doesn't recognize degrades to
    /// `Unknown` via `#[serde(other)]` instead of failing the deserialize —
    /// even though the sibling `Partial`/`Empty` variants carry data. Extra
    /// unrecognized fields alongside the unrecognized `state` are silently
    /// discarded, same as any `#[serde(other)]` catch-all.
    #[test]
    fn an_unrecognized_state_degrades_to_unknown_instead_of_failing_to_parse() {
        let json = r#"{"state":"throttled","some_future_field":123}"#;
        let outcome: RunOutcome = serde_json::from_str(json).expect("must degrade, not error");
        assert_eq!(outcome, RunOutcome::Unknown);
    }

    /// The `state` tag round-trips through JSON — this is the shape a
    /// future envelope/artifact would serialize, so the tag name and casing
    /// are load-bearing for any downstream consumer, not just an internal
    /// implementation detail.
    #[test]
    fn serde_round_trips_every_variant_with_the_tagged_shape() {
        for outcome in [
            RunOutcome::Complete,
            RunOutcome::Partial { reasons: vec!["a".to_string(), "b".to_string()] },
            RunOutcome::Empty { reason: "dead run".to_string() },
        ] {
            let json = serde_json::to_string(&outcome).unwrap();
            let back: RunOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(back, outcome);
        }
        let json = serde_json::to_string(&RunOutcome::Complete).unwrap();
        assert_eq!(json, r#"{"state":"complete"}"#);
        let json = serde_json::to_string(&RunOutcome::Partial { reasons: vec!["x".to_string()] }).unwrap();
        assert_eq!(json, r#"{"state":"partial","reasons":["x"]}"#);
        let json = serde_json::to_string(&RunOutcome::Empty { reason: "y".to_string() }).unwrap();
        assert_eq!(json, r#"{"state":"empty","reason":"y"}"#);
    }
}
