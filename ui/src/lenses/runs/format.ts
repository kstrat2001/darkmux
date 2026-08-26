/**
 * Pure port of the runs-lens formatting/grouping logic in `viewer.html`
 * (the `── the runs lens ──` and `── Lab observer lens ──` sections). Kept
 * as standalone functions — not component-local — so they're independently
 * unit-testable against the same inputs/outputs the legacy `<script>` block
 * produces, and so `RunsBoard.tsx` reads as "wire data in, JSX out" rather
 * than re-deriving this logic inline.
 *
 * Every function name and behavior below is a DELIBERATE 1:1 match to its
 * `viewer.html` namesake (see that file's own comments for the "why", not
 * repeated here) — this is a port, not a redesign. `RUNS_KINDS`/`RunsKind`
 * live in `../../lib/route.ts` already (the scaffold's hash-grammar port);
 * imported from there rather than redeclared.
 *
 * The lab-series six (`shortModel`/`labFieldVal`/`labTaskKey`/
 * `groupLabRunsByTask`/`labKnobSummary`/`labKnobDiff`) used to be
 * hand-duplicated here — this file predates `../lab/labSeries.ts`, the lab
 * lens's own dedicated pure-logic module, extracted+differentially-tested
 * against the legacy viewer later. The two copies had already drifted:
 * this file's `labFieldVal` stringified every non-`darkmux:`-prefixed value
 * (`String(v)`), where legacy (and `labSeries.ts`) pass a number straight
 * through unchanged — invisible in the rendered DOM (template-literal
 * interpolation stringifies either way) but a real behavioral difference a
 * caller comparing the return value directly would see. Re-exported from
 * `labSeries.ts` below instead of maintained twice; only `labCounts` (not
 * one of the "six" — no `labSeries.ts` counterpart) stays defined here.
 */

import type { Run } from "../../types/generated/Run";
import type { LabRun } from "../../types/handwritten";
import {
  shortModel,
  labFieldVal,
  labTaskKey,
  groupLabRunsByTask,
  labKnobSummary,
  labKnobDiff,
} from "../lab/labSeries";
import type { LabSeries } from "../lab/types";

export { shortModel, labFieldVal, labTaskKey, groupLabRunsByTask, labKnobSummary, labKnobDiff };
/** Same shape as `../lab/labSeries.ts`'s `LabSeries` — kept under this
 * file's pre-existing name so `RunsBoard.tsx`'s import site doesn't churn. */
export type LabTaskGroup = LabSeries;

export const RUNS_CAP = 25; // viewer.html: `const RUNS_CAP=25`

/** viewer.html: `function runActivity(r)`. STRICTLY newest-activity-first
 * ordering — see that function's own comment for why `running` rows are
 * never hoisted above their actual last-activity time. */
export function runActivity(r: Run): number {
  return r.updated_ts || r.completed_ts || r.started_ts || 0;
}

/** viewer.html: `function runsAgo(r)`, minus the `runsIsPlayback()` branch —
 * this port has no playback/`daemon-mode-play` concept (the scaffold's
 * `/next` route is always "live"), so only the relative-time branch applies.
 * `now` defaults to `Date.now()` but is threaded as a parameter so a test
 * (or a future frozen-clock caller) doesn't have to mock the global clock. */
export function runsAgo(r: Run, now: number = Date.now()): string {
  const ts = runActivity(r);
  if (!ts) return "";
  const secs = Math.max(0, Math.floor(now / 1000) - ts);
  if (secs < 60) return `${secs}s ago`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h ago`;
  return `${Math.floor(secs / 86400)}d ago`;
}

/**
 * (#1907) The status BADGE's display text. `RunStatus::Abandoned` alone
 * covers two genuinely different situations — a human ran `mission abort`,
 * or nothing ever wrote an ending (killed/crashed/no terminal record) —
 * and reading the same word for both is what prompted "i'm not sure what
 * abandoned means?" on the console's own activity list (this function's
 * origin issue). `Run.abandoned_reason` is set ONLY alongside `"abandoned"`
 * (`crates/darkmux-serve/src/runs.rs::Run::abandoned_reason`'s own doc), so
 * this reads it directly rather than re-deriving the distinction client-
 * side. Every other status keeps its own plain name, unchanged — this is
 * the ONE place `RunRow`'s badge text can diverge from `run.status` itself
 * (the CSS class backing the badge's COLOR stays keyed on `run.status`
 * verbatim, so `.labbadge.abandoned` styling is untouched by this).
 */
export function runStatusLabel(r: Run): string {
  if (r.status !== "abandoned") return r.status;
  return r.abandoned_reason === "aborted" ? "aborted" : "no ending recorded";
}

/** viewer.html: `function runSubtitle(r, showMachine)`. */
export function runSubtitle(r: Run, showMachine: boolean): string {
  const bits: string[] = [];
  if (r.role) bits.push(r.role);
  if (r.model) bits.push(shortModel(r.model));
  if (r.route) bits.push(`via ${r.route}`);
  if (showMachine && r.machine) bits.push(r.machine);
  return bits.join(" · ");
}

/** viewer.html: `function runsMultiMachine()`, generalized to take the runs
 * array as a parameter (legacy reads the module-global `RUNS`). */
export function runsMultiMachine(runs: Run[]): boolean {
  return new Set(runs.map((r) => r.machine).filter((m): m is string => Boolean(m))).size > 1;
}

/** viewer.html: `function runsFiltered()`, parameterized over `runs`/`kind`
 * rather than reading `state.runsKind`/`RUNS` off module globals. */
export function runsFiltered(runs: Run[], kind: string): Run[] {
  const rows = kind === "all" ? runs.slice() : runs.filter((r) => r.kind === kind);
  rows.sort((a, b) => runActivity(b) - runActivity(a));
  return rows;
}

/** viewer.html: `function labCounts(run)` — text only (no HTML), the
 * "degenerate" flag is a boolean the component renders as its own element
 * rather than an inline HTML fragment (see `RunsBoard.tsx`). */
export function labCounts(run: LabRun): string {
  return `bundles ${run.bundles} · flags ${run.raw_flags}→${run.deduped_flags} · confirmed ${run.confirmed} · needs_check ${run.needs_check} · archived ${run.archived}`;
}

/**
 * (#1809, #1508 step 4) Filter a runs list down to ONE pinned machine — the
 * runs-lens half of the machine dimension legacy never had. Unlike every
 * other export in this file (see the module doc's opening paragraph), this
 * one has no `viewer.html` namesake; it is new.
 *
 * `Run.machine` (`crates/darkmux-serve/src/runs.rs::build_runs`) is a
 * `machine_id` NAME, not a uid — and one machine can carry SEVERAL names
 * over its lifetime (a laptop logging as both `MacBook-Pro` and
 * `MacBook-Pro.local` — see `lib/flow.ts::machineNames`'s own doc for why).
 * Matching the route's pinned uid against `Run.machine` by resolving ONE
 * label (`nameOf`) and comparing strings would silently drop every row
 * filed under an older alias: measured against the live daemon's real
 * `/runs` (380 rows), that approach returned ZERO rows for the very machine
 * the page was pinned to, because every row said `MacBook-Pro` while
 * `nameOf` resolved the uid to `MacBook-Pro.local`. `names` must be the
 * FULL alias set (`machineNames(...)`), never a single resolved label.
 *
 * A run with NO `machine` at all is excluded from every pin. That set is
 * missions and dispatches only — every lab run carries a machine, because
 * `lab_summary_to_run` takes the daemon's own `machine_id` directly instead
 * of deriving it — and every one of them is `tracked: true`, so this is not
 * ghost noise. The mechanism, rather than a count that rots: `/runs`
 * resolves a mission's machine from the WINDOWED flow session index
 * (`RUNS_FLOW_SCAN_WINDOW_DAYS`, 14 days), and the durable `mission.json`
 * has no machine field to fall back on, so any tracked run older than that
 * window loses its attribution even though the flow records are still on
 * disk. Filed as #1810.
 *
 * Excluding them is the honest call — claiming an unattributed row as "this
 * machine" would be the worse lie — but it is worth naming so nobody
 * re-derives "where did the missing rows go" from scratch. It is a
 * meaningful fraction: at the time of writing, 82 of 380.
 *
 * KNOWN LIMIT, not a defect of this function: `Run` carries no
 * `machine_uid`, only the name, so two uids that have EVER logged the same
 * `machine_id` are indistinguishable here — two Macs on Apple's default
 * hostname, or a rename that hands a name from one host to another. Both
 * alias sets would contain the shared name and both pins would return the
 * union, under a chip naming one of them. Disjoint on any fleet where
 * machine names are distinct (verified on the operator's: `{MacBook-Pro,
 * MacBook-Pro.local}` vs `{m1-max-32gb-studio}`). The real fix is a
 * `machine_uid` on `Run`, an #1810 sibling — this is the first surface that
 * claims per-machine scoping over name-keyed data, so the collision is
 * named here rather than discovered later.
 */
export function runsForMachine(runs: Run[], names: Set<string>): Run[] {
  if (names.size === 0) return [];
  return runs.filter((r) => r.machine != null && names.has(r.machine));
}

/**
 * (#1904 QA fix) A run's click destination, extracted from `RunsBoard.tsx`'s
 * `activateRun` so the four-branch decision lives in exactly one place. It
 * used to also serve `ActivityPanel.tsx`'s own `activityRunActivate` — a
 * SECOND hand-rolled copy of the same decision, whose independent drift
 * (dropping the `unreachable` branch silently: a tracked mission/dispatch
 * row rendered clickable via `RunRow`'s own `interactive` gate, but
 * clicking it when `missionGraphReachable()` is false did nothing at all
 * — the exact #1900 failure class) is why this got pulled out at all.
 * `ActivityPanel.tsx` is deleted (#1905 step 3 — `run-list`, a real CLI
 * panel over the same `/runs` union, supersedes it), leaving `RunsBoard`
 * as this function's one remaining caller; the shared shape stays because
 * the decision itself — and the drift risk a second caller could someday
 * reintroduce — is unchanged by having only one caller today.
 *
 * The LAB branch is deliberately NOT resolved to a navigation here —
 * `RunsBoard` swaps to an in-page detail pane for a lab run (`openLabRun`,
 * a `labRunDir` state change plus a hash write) rather than navigating
 * anywhere. Returning the run's `id` (the lab run's directory) lets the
 * caller build its own destination from it, rather than this function
 * picking a navigation mechanism the caller doesn't use. */
export type RunDestination =
  | { kind: "lab"; dir: string }
  | { kind: "hash"; hash: string }
  /** A tracked mission (its own mission graph exists), but this page has
   * no live daemon behind it to fetch `/mission/<id>/graph.json` from
   * (the daemon-less static demo build). The row is still INTERACTIVE —
   * clicking it is a real, expected action — it just can't navigate
   * anywhere useful; callers show `MISSION_GRAPH_UNREACHABLE_NOTICE`
   * instead of silently doing nothing. */
  | { kind: "unreachable" }
  /** An untracked row with no representative session to drill into either
   * — genuinely nothing to do here. Callers render the row
   * non-interactive, matching `RunRow`'s own `interactive` gate ("has a
   * destination" — see that component's own doc), which already excludes
   * exactly this case. Rare in practice (every untracked row this build
   * has actually produced carries a `session_id`, ghost or mission
   * alike), but not impossible — a mission this daemon knows about
   * ONLY through a terminal record, with no dispatch session ever
   * joined to it, is the honest shape that reaches here. */
  | { kind: "none" };

/**
 * (#1915) Untracked no longer means inert. #1900/#1902 widened this ONLY
 * for `kind === "dispatch"`, because a dispatch row's `id` happens to BE
 * its own session id — but `tracked` was never actually the right test;
 * "does this row carry a session it can be drilled into" is. The server
 * now carries that pick explicitly (`Run.session_id` —
 * `crates/darkmux-serve/src/runs.rs`'s `mission_to_run`/
 * `flow_mission_to_run`/`ghost_runs`, all resolving it the SAME
 * representative-session rule already used for role/model/route), so ANY
 * untracked row with one — a ghost dispatch (whose `session_id` equals
 * its own `id`) or an untracked mission (a peer's, #1705, or a local
 * ephemeral with no durable record) — drills the same way. On the
 * reported machine this was 40 of 104 mission rows, the entire newest
 * page a person actually sees (the board sorts newest-first).
 *
 * A tracked mission cannot use this shortcut even when it also carries a
 * `session_id` (`mission_to_run` populates it uniformly — see that
 * field's own doc): `/mission/<id>/graph.json` is served from THIS
 * machine's own durable state, which a tracked row by definition has, so
 * the richer mission GRAPH is the right destination, not a session. An
 * UNTRACKED mission structurally cannot make that same claim — there is
 * no local `Mission`/`Phase`/`Task`/`Step` record for it, on this machine
 * or (for a peer's mission) on any machine this daemon can query — so its
 * session is the best this view can ever offer, not a fallback pending a
 * richer one.
 */
export function runDestination(run: Run, graphReachable: boolean): RunDestination {
  if (run.kind === "lab") return { kind: "lab", dir: run.id };
  // (#1973) A DISPATCH-kind run drills to the DETAIL view, tracked or not.
  //
  // Previously only UNTRACKED rows came here; a tracked `darkmux dispatch`
  // mints a crew-of-one mission, so it fell through to `#mission=` and opened
  // the graph. That graph is a single node with no click handler — it showed
  // strictly less than the detail view and then dead-ended, so the path
  // "Runs -> Dispatch -> detail" did not exist for the rows most likely to be
  // clicked. The only way here was a hand-typed URL or a fleet activity bar.
  //
  // Gated on `session_id` because that is the key this route addresses (a
  // dispatch-kind run is named for its content but keyed by its session — see
  // `CLAUDE.md` contract 8). A run whose flow records have aged out of the
  // window carries none, and falls through to the graph rather than offering
  // a link to nothing.
  if (run.kind === "dispatch" && run.session_id) {
    return { kind: "hash", hash: `dispatch=${encodeURIComponent(run.session_id)}` };
  }
  if (!run.tracked) {
    // No `graphReachable` gate here — `/flow-session/<id>` is a plain
    // daemon fetch (`SessionReplay`'s own fetch, same as the ungated
    // `#session=<sid>` bars `FleetLens.tsx`'s activity timeline already
    // navigates to), not the mission-graph lens's endpoint.
    if (run.session_id) return { kind: "hash", hash: `dispatch=${encodeURIComponent(run.session_id)}` };
    return { kind: "none" };
  }
  if (!graphReachable) return { kind: "unreachable" };
  return { kind: "hash", hash: `mission=${encodeURIComponent(run.id)}` };
}

/** The notice used to point at "the classic viewer at /" — `viewer.html`
 * was deleted in #1865 and `/` now serves THIS SAME app, so a daemon-less
 * visitor was being told to go to the page they were already on. There is
 * genuinely nowhere else to send them (a static build has no daemon to
 * reach, full stop), so the fix names the missing capability instead of a
 * bogus destination — matching `RunsBoard.tsx`'s own `onLabRunUnresolvable`
 * `"run detail needs a running daemon — …"` phrasing. Shared (not
 * redeclared per caller) so the two surfaces that can hit `runDestination`'s
 * `unreachable` branch say the same thing. */
export const MISSION_GRAPH_UNREACHABLE_NOTICE =
  "mission graph needs a running daemon behind this page — this static build has no mission graph data to show.";
