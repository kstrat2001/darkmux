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
//!
//! (#2902 step 2b) Tokens ride the same union: every row's `tokens` and
//! the `--usage` breakdown come from `darkmux_serve::usage_sum`, the one
//! fold `build_runs_with_usage` runs over the same records, so this verb
//! renders counts and never sums anything itself. `--since` bounds both.

use anyhow::Result;
use serde::Serialize;
use darkmux_serve::usage_sum::{UsageBreakdown, UsageGroup, UsageSplit};
use darkmux_serve::{AbandonReason, Run, RunKind, RunStatus};
use darkmux_types::style;

use crate::cli::RunKindArg;

pub(crate) fn run(
    kind: RunKindArg,
    limit: usize,
    all: bool,
    json: bool,
    usage: bool,
    since: Option<&str>,
) -> Result<i32> {
    let flows_dir = darkmux_types::config_access::flows_dir();
    let lab_dir = darkmux_types::config_access::lab_dir();
    let since_secs = match since {
        Some(spec) => Some(
            darkmux_serve::usage_sum::parse_since(spec, now_unix()).map_err(|e| anyhow::anyhow!(e))?,
        ),
        None => None,
    };
    // (#1905) The SAME three inputs `runs_handler` assembles for `GET
    // /runs` — `fleet_records_for_runs()` degrades to an empty vec on a
    // standalone install (no `DARKMUX_REDIS_URL`), same as the handler.
    let fleet = darkmux_serve::fleet_records_for_runs();
    let mut built = darkmux_serve::build_runs_with_usage(&flows_dir, Some(&lab_dir), &fleet.records, since_secs);
    overlay_not_reporting(&mut built.runs, || {
        let view = darkmux_serve::fleet_view::fetch_local_daemon_view_within(&darkmux_types::config_access::serve_client_addr(), darkmux_serve::fleet_view::LOCAL_VIEW_COLD_WAIT)?;
        Some((view, darkmux_serve::live_session_ids()))
    });
    let mut filtered = filter_since(filter_by_kind(built.runs, kind), since_secs);
    let mut report = usage.then(|| UsageReport {
        since: built.since.clone(),
        default_window: built.default_window,
        breakdown: built.usage,
    });
    if darkmux_types::panel_audience::remote() {
        withhold_addresses(&mut filtered, report.as_mut());
    }

    // The bound to name, whenever the operator gave one.
    let since_label = since.map(|_| built.since.as_str());
    if json {
        let payload = json_payload(&filtered, kind, &fleet.state, since_label, report.as_ref());
        crate::cli_json::emit(&payload)?;
        return Ok(0);
    }

    let selection = select_rows(filtered, limit, all);
    render_text(&selection, kind, &fleet.state, since_label);
    if let Some(report) = &report {
        println!();
        for line in usage_lines(report, style::terminal_width()) {
            println!("{line}");
        }
    }
    Ok(0)
}

/// (5.0) The remote form, for a console viewer that is not this machine
/// (`darkmux_types::panel_audience`): every field darkmux knows is an address
/// is withheld whatever its value, because a peer's records name endpoints
/// this machine's configuration does not, so no list of known values could
/// catch them. A run's route (`kind:host/model`) and a usage group's endpoint
/// string go; a named endpoint still reads as its registry id, which is a
/// name, not an address.
fn withhold_addresses(rows: &mut [Run], report: Option<&mut UsageReport>) {
    use darkmux_types::panel_audience::WITHHELD;
    for route in rows.iter_mut().filter_map(|r| r.route.as_mut()) {
        *route = WITHHELD.to_string();
    }
    for endpoint in report.into_iter().flat_map(|r| r.breakdown.groups.iter_mut()).filter_map(|g| g.endpoint.as_mut()) {
        *endpoint = WITHHELD.to_string();
    }
}

/// (#2902) `--since`: keep the rows active at or after the bound — the
/// same activity stamp [`select_rows`] orders by, so the window and the
/// ordering agree on what "recent" means. `None` keeps everything.
fn filter_since(rows: Vec<Run>, since: Option<u64>) -> Vec<Run> {
    match since {
        Some(bound) => rows.into_iter().filter(|r| run_activity(r) >= bound).collect(),
        None => rows,
    }
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
            "fleet: could not reach the shared stream; showing a snapshot {} old: runs on other machines may be missing or out of date",
            format_span(age_ms / 1_000)
        )),
        SourceState::Unavailable { .. } => Some(
            "fleet: could not reach the shared stream and nothing was cached: runs on other machines are missing from this list".to_string(),
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

/// `updated_ts || completed_ts || started_ts || 0`: the row's own time, for the
/// `--since` bound only. Ordering is by `Run::receive_key`.
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
/// Both halves are ordered newest-first by the hub's receive order (`Run::receive_key`).
/// `all` lifts the cap, and `limit == 0` is treated as unlimited too — the
/// SAME convention `mission status --limit` documents ("0 = no cap"), kept
/// consistent here rather than reinventing a second meaning for zero.
fn select_rows(mut rows: Vec<Run>, limit: usize, all: bool) -> Selection {
    rows.sort_by_key(|r| std::cmp::Reverse(r.receive_key));
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
    let full = format!("showing {shown} of {total} runs ({hidden} more not shown: `--all` for every run)");
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

pub(crate) fn kind_label(kind: RunKind) -> &'static str {
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


/// Whether a running row executed on a machine other than this one: the only
/// rows that can read "not reporting".
fn running_on_another_machine(r: &Run) -> bool {
    if r.status != RunStatus::Running {
        return false;
    }
    match (&r.machine_uid, darkmux_hardware::machine_uid()) {
        (Some(uid), Some(mine)) => !uid.eq_ignore_ascii_case(mine),
        // No uid to compare: by name. A machine whose own name is unknown cannot rule
        // a row out, and a needless look costs only time (the view never marks this machine).
        _ => match (&r.machine, darkmux_types::config_access::machine_id()) {
            (Some(name), Some(mine)) => !name.eq_ignore_ascii_case(&mine),
            (Some(_), None) => true,
            (None, _) => false,
        },
    }
}

/// The overlay the daemon's `/runs` applies (`darkmux_serve::apply_not_reporting`,
/// the one owner of "not reporting"). It gathers only when some running row is on
/// ANOTHER machine, quietly and within a bound (the view from its own local daemon,
/// which on a cold cache waits up to one peer-card timeout; the live beats from
/// Redis, read directly and bounded); a gather that fails marks nothing
/// and says nothing.
fn overlay_not_reporting(
    rows: &mut [Run],
    gather: impl FnOnce() -> Option<(darkmux_serve::fleet_view::FleetView, Option<std::collections::HashSet<String>>)>,
) {
    if !rows.iter().any(running_on_another_machine) {
        return;
    }
    if let Some((view, live)) = gather() {
        darkmux_serve::apply_not_reporting(rows, Some(&view), live.as_ref());
    }
}

/// The STATUS column's word: the same one the viewer's board and run page
/// read (`ui/src/lib/runStatusWord.ts`). `tests/fixtures/run-status-words.json`
/// holds the words both sides must give. An abandoned run says why in its own
/// word, so the column needs no second line for it.
pub(crate) fn status_label(r: &Run) -> &'static str {
    match r.status {
        RunStatus::Running if r.not_reporting => "not reporting",
        RunStatus::Abandoned if r.abandoned_reason == Some(AbandonReason::Aborted) => "aborted",
        RunStatus::Abandoned => "no ending",
        status => status.as_str(),
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

/// (#2902) The TOKENS cell: the row's ALL-tokens sum, compact the way the
/// viewer's `fmtC` (`ui/src/lib/format.ts`) prints a tile — exact below
/// 1000, two decimals in the thousands (so a difference of a few hundred
/// tokens between two rows stays visible), one in the millions, none from
/// 10M. `-` when nothing was measured; never a `0` standing in for
/// "unknown".
///
/// One deliberate difference: from 999,995 the millions arm takes over,
/// because two decimals of thousands round that to `1000.00k`, a cell one
/// column too wide that also spells a million as a thousand. (`fmtC` has
/// the same boundary and renders it that way today.)
fn tokens_cell(tokens: Option<u64>) -> String {
    let Some(n) = tokens else {
        return "-".to_string();
    };
    if n >= 10_000_000 {
        format!("{:.0}M", n as f64 / 1e6)
    } else if n >= 999_995 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1000 {
        format!("{:.2}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Thousands-grouped integer (the viewer's `fmtN`), for the breakdown's
/// exact counts.
pub(crate) fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, chunk) in digits.as_bytes().rchunks(3).rev().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(std::str::from_utf8(chunk).expect("ascii digits"));
    }
    out
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
/// the Rust twin of `ui/src/lib/format.ts::shortModel`, which
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
    darkmux_gestalt::bare_model_key(model)
}

/// The narrow-pane subtitle: everything [`subtitle_for`] carries, plus the
/// machine, for when the MACHINE column has been shed (#1929).
fn subtitle_with_machine(r: &Run) -> String {
    let mut bits = subtitle_bits(r);
    bits.extend(r.machine.clone());
    bits.extend(relay_text(r));
    bits.join(" · ")
}

/// A relayed run says where it was asked: "from <machine>", the same words as
/// `ui/src/lib/relayWords.ts` (`tests/fixtures/run-status-words.json`).
fn relay_text(r: &Run) -> Option<String> {
    r.relay.as_ref().map(|relay| format!("from {}", relay.asked_on_machine))
}

/// `[workload ·] [verify ·] role · model · via route · [from machine]` — the
/// same fields, join and order as `ui/src/lenses/runs/format.ts::runSubtitle`,
/// including its `shortModel` treatment of the model id (see [`short_model`]).
/// Why an abandoned run stopped is in its STATUS word, not here.
///
/// (#1929) Machine is NOT here any more — it is a real headed column. It
/// used to ride in this subtitle, which reads as prose on a mission row
/// (`gpt-4o · via azure:… · m1-max-32gb-studio`) but collapses on a lab
/// row, which has no role, model or route, to a bare machine name under
/// no header — reading exactly like a column the table forgot to label.
/// See [`subtitle_with_machine`] for the narrow-pane case where the
/// column is shed and machine rejoins the line.
fn subtitle_for(r: &Run) -> String {
    let mut bits = subtitle_bits(r);
    bits.extend(relay_text(r));
    bits.join(" · ")
}

fn subtitle_bits(r: &Run) -> Vec<String> {
    let mut bits: Vec<String> = Vec::new();
    if let Some(workload) = &r.workload {
        bits.push(workload.clone());
    }
    if let Some(verify) = verify_label(r) {
        bits.push(verify.to_string());
    }
    if let Some(role) = &r.role {
        bits.push(role.clone());
    }
    if let Some(model) = &r.model {
        bits.push(short_model(model).to_string());
    }
    if let Some(route) = &r.route {
        bits.push(format!("via {route}"));
    }
    bits
}

/// A lab row's verify outcome, in three states rather than two (#2494): what
/// the workload's own tests said is a different fact from how the dispatch
/// ended (`status`), and a failed verify has to be visible in this table.
/// Missions and dispatches have no verify to report, and a lab run with no
/// manifest yet (still running) has none either.
fn verify_label(r: &Run) -> Option<&'static str> {
    match (r.kind, r.verify_passed, r.workload.is_some()) {
        (RunKind::Lab, Some(true), _) => Some("verify pass"),
        (RunKind::Lab, Some(false), _) => Some(VERIFY_FAIL_LABEL),
        // A manifest exists (it names the workload) and declares no verify.
        (RunKind::Lab, None, true) => Some("verify \u{2014}"),
        // No manifest yet, so nothing has been checked or declared.
        (RunKind::Lab, None, false) => None,
        (RunKind::Mission | RunKind::Dispatch, _, _) => None,
    }
}

const VERIFY_FAIL_LABEL: &str = "verify FAIL";

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
const STATUS_COLS: usize = 13; // "not reporting"
const STARTED_COLS: usize = 10;
const DURATION_COLS: usize = 10;
/// (#2902) `999.99k` is the widest [`tokens_cell`] below a billion tokens.
const TOKENS_COLS: usize = 7;
/// The two-space gap between DURATION and ID (every other gap is one).
const ID_GAP_COLS: usize = 2;
/// (#1929) MACHINE is a real column, not a subtitle field. It is present on
/// most rows and is genuinely tabular, and leaving it in the subtitle made
/// a lab row — which has no role, model or route — render a bare machine
/// name under no header, reading exactly like a column the table forgot to
/// label. Widest fleet name today is `m1-max-32gb-studio` (18).
const MACHINE_COLS: usize = 20;

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
const FIXED_COLS: usize = INDENT_COLS
    + KIND_COLS
    + 1
    + STATUS_COLS
    + 1
    + STARTED_COLS
    + 1
    + DURATION_COLS
    + 1
    + TOKENS_COLS
    + ID_GAP_COLS;

/// (#1929) What the MACHINE column costs when shown: the column plus its
/// separating space. Kept OUT of [`FIXED_COLS`] because the column is
/// SHED on a narrow pane rather than being unconditional -- see
/// [`show_machine_column`]. Adding it unconditionally raised the narrowest
/// honorable row from 58 to 79 columns, and the console panel negotiates
/// as little as 36 on a phone, so a mandatory column would have made the
/// mobile render worse to fix a desktop one.
const MACHINE_TOTAL_COLS: usize = MACHINE_COLS + 1;

/// Is there room for the MACHINE column at this width?
///
/// Piped output (`None`) always shows it -- complete and greppable, the
/// same rule `id_width` follows. A known width shows it only if the row
/// still clears its floor with the column's cost added; otherwise machine
/// falls back into the subtitle, where it lived before this change, and
/// the table stays inside the pane. This is the column-shedding
/// `MIN_ROW_COLS`'s own doc anticipated for the phone-width case.
fn show_machine_column(width: Option<usize>) -> bool {
    match width {
        None => true,
        Some(w) => w >= MIN_ROW_COLS + MACHINE_TOTAL_COLS,
    }
}
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
    // (#1929) The MACHINE column's cost belongs in this budget whenever the
    // column is actually shown. Omitting it let a width of 80 pass
    // `show_machine_column` (floor 79) and then render 101 columns, because
    // the id was sized as if the column were not there. Caught by
    // `every_clamped_row_fits_the_width_it_was_planned_for`, which measures
    // the RENDERED string rather than recomputing the sum.
    let fixed = FIXED_COLS + if show_machine_column(width) { MACHINE_TOTAL_COLS } else { 0 };
    // (#1929) The 90th percentile, NOT the max. Measured on a real corpus:
    // 501 rows, median id 26 chars, widest 83 — a single tempdir-derived
    // `pr-review--var-folders-...-wt-...` id. Padding to the max made every
    // one of the other 500 rows carry ~57 columns of whitespace before the
    // next field, which is what the operator noticed. Ellipsizing one
    // outlier costs less than spacing out the whole table for it, and
    // `ellipsize` keeps the discriminating tail.
    let mut lens: Vec<usize> = rows.iter().map(|r| r.id.chars().count()).collect();
    lens.sort_unstable();
    let max_id = if lens.is_empty() {
        0
    } else {
        lens[(lens.len() * 9 / 10).min(lens.len() - 1)]
    };
    match width {
        None => max_id,
        Some(w) => {
            if max_id + fixed <= w {
                max_id
            } else {
                // Clamp to the pane, but never below [`MIN_ROW_COLS`]'s
                // floor — expressed by raising `w` to the floor rather
                // than by a second `.max(MIN_ID_COLS)`, so the narrowest
                // honorable row is stated in ONE place and this branch
                // cannot drift from it. (`MIN_ROW_COLS > FIXED_COLS` by
                // construction, so this subtraction cannot underflow.)
                w.max(MIN_ROW_COLS + (fixed - FIXED_COLS)) - fixed
            }
        }
    }
}

fn header_line(id_w: usize, machine_col: bool) -> String {
    format!(
        "{:i$}{:<k$} {:<s$} {:<t$} {:<d$} {:>n$}{:g$}{:<id_w$}{}",
        "",
        "KIND",
        "STATUS",
        "STARTED",
        "DURATION",
        "TOKENS",
        "",
        "ID",
        if machine_col { format!(" {:<w$}", "MACHINE", w = MACHINE_COLS) } else { String::new() },
        i = INDENT_COLS,
        k = KIND_COLS,
        s = STATUS_COLS,
        t = STARTED_COLS,
        d = DURATION_COLS,
        n = TOKENS_COLS,
        g = ID_GAP_COLS,
    )
}

/// Render one row. Split from the `println!` so a test can MEASURE what
/// this returns — see [`FIXED_COLS`] for why measuring beats recomputing
/// the budget.
fn format_row(now: u64, r: &Run, id_w: usize, width: Option<usize>, machine_col: bool) -> String {
    let id_cell = format!("{:<id_w$}", ellipsize(&r.id, id_w));
    let base = format!(
        "{:i$}{:<k$} {:<s$} {:<t$} {:<d$} {:>n$}{:g$}{}{}",
        "",
        kind_label(r.kind),
        status_label(r),
        started_cell(now, r),
        duration_cell(now, r),
        tokens_cell(r.tokens),
        "",
        id_cell,
        if machine_col {
            format!(" {:<w$}", ellipsize(r.machine.as_deref().unwrap_or("-"), MACHINE_COLS), w = MACHINE_COLS)
        } else {
            String::new()
        },
        i = INDENT_COLS,
        k = KIND_COLS,
        s = STATUS_COLS,
        t = STARTED_COLS,
        d = DURATION_COLS,
        n = TOKENS_COLS,
        g = ID_GAP_COLS,
    );
    // (#1929) When the column is shed, machine rejoins the subtitle so the
    // information is never simply lost on a narrow pane.
    let subtitle = if machine_col { subtitle_for(r) } else { subtitle_with_machine(r) };
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
        // A failed verify outlives the dropped subtitle: hiding it on a
        // narrow terminal would bring back the tick #2494 removed.
        Some(w) => {
            let failed = format!("{base}  {VERIFY_FAIL_LABEL}");
            if verify_label(r) == Some(VERIFY_FAIL_LABEL) && failed.chars().count() <= w {
                failed
            } else {
                base
            }
        }
    }
}

/// The empty state. With `--since` it names the bound (review CONSIDER
/// 6): "nothing recorded yet" would be a claim about all history, and a
/// bounded question deserves a bounded answer.
fn empty_state_line(kind: RunKindArg, since: Option<&str>) -> String {
    match (kind, since) {
        (RunKindArg::All, None) => "no recorded run activity yet".to_string(),
        (_, None) => format!("no recorded {} runs yet", kind_arg_label(kind)),
        (RunKindArg::All, Some(bound)) => format!("no run activity since {bound}"),
        (_, Some(bound)) => format!("no {} runs since {bound}", kind_arg_label(kind)),
    }
}

fn render_text(
    sel: &Selection,
    kind: RunKindArg,
    fleet: &darkmux_serve::source_state::SourceState,
    since: Option<&str>,
) {
    // Printed BEFORE the table (and before the empty-state line) — an
    // incomplete answer has to be qualified where the reader meets it, not
    // in a footnote under rows they have already believed.
    if let Some(warning) = fleet_warning(fleet) {
        eprintln!("{}", style::warn(&warning));
    }
    if sel.rows.is_empty() {
        println!("{}", style::dim(&empty_state_line(kind, since)));
        return;
    }

    let width = style::terminal_width();
    let machine_col = show_machine_column(width);
    let id_w = id_width(&sel.rows, width);
    let now = now_unix();

    println!("{}", style::header(&format!("runs: {} shown", sel.rows.len())));
    println!("{}", header_line(id_w, machine_col));
    for r in &sel.rows {
        println!("{}", format_row(now, r, id_w, width, machine_col));
    }
    if let Some(line) = footer(sel, width) {
        println!("{}", style::dim(&line));
    }
}

/// (#2902) What `--usage` reports: the breakdown and the bound it covers.
#[derive(Serialize, schemars::JsonSchema)]
pub struct UsageReport {
    /// The inclusive bound, in the flow schema's own `ts` spelling: the
    /// operator's `--since`, else the default window's first day.
    since: String,
    /// True when no `--since` was given and the bound is the default
    /// 14-day scan window.
    default_window: bool,
    #[serde(flatten)]
    breakdown: UsageBreakdown,
}

/// The `--json` document. The top-level `since` appears whenever `--since`
/// was given, `usage` only with `--usage`; every row carries its own
/// `tokens` regardless.
///
/// Never paginated (#1905, matching `mission status --json`): a machine reader
/// gets every row the kind filter selected; `--limit`/`--all` only shape the
/// human table.
#[derive(Serialize, schemars::JsonSchema)]
pub struct RunListOutput<'a> {
    /// The kind filter the rows were selected by.
    pub kind: RunKindArg,
    /// Most recently active first.
    pub runs: Vec<&'a Run>,
    pub total: usize,
    /// The same honesty `runs_handler` ships as `meta`: a script must be able
    /// to tell an incomplete answer from a quiet fleet.
    pub fleet: &'a darkmux_serve::source_state::SourceState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<&'a UsageReport>,
}

fn json_payload<'a>(
    rows: &'a [Run],
    kind: RunKindArg,
    fleet: &'a darkmux_serve::source_state::SourceState,
    since: Option<&'a str>,
    usage: Option<&'a UsageReport>,
) -> RunListOutput<'a> {
    let mut sorted: Vec<&Run> = rows.iter().collect();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.receive_key));
    RunListOutput { kind, total: sorted.len(), runs: sorted, fleet, since, usage }
}

// ── (#2902) the --usage breakdown ────────────────────────────────────

const USAGE_INDENT: usize = 2;
const CALLS_COLS: usize = 5;
const COUNT_COLS: usize = 11; // "999,999,999"
/// `GENERATED` is the widest numeric header.
const GENERATED_COLS: usize = 10;
/// Endpoint and model columns are sized to their content, capped so one
/// long deployment label cannot push the counts off a pane.
const ENDPOINT_MAX_COLS: usize = 40;
const MACHINE_MAX_COLS: usize = 20;
const MODEL_MAX_COLS: usize = 28;

/// What the ENDPOINT cell shows: the registry id a named endpoint went
/// through, else the endpoint string darkmux called.
fn endpoint_cell(g: &UsageGroup) -> &str {
    none_or(g.endpoint_id.as_deref().or(g.endpoint.as_deref()))
}

fn none_or(s: Option<&str>) -> &str {
    s.unwrap_or("(none)")
}

fn count_cell(n: Option<u64>) -> String {
    match n {
        Some(n) => grouped(n),
        None => "-".to_string(),
    }
}

/// One numeric row of the breakdown: `CALLS INPUT CACHED GENERATED TOTAL`,
/// right-aligned, `-` for a cached count nothing reported.
fn usage_numbers(s: &UsageSplit) -> String {
    format!(
        "{:>c$} {:>w$} {:>w$} {:>g$} {:>w$}",
        grouped(s.calls),
        grouped(s.input),
        count_cell(s.cached),
        grouped(s.generated),
        grouped(s.total),
        c = CALLS_COLS,
        w = COUNT_COLS,
        g = GENERATED_COLS,
    )
}

fn merged(a: &UsageSplit, b: &UsageSplit) -> UsageSplit {
    UsageSplit {
        calls: a.calls + b.calls,
        total: a.total + b.total,
        input: a.input + b.input,
        cached: match (a.cached, b.cached) {
            (None, None) => None,
            (x, y) => Some(x.unwrap_or(0) + y.unwrap_or(0)),
        },
        generated: a.generated + b.generated,
        unreported: a.unreported + b.unreported,
    }
}

/// The calls that belong to no run (radio routing, a `doctor --probe`) are in
/// the totals above but on no run's TOKENS cell; say how much, so the rows
/// plus this line equal the total.
fn no_run_note(split: &UsageSplit) -> String {
    let calls = if split.calls == 1 { "call" } else { "calls" };
    format!("{} {calls} on no run ({} tokens): in the totals, on no run's TOKENS cell", grouped(split.calls), grouped(split.total))
}

/// (#3067) Calls that name a run with no row in this listing (its start record
/// is outside the window): in the totals, on no row.
fn unlisted_note(split: &UsageSplit) -> String {
    let calls = if split.calls == 1 { "call" } else { "calls" };
    format!("{} {calls} on a run with no row here ({} tokens): in the totals, on no row's TOKENS cell", grouped(split.calls), grouped(split.total))
}

/// The calls a provider answered without a usage block are counted as calls
/// and add 0 tokens here, the same reading the endpoint window budget takes
/// (the conservative charge applies only to a dispatch's own cap). Say so
/// rather than let a short total pass for a complete one.
fn unreported_note(n: u64) -> String {
    let calls = if n == 1 { "call" } else { "calls" };
    format!("{n} {calls} reported no usage: counted in CALLS, 0 tokens each")
}

/// The `--usage` section, as lines. One line per executing machine +
/// endpoint + requested model carrying the group's ALL tokens; beneath it, `utility` when
/// darkmux's own calls (compaction, radio routing) landed there, and
/// `reported model:` when the reply named a model other than the one
/// requested — each a fact off the records, nothing inferred. Then the
/// totals. Endpoint and model cells are ellipsized only against a KNOWN
/// width; piped output stays complete and greppable, the same rule the
/// table above follows.
fn usage_lines(report: &UsageReport, width: Option<usize>) -> Vec<String> {
    let b = &report.breakdown;
    let o = &b.overall;
    let mut lines = Vec::new();
    let window = if report.default_window { "the default 14-day window" } else { "--since" };
    lines.push(style::header(&format!(
        "usage since {} ({window}) · {} calls",
        report.since,
        grouped(o.usage_records)
    )));

    let machine_w = b
        .groups
        .iter()
        .map(|g| none_or(g.machine.as_deref()).chars().count())
        .max()
        .unwrap_or(0)
        .max("MACHINE".len());
    let ep_w = b.groups.iter().map(|g| endpoint_cell(g).chars().count()).max().unwrap_or(0).max("ENDPOINT".len());
    let model_w = b
        .groups
        .iter()
        .map(|g| none_or(g.requested_model.as_deref()).chars().count())
        .max()
        .unwrap_or(0)
        .max("MODEL".len());
    let (machine_w, ep_w, model_w) = match width {
        Some(_) => (machine_w.min(MACHINE_MAX_COLS), ep_w.min(ENDPOINT_MAX_COLS), model_w.min(MODEL_MAX_COLS)),
        None => (machine_w, ep_w, model_w),
    };
    let label = |machine: &str, ep: &str, model: &str| {
        format!(
            "{:i$}{:<a$} {:<e$} {:<m$} ",
            "",
            ellipsize(machine, machine_w),
            ellipsize(ep, ep_w),
            ellipsize(model, model_w),
            i = USAGE_INDENT,
            a = machine_w,
            e = ep_w,
            m = model_w
        )
    };
    lines.push(format!(
        "{}{:>c$} {:>w$} {:>w$} {:>g$} {:>w$}",
        label("MACHINE", "ENDPOINT", "MODEL"),
        "CALLS",
        "INPUT",
        "CACHED",
        "GENERATED",
        "TOTAL",
        c = CALLS_COLS,
        w = COUNT_COLS,
        g = GENERATED_COLS,
    ));
    let sub_indent = " ".repeat(USAGE_INDENT + 2);
    for g in &b.groups {
        lines.push(format!(
            "{}{}",
            label(none_or(g.machine.as_deref()), endpoint_cell(g), none_or(g.requested_model.as_deref())),
            usage_numbers(&merged(&g.work, &g.utility))
        ));
        if g.utility.calls > 0 {
            lines.push(format!(
                "{sub_indent}{:<u$} {}",
                "utility",
                usage_numbers(&g.utility),
                u = machine_w + 1 + ep_w + 1 + model_w - 2,
            ));
        }
        if let Some(served) = &g.reported_model {
            lines.push(style::dim(&format!("{sub_indent}reported model: {served}")));
        }
    }
    let (mut work, mut utility) = (UsageSplit::default(), UsageSplit::default());
    for g in &b.groups {
        work = merged(&work, &g.work);
        utility = merged(&utility, &g.utility);
    }
    let all = merged(&work, &utility);
    lines.push(format!("{}{}", label("all", "", ""), usage_numbers(&all)));
    if utility.calls > 0 {
        lines.push(format!("{}{}", label("utility", "", ""), usage_numbers(&utility)));
    }
    if b.no_run.calls > 0 {
        lines.push(style::dim(&no_run_note(&b.no_run)));
    }
    if b.unlisted.calls > 0 {
        lines.push(style::dim(&unlisted_note(&b.unlisted)));
    }
    if all.unreported > 0 {
        lines.push(style::dim(&unreported_note(all.unreported)));
    }
    lines
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
            machine_uid: None,
            route: None,
            role: None,
            model: None,
            started_ts: Some(updated_ts),
            completed_ts: if status == RunStatus::Running { None } else { Some(updated_ts) },
            updated_ts: Some(updated_ts),
            tracked: true,
            // (#1915) This CLI-verb test helper only exercises filtering/
            // sorting, never drill-in — `None` is the honest value for a
            // synthetic row that was never joined to a real flow session.
            dispatch_id: None,
            // (#1907) None of this module's tests exercise `abandoned_reason`
            // directly — that behavior is covered in `darkmux-serve`'s own
            // `runs.rs` tests, where every construction site lives. This
            // helper's rows are never `Abandoned` in the existing suite.
            abandoned_reason: None,
            tokens: None,
            workload: None,
            verify_passed: None,
            relay: None,
            receive_key: updated_ts * 1000 * 1024,
            not_reporting: false,
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
            "the live run was truncated away: this is the exact defect #1905 exists to fix"
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

    // ── columns (#1929) ──────────────────────────────────────────────

    /// One outlier id must not widen the table for every other row.
    /// Measured on the real corpus: 501 rows, median id 26 chars, widest 83
    /// (a single tempdir-derived id). Padding to the max left ~57 columns
    /// of whitespace on every other row, which is what the operator saw.
    #[test]
    fn one_freakishly_long_id_does_not_pad_every_other_row() {
        let mut rows: Vec<Run> =
            (0..20u64).map(|i| mk_run(&format!("short-{i}"), RunKind::Lab, RunStatus::Complete, i)).collect();
        rows.push(mk_run(&"x".repeat(83), RunKind::Mission, RunStatus::Complete, 99));
        let w = id_width(&rows, None);
        assert!(
            w < 40,
            "one 83-char id widened the column to {w}: the other 20 rows now carry that padding"
        );
    }

    /// MACHINE is a real, HEADED column. It used to ride in the subtitle,
    /// which reads as prose on a mission row (`gpt-4o · via azure:…`) but
    /// collapses to a bare machine name on a lab row, where it looks
    /// exactly like a column the table forgot to label.
    #[test]
    fn machine_renders_under_its_own_header_and_not_in_the_subtitle() {
        let mut r = mk_run("run-1", RunKind::Lab, RunStatus::Complete, 1);
        r.machine = Some("MacBook-Pro".to_string());
        assert!(header_line(20, true).contains("MACHINE"), "the column must be labelled");
        assert!(format_row(2, &r, 20, None, true).contains("MacBook-Pro"));
        assert!(!subtitle_for(&r).contains("MacBook-Pro"), "machine must no longer be a subtitle field");
    }

    #[test]
    fn a_row_with_no_machine_renders_a_dash_under_the_column() {
        let r = mk_run("run-1", RunKind::Mission, RunStatus::Complete, 1);
        assert!(format_row(2, &r, 20, None, true).contains('-'));
    }

    /// The column is SHED on a pane too narrow for it, and machine rejoins
    /// the subtitle so the information is never simply lost. Adding it
    /// unconditionally would have raised the narrowest honorable row from
    /// 58 to 79 columns, and the console panel negotiates as little as 36.
    #[test]
    fn the_machine_column_sheds_on_a_narrow_pane_rather_than_overflowing() {
        assert!(show_machine_column(None), "piped output stays complete");
        assert!(show_machine_column(Some(200)));
        assert!(!show_machine_column(Some(54)), "a phone-width pane must shed it");

        let mut r = mk_run("run-1", RunKind::Dispatch, RunStatus::Complete, 1);
        r.machine = Some("MacBook-Pro".to_string());
        assert_eq!(subtitle_with_machine(&r), "MacBook-Pro", "shed means it rejoins the subtitle");
        r.role = Some("coder".to_string());
        assert_eq!(subtitle_with_machine(&r), "coder · MacBook-Pro");
    }

    /// Shedding must not have moved the floor: `MACHINE_TOTAL_COLS` stays
    /// out of `FIXED_COLS` precisely so a narrow render is no worse than
    /// before the column existed.
    #[test]
    fn adding_the_machine_column_did_not_raise_the_narrow_floor() {
        assert_eq!(MIN_ROW_COLS, FIXED_COLS + MIN_ID_COLS);
        assert!(!show_machine_column(Some(MIN_ROW_COLS)), "the floor row has no room for the column");
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
            let header = header_line(id_w, show_machine_column(Some(w)));
            let row = format_row(2_000, &rows[0], id_w, Some(w), show_machine_column(Some(w)));
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

    #[derive(serde::Deserialize)]
    struct WordCase {
        status: RunStatus,
        #[serde(default)]
        abandoned_reason: Option<AbandonReason>,
        #[serde(default)]
        not_reporting: bool,
        word: String,
    }
    #[derive(serde::Deserialize)]
    struct RelayCase {
        asked_on_machine: String,
        text: String,
    }
    #[derive(serde::Deserialize)]
    struct Words {
        statuses: Vec<WordCase>,
        relay: RelayCase,
    }
    fn words() -> Words {
        serde_json::from_str(include_str!("../tests/fixtures/run-status-words.json")).expect("the shared words parse")
    }

    /// 5.0: the STATUS column reads the word the viewer's board and run page
    /// read, from one shared fixture (`runStatusWord.test.ts` reads it too).
    /// A degraded run is `degraded`, never `complete` (F10/F11).
    #[test]
    fn the_status_column_reads_the_words_the_board_reads() {
        for c in words().statuses {
            let mut r = mk_run("r", RunKind::Mission, c.status, 1);
            r.abandoned_reason = c.abandoned_reason;
            r.not_reporting = c.not_reporting;
            assert_eq!(status_label(&r), c.word, "{:?} {:?}", c.status, c.abandoned_reason);
        }
    }

    fn running_on(machine: &str) -> Run {
        let mut r = mk_run("r", RunKind::Dispatch, RunStatus::Running, 1);
        r.machine = Some(machine.to_string());
        r
    }

    fn down_peer_view() -> darkmux_serve::fleet_view::FleetView {
        use crate::machine_list::tests::{machine, own_row, view};
        use darkmux_serve::fleet_view::{CardOutcome, Liveness, UnreachableReason};
        view(vec![
            own_row(),
            machine("far-peer", Liveness::NoBeat, CardOutcome::Unreachable { reason: UnreachableReason::ListenerOff, detail: None }),
        ])
    }

    /// 5.0: `run list` pays for no gather unless a row is running on ANOTHER machine.
    #[test]
    fn the_overlay_gathers_nothing_when_no_row_runs_on_another_machine() {
        let mut done_elsewhere = running_on("far-peer");
        done_elsewhere.status = RunStatus::Complete;
        let mut rows = vec![done_elsewhere, mk_run("local", RunKind::Dispatch, RunStatus::Running, 1)];
        overlay_not_reporting(&mut rows, || panic!("a gather happened with no running remote row"));
        assert!(rows.iter().all(|r| !r.not_reporting));
    }

    /// The overlay is wired at this call site: a running row on a down peer reads
    /// not reporting; a gather that fails, or a failed beat read, marks nothing.
    #[test]
    fn the_overlay_marks_a_running_row_on_a_down_peer_and_nothing_on_a_failed_read() {
        let mut rows = vec![running_on("far-peer")];
        overlay_not_reporting(&mut rows, || Some((down_peer_view(), Some(Default::default()))));
        assert!(rows[0].not_reporting, "the down peer's running row is marked");

        let mut rows = vec![running_on("far-peer")];
        overlay_not_reporting(&mut rows, || None);
        assert!(!rows[0].not_reporting, "no view: nothing is marked");

        let mut rows = vec![running_on("far-peer")];
        overlay_not_reporting(&mut rows, || Some((down_peer_view(), None)));
        assert!(!rows[0].not_reporting, "a failed beat read: nothing is marked");
    }

    /// 5.0 (#3017): `run list` orders by the hub's receive order like the board,
    /// never by an executor's clock.
    #[test]
    fn rows_order_by_receive_key_not_by_the_executors_clock() {
        let mut skewed = mk_run("skewed", RunKind::Mission, RunStatus::Complete, 4_000_000_000);
        skewed.receive_key = 1_700_000_000_000 * 1024;
        let mut honest = mk_run("honest", RunKind::Dispatch, RunStatus::Complete, 1_700_000_100);
        honest.receive_key = 1_700_000_100_000 * 1024;
        let sel = select_rows(vec![skewed.clone(), honest.clone()], 10, false);
        let ids: Vec<&str> = sel.rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["honest", "skewed"]);
        let rows = [skewed, honest];
        let payload = json_payload(&rows, RunKindArg::All, &darkmux_serve::source_state::SourceState::Ok, None, None);
        assert_eq!(payload.runs.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["honest", "skewed"]);
    }

    /// A relayed run says "from <machine>", in the subtitle (and in the narrow
    /// pane, after the machine), as the board does.
    #[test]
    fn a_relayed_run_reads_from_the_asking_machine() {
        let w = words().relay;
        let mut r = mk_run("r", RunKind::Dispatch, RunStatus::Complete, 1);
        r.role = Some("coder".into());
        r.machine = Some("studio".into());
        r.relay = Some(darkmux_serve::RunRelay { asked_on_machine: w.asked_on_machine, sender_run: None });
        assert_eq!(subtitle_for(&r), format!("coder · {}", w.text));
        assert_eq!(subtitle_with_machine(&r), format!("coder · studio · {}", w.text));
    }

    /// Every label a column can emit must FIT that column, because
    /// `{:<w$}` is a minimum width and never truncates: one label over
    /// budget silently widens every row carrying it. `unparseable` (11)
    /// is why `STATUS_COLS` is not 10.
    #[test]
    fn every_column_label_fits_its_width() {
        for c in words().statuses {
            let label = c.word;
            assert!(
                label.chars().count() <= STATUS_COLS,
                "status label {label:?} is {} cols, STATUS_COLS is {STATUS_COLS}: every row \
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
        let line = format_row(2, &rows[0], id_w, None, true);
        assert!(line.contains("a-very-long-run-identifier-indeed"), "piped id was elided: {line}");
        assert!(line.contains("pr-reviewer"), "piped subtitle was dropped: {line}");
    }

    /// The subtitle is dropped WHOLE rather than wrapped or half-printed
    /// when it cannot fit the known width.
    #[test]
    fn an_overlong_subtitle_is_dropped_whole_not_wrapped() {
        // The narrowest pane the table supports (`MIN_ROW_COLS`). The STATUS column grew to
        // 13 for "not reporting", so this floor is 2 columns wider than it was at 60.
        let mut r = mk_run("run-1", RunKind::Mission, RunStatus::Complete, 1);
        r.role = Some("a-role-name-far-too-long-to-fit-in-this-narrow-pane".to_string());
        let rows = vec![r];
        let id_w = id_width(&rows, Some(MIN_ROW_COLS));
        let line = format_row(2, &rows[0], id_w, Some(MIN_ROW_COLS), show_machine_column(Some(MIN_ROW_COLS)));
        assert!(!line.contains("a-role-name"), "subtitle should have been dropped whole: {line}");
        assert!(line.chars().count() <= MIN_ROW_COLS);
        assert_eq!(line.lines().count(), 1, "a dropped subtitle must never become a second line");
    }

    /// (#2494) A run whose dispatch succeeded and whose tests failed must say
    /// so on its row in the verb the operator reaches for first: three
    /// states, never two. Verify rides the subtitle and never the STATUS
    /// cell, so `status` keeps meaning how the dispatch ended.
    #[test]
    fn a_lab_row_shows_its_verify_outcome_in_three_states() {
        let lab = |verify: Option<bool>| {
            let mut r = mk_run("quick-coding-1", RunKind::Lab, RunStatus::Complete, 1);
            r.workload = Some("quick-coding".to_string());
            r.model = Some("darkmux:qwen3.6-35b-a3b".to_string());
            r.verify_passed = verify;
            r
        };
        assert_eq!(subtitle_for(&lab(Some(false))), "quick-coding \u{b7} verify FAIL \u{b7} qwen3.6-35b-a3b");
        assert_eq!(subtitle_for(&lab(Some(true))), "quick-coding \u{b7} verify pass \u{b7} qwen3.6-35b-a3b");
        assert_eq!(subtitle_for(&lab(None)), "quick-coding \u{b7} verify \u{2014} \u{b7} qwen3.6-35b-a3b");
        let failed = format_row(2, &lab(Some(false)), 20, None, true);
        assert!(failed.contains("verify FAIL"), "{failed}");
        assert!(failed.contains("complete"), "the dispatch result is still shown as it was: {failed}");
        // A run still in flight has no manifest, hence no workload and no verify.
        let mut running = lab(None);
        running.workload = None;
        assert!(!subtitle_for(&running).contains("verify"), "{}", subtitle_for(&running));
        // A mission or dispatch row has no verify to report.
        let mut m = mk_run("m1", RunKind::Mission, RunStatus::Complete, 1);
        m.verify_passed = Some(false);
        assert!(!subtitle_for(&m).contains("verify"), "verify is a lab-row fact");
    }

    /// The subtitle is dropped whole when it does not fit, but a FAILED
    /// verify must survive a narrow terminal: hiding it there would bring
    /// back exactly what #2494 fixed.
    #[test]
    fn a_failed_verify_survives_a_terminal_too_narrow_for_the_subtitle() {
        let mut r = mk_run("quick-coding-1", RunKind::Lab, RunStatus::Complete, 1);
        r.workload = Some("quick-coding".to_string());
        r.model = Some("qwen3.6-35b-a3b-with-a-long-name".to_string());
        r.verify_passed = Some(false);
        let base_len = format_row(2, &mk_run("quick-coding-1", RunKind::Mission, RunStatus::Complete, 1), 20, Some(200), false)
            .chars()
            .count();
        let width = base_len + "  verify FAIL".chars().count();
        let line = format_row(2, &r, 20, Some(width), false);
        assert!(line.ends_with("verify FAIL"), "{line}");
        assert!(line.chars().count() <= width, "still fits the width: {line}");
        assert!(!line.contains("qwen3.6"), "the rest of the subtitle is still dropped: {line}");
        // Passing and unchecked rows stay dropped-whole.
        r.verify_passed = Some(true);
        assert!(!format_row(2, &r, 20, Some(width), false).contains("verify"));
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

    /// An abandoned row's reason is its STATUS word, so the subtitle carries none.
    #[test]
    fn subtitle_for_carries_no_reason() {
        let r = mk_run("run-1", RunKind::Mission, RunStatus::Complete, 1);
        assert_eq!(subtitle_for(&r), "");
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

    // ── #2902 step 2b: the TOKENS column, --since, --usage ────────────

    /// The cell mirrors the viewer's `fmtC` (`ui/src/lib/format.ts`):
    /// exact below 1000, two decimals in the thousands, one in the
    /// millions, none from 10M — and `-` when nothing was measured.
    #[test]
    fn tokens_cell_is_compact_like_the_viewer_and_dashes_when_absent() {
        assert_eq!(tokens_cell(None), "-");
        assert_eq!(tokens_cell(Some(0)), "0");
        assert_eq!(tokens_cell(Some(984)), "984");
        assert_eq!(tokens_cell(Some(999)), "999");
        assert_eq!(tokens_cell(Some(1000)), "1.00k");
        assert_eq!(tokens_cell(Some(29_180)), "29.18k");
        assert_eq!(tokens_cell(Some(999_600)), "999.60k");
        assert_eq!(tokens_cell(Some(999_994)), "999.99k");
        assert_eq!(tokens_cell(Some(999_995)), "1.0M", "rounds into the millions arm, never 1000.00k");
        assert_eq!(tokens_cell(Some(1_234_567)), "1.2M");
        assert_eq!(tokens_cell(Some(12_345_678)), "12M");
        for n in [0u64, 999, 1000, 999_999, 1_000_000, 9_999_999, 10_000_000, 999_999_999] {
            assert!(tokens_cell(Some(n)).chars().count() <= TOKENS_COLS, "{n} overflows TOKENS_COLS");
        }
    }

    /// TOKENS sits between DURATION and ID, right-aligned under its header
    /// so a column of counts lines up on the last digit.
    #[test]
    fn header_and_row_carry_the_tokens_column_between_duration_and_id() {
        let header = header_line(12, false);
        let d = header.find("DURATION").expect("DURATION header");
        let t = header.find("TOKENS").expect("TOKENS header");
        let i = header.find("ID").expect("ID header");
        assert!(d < t && t < i, "{header:?}");
        let mut r = mk_run("run-1", RunKind::Dispatch, RunStatus::Complete, 100);
        r.tokens = Some(29_180);
        let row = format_row(200, &r, 12, None, false);
        let cell_end = row.find("29.18k").expect("the cell") + "29.18k".len();
        assert_eq!(cell_end, t + "TOKENS".len(), "right-aligned under the header:\n{header}\n{row}");
        r.tokens = None;
        let row = format_row(200, &r, 12, None, false);
        assert_eq!(row.find(" -  ").map(|p| p + 2), Some(t + "TOKENS".len()), "absent reads as a dash:\n{header}\n{row}");
    }

    /// `--since` keeps the rows active at or after the bound (the same
    /// activity stamp the table sorts by), in the order they came.
    #[test]
    fn filter_since_keeps_rows_active_at_or_after_the_bound() {
        let rows = vec![
            mk_run("old", RunKind::Mission, RunStatus::Complete, 99),
            mk_run("at", RunKind::Mission, RunStatus::Complete, 100),
            mk_run("new", RunKind::Dispatch, RunStatus::Running, 150),
        ];
        let kept: Vec<String> = filter_since(rows.clone(), Some(100)).into_iter().map(|r| r.id).collect();
        assert_eq!(kept, vec!["at", "new"]);
        assert_eq!(filter_since(rows, None).len(), 3, "no bound keeps everything");
    }

    fn split(calls: u64, total: u64, input: u64, cached: Option<u64>, generated: u64) -> UsageSplit {
        UsageSplit { calls, total, input, cached, generated, unreported: 0 }
    }

    fn sample_report() -> UsageReport {
        UsageReport {
            since: "2026-09-12T00:00:00Z".to_string(),
            default_window: true,
            breakdown: UsageBreakdown {
                // The sum of the four groups below, field by field.
                overall: darkmux_serve::usage_sum::UsageSum {
                    total: 2045,
                    prompt: 1755,
                    completion: 290,
                    cached: Some(140),
                    utility: 145,
                    usage_records: 9,
                    reported: 9,
                },
                no_run: split(2, 110, 95, None, 15),
                unlisted: split(0, 0, 0, None, 0),
                groups: vec![
                    UsageGroup {
                        machine: Some("laptop".into()),
                        endpoint_id: None,
                        endpoint: Some("http://127.0.0.1:1234/v1".into()),
                        requested_model: Some("qwen-a".into()),
                        reported_model: None,
                        work: split(3, 360, 300, Some(40), 60),
                        utility: split(0, 0, 0, None, 0),
                    },
                    UsageGroup {
                        machine: Some("laptop".into()),
                        endpoint_id: Some("azure".into()),
                        endpoint: Some("azure:example.azure.com/gpt-x".into()),
                        requested_model: Some("gpt-x".into()),
                        reported_model: Some("gpt-x-2026-01".into()),
                        work: split(1, 500, 400, Some(100), 100),
                        utility: split(1, 35, 30, None, 5),
                    },
                    UsageGroup {
                        machine: Some("studio".into()),
                        endpoint_id: None,
                        endpoint: Some("http://127.0.0.1:1234/v1".into()),
                        requested_model: Some("util-4b".into()),
                        reported_model: None,
                        work: split(0, 0, 0, None, 0),
                        utility: split(2, 110, 95, None, 15),
                    },
                    UsageGroup {
                        machine: None,
                        endpoint_id: None,
                        endpoint: None,
                        requested_model: None,
                        reported_model: None,
                        work: split(2, 1040, 930, None, 110),
                        utility: split(0, 0, 0, None, 0),
                    },
                ],
            },
        }
    }

    /// One line per machine + endpoint + requested model with the group's ALL
    /// tokens; a `utility` line under it when darkmux's own calls landed
    /// there; the served model as a fact under the line when it differs;
    /// `-` for cached wherever nothing reported it; and the totals.
    #[test]
    fn usage_lines_render_each_endpoint_model_with_utility_split_and_reported_model_as_a_fact() {
        let lines = usage_lines(&sample_report(), None);
        let text = lines.join("\n");
        assert!(lines[0].starts_with("usage since 2026-09-12T00:00:00Z"), "{text}");
        assert!(lines[0].contains("9 calls") && !lines[0].contains("legacy"), "{text}");
        assert!(text.contains("2 calls on no run (110 tokens)"), "{text}");
        let header = &lines[1];
        for col in ["MACHINE", "ENDPOINT", "MODEL", "CALLS", "INPUT", "CACHED", "GENERATED", "TOTAL"] {
            assert!(header.contains(col), "{col} missing from {header:?}");
        }
        // Every group line carries its endpoint, model and ALL-tokens total.
        let qwen = lines.iter().find(|l| l.contains("qwen-a")).expect("qwen-a line");
        assert!(qwen.contains("laptop") && qwen.contains("http://127.0.0.1:1234/v1") && qwen.ends_with("360"), "{qwen:?}");
        // A named endpoint reads as its registry id, never the raw string; the
        // same URL on another machine is its own line (#3067).
        let gpt_line = lines.iter().find(|l| l.contains("gpt-x ")).expect("gpt-x line");
        assert!(gpt_line.contains("azure ") && !gpt_line.contains("example.azure.com"), "{gpt_line:?}");
        let util_line = lines.iter().find(|l| l.contains("util-4b")).expect("util-4b line");
        assert!(util_line.contains("studio") && util_line.contains("http://127.0.0.1:1234/v1"), "{util_line:?}");
        assert!(qwen.contains(" 40 "), "cached reported: {qwen:?}");
        let gpt = lines.iter().position(|l| l.contains("gpt-x ") || l.ends_with("gpt-x")).expect("gpt-x line");
        assert!(lines[gpt].ends_with("535"), "work + utility: {:?}", lines[gpt]);
        assert!(lines[gpt + 1].trim_start().starts_with("utility") && lines[gpt + 1].ends_with("35"), "{:?}", lines[gpt + 1]);
        assert!(lines[gpt + 2].contains("reported model: gpt-x-2026-01"), "{:?}", lines[gpt + 2]);
        assert_eq!(text.matches("reported model:").count(), 1, "only the differing group says so");
        // A utility-only group is one line with its total plus the split.
        let util = lines.iter().position(|l| l.contains("util-4b")).expect("util-4b line");
        assert!(lines[util].ends_with("110") && lines[util + 1].trim_start().starts_with("utility"), "{text}");
        // Cached absent renders `-`, never 0.
        assert!(lines[util].contains(" - "), "{:?}", lines[util]);
        // Records with no endpoint/model are still shown, labeled.
        assert!(text.contains("(none)"), "{text}");
        // Totals: all tokens, and utility's share.
        let all = lines.iter().find(|l| l.trim_start().starts_with("all")).expect("all line");
        assert!(all.contains("1,755") && all.contains("140") && all.contains("290") && all.ends_with("2,045"), "{all:?}");
        let util_total = lines.iter().rev().find(|l| l.trim_start().starts_with("utility")).unwrap();
        assert!(util_total.ends_with("145"), "{util_total:?}");
        // Nothing here prices, classifies or estimates anything.
        for word in ["$", "cost", "local", "cloud", "metered", "saved"] {
            assert!(!text.to_lowercase().contains(word), "{word:?} in {text}");
        }
    }

    /// (#3067) Calls on a run with no row print their own line, only when there are some.
    #[test]
    fn usage_lines_name_calls_on_a_run_with_no_row_only_when_there_are_some() {
        let mut report = sample_report();
        assert!(!usage_lines(&report, None).join("\n").contains("no row here"));
        report.breakdown.unlisted = split(1, 50, 40, None, 10);
        let text = usage_lines(&report, None).join("\n");
        assert!(text.contains("1 call on a run with no row here (50 tokens)"), "{text}");
    }

    /// (#3067) A call that reported no usage is named under the totals, so a
    /// short total is not read as a complete one.
    #[test]
    fn usage_lines_name_calls_that_reported_no_usage() {
        let mut report = sample_report();
        report.breakdown.groups[0].work.unreported = 2;
        let text = usage_lines(&report, None).join("\n");
        assert!(text.contains("2 calls reported no usage: counted in CALLS, 0 tokens each"), "{text}");
        assert!(!usage_lines(&sample_report(), None).join("\n").contains("reported no usage"));
    }

    /// (review CONSIDER 5) `--since` alone still names its bound in the
    /// JSON, and the empty state names it in text (CONSIDER 6).
    #[test]
    fn since_without_usage_still_reports_its_bound() {
        let rows = vec![mk_run("m1", RunKind::Mission, RunStatus::Complete, 100)];
        let fleet = darkmux_serve::source_state::SourceState::Off;
        let payload = serde_json::to_value(json_payload(&rows, RunKindArg::All, &fleet, Some("2026-09-12T00:00:00Z"), None)).unwrap();
        assert_eq!(payload["since"], "2026-09-12T00:00:00Z");
        assert!(payload.get("usage").is_none());
        assert_eq!(empty_state_line(RunKindArg::All, None), "no recorded run activity yet");
        assert_eq!(empty_state_line(RunKindArg::Lab, None), "no recorded lab runs yet");
        assert_eq!(empty_state_line(RunKindArg::All, Some("2026-09-12T00:00:00Z")), "no run activity since 2026-09-12T00:00:00Z");
        assert_eq!(empty_state_line(RunKindArg::Lab, Some("2026-09-12T00:00:00Z")), "no lab runs since 2026-09-12T00:00:00Z");
    }

    /// The same breakdown, structurally, plus the bound it covers.
    #[test]
    fn usage_json_carries_the_same_breakdown_structurally() {
        let rows = vec![mk_run("m1", RunKind::Mission, RunStatus::Complete, 100)];
        let fleet = darkmux_serve::source_state::SourceState::Off;
        let report = sample_report();
        let payload = serde_json::to_value(json_payload(&rows, RunKindArg::All, &fleet, Some(&report.since), Some(&report))).unwrap();
        assert_eq!(payload["usage"]["since"], "2026-09-12T00:00:00Z");
        assert_eq!(payload["usage"]["default_window"], true);
        assert_eq!(payload["usage"]["overall"]["total"], 2045);
        assert_eq!(payload["usage"]["overall"]["cached"], 140);
        let groups = payload["usage"]["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 4);
        assert_eq!(groups[1]["reported_model"], "gpt-x-2026-01");
        assert_eq!(groups[1]["work"]["cached"], 100);
        assert_eq!((groups[1]["machine"].as_str(), groups[1]["endpoint_id"].as_str()), (Some("laptop"), Some("azure")), "{}", groups[1]);
        assert!(groups[3].get("machine").is_none(), "absent on the wire: {}", groups[3]);
        assert!(groups[1]["utility"].get("cached").is_none(), "absent, never 0: {}", groups[1]);
        assert!(groups[3].get("endpoint").is_none(), "absent on the wire: {}", groups[3]);
        assert_eq!(payload["since"], "2026-09-12T00:00:00Z");
        // Without --usage there is no usage key at all, and rows still
        // carry their own `tokens`.
        let plain = serde_json::to_value(json_payload(&rows, RunKindArg::All, &fleet, None, None)).unwrap();
        assert!(plain.get("usage").is_none() && plain.get("since").is_none());
        assert_eq!(plain["runs"][0]["id"], "m1");
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
            "RUNS_KINDS not found in ui/src/lib/route.ts: the twin this test pins against \
             was renamed or removed; darkmux run list --kind and the RUNS lens's kind chips \
             can now drift apart silently (#1905)",
        );
        let (body, _) = after.split_once(']').expect(
            "RUNS_KINDS has no closing `]` in ui/src/lib/route.ts: the twin this test pins \
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
             ui/src/lib/route.ts's RUNS_KINDS: update BOTH twins together (#1905), the pill \
             row and the CLI flag must show the same vocabulary"
        );

        // (#1911) The THIRD leg. `panel.rs`'s `RUN_LIST_KIND_OPT` declares
        // the same four values for the console's `--kind` option, and the
        // panel spawns whatever that table says. Without this leg, dropping
        // `Lab` from `RunKindArg` keeps the two legs above agreeing while
        // the panel goes on spawning `run list --kind lab` — clap rejects
        // it, and the operator sees an empty body with `exit_code: 2`, a
        // wrong reading with no failing test anywhere. The reverse (a fifth
        // kind) silently makes it unreachable from the console.
        //
        // Text-scanned for the same reason the TS leg is: `panel.rs`'s
        // module is private to `darkmux-serve`, so there is no binding to
        // compare against, and a scan that cannot find its anchor fails
        // loudly rather than passing empty.
        let panel_rs = include_str!("../crates/darkmux-serve/src/panel.rs");
        let (_, after) = panel_rs.split_once("const RUN_LIST_KIND_OPT: PanelOpt = PanelOpt {").expect(
            "RUN_LIST_KIND_OPT not found in crates/darkmux-serve/src/panel.rs: the console \
             option table this pins against was renamed or removed, so `darkmux run list \
             --kind` and the panel that spawns it can now drift apart silently (#1911)",
        );
        let (body, _) = after.split_once("};").expect(
            "RUN_LIST_KIND_OPT has no closing `};` in panel.rs: the twin changed shape (#1911)",
        );
        let mut panel_kinds: Vec<String> = body
            .match_indices("value: \"")
            .filter_map(|(i, _)| {
                let rest = &body[i + "value: \"".len()..];
                rest.split_once('"').map(|(v, _)| v.to_string())
            })
            .collect();
        panel_kinds.sort();
        assert_eq!(
            rust_kinds, panel_kinds,
            "darkmux run list --kind's accepted values drifted from the console panel's own \
             RUN_LIST_KIND_OPT table (crates/darkmux-serve/src/panel.rs): the panel would \
             spawn a flag the CLI no longer accepts, or hide one it does (#1911)"
        );
    }
}
