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
///
/// **Carries no per-RUN identity (#1918).** `task_id` comes straight out of
/// the mission config document — byte-identical across every launch of the
/// same config — so this alone is NOT a valid flow-index key across
/// missions. Every production caller that stamps this onto a real
/// `FlowRecord` composes it with [`scope_to_run`] using the launcher's own
/// per-run `mission_id`, at the same choke point that already backfills
/// `FlowRecord.mission_id` (#1641). This constructor's raw output is still
/// correct and unchanged for callers that only need the per-CONFIG grouping
/// key it always meant (e.g. `dispatch_session_id`'s trait-level contract) —
/// #1918's fix is additive at the emission boundary, not a change to what
/// this function returns.
pub fn task(task_id: &str) -> String {
    session_id("task", task_id, "")
}

/// Step-scoped default dispatch session id (a `dispatch.internal` step with
/// no caller-supplied session id). Was `step:{id}` pre-#1436; the
/// mission-graph page indexes this form to map a record back to its step.
///
/// **Carries no per-RUN identity (#1918)** for the same reason [`task`]
/// doesn't — `step_id` is a literal out of the mission config document. See
/// [`task`]'s doc for the full explanation and [`scope_to_run`] for the fix
/// applied at emission time.
pub fn step(step_id: &str) -> String {
    session_id("step", step_id, "")
}

/// (#1918) Disambiguates a config-derived session id by composing the
/// caller's own per-run identity into it.
///
/// [`task`] and [`step`] are the only two mints in this module that carry
/// NO per-run identity of their own: both derive purely from a task or step
/// id straight out of a mission config, so the SAME config launched twice
/// produces the byte-identical string both times. Every other mint here
/// already composes a mission id or a time-based disambiguator
/// ([`mission`], [`mission_run`], [`mission_phase_dispatch`],
/// [`phase_estimate_narrate`], [`phase_review`]) and passes through this
/// function unchanged.
///
/// Applied at the launcher's `emit`-wrap choke point (`src/mission_launch.rs`,
/// `src/acp_panel.rs`) — the same place that already backfills
/// `FlowRecord.mission_id` (#1641) — rather than inside `darkmux-crew`
/// itself, because the scheduler and the built-in `StepKind`s structurally
/// have no `Mission` concept of their own (see `scheduler::step_lifecycle_
/// record`'s doc).
///
/// **Read-side inventory (#1918 QA).** Two consumers reconstruct these
/// strings independently, and they need DIFFERENT things — because the
/// session id answers two different questions:
///
/// - `darkmux-serve::runs::collect_mission_step_sessions` predicts the
///   UNSCOPED form ONLY, on purpose. It is a MISSION-attribution join, and
///   a scoped record always also carries `FlowRecord.mission_id` (every
///   site that applies this function does so in lock-step with populating
///   that field — unconditionally in the launcher's `emit`-wrap, and gated
///   on the SAME `resolve_mission_for_phase` result at ITS OWN resolution
///   site. As of #1645 fix-pass that is three sites, not one:
///   `dispatch_internal::dispatch` (the container-agentic path), and
///   `dispatch_internal::dispatch_remote` /
///   `dispatch_internal::dispatch_local_single_shot` (the hosted and
///   container-free local single-shot arms, which route around the first
///   entirely for a remote-resolved profile) — all three resolve
///   `mission_id` and apply this function to `session_id` back to back, at
///   their own call site), so `mission_id` already answers it. The
///   unscoped prediction is still needed for the case where the mission
///   does NOT resolve, which leaves BOTH fields in their raw form
///   together.
/// - `darkmux-serve::mission_graph::step_for_record` (and its page-side
///   twin `ui/src/lenses/mission/graph.ts::stepForRecord`) DOES have to
///   accept both spellings. It asks WHICH STEP a record belongs to, and
///   `mission_id` cannot answer that — it only gates admission. The
///   `dispatch complete` record those meters read carries no
///   `payload.step_id` and its `handle` is the ROLE id, so the session id
///   is its only step key. Both peel a trailing `-{mission_id}` before the
///   lookup.
///
/// The general rule for a new consumer: "scoped implies `mission_id`
/// present" is true, but it only lets you SKIP this function when the
/// thing you are identifying is the MISSION.
///
/// Idempotent by substring, not by exact suffix: `run_id` may already
/// appear anywhere in `session_id` (a crew-of-one dispatch's `task_id` is
/// itself minted as `{mission_id}-task`, so `task(&task_id)` already
/// contains the mission id it would otherwise be scoped to) — in that case
/// this is a no-op, not a second stamp.
///
/// A narrow, accepted gap: an OPERATOR-AUTHORED `step.config["session_id"]`
/// that happens to literally start with `task-`/`step-` also gets scoped by
/// this function, since the emission choke point cannot tell "the
/// convention default" apart from "an explicit override that happens to
/// share the prefix" once it's already a plain string on the `FlowRecord`.
/// No built-in role/mission-config template does this; the cost of the
/// false-positive case is a harmless extra suffix on a self-chosen name,
/// never a correctness break.
pub fn scope_to_run(session_id: &str, run_id: &str) -> String {
    let collision_prone = session_id.starts_with("task-") || session_id.starts_with("step-");
    if collision_prone && !session_id.contains(run_id) {
        format!("{session_id}-{run_id}")
    } else {
        session_id.to_string()
    }
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

    // ── scope_to_run (#1918) ─────────────────────────────────────────────

    #[test]
    fn scope_to_run_disambiguates_task_and_step_forms() {
        assert_eq!(scope_to_run(&task("t1"), "m1"), "task-t1-m1");
        assert_eq!(scope_to_run(&step("s1"), "m1"), "step-s1-m1");
    }

    #[test]
    fn scope_to_run_disambiguates_the_same_task_across_two_missions_differently() {
        // The actual #1918 collision: two missions launched from the SAME
        // config mint the SAME `task(&task_id)`. Scoping to each mission's
        // own id must diverge them.
        let a = scope_to_run(&task("t1"), "mission-a");
        let b = scope_to_run(&task("t1"), "mission-b");
        assert_ne!(a, b);
    }

    #[test]
    fn scope_to_run_leaves_already_run_scoped_forms_untouched() {
        // Every OTHER mint already carries a mission id or a time-based
        // disambiguator — scoping must be a no-op for these, never a
        // second stamp.
        assert_eq!(scope_to_run(&mission("m1"), "m1"), mission("m1"));
        assert_eq!(scope_to_run(&mission_run("m1", "p1"), "m1"), mission_run("m1", "p1"));
        assert_eq!(
            scope_to_run(&mission_phase_dispatch("m1", "p1", 100, 0), "m1"),
            mission_phase_dispatch("m1", "p1", 100, 0)
        );
        assert_eq!(scope_to_run(&phase_review(42), "m1"), phase_review(42));
    }

    #[test]
    fn scope_to_run_is_idempotent_by_substring_not_only_exact_suffix() {
        // A crew-of-one dispatch mints `task_id` as `{mission_id}-task`, so
        // `task(&task_id)` already contains the mission id in the MIDDLE of
        // the string, not at the end. Scoping must recognize that as
        // already-disambiguated rather than appending a second, redundant
        // stamp — the read-side predictor (`collect_mission_step_sessions`)
        // calls this unconditionally for every mission shape and must
        // reconstruct the EXACT same string the write side left unscoped.
        let crew_of_one_task_id = "mission-xyz-task";
        let sid = task(crew_of_one_task_id);
        assert_eq!(sid, "task-mission-xyz-task");
        assert_eq!(scope_to_run(&sid, "mission-xyz"), sid, "already contains the run id — must not double-stamp");

        // Calling it twice with the genuinely new-collision shape must also
        // never double-stamp.
        let once = scope_to_run(&task("t1"), "m1");
        assert_eq!(scope_to_run(&once, "m1"), once);
    }

    #[test]
    fn scope_to_run_never_touches_a_non_collision_prone_explicit_session_id() {
        // An operator-authored `step.config["session_id"]` with no
        // `task-`/`step-` prefix at all is untouched.
        assert_eq!(scope_to_run("crew-dispatch-coder-123", "m1"), "crew-dispatch-coder-123");
    }
}
