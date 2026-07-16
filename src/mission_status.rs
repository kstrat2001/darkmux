//! `darkmux mission status` — the global mission-control read (#829).
//!
//! Every other `mission`/`phase` verb is a mutation or a single-shot op;
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
//! The board is computed purely from the durable mission + phase JSON (the
//! loader), so it works offline with no Redis/flow dependency — exactly what
//! a session-start housekeeping cue needs.

use anyhow::Result;
use std::collections::BTreeMap;

use crate::crew;
use crate::crew::types::{Mission, MissionStatus, Phase, PhaseStatus};
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

fn is_terminal(s: PhaseStatus) -> bool {
    matches!(s, PhaseStatus::Complete | PhaseStatus::Abandoned)
}

/// State-accurate reconcile commands for a non-terminal phase. `complete`
/// only transitions Running→Complete, so a PLANNED (never-started) phase
/// needs `phase start` first — emitting a bare `phase complete` for it
/// (the original bug) prints a command that errors. `abandon` works from
/// either state. Surfaced by the cold-session reconcile of the
/// cli-styling-foundation phases, which were planned, not running (#829).
fn reconcile_cmds(s: &Phase) -> Vec<String> {
    let shipped = match s.status {
        PhaseStatus::Planned => format!(
            "darkmux phase start {id} && darkmux phase complete {id}   # if its work shipped",
            id = s.id
        ),
        // Running (or any other non-terminal): complete goes straight through.
        _ => format!("darkmux phase complete {}   # if its work shipped", s.id),
    };
    vec![
        shipped,
        format!("darkmux phase abandon {}   # if it was dropped", s.id),
    ]
}

/// Pure drift detection for one mission given its phases. `now` and
/// `stale_days` are passed in (rather than read internally) so the function
/// stays IO-free and unit-testable with fixed timestamps — see the module
/// doc. Four load-bearing inconsistencies:
///   - a CLOSED mission with a non-terminal (planned/running) phase — the
///     work likely shipped outside `mission ship --merge`, or `mission close`
///     didn't reconcile; the board reads "closed · 0/1".
///   - an ACTIVE/PAUSED mission whose phases are ALL terminal with at least
///     one complete — done, just never closed out.
///   - (#1230 Packet 5) an ACTIVE mission with ZERO complete phases whose
///     `started_ts` is older than `stale_days` — the `doom-loop-m4` case
///     (0/4 phases for ~20 days, no drift surfaced by either check above).
///   - (#1230 Packet 5, revised #1341 for linear phases) a PLANNED phase
///     with an earlier-in-mission-order Abandoned phase — permanently
///     stuck even though the phase itself is still Planned.
fn detect_drift(m: &Mission, phases: &[&Phase], now: u64, stale_days: u64) -> Vec<Drift> {
    let mut out = Vec::new();
    let open: Vec<&&Phase> = phases.iter().filter(|s| !is_terminal(s.status)).collect();
    let complete = phases.iter().filter(|s| s.status == PhaseStatus::Complete).count();
    let all_terminal = !phases.is_empty() && open.is_empty();

    if m.status == MissionStatus::Closed && !open.is_empty() {
        let mut suggest = Vec::new();
        for s in &open {
            suggest.extend(reconcile_cmds(s));
        }
        out.push(Drift {
            kind: "closed-with-open-phase",
            detail: format!(
                "mission is Closed but {} phase(s) are not terminal (planned/running)",
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
            detail: "all phases are terminal — the mission looks done but is still open"
                .to_string(),
            suggest: vec![format!("darkmux mission close {}", m.id)],
        });
    }

    if let Some(d) = stale_active_drift(m, complete, now, stale_days) {
        out.push(d);
    }

    out.extend(unreachable_phase_drifts(m, phases));

    out
}

/// An Active mission with zero Complete phases, stalled for `stale_days`
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
            "mission has been Active for {age_days} day(s) with zero phases complete \
             (staleness threshold: {stale_days} day(s))"
        ),
        suggest: vec![format!(
            "darkmux mission status --json   # inspect phase details — consider \
             `darkmux phase abandon <id>` for stalled work or `darkmux mission close {}` \
             if the mission is done",
            m.id
        )],
    })
}

/// A Planned phase that can never legally run because an EARLIER phase in
/// `Mission.phase_ids` order was Abandoned. (#1341) Phases are strictly
/// linear now — no `depends_on` graph to walk (`reachable`/`PhaseNode` are
/// gone) — so this is a linear scan: any phase abandoned before this one's
/// position permanently blocks it (a strictly linear list has no
/// alternate path around a dead predecessor, unlike the old DAG shape).
/// This is the `doom-loop-m4` signal: `validate-cure` is still Planned but
/// can never legally run because an earlier phase was abandoned.
fn unreachable_phase_drifts(m: &Mission, phases: &[&Phase]) -> Vec<Drift> {
    let phase_by_id: BTreeMap<&str, &&Phase> = phases.iter().map(|p| (p.id.as_str(), p)).collect();

    let mut out = Vec::new();
    let mut dead_ancestor = false;
    for phase_id in &m.phase_ids {
        let Some(phase) = phase_by_id.get(phase_id.as_str()) else { continue };
        if dead_ancestor && phase.status == PhaseStatus::Planned {
            out.push(Drift {
                kind: "unreachable-phase",
                detail: format!(
                    "phase '{}' can never run — an earlier phase in this mission was abandoned",
                    phase.id
                ),
                suggest: vec![format!(
                    "darkmux phase abandon {}   # blocked by an earlier abandoned phase",
                    phase.id
                )],
            });
        }
        if phase.status == PhaseStatus::Abandoned {
            dead_ancestor = true;
        }
    }
    out
}

/// Entry from main.rs's dispatch. `--json` emits a structured board for the
/// frontier / CI; otherwise a grouped, colorized human board ending with the
/// aggregated suggested-next-steps.
pub fn run(json: bool) -> Result<i32> {
    let missions = crew::loader::load_missions()?;
    let phases = crew::loader::load_phases()?;
    let now = now_unix();
    let stale_days = config_access::mission_stale_active_days();

    // Bucket phases by mission_id once.
    let mut by_mission: BTreeMap<&str, Vec<&Phase>> = BTreeMap::new();
    for s in &phases {
        by_mission.entry(s.mission_id.as_str()).or_default().push(s);
    }

    let mut views: Vec<MissionView> = missions
        .iter()
        .map(|m| {
            let ss: Vec<&Phase> = by_mission.get(m.id.as_str()).cloned().unwrap_or_default();
            MissionView {
                total: ss.len(),
                complete: ss.iter().filter(|s| s.status == PhaseStatus::Complete).count(),
                running: ss.iter().filter(|s| s.status == PhaseStatus::Running).count(),
                planned: ss.iter().filter(|s| s.status == PhaseStatus::Planned).count(),
                abandoned: ss.iter().filter(|s| s.status == PhaseStatus::Abandoned).count(),
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
            let mix = phase_mix(v);
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
        println!("{}", style::success("✓ board is clean — every mission's phases are reconciled"));
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
                "phases": {
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

fn phase_mix(v: &MissionView) -> String {
    if v.total == 0 {
        return "no phases".to_string();
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
            phase_ids: vec![],
            created_ts: 0,
            started_ts: None,
            closed_ts: None,
            paused_ts: None,
            source_input: None,
            ticket: None,
        }
    }
    fn phase(id: &str, mid: &str, status: PhaseStatus) -> Phase {
        Phase {
            id: id.into(),
            mission_id: mid.into(),
            description: "d".into(),
            display_name: None,
            status,
            created_ts: 0,
            started_ts: None,
            completed_ts: None,
            abandoned_ts: None,
            task_ids: Vec::new(),
        }
    }

    #[test]
    fn closed_mission_with_running_phase_drifts() {
        let m = mission("m1", MissionStatus::Closed);
        let s = phase("s1", "m1", PhaseStatus::Running);
        let d = detect_drift(&m, &[&s], 0, 14);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, "closed-with-open-phase");
        // RUNNING → complete transitions straight through (no `start`).
        assert!(d[0].suggest.iter().any(|c| c.contains("phase complete s1")
            && !c.contains("phase start")));
        assert!(d[0].suggest.iter().any(|c| c.contains("phase abandon s1")));
    }

    #[test]
    fn closed_mission_with_planned_phase_suggests_start_then_complete() {
        // (#829 follow-up) A PLANNED phase can't go straight to complete —
        // the cue must include `phase start` first, or it prints a command
        // that errors. Caught by the cold-session reconcile.
        let m = mission("m1", MissionStatus::Closed);
        let s = phase("s1", "m1", PhaseStatus::Planned);
        let d = detect_drift(&m, &[&s], 0, 14);
        assert_eq!(d.len(), 1);
        let shipped = d[0].suggest.iter().find(|c| c.contains("if its work shipped")).unwrap();
        assert!(shipped.contains("phase start s1") && shipped.contains("phase complete s1"),
            "planned-phase cue must start then complete; got: {shipped}");
        assert!(d[0].suggest.iter().any(|c| c.contains("phase abandon s1")));
    }

    #[test]
    fn closed_mission_all_terminal_is_clean() {
        let m = mission("m1", MissionStatus::Closed);
        let s = phase("s1", "m1", PhaseStatus::Complete);
        assert!(detect_drift(&m, &[&s], 0, 14).is_empty());
    }

    #[test]
    fn active_mission_all_terminal_suggests_close() {
        let m = mission("m1", MissionStatus::Active);
        let s = phase("s1", "m1", PhaseStatus::Complete);
        let d = detect_drift(&m, &[&s], 0, 14);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, "done-not-closed");
        assert!(d[0].suggest[0].contains("mission close m1"));
    }

    #[test]
    fn active_mission_with_running_phase_is_clean() {
        // Work in flight is normal, not drift.
        let m = mission("m1", MissionStatus::Active);
        let s = phase("s1", "m1", PhaseStatus::Running);
        assert!(detect_drift(&m, &[&s], 0, 14).is_empty());
    }

    #[test]
    fn active_mission_only_abandoned_is_not_done() {
        // All terminal but nothing COMPLETE → not "done", don't nag to close.
        let m = mission("m1", MissionStatus::Active);
        let s = phase("s1", "m1", PhaseStatus::Abandoned);
        assert!(detect_drift(&m, &[&s], 0, 14).is_empty());
    }

    #[test]
    fn mission_with_no_phases_is_clean() {
        let m = mission("m1", MissionStatus::Active);
        assert!(detect_drift(&m, &[], 0, 14).is_empty());
    }

    // ─── stale-active (#1230 Packet 5) ─────────────────────────────────

    #[test]
    fn stale_active_mission_past_threshold_drifts() {
        let mut m = mission("m1", MissionStatus::Active);
        m.started_ts = Some(0);
        // No phases at all — zero complete either way.
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
    fn active_mission_with_a_complete_phase_is_not_flagged_stale() {
        // Old started_ts, but at least one phase completed — progress is
        // happening, this is `done-not-closed`/normal territory, not stale.
        let mut m = mission("m1", MissionStatus::Active);
        m.started_ts = Some(0);
        let s = phase("s1", "m1", PhaseStatus::Complete);
        let d = detect_drift(&m, &[&s], 30 * 86_400, 14);
        // `done-not-closed` fires (all terminal + complete>0), but NOT
        // `stale-active`.
        assert!(!d.iter().any(|dr| dr.kind == "stale-active"));
    }

    // ─── unreachable-phase (#1230 Packet 5) ────────────────────────────

    #[test]
    fn planned_phase_after_abandoned_phase_drifts() {
        // (#1341) Phases are strictly linear — ordered by `Mission.phase_ids`
        // — so "blocked depends on dead" is now expressed by list order:
        // `dead` comes before `blocked`.
        let mut dead = phase("dead", "m1", PhaseStatus::Abandoned);
        dead.abandoned_ts = Some(1);
        let blocked = phase("blocked", "m1", PhaseStatus::Planned);
        let mut m = mission("m1", MissionStatus::Active);
        m.phase_ids = vec!["dead".to_string(), "blocked".to_string()];

        let d = detect_drift(&m, &[&dead, &blocked], 0, 14);
        assert!(d.iter().any(|dr| dr.kind == "unreachable-phase"
            && dr.detail.contains("blocked")
            && dr.suggest.iter().any(|c| c.contains("phase abandon blocked"))));
    }

    #[test]
    fn planned_phase_after_healthy_phase_is_not_flagged_unreachable() {
        let done = phase("done", "m1", PhaseStatus::Complete);
        let next = phase("next", "m1", PhaseStatus::Planned);
        let mut m = mission("m1", MissionStatus::Active);
        m.phase_ids = vec!["done".to_string(), "next".to_string()];

        let d = detect_drift(&m, &[&done, &next], 0, 14);
        assert!(!d.iter().any(|dr| dr.kind == "unreachable-phase"));
    }

    #[test]
    fn non_planned_phase_after_abandoned_phase_is_not_flagged() {
        // Only PLANNED phases get flagged — a phase that already
        // completed/abandoned/started isn't "stuck", it already resolved.
        let dead = phase("dead", "m1", PhaseStatus::Abandoned);
        let done = phase("done", "m1", PhaseStatus::Complete);
        let mut m = mission("m1", MissionStatus::Active);
        m.phase_ids = vec!["dead".to_string(), "done".to_string()];

        let d = detect_drift(&m, &[&dead, &done], 0, 14);
        assert!(!d.iter().any(|dr| dr.kind == "unreachable-phase"));
    }

    /// (#1230 Packet 5 acceptance, revised #1341 for linear phases)
    /// Reproduces the REAL `doom-loop-m4` mission read from
    /// `~/.darkmux/missions/doom-loop-m4/` on disk: `mission.json` (Active,
    /// `started_ts: 1782141824`) + its four phases IN ORDER —
    /// `runtime-capture` (Planned), `file-match` (Abandoned),
    /// `sovereignty-verbs` (Planned), `validate-cure` (Planned). Under the
    /// pre-#1341 DAG shape only `validate-cure` (which explicitly declared
    /// `file-match` as a dependency) was unreachable; under strict
    /// linearity BOTH `sovereignty-verbs` and `validate-cure` are
    /// unreachable, since they both sit after the abandoned `file-match`
    /// in `Mission.phase_ids` order and a linear list has no alternate
    /// path around a dead predecessor. The mission has sat at 0/4 phases
    /// since `started_ts`, which is `> stale_days` ago as of any `now`
    /// after that timestamp — real elapsed wall-clock, not a synthetic
    /// offset, since the operator's live board (read-only, never mutated
    /// by this test) is the acceptance target.
    #[test]
    fn doom_loop_m4_mission_status_fixture_flags_both_drift_variants() {
        let m = Mission {
            id: "doom-loop-m4".to_string(),
            description: "M4 doom-loop arc".to_string(),
            status: MissionStatus::Active,
            phase_ids: vec![
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
        let runtime_capture = phase("runtime-capture", "doom-loop-m4", PhaseStatus::Planned);
        let mut file_match = phase("file-match", "doom-loop-m4", PhaseStatus::Abandoned);
        file_match.started_ts = Some(1_782_141_937);
        file_match.abandoned_ts = Some(1_782_147_136);
        let sovereignty_verbs =
            phase("sovereignty-verbs", "doom-loop-m4", PhaseStatus::Planned);
        // (#1341) `file-match` sits before `validate-cure` in
        // `m.phase_ids` (set above) — that ordering alone now makes
        // `validate-cure` unreachable once `file-match` is Abandoned; no
        // separate `depends_on` declaration.
        let validate_cure = phase("validate-cure", "doom-loop-m4", PhaseStatus::Planned);
        let phases: Vec<&Phase> =
            vec![&runtime_capture, &file_match, &sovereignty_verbs, &validate_cure];

        let now = now_unix(); // real elapsed time since the real started_ts
        let d = detect_drift(&m, &phases, now, 14);

        assert!(
            d.iter().any(|dr| dr.kind == "stale-active"),
            "doom-loop-m4 has sat at 0/4 phases for weeks — must flag stale-active: {d:?}"
        );
        assert!(
            d.iter().any(|dr| dr.kind == "unreachable-phase"
                && dr.detail.contains("validate-cure")),
            "validate-cure sits after abandoned file-match in phase_ids order — must flag \
             unreachable-phase: {d:?}"
        );
        // (#1341) Phases are strictly linear now — `sovereignty-verbs` ALSO
        // sits after the abandoned `file-match` in `phase_ids` order, so it
        // is genuinely blocked too (there's no such thing as an
        // "independent phase" anymore under strict linearity — every
        // phase depends on every phase before it in sequence). This is a
        // real, correct behavior change from the pre-#1341 DAG-shaped
        // fixture (where `sovereignty-verbs` had no explicit dependency on
        // `file-match` and stayed reachable) — not a regression.
        assert!(
            d.iter().any(|dr| dr.kind == "unreachable-phase"
                && dr.detail.contains("sovereignty-verbs")),
            "sovereignty-verbs also sits after abandoned file-match — must flag too under \
             strict linearity: {d:?}"
        );
        // Exactly these three — no accidental extra/missing drift on this
        // mission's real shape.
        assert_eq!(d.len(), 3, "unexpected drift set: {d:?}");
    }

    #[test]
    fn progress_bar_rounds_sensibly() {
        assert_eq!(progress_bar(0, 1), "░░░░");
        assert_eq!(progress_bar(1, 1), "▓▓▓▓");
        assert_eq!(progress_bar(1, 2), "▓▓░░");
        assert_eq!(progress_bar(0, 0), "····");
    }
}
