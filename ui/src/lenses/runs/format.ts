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
 * A run with NO `machine` at all is excluded from every pin — measured on
 * the live daemon, that is 50 missions + 15 dispatches (every one
 * `tracked: true`; every lab run DOES carry a machine, so this is not
 * lab-run noise). That is a real attribution gap upstream (the mission/
 * dispatch machine field was never recorded for those rows), not phantom
 * work — a reader who notices fewer rows under a pin should read that as
 * "unattributed", not "destroyed". Claiming an unattributed row as "this
 * machine" would be the worse lie; excluding it is the honest call, but it
 * is worth naming so nobody re-derives "where did 65 rows go" from scratch.
 */
export function runsForMachine(runs: Run[], names: Set<string>): Run[] {
  if (names.size === 0) return [];
  return runs.filter((r) => r.machine != null && names.has(r.machine));
}
