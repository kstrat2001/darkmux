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

/// (#1569 packet A) The daemon URL a mission id links to.
///
/// The id IS percent-encoded: mission ids are not guaranteed path-safe, and
/// an unencoded one would emit extra path segments that resolve to the wrong
/// route or to nothing.
///
/// **What encoding does and does not buy** (#1593 gate — the first version of
/// this comment implied more): it makes the URL *well-formed*, not
/// *resolvable*. The daemon's `mission_graph_json_handler` gates on
/// `is_valid_catalog_id` (`[A-Za-z0-9-_.:]`, ≤128 chars) AFTER decoding, so an
/// id containing `/`, `@`, `?`, `#`, non-ASCII, or over 128 chars produces a
/// correct-looking link to a guaranteed 400. Encoding is still right — a
/// well-formed link that 400s beats a malformed one that hits an unrelated
/// route — but it is not a fix for out-of-charset ids.
///
/// Every real id on disk today is slugified and passes. The live constraint
/// is for #1563: whatever charset that fix mints for pr-review ids must stay
/// inside `is_valid_catalog_id`, or these links go dead for exactly the ids it
/// introduces. `:` is in the allowed set and round-trips correctly; `/` and
/// `@` are not.
///
/// Encoding is inline rather than a new dependency, per this repo's
/// small-dep convention: the rule needed here is one line of RFC 3986
/// unreserved-set logic, not a crate.
fn mission_url(base: &str, id: &str) -> String {
    let encoded: String = id
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect();
    // `base` always carries its trailing slash (see `viewer_link_base`).
    format!("{base}mission/{encoded}/graph")
}

/// A deep link to another panel, but ONLY when this process is rendering
/// into the console (`DARKMUX_PANEL`, set by the serve daemon's panel
/// spawner) and is not already the target.
///
/// The point is that the ADVICE has to match the surface. "`--all` for every
/// mission" is actionable in a terminal and a dead end in a panel, where
/// there is no prompt to type it at — the operator hit exactly that. The verb
/// fixes it here rather than the viewer pattern-matching this text, because
/// that matching is the twin drift `/panel/:id` exists to kill: the flag and
/// its link are one edit, in one file.
fn panel_deep_link(link_base: &str, target: &str) -> Option<String> {
    let current = std::env::var("DARKMUX_PANEL").ok()?;
    if current == target {
        return None;
    }
    // `link_base` always carries its trailing slash (see `viewer_link_base`).
    Some(format!("{link_base}#lens=console&panel={target}"))
}

/// (#1612) What the row CALLS a mission.
///
/// The id is a mint artifact — `dispatch-code-reviewer-1785589698-5d6a-0` —
/// and on a phone it ate two thirds of the width for the least informative
/// thing on the line, pushing progress off the edge. Meanwhile every mission
/// already carries a `description` (92/92 populated on this board) and it was
/// going unshown.
///
/// The descriptions come in two measured shapes, which is what makes this a
/// lookup rather than a truncation (92 real missions: 59 / 33):
///   - operator-named missions carry prose — "PR review — kstrat2001/darkmux@…";
///   - auto-minted ones carry "dispatch: code-reviewer", i.e. the role.
///
/// Both beat the id, and the `dispatch: ` prefix is noise once the row's own
/// glyph already distinguishes a single-role dispatch from a graph, so it is
/// stripped. The id is not lost: the row is an OSC 8 link, so it is one click
/// away, and any row that needs an id typed carries it verbatim in the drift
/// suggestion printed directly beneath it.
///
/// Falls back to the id when a description is genuinely absent — an id is a
/// poor label but never a wrong one.
fn display_label(m: &Mission) -> &str {
    let d = m.description.trim();
    if d.is_empty() {
        return &m.id;
    }
    d.strip_prefix("dispatch: ").unwrap_or(d)
}

/// (#1612) Single-role dispatch vs multi-phase graph, in one column-safe glyph.
///
/// The operator asked whether a row could say "graph" versus "role" and floated
/// an emoji. Emoji are DOUBLE-WIDTH, and every column budget in this module is
/// exact arithmetic over `chars().count()` — one emoji would silently overflow
/// every row it appeared on. These two are single-width, so the distinction is
/// free: it costs no column at all, replacing a marker that was already there.
fn kind_glyph(total_phases: usize) -> &'static str {
    if total_phases > 1 {
        "◆"
    } else {
        "•"
    }
}

/// (#1612) A stable short handle for a mission, or `None` when the id has no
/// discriminating token.
///
/// The operator's ask was "a number" — something short that tells two otherwise
/// identical rows apart. Both minting paths already end in one: `mint_run_id`
/// emits `<config>-<secs>-<hex6>` (`review-1785400940-136e76`), and the older
/// dispatch path emits `dispatch-<role>-<secs>-<hex>-<n>`.
///
/// So: scan segments right-to-left for the first all-hex run of at least
/// `MIN_HANDLE_HEX` chars. A HEURISTIC, deliberately — the two formats are not
/// one, and hand-authored ids (`doom-loop-m4`, `104-daemon-observability`) match
/// nothing and correctly yield `None`. It is display-only and drops to nothing
/// on no match, so a wrong guess costs a column, never correctness.
fn short_handle(id: &str) -> Option<&str> {
    id.rsplit('-').find(|seg| {
        seg.len() >= MIN_HANDLE_HEX && seg.chars().all(|c| c.is_ascii_hexdigit())
    })
}

/// Shortest run of hex that reads as a deliberate discriminator rather than an
/// accident. Below 4, ordinary id fragments (`-0`, `-5`, a version `-2`) start
/// matching and the column fills with noise.
const MIN_HANDLE_HEX: usize = 4;

/// (#1612) Compact "how long ago", in at most `AGE_COLS` columns.
///
/// One unit, never two — `3d` not `3d 4h`. The board answers "what needs me
/// now"; the difference between 3d and 3d4h has never changed that answer, and
/// the second unit costs the columns the name needs. Rounds DOWN, so a row
/// never claims to be older than it is.
fn relative_age(now: u64, then: u64) -> String {
    let secs = now.saturating_sub(then);
    match secs {
        0..=59 => "now".to_string(),
        60..=3_599 => format!("{}m", secs / 60),
        3_600..=86_399 => format!("{}h", secs / 3_600),
        86_400..=2_591_999 => format!("{}d", secs / 86_400),
        _ => format!("{}w", secs / 604_800),
    }
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
        // (#1582) One entry PER COMMAND, not one entry whose rationale
        // mentions two more commands in prose. Only the pre-`#` segment gets
        // the never-wrapped verbatim treatment, so an `abort`/`finalize`
        // buried in the rationale was word-wrapped with a 10-space indent
        // injected mid-command — unpasteable, which is the one thing the
        // #1569 rule exists to prevent. The prose now only explains the
        // CHOICE between them; the commands themselves are copyable lines.
        suggest: vec![
            "darkmux mission status --json   # inspect the phase details first".to_string(),
            format!(
                "darkmux mission abort {id}   # …then this, to tear the stalled mission down",
                id = m.id
            ),
            format!(
                "darkmux mission finalize {id}   # …or this instead, if the work is actually done",
                id = m.id
            ),
        ],
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

    let mut blocked: Vec<&str> = Vec::new();
    // Non-terminal phases that are NOT blocked — the ones a whole-mission
    // abort would destroy that the per-phase teardown would spare. Whether
    // any exist decides whether the bare-abort caveat below is a real warning
    // or noise (see its comment).
    //
    // A RUNNING phase positioned after the abandoned ancestor counts as
    // salvageable, which is deliberate and NOT an oversight of the linearity
    // rule: strict linearity (#1341) is this detector's heuristic, not a
    // lifecycle invariant — `lifecycle::phase_start` has no ancestor gate, so
    // a Running phase there is genuinely live work, and `mission abort` is
    // exactly what would reconcile it to Abandoned. It is real collateral, so
    // the caveat must count it. (Only PLANNED phases after the dead ancestor
    // are treated as blocked, which is what keeps the two sets disjoint.)
    let mut salvageable = 0usize;
    let mut dead_ancestor = false;
    for phase_id in &m.phase_ids {
        let Some(phase) = phase_by_id.get(phase_id.as_str()) else { continue };
        let live = matches!(phase.status, PhaseStatus::Planned | PhaseStatus::Running);
        if dead_ancestor && phase.status == PhaseStatus::Planned {
            blocked.push(phase.id.as_str());
        } else if live {
            salvageable += 1;
        }
        if phase.status == PhaseStatus::Abandoned {
            dead_ancestor = true;
        }
    }
    if blocked.is_empty() {
        return Vec::new();
    }

    // (#1582) ONE drift for the whole situation, not one per blocked phase.
    // Every sibling shared the same abandoned ancestor and so emitted the
    // same four lines of rationale verbatim — on a real board that was ~8 of
    // 21 default lines saying nothing new. The rationale is the same fact
    // whether one phase or five are blocked, so it is stated once in
    // `detail`; the per-phase specifics ride the `suggest` list, where each
    // command already gets its own line and the never-wrapped verbatim
    // treatment.
    //
    // This deliberately narrows `--json`'s drift array from N entries to 1
    // for this kind. That is the more accurate model — it is one problem
    // with N instances, not N problems — and the per-phase detail is not
    // lost: it is in `detail` and in one `suggest` entry per phase. Nothing
    // counts drift ENTRIES (the attention rollup counts missions carrying
    // any drift), so no consumer's arithmetic changes.
    let names = blocked.iter().map(|p| format!("'{p}'")).collect::<Vec<_>>().join(", ");
    let subject = if blocked.len() == 1 {
        format!("phase {names} can never run")
    } else {
        format!("{n} phases can never run ({names})", n = blocked.len())
    };
    // (#1463 CONSIDER 5, re-scoped by #1582) This line distinguishes two
    // commands rather than forbidding one. `stale-active` can fire on the
    // SAME mission and offer `mission abort <id>` as a copyable command, so
    // phrasing this as a prohibition made the board warn against a command it
    // was simultaneously recommending. They are not in conflict — they answer
    // different questions ("give up on this mission" vs "unblock it") — and
    // saying so is what makes both readable together.
    //
    // It stays prose, and the whole-mission abort never becomes a copyable
    // `→` line HERE: this drift's own recommendation is the per-phase
    // teardown, and offering both as commands would just restate the
    // ambiguity it exists to resolve.
    //
    // With nothing salvageable the distinction is vacuous — the two commands
    // would destroy exactly the same work — so the line is dropped entirely
    // rather than printed as a difference that makes no difference.
    let caveat = if salvageable > 0 {
        format!(
            ". A bare `darkmux mission abort {mid}` ends the WHOLE mission, including \
             {salvageable} {phase} that can still run — scope it per-phase instead if you \
             intend to keep this mission going",
            mid = m.id,
            phase = if salvageable == 1 { "phase" } else { "phases" }
        )
    } else {
        String::new()
    };
    let detail = format!(
        "{subject} — an earlier phase in this mission was abandoned{caveat}. The mission closes \
         on its own once every phase is terminal"
    );

    // No per-command rationale: `detail` just said what these are for, and
    // repeating "abandon just this blocked phase" once per sibling would
    // reintroduce the duplication this drift was collapsed to remove — the
    // same defect one level down.
    let suggest: Vec<String> = blocked
        .iter()
        .map(|pid| format!("darkmux mission abort {mid} --phase {pid}", mid = m.id))
        .collect();

    vec![Drift { kind: "unreachable-phase", detail, suggest }]
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
    // (#1569 packet A) Resolved ONCE per board, not per row: on a hub/peer
    // this may spawn `tailscale serve status --json`, and doing that 82 times
    // for an 82-mission board would be absurd. It short-circuits to loopback
    // without spawning when the machine declares itself standalone, or when
    // no links will be emitted at all.
    //
    // NB the old "isn't a TTY" spelling of that second case stopped being
    // true in B1: a panel spawn is a pipe but sets CLICOLOR_FORCE, so it DOES
    // resolve — bounded by the daemon's own panel cache.
    let link_base = darkmux_doctor::viewer_link_base(8765);
    let all_link = panel_deep_link(&link_base, "mission-status-all");
    // The link is one affordance for the whole board, not one per section:
    // it goes to the same place from every group, and Active + Paused +
    // Finalized all overflowing would otherwise stack three identical rows.
    let mut all_link_shown = false;
    if views.is_empty() {
        // (#1582) The prose wraps; the command does not. Same rule the drift
        // suggestions follow, for the same reason — this is the one command a
        // brand-new operator will copy, and it is the worst possible one to
        // break across a line with an indent injected into the middle.
        for line in wrap_indented("no missions yet — propose one with:", 2, width) {
            println!("{}", style::dim(&line));
        }
        println!("  {} darkmux mission propose", style::dim("→"));
        return Ok(0);
    }

    // Section membership first, so the layout can be planned from exactly the
    // rows that will be printed (and stay aligned across every section).
    let groups: Vec<(MissionStatus, Vec<&MissionView>)> =
        [
            MissionStatus::Active,
            MissionStatus::Paused,
            MissionStatus::Finalized,
            // (#1626) Its own section, last: a torn-down mission is terminal but
            // is NOT a success, and folding it under FINALIZED is what let 6 of
            // 51 phase-bearing missions read as finished work that never ran.
            MissionStatus::Aborted,
        ]
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
            let name = ellipsize(display_label(v.m), layout.name_width);
            // (#1569 packet A) Pad BEFORE linking, and by the VISIBLE width:
            // `{:<width$}` counts the OSC 8 escape bytes, so formatting a
            // linkified name would silently destroy the column alignment the
            // whole layout planner exists to maintain. The link wraps only
            // the name text; the padding stays outside it, so the clickable
            // target is the name rather than a run of trailing whitespace.
            let name_cell = format!(
                "{}{}",
                style::link(&mission_url(&link_base, &v.m.id), &name),
                " ".repeat(layout.name_width.saturating_sub(name.chars().count()))
            );
            // (#1612) Dim, and blank-padded rather than omitted, so a board
            // where only some ids carry a handle keeps one straight column.
            let handle_cell = if layout.show_handle {
                let h = short_handle(&v.m.id).unwrap_or("");
                format!(
                    "  {}{}",
                    style::dim(h),
                    " ".repeat(layout.handle_width.saturating_sub(h.chars().count()))
                )
            } else {
                String::new()
            };
            // Right-aligned by hand for the same reason the name is padded by
            // hand: `{:>width$}` would count `style::dim`'s escape bytes and
            // silently eat the alignment.
            let age = relative_age(now, last_activity(v.m));
            let age_cell = format!(
                "{}{}",
                " ".repeat(AGE_COLS.saturating_sub(age.chars().count())),
                style::dim(&age)
            );
            let row = format!(
                "  {} {}{}  {}  {:>5}  {}",
                kind_glyph(v.total),
                name_cell,
                handle_cell,
                age_cell,
                prog,
                bar,
            );
            if layout.show_mix {
                println!("{row}  {}", style::dim(&phase_mix(v)));
            } else {
                // Narrow terminal: the mix is dropped rather than the name, the
                // age or the progress, because it is the one column whose
                // information the others already carry.
                println!("{row}");
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
            // In a panel the flag names itself once, as a link, at the end of
            // the block — so the two overflow lines don't each repeat advice
            // the operator cannot take.
            let more = if all_link.is_some() {
                format!("… {} more ({} of {} shown)", g.len() - shown, shown, g.len())
            } else {
                format!(
                    "… {} more ({} of {} shown) — `--all` for every mission",
                    g.len() - shown,
                    shown,
                    g.len()
                )
            };
            for line in wrap_indented(&more, 2, width) {
                println!("{}", style::dim(&line));
            }
            if hidden_drift > 0 {
                any_drift_hidden = true;
                // Never let a limit silently swallow an attention item.
                let warn = format!(
                    "⚠ {} hidden mission{} need{} attention{}",
                    hidden_drift,
                    if hidden_drift == 1 { "" } else { "s" },
                    if hidden_drift == 1 { "s" } else { "" },
                    if all_link.is_some() { "" } else { " — run with `--all`" }
                );
                for line in wrap_indented(&warn, 2, width) {
                    println!("{}", style::warn(&line));
                }
            }
            if let Some(url) = &all_link {
                if !all_link_shown {
                    all_link_shown = true;
                    println!("  {}", style::link(url, "→ show every mission"));
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
            if !any_drift_hidden {
                ""
            } else if all_link.is_some() {
                " (some are hidden — open the full board above)"
            } else {
                " (some are hidden — `--all` to see them)"
            }
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
        // (#1626) Aborted is closed history like Finalized — recent context,
        // not the question the board answers.
        MissionStatus::Finalized | MissionStatus::Aborted => 3,
        MissionStatus::Active | MissionStatus::Paused => 10,
    }
}

/// Fixed per-row cost of a row with NEITHER optional column: `"  ◆ "` (4) +
/// the age gap (2) + the age field (3) + a gap (2) + the 5-wide progress field
/// + a gap (2) + the 4-wide bar = 22.
///
/// Exactly this and no more: a row that also shows the mix or the handle pays
/// one ADDITIONAL gap each (`MIX_GAP_COLS` / `HANDLE_GAP_COLS`). Conflating the
/// gap into the base let `plan_layout` judge a with-mix row 2 columns narrower
/// than it renders, so at `COLUMNS=51` an id(12) + mix(22) row measured 51 and
/// printed 53 — hard-wrapping on exactly the terminal width the adaptation
/// exists to respect. The same trap now exists twice; the arithmetic is pinned
/// by `plan_layout_row_fits_every_width_it_can_honor`, which measures the
/// RENDERED string rather than recomputing this budget. It earned its keep
/// immediately: the first draft of this constant said 23.
const ROW_FIXED_COLS: usize = 22;

/// Width of the age field. `now`/`59m`/`23h`/`29d`/`99w` — three columns covers
/// every value `relative_age` can emit below a hundred weeks.
const AGE_COLS: usize = 3;

/// The extra 2-space gap between the bar and the mix column, paid only when the
/// mix is shown. See `ROW_FIXED_COLS`.
const MIX_GAP_COLS: usize = 2;

/// The extra 2-space gap between the name and the handle column, paid only when
/// the handle is shown. See `ROW_FIXED_COLS`.
const HANDLE_GAP_COLS: usize = 2;

/// Never shrink the name column below this — a name truncated to a few
/// characters identifies nothing, which defeats the point of keeping it.
const MIN_NAME_COLS: usize = 12;

/// Below this much room for text, `wrap_indented` stops wrapping and emits one
/// overlong line instead. Wrapping prose into a 3-column gutter produces
/// something less readable than an overflowing line, not more.
///
/// Deliberately its OWN constant rather than reusing `MIN_NAME_COLS`: the two
/// happen to share a value but answer unrelated questions (how short an id may
/// be truncated vs. how narrow a paragraph is worth wrapping), so tying them
/// together would make one silently move when the other is tuned.
const MIN_WRAP_ROOM: usize = 12;

/// How one board row is laid out at the current terminal width.
#[derive(Debug, PartialEq)]
struct Layout {
    name_width: usize,
    handle_width: usize,
    show_handle: bool,
    show_mix: bool,
}

/// Plan the row layout from the rows that will actually be printed.
///
/// Degradation order is deliberate, widest-terminal first:
///   1. the phase mix goes — the progress fraction and bar already carry it;
///   2. then the handle — the age already tells two same-named rows apart, and
///      the full id is a click away on the row's own link;
///   3. only then does the name get truncated.
///
/// The age is never dropped. It is 3 columns, it is the one field that answers
/// "is this still relevant", and it is what makes step 2 survivable.
///
/// `width == None` means output isn't a terminal, so nothing is adapted and
/// nothing is dropped — piped output stays complete.
fn plan_layout<'a>(
    rows: impl Iterator<Item = &'a MissionView<'a>>,
    width: Option<usize>,
) -> Layout {
    let (max_name, max_handle, max_mix) = rows.fold((0, 0, 0), |(n, h, x), v| {
        (
            n.max(display_label(v.m).chars().count()),
            h.max(short_handle(&v.m.id).map_or(0, |s| s.chars().count())),
            x.max(phase_mix(v).chars().count()),
        )
    });
    // A handle column is only ever planned if some row actually has one —
    // otherwise every row would pay two gap columns for a run of blanks.
    let handle_cost = if max_handle == 0 { 0 } else { HANDLE_GAP_COLS + max_handle };
    let with_handle = |name_width: usize, show_mix: bool| Layout {
        name_width,
        handle_width: max_handle,
        show_handle: max_handle > 0,
        show_mix,
    };
    let Some(w) = width else {
        return with_handle(max_name, true);
    };
    if max_name + ROW_FIXED_COLS + handle_cost + MIX_GAP_COLS + max_mix <= w {
        with_handle(max_name, true)
    } else if max_name + ROW_FIXED_COLS + handle_cost <= w {
        with_handle(max_name, false)
    } else if max_name + ROW_FIXED_COLS <= w {
        Layout { name_width: max_name, handle_width: 0, show_handle: false, show_mix: false }
    } else {
        Layout {
            name_width: w.saturating_sub(ROW_FIXED_COLS).max(MIN_NAME_COLS),
            handle_width: 0,
            show_handle: false,
            show_mix: false,
        }
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
        MissionStatus::Aborted => "aborted",
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
            // (#1612) Mirror the id: the row labels from `description` now, so
            // a fixed "d" would make every width test measure a 1-column name
            // and silently stop exercising the arithmetic it exists to pin.
            description: id.into(),
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
        assert_eq!(l.name_width, 42);
        assert!(l.show_mix);
    }

    #[test]
    fn plan_layout_sizes_the_id_column_to_the_widest_shown_row() {
        let short = mission("m1", MissionStatus::Active);
        let long = mission("m-longer-id", MissionStatus::Active);
        let rows = [view(&short, 1, 0), view(&long, 1, 0)];
        let l = plan_layout(rows.iter(), Some(200));
        // Natural width, not the old hardcoded 30 — narrow boards stay narrow.
        assert_eq!(l.name_width, "m-longer-id".len());
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
        let name = ellipsize(display_label(v.m), layout.name_width);
        let handle = if layout.show_handle {
            let h = short_handle(&v.m.id).unwrap_or("");
            format!("  {:<width$}", h, width = layout.handle_width)
        } else {
            String::new()
        };
        // Any age of the right WIDTH exercises the same budget — the row's
        // column cost is `AGE_COLS`, never the particular value.
        let age = format!("{:>width$}", "9d", width = AGE_COLS);
        let row = format!(
            "  {} {:<width$}{}  {}  {:>5}  {}",
            kind_glyph(v.total),
            name,
            handle,
            age,
            prog,
            bar,
            width = layout.name_width
        );
        if layout.show_mix {
            format!("{row}  {}", phase_mix(v))
        } else {
            row
        }
    }

    /// The narrowest terminal a row can honor: the fixed columns plus the id
    /// floor. Below this the row overflows BY DESIGN (see `MIN_NAME_COLS`).
    const NARROWEST_HONORABLE: usize = ROW_FIXED_COLS + MIN_NAME_COLS;

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
            assert_eq!(layout.name_width, MIN_NAME_COLS, "at COLUMNS={w}");
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
        let name_cols = display_label(&m).chars().count();
        // (#1612) This id's trailing `0123456789` is all hex, so the row also
        // carries a handle — and the mix is only droppable AFTER the widest
        // row it must sit beside is accounted for. Omitting this term is what
        // made this test fail when the handle column landed.
        let handle_cols =
            HANDLE_GAP_COLS + short_handle(&m.id).map_or(0, |h| h.chars().count());
        let base = name_cols + ROW_FIXED_COLS + handle_cols;

        // One column short of fitting the mix: the mix goes, the name AND the
        // handle survive intact — the fraction and bar already carry the mix's
        // information, so it is the first thing worth losing.
        let l = plan_layout([view(&m, 2, 1)].iter(), Some(base + MIX_GAP_COLS + mix_cols - 1));
        assert_eq!(l.name_width, name_cols, "the name must not shrink while the mix is droppable");
        assert!(l.show_handle, "the handle must not go before the mix");
        assert!(!l.show_mix);

        // And one column MORE than the widest with-mix row does fit it.
        assert!(plan_layout([view(&m, 2, 1)].iter(), Some(base + MIX_GAP_COLS + mix_cols)).show_mix);
    }

    #[test]
    fn plan_layout_truncates_the_id_only_when_even_that_cannot_fit() {
        let m = mission("m-0123456789-0123456789", MissionStatus::Active);
        let l = plan_layout([view(&m, 1, 0)].iter(), Some(ROW_FIXED_COLS + 15));
        assert_eq!(l.name_width, 15);
        assert!(!l.show_mix);
    }

    #[test]
    fn plan_layout_never_shrinks_the_id_below_the_legible_floor() {
        // An absurdly narrow terminal overflows the row rather than rendering
        // an id too short to identify anything.
        let m = mission("m-0123456789-0123456789", MissionStatus::Active);
        let l = plan_layout([view(&m, 1, 0)].iter(), Some(10));
        assert_eq!(l.name_width, MIN_NAME_COLS);
    }

    // ── (#1612) What a row actually calls a mission ─────────────────────────

    /// The two description shapes measured on a real 92-mission board, and the
    /// fallback. The `dispatch: ` prefix goes because `kind_glyph` already
    /// carries "this is one role, not a graph".
    #[test]
    fn display_label_prefers_the_description_and_drops_the_dispatch_prefix() {
        let mut m = mission("dispatch-code-reviewer-1785589698-5d6a-0", MissionStatus::Finalized);
        m.description = "dispatch: code-reviewer".into();
        assert_eq!(display_label(&m), "code-reviewer");

        m.description = "PR review — kstrat2001/darkmux@38031a5".into();
        assert_eq!(display_label(&m), "PR review — kstrat2001/darkmux@38031a5");

        // No description at all: an id is a poor label, never a wrong one.
        m.description = "   ".into();
        assert_eq!(display_label(&m), "dispatch-code-reviewer-1785589698-5d6a-0");
    }

    /// Both minting formats seen in the wild yield a handle; hand-authored ids
    /// correctly yield none rather than a meaningless word fragment.
    #[test]
    fn short_handle_finds_the_mint_discriminator_or_nothing() {
        // `mint_run_id`: <config>-<secs>-<hex6>
        assert_eq!(short_handle("review-1785400940-136e76"), Some("136e76"));
        // the older dispatch path: <config>-<role>-<secs>-<hex>-<n>
        assert_eq!(short_handle("dispatch-code-reviewer-1785589698-5d6a-0"), Some("5d6a"));
        // Hand-authored ids have no discriminator — a column of word fragments
        // would be worse than an empty column.
        assert_eq!(short_handle("doom-loop-m4"), None);
        assert_eq!(short_handle("104-daemon-observability"), None);
        // The `-0` counter is below the hex floor, so it can never be picked as
        // the handle in preference to the real one.
        assert!(short_handle("dispatch-x-1785589698-5d6a-0") != Some("0"));
    }

    /// Rounds DOWN and emits exactly one unit, so the column is `AGE_COLS` wide
    /// for every value it can produce short of a hundred weeks.
    #[test]
    fn relative_age_is_one_unit_and_fits_its_column() {
        assert_eq!(relative_age(30, 0), "now");
        assert_eq!(relative_age(59, 0), "now");
        assert_eq!(relative_age(60, 0), "1m");
        assert_eq!(relative_age(3_599, 0), "59m");
        assert_eq!(relative_age(3_600, 0), "1h");
        assert_eq!(relative_age(86_399, 0), "23h");
        assert_eq!(relative_age(86_400, 0), "1d");
        assert_eq!(relative_age(2_591_999, 0), "29d");
        assert_eq!(relative_age(2_592_000, 0), "4w");
        // A clock that went backwards must not underflow into a huge age.
        assert_eq!(relative_age(0, 5_000), "now");
        for secs in [0u64, 61, 4_000, 90_000, 3_000_000, 60_000_000] {
            assert!(
                relative_age(secs, 0).chars().count() <= AGE_COLS,
                "{secs}s rendered wider than AGE_COLS"
            );
        }
    }

    /// Single-width, both of them — an emoji here would overflow every row it
    /// appeared on, because every budget in this module is exact `chars()` math.
    #[test]
    fn kind_glyph_separates_graphs_from_single_role_dispatches() {
        assert_eq!(kind_glyph(3), "◆");
        assert_eq!(kind_glyph(1), "•");
        assert_eq!(kind_glyph(0), "•");
        for total in [0, 1, 2, 9] {
            assert_eq!(kind_glyph(total).chars().count(), 1);
        }
    }

    /// The new rung in the ladder. Between "everything fits" and "truncate the
    /// name" the handle goes — the age still tells two same-named rows apart,
    /// and the full id is on the row's own link.
    #[test]
    fn plan_layout_drops_the_handle_before_truncating_the_name() {
        let m = mission("review-1785400940-136e76", MissionStatus::Active);
        let name_cols = display_label(&m).chars().count();
        let handle_cols = short_handle(&m.id).unwrap().chars().count();

        // Exactly wide enough for the handle but not the mix: handle stays.
        let with_handle = name_cols + ROW_FIXED_COLS + HANDLE_GAP_COLS + handle_cols;
        let l = plan_layout([view(&m, 2, 1)].iter(), Some(with_handle));
        assert!(l.show_handle && !l.show_mix);
        assert_eq!(l.name_width, name_cols);

        // One column short: the handle goes, the name survives INTACT.
        let l = plan_layout([view(&m, 2, 1)].iter(), Some(with_handle - 1));
        assert!(!l.show_handle);
        assert_eq!(l.name_width, name_cols, "the name must not shrink while the handle is droppable");

        // Only below the no-handle row does the name finally truncate.
        let l = plan_layout([view(&m, 2, 1)].iter(), Some(name_cols + ROW_FIXED_COLS - 1));
        assert!(!l.show_handle);
        assert!(l.name_width < name_cols);
    }

    /// A board whose ids carry no discriminator must not pay two gap columns
    /// for a column of blanks.
    #[test]
    fn plan_layout_plans_no_handle_column_when_no_row_has_one() {
        let m = mission("doom-loop-m4", MissionStatus::Active);
        let l = plan_layout([view(&m, 2, 1)].iter(), Some(200));
        assert!(!l.show_handle);
        assert_eq!(l.handle_width, 0);
    }

    /// (#1569 packet A) Mission ids are NOT guaranteed path-safe — `pr-review`
    /// ids embed a full TMPDIR path (#1563) — so an unencoded id would emit a
    /// URL with extra path segments pointing at the wrong route, or none.
    #[test]
    #[serial_test::serial] // mutates DARKMUX_PANEL, a process-global
    fn panel_deep_link_only_fires_inside_a_panel_and_never_at_itself() {
        let base = "http://127.0.0.1:8765/";
        // A terminal has a prompt to type `--all` at, so the hint stays a
        // hint and no link is emitted.
        std::env::remove_var("DARKMUX_PANEL");
        assert_eq!(panel_deep_link(base, "mission-status-all"), None);

        // Rendering into the base panel: the flag becomes reachable.
        std::env::set_var("DARKMUX_PANEL", "mission-status");
        assert_eq!(
            panel_deep_link(base, "mission-status-all").as_deref(),
            Some("http://127.0.0.1:8765/#lens=console&panel=mission-status-all")
        );

        // Already the unlimited panel — a link to where you are is noise, and
        // it is the one case the caller's `shown < len` guard would not catch
        // if the section limit ever applied under `--all`.
        std::env::set_var("DARKMUX_PANEL", "mission-status-all");
        assert_eq!(panel_deep_link(base, "mission-status-all"), None);
        std::env::remove_var("DARKMUX_PANEL");
    }

    #[test]
    fn mission_url_percent_encodes_ids_that_are_not_path_safe() {
        let base = "http://127.0.0.1:8765/";
        assert_eq!(mission_url(base, "doom-loop-m4"), "http://127.0.0.1:8765/mission/doom-loop-m4/graph");
        // The #1563 shape: a slash would otherwise open a new path segment.
        assert_eq!(
            mission_url(base, "review-/tmp/x"),
            "http://127.0.0.1:8765/mission/review-%2Ftmp%2Fx/graph"
        );
        // `?`/`#` would truncate the path into a query/fragment.
        assert_eq!(
            mission_url(base, "a?b#c"),
            "http://127.0.0.1:8765/mission/a%3Fb%23c/graph"
        );
        // RFC 3986 unreserved characters survive unescaped.
        assert_eq!(
            mission_url(base, "a-b_c.d~e"),
            "http://127.0.0.1:8765/mission/a-b_c.d~e/graph"
        );
    }

    /// (#1569 packet A) The id column must stay aligned once ids carry OSC 8
    /// escapes. `{:<width$}` counts the escape BYTES, so formatting a
    /// linkified id directly would silently destroy every column to its
    /// right — the failure the layout planner exists to prevent, reintroduced
    /// by the feature. Padding is therefore computed from the VISIBLE text.
    // Mutates the process-global colorize override — see the note on
    // `style::set_colorize_override`. Without this a concurrent test flips it
    // mid-assertion and `link()` returns plain text (the #1544 class, and I
    // reproduced it writing these).
    #[test]
    #[serial_test::serial]
    fn linkified_id_cell_pads_by_visible_width_not_byte_length() {
        style::set_colorize_override(Some(true));
        let id = ellipsize("m1", 12);
        let cell = format!(
            "{}{}",
            style::link(&mission_url("http://127.0.0.1:8765/", "m1"), &id),
            " ".repeat(12usize.saturating_sub(id.chars().count()))
        );
        style::set_colorize_override(None);

        // The cell carries the escape…
        assert!(cell.contains("\x1b]8;;"), "{cell:?}");
        // …and exactly the padding the plain form would have had.
        let visible: String = strip_ansi(&cell);
        assert_eq!(visible, format!("{:<12}", "m1"), "visible width must match the plain cell");
    }

    /// (#1569 packet A) A narrow terminal elides the id for DISPLAY, but the
    /// link must still target the FULL id — otherwise every row on a narrow
    /// terminal links to a mission that doesn't exist, and the failure is a
    /// 404 the operator would reasonably blame on the daemon rather than on
    /// the renderer. The display text and the URL come from different values
    /// on purpose; this pins that they stay different.
    // Mutates the process-global colorize override — see the note on
    // `style::set_colorize_override`. Without this a concurrent test flips it
    // mid-assertion and `link()` returns plain text (the #1544 class, and I
    // reproduced it writing these).
    #[test]
    #[serial_test::serial]
    fn a_narrowed_id_still_links_to_the_full_mission() {
        style::set_colorize_override(Some(true));
        let full = "dispatch-code-reviewer-1785570518-17301-0";
        let shown = ellipsize(full, 18);
        assert_ne!(shown, full, "precondition: this width must actually elide");
        assert!(shown.contains('…'), "{shown}");

        let cell = style::link(&mission_url("http://127.0.0.1:8765/", full), &shown);
        style::set_colorize_override(None);

        // The visible text is the elided form…
        assert!(strip_ansi(&cell).contains('…'), "{cell:?}");
        // …while the target carries the whole id, unelided.
        assert!(cell.contains(&format!("mission/{full}/graph")), "{cell:?}");
        assert!(!cell.contains(&format!("mission/{shown}/graph")), "elided id must never be the target: {cell:?}");
    }

    /// Minimal ANSI/OSC stripper for the alignment assertion above — enough
    /// for the two sequences this renderer emits (SGR and OSC 8), not a
    /// general terminal parser.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            match chars.next() {
                // OSC: consume through ST (ESC \).
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                // CSI: consume through the final byte (@..~).
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
        out
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

    /// (#1582) Every command the operator might actually RUN is its own
    /// suggestion, because only the pre-`#` segment of a suggestion is
    /// protected from wrapping. A command left inside rationale prose gets
    /// word-wrapped with the rationale's indent injected mid-command and
    /// will not survive a copy-paste — the exact failure the #1569
    /// verbatim-command rule exists to prevent.
    #[test]
    fn stale_active_actionable_commands_are_each_their_own_suggestion() {
        let mut m = mission("m1", MissionStatus::Active);
        m.started_ts = Some(0);
        let d = detect_drift(&m, &[], 15 * 86_400, 14);
        let stale = d.iter().find(|dr| dr.kind == "stale-active").expect("stale-active drift");

        for want in ["darkmux mission abort m1", "darkmux mission finalize m1"] {
            let is_own_command = stale
                .suggest
                .iter()
                .any(|s| split_suggestion(s).0 == want);
            assert!(is_own_command, "`{want}` must be a suggestion's own verbatim command, not prose");
        }

        // …and no suggestion's RATIONALE smuggles a runnable command back in.
        for s in &stale.suggest {
            let note = split_suggestion(s).1;
            assert!(
                !note.contains("darkmux mission abort") && !note.contains("darkmux mission finalize"),
                "rationale must not embed a runnable command (it would wrap): {note}"
            );
        }
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

    /// (#1582) Siblings blocked by the SAME abandoned ancestor are one
    /// problem with N instances, not N problems. Each used to emit the same
    /// four lines of rationale verbatim — on a real board that was roughly 8
    /// of 21 default lines saying nothing new, the #1569 "a default answers a
    /// question" failure one level down.
    #[test]
    fn siblings_blocked_by_one_dead_ancestor_state_the_rationale_once() {
        let mut dead = phase("dead", "m1", PhaseStatus::Abandoned);
        dead.abandoned_ts = Some(1);
        let a = phase("blocked-a", "m1", PhaseStatus::Planned);
        let b = phase("blocked-b", "m1", PhaseStatus::Planned);
        let mut m = mission("m1", MissionStatus::Active);
        m.phase_ids = ["dead", "blocked-a", "blocked-b"].map(String::from).to_vec();

        let d = detect_drift(&m, &[&dead, &a, &b], 0, 14);
        let un: Vec<_> = d.iter().filter(|dr| dr.kind == "unreachable-phase").collect();
        assert_eq!(un.len(), 1, "two blocked siblings must not repeat the rationale twice");

        // The rationale is stated once and names every blocked phase…
        assert!(un[0].detail.contains("blocked-a") && un[0].detail.contains("blocked-b"));
        // …while the per-phase specifics stay one copyable command each.
        let cmds: Vec<&str> = un[0].suggest.iter().map(|s| split_suggestion(s).0).collect();
        assert!(cmds.contains(&"darkmux mission abort m1 --phase blocked-a"));
        assert!(cmds.contains(&"darkmux mission abort m1 --phase blocked-b"));

        // With nothing salvageable, the bare-abort caveat is noise — and
        // worse than noise, because `stale-active` fires on this same
        // mission and offers `mission abort m1` as a copyable command. The
        // board must not warn against a command it is also recommending.
        assert!(
            !un[0].detail.contains("ends the WHOLE mission"),
            "no salvageable phase -> the whole-vs-per-phase distinction is vacuous and must be \
             dropped, not printed as a difference that makes no difference: {}",
            un[0].detail
        );
        assert!(
            !cmds.contains(&"darkmux mission abort m1"),
            "the bare abort is never offered by THIS drift, salvageable or not"
        );
    }

    /// (#1463 CONSIDER 5, re-scoped by #1582) The counter-example only earns
    /// its line when a whole-mission abort would actually destroy something
    /// the per-phase teardown spares.
    #[test]
    fn bare_abort_caveat_appears_only_when_a_phase_would_be_lost() {
        // `healthy` sits BEFORE the abandoned phase, so it is still runnable
        // — real collateral for a bare abort.
        let healthy = phase("healthy", "m1", PhaseStatus::Planned);
        let mut dead = phase("dead", "m1", PhaseStatus::Abandoned);
        dead.abandoned_ts = Some(1);
        let blocked = phase("blocked", "m1", PhaseStatus::Planned);
        let mut m = mission("m1", MissionStatus::Active);
        m.phase_ids = ["healthy", "dead", "blocked"].map(String::from).to_vec();

        let d = detect_drift(&m, &[&healthy, &dead, &blocked], 0, 14);
        let un = d.iter().find(|dr| dr.kind == "unreachable-phase").expect("unreachable drift");
        assert!(un.detail.contains("ends the WHOLE mission"), "caveat missing: {}", un.detail);
        assert!(
            un.detail.contains("1 phase that can still run"),
            "caveat must count the collateral, and pluralize it: {}",
            un.detail
        );
        // Still prose, never a copyable command.
        let cmds: Vec<&str> = un.suggest.iter().map(|s| split_suggestion(s).0).collect();
        assert!(!cmds.contains(&"darkmux mission abort m1"));
    }

    /// (#1582 gate) A RUNNING phase sitting AFTER the abandoned ancestor is
    /// salvageable, and that is a deliberate decision rather than an
    /// oversight of the strict-linearity rule — so it is pinned here against
    /// a future refactor "fixing" it.
    ///
    /// Strict linearity (#1341) is THIS DETECTOR's heuristic, not a lifecycle
    /// invariant: `lifecycle::phase_start` has no ancestor gate, so a Running
    /// phase after a dead predecessor is genuinely live work — and
    /// `mission abort` is precisely what reconciles it to Abandoned. It is
    /// real collateral, so the caveat must count it. Only PLANNED phases
    /// after the dead ancestor are treated as blocked, which keeps the
    /// blocked and salvageable sets disjoint.
    #[test]
    fn a_running_phase_after_the_dead_ancestor_counts_as_collateral() {
        let mut dead = phase("dead", "m1", PhaseStatus::Abandoned);
        dead.abandoned_ts = Some(1);
        let running = phase("in-flight", "m1", PhaseStatus::Running);
        let blocked = phase("blocked", "m1", PhaseStatus::Planned);
        let mut m = mission("m1", MissionStatus::Active);
        m.phase_ids = ["dead", "in-flight", "blocked"].map(String::from).to_vec();

        let d = detect_drift(&m, &[&dead, &running, &blocked], 0, 14);
        let un = d.iter().find(|dr| dr.kind == "unreachable-phase").expect("unreachable drift");
        assert!(
            un.detail.contains("1 phase that can still run"),
            "the Running phase is live work a bare abort would destroy: {}",
            un.detail
        );
        // …and it is NOT reported as blocked: only Planned phases are.
        assert!(!un.detail.contains("in-flight"), "a Running phase is not blocked: {}", un.detail);
        let cmds: Vec<&str> = un.suggest.iter().map(|s| split_suggestion(s).0).collect();
        assert_eq!(cmds, vec!["darkmux mission abort m1 --phase blocked"]);
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
        // (#1582) Exactly TWO now, not three: both blocked phases share one
        // abandoned ancestor, so they are one drift with two instances
        // rather than two drifts repeating the same rationale verbatim.
        // Both phase names are still asserted individually above, so the
        // collapse cannot silently drop one.
        assert_eq!(d.len(), 2, "unexpected drift set: {d:?}");

        // (#1582) This real shape is also the case that earns the bare-abort
        // caveat: `runtime-capture` is Planned and sits BEFORE the abandoned
        // `file-match`, so it is still runnable and a whole-mission abort
        // would genuinely destroy it. On a mission with nothing salvageable
        // the caveat is suppressed — see
        // `siblings_blocked_by_one_dead_ancestor_state_the_rationale_once`.
        let un = d.iter().find(|dr| dr.kind == "unreachable-phase").unwrap();
        assert!(
            un.detail.contains("1 phase that can still run"),
            "runtime-capture is salvageable here — the caveat must fire and count it: {}",
            un.detail
        );
    }

    #[test]
    fn progress_bar_rounds_sensibly() {
        assert_eq!(progress_bar(0, 1), "░░░░");
        assert_eq!(progress_bar(1, 1), "▓▓▓▓");
        assert_eq!(progress_bar(1, 2), "▓▓░░");
        assert_eq!(progress_bar(0, 0), "····");
    }
}
