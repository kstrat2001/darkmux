//! `darkmux mission status` — the global mission-control read (#829).
//!
//! Every other `mission` verb is a mutation or a single-shot op;
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

/// Pure drift detection for one mission given its phases. `now` and
/// `stale_days` are passed in (rather than read internally) so the function
/// stays IO-free and unit-testable with fixed timestamps — see the module
/// doc. Load-bearing inconsistencies:
///   - an ACTIVE/PAUSED mission whose phases are ALL terminal with at least
///     one complete — done, just never finalized.
///   - (#1230 Packet 5) an ACTIVE mission with ZERO complete phases whose
///     `started_ts` is older than `stale_days` — the `doom-loop-m4` case
///     (0/4 phases for ~20 days, no drift surfaced by either check above).
///   - (#1230 Packet 5, revised #1341 for linear phases) a PLANNED phase
///     with an earlier-in-mission-order Abandoned phase — permanently
///     stuck even though the phase itself is still Planned.
///
/// (#1463) The old "CLOSED mission with a non-terminal phase" arm RETIRED:
/// `mission finalize` / `mission abort` now reconcile EVERY phase to a
/// terminal status as part of closing the mission, so a Finalized mission
/// with an open phase is no longer a reachable state to detect. (Its `phase
/// complete`/`phase abandon` reconcile hints went with the retired `phase`
/// family; the surviving hints point at `mission finalize` / `mission abort`.)
fn detect_drift(m: &Mission, phases: &[&Phase], now: u64, stale_days: u64) -> Vec<Drift> {
    let mut out = Vec::new();
    let open: Vec<&&Phase> = phases.iter().filter(|s| !is_terminal(s.status)).collect();
    let complete = phases.iter().filter(|s| s.status == PhaseStatus::Complete).count();
    let all_terminal = !phases.is_empty() && open.is_empty();

    if matches!(m.status, MissionStatus::Active | MissionStatus::Paused)
        && all_terminal
        && complete > 0
    {
        out.push(Drift {
            kind: "done-not-finalized",
            detail: "all phases are terminal — the mission looks done but is still open"
                .to_string(),
            suggest: vec![format!("darkmux mission finalize {}", m.id)],
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
            "darkmux mission status --json   # inspect phase details — then \
             `darkmux mission abort {id}` to tear the stalled mission down, or \
             `darkmux mission finalize {id}` if the work is actually done",
            id = m.id
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
                    "darkmux mission abort {mid} --phase {pid}   # abandon just this \
                     permanently-blocked phase (a bare `mission abort {mid}` would abandon \
                     every healthy phase too); the mission closes on its own once that \
                     leaves every phase terminal",
                    mid = m.id,
                    pid = phase.id
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
///
/// `limit` caps rows PER SECTION (not per board) so a long finalized history
/// can't push the active work off screen; `all` lifts the cap. `None` selects
/// the per-section defaults (see `default_limit`) — an explicit `Some(n)`
/// applies uniformly, because a number the operator typed outranks one the
/// system derived (#44). `--json` is never paginated: a machine reader wants
/// the whole board, and trimming it would make the structured output lie about
/// what exists (#1569).
pub fn run(json: bool, limit: Option<usize>, all: bool) -> Result<i32> {
    let unlimited = all || limit == Some(0);
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
    views.sort_by(board_order);

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
    // Resolved once, above the early return, so every prose line in this
    // renderer — including the empty-board hint — wraps to the same width.
    let width = style::terminal_width();
    if views.is_empty() {
        for line in
            wrap_indented("no missions — propose one with `darkmux mission propose`", 2, width)
        {
            println!("{}", style::dim(&line));
        }
        return Ok(0);
    }

    // Section membership first, so the layout can be planned from exactly the
    // rows that will be printed (and stay aligned across every section).
    let groups: Vec<(MissionStatus, Vec<&MissionView>)> =
        [MissionStatus::Active, MissionStatus::Paused, MissionStatus::Finalized]
            .into_iter()
            .map(|group| (group, views.iter().filter(|v| v.m.status == group).collect()))
            .filter(|(_, g): &(_, Vec<&MissionView>)| !g.is_empty())
            .collect();

    let shown_counts: Vec<usize> = groups
        .iter()
        .map(|(group, g)| {
            if unlimited {
                g.len()
            } else {
                limit.unwrap_or_else(|| default_limit(*group)).min(g.len())
            }
        })
        .collect();
    let layout = plan_layout(
        groups.iter().zip(&shown_counts).flat_map(|((_, g), n)| g.iter().take(*n).copied()),
        width,
    );

    // Tracked across sections so the closing rollup can admit that some of the
    // missions it counts had their suggestions paginated away.
    let mut any_drift_hidden = false;

    for ((group, g), &shown) in groups.iter().zip(&shown_counts) {
        println!(
            "\n{}",
            style::dim(&format!("{} ({})", status_word(*group).to_uppercase(), g.len()))
        );
        for v in g.iter().take(shown) {
            let prog = format!("{}/{}", v.complete, v.total);
            let bar = progress_bar(v.complete, v.total);
            let id = ellipsize(&v.m.id, layout.id_width);
            if layout.show_mix {
                println!(
                    "  ◆ {:<width$}  {:>5}  {}  {}",
                    id,
                    prog,
                    bar,
                    style::dim(&phase_mix(v)),
                    width = layout.id_width
                );
            } else {
                // Narrow terminal: the mix is dropped rather than the id or the
                // progress, because it is the one column whose information the
                // other two already carry.
                println!("  ◆ {:<width$}  {:>5}  {}", id, prog, bar, width = layout.id_width);
            }
            for d in &v.drifts {
                // The ⚠ marks the warning, not each of its lines — continuation
                // lines get blank space in the marker column so one wrapped
                // warning still reads as one warning.
                for (i, line) in wrap_indented(&d.detail, 8, width).iter().enumerate() {
                    let marker = if i == 0 { style::warn("⚠") } else { " ".to_string() };
                    println!("      {} {}", marker, style::warn(line.trim_start()));
                }
                for cmd in &d.suggest {
                    // The command itself is printed verbatim and never wrapped
                    // or truncated — it exists to be copy-pasted, and a command
                    // broken across lines by a renderer is worse than one that
                    // overflows. Only its trailing rationale is wrapped.
                    let (command, note) = split_suggestion(cmd);
                    println!("        {} {}", style::dim("→"), command);
                    for line in wrap_indented(note, 10, width) {
                        println!("{}", style::dim(&line));
                    }
                }
            }
        }
        if shown < g.len() {
            let hidden_drift = g.iter().skip(shown).filter(|v| !v.drifts.is_empty()).count();
            let more = format!(
                "… {} more ({} of {} shown) — `--all` for every mission",
                g.len() - shown,
                shown,
                g.len()
            );
            for line in wrap_indented(&more, 2, width) {
                println!("{}", style::dim(&line));
            }
            if hidden_drift > 0 {
                any_drift_hidden = true;
                // Never let a limit silently swallow an attention item.
                let warn = format!(
                    "⚠ {} hidden mission{} need{} attention — run with `--all`",
                    hidden_drift,
                    if hidden_drift == 1 { "" } else { "s" },
                    if hidden_drift == 1 { "s" } else { "" }
                );
                for line in wrap_indented(&warn, 2, width) {
                    println!("{}", style::warn(&line));
                }
            }
        }
    }

    println!();
    if attention == 0 {
        for line in wrap_indented("✓ board is clean — every mission's phases are reconciled", 0, width) {
            println!("{}", style::success(&line));
        }
    } else {
        // "above" is only true for the drifted missions that were PRINTED; a
        // section limit can leave others unshown (each section warns), so the
        // rollup admits it rather than pointing at commands that never appeared.
        let summary = format!(
            "{} mission{} {} attention — run the suggested commands above to reconcile{}",
            attention,
            if attention == 1 { "" } else { "s" },
            if attention == 1 { "needs" } else { "need" },
            if any_drift_hidden { " (some are hidden — `--all` to see them)" } else { "" }
        );
        for line in wrap_indented(&summary, 0, width) {
            println!("{}", style::warn(&line));
        }
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

/// Board ordering: drifted first (attention leads), then most-recently-touched,
/// then id as a stable tiebreak.
///
/// Extracted from an inline closure specifically so the DIRECTION is testable.
/// It was previously written `(b.is_empty()).cmp(&(a.is_empty()))` under a
/// comment claiming "drifted first", which sorts clean missions first — the
/// exact inverse of both the comment and the intent. Nothing caught it because
/// the comparator lived inside `run()`, which needs mission JSON on disk to
/// exercise, so no unit test could reach it.
///
/// Direction, stated so it can't silently invert again: a mission WITH drift has
/// `drifts.is_empty() == false == 0u8`, and `Ordering::Less` sorts first, so the
/// drift key must be `a.cmp(b)` — NOT `b.cmp(a)`. Recency is the one key that IS
/// reversed (`b.cmp(a)`), because newest-first means larger-timestamp-first.
///
/// This direction is load-bearing for pagination, not cosmetic: the per-section
/// limit is only safe because truncation drops from the TAIL, so with drift
/// sorted first a cap can only ever hide rows needing no attention.
fn board_order(a: &MissionView, b: &MissionView) -> std::cmp::Ordering {
    (a.drifts.is_empty() as u8)
        .cmp(&(b.drifts.is_empty() as u8))
        .then(last_activity(b.m).cmp(&last_activity(a.m)))
        .then(a.m.id.cmp(&b.m.id))
}

/// Most recent state transition on a mission — the honest "last touched".
///
/// A max over the present timestamps rather than a single field, because which
/// field is newest depends on the mission's path through the state machine
/// (`created` → maybe `started` → maybe `paused` → maybe `finalized`), and a
/// mission can be paused after being started, or finalized without ever having
/// started. Nothing is subtracted, so a mission with only `created_ts` still
/// sorts by that.
fn last_activity(m: &Mission) -> u64 {
    m.created_ts
        .max(m.started_ts.unwrap_or(0))
        .max(m.paused_ts.unwrap_or(0))
        .max(m.finalized_ts.unwrap_or(0))
}

/// Rows shown per section when the operator names no `--limit`.
///
/// FINALIZED gets a much smaller budget than the open sections on purpose:
/// closed work is recent-history context, not the question the board answers,
/// and on a real board it dominates by an order of magnitude (measured: 69
/// finalized vs 1 active, 21 of them single-turn `dispatch-*` records minted one
/// per `darkmux dispatch`). Spending equal screen space on both would let
/// exhaust crowd out the work — the failure this pagination exists to fix.
/// An explicit `--limit n` overrides this uniformly (#44: a typed number
/// outranks a derived one).
fn default_limit(group: MissionStatus) -> usize {
    match group {
        MissionStatus::Finalized => 3,
        MissionStatus::Active | MissionStatus::Paused => 10,
    }
}

/// Fixed per-row cost of a row WITHOUT the mix column: `"  ◆ "` (4) + the two
/// 2-space gaps + the 5-wide progress field + the 4-wide bar = 17.
///
/// Exactly this and no more: a row that also shows the mix pays one ADDITIONAL
/// gap, which is `MIX_GAP_COLS`. Conflating the two let `plan_layout` judge a
/// with-mix row 2 columns narrower than it renders, so at `COLUMNS=51` an
/// id(12) + mix(22) row measured 51 and printed 53 — hard-wrapping on exactly
/// the terminal width the adaptation exists to respect.
const ROW_FIXED_COLS: usize = 17;

/// The extra 2-space gap between the bar and the mix column, paid only when the
/// mix is shown. See `ROW_FIXED_COLS`.
const MIX_GAP_COLS: usize = 2;

/// Never shrink the id column below this — a mission id truncated to a few
/// characters identifies nothing, which defeats the point of keeping it.
const MIN_ID_COLS: usize = 12;

/// Below this much room for text, `wrap_indented` stops wrapping and emits one
/// overlong line instead. Wrapping prose into a 3-column gutter produces
/// something less readable than an overflowing line, not more.
///
/// Deliberately its OWN constant rather than reusing `MIN_ID_COLS`: the two
/// happen to share a value but answer unrelated questions (how short an id may
/// be truncated vs. how narrow a paragraph is worth wrapping), so tying them
/// together would make one silently move when the other is tuned.
const MIN_WRAP_ROOM: usize = 12;

/// How one board row is laid out at the current terminal width.
#[derive(Debug, PartialEq)]
struct Layout {
    id_width: usize,
    show_mix: bool,
}

/// Plan the row layout from the rows that will actually be printed.
///
/// Degradation order is deliberate: the phase mix goes first (the progress
/// fraction and bar already carry its information), and only then does the id
/// get truncated. `width == None` means output isn't a terminal, so nothing is
/// adapted and nothing is dropped — piped output stays complete.
fn plan_layout<'a>(
    rows: impl Iterator<Item = &'a MissionView<'a>>,
    width: Option<usize>,
) -> Layout {
    let (max_id, max_mix) = rows.fold((0, 0), |(i, x), v| {
        (i.max(v.m.id.chars().count()), x.max(phase_mix(v).chars().count()))
    });
    let Some(w) = width else {
        return Layout { id_width: max_id, show_mix: true };
    };
    if max_id + ROW_FIXED_COLS + MIX_GAP_COLS + max_mix <= w {
        Layout { id_width: max_id, show_mix: true }
    } else if max_id + ROW_FIXED_COLS <= w {
        Layout { id_width: max_id, show_mix: false }
    } else {
        Layout { id_width: w.saturating_sub(ROW_FIXED_COLS).max(MIN_ID_COLS), show_mix: false }
    }
}

/// Split a suggested command from its trailing `#` rationale.
///
/// Drift suggestions are authored as `<command>   # <why + caveats>`, and the
/// rationale is where nearly all the length lives (measured: 265-column lines
/// against an 80-column terminal). Splitting lets the command stay verbatim
/// while the prose wraps. A suggestion with no `#` comment yields an empty
/// note, so callers print nothing extra.
fn split_suggestion(s: &str) -> (&str, &str) {
    match s.find("  #") {
        Some(i) => (s[..i].trim_end(), s[i..].trim_start().trim_start_matches('#').trim_start()),
        None => (s, ""),
    }
}

/// Word-wrap `text` to `width` columns, prefixing every line with `indent`
/// spaces. Returns empty for empty text (callers print nothing).
///
/// `width == None` (not a terminal) means no wrapping at all — piped output
/// keeps each logical message on exactly one line, which is what makes it
/// greppable. Words longer than the available room are left overlong rather
/// than hard-split, since the long tokens here are file paths and commands
/// that must survive intact.
///
/// One divergence between the two paths, harmless today but worth knowing: the
/// wrapped path re-joins on `split_whitespace`, so it COLLAPSES internal
/// whitespace runs, while the `None` path emits `text` verbatim. Every string
/// this renders is single-spaced prose, so the paths agree; a future detail
/// carrying deliberate alignment would render differently piped vs. in a
/// terminal.
fn wrap_indented(text: &str, indent: usize, width: Option<usize>) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let pad = " ".repeat(indent);
    let Some(w) = width.filter(|w| w.saturating_sub(indent) >= MIN_WRAP_ROOM) else {
        return vec![format!("{pad}{text}")];
    };
    let room = w - indent;
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let add = if line.is_empty() { word.chars().count() } else { line.chars().count() + 1 + word.chars().count() };
        if !line.is_empty() && add > room {
            out.push(format!("{pad}{line}"));
            line = word.to_string();
        } else {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
    }
    if !line.is_empty() {
        out.push(format!("{pad}{line}"));
    }
    out
}

/// Truncate to `max` CHARS, eliding the MIDDLE with a `…`.
///
/// Chars, not bytes (so a multi-byte id can't be cut mid-character) and not
/// display columns: for the ASCII machine-minted ids this renders, one char is
/// one column, but a double-width glyph would be counted as one and drawn as
/// two. The same char-as-column assumption underlies `wrap_indented` and
/// `plan_layout`; it holds for every string this module renders today.
///
/// The elision is in the middle, not the tail, because darkmux's machine-minted
/// ids carry their discriminator as a SUFFIX
/// (`dispatch-code-reviewer-1785386551-4b71-0`): tail-truncating a screenful of
/// those renders every row as the identical string
/// `dispatch-code-reviewer-17853…`, which destroys exactly the identity the id
/// column exists to preserve. Keeping both ends costs nothing and tells the
/// rows apart. Observed directly at 46 columns while building #1569.
fn ellipsize(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max || max == 0 {
        return s.to_string();
    }
    if max == 1 {
        return "…".to_string();
    }
    let keep = max - 1; // one column for the `…`
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let chars: Vec<char> = s.chars().collect();
    let front: String = chars[..head].iter().collect();
    let back: String = chars[n - tail..].iter().collect();
    format!("{front}…{back}")
}

fn status_word(s: MissionStatus) -> &'static str {
    match s {
        MissionStatus::Active => "active",
        MissionStatus::Paused => "paused",
        MissionStatus::Finalized => "finalized",
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
            finalized_ts: None,
            paused_ts: None,
            source_input: None,
            ticket: None,
            spec: None,
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

    /// A `MissionView` carrying just enough to exercise the layout planner:
    /// the id sets the identity column, the phase counts set the mix column.
    fn view<'a>(m: &'a Mission, complete: usize, running: usize) -> MissionView<'a> {
        MissionView {
            m,
            total: complete + running,
            complete,
            running,
            planned: 0,
            abandoned: 0,
            drifts: Vec::new(),
        }
    }

    #[test]
    fn split_suggestion_separates_the_command_from_its_rationale() {
        let (cmd, note) = split_suggestion(
            "darkmux mission abort m1 --phase p2   # abandon just this blocked phase",
        );
        assert_eq!(cmd, "darkmux mission abort m1 --phase p2");
        assert_eq!(note, "abandon just this blocked phase");
    }

    #[test]
    fn split_suggestion_leaves_a_bare_command_whole() {
        // No `#` rationale: the whole string is the command and the note is
        // empty, so the renderer prints no extra line.
        let (cmd, note) = split_suggestion("darkmux mission finalize m1");
        assert_eq!(cmd, "darkmux mission finalize m1");
        assert_eq!(note, "");
    }

    #[test]
    fn split_suggestion_keeps_a_shell_comment_inside_the_command() {
        // A single `#` with no preceding double-space is part of the command
        // (e.g. a `--message '#1569'` argument), not a rationale separator.
        let (cmd, note) = split_suggestion("darkmux flow note --text 'fixes #1569'");
        assert_eq!(cmd, "darkmux flow note --text 'fixes #1569'");
        assert_eq!(note, "");
    }

    #[test]
    fn wrap_indented_wraps_to_width_and_indents_every_line() {
        let lines = wrap_indented("alpha beta gamma delta", 4, Some(16));
        // room = 16 - 4 = 12 columns of text per line.
        assert_eq!(lines, vec!["    alpha beta".to_string(), "    gamma delta".to_string()]);
        assert!(lines.iter().all(|l| l.chars().count() <= 16));
    }

    #[test]
    fn wrap_indented_does_not_wrap_when_output_is_not_a_terminal() {
        // Piped output keeps one logical message on one line so it stays
        // greppable — the same reason plan_layout adapts nothing at None.
        let long = "alpha beta gamma delta epsilon zeta eta theta";
        assert_eq!(wrap_indented(long, 4, None), vec![format!("    {long}")]);
    }

    #[test]
    fn wrap_indented_leaves_an_overlong_word_intact() {
        // Long tokens here are paths and commands; hard-splitting one would
        // corrupt it, so it overflows instead.
        let path = "/very/long/path/that/exceeds/the/room";
        let lines = wrap_indented(&format!("run {path} now"), 4, Some(20));
        // The overlong word gets its OWN line (it can't share one), so assert
        // it survives somewhere intact rather than assuming which line.
        assert!(lines.iter().any(|l| l.contains(path)), "path was split: {lines:?}");
        assert!(lines.iter().all(|l| l.starts_with("    ")));
    }

    #[test]
    fn wrap_indented_is_empty_for_empty_text() {
        assert!(wrap_indented("", 4, Some(80)).is_empty());
    }

    #[test]
    fn last_activity_takes_the_newest_present_timestamp() {
        let mut m = mission("m1", MissionStatus::Finalized);
        m.created_ts = 100;
        assert_eq!(last_activity(&m), 100, "created_ts alone is the floor");

        m.started_ts = Some(200);
        m.paused_ts = Some(400);
        m.finalized_ts = Some(300);
        // Deliberately out of chronological order: a mission can be paused
        // after being finalized on hand-edited data, and the sort must still
        // pick the newest stamp rather than trusting a field precedence.
        assert_eq!(last_activity(&m), 400);
    }

    /// A `MissionView` with drift attached, for ordering tests.
    fn drifted<'a>(m: &'a Mission) -> MissionView<'a> {
        let mut v = view(m, 1, 0);
        v.drifts.push(Drift { kind: "test", detail: "d".into(), suggest: vec![] });
        v
    }

    #[test]
    fn board_order_puts_drifted_missions_first() {
        // THE regression. The comparator previously read
        // `(b.is_empty()).cmp(&(a.is_empty()))` under a "drifted first" comment,
        // which sorted CLEAN first — inverting the one property that makes the
        // per-section limit safe (truncation drops from the tail, so drift must
        // lead or a cap hides exactly the rows that needed attention).
        let (ma, mb, mc, md) = (
            mission("m-clean-a", MissionStatus::Active),
            mission("m-drift-b", MissionStatus::Active),
            mission("m-clean-c", MissionStatus::Active),
            mission("m-drift-d", MissionStatus::Active),
        );
        let mut views = [view(&ma, 1, 0), drifted(&mb), view(&mc, 1, 0), drifted(&md)];
        views.sort_by(board_order);
        let ids: Vec<&str> = views.iter().map(|v| v.m.id.as_str()).collect();
        assert_eq!(ids, vec!["m-drift-b", "m-drift-d", "m-clean-a", "m-clean-c"]);
    }

    #[test]
    fn board_order_sorts_newest_first_within_equal_drift() {
        // Ids sort ascending b < c, activity sorts c newer — recency must win,
        // since mission ids are largely machine-minted and carry no work order.
        let mut older = mission("m-bbb", MissionStatus::Active);
        older.started_ts = Some(100);
        let mut newer = mission("m-ccc", MissionStatus::Active);
        newer.started_ts = Some(900);
        let mut views = [view(&older, 1, 0), view(&newer, 1, 0)];
        views.sort_by(board_order);
        assert_eq!(
            views.iter().map(|v| v.m.id.as_str()).collect::<Vec<_>>(),
            vec!["m-ccc", "m-bbb"],
            "newest-touched must lead despite sorting later by id"
        );
    }

    #[test]
    fn board_order_falls_back_to_id_when_drift_and_recency_tie() {
        let mut a = mission("m-zzz", MissionStatus::Active);
        let mut b = mission("m-aaa", MissionStatus::Active);
        a.created_ts = 500;
        b.created_ts = 500;
        let mut views = [view(&a, 1, 0), view(&b, 1, 0)];
        views.sort_by(board_order);
        assert_eq!(
            views.iter().map(|v| v.m.id.as_str()).collect::<Vec<_>>(),
            vec!["m-aaa", "m-zzz"],
            "identical drift + timestamps must order stably by id"
        );
    }

    #[test]
    fn drift_leads_so_a_section_limit_cannot_hide_an_attention_item() {
        // The pagination-safety property stated in run()'s docs, asserted
        // directly: after sorting, taking the first N rows (what the limit
        // does) retains every drifted mission while N >= the drift count.
        let ms: Vec<Mission> =
            (0..8).map(|i| mission(&format!("m-{i}"), MissionStatus::Finalized)).collect();
        let mut views: Vec<MissionView> = ms
            .iter()
            .enumerate()
            .map(|(i, m)| if i % 3 == 0 { drifted(m) } else { view(m, 1, 0) })
            .collect();
        let total_drift = views.iter().filter(|v| !v.drifts.is_empty()).count();
        views.sort_by(board_order);
        let kept = views.iter().take(total_drift).filter(|v| !v.drifts.is_empty()).count();
        assert_eq!(kept, total_drift, "a tail-truncating limit must not drop drifted rows");
    }

    #[test]
    fn plan_layout_shows_everything_when_output_is_not_a_terminal() {
        // `None` = piped/redirected. Nothing adapts and nothing is dropped, so
        // `mission status | grep` is byte-predictable regardless of the window
        // it ran in.
        let m = mission("a-very-long-machine-minted-mission-id-0001", MissionStatus::Active);
        let v = view(&m, 3, 1);
        let l = plan_layout([v].iter(), None);
        assert_eq!(l.id_width, 42);
        assert!(l.show_mix);
    }

    #[test]
    fn plan_layout_sizes_the_id_column_to_the_widest_shown_row() {
        let short = mission("m1", MissionStatus::Active);
        let long = mission("m-longer-id", MissionStatus::Active);
        let rows = [view(&short, 1, 0), view(&long, 1, 0)];
        let l = plan_layout(rows.iter(), Some(200));
        // Natural width, not the old hardcoded 30 — narrow boards stay narrow.
        assert_eq!(l.id_width, "m-longer-id".len());
        assert!(l.show_mix);
    }

    /// Render one board row exactly as `run()` does, so tests can assert the
    /// width of what is actually PRINTED rather than of the plan.
    ///
    /// Layout bugs here are off-by-N in a column budget, and a test that
    /// recomputes the same budget it is checking will agree with a wrong one —
    /// which is how a 2-column undercount survived its own unit test.
    fn render_row(v: &MissionView, layout: &Layout) -> String {
        let prog = format!("{}/{}", v.complete, v.total);
        let bar = progress_bar(v.complete, v.total);
        let id = ellipsize(&v.m.id, layout.id_width);
        if layout.show_mix {
            format!(
                "  ◆ {:<width$}  {:>5}  {}  {}",
                id,
                prog,
                bar,
                phase_mix(v),
                width = layout.id_width
            )
        } else {
            format!("  ◆ {:<width$}  {:>5}  {}", id, prog, bar, width = layout.id_width)
        }
    }

    /// The narrowest terminal a row can honor: the fixed columns plus the id
    /// floor. Below this the row overflows BY DESIGN (see `MIN_ID_COLS`).
    const NARROWEST_HONORABLE: usize = ROW_FIXED_COLS + MIN_ID_COLS;

    #[test]
    fn plan_layout_row_fits_every_width_it_can_honor() {
        // THE off-by-two regression, asserted against the RENDERED string
        // rather than a recomputed budget: at COLUMNS=51 an id(12) + mix(22)
        // row used to measure 51 and print 53. Sweeping widths also pins the
        // mix-shown/mix-dropped/id-truncated boundaries all at once.
        let m = mission("m-0123456789", MissionStatus::Active);
        for w in NARROWEST_HONORABLE..=90 {
            let layout = plan_layout([view(&m, 2, 1)].iter(), Some(w));
            let row = render_row(&view(&m, 2, 1), &layout);
            let cols = row.chars().count();
            assert!(
                cols <= w,
                "at COLUMNS={w} the row rendered {cols} cols (show_mix={}): {row:?}",
                layout.show_mix
            );
        }
    }

    #[test]
    fn below_the_id_floor_the_row_overflows_by_design_but_stays_bounded() {
        // Deliberate, and worth pinning so it can't drift into unbounded
        // overflow: under ~29 columns the id floor wins over fitting the row,
        // because an id truncated to 3 chars identifies nothing. The row is
        // still exactly the floor row — never wider.
        let m = mission("m-0123456789-0123456789", MissionStatus::Active);
        for w in 1..NARROWEST_HONORABLE {
            let layout = plan_layout([view(&m, 2, 1)].iter(), Some(w));
            assert_eq!(layout.id_width, MIN_ID_COLS, "at COLUMNS={w}");
            assert!(!layout.show_mix, "at COLUMNS={w} the mix must be gone before this point");
            let cols = render_row(&view(&m, 2, 1), &layout).chars().count();
            assert_eq!(cols, NARROWEST_HONORABLE, "at COLUMNS={w} overflow must stay bounded");
        }
    }

    #[test]
    fn plan_layout_drops_the_mix_before_truncating_the_id() {
        let m = mission("m-0123456789", MissionStatus::Active);
        let v = view(&m, 2, 1); // mix = "2 complete · 1 running"
        let mix_cols = phase_mix(&v).chars().count();
        let id_cols = m.id.chars().count();

        // One column short of fitting the mix: the mix goes, the id survives
        // intact — the fraction and bar already carry the mix's information.
        let tight = id_cols + ROW_FIXED_COLS + MIX_GAP_COLS + mix_cols - 1;
        let l = plan_layout([view(&m, 2, 1)].iter(), Some(tight));
        assert_eq!(l.id_width, id_cols, "id must not shrink while a mix column is still droppable");
        assert!(!l.show_mix);

        // And one column MORE than the widest with-mix row does fit it.
        let exact = id_cols + ROW_FIXED_COLS + MIX_GAP_COLS + mix_cols;
        assert!(plan_layout([view(&m, 2, 1)].iter(), Some(exact)).show_mix);
    }

    #[test]
    fn plan_layout_truncates_the_id_only_when_even_that_cannot_fit() {
        let m = mission("m-0123456789-0123456789", MissionStatus::Active);
        let l = plan_layout([view(&m, 1, 0)].iter(), Some(ROW_FIXED_COLS + 15));
        assert_eq!(l.id_width, 15);
        assert!(!l.show_mix);
    }

    #[test]
    fn plan_layout_never_shrinks_the_id_below_the_legible_floor() {
        // An absurdly narrow terminal overflows the row rather than rendering
        // an id too short to identify anything.
        let m = mission("m-0123456789-0123456789", MissionStatus::Active);
        let l = plan_layout([view(&m, 1, 0)].iter(), Some(10));
        assert_eq!(l.id_width, MIN_ID_COLS);
    }

    #[test]
    fn ellipsize_preserves_short_ids_and_marks_truncated_ones() {
        assert_eq!(ellipsize("m1", 10), "m1");
        assert_eq!(ellipsize("m-0123456789", 12), "m-0123456789", "exact fit is untouched");
        let cut = ellipsize("m-0123456789", 6);
        assert_eq!(cut, "m-0…89");
        assert_eq!(cut.chars().count(), 6, "must not exceed the budget");
        // Char-counted, not byte-counted: a multi-byte id must not be cut
        // mid-character (which would emit invalid UTF-8 to the terminal).
        assert_eq!(ellipsize("mécanique", 4), "mé…e");
        assert_eq!(ellipsize("m-0123456789", 1), "…");
    }

    #[test]
    fn ellipsize_keeps_suffix_discriminated_ids_distinguishable() {
        // The real regression this guards: darkmux's machine-minted ids differ
        // only in their SUFFIX, so tail-truncation collapses a whole screenful
        // into one indistinguishable string. Middle elision keeps them apart.
        let a = ellipsize("dispatch-code-reviewer-1785386551-4b71-0", 20);
        let b = ellipsize("dispatch-code-reviewer-1785384819-157-0", 20);
        assert_ne!(a, b, "rows differing only by suffix must not render identically");
        assert!(a.ends_with("4b71-0"), "the discriminating suffix survives: {a}");
        assert!(a.starts_with("dispatch-"), "the identifying prefix survives: {a}");
        assert_eq!(a.chars().count(), 20);
    }

    #[test]
    fn finalized_mission_with_open_phase_no_longer_drifts() {
        // (#1463) The "finalized-with-open-phase" arm retired: `mission
        // finalize` / `mission abort` reconcile every phase to terminal as
        // part of closing, so this is no longer a reachable state — and a
        // Finalized mission never surfaces a drift on this axis anymore,
        // even if a hand-edited JSON produced one. (Legacy on-disk data is a
        // `mission finalize`/`abort` re-run away from clean.)
        let m = mission("m1", MissionStatus::Finalized);
        let running = phase("s1", "m1", PhaseStatus::Running);
        let planned = phase("s2", "m1", PhaseStatus::Planned);
        assert!(detect_drift(&m, &[&running, &planned], 0, 14).is_empty());
    }

    #[test]
    fn finalized_mission_all_terminal_is_clean() {
        let m = mission("m1", MissionStatus::Finalized);
        let s = phase("s1", "m1", PhaseStatus::Complete);
        assert!(detect_drift(&m, &[&s], 0, 14).is_empty());
    }

    #[test]
    fn active_mission_all_terminal_suggests_finalize() {
        let m = mission("m1", MissionStatus::Active);
        let s = phase("s1", "m1", PhaseStatus::Complete);
        let d = detect_drift(&m, &[&s], 0, 14);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, "done-not-finalized");
        assert!(d[0].suggest[0].contains("mission finalize m1"));
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
        // happening, this is `done-not-finalized`/normal territory, not stale.
        let mut m = mission("m1", MissionStatus::Active);
        m.started_ts = Some(0);
        let s = phase("s1", "m1", PhaseStatus::Complete);
        let d = detect_drift(&m, &[&s], 30 * 86_400, 14);
        // `done-not-finalized` fires (all terminal + complete>0), but NOT
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
        // (#1463 CONSIDER 5) The suggestion must scope the teardown to the ONE
        // blocked phase (`--phase blocked`), not a bare whole-mission abort that
        // would abandon every healthy phase too.
        assert!(d.iter().any(|dr| dr.kind == "unreachable-phase"
            && dr.detail.contains("blocked")
            && dr.suggest.iter().any(|c| c.contains("mission abort m1 --phase blocked"))));
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
            finalized_ts: None,
            paused_ts: None,
            source_input: None,
            ticket: None,
            spec: None,
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
