//! The read half of the three-state contract: **empty is never silent.**
//!
//! Every darkmux surface that reads an optional source (today: the shared
//! Redis the fleet coordinates through) has historically converted a failed
//! read into an EMPTY SUCCESS — `.ok().and_then(…).unwrap_or_default()`,
//! `unwrap_or_default()` on a join, a bare `Vec::new()` on the error arm.
//! Over HTTP the result was indistinguishable from the truth: a hub whose
//! Redis had just died served byte-identical JSON to a genuinely quiet
//! fleet. That single defect is the one behind the dead-looking seats
//! (#1483), the fleet-blind lenses (#1705) and every "looks stuck" panel —
//! the viewer could not render honesty it was never sent.
//!
//! [`SourceState`] is what the response carries instead. Four states,
//! because collapsing to three loses the distinction that actually matters:
//!
//! | state | meaning | is this a problem? |
//! |---|---|---|
//! | `ok` | the read succeeded | no |
//! | `stale` | the read failed; serving the last-known-good snapshot | yes, softly — say how old |
//! | `unavailable` | the read failed and nothing was cached | yes — the answer is incomplete |
//! | `off` | the source is not configured | **no** — this is a correct standalone machine |
//!
//! `off` vs `unavailable` is the pair worth the extra variant. A standalone
//! laptop with no fleet rows is CORRECT and must never show a warning; a hub
//! with no fleet rows is LYING. Before this module they were the same bytes.
//!
//! ## Why `detail` never carries the underlying error text
//!
//! The obvious implementation puts `e.to_string()` in `detail`. It must not:
//! a Redis error can embed the connection URL, and the env tier of
//! `redis_url()` carries an inline password (the #661 Slice 5 rule that
//! every log site in this crate already follows). The daemon may bind
//! non-loopback, and this body is rendered on a phone over a tailnet — so
//! `detail` is populated ONLY from our own literals, and the full error goes
//! to stderr where the operator already looks for it.

use serde::Serialize;

/// How completely one underlying source answered for this response.
///
/// Serializes as an internally-tagged object so a reader can switch on
/// `state` and find the variant's fields alongside it:
/// `{"state":"stale","age_ms":41200,"detail":"could not reach Redis"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SourceState {
    /// The read succeeded; this response covers the source completely.
    Ok,
    /// The read failed and the last-known-good snapshot is being served in
    /// its place. `age_ms` is how long ago that snapshot was taken — the
    /// number a client needs to decide whether to warn or to shrug.
    Stale { age_ms: u64, detail: &'static str },
    /// The read failed with nothing cached to fall back on. Whatever this
    /// source would have contributed is simply MISSING from the response;
    /// a client must not render the result as a complete picture.
    Unavailable { detail: &'static str },
    /// The source is not configured on this machine. Not a degradation:
    /// a single-machine install has no shared substrate by design, and
    /// showing it a warning would be the bug.
    Off,
}

impl SourceState {
    /// Whether this response covers the source completely — true for both
    /// `Ok` and `Off`, since an unconfigured source has nothing to omit.
    ///
    /// The predicate a client wants for "should I warn?" is the NEGATION of
    /// this, which is why it exists as one named function rather than being
    /// re-derived (subtly differently) at each call site.
    pub(crate) fn is_complete(&self) -> bool {
        matches!(self, SourceState::Ok | SourceState::Off)
    }
}

/// The `meta` object a response carries: the state of each source that
/// contributed to it, keyed by source name.
///
/// **Only sources whose state is genuinely tracked appear here.** It is
/// tempting to also emit `"local": {"state":"ok"}` for the day-file walk to
/// make the map look symmetric — but that walk does not yet track its own
/// failures (it swallows unreadable files), so the `ok` would be a claim
/// nobody verified. Publishing an unverified `ok` is the exact defect this
/// module exists to remove, one layer up. When the local walk carries a real
/// state, it gets a key here; until then, its absence is the honest answer.
///
/// `complete` is the derived "is this response the whole truth?" — the single
/// field a client needs to decide whether to warn at all, so that rendering a
/// marker never requires enumerating (and re-deriving the meaning of) every
/// state. It stays correct as sources are added: a response is complete only
/// when EVERY tracked source is.
pub(crate) fn coverage_meta(fleet: &SourceState) -> serde_json::Value {
    serde_json::json!({
        "sources": { "fleet": fleet },
        "complete": fleet.is_complete(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_and_off_are_complete_stale_and_unavailable_are_not() {
        // The four-state distinction, asserted as behavior rather than
        // trusted from the shape: `off` must read as COMPLETE (a standalone
        // machine is not degraded) while `unavailable` must not, even though
        // both produce an empty result set.
        assert!(SourceState::Ok.is_complete());
        assert!(SourceState::Off.is_complete(), "an unconfigured source is not a degradation");
        assert!(!SourceState::Unavailable { detail: "x" }.is_complete());
        assert!(!SourceState::Stale { age_ms: 1, detail: "x" }.is_complete());
    }

    #[test]
    fn states_serialize_with_a_flat_tag_the_client_can_switch_on() {
        assert_eq!(serde_json::to_value(SourceState::Ok).unwrap(), serde_json::json!({"state": "ok"}));
        assert_eq!(serde_json::to_value(SourceState::Off).unwrap(), serde_json::json!({"state": "off"}));
        assert_eq!(
            serde_json::to_value(SourceState::Stale { age_ms: 41_200, detail: "could not reach Redis" })
                .unwrap(),
            serde_json::json!({"state": "stale", "age_ms": 41_200, "detail": "could not reach Redis"})
        );
        assert_eq!(
            serde_json::to_value(SourceState::Unavailable { detail: "could not reach Redis" }).unwrap(),
            serde_json::json!({"state": "unavailable", "detail": "could not reach Redis"})
        );
    }

    #[test]
    fn coverage_meta_omits_sources_it_does_not_track() {
        // Guards the honesty rule in this module's docs: the map must NOT
        // grow a fabricated `local: ok` for symmetry's sake. If a future
        // change adds `local`, it must come with real local-failure
        // tracking — and this assertion is where that conversation starts.
        let meta = coverage_meta(&SourceState::Ok);
        assert_eq!(
            meta,
            serde_json::json!({"sources": {"fleet": {"state": "ok"}}, "complete": true})
        );
        assert!(
            meta["sources"].get("local").is_none(),
            "an untracked source must be absent, never reported as ok"
        );
    }

    #[test]
    fn complete_tracks_the_source_state_including_the_off_case() {
        // The inverted case is the one that matters: `off` must NOT set
        // `complete: false`, or every standalone machine renders a permanent
        // warning for a fleet it was never configured to have.
        assert_eq!(coverage_meta(&SourceState::Off)["complete"], serde_json::json!(true));
        assert_eq!(
            coverage_meta(&SourceState::Unavailable { detail: "x" })["complete"],
            serde_json::json!(false)
        );
        assert_eq!(
            coverage_meta(&SourceState::Stale { age_ms: 5, detail: "x" })["complete"],
            serde_json::json!(false)
        );
    }
}
