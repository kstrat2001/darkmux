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
use crate::crew::types::{Mission, MissionStatus, Sprint, SprintStatus};
use darkmux_types::style;

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

/// Pure drift detection for one mission given its sprints. The two
/// load-bearing inconsistencies (both observed live 2026-06-14):
///   - a CLOSED mission with a non-terminal (planned/running) sprint — the
///     work likely shipped outside `mission ship --merge`, or `mission close`
///     didn't reconcile; the board reads "closed · 0/1".
///   - an ACTIVE/PAUSED mission whose sprints are ALL terminal with at least
///     one complete — done, just never closed out.
fn detect_drift(m: &Mission, sprints: &[&Sprint]) -> Vec<Drift> {
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

    out
}

/// Entry from main.rs's dispatch. `--json` emits a structured board for the
/// frontier / CI; otherwise a grouped, colorized human board ending with the
/// aggregated suggested-next-steps.
pub fn run(json: bool) -> Result<i32> {
    let missions = crew::loader::load_missions()?;
    let sprints = crew::loader::load_sprints()?;

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
                drifts: detect_drift(m, &ss),
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
        let d = detect_drift(&m, &[&s]);
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
        let d = detect_drift(&m, &[&s]);
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
        assert!(detect_drift(&m, &[&s]).is_empty());
    }

    #[test]
    fn active_mission_all_terminal_suggests_close() {
        let m = mission("m1", MissionStatus::Active);
        let s = sprint("s1", "m1", SprintStatus::Complete);
        let d = detect_drift(&m, &[&s]);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, "done-not-closed");
        assert!(d[0].suggest[0].contains("mission close m1"));
    }

    #[test]
    fn active_mission_with_running_sprint_is_clean() {
        // Work in flight is normal, not drift.
        let m = mission("m1", MissionStatus::Active);
        let s = sprint("s1", "m1", SprintStatus::Running);
        assert!(detect_drift(&m, &[&s]).is_empty());
    }

    #[test]
    fn active_mission_only_abandoned_is_not_done() {
        // All terminal but nothing COMPLETE → not "done", don't nag to close.
        let m = mission("m1", MissionStatus::Active);
        let s = sprint("s1", "m1", SprintStatus::Abandoned);
        assert!(detect_drift(&m, &[&s]).is_empty());
    }

    #[test]
    fn mission_with_no_sprints_is_clean() {
        let m = mission("m1", MissionStatus::Active);
        assert!(detect_drift(&m, &[]).is_empty());
    }

    #[test]
    fn progress_bar_rounds_sensibly() {
        assert_eq!(progress_bar(0, 1), "░░░░");
        assert_eq!(progress_bar(1, 1), "▓▓▓▓");
        assert_eq!(progress_bar(1, 2), "▓▓░░");
        assert_eq!(progress_bar(0, 0), "····");
    }
}
