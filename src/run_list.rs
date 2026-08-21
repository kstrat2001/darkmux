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
    let all_rows = darkmux_serve::build_runs(&flows_dir, Some(&lab_dir), &fleet.records);
    let filtered = filter_by_kind(all_rows, kind);

    if json {
        return run_json(&filtered, kind, &fleet.state);
    }

    let selection = select_rows(filtered, limit, all);
    render_text(&selection, kind, &fleet.state);
    Ok(0)
}

/// One line naming an INCOMPLETE fleet read, or `None` when the answer is
/// whole.
///
/// `source_state`'s module doc calls this contract "empty is never
/// silent": a hub whose Redis just died and a genuinely quiet fleet
/// produce the same rows, and without this they would produce the same
/// output too. `Off` is deliberately silent — a standalone install has no
/// fleet substrate by design, and warning about it would be the bug.
fn fleet_warning(state: &darkmux_serve::source_state::SourceState) -> Option<String> {
    use darkmux_serve::source_state::SourceState;
    match state {
        SourceState::Ok | SourceState::Off => None,
        SourceState::Stale { age_ms, .. } => Some(format!(
            "fleet: could not reach the shared stream; showing a snapshot {} old — runs on other machines may be missing or out of date",
            format_span(age_ms / 1_000)
        )),
        SourceState::Unavailable { .. } => Some(
            "fleet: could not reach the shared stream and nothing was cached — runs on other machines are missing from this list".to_string(),
        ),
    }
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
fn footer(sel: &Selection, width: Option<usize>) -> Option<String> {
    if sel.shown_terminal >= sel.total_terminal {
        return None;
    }
    let shown = sel.rows.len();
    let hidden = sel.total_terminal - sel.shown_terminal;
    let total = shown + hidden;
    let full = format!("showing {shown} of {total} runs ({hidden} more not shown — `--all` for every run)");
    match width {
        Some(w) if full.chars().count() > w => {
            // The compact form keeps all three load-bearing facts: what
            // was shown, the REAL total, and the escape hatch. Only the
            // "N more" restatement is dropped, and it is pure subtraction
            // of the two numbers still present — so a narrow pane loses
            // no disclosure, just a wrapped line it did not need. (The
            // rows themselves are clamped by `id_width`; a footer that
            // wrapped while every row fit would be the same overflow bug
            // one line lower.)
            Some(format!("showing {shown} of {total} runs · `--all` for every run"))
        }
        _ => Some(full),
    }
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

/// Strip the `darkmux:` residency namespace off a model id for display —
/// the Rust twin of `ui/src/lenses/lab/labSeries.ts::shortModel`, which
/// `runSubtitle` applies before pushing the model.
///
/// Not cosmetic. The namespace convention states the prefix is "invisible
/// at dispatch time" and exists so darkmux can recognize its OWN loaded
/// instances in `lms ps`; printing it in a run listing leaks bookkeeping
/// into an operator-facing column. It also costs 8 columns, and under
/// [`format_row`]'s drop-the-subtitle-whole rule those 8 can be what
/// pushes a line past the terminal width, costing role, route AND machine
/// together on a row that would otherwise have fit.
fn short_model(model: &str) -> &str {
    model.strip_prefix("darkmux:").unwrap_or(model)
}

/// `role · model · via route · machine` — same fields, same join, and same
/// order as `ui/src/lenses/runs/format.ts::runSubtitle`, including its
/// `shortModel` treatment of the model id (see [`short_model`]). The one
/// difference is that function's `showMachine` gate, which exists there to
/// avoid repeating a machine pin the lens already filtered to; this verb
/// has no such pin, so machine is always eligible.
fn subtitle_for(r: &Run) -> String {
    let mut bits: Vec<String> = Vec::new();
    if let Some(role) = &r.role {
        bits.push(role.clone());
    }
    if let Some(model) = &r.model {
        bits.push(short_model(model).to_string());
    }
    if let Some(route) = &r.route {
        bits.push(format!("via {route}"));
    }
    if let Some(machine) = &r.machine {
        bits.push(machine.clone());
    }
    bits.join(" · ")
}

/// Column widths. The format strings in [`header_line`]/[`format_row`]
/// take their widths FROM these consts (`{:<w$}`) rather than repeating
/// them as literals, so the "change one, change both" coupling that
/// `mission_status.rs::ROW_FIXED_COLS` documents cannot exist here: there
/// is one number per column, used by both the budget and the render.
///
/// Each width is the longest label its column can hold, pinned by
/// `every_column_label_fits_its_width` — `status_label` can emit
/// `unparseable` (11), which a 10-wide column silently renders ONE column
/// over budget, since `{:<10}` is a minimum and never truncates.
const INDENT_COLS: usize = 2;
const KIND_COLS: usize = 8; // "dispatch"
const STATUS_COLS: usize = 11; // "unparseable"
const STARTED_COLS: usize = 10;
const DURATION_COLS: usize = 10;
/// The two-space gap between DURATION and ID (every other gap is one).
const ID_GAP_COLS: usize = 2;

/// Everything a row spends before the ID column.
///
/// The LEADING INDENT is part of this budget, and forgetting it is the
/// specific trap `mission_status.rs::ROW_FIXED_COLS` documents having
/// already sprung twice ("the first draft of this constant said 23"). The
/// first draft of THIS one omitted the indent AND undersized STATUS, so
/// `id_width` planned a row two-to-three columns narrower than it rendered
/// and the clamp branch could not fit at any width. Pinned by
/// `every_clamped_row_fits_the_width_it_was_planned_for`, which measures
/// the RENDERED string — a test that recomputed this sum would have agreed
/// with the wrong constant.
const FIXED_COLS: usize =
    INDENT_COLS + KIND_COLS + 1 + STATUS_COLS + 1 + STARTED_COLS + 1 + DURATION_COLS + ID_GAP_COLS;
const MIN_ID_COLS: usize = 12;

/// The narrowest terminal this row shape can honor. Below it, [`id_width`]
/// hits its `MIN_ID_COLS` floor and the row is wider than the pane: an id
/// elided past twelve characters stops identifying anything, so a narrow
/// pane gets a slightly-too-wide row rather than a useless one. Named so
/// the guarantee is stateable and testable, rather than being an
/// unremarked property of two constants. (A narrow render that SHEDS
/// columns, the way `mission status` sheds its optional ones, is the real
/// fix when this verb becomes a phone-width console panel — #1905 step 2.)
const MIN_ROW_COLS: usize = FIXED_COLS + MIN_ID_COLS;

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
                // Clamp to the pane, but never below [`MIN_ROW_COLS`]'s
                // floor — expressed by raising `w` to the floor rather
                // than by a second `.max(MIN_ID_COLS)`, so the narrowest
                // honorable row is stated in ONE place and this branch
                // cannot drift from it. (`MIN_ROW_COLS > FIXED_COLS` by
                // construction, so this subtraction cannot underflow.)
                w.max(MIN_ROW_COLS) - FIXED_COLS
            }
        }
    }
}

fn header_line(id_w: usize) -> String {
    format!(
        "{:i$}{:<k$} {:<s$} {:<t$} {:<d$}{:g$}{:<id_w$}",
        "",
        "KIND",
        "STATUS",
        "STARTED",
        "DURATION",
        "",
        "ID",
        i = INDENT_COLS,
        k = KIND_COLS,
        s = STATUS_COLS,
        t = STARTED_COLS,
        d = DURATION_COLS,
        g = ID_GAP_COLS,
    )
}

/// Render one row. Split from the `println!` so a test can MEASURE what
/// this returns — see [`FIXED_COLS`] for why measuring beats recomputing
/// the budget.
fn format_row(now: u64, r: &Run, id_w: usize, width: Option<usize>) -> String {
    let id_cell = format!("{:<id_w$}", ellipsize(&r.id, id_w));
    let base = format!(
        "{:i$}{:<k$} {:<s$} {:<t$} {:<d$}{:g$}{}",
        "",
        kind_label(r.kind),
        status_label(r.status),
        started_cell(now, r),
        duration_cell(now, r),
        "",
        id_cell,
        i = INDENT_COLS,
        k = KIND_COLS,
        s = STATUS_COLS,
        t = STARTED_COLS,
        d = DURATION_COLS,
        g = ID_GAP_COLS,
    );
    let subtitle = subtitle_for(r);
    if subtitle.is_empty() {
        return base;
    }
    // Only append the subtitle when it demonstrably fits — piped output
    // (width None) always gets it; a real terminal gets it only if the
    // combined line doesn't exceed the known width. No wrapping: an
    // overflowing subtitle is dropped whole rather than broken mid-line
    // (same "worse to wrap a gutter" call `mission_status.rs::wrap_indented`
    // documents for its own overlong lines).
    let full = format!("{base}  {subtitle}");
    match width {
        None => full,
        Some(w) if full.chars().count() <= w => full,
        Some(_) => base,
    }
}

fn render_text(sel: &Selection, kind: RunKindArg, fleet: &darkmux_serve::source_state::SourceState) {
    // Printed BEFORE the table (and before the empty-state line) — an
    // incomplete answer has to be qualified where the reader meets it, not
    // in a footnote under rows they have already believed.
    if let Some(warning) = fleet_warning(fleet) {
        eprintln!("{}", style::warn(&warning));
    }
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
    println!("{}", header_line(id_w));
    for r in &sel.rows {
        println!("{}", format_row(now, r, id_w, width));
    }
    if let Some(line) = footer(sel, width) {
        println!("{}", style::dim(&line));
    }
}

fn run_json(
    rows: &[Run],
    kind: RunKindArg,
    fleet: &darkmux_serve::source_state::SourceState,
) -> Result<i32> {
    // (#1905, matching `mission status --json`'s posture) NEVER paginated —
    // a machine reader gets every row the kind filter selected;
    // `--limit`/`--all` only shape the human table above.
    let mut sorted: Vec<&Run> = rows.iter().collect();
    sorted.sort_by_key(|r| std::cmp::Reverse(run_activity(r)));
    let payload = serde_json::json!({
        "kind": kind_arg_label(kind),
        "runs": sorted,
        "total": sorted.len(),
        // The same honesty `runs_handler` ships as `meta`: a script must be
        // able to tell an incomplete answer from a quiet fleet.
        "fleet": fleet,
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
        assert_eq!(footer(&sel, None), None);
    }

    #[test]
    fn footer_names_the_real_total_not_the_cap() {
        // 10 rows rendered, all terminal, out of 47 terminal runs on disk.
        let rows: Vec<Run> =
            (0..10u64).map(|i| mk_run(&format!("t{i}"), RunKind::Mission, RunStatus::Complete, i)).collect();
        let sel = Selection { rows, shown_terminal: 10, total_terminal: 47 };
        let line = footer(&sel, None).expect("something was hidden, footer must print");
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
        let line = footer(&sel, None).expect("6 terminal rows were hidden, footer must print");
        assert!(line.contains("showing 10 of 16 runs"), "footer disagreed with the table: {line}");
        assert!(line.contains("6 more"), "must name how many were hidden: {line}");
    }

    // ── fleet-state disclosure: empty is never silent ────────────────

    /// The load-bearing silence: a standalone install has no fleet
    /// substrate BY DESIGN, and `source_state`'s own doc says warning
    /// about it "would be the bug".
    #[test]
    fn a_standalone_install_is_never_warned_about_its_missing_fleet() {
        use darkmux_serve::source_state::SourceState;
        assert_eq!(fleet_warning(&SourceState::Off), None);
        assert_eq!(fleet_warning(&SourceState::Ok), None);
    }

    /// The inverse, and the reason this exists: an unreachable hub and a
    /// quiet fleet return the same ROWS, so they must not return the same
    /// OUTPUT.
    #[test]
    fn an_incomplete_fleet_read_is_named_rather_than_rendered_as_quiet() {
        use darkmux_serve::source_state::SourceState;
        let stale = fleet_warning(&SourceState::Stale { age_ms: 120_000, detail: "could not reach Redis" })
            .expect("a stale fleet read must be disclosed");
        assert!(stale.contains("2m"), "must say how old the snapshot is: {stale}");
        assert!(stale.contains("other machines"), "must say what is affected: {stale}");

        let gone = fleet_warning(&SourceState::Unavailable { detail: "could not reach Redis" })
            .expect("an unavailable fleet read must be disclosed");
        assert!(gone.contains("missing"), "must say the list is incomplete: {gone}");
    }

    /// The warning must never carry the underlying error text — a Redis
    /// error can embed the connection URL, and the env tier of
    /// `redis_url()` carries an inline password. `source_state`'s module
    /// doc makes this rule explicit for the HTTP body; the CLI renders to
    /// a terminal that gets pasted into issues, so it holds here too.
    #[test]
    fn the_fleet_warning_never_echoes_the_source_detail() {
        use darkmux_serve::source_state::SourceState;
        let secret = "redis://user:hunter2@hub.example:6379";
        let line = fleet_warning(&SourceState::Unavailable { detail: secret }).unwrap();
        assert!(!line.contains("hunter2"), "the warning leaked credential-bearing detail: {line}");
        assert!(!line.contains("redis://"), "the warning leaked a connection URL: {line}");
    }

    /// A footer that wrapped while every row fit would be the same
    /// overflow defect one line lower. The compact form must still carry
    /// what was shown, the REAL total, and the escape hatch.
    #[test]
    fn the_footer_fits_a_narrow_pane_without_losing_disclosure() {
        let rows: Vec<Run> =
            (0..3u64).map(|i| mk_run(&format!("t{i}"), RunKind::Mission, RunStatus::Complete, i)).collect();
        let sel = Selection { rows, shown_terminal: 3, total_terminal: 111 };

        let narrow = footer(&sel, Some(58)).expect("rows were hidden, footer must print");
        assert!(narrow.chars().count() <= 58, "footer rendered {} cols at width 58: {narrow}", narrow.chars().count());
        assert!(narrow.contains('3'), "must still name what was shown: {narrow}");
        assert!(narrow.contains("111"), "must still name the REAL total, not the cap: {narrow}");
        assert!(narrow.contains("--all"), "must still name the escape hatch: {narrow}");

        // A wide pane keeps the fuller phrasing.
        let wide = footer(&sel, Some(120)).unwrap();
        assert!(wide.contains("108 more not shown"), "a wide pane should keep the restatement: {wide}");
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

    // ── rendering: measure the RENDERED string, never the budget ─────

    /// The guard [`FIXED_COLS`] earned. For every width `id_width` claims
    /// it can honor, the rendered header AND row must actually fit. This
    /// measures the output rather than recomputing the arithmetic, because
    /// a test that recomputed it would have agreed with the wrong
    /// constant (the first draft omitted the two-space indent and made the
    /// clamp branch overflow at every width).
    #[test]
    fn every_clamped_row_fits_the_width_it_was_planned_for() {
        // An id long enough to force the clamp branch at every width below.
        let mut r = mk_run(
            "dispatch-code-reviewer-1787054517-1526-0-and-then-some-more",
            RunKind::Dispatch,
            RunStatus::Complete,
            1_000,
        );
        r.role = Some("code-reviewer".to_string());
        r.model = Some("darkmux:qwen3.6-35b-a3b".to_string());
        r.machine = Some("MacBook-Pro.local".to_string());
        let rows = vec![r];

        for w in [MIN_ROW_COLS, MIN_ROW_COLS + 1, 70, 80, 100, 120, 200] {
            let id_w = id_width(&rows, Some(w));
            let header = header_line(id_w);
            let row = format_row(2_000, &rows[0], id_w, Some(w));
            assert!(
                header.chars().count() <= w,
                "header rendered {} cols at width {w}: {header:?}",
                header.chars().count()
            );
            assert!(
                row.chars().count() <= w,
                "row rendered {} cols at width {w}: {row:?}",
                row.chars().count()
            );
        }
    }

    /// Every label a column can emit must FIT that column, because
    /// `{:<w$}` is a minimum width and never truncates: one label over
    /// budget silently widens every row carrying it. `unparseable` (11)
    /// is why `STATUS_COLS` is not 10.
    #[test]
    fn every_column_label_fits_its_width() {
        for st in [
            RunStatus::Planned,
            RunStatus::Running,
            RunStatus::Complete,
            RunStatus::Error,
            RunStatus::Abandoned,
            RunStatus::Unparseable,
        ] {
            let label = status_label(st);
            assert!(
                label.chars().count() <= STATUS_COLS,
                "status label {label:?} is {} cols, STATUS_COLS is {STATUS_COLS} — every row \
                 carrying this status renders over budget",
                label.chars().count()
            );
        }
        for k in [RunKind::Mission, RunKind::Dispatch, RunKind::Lab] {
            let label = kind_label(k);
            assert!(label.chars().count() <= KIND_COLS, "kind label {label:?} exceeds KIND_COLS");
        }
    }

    /// The floor is a real, stated guarantee: at `MIN_ROW_COLS` the row
    /// fits, and below it the row is deliberately wider than the pane
    /// rather than eliding the id into uselessness.
    #[test]
    fn below_the_floor_the_id_holds_its_minimum_rather_than_vanishing() {
        let rows = vec![mk_run(
            "dispatch-code-reviewer-1787054517-1526-0",
            RunKind::Dispatch,
            RunStatus::Unparseable,
            1,
        )];
        assert_eq!(id_width(&rows, Some(MIN_ROW_COLS)), MIN_ID_COLS);
        assert_eq!(
            id_width(&rows, Some(MIN_ROW_COLS - 20)),
            MIN_ID_COLS,
            "the id column must not shrink past its floor on a very narrow pane"
        );
    }

    /// Piped output (`width == None`) is never clamped and never drops the
    /// subtitle — complete and greppable, matching
    /// `mission_status.rs::plan_layout`'s same rule.
    #[test]
    fn piped_output_keeps_the_full_id_and_subtitle() {
        let mut r = mk_run("a-very-long-run-identifier-indeed", RunKind::Mission, RunStatus::Complete, 1);
        r.role = Some("pr-reviewer".to_string());
        let rows = vec![r];
        let id_w = id_width(&rows, None);
        let line = format_row(2, &rows[0], id_w, None);
        assert!(line.contains("a-very-long-run-identifier-indeed"), "piped id was elided: {line}");
        assert!(line.contains("pr-reviewer"), "piped subtitle was dropped: {line}");
    }

    /// The subtitle is dropped WHOLE rather than wrapped or half-printed
    /// when it cannot fit the known width.
    #[test]
    fn an_overlong_subtitle_is_dropped_whole_not_wrapped() {
        let mut r = mk_run("run-1", RunKind::Mission, RunStatus::Complete, 1);
        r.role = Some("a-role-name-far-too-long-to-fit-in-this-narrow-pane".to_string());
        let rows = vec![r];
        let id_w = id_width(&rows, Some(60));
        let line = format_row(2, &rows[0], id_w, Some(60));
        assert!(!line.contains("a-role-name"), "subtitle should have been dropped whole: {line}");
        assert!(line.chars().count() <= 60);
        assert_eq!(line.lines().count(), 1, "a dropped subtitle must never become a second line");
    }

    /// The `darkmux:` residency namespace is bookkeeping, not operator-
    /// facing text (see [`short_model`]).
    #[test]
    fn the_darkmux_namespace_is_stripped_from_the_displayed_model() {
        let mut r = mk_run("run-1", RunKind::Dispatch, RunStatus::Complete, 1);
        r.model = Some("darkmux:qwen3.6-35b-a3b".to_string());
        let sub = subtitle_for(&r);
        assert_eq!(sub, "qwen3.6-35b-a3b", "the darkmux: prefix leaked into the run listing");
        // A model that never carried the prefix is untouched.
        r.model = Some("gpt-4o".to_string());
        assert_eq!(subtitle_for(&r), "gpt-4o");
    }

    #[test]
    fn ellipsize_never_exceeds_its_budget_and_keeps_the_discriminating_tail() {
        let id = "dispatch-code-reviewer-1787054517-1526-0";
        for max in 1..=id.chars().count() + 2 {
            assert!(ellipsize(id, max).chars().count() <= max.max(1), "max={max}");
        }
        // The tail is what discriminates darkmux's minted ids, so it must
        // survive a middle elision.
        let short = ellipsize(id, 20);
        assert!(short.ends_with("1526-0"), "the discriminating tail was elided: {short}");
        // Multibyte safety: char-indexed, never byte-indexed.
        assert!(ellipsize("ααααααααααββββββββββ", 9).chars().count() <= 9);
    }

    #[test]
    fn a_running_row_marks_its_duration_as_still_elapsing() {
        let mut r = mk_run("live", RunKind::Dispatch, RunStatus::Running, 100);
        r.completed_ts = None;
        let cell = duration_cell(160, &r);
        assert_eq!(cell, "1m+", "a live run's duration must read as at-least, not finished");
    }

    #[test]
    fn an_absent_timestamp_renders_a_dash_rather_than_an_inferred_value() {
        let mut r = mk_run("lab-run", RunKind::Lab, RunStatus::Complete, 100);
        r.started_ts = None;
        assert_eq!(started_cell(200, &r), "-");
        assert_eq!(duration_cell(200, &r), "-");
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
