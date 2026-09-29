//! (#849) The persisted adjudication corrections — darkmux's SECOND memory
//! kind, and the reader both consumers share.
//!
//! A correction is what the user's reviewer recorded when they adjudicated a
//! dispatch's QA findings: `darkmux flow note --execution <id> --text
//! "<verdict · what you overrode · why>" --source adjudication`. Unlike the
//! authored [`crate::lessons`] store, corrections are never hand-authored as a
//! memory entry — they are RECORDED BY THE REVIEW PATH as flow records, and the
//! flow trail is their only home. That is why this module is read-only: there
//! is no `add` here by design (#1426, decision 17).
//!
//! Two consumers read them, and they must not drift apart (the cross-system
//! contract discipline in CLAUDE.md — a subsystem's unit tests can't catch a
//! misalignment between subsystems):
//!
//! * the coder-brief injection path (`src/coder_phase.rs`), which carries a
//!   mission's prior corrections forward into the next coder dispatch so a
//!   correction made once is never re-derived;
//! * `darkmux memory correction list` (#1426), the first verb these have ever
//!   had.
//!
//! [`scan`] is the single definition of "what a correction is" that both read,
//! so the verb can never show the operator a different set than the one the
//! brief actually injects.
//!
//! Storage shape: the flow trail is per-day JSONL. A correction is a record
//! with `action=note`, `source=adjudication`, and a `session_id`. Reads are
//! best-effort by design — any IO/parse problem reads as "no corrections"
//! rather than an error, because the injection path must never fail a dispatch
//! over an unreadable day-file.

use darkmux_types::session_id::{SessionId, SessionKind};
use serde::Serialize;
use std::collections::HashSet;

/// How many of the most-recent day-files a correction scan reads. Corrections
/// are carried forward WITHIN a mission's working window; an unbounded scan
/// would grow with the whole flow trail for no benefit.
pub const ADJUDICATION_LOOKBACK_DAYS: usize = 7;

/// One recorded adjudication correction, as it sits in the flow trail.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Correction {
    /// The flow record's timestamp (RFC3339, as written).
    pub ts: String,
    /// The dispatch session the reviewer adjudicated.
    pub session_id: String,
    /// The correction text — the `--text` the reviewer recorded.
    pub text: String,
}

/// The coder runs of some phases of one mission: each phase's
/// [`SessionKind::Phase`] session. What the coder-brief path and a mission's
/// debrief scope their reads to.
#[derive(Debug, Clone)]
pub struct PhaseSessions {
    mission: String,
    phases: HashSet<String>,
}

impl PhaseSessions {
    pub fn new(mission: &str, phases: impl IntoIterator<Item = String>) -> Self {
        Self { mission: mission.to_string(), phases: phases.into_iter().collect() }
    }

    pub fn is_empty(&self) -> bool {
        self.phases.is_empty()
    }

    /// Whether a record's `session_id` (in its current or its pre-4.0
    /// spelling) is one of these phases' coder runs. An exact phase match,
    /// never a prefix: a sibling mission whose id is a hyphen-extension of
    /// this one (`auth` and `auth-v2`) never bleeds in, the #849 regression
    /// the brief-injection path's tests pin.
    pub fn admits(&self, session_id: &str) -> bool {
        SessionId::parse_legacy(session_id, Some(&self.mission)).is_some_and(|s| {
            s.mission_id() == Some(self.mission.as_str())
                && matches!(s.kind(), SessionKind::Phase { phase } if self.phases.contains(phase))
        })
    }
}

/// Which corrections a scan reads.
#[derive(Debug, Clone, Copy)]
pub enum Scope<'a> {
    /// Every correction in the window.
    All,
    /// The corrections recorded against exactly this session id.
    Session(&'a str),
    /// The corrections recorded against these phases' coder runs.
    Phases(&'a PhaseSessions),
}

impl Scope<'_> {
    fn admits(&self, session_id: &str) -> bool {
        match self {
            Scope::All => true,
            Scope::Session(sid) => *sid == session_id,
            Scope::Phases(p) => p.admits(session_id),
        }
    }
}

/// Scan the most-recent `days` day-files of the flow trail for adjudication
/// corrections within `scope`, returned **oldest→newest**.
///
/// Best-effort: unreadable dirs/files and unparsable lines are skipped, never
/// surfaced as an error. Neither deduped nor capped — each consumer applies its
/// own policy on top ([`crate::lessons`]-style curation is not a thing here;
/// the brief path dedups + budgets, the `list` verb shows what's recorded).
pub fn scan(days: usize, scope: Scope<'_>) -> Vec<Correction> {
    // An empty scope can match nothing — skip the IO entirely.
    if matches!(scope, Scope::Phases(p) if p.is_empty()) {
        return Vec::new();
    }
    let flows_dir = darkmux_types::config_access::flows_dir();
    // The most-recent `days`, oldest→newest within the window.
    let mut recent = darkmux_flow::reader::recent_day_files(&flows_dir, days);
    recent.reverse();

    let mut out: Vec<Correction> = Vec::new();
    for day in &recent {
        for r in darkmux_flow::reader::day_file_records(day) {
            if darkmux_flow::reader::action_of(&r) != Some(darkmux_flow::FlowAction::OperatorNote)
                || r.get("source").and_then(|v| v.as_str()) != Some("adjudication")
            {
                continue;
            }
            let Some(sid) = r.get("session_id").and_then(|v| v.as_str()) else {
                continue;
            };
            if !scope.admits(sid) {
                continue;
            }
            let text = r
                .get("handle")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if text.is_empty() {
                continue;
            }
            out.push(Correction {
                ts: r
                    .get("ts")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                session_id: sid.to_string(),
                text: text.to_string(),
            });
        }
    }
    out
}

#[cfg(test)]
#[path = "corrections_tests.rs"]
mod corrections_tests;
