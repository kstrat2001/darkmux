//! `darkmux run list` (#1905) — the CLI twin of `GET /runs`. Calls the SAME
//! `darkmux_serve::build_runs` union the daemon's `/runs` handler calls;
//! neither this verb nor the handler computes its own union (see
//! `darkmux_serve::runs`'s module doc, "Two callers, one union"). This
//! module owns only SELECTION (kind filter, live-never-truncated ordering,
//! honest cap disclosure) and RENDERING — no aggregation logic lives here.
//!
//! `--json` follows `mission status --json`'s posture (per #1905's design):
//! never paginated. A machine reader gets every row the kind filter
//! selected; `--limit`/`--all` only shape the human table.

use anyhow::Result;
use darkmux_serve::{Run, RunKind, RunStatus};
use darkmux_types::style;

use crate::cli::RunKindArg;

pub(crate) fn run(kind: RunKindArg, limit: usize, all: bool, json: bool) -> Result<i32> {
    let flows_dir = darkmux_types::config_access::flows_dir();
    let lab_dir = darkmux_types::config_access::lab_dir();
    // (#1905) The SAME three inputs `runs_handler` assembles for `GET
    // /runs` — `fleet_records_for_runs()` degrades to an empty vec on a
    // standalone install (no `DARKMUX_REDIS_URL`), same as the handler.
    let fleet = darkmux_serve::fleet_records_for_runs();
    let all_rows = darkmux_serve::build_runs(&flows_dir, Some(&lab_dir), &fleet);
    let filtered = filter_by_kind(all_rows, kind);

    if json {
        return run_json(&filtered, kind);
    }

    let selection = select_rows(filtered, limit, all);
    render_text(&selection, kind);
    Ok(0)
}

fn filter_by_kind(rows: Vec<Run>, kind: RunKindArg) -> Vec<Run> {
    match kind {
        RunKindArg::All => rows,
        RunKindArg::Mission => rows.into_iter().filter(|r| r.kind == RunKind::Mission).collect(),
        RunKindArg::Dispatch => rows.into_iter().filter(|r| r.kind == RunKind::Dispatch).collect(),
        RunKindArg::Lab => rows.into_iter().filter(|r| r.kind == RunKind::Lab).collect(),
    }
}

/// `updated_ts || completed_ts || started_ts || 0` — ported verbatim from
/// `ui/src/lenses/runs/format.ts::runActivity`, the RUNS lens's own recency
/// key (see `Run::updated_ts`'s own doc: "the one field the runs lens can
/// always order by, across all three sources"). Drives both the ordering
/// within each half of [`select_rows`] and the JSON default order.
fn run_activity(r: &Run) -> u64 {
    r.updated_ts.or(r.completed_ts).or(r.started_ts).unwrap_or(0)
}

/// What [`select_rows`] hands the renderer: the rows to print (running
/// rows first, newest-activity-first within each half — see that
/// function's own doc), plus enough of the terminal-side pagination
/// arithmetic that the footer never has to re-derive it.
struct Selection {
    rows: Vec<Run>,
    shown_terminal: usize,
    total_terminal: usize,
}

/// Split `rows` into (running, terminal) and fill up to `limit` ROWS
/// TOTAL, running first.
///
/// Two rules, in this order (operator direction, #1905):
///
/// 1. **`limit` is the total row count, not a per-half cap** — the ask was
///    "most recent 10 in union", so a default render is 10 ROWS, not 10
///    terminal rows plus however many happen to be live. Running rows are
///    laid down first and the terminal half fills whatever budget is left.
/// 2. **Running rows are never truncated**, and rule 1 yields to this one.
///    More live runs than `limit` prints all of them and no history: the
///    whole reason this verb exists is that "the console was hiding an
///    in-flight run", and a cap that could hide live work reintroduces
///    that bug in a new place. A shorter table is the better failure.
///
/// Both halves are ordered newest-activity-first via [`run_activity`].
/// `all` lifts the cap, and `limit == 0` is treated as unlimited too — the
/// SAME convention `mission status --limit` documents ("0 = no cap"), kept
/// consistent here rather than reinventing a second meaning for zero.
fn select_rows(mut rows: Vec<Run>, limit: usize, all: bool) -> Selection {
    rows.sort_by_key(|r| std::cmp::Reverse(run_activity(r)));
    let (running, terminal): (Vec<Run>, Vec<Run>) =
        rows.into_iter().partition(|r| r.status == RunStatus::Running);
    let total_terminal = terminal.len();
    let unlimited = all || limit == 0;
    // Rule 1 + rule 2: the live rows are already committed, so the
    // terminal budget is whatever `limit` has left over. `saturating_sub`
    // IS rule 2's yield — more live rows than `limit` leaves 0 budget and
    // prints every live row anyway, rather than dropping any of them.
    let terminal_budget = limit.saturating_sub(running.len());
    let terminal_shown: Vec<Run> =
        if unlimited { terminal } else { terminal.into_iter().take(terminal_budget).collect() };
    let shown_terminal = terminal_shown.len();
    let mut out = running;
    out.extend(terminal_shown);
    Selection { rows: out, shown_terminal, total_terminal }
}

/// The honest cap-disclosure footer (#1876, #1891: never report the cap as
/// the total). `None` when nothing was hidden — `select_rows` already
/// returned everything, so a footer here would be noise, not disclosure.
///
/// Counts are stated over the WHOLE union, not the terminal half alone.
/// The reader sees N rows on screen, live and terminal together, so a
/// footer counting only the terminal half would disagree with the table
/// directly above it ("showing 9 of 15" under a 10-row table). Only
/// terminal rows are ever hidden, so the difference between the two
/// numbers is still exactly the terminal rows that were cut.
fn footer(sel: &Selection) -> Option<String> {
    if sel.shown_terminal >= sel.total_terminal {
        return None;
    }
    let shown = sel.rows.len();
    let hidden = sel.total_terminal - sel.shown_terminal;
    Some(format!("showing {shown} of {} runs ({hidden} more not shown — `--all` for every run)", shown + hidden))
}

fn kind_label(kind: RunKind) -> &'static str {
    match kind {
        RunKind::Mission => "mission",
        RunKind::Dispatch => "dispatch",
        RunKind::Lab => "lab",
    }
}

fn kind_arg_label(kind: RunKindArg) -> &'static str {
    match kind {
        RunKindArg::All => "all",
        RunKindArg::Mission => "mission",
        RunKindArg::Dispatch => "dispatch",
        RunKindArg::Lab => "lab",
    }
}

fn status_label(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Planned => "planned",
        RunStatus::Running => "running",
        RunStatus::Complete => "complete",
        RunStatus::Error => "error",
        RunStatus::Abandoned => "abandoned",
        RunStatus::Unparseable => "unparseable",
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// One-unit relative age (`now`/`Nm`/`Nh`/`Nd`/`Nw`), rounding down. Same
/// shape as `mission_status.rs::relative_age` — kept local rather than
/// shared (this module has no other dependency on `mission_status`, and
/// the function is four lines).
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

fn started_cell(now: u64, r: &Run) -> String {
    match r.started_ts {
        Some(ts) => format!("{} ago", relative_age(now, ts)),
        // Honest, not a guess: a lab run in particular carries no start
        // timestamp at all (`Run::started_ts`'s own doc) — see that field's
        // comment for why leaving it absent beats a wrong inference.
        None => "-".to_string(),
    }
}

/// One-unit elapsed span (`Ns`/`Nm`/`Nh`/`Nd`), rounding down. Distinct
/// constant table from [`relative_age`] (no `w` bucket — a run's own
/// duration realistically never reaches weeks the way "last touched" can).
fn format_span(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3_599 => format!("{}m", secs / 60),
        3_600..=86_399 => format!("{}h", secs / 3_600),
        _ => format!("{}d", secs / 86_400),
    }
}

fn duration_cell(now: u64, r: &Run) -> String {
    let Some(started) = r.started_ts else {
        return "-".to_string();
    };
    if r.status == RunStatus::Running {
        // Still going: elapsed-so-far, marked with a trailing `+` so it
        // reads as "at least this long", not a finished duration.
        return format!("{}+", format_span(now.saturating_sub(started)));
    }
    match r.completed_ts {
        Some(end) => format_span(end.saturating_sub(started)),
        None => "-".to_string(),
    }
}

/// Truncate to `max` CHARS, eliding the MIDDLE with a `…` — same rationale
/// as `mission_status.rs::ellipsize`: darkmux's machine-minted ids carry
/// their discriminating suffix at the END
/// (`dispatch-code-reviewer-1785589698-5d6a-0`), so tail-truncating a
/// screenful renders every row as the same string. Kept local rather than
/// shared (four lines, no IO, and `mission_status`'s copy is private to
/// that module).
fn ellipsize(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max || max == 0 {
        return s.to_string();
    }
    if max == 1 {
        return "…".to_string();
    }
    let keep = max - 1;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let chars: Vec<char> = s.chars().collect();
    let front: String = chars[..head].iter().collect();
    let back: String = chars[n - tail..].iter().collect();
    format!("{front}…{back}")
}

/// `role · model · via route · machine` — same fields, same join, and same
/// order as `ui/src/lenses/runs/format.ts::runSubtitle` (minus that
/// function's `showMachine` gate, which exists there to avoid repeating a
/// machine pin the lens already filtered to; this verb has no such pin, so
/// machine is always eligible).
fn subtitle_for(r: &Run) -> String {
    let mut bits: Vec<String> = Vec::new();
    if let Some(role) = &r.role {
        bits.push(role.clone());
    }
    if let Some(model) = &r.model {
        bits.push(model.clone());
    }
    if let Some(route) = &r.route {
        bits.push(format!("via {route}"));
    }
    if let Some(machine) = &r.machine {
        bits.push(machine.clone());
    }
    bits.join(" · ")
}

// KIND(8) + gap(1) + STATUS(10) + gap(1) + STARTED(10) + gap(1) +
// DURATION(10) + gap(2) = 43. Matches the literal format string in
// `print_row`/`print_header` below exactly — a change to either must
// change both, same coupling `mission_status.rs::ROW_FIXED_COLS` documents
// for its own row shape.
const FIXED_COLS: usize = 8 + 1 + 10 + 1 + 10 + 1 + 10 + 2;
const MIN_ID_COLS: usize = 12;

/// The ID column's width for this render: wide enough for the longest id
/// present, clamped to fit `width` (when known) with the fixed columns
/// already accounted for. `width == None` (piped output) never clamps —
/// piped output stays complete and greppable, matching
/// `mission_status.rs::plan_layout`'s same rule for the same reason.
fn id_width(rows: &[Run], width: Option<usize>) -> usize {
    let max_id = rows.iter().map(|r| r.id.chars().count()).max().unwrap_or(0);
    match width {
        None => max_id,
        Some(w) => {
            if max_id + FIXED_COLS <= w {
                max_id
            } else {
                w.saturating_sub(FIXED_COLS).max(MIN_ID_COLS)
            }
        }
    }
}

fn print_header(id_w: usize) {
    println!("  {:<8} {:<10} {:<10} {:<10}  {:<id_w$}", "KIND", "STATUS", "STARTED", "DURATION", "ID");
}

fn print_row(now: u64, r: &Run, id_w: usize, width: Option<usize>) {
    let id_cell = format!("{:<id_w$}", ellipsize(&r.id, id_w));
    let base = format!(
        "  {:<8} {:<10} {:<10} {:<10}  {}",
        kind_label(r.kind),
        status_label(r.status),
        started_cell(now, r),
        duration_cell(now, r),
        id_cell,
    );
    let subtitle = subtitle_for(r);
    if subtitle.is_empty() {
        println!("{base}");
        return;
    }
    // Only append the subtitle when it demonstrably fits — piped output
    // (width None) always gets it; a real terminal gets it only if the
    // combined line doesn't exceed the known width. No wrapping: an
    // overflowing subtitle is dropped whole rather than broken mid-line
    // (same "worse to wrap a gutter" call `mission_status.rs::wrap_indented`
    // documents for its own overlong lines).
    let full = format!("{base}  {subtitle}");
    match width {
        None => println!("{full}"),
        Some(w) if full.chars().count() <= w => println!("{full}"),
        Some(_) => println!("{base}"),
    }
}

fn render_text(sel: &Selection, kind: RunKindArg) {
    if sel.rows.is_empty() {
        let msg = if matches!(kind, RunKindArg::All) {
            "no recorded run activity yet".to_string()
        } else {
            format!("no recorded {} runs yet", kind_arg_label(kind))
        };
        println!("{}", style::dim(&msg));
        return;
    }

    let width = style::terminal_width();
    let id_w = id_width(&sel.rows, width);
    let now = now_unix();

    println!("{}", style::header(&format!("runs — {} shown", sel.rows.len())));
    print_header(id_w);
    for r in &sel.rows {
        print_row(now, r, id_w, width);
    }
    if let Some(line) = footer(sel) {
        println!("{}", style::dim(&line));
    }
}

fn run_json(rows: &[Run], kind: RunKindArg) -> Result<i32> {
    // (#1905, matching `mission status --json`'s posture) NEVER paginated —
    // a machine reader gets every row the kind filter selected;
    // `--limit`/`--all` only shape the human table above.
    let mut sorted: Vec<&Run> = rows.iter().collect();
    sorted.sort_by_key(|r| std::cmp::Reverse(run_activity(r)));
    let payload = serde_json::json!({
        "kind": kind_arg_label(kind),
        "runs": sorted,
        "total": sorted.len(),
    });
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum;

    fn mk_run(id: &str, kind: RunKind, status: RunStatus, updated_ts: u64) -> Run {
        Run {
            id: id.to_string(),
            kind,
            status,
            machine: None,
            route: None,
            role: None,
            model: None,
            started_ts: Some(updated_ts),
            completed_ts: if status == RunStatus::Running { None } else { Some(updated_ts) },
            updated_ts: Some(updated_ts),
            tracked: true,
        }
    }

    // ── filter_by_kind ───────────────────────────────────────────────

    #[test]
    fn filter_by_kind_all_keeps_every_kind() {
        let rows = vec![
            mk_run("m1", RunKind::Mission, RunStatus::Complete, 100),
            mk_run("d1", RunKind::Dispatch, RunStatus::Complete, 100),
            mk_run("l1", RunKind::Lab, RunStatus::Complete, 100),
        ];
        assert_eq!(filter_by_kind(rows, RunKindArg::All).len(), 3);
    }

    #[test]
    fn filter_by_kind_narrows_to_one_kind() {
        let rows = vec![
            mk_run("m1", RunKind::Mission, RunStatus::Complete, 100),
            mk_run("d1", RunKind::Dispatch, RunStatus::Complete, 100),
            mk_run("l1", RunKind::Lab, RunStatus::Complete, 100),
        ];
        let got = filter_by_kind(rows, RunKindArg::Lab);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "l1");
    }

    // ── select_rows: the live-never-truncated rule (#1905's own reason to exist) ─

    /// A fixture with 1 running run OLDER than 15 terminal runs, `--limit
    /// 10`. The running row must still appear — a limit that hides
    /// in-flight work is exactly the bug this verb exists to fix.
    #[test]
    fn live_run_survives_a_limit_that_would_bury_it_by_recency() {
        let mut rows = vec![mk_run("running-old", RunKind::Dispatch, RunStatus::Running, 1)];
        for i in 0..15u64 {
            // Every terminal row is strictly newer than the running one.
            rows.push(mk_run(&format!("terminal-{i}"), RunKind::Mission, RunStatus::Complete, 1_000 + i));
        }

        let sel = select_rows(rows, 10, false);

        assert!(
            sel.rows.iter().any(|r| r.id == "running-old"),
            "the live run was truncated away — this is the exact defect #1905 exists to fix"
        );
        // `limit` is the TOTAL row count, so the one live row spends part
        // of the budget: 1 live + 9 terminal = 10 rows.
        assert_eq!(sel.rows.len(), 10, "--limit 10 must render 10 ROWS, not 10 terminal rows plus the live one");
        assert_eq!(sel.total_terminal, 15);
        assert_eq!(sel.shown_terminal, 9);
    }

    /// Rule 2 yielding rule 1 (see [`select_rows`]'s doc): more live runs
    /// than `limit` prints every live run and no history. A shorter table
    /// is the better failure; silently dropping an in-flight run is the
    /// defect this verb exists to prevent.
    #[test]
    fn more_live_runs_than_the_limit_prints_all_of_them_and_no_history() {
        let mut rows: Vec<Run> = (0..12u64)
            .map(|i| mk_run(&format!("live-{i}"), RunKind::Dispatch, RunStatus::Running, i))
            .collect();
        for i in 0..5u64 {
            rows.push(mk_run(&format!("t{i}"), RunKind::Mission, RunStatus::Complete, 900 + i));
        }

        let sel = select_rows(rows, 10, false);

        assert_eq!(sel.rows.len(), 12, "every live run must render even past the limit");
        assert!(
            sel.rows.iter().all(|r| r.status == RunStatus::Running),
            "the terminal budget is exhausted by live rows, so no history should render"
        );
        // The footer still tells the truth about what was hidden.
        assert_eq!(sel.shown_terminal, 0);
        assert_eq!(sel.total_terminal, 5);
    }

    /// The plain case, stated on its own so the total-not-per-half rule
    /// has a test that fails loudly if anyone reinstates a terminal-only
    /// cap: 3 live + 20 terminal at `--limit 10` is 3 + 7, not 3 + 10.
    #[test]
    fn live_rows_count_against_the_limit_rather_than_adding_to_it() {
        let mut rows: Vec<Run> = (0..3u64)
            .map(|i| mk_run(&format!("live-{i}"), RunKind::Dispatch, RunStatus::Running, i))
            .collect();
        for i in 0..20u64 {
            rows.push(mk_run(&format!("t{i}"), RunKind::Mission, RunStatus::Complete, 900 + i));
        }

        let sel = select_rows(rows, 10, false);

        assert_eq!(sel.rows.len(), 10);
        assert_eq!(sel.shown_terminal, 7);
        assert_eq!(sel.total_terminal, 20);
    }

    #[test]
    fn running_rows_sort_before_terminal_rows() {
        let rows = vec![
            mk_run("terminal-newest", RunKind::Mission, RunStatus::Complete, 9_999),
            mk_run("running-oldest", RunKind::Dispatch, RunStatus::Running, 1),
        ];
        let sel = select_rows(rows, 10, false);
        assert_eq!(sel.rows[0].id, "running-oldest", "running rows must sort first regardless of recency");
        assert_eq!(sel.rows[1].id, "terminal-newest");
    }

    #[test]
    fn terminal_rows_within_the_limit_are_the_most_recent_ones() {
        let rows: Vec<Run> =
            (0..5u64).map(|i| mk_run(&format!("t{i}"), RunKind::Mission, RunStatus::Complete, i)).collect();
        // limit 2 of 5 -> the two NEWEST (t4, t3), not the first two inserted.
        let sel = select_rows(rows, 2, false);
        let ids: Vec<&str> = sel.rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["t4", "t3"]);
    }

    #[test]
    fn all_flag_lifts_the_terminal_cap() {
        let rows: Vec<Run> =
            (0..20u64).map(|i| mk_run(&format!("t{i}"), RunKind::Mission, RunStatus::Complete, i)).collect();
        let sel = select_rows(rows, 10, true);
        assert_eq!(sel.shown_terminal, 20);
        assert_eq!(sel.total_terminal, 20);
        assert_eq!(sel.rows.len(), 20);
    }

    #[test]
    fn limit_zero_is_treated_as_unlimited_matching_mission_status_convention() {
        let rows: Vec<Run> =
            (0..5u64).map(|i| mk_run(&format!("t{i}"), RunKind::Mission, RunStatus::Complete, i)).collect();
        let sel = select_rows(rows, 0, false);
        assert_eq!(sel.shown_terminal, 5);
    }

    // ── footer: never report the cap as the total (#1876, #1891) ────────

    #[test]
    fn footer_is_absent_when_nothing_was_truncated() {
        let sel = Selection { rows: vec![], shown_terminal: 5, total_terminal: 5 };
        assert_eq!(footer(&sel), None);
    }

    #[test]
    fn footer_names_the_real_total_not_the_cap() {
        // 10 rows rendered, all terminal, out of 47 terminal runs on disk.
        let rows: Vec<Run> =
            (0..10u64).map(|i| mk_run(&format!("t{i}"), RunKind::Mission, RunStatus::Complete, i)).collect();
        let sel = Selection { rows, shown_terminal: 10, total_terminal: 47 };
        let line = footer(&sel).expect("something was hidden, footer must print");
        assert!(line.contains("10"), "must name what's shown: {line}");
        assert!(line.contains("47"), "must name the REAL total, not the cap: {line}");
        assert!(line.contains("--all"), "must name the escape hatch: {line}");
    }

    /// The footer's numbers must agree with the table directly above it:
    /// a render of 1 live + 9 terminal out of 15 terminal reads "10 of 16",
    /// never "9 of 15" under a ten-row table.
    #[test]
    fn footer_counts_the_union_it_rendered_not_the_terminal_half() {
        let mut rows = vec![mk_run("live", RunKind::Dispatch, RunStatus::Running, 1)];
        for i in 0..15u64 {
            rows.push(mk_run(&format!("t{i}"), RunKind::Mission, RunStatus::Complete, 1_000 + i));
        }
        let sel = select_rows(rows, 10, false);
        let line = footer(&sel).expect("6 terminal rows were hidden, footer must print");
        assert!(line.contains("showing 10 of 16 runs"), "footer disagreed with the table: {line}");
        assert!(line.contains("6 more"), "must name how many were hidden: {line}");
    }

    // ── run_activity ordering key ────────────────────────────────────

    #[test]
    fn run_activity_prefers_updated_then_completed_then_started() {
        let mut r = mk_run("x", RunKind::Mission, RunStatus::Complete, 0);
        r.updated_ts = None;
        r.completed_ts = None;
        r.started_ts = Some(5);
        assert_eq!(run_activity(&r), 5);
        r.completed_ts = Some(7);
        assert_eq!(run_activity(&r), 7);
        r.updated_ts = Some(9);
        assert_eq!(run_activity(&r), 9);
    }

    // ── cross-language kind-vocabulary drift guard ───────────────────

    /// Pins `RunKindArg`'s accepted `--kind` values against
    /// `ui/src/lib/route.ts::RUNS_KINDS` — the RUNS lens's own kind-chip
    /// vocabulary (#1905's settled design: "the flag is its twin, and a
    /// test should pin them to each other"). `include_str!` reaches
    /// outside this crate into `ui/` on purpose (test-only,
    /// `#[cfg(test)]`-gated, so it never ships in the release binary) —
    /// there is no Rust-side binding to pin against on the TS side of this
    /// contract, so a text scan is the mechanical tie, same pattern
    /// `crates/darkmux-serve/src/lib_tests.rs`'s
    /// `mission_graph_lens_pins_flow_action_strings` already uses against
    /// `ui/src/lenses/mission/graph.ts`.
    #[test]
    fn run_kind_arg_vocabulary_matches_the_ui_runs_kinds_twin() {
        let mut rust_kinds: Vec<String> = RunKindArg::value_variants()
            .iter()
            .map(|v| {
                v.to_possible_value()
                    .expect("every RunKindArg variant must have a possible value")
                    .get_name()
                    .to_string()
            })
            .collect();
        rust_kinds.sort();

        let route_ts = include_str!("../ui/src/lib/route.ts");
        let (_, after) = route_ts.split_once("RUNS_KINDS = [").expect(
            "RUNS_KINDS not found in ui/src/lib/route.ts — the twin this test pins against \
             was renamed or removed; darkmux run list --kind and the RUNS lens's kind chips \
             can now drift apart silently (#1905)",
        );
        let (body, _) = after.split_once(']').expect(
            "RUNS_KINDS has no closing `]` in ui/src/lib/route.ts — the twin this test pins \
             against changed shape (#1905)",
        );
        let mut ts_kinds: Vec<String> = body
            .split(',')
            .filter_map(|tok| {
                let t = tok.trim().trim_matches('"');
                if t.is_empty() { None } else { Some(t.to_string()) }
            })
            .collect();
        ts_kinds.sort();

        assert_eq!(
            rust_kinds, ts_kinds,
            "darkmux run list --kind's accepted values (RunKindArg, src/cli.rs) drifted from \
             ui/src/lib/route.ts's RUNS_KINDS — update BOTH twins together (#1905), the pill \
             row and the CLI flag must show the same vocabulary"
        );
    }
}
