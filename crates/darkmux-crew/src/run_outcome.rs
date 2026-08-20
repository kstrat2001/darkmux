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
}

impl RunOutcome {
    pub fn is_complete(&self) -> bool {
        matches!(self, RunOutcome::Complete)
    }

    pub fn is_partial(&self) -> bool {
        matches!(self, RunOutcome::Partial { .. })
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
