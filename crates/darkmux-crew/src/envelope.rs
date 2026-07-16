//! `MissionEnvelope` — the standard output contract EVERY mission emits
//! (#1284 Packet 2), plus generalized finalization.
//!
//! Prior to this packet, mission finalization was a bespoke, per-mission-
//! type shape: `src/pr_review.rs::finalize_review_mission` (#1365) hand-
//! rolled the "clean run completes phases + closes the mission; anything
//! else abandons phases + closes with a reason" decision directly against
//! `ReviewEnvelope`. That logic is genuinely mission-type-agnostic — the
//! decision only ever needed an overall outcome + a reason, never
//! `ReviewEnvelope`'s review-specific fields (tier counts, judged flags,
//! …). This module extracts the AGNOSTIC part into a standard contract
//! ([`MissionEnvelope`]) plus ONE finalization function ([`finalize_mission`])
//! every future mission-type driver can reuse, while the mission-specific
//! payload rides untouched in [`MissionEnvelope::payload`].
//!
//! **Producers and consumers.** A mission driver (today: `run_review_graph`
//! via `src/pr_review.rs`'s conversion; future: any config-launched mission,
//! #1284 Packet 3+) builds a `MissionEnvelope` at the end of its run and
//! hands it to [`finalize_mission`], which drives the phase/mission
//! terminal transitions in [`crate::lifecycle`]. The SAME envelope is the
//! natural artifact for the mission board (`darkmux mission status`) and
//! the future graph-lens viewer (#1284 Packet 5) to render — both read
//! `status`/`reason`/`warnings`/`remote_budgets` without needing to
//! understand any mission type's own payload shape.
//!
//! # Status decision — who decides what, and who consumes it
//!
//! [`MissionOutcomeStatus`] has four values. A mission driver's OWN
//! conversion logic decides which one applies (this module does not
//! prescribe the mission-type-specific thresholds — see `CLAUDE.md`'s
//! `DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION` budget-policy text for the
//! review pipeline's own rules), but the SHAPE of the decision is uniform:
//!
//! - **Clean** — the mission produced its full intended output with no
//!   constrained sub-work. (Review: `env.degenerate.is_none() &&
//!   env.warnings.is_empty()`.)
//! - **Degraded** — real, postable output was produced, but some sub-stage
//!   was CONSTRAINED (a remote budget exhausted, a bounded-retry failure
//!   reduced coverage) — the run's warnings channel is non-empty but no
//!   "treat as dead" gate fired. Degraded is NOT a lesser Clean for
//!   finalization purposes — see the mapping below. (Review:
//!   `env.degenerate.is_none() && !env.warnings.is_empty()`.)
//! - **Degenerate** — the mission ran to completion but hit an
//!   operator-decision "this run produced no usable signal" gate. (Review:
//!   `env.degenerate.is_some()`.)
//! - **Error** — the mission's driver returned a hard `Err` before
//!   producing any envelope at all (a scheduling failure, a resolution
//!   failure before dispatch began).
//!
//! **Finalization consumes ONLY the status**, via
//! [`MissionOutcomeStatus::phase_outcome`]:
//!
//! | Status       | Phase outcome | Mission outcome        |
//! |--------------|---------------|-------------------------|
//! | Clean        | Complete      | Closed (real reason)   |
//! | Degraded     | Complete      | Closed (real reason)   |
//! | Degenerate   | Abandoned     | Closed (real reason)   |
//! | Error        | Abandoned     | Closed (real reason)   |
//!
//! Degraded folds into the SAME phase/mission outcome as Clean — a
//! degraded run still posted real findings (CLAUDE.md: "DEGRADED-but-
//! posted runs must keep posting"; degraded means honest LABELING, not
//! discarding). The `status` field itself is what a consumer (the board,
//! the viewer) reads to tell Clean and Degraded apart; finalization's own
//! phase/mission-status decision doesn't need to.
//!
//! Every mission ALWAYS reaches a terminal phase/mission status —
//! `finalize_mission` never leaves anything `Running`/`Active` (#1365's
//! own finding: a mid-flight failure that stays `Active` forever is
//! indistinguishable from a stuck mission in `darkmux mission status`).

use crate::lifecycle;
use crate::types::PhaseStatus;
use serde::{Deserialize, Serialize};

/// Current schema version for `MissionEnvelope` documents. Plain semver
/// applied to the DATA SHAPE — mirrors `MISSION_CONFIG_SCHEMA`
/// (`crate::mission_config`), `RULES_SCHEMA_VERSION` (`darkmux-eureka`),
/// `FLOW_SCHEMA_VERSION` (`darkmux-flow`). See `CLAUDE.md`'s "Versioning"
/// section for the additive-minor / breaking-major bump discipline.
pub const MISSION_ENVELOPE_SCHEMA: &str = "1.0";

/// The overall outcome a mission's run reached — see the module doc's
/// "Status decision" section for how each value is decided and consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MissionOutcomeStatus {
    Clean,
    Degraded,
    Degenerate,
    Error,
}

impl MissionOutcomeStatus {
    /// The phase-level outcome [`finalize_mission`] applies for every phase
    /// named in [`MissionEnvelope::phases`] — see the module doc's mapping
    /// table. Clean/Degraded both complete (a degraded run still posted
    /// real findings); Degenerate/Error both abandon.
    pub fn phase_outcome(self) -> PhaseOutcomeKind {
        match self {
            MissionOutcomeStatus::Clean | MissionOutcomeStatus::Degraded => PhaseOutcomeKind::Complete,
            MissionOutcomeStatus::Degenerate | MissionOutcomeStatus::Error => PhaseOutcomeKind::Abandoned,
        }
    }

    /// A short, human-readable default reason when the caller didn't
    /// supply one via [`MissionEnvelope::reason`] — finalization always
    /// records SOME reason on the mission-close flow record, never a bare
    /// close with no provenance.
    fn default_reason(self) -> &'static str {
        match self {
            MissionOutcomeStatus::Clean => "mission completed",
            MissionOutcomeStatus::Degraded => "mission completed (degraded)",
            MissionOutcomeStatus::Degenerate => "mission ended degenerate",
            MissionOutcomeStatus::Error => "mission errored",
        }
    }
}

/// The terminal `PhaseStatus` a phase reaches under [`finalize_mission`] —
/// a narrower enum than `crate::types::PhaseStatus` (only the two terminal
/// values finalization ever drives a phase to; `Planned`/`Running` are not
/// finalization outcomes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PhaseOutcomeKind {
    Complete,
    Abandoned,
}

/// One phase's declared finalization outcome — see [`MissionEnvelope::phases`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseOutcome {
    pub phase_id: String,
    pub outcome: PhaseOutcomeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One pipeline stage's remote token-bucket outcome — the generic shape
/// every mission-type-specific budget record (e.g. `darkmux-lab`'s
/// `RemoteBudgetRecord`) maps onto. Field-for-field identical by design so
/// a mapping is a pure struct-literal copy, never a lossy translation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteBudgetRow {
    pub stage: String,
    pub max_tokens: u64,
    pub used_tokens: u64,
    pub exhausted: bool,
    pub skipped_calls: u32,
}

/// The standard output contract every mission emits (#1284 Packet 2). See
/// the module doc for the status-decision rules and who consumes each
/// field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionEnvelope {
    pub mission_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<String>,
    pub status: MissionOutcomeStatus,
    /// Human-readable provenance for `status` — e.g. the review pipeline's
    /// `env.degenerate` text, or a hard error's `Display` — recorded on the
    /// mission-close flow record so the board never shows a bare status
    /// with no explanation. `None` falls back to
    /// [`MissionOutcomeStatus::default_reason`] at finalization time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Per-phase finalization outcomes — see [`finalize_mission`]. Ordered
    /// (a `Vec`, not a set) so a future graph lens can render phases in
    /// declaration order without a separate sort key.
    #[serde(default)]
    pub phases: Vec<PhaseOutcome>,
    /// Non-fatal run findings the operator should read — the SAME concept
    /// `ReviewEnvelope::warnings` names (a bounded-retry failure, a
    /// partially-exhausted remote budget), generalized. Empty on a clean
    /// run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_budgets: Vec<RemoteBudgetRow>,
    /// Mission-type-specific payload — `ReviewEnvelope` (or any future
    /// mission type's own envelope) maps INTO this field rather than being
    /// replaced by `MissionEnvelope`. `Null` when the caller has nothing
    /// type-specific to attach (never used for `serde_json::Value::is_null`
    /// on read — always present on write, just possibly `Null`).
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub payload: serde_json::Value,
}

impl MissionEnvelope {
    /// Construct a `MissionEnvelope` with the given `status`, deriving
    /// every named phase's outcome from [`MissionOutcomeStatus::phase_outcome`]
    /// — the common case (a mission's phases all share ONE run-level
    /// outcome, exactly like the PR-review pipeline's three phases today).
    /// A mission type whose phases genuinely diverge (some complete, some
    /// abandon, from the SAME run) constructs `phases` by hand instead of
    /// calling this constructor.
    pub fn new(mission_id: impl Into<String>, status: MissionOutcomeStatus, phase_ids: &[&str]) -> Self {
        let outcome = status.phase_outcome();
        MissionEnvelope {
            mission_id: mission_id.into(),
            schema_version: Some(MISSION_ENVELOPE_SCHEMA.to_string()),
            status,
            reason: None,
            phases: phase_ids
                .iter()
                .map(|id| PhaseOutcome { phase_id: id.to_string(), outcome, reason: None })
                .collect(),
            warnings: Vec::new(),
            remote_budgets: Vec::new(),
            payload: serde_json::Value::Null,
        }
    }
}

/// Drive every phase named in `envelope.phases` to its declared outcome,
/// then close the mission — the generalized #1365 finalization shape,
/// absorbing `src/pr_review.rs::finalize_review_mission`'s logic.
///
/// Best-effort throughout, matching the discipline `finalize_review_mission`
/// already established: a persistence hiccup here must never propagate as a
/// caller-visible error — only the mission-status/graph-lens VIEW of the run
/// would be degraded, not the run itself (the run already finished by the
/// time this is called).
///
/// (#1406) A refused phase transition is NO LONGER silently swallowed
/// (`let _`). The underlying `lifecycle` verbs are idempotent-safe via their
/// own state-machine guards, so a BENIGN no-op — the phase is already in the
/// terminal state the declared outcome asks for (e.g. a reopen re-finalizing
/// a phase a prior run already completed) — stays quiet. But a GENUINE drift
/// refusal — the phase is in a state the declared outcome can't reach (e.g. a
/// `Planned` phase asked to `Complete`, the exact #1406 bug the honest
/// per-phase derivation upstream is meant to prevent) — is surfaced as a loud
/// warning naming the phase, the intended outcome, and the refusal, instead
/// of leaving a Closed mission whose envelope disagrees with disk with no
/// signal. This function still applies the declared outcome and lets
/// `lifecycle` do the legality check; it only inspects current state to
/// classify a refusal, never to gate the transition.
///
/// (#1284 Packet 3) ALSO persists `envelope` itself via
/// `lifecycle::save_envelope` (`envelope.json` beside `mission.json`,
/// atomic-rename write like every other lifecycle save) — makes the
/// envelope a real on-disk artifact instead of constructed-and-dropped, and
/// is what makes a future graph-lens viewer (#1284 Packet 5) able to read a
/// mission's last-run outcome without re-deriving it from flow records.
/// Best-effort like everything else here: a persistence hiccup degrades
/// only the VIEW, never the run itself (already finished by this point).
pub fn finalize_mission(envelope: &MissionEnvelope) {
    for phase in &envelope.phases {
        let result = match phase.outcome {
            PhaseOutcomeKind::Complete => lifecycle::phase_complete(&phase.phase_id),
            PhaseOutcomeKind::Abandoned => lifecycle::phase_abandon(&phase.phase_id),
        };
        if let Err(e) = result {
            // (#1406) Classify the refusal: a phase ALREADY in the outcome's
            // terminal state is a benign idempotent no-op (a reopen
            // re-finalizing an already-terminal phase); anything else is real
            // drift worth a loud, named warning.
            let already_terminal = matches!(
                (phase.outcome, lifecycle::load_phase_by_id(&phase.phase_id).map(|p| p.status)),
                (PhaseOutcomeKind::Complete, Ok(PhaseStatus::Complete))
                    | (PhaseOutcomeKind::Abandoned, Ok(PhaseStatus::Abandoned))
            );
            if !already_terminal {
                eprintln!(
                    "warning: mission `{}` finalize could not drive phase `{}` to {:?}: {e:#} \
                     — phase left as-is; reconcile with `darkmux phase` verbs (#1406)",
                    envelope.mission_id, phase.phase_id, phase.outcome
                );
            }
        }
    }
    let reason = envelope
        .reason
        .clone()
        .unwrap_or_else(|| envelope.status.default_reason().to_string());
    let _ = lifecycle::mission_close_with_reasoning(&envelope.mission_id, Some(&reason));
    let _ = lifecycle::save_envelope(&envelope.mission_id, envelope);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Mission, MissionStatus, Phase, PhaseStatus};
    use std::env;
    use tempfile::TempDir;

    /// Mirrors `lifecycle::tests::CrewGuard` — isolates both the crew root
    /// (mission/phase JSON) and the flow-record sink so these tests never
    /// touch the operator's real `~/.darkmux` state.
    struct CrewGuard {
        _tmp_crew: TempDir,
        _tmp_flows: TempDir,
        prev_crew: Option<String>,
        prev_flows: Option<String>,
    }

    impl CrewGuard {
        fn new() -> Self {
            let tmp_crew = TempDir::new().unwrap();
            let tmp_flows = TempDir::new().unwrap();
            let prev_crew = env::var("DARKMUX_CREW_DIR").ok();
            let prev_flows = env::var("DARKMUX_FLOWS_DIR").ok();
            // SAFETY: serialized via #[serial_test::serial] on every caller.
            unsafe {
                env::set_var("DARKMUX_CREW_DIR", tmp_crew.path());
                env::set_var("DARKMUX_FLOWS_DIR", tmp_flows.path());
            }
            Self { _tmp_crew: tmp_crew, _tmp_flows: tmp_flows, prev_crew, prev_flows }
        }
    }

    impl Drop for CrewGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev_crew {
                    Some(v) => env::set_var("DARKMUX_CREW_DIR", v),
                    None => env::remove_var("DARKMUX_CREW_DIR"),
                }
                match &self.prev_flows {
                    Some(v) => env::set_var("DARKMUX_FLOWS_DIR", v),
                    None => env::remove_var("DARKMUX_FLOWS_DIR"),
                }
            }
        }
    }

    /// `lifecycle::save_json` is private to that module — these tests
    /// write the fixture JSON directly (same atomic-rename discipline
    /// isn't needed for a test fixture write).
    fn write_json<T: Serialize>(path: &std::path::Path, value: &T) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    }

    fn seed_mission(id: &str) -> Mission {
        let m = Mission {
            id: id.to_string(),
            description: "test mission".to_string(),
            status: MissionStatus::Active,
            phase_ids: vec!["p1".to_string(), "p2".to_string()],
            created_ts: 1_700_000_000,
            started_ts: Some(1_700_000_000),
            closed_ts: None,
            paused_ts: None,
            source_input: None,
            ticket: None,
        };
        write_json(&lifecycle::mission_path(id), &m);
        m
    }

    fn seed_phase(mission_id: &str, phase_id: &str) -> Phase {
        let p = Phase {
            id: phase_id.to_string(),
            mission_id: mission_id.to_string(),
            description: "test phase".to_string(),
            display_name: None,
            status: PhaseStatus::Running,
            created_ts: 1_700_000_000,
            started_ts: Some(1_700_000_000),
            completed_ts: None,
            abandoned_ts: None,
            task_ids: Vec::new(),
        };
        write_json(&lifecycle::phase_path(mission_id, phase_id), &p);
        p
    }

    fn phase_status(mission_id: &str, phase_id: &str) -> PhaseStatus {
        let text = std::fs::read_to_string(lifecycle::phase_path(mission_id, phase_id)).unwrap();
        serde_json::from_str::<Phase>(&text).unwrap().status
    }

    fn mission_status(mission_id: &str) -> MissionStatus {
        let text = std::fs::read_to_string(lifecycle::mission_path(mission_id)).unwrap();
        serde_json::from_str::<Mission>(&text).unwrap().status
    }

    #[serial_test::serial]
    #[test]
    fn clean_status_completes_phases_and_closes_mission() {
        let _g = CrewGuard::new();
        seed_mission("m1");
        seed_phase("m1", "p1");
        seed_phase("m1", "p2");

        let envelope = MissionEnvelope::new("m1", MissionOutcomeStatus::Clean, &["p1", "p2"]);
        finalize_mission(&envelope);

        assert_eq!(phase_status("m1", "p1"), PhaseStatus::Complete);
        assert_eq!(phase_status("m1", "p2"), PhaseStatus::Complete);
        assert_eq!(mission_status("m1"), MissionStatus::Closed);
    }

    #[serial_test::serial]
    #[test]
    fn degraded_status_completes_phases_same_as_clean() {
        // (deliverable 1's mapping table) Degraded folds into the SAME
        // phase/mission outcome as Clean — a degraded run still posted
        // real findings; degraded means honest labeling, not discarding.
        let _g = CrewGuard::new();
        seed_mission("m2");
        seed_phase("m2", "p1");
        seed_phase("m2", "p2");

        let mut envelope = MissionEnvelope::new("m2", MissionOutcomeStatus::Degraded, &["p1", "p2"]);
        envelope.warnings = vec!["remote probe seat failed after bounded retries".to_string()];
        finalize_mission(&envelope);

        assert_eq!(phase_status("m2", "p1"), PhaseStatus::Complete);
        assert_eq!(phase_status("m2", "p2"), PhaseStatus::Complete);
        assert_eq!(mission_status("m2"), MissionStatus::Closed);
    }

    #[serial_test::serial]
    #[test]
    fn degenerate_status_abandons_phases_and_closes_mission() {
        let _g = CrewGuard::new();
        seed_mission("m3");
        seed_phase("m3", "p1");
        seed_phase("m3", "p2");

        let mut envelope = MissionEnvelope::new("m3", MissionOutcomeStatus::Degenerate, &["p1", "p2"]);
        envelope.reason = Some("no bundles produced from the diff".to_string());
        finalize_mission(&envelope);

        assert_eq!(phase_status("m3", "p1"), PhaseStatus::Abandoned);
        assert_eq!(phase_status("m3", "p2"), PhaseStatus::Abandoned);
        assert_eq!(mission_status("m3"), MissionStatus::Closed);
    }

    #[serial_test::serial]
    #[test]
    fn error_status_abandons_phases_and_closes_mission() {
        let _g = CrewGuard::new();
        seed_mission("m4");
        seed_phase("m4", "p1");
        seed_phase("m4", "p2");

        let mut envelope = MissionEnvelope::new("m4", MissionOutcomeStatus::Error, &["p1", "p2"]);
        envelope.reason = Some("review errored: probe dispatch failed".to_string());
        finalize_mission(&envelope);

        assert_eq!(phase_status("m4", "p1"), PhaseStatus::Abandoned);
        assert_eq!(phase_status("m4", "p2"), PhaseStatus::Abandoned);
        assert_eq!(mission_status("m4"), MissionStatus::Closed);
    }

    #[serial_test::serial]
    #[test]
    fn payload_round_trips_a_mission_type_specific_shape() {
        let envelope = MissionEnvelope {
            payload: serde_json::json!({"confirmed": 3, "needs_check": 1}),
            ..MissionEnvelope::new("m5", MissionOutcomeStatus::Clean, &[])
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let back: MissionEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.payload["confirmed"], serde_json::json!(3));
        assert_eq!(back.payload["needs_check"], serde_json::json!(1));
    }

    #[serial_test::serial]
    #[test]
    fn finalize_mission_persists_envelope_json_beside_mission_json() {
        // (#1284 Packet 3) `envelope.json` is a real artifact, not
        // constructed-and-dropped — verifies BOTH the path (beside
        // `mission.json`, per `lifecycle::envelope_path`'s doc) and that
        // the round-tripped content matches what was finalized.
        let _g = CrewGuard::new();
        seed_mission("m6");
        seed_phase("m6", "p1");

        let mut envelope = MissionEnvelope::new("m6", MissionOutcomeStatus::Degraded, &["p1"]);
        envelope.warnings = vec!["remote probe token budget exhausted".to_string()];
        envelope.payload = serde_json::json!({"confirmed": 2});
        finalize_mission(&envelope);

        let path = lifecycle::envelope_path("m6");
        assert_eq!(path, lifecycle::mission_path("m6").parent().unwrap().join("envelope.json"));
        let persisted = lifecycle::load_envelope("m6")
            .expect("load_envelope must succeed")
            .expect("an envelope.json must exist after finalize_mission");
        assert_eq!(persisted.mission_id, "m6");
        assert_eq!(persisted.status, MissionOutcomeStatus::Degraded);
        assert_eq!(persisted.warnings, vec!["remote probe token budget exhausted".to_string()]);
        assert_eq!(persisted.payload["confirmed"], serde_json::json!(2));
    }

    #[serial_test::serial]
    #[test]
    fn load_envelope_returns_none_when_no_envelope_json_exists_yet() {
        let _g = CrewGuard::new();
        seed_mission("m7");
        assert!(lifecycle::load_envelope("m7").unwrap().is_none());
    }
}
