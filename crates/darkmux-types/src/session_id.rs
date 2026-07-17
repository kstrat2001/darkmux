//! (#1436) Canonical session-id minting — ONE shape, ONE helper.
//!
//! A session id is the string that ties a family of flow records together:
//! the viewer joins records by it, dispatch-liveness bookends pair on it,
//! presence keys on it, the mission-graph events panel scopes to it. Every
//! consumer treats the id OPAQUELY (equality match) — the internal structure
//! is a producer-side convention, never a parse contract.
//!
//! Canonical shape: `{kind}-{owner}` or `{kind}-{owner}-{disambiguator}`,
//! hyphen-delimited. Pre-#1436 the tree minted these strings in 16 distinct
//! inline `format!` shapes, including colon-delimited forms (`mission:{id}`,
//! `task:{id}`, `step:{id}`) that broke the hyphen convention. #1436 routes
//! every mint through this one module so the shape is defined in exactly one
//! place and the post-2.0 dispatch core inherits a single helper instead of
//! centralizing a format zoo.
//!
//! The `mission-run-` prefix survives for the coder-phase dispatch id
//! ([`mission_run`]) even though the `mission run` verb itself retired in 2.0
//! (#1426, ship-4): the viewer's mission lens groups a run's records on that
//! prefix, the bundled demo data carries it, and existing flow archives are
//! already stamped with it — changing the bytes would strand all three. It is
//! a stable id prefix now, not a live verb name.

/// The core mint. Joins `kind`, `owner`, and (when non-empty) `disambiguator`
/// with hyphens. An empty `disambiguator` yields `{kind}-{owner}` so the
/// two-part forms (`mission-{id}`) stay free of a trailing separator.
pub fn session_id(kind: &str, owner: &str, disambiguator: &str) -> String {
    if disambiguator.is_empty() {
        format!("{kind}-{owner}")
    } else {
        format!("{kind}-{owner}-{disambiguator}")
    }
}

/// Coder-phase dispatch id (worktree → coder → verify). Stable across the
/// `mission launch coder-phase` path and the `mission finalize`/`mission abort`
/// verbs that finish or tear down the same gate-held run — the id is what
/// ties every record of one phase's run together. Byte-identical to the
/// retired `mission run`'s own `mission-run-{mission}-{phase}` so the viewer
/// lens, demo data, and legacy archives keep grouping it.
pub fn mission_run(mission_id: &str, phase_id: &str) -> String {
    session_id("mission-run", mission_id, phase_id)
}

/// Mission-lifecycle record id (start/close/pause/resume transitions, the
/// #849 adjudication-note channel). Was `mission:{id}` pre-#1436.
pub fn mission(mission_id: &str) -> String {
    session_id("mission", mission_id, "")
}

/// Per-phase fleet-dispatch id (`mission dispatch` fan-out onto the work
/// stream). The disambiguator carries the phase id plus a per-dispatch
/// microsecond stamp and the fan-out index, so two phases dispatched in the
/// same batch — or the same phase re-dispatched — never collide.
pub fn mission_phase_dispatch(
    mission_id: &str,
    phase_id: &str,
    dispatch_micros: u128,
    idx: usize,
) -> String {
    session_id(
        "mission",
        mission_id,
        &format!("phase-{phase_id}-{dispatch_micros}-{idx}"),
    )
}

/// Task-scoped step-lifecycle record id (#1399). Was `task:{id}` pre-#1436;
/// the mission-graph page's events-panel scoping matches on this exact form,
/// updated in lockstep in the same change.
pub fn task(task_id: &str) -> String {
    session_id("task", task_id, "")
}

/// Step-scoped default dispatch session id (a `dispatch.internal` step with
/// no caller-supplied session id). Was `step:{id}` pre-#1436; the
/// mission-graph page indexes this form to map a record back to its step.
pub fn step(step_id: &str) -> String {
    session_id("step", step_id, "")
}

/// Phase-estimate narration flow id (the utility-agent narrate pass).
pub fn phase_estimate_narrate(micros: u128) -> String {
    session_id("phase-estimate-narrate", &micros.to_string(), "")
}

/// Phase-review flow id (the local mechanical-verify QA pass).
pub fn phase_review(secs: u64) -> String {
    session_id("phase-review", &secs.to_string(), "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_shape_is_hyphen_delimited_no_colons() {
        assert_eq!(session_id("mission", "m1", ""), "mission-m1");
        assert_eq!(session_id("mission-run", "m1", "p1"), "mission-run-m1-p1");
        // No colon ever appears — the pre-#1436 colon forms are gone.
        for s in [
            mission("m1"),
            task("t1"),
            step("s1"),
            mission_run("m1", "p1"),
            mission_phase_dispatch("m1", "p1", 12345, 0),
        ] {
            assert!(!s.contains(':'), "session id must be colon-free: {s}");
        }
    }

    #[test]
    fn empty_disambiguator_drops_the_trailing_separator() {
        assert_eq!(session_id("kind", "owner", ""), "kind-owner");
        assert!(!session_id("kind", "owner", "").ends_with('-'));
    }

    #[test]
    fn minting_is_deterministic_same_inputs_same_id() {
        assert_eq!(mission_run("m", "p"), mission_run("m", "p"));
        assert_eq!(mission("m"), mission("m"));
        assert_eq!(task("build-coder"), task("build-coder"));
        assert_eq!(
            mission_phase_dispatch("m", "p", 999, 3),
            mission_phase_dispatch("m", "p", 999, 3),
        );
    }

    #[test]
    fn disambiguator_resists_collision_across_dispatch_index_and_stamp() {
        // Two fan-out siblings of the SAME phase differ only by index.
        assert_ne!(
            mission_phase_dispatch("m", "p", 100, 0),
            mission_phase_dispatch("m", "p", 100, 1),
        );
        // The same phase re-dispatched in a later batch differs by micros.
        assert_ne!(
            mission_phase_dispatch("m", "p", 100, 0),
            mission_phase_dispatch("m", "p", 200, 0),
        );
        // Two different phases of the same mission never share an id.
        assert_ne!(
            mission_phase_dispatch("m", "p1", 100, 0),
            mission_phase_dispatch("m", "p2", 100, 0),
        );
    }

    #[test]
    fn family_prefixes_stay_distinct() {
        // The kind prefix keeps the families apart for opaque grouping.
        assert!(mission_run("m", "p").starts_with("mission-run-"));
        assert!(task("t").starts_with("task-"));
        assert!(step("s").starts_with("step-"));
        assert_eq!(phase_review(42), "phase-review-42");
        assert_eq!(phase_estimate_narrate(7), "phase-estimate-narrate-7");
    }
}
