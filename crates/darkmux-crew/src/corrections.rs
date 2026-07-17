//! (#849) The persisted adjudication corrections — darkmux's SECOND memory
//! kind, and the reader both consumers share.
//!
//! A correction is what the user's reviewer recorded when they adjudicated a
//! dispatch's QA findings: `darkmux flow note --session-id <sid> --text
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

use serde::Serialize;
use std::collections::HashSet;
use std::path::PathBuf;

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

/// Scan the most-recent `days` day-files of the flow trail for adjudication
/// corrections, returned **oldest→newest**.
///
/// `sessions` scopes the read:
/// * `Some(set)` — EXACT-set match on `session_id`. Exact, never a prefix: a
///   `mission-run-auth-` prefix would bleed a sibling mission whose id is a
///   hyphen-extension (`mission-run-auth-v2-s1` starts with it), which is the
///   #849 regression the brief-injection path's tests pin.
/// * `None` — every session in the window.
///
/// Best-effort: unreadable dirs/files and unparsable lines are skipped, never
/// surfaced as an error. Neither deduped nor capped — each consumer applies its
/// own policy on top ([`crate::lessons`]-style curation is not a thing here;
/// the brief path dedups + budgets, the `list` verb shows what's recorded).
pub fn scan(days: usize, sessions: Option<&HashSet<String>>) -> Vec<Correction> {
    // An empty scope can match nothing — skip the IO entirely.
    if sessions.is_some_and(|s| s.is_empty()) {
        return Vec::new();
    }
    let flows_dir = darkmux_types::config_access::flows_dir();
    let Ok(entries) = std::fs::read_dir(&flows_dir) else {
        return Vec::new();
    };
    let mut day_files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .collect();
    // Day-files are date-named, so a lexical sort is chronological.
    day_files.sort();
    // The most-recent `days`, restored to oldest→newest within the window.
    let recent: Vec<PathBuf> = day_files
        .iter()
        .rev()
        .take(days)
        .rev()
        .cloned()
        .collect();

    let mut out: Vec<Correction> = Vec::new();
    for day in &recent {
        let Ok(raw) = std::fs::read_to_string(day) else {
            continue;
        };
        for line in raw.lines() {
            let Ok(r) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if r.get("action").and_then(|v| v.as_str()) != Some("note")
                || r.get("source").and_then(|v| v.as_str()) != Some("adjudication")
            {
                continue;
            }
            let Some(sid) = r.get("session_id").and_then(|v| v.as_str()) else {
                continue;
            };
            if sessions.is_some_and(|set| !set.contains(sid)) {
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
