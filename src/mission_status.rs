//! `darkmux mission status` — the global mission-control read (#829).
//!
//! Every other `mission`/`sprint` verb is a mutation or a single-shot op;
//! none answers "show me the whole board, what's drifted, what needs closing
//! out." This is that read — the CLI twin of the viewer's missions lens,
//! headless and scriptable. It completes the `<noun> status` family that
//! `flow status` and `model status` already established; it is deliberately
//! NOT folded into `doctor` (doctor = runtime/substrate health; this = the
//! work-tracking board).
//!
//! READ-ONLY by design (operator-sovereignty, #44): it surfaces drift and
//! prints copy-pasteable reconcile commands, but never mutates state. The
//! operator (or the frontier reading `--json`) runs the suggested commands.
//!
//! The board is computed purely from the durable mission + sprint JSON (the
//! loader), so it works offline with no Redis/flow dependency — exactly what
//! a session-start housekeeping cue needs.

use anyhow::Result;
use std::collections::BTreeMap;

use crate::crew;
use crate::crew::scheduler::{reachable, DependencyNode, SprintNode};
use crate::crew::types::{Mission, MissionStatus, Sprint, SprintStatus};
use darkmux_types::{config_access, style};

/// A flagged inconsistency on one mission, with concrete reconcile commands.
/// Kept data-only (no IO) so `detect_drift` is unit-testable.
#[derive(Debug, Clone, PartialEq)]
struct Drift {
    kind: &'static str,
    detail: String,
    suggest: Vec<String>,
}

/// Per-mission rollup the renderer consumes.
struct MissionView<'a> {
    m: &'a Mission,
    total: usize,
    complete: usize,
    running: usize,
    planned: usize,
    abandoned: usize,
    drifts: Vec<Drift>,
}

fn is_terminal(s: SprintStatus) -> bool {
    matches!(s, SprintStatus::Complete | SprintStatus::Abandoned)
}

/// State-accurate reconcile commands for a non-terminal sprint. `complete`
/// only transitions Running→Complete, so a PLANNED (never-started) sprint
/// needs `sprint start` first — emitting a bare `sprint complete` for it
/// (the original bug) prints a command that errors. `abandon` works from
/// either state. Surfaced by the cold-session reconcile of the
/// cli-styling-foundation sprints, which were planned, not running (#829).
fn reconcile_cmds(s: &Sprint) -> Vec<String> {
    let shipped = match s.status {
        SprintStatus::Planned => format!(
            "darkmux sprint start {id} && darkmux sprint complete {id}   # if its work shipped",
            id = s.id
        ),
        // Running (or any other non-terminal): complete goes straight through.
        _ => format!("darkmux sprint complete {}   # if its work shipped", s.id),
    };
    vec![
        shipped,
        format!("darkmux sprint abandon {}   # if it was dropped", s.id),
    ]
}

/// Pure drift detection for one mission given its sprints. `now` and
/// `stale_days` are passed in (rather than read internally) so the function
/// stays IO-free and unit-testable with fixed timestamps — see the module
/// doc. Four load-bearing inconsistencies:
///   - a CLOSED mission with a non-terminal (planned/running) sprint — the
///     work likely shipped outside `mission ship --merge`, or `mission close`
///     didn't reconcile; the board reads "closed · 0/1".
///   - an ACTIVE/PAUSED mission whose sprints are ALL terminal with at least
///     one complete — done, just never closed out.
///   - (#1230 Packet 5) an ACTIVE mission with ZERO complete sprints whose
///     `started_ts` is older than `stale_days` — the `doom-loop-m4` case
///     (0/4 sprints for ~20 days, no drift surfaced by either check above).
///   - (#1230 Packet 5) a PLANNED sprint whose dependency chain includes an
///     Abandoned/Error sprint, via Packet 2's `reachable()` — permanently
///     stuck even though the sprint itself is still Planned.
fn detect_drift(m: &Mission, sprints: &[&Sprint], now: u64, stale_days: u64) -> Vec<Drift> {
    let mut out = Vec::new();
    let open: Vec<&&Sprint> = sprints.iter().filter(|s| !is_terminal(s.status)).collect();
    let complete = sprints.iter().filter(|s| s.status == SprintStatus::Complete).count();
    let all_terminal = !sprints.is_empty() && open.is_empty();

    if m.status == MissionStatus::Closed && !open.is_empty() {
        let mut suggest = Vec::new();
        for s in &open {
            suggest.extend(reconcile_cmds(s));
        }
        out.push(Drift {
            kind: "closed-with-open-sprint",
            detail: format!(
                "mission is Closed but {} sprint(s) are not terminal (planned/running)",
                open.len()
            ),
            suggest,
        });
    }

    if matches!(m.status, MissionStatus::Active | MissionStatus::Paused)
        && all_terminal
        && complete > 0
    {
        out.push(Drift {
            kind: "done-not-closed",
            detail: "all sprints are terminal — the mission looks done but is still open"
                .to_string(),
            suggest: vec![format!("darkmux mission close {}", m.id)],
        });
    }

    if let Some(d) = stale_active_drift(m, complete, now, stale_days) {
        out.push(d);
    }

    out.extend(unreachable_sprint_drifts(sprints));

    out
}

/// An Active mission with zero Complete sprints, stalled for `stale_days`
/// or longer since `started_ts`. A mission that hasn't started yet
/// (`started_ts: None`) can't be judged stale — fails closed, same
/// discipline `reachable` uses for a dangling dependency reference.
fn stale_active_drift(m: &Mission, complete: usize, now: u64, stale_days: u64) -> Option<Drift> {
    if m.status != MissionStatus::Active || complete > 0 {
        return None;
    }
    let started = m.started_ts?;
    let age_days = now.saturating_sub(started) / 86_400;
    if age_days < stale_days {
        return None;
    }
    Some(Drift {
        kind: "stale-active",
        detail: format!(
            "mission has been Active for {age_days} day(s) with zero sprints complete \
             (staleness threshold: {stale_days} day(s))"
        ),
        suggest: vec![format!(
            "darkmux mission status --json   # inspect sprint details — consider \
             `darkmux sprint abandon <id>` for stalled work or `darkmux mission close {}` \
             if the mission is done",
            m.id
        )],
    })
}

/// A Planned sprint whose dependency chain includes an Abandoned/Error
/// sprint — via Packet 2's `reachable()` through the existing
/// `Sprint -> DependencyNode` adapter (`SprintNode`), no new graph-walking
/// logic. This is the `doom-loop-m4` signal: `validate-cure` is still
/// Planned but can never legally run because `file-match` (a dependency)
/// was abandoned.
fn unreachable_sprint_drifts(sprints: &[&Sprint]) -> Vec<Drift> {
    let nodes: Vec<SprintNode> = sprints.iter().map(|s| SprintNode(s)).collect();
    let by_id: BTreeMap<String, &SprintNode> =
        nodes.iter().map(|n| (n.node_id().to_string(), n)).collect();

    sprints
        .iter()
        .filter(|s| s.status == SprintStatus::Planned)
        .filter(|s| !reachable(&s.id, &by_id))
        .map(|s| Drift {
            kind: "unreachable-sprint",
            detail: format!(
                "sprint '{}' can never run — its dependency chain includes an \
                 abandoned or errored sprint",
                s.id
            ),
            suggest: vec![format!(
                "darkmux sprint abandon {}   # dependency chain is permanently dead",
                s.id
            )],
        })
        .collect()
}

/// Entry from main.rs's dispatch. `--json` emits a structured board for the
/// frontier / CI; otherwise a grouped, colorized human board ending with the
/// aggregated suggested-next-steps.
pub fn run(json: bool) -> Result<i32> {
    let missions = crew::loader::load_missions()?;
    let sprints = crew::loader::load_sprints()?;
    let now = now_unix();
    let stale_days = config_access::mission_stale_active_days();

    // Bucket sprints by mission_id once.
    let mut by_mission: BTreeMap<&str, Vec<&Sprint>> = BTreeMap::new();
    for s in &sprints {
        by_mission.entry(s.mission_id.as_str()).or_default().push(s);
    }

    let mut views: Vec<MissionView> = missions
        .iter()
        .map(|m| {
            let ss: Vec<&Sprint> = by_mission.get(m.id.as_str()).cloned().unwrap_or_default();
            MissionView {
                total: ss.len(),
                complete: ss.iter().filter(|s| s.status == SprintStatus::Complete).count(),
                running: ss.iter().filter(|s| s.status == SprintStatus::Running).count(),
                planned: ss.iter().filter(|s| s.status == SprintStatus::Planned).count(),
                abandoned: ss.iter().filter(|s| s.status == SprintStatus::Abandoned).count(),
                drifts: detect_drift(m, &ss, now, stale_days),
                m,
            }
        })
        .collect();
    // Drifted first (so attention items lead), then by id for stability.
    views.sort_by(|a, b| {
        (b.drifts.is_empty() as u8)
            .cmp(&(a.drifts.is_empty() as u8))
            .then(a.m.id.cmp(&b.m.id))
    });

    if json {
        return run_json(&views);
    }

    let attention: usize = views.iter().filter(|v| !v.drifts.is_empty()).count();
    println!(
        "{}",
        style::header(&format!(
            "mission status — {} mission{}",
            views.len(),
            if views.len() == 1 { "" } else { "s" }
        ))
    );
    if views.is_empty() {
        println!("  {}", style::dim("no missions — propose one with `darkmux mission propose`"));
        return Ok(0);
    }

    for group in [MissionStatus::Active, MissionStatus::Paused, MissionStatus::Closed] {
        let g: Vec<&MissionView> = views.iter().filter(|v| v.m.status == group).collect();
        if g.is_empty() {
            continue;
        }
        println!("\n{}", style::dim(&format!("{} ({})", status_word(group).to_uppercase(), g.len())));
        for v in g {
            let prog = format!("{}/{}", v.complete, v.total);
            let bar = progress_bar(v.complete, v.total);
            let mix = sprint_mix(v);
            println!("  ◆ {:30}  {:>5}  {}  {}", v.m.id, prog, bar, style::dim(&mix));
            for d in &v.drifts {
                println!("      {} {}", style::warn("⚠"), style::warn(&d.detail));
                for cmd in &d.suggest {
                    println!("        {} {}", style::dim("→"), cmd);
                }
            }
        }
    }

    println!();
    if attention == 0 {
        println!("{}", style::success("✓ board is clean — every mission's sprints are reconciled"));
    } else {
        println!(
            "{}",
            style::warn(&format!(
                "{} mission{} need attention — run the suggested commands above to reconcile",
                attention,
                if attention == 1 { "" } else { "s" }
            ))
        );
    }
    Ok(0)
}

fn run_json(views: &[MissionView]) -> Result<i32> {
    let arr: Vec<serde_json::Value> = views
        .iter()
        .map(|v| {
            serde_json::json!({
                "id": v.m.id,
                "status": status_word(v.m.status),
                "ticket": v.m.ticket,
                "sprints": {
                    "total": v.total, "complete": v.complete, "running": v.running,
                    "planned": v.planned, "abandoned": v.abandoned,
                },
                "drift": v.drifts.iter().map(|d| serde_json::json!({
                    "kind": d.kind, "detail": d.detail, "suggest": d.suggest,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let attention = views.iter().filter(|v| !v.drifts.is_empty()).count();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "missions": arr,
            "summary": { "total": views.len(), "needs_attention": attention },
        }))?
    );
    Ok(0)
}

fn status_word(s: MissionStatus) -> &'static str {
    match s {
        MissionStatus::Active => "active",
        MissionStatus::Paused => "paused",
        MissionStatus::Closed => "closed",
    }
}

fn sprint_mix(v: &MissionView) -> String {
    if v.total == 0 {
        return "no sprints".to_string();
    }
    let mut parts = Vec::new();
    if v.complete > 0 { parts.push(format!("{} complete", v.complete)); }
    if v.running > 0 { parts.push(format!("{} running", v.running)); }
    if v.planned > 0 { parts.push(format!("{} planned", v.planned)); }
    if v.abandoned > 0 { parts.push(format!("{} abandoned", v.abandoned)); }
    parts.join(" · ")
}

fn progress_bar(done: usize, total: usize) -> String {
    if total == 0 {
        return "····".to_string();
    }
    let width = 4;
    let filled = (done * width + total / 2) / total;
    let filled = filled.min(width);
    format!("{}{}", "▓".repeat(filled), "░".repeat(width - filled))
}

/// The only IO/clock touch in this module — kept to one call site in `run()`
/// so `detect_drift` itself stays pure and unit-testable with fixed
/// timestamps.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mission(id: &str, status: MissionStatus) -> Mission {
        Mission {
            id: id.into(),
            description: "d".into(),
            status,
            sprint_ids: vec![],
            created_ts: 0,
            started_ts: None,
            closed_ts: None,
            paused_ts: None,
            source_input: None,
            ticket: None,
        }
    }
    fn sprint(id: &str, mid: &str, status: SprintStatus) -> Sprint {
        Sprint {
            id: id.into(),
            mission_id: mid.into(),
            description: "d".into(),
            status,
            depends_on: vec![],
            created_ts: 0,
            started_ts: None,
            completed_ts: None,
            abandoned_ts: None,
            task_ids: Vec::new(),
        }
    }

    #[test]
    fn closed_mission_with_running_sprint_drifts() {
        let m = mission("m1", MissionStatus::Closed);
        let s = sprint("s1", "m1", SprintStatus::Running);
        let d = detect_drift(&m, &[&s], 0, 14);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, "closed-with-open-sprint");
        // RUNNING → complete transitions straight through (no `start`).
        assert!(d[0].suggest.iter().any(|c| c.contains("sprint complete s1")
            && !c.contains("sprint start")));
        assert!(d[0].suggest.iter().any(|c| c.contains("sprint abandon s1")));
    }

    #[test]
    fn closed_mission_with_planned_sprint_suggests_start_then_complete() {
        // (#829 follow-up) A PLANNED sprint can't go straight to complete —
        // the cue must include `sprint start` first, or it prints a command
        // that errors. Caught by the cold-session reconcile.
        let m = mission("m1", MissionStatus::Closed);
        let s = sprint("s1", "m1", SprintStatus::Planned);
        let d = detect_drift(&m, &[&s], 0, 14);
        assert_eq!(d.len(), 1);
        let shipped = d[0].suggest.iter().find(|c| c.contains("if its work shipped")).unwrap();
        assert!(shipped.contains("sprint start s1") && shipped.contains("sprint complete s1"),
            "planned-sprint cue must start then complete; got: {shipped}");
        assert!(d[0].suggest.iter().any(|c| c.contains("sprint abandon s1")));
    }

    #[test]
    fn closed_mission_all_terminal_is_clean() {
        let m = mission("m1", MissionStatus::Closed);
        let s = sprint("s1", "m1", SprintStatus::Complete);
        assert!(detect_drift(&m, &[&s], 0, 14).is_empty());
    }

    #[test]
    fn active_mission_all_terminal_suggests_close() {
        let m = mission("m1", MissionStatus::Active);
        let s = sprint("s1", "m1", SprintStatus::Complete);
        let d = detect_drift(&m, &[&s], 0, 14);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, "done-not-closed");
        assert!(d[0].suggest[0].contains("mission close m1"));
    }

    #[test]
    fn active_mission_with_running_sprint_is_clean() {
        // Work in flight is normal, not drift.
        let m = mission("m1", MissionStatus::Active);
        let s = sprint("s1", "m1", SprintStatus::Running);
        assert!(detect_drift(&m, &[&s], 0, 14).is_empty());
    }

    #[test]
    fn active_mission_only_abandoned_is_not_done() {
        // All terminal but nothing COMPLETE → not "done", don't nag to close.
        let m = mission("m1", MissionStatus::Active);
        let s = sprint("s1", "m1", SprintStatus::Abandoned);
        assert!(detect_drift(&m, &[&s], 0, 14).is_empty());
    }

    #[test]
    fn mission_with_no_sprints_is_clean() {
        let m = mission("m1", MissionStatus::Active);
        assert!(detect_drift(&m, &[], 0, 14).is_empty());
    }

    // ─── stale-active (#1230 Packet 5) ─────────────────────────────────

    #[test]
    fn stale_active_mission_past_threshold_drifts() {
        let mut m = mission("m1", MissionStatus::Active);
        m.started_ts = Some(0);
        // No sprints at all — zero complete either way.
        let now = 15 * 86_400; // 15 days later
        let d = detect_drift(&m, &[], now, 14);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, "stale-active");
        assert!(d[0].detail.contains("15 day"));
    }

    #[test]
    fn active_mission_within_staleness_threshold_is_clean() {
        let mut m = mission("m1", MissionStatus::Active);
        m.started_ts = Some(0);
        let now = 5 * 86_400; // only 5 days in — under the 14-day default
        assert!(detect_drift(&m, &[], now, 14).is_empty());
    }

    #[test]
    fn active_mission_never_started_is_not_flagged_stale() {
        // started_ts: None (never actually kicked off) — can't judge
        // staleness, fails closed rather than flagging.
        let m = mission("m1", MissionStatus::Active);
        assert!(m.started_ts.is_none());
        assert!(detect_drift(&m, &[], 999 * 86_400, 14).is_empty());
    }

    #[test]
    fn active_mission_with_a_complete_sprint_is_not_flagged_stale() {
        // Old started_ts, but at least one sprint completed — progress is
        // happening, this is `done-not-closed`/normal territory, not stale.
        let mut m = mission("m1", MissionStatus::Active);
        m.started_ts = Some(0);
        let s = sprint("s1", "m1", SprintStatus::Complete);
        let d = detect_drift(&m, &[&s], 30 * 86_400, 14);
        // `done-not-closed` fires (all terminal + complete>0), but NOT
        // `stale-active`.
        assert!(!d.iter().any(|dr| dr.kind == "stale-active"));
    }

    // ─── unreachable-sprint (#1230 Packet 5) ────────────────────────────

    #[test]
    fn planned_sprint_depending_on_abandoned_sprint_drifts() {
        let mut dead = sprint("dead", "m1", SprintStatus::Abandoned);
        dead.abandoned_ts = Some(1);
        let mut blocked = sprint("blocked", "m1", SprintStatus::Planned);
        blocked.depends_on = vec!["dead".to_string()];
        let m = mission("m1", MissionStatus::Active);

        let d = detect_drift(&m, &[&dead, &blocked], 0, 14);
        assert!(d.iter().any(|dr| dr.kind == "unreachable-sprint"
            && dr.detail.contains("blocked")
            && dr.suggest.iter().any(|c| c.contains("sprint abandon blocked"))));
    }

    #[test]
    fn planned_sprint_with_healthy_dependency_is_not_flagged_unreachable() {
        let done = sprint("done", "m1", SprintStatus::Complete);
        let mut next = sprint("next", "m1", SprintStatus::Planned);
        next.depends_on = vec!["done".to_string()];
        let m = mission("m1", MissionStatus::Active);

        let d = detect_drift(&m, &[&done, &next], 0, 14);
        assert!(!d.iter().any(|dr| dr.kind == "unreachable-sprint"));
    }

    #[test]
    fn non_planned_sprint_with_abandoned_dependency_is_not_flagged() {
        // Only PLANNED sprints get flagged — a sprint that already
        // completed/abandoned/started isn't "stuck", it already resolved.
        let dead = sprint("dead", "m1", SprintStatus::Abandoned);
        let mut done = sprint("done", "m1", SprintStatus::Complete);
        done.depends_on = vec!["dead".to_string()];
        let m = mission("m1", MissionStatus::Active);

        let d = detect_drift(&m, &[&dead, &done], 0, 14);
        assert!(!d.iter().any(|dr| dr.kind == "unreachable-sprint"));
    }

    /// (#1230 Packet 5 acceptance) Reproduces the REAL `doom-loop-m4`
    /// mission read from `~/.darkmux/missions/doom-loop-m4/` on disk:
    /// `mission.json` (Active, `started_ts: 1782141824`) + its four
    /// sprints — `runtime-capture`/`sovereignty-verbs` (Planned),
    /// `file-match` (Abandoned), `validate-cure` (Planned, depends on all
    /// three, including the abandoned `file-match`). Same real shape
    /// `scheduler::tests::doom_loop_m4_fixture_validate_cure_is_unreachable`
    /// (#1230 Packet 2) already used for the scheduler-level check — this
    /// is the `mission status` surfacing of the same real data. The mission
    /// has sat at 0/4 sprints since `started_ts`, which is `> stale_days`
    /// ago as of any `now` after that timestamp — real elapsed wall-clock,
    /// not a synthetic offset, since the operator's live board (read-only,
    /// never mutated by this test) is the acceptance target.
    #[test]
    fn doom_loop_m4_mission_status_fixture_flags_both_drift_variants() {
        let m = Mission {
            id: "doom-loop-m4".to_string(),
            description: "M4 doom-loop arc".to_string(),
            status: MissionStatus::Active,
            sprint_ids: vec![
                "runtime-capture".to_string(),
                "file-match".to_string(),
                "sovereignty-verbs".to_string(),
                "validate-cure".to_string(),
            ],
            created_ts: 1_782_141_824,
            started_ts: Some(1_782_141_824),
            closed_ts: None,
            paused_ts: None,
            source_input: None,
            ticket: None,
        };
        let runtime_capture = sprint("runtime-capture", "doom-loop-m4", SprintStatus::Planned);
        let mut file_match = sprint("file-match", "doom-loop-m4", SprintStatus::Abandoned);
        file_match.started_ts = Some(1_782_141_937);
        file_match.abandoned_ts = Some(1_782_147_136);
        let sovereignty_verbs =
            sprint("sovereignty-verbs", "doom-loop-m4", SprintStatus::Planned);
        let mut validate_cure = sprint("validate-cure", "doom-loop-m4", SprintStatus::Planned);
        validate_cure.depends_on = vec![
            "runtime-capture".to_string(),
            "file-match".to_string(),
            "sovereignty-verbs".to_string(),
        ];
        let sprints: Vec<&Sprint> =
            vec![&runtime_capture, &file_match, &sovereignty_verbs, &validate_cure];

        let now = now_unix(); // real elapsed time since the real started_ts
        let d = detect_drift(&m, &sprints, now, 14);

        assert!(
            d.iter().any(|dr| dr.kind == "stale-active"),
            "doom-loop-m4 has sat at 0/4 sprints for weeks — must flag stale-active: {d:?}"
        );
        assert!(
            d.iter().any(|dr| dr.kind == "unreachable-sprint"
                && dr.detail.contains("validate-cure")),
            "validate-cure depends on abandoned file-match — must flag unreachable-sprint: {d:?}"
        );
        // Exactly these two — no accidental extra/missing drift on this
        // mission's real shape.
        assert_eq!(d.len(), 2, "unexpected drift set: {d:?}");
    }

    #[test]
    fn progress_bar_rounds_sensibly() {
        assert_eq!(progress_bar(0, 1), "░░░░");
        assert_eq!(progress_bar(1, 1), "▓▓▓▓");
        assert_eq!(progress_bar(1, 2), "▓▓░░");
        assert_eq!(progress_bar(0, 0), "····");
    }
}
