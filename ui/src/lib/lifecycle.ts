/**
 * A run's lifecycle as of an instant: the viewer's one set of rules for
 * "has it started, is it running, waiting, finished, or gone quiet".
 *
 * Every surface asks `lifecycleAt` (the fleet card, the activity timeline,
 * the run page's pill and clock, the mission graph's step meter, playback's
 * focus range, the live token scope), so the same run reads the same phase
 * on each of them at the same moment. Live is `asOf = now`; playback is
 * `asOf = the playhead`. Presence is an extra input that can only ADD:
 * it holds a run open against the staleness clock, never against a record
 * that closed it.
 *
 * The rules:
 *
 * 1. Attempts. A run's records segment into attempts (`attemptsOf`). An
 *    attempt opens on its first opening record (a `dispatch.start`, a
 *    `budget.wait`, a `mission.start`, a `step.start`, or, when nothing
 *    opened yet, any turn, heartbeat, tool call or rest). A `dispatch.start`
 *    in an attempt that already has one, or any reopening record after the
 *    attempt closed, starts the next attempt (a relaunch under the same id).
 *    The attempt current as of t is the latest one opened by t.
 * 2. Close. An attempt closes on its earliest closing record: a dispatch or
 *    step terminal, `session.end`, `budget.stop`, `mission.close` or
 *    `mission.abort`. A closing record timestamped before anything opened
 *    (clock skew across machines, #1988) closes the first attempt left with
 *    no close of its own, and is marked `skewed`. A record with an
 *    unparsable `ts` is inside every as-of cut, so an untimed terminal still
 *    closes its run.
 * 3. Outcome. How it ended comes from the attempt's dispatch terminal when
 *    it has one (a `session.end` that lands first does not erase a clean
 *    `dispatch.complete`), else from the closing record itself.
 * 4. Waiting. A `budget.wait` with no `budget.resume` or closing record
 *    after it holds the run `waiting` until its announced resume time plus
 *    `budgetWaitGraceMs`; past that the staleness clock runs from there.
 * 5. Stale. An open attempt whose last record is more than `staleAfterMs`
 *    before t has stopped with no ending recorded. So has an attempt a later
 *    one superseded.
 * 6. A mission's whole-run bookend (`runRef.ts`'s `run` grain) never beats
 *    itself; its steps do. Its activity and its waits are its mission's
 *    other runs' too, so it is in flight while any of them is.
 */

import { ACTION, byTime, isAsOf, isDispatchTerminal, latestByTime, recordsAsOf, timesOf, type NormAction, type NormRecord } from "./ingest";
import type { RunState } from "./flow";
import type { RunGroup, RunRecords } from "./runRef";
import type { RunsPolicy } from "../types/generated/RunsPolicy";

export type LifecyclePhase = "not_started" | "open" | "waiting" | "closed" | "stale";

/** How a closed attempt ended. */
export type CloseEdge =
  | { readonly kind: "complete" }
  | { readonly kind: "error"; readonly killed: boolean; readonly exitCode: number | null }
  | { readonly kind: "session_end" }
  /** A budget wait ended because its run was stopped; nothing was sent.
   *  `byOperator` when the record names why (every reason a producer writes
   *  is an operator's stop: an interrupt, `mission abort`/`finalize`, an
   *  abandoned phase). */
  | { readonly kind: "budget_stop"; readonly byOperator: boolean }
  | { readonly kind: "mission_abort" };

export interface LifecyclePolicy {
  /** How long an open run may go silent before it reads as stopped. The
   *  daemon's own rule (twice the runtime's inactivity budget). */
  readonly staleAfterMs: number;
  /** How long past its announced resume time a budget wait stays open with
   *  no further word from its waiter. */
  readonly budgetWaitGraceMs: number;
}

/** The daemon's defaults (twice the runtime's default 600 s inactivity
 *  budget; one minute of grace), for a page that has not read `/runs` yet
 *  or one served without it. */
export const DEFAULT_POLICY: LifecyclePolicy = { staleAfterMs: 1_200_000, budgetWaitGraceMs: 60_000 };

/** The policy `/runs` publishes (`RunsPolicy`), else the default: a daemon
 *  from before it published one answers without it. */
export function policyOf(p: RunsPolicy | undefined): LifecyclePolicy {
  return p ? { staleAfterMs: p.stale_after_ms, budgetWaitGraceMs: p.budget_wait_grace_ms } : DEFAULT_POLICY;
}

export interface Close {
  readonly edge: CloseEdge;
  /** When it closed (the earliest closing record); `null` when that record's
   *  `ts` does not parse. */
  readonly atMs: number | null;
  /** The close was read from a record timestamped before the run opened. */
  readonly skewed: boolean;
  /** The record the outcome was read from (its payload carries `wall_ms`,
   *  `exit_code`, the endpoint and the tokens). */
  readonly record: NormRecord;
}

export interface Lifecycle {
  readonly phase: LifecyclePhase;
  /** The attempt's start: its `dispatch.start`'s time, else its opening
   *  record's, else its earliest timed record's. */
  readonly startMs: number | null;
  /** The attempt's latest timed record as of t. */
  readonly lastActivityMs: number | null;
  readonly close: Close | null;
  /** While `waiting`: when the wait lapses. */
  readonly waitUntilMs: number | null;
}

/** Session ids presence reports live. */
export type Presence = ReadonlySet<string>;
export const NO_PRESENCE: Presence = new Set<string>();

/** One attempt of a run (rule 1). */
export interface Attempt {
  /** The record that opened it. */
  readonly opening: NormRecord;
  /** Its `dispatch.start`, when it has one (the brief's payload). */
  readonly start: NormRecord | null;
  /** Its records, time order: from its opening (for the first attempt,
   *  from the run's first record) to the next attempt's opening. */
  readonly records: readonly NormRecord[];
  /** Its earliest closing record. */
  readonly close: NormRecord | null;
  readonly skewed: boolean;
}

/** The records that open an attempt even after an earlier one closed. */
const REOPENERS: ReadonlySet<NormAction> = new Set<NormAction>([
  ACTION.DispatchStart,
  ACTION.BudgetWait,
  ACTION.MissionStart,
  ACTION.StepStart,
]);

/** Records that open an attempt when none is open yet: proof the work ran,
 *  when its start is outside the window. */
const FIRST_OPENERS: ReadonlySet<NormAction> = new Set<NormAction>([
  ACTION.DispatchTurn,
  ACTION.DispatchTurnHeartbeat,
  ACTION.DispatchTool,
  ACTION.DispatchRest,
]);

const exitCodeOf = (r: NormRecord): number | null => {
  const c = (r.payload as { exit_code?: unknown } | undefined)?.exit_code;
  return typeof c === "number" ? c : null;
};

const hasReason = (r: NormRecord): boolean => {
  const p = (r.payload ?? r.fields) as { reason?: unknown } | undefined;
  return typeof p?.reason === "string" && p.reason.length > 0;
};

/** The edge a closing record implies; `null` for any other record. */
export function closeEdgeOf(r: NormRecord): CloseEdge | null {
  switch (r.action) {
    case ACTION.DispatchComplete:
    case ACTION.StepComplete:
    case ACTION.MissionClose:
      return { kind: "complete" };
    case ACTION.DispatchError:
      return { kind: "error", killed: exitCodeOf(r) === 137, exitCode: exitCodeOf(r) };
    case ACTION.StepError:
      return { kind: "error", killed: false, exitCode: null };
    case ACTION.SessionEnd:
      return { kind: "session_end" };
    case ACTION.BudgetStop:
      return { kind: "budget_stop", byOperator: hasReason(r) };
    case ACTION.MissionAbort:
      return { kind: "mission_abort" };
    default:
      return null;
  }
}

const isClosing = (r: NormRecord): boolean => closeEdgeOf(r) !== null;

interface Building {
  opening: NormRecord;
  start: NormRecord | null;
  records: NormRecord[];
  close: NormRecord | null;
  skewed: boolean;
}

/** Whether `r` opens a new attempt after `cur` (rule 1). */
function opensAttempt(cur: Building | null, r: NormRecord): boolean {
  const reopener = r.action !== undefined && REOPENERS.has(r.action);
  if (cur === null) return reopener || (r.action !== undefined && FIRST_OPENERS.has(r.action));
  if (!reopener) return false;
  if (cur.close !== null) return true;
  return r.action === ACTION.DispatchStart && cur.start !== null;
}

function add(cur: Building, r: NormRecord): void {
  cur.records.push(r);
  if (r.action === ACTION.DispatchStart && cur.start === null) cur.start = r;
  if (cur.close === null && isClosing(r)) cur.close = r;
}

/** Closing records seen before anything opened go to the first attempt left
 *  with no close of its own (rule 2's skew case). */
function assignOrphans(attempts: Building[], orphans: NormRecord[]): void {
  for (const orphan of orphans) {
    const open = attempts.find((a) => a.close === null);
    if (!open) return;
    open.close = orphan;
    open.skewed = true;
  }
}

/** A run's records segmented into attempts (rule 1), time order. */
export function attemptsOf(records: readonly NormRecord[]): Attempt[] {
  const sorted = [...records].sort(byTime);
  const attempts: Building[] = [];
  const lead: NormRecord[] = [];
  const orphans: NormRecord[] = [];
  let cur: Building | null = null;
  for (const r of sorted) {
    if (opensAttempt(cur, r)) {
      cur = { opening: r, start: null, records: [], close: null, skewed: false };
      attempts.push(cur);
    }
    if (cur) {
      add(cur, r);
    } else {
      lead.push(r);
      if (isClosing(r)) orphans.push(r);
    }
  }
  if (attempts.length) attempts[0].records.unshift(...lead);
  else if (orphans.length) attempts.push({ opening: orphans[0], start: null, records: lead, close: orphans[0], skewed: false });
  assignOrphans(attempts, orphans);
  return attempts;
}

/** The attempt's close as of `asOf` (rules 2 and 3), or `null`. */
function closeAsOf(a: Attempt, asOf: number): Close | null {
  if (!a.close || !isAsOf(a.close, asOf)) return null;
  const terminal = a.records.find((r) => isDispatchTerminal(r.action) && isAsOf(r, asOf));
  const record = terminal ?? a.close;
  return { edge: closeEdgeOf(record) ?? { kind: "session_end" }, atMs: a.close.tMs, skewed: a.skewed, record };
}

const waitSecondsOf = (r: NormRecord): number => {
  const s = ((r.payload ?? r.fields) as { wait_seconds?: unknown } | undefined)?.wait_seconds;
  return typeof s === "number" && Number.isFinite(s) ? Math.max(0, s) : 0;
};

/** When the attempt's open budget wait lapses (rule 4), or `null` when no
 *  wait is open as of `asOf`. */
function openWaitUntil(recs: readonly NormRecord[], policy: LifecyclePolicy): number | null {
  const wait = latestByTime(recs.filter((r) => r.action === ACTION.BudgetWait));
  if (!wait || wait.tMs === null) return null;
  const at = wait.tMs;
  const ended = recs.some((r) => r !== wait && (r.tMs === null || r.tMs >= at) && (r.action === ACTION.BudgetResume || isClosing(r)));
  return ended ? null : at + waitSecondsOf(wait) * 1000 + policy.budgetWaitGraceMs;
}

/** Whether `lastActivityMs` is more than the policy's window before `asOf`.
 *  No activity at all is stale: absence of evidence is not evidence of life.
 *  The one staleness rule; the mission graph's step meter asks it too. */
export function isStale(lastActivityMs: number | null, asOf: number, policy: LifecyclePolicy): boolean {
  return lastActivityMs === null || asOf - lastActivityMs > policy.staleAfterMs;
}

const latestTime = (recs: readonly NormRecord[]): number | null => {
  const ts = timesOf(recs);
  return ts.length ? Math.max(...ts) : null;
};

function startOf(a: Attempt): number | null {
  const ts = timesOf(a.records);
  return a.start?.tMs ?? a.opening.tMs ?? (ts.length ? Math.min(...ts) : null);
}

/** The records whose activity keeps an open attempt alive: its own, and
 *  for a mission's whole-run bookend, every other run of its mission (rule
 *  6). */
function activityOf(run: RunRecords, recs: readonly NormRecord[], asOf: number): (readonly NormRecord[])[] {
  return [recs, ...run.group.siblings.map((g) => recordsAsOf(g.records, asOf))];
}

/** The phase of an attempt that has opened and not closed (rules 4-6). */
function openPhase(run: RunRecords, recs: readonly NormRecord[], asOf: number, policy: LifecyclePolicy, presence: Presence): Pick<Lifecycle, "phase" | "waitUntilMs"> {
  if (run.next && isAsOf(run.next.opening, asOf)) return { phase: "stale", waitUntilMs: null };
  if (presence.has(run.ref.sessionId)) return { phase: "open", waitUntilMs: null };
  const sets = activityOf(run, recs, asOf);
  const until = maxOf(sets.map((set) => openWaitUntil(set, policy)));
  if (until !== null && asOf <= until) return { phase: "waiting", waitUntilMs: until };
  const quietFrom = maxOf([...sets.map(latestTime), until]);
  return { phase: isStale(quietFrom, asOf, policy) ? "stale" : "open", waitUntilMs: null };
}

/** The largest of `xs`, ignoring `null`s; `null` when there is none. */
function maxOf(xs: readonly (number | null)[]): number | null {
  let best: number | null = null;
  for (const x of xs) if (x !== null && (best === null || x > best)) best = x;
  return best;
}

/** `run`'s lifecycle as of `asOf`. */
export function lifecycleAt(run: RunRecords, asOf: number, policy: LifecyclePolicy, presence: Presence = NO_PRESENCE): Lifecycle {
  const a = run.attempt;
  if (!a || !isAsOf(a.opening, asOf)) {
    const phase = presence.has(run.ref.sessionId) ? "open" : "not_started";
    return { phase, startMs: null, lastActivityMs: latestTime(recordsAsOf(run.group.records, asOf)), close: null, waitUntilMs: null };
  }
  const recs = recordsAsOf(a.records, asOf);
  const base = { startMs: startOf(a), lastActivityMs: latestTime(recs) };
  const close = closeAsOf(a, asOf);
  if (close) return { ...base, phase: "closed", close, waitUntilMs: null };
  return { ...base, close: null, ...openPhase(run, recs, asOf, policy, presence) };
}

/** Whether the run is in flight: open, or held by a budget wait. */
export const isRunning = (l: Lifecycle): boolean => l.phase === "open" || l.phase === "waiting";

/** Where the run's span ends as of `asOf`: now while it runs, its close, or
 *  its last sign of life when it stopped with no ending recorded. */
export function endMs(l: Lifecycle, asOf: number): number | null {
  if (isRunning(l)) return asOf;
  return l.close?.atMs ?? l.lastActivityMs;
}

function closedState(edge: CloseEdge): RunState {
  switch (edge.kind) {
    case "complete":
      return { status: "complete", killed: false };
    case "error":
      return { status: "error", killed: edge.killed };
    case "session_end":
      return { status: "abandoned", killed: false, abandonReason: "noterminal" };
    case "budget_stop":
      return { status: "abandoned", killed: false, abandonReason: edge.byOperator ? "aborted" : "noterminal" };
    case "mission_abort":
      return { status: "abandoned", killed: false, abandonReason: "aborted" };
  }
}

/** The run's canonical status: the ONLY lifecycle → `RunStatus` map. */
export function toRunState(l: Lifecycle): RunState {
  switch (l.phase) {
    case "open":
    case "waiting":
      return { status: "running", killed: false };
    case "not_started":
      return { status: "planned", killed: false };
    case "stale":
      return { status: "abandoned", killed: false, abandonReason: "noterminal" };
    case "closed":
      return closedState(l.close?.edge ?? { kind: "session_end" });
  }
}

/** (#2011, #2346) The run's own measured duration: the `wall_ms` its
 *  dispatch terminal carries (the runtime's sub-second measure, taken
 *  between its start and terminal writes), or `null` when it closed any
 *  other way or the record predates the field. The run page's run-time tile
 *  and playback's elapsed readout both read it, so the two agree. */
export function recordedWallMs(close: Close | null): number | null {
  if (!close || !isDispatchTerminal(close.record.action)) return null;
  const w = (close.record.payload as { wall_ms?: unknown } | undefined)?.wall_ms;
  return typeof w === "number" && Number.isFinite(w) ? w : null;
}

/** A run's span over every attempt it had: the first attempt's start to the
 *  last one's close (`null` while it has none). What playback's focus range
 *  covers. */
export function spanOf(group: RunGroup): { startMs: number | null; endMs: number | null } {
  const first = group.attempts[0];
  const last = group.attempts[group.attempts.length - 1];
  return { startMs: first ? startOf(first) : null, endMs: last?.close?.tMs ?? null };
}

const MISSION_LIFECYCLE: ReadonlySet<NormAction> = new Set<NormAction>([ACTION.MissionStart, ACTION.MissionClose, ACTION.MissionAbort]);

/** Whether `group` is a mission's own lifecycle session (it carries the
 *  mission's start or end). */
export const isMissionLifecycle = (group: RunGroup): boolean =>
  group.records.some((r) => r.action !== undefined && MISSION_LIFECYCLE.has(r.action));

/** Whether a mission's lifecycle session has closed (a `mission.close` or
 *  `mission.abort`), in whatever record set it is read from. */
export function missionClosed(groups: readonly RunGroup[]): boolean {
  return groups.some((g) => {
    const last = g.attempts[g.attempts.length - 1];
    return isMissionLifecycle(g) && last?.close != null;
  });
}
