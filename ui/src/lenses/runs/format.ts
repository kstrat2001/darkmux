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
 * (#1904 QA fix) A run's click destination, shared by every caller that
 * opens a `Run` row — `RunsBoard.tsx`'s `activateRun` and
 * `ActivityPanel.tsx`'s `activityRunActivate` used to hand-roll the SAME
 * four-branch decision independently. That duplication is exactly what
 * let them drift: `ActivityPanel`'s copy dropped the `unreachable` branch
 * silently (a tracked mission/dispatch row rendered clickable via
 * `RunRow`'s own `interactive` gate, but clicking it when
 * `missionGraphReachable()` is false did nothing at all — a dead
 * affordance, the exact #1900 failure class both callers' own doc
 * comments invoke). One function, one place the rule lives.
 *
 * The LAB branch is deliberately NOT resolved to a navigation here —
 * `RunsBoard` swaps to an in-page detail pane for a lab run (`openLabRun`,
 * a `labRunDir` state change plus a hash write), while `ActivityPanel`
 * isn't the runs lens and has no such state, so it navigates there
 * instead (`lens=runs&kind=lab&run=<dir>`). Returning the run's `id` (the
 * lab run's directory) lets each caller build its own destination from
 * it, rather than this function picking one caller's mechanism as
 * "correct" for both. */
export type RunDestination =
  | { kind: "lab"; dir: string }
  | { kind: "hash"; hash: string }
  /** A tracked mission or dispatch (its own mission graph exists), but this
   * page has no live daemon behind it to fetch `/mission/<id>/graph.json`
   * from (the daemon-less static demo build). The row is still
   * INTERACTIVE — clicking it is a real, expected action — it just can't
   * navigate anywhere useful; callers show `MISSION_GRAPH_UNREACHABLE_NOTICE`
   * instead of silently doing nothing. */
  | { kind: "unreachable" }
  /** An untracked mission (a peer's mission this daemon only knows via the
   * fleet stream, #1705): its `id` is a mission id, not a session id, so
   * there is no local record to open. Genuinely nothing to do here —
   * callers render the row non-interactive, matching `RunRow`'s own
   * `interactive` gate (`run.kind === "lab" || run.tracked || run.kind ===
   * "dispatch"`), which already excludes exactly this case. */
  | { kind: "none" };

export function runDestination(run: Run, graphReachable: boolean): RunDestination {
  if (run.kind === "lab") return { kind: "lab", dir: run.id };
  if (run.kind === "dispatch" && !run.tracked) {
    // No `graphReachable` gate here — `/flow-session/<id>` is a plain
    // daemon fetch (`SessionReplay`'s own fetch, same as the ungated
    // `#session=<sid>` bars `FleetLens.tsx`'s activity timeline already
    // navigates to), not the mission-graph lens's endpoint.
    return { kind: "hash", hash: `session=${encodeURIComponent(run.id)}` };
  }
  if (!run.tracked) return { kind: "none" };
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
