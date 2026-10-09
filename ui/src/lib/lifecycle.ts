/**
 * A run's lifecycle as of an instant: the viewer's one set of rules for
 * "has it started, is it running, waiting, finished, or gone quiet".
 *
 * Every surface asks `lifecycleAt` (the fleet card, the activity timeline,
 * the run page's pill and clock, the mission graph's step meter, playback's
 * focus range, the live token scope), so the same run reads the same phase
 * on each of them at the same moment. Live is `asOf = now`; playback is
 * `asOf = the playhead`. Presence is an extra input that can only ADD: it
 * holds the session's CURRENT run (its latest attempt, whatever mission)
 * open against the staleness clock, never a run a later attempt
 * superseded, never one that has not started, and never against a record
 * that closed it. It is a fact about now: a caller passes it only when the
 * instant it judges is the live edge.
 *
 * The rules:
 *
 * 1. Attempts. A session's records segment into attempts, every mission's
 *    together in time order (`segmentSession`). An attempt opens on its
 *    first opening record (a bookend start, `run.start` or
 *    `dispatch.start`; a `budget.wait`, a `mission.start`, a `step.start`;
 *    or, when nothing opened yet, any turn, heartbeat, tool call or rest).
 *    A record of an execution (every execution-grain record carries its
 *    `execution_id`; a pre-4.0 one reads as its session and mission) joins
 *    the latest attempt of that execution, so a session holding several (a
 *    map's items) keeps each one's records and its own close apart. Any
 *    other record, and one of an execution with no attempt yet, joins by
 *    mission: a record naming a mission joins that mission's latest
 *    attempt; one naming none joins the latest attempt still open at its
 *    time, or the latest opened when none is. A bookend start in an attempt that
 *    already has one, or any reopening record after the attempt closed,
 *    starts the next attempt (a relaunch under the same id). The attempt
 *    current as of t is the latest one opened by t.
 * 2. Close. An attempt closes on its earliest closing record: a bookend
 *    terminal (a run's or an execution's), a step terminal (a `step.complete`
 *    that names a later step of its task still planned does not close it,
 *    #3074), `session.end`,
 *    `budget.stop`, `mission.close` or `mission.abort`. A closing record timestamped before anything opened
 *    (clock skew across machines, #1988) closes the first attempt of its
 *    mission (any, when it names none) left with no close of its own, and
 *    is marked `skewed`; such closes are placed in time order, every
 *    mission's together. With no attempt at all to close, it is not a run (phase `not_started`), but its session recorded
 *    its end, which the lifecycle carries as `close` and a status reads. A
 *    record with an unparsable `ts` is inside every as-of cut, so an untimed
 *    terminal still closes its run.
 * 3. Outcome. How it ended comes from the attempt's bookend terminal when
 *    it has one (a `session.end` that lands first does not erase a clean
 *    `run.complete` or `dispatch.complete`), else from the closing record
 *    itself. A terminal that names the operator's stop (a `budget.stop`'s
 *    `reason`, a `dispatch.error`'s `stop_reason`) is abandoned as aborted,
 *    never an error.
 * 4. Waiting. A `budget.wait` with no `budget.resume` or closing record
 *    after it holds the run `waiting` until its announced resume time plus
 *    `budgetWaitGraceMs`; past that the staleness clock runs from there.
 * 5. Stale. An open attempt whose last record is more than `staleAfterMs`
 *    before t has stopped with no ending recorded. So has an attempt a later
 *    one of its mission superseded. Another mission's later attempt on the
 *    same session does not: missions launched from one config share a task
 *    session and run at once (#2125).
 * 6. A mission's run session (`runRef.ts`'s `run` grain, opened by
 *    `run.start`) never beats itself; its executions do. Its activity and
 *    its waits are its mission's other runs' too, so it is in flight while
 *    any of them is.
 */

import { ACTION, byTime, isAsOf, isAtOrAfter, isBookendStart, isBookendTerminal, isExecutionAction, endPayloadOf, latestByTime, payloadOf, recordsAsOf, type NormAction, type NormRecord } from "./ingest";
import type { RunState } from "./flow";
import type { RunGroup, RunRecords } from "./runRef";
import type { RunsPolicy } from "../types/generated/RunsPolicy";
import type { Run } from "../types/generated/Run";

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
  | { readonly kind: "mission_abort" }
  /** An execution the operator stopped: its `dispatch.error` names the stop
   *  (`stop_reason`: a caught signal). Not a failure. */
  | { readonly kind: "operator_stop" };

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
  /** The attempt's start: its bookend start's time, else its opening
   *  record's, else its earliest timed record's. */
  readonly startMs: number | null;
  /** The attempt's latest timed record as of t. */
  readonly lastActivityMs: number | null;
  /** How it ended once `closed`; for a group nothing of which opened
   *  (`not_started`), how its session recorded its end, when it did. */
  readonly close: Close | null;
  /** While `waiting`: when the wait lapses. */
  readonly waitUntilMs: number | null;
}

/** Session ids presence reports live. */
export type Presence = ReadonlySet<string>;
export const NO_PRESENCE: Presence = new Set<string>();

/** The instant runs are judged at, and the presence they are judged with. */
export interface Judgement {
  readonly asOf: number;
  readonly presence: Presence;
}

/** The one rule for what a page judges its runs at: a parked playhead's
 *  instant, with no presence (presence is a fact about now); else now, with
 *  the sessions presence reports live. The run page, the fleet lens and the
 *  event log all read it. */
export function judgementAt(playhead: number | null, now: number, live: Presence): Judgement {
  return playhead === null ? { asOf: now, presence: live } : { asOf: playhead, presence: NO_PRESENCE };
}

/** One attempt of a run (rule 1). */
export interface Attempt {
  /** The record that opened it. */
  readonly opening: NormRecord;
  /** Its bookend start (`run.start` or `dispatch.start`, the brief's
   *  payload), when it has one. */
  readonly start: NormRecord | null;
  /** Its records, time order: from its opening (for the first attempt,
   *  from the run's first record) to the next attempt's opening. */
  readonly records: readonly NormRecord[];
  /** Its earliest closing record. */
  readonly close: NormRecord | null;
  readonly skewed: boolean;
  /** The mission its records name (the first that names one); `null` when
   *  none does. */
  readonly missionId: string | null;
  /** The execution of its first record that is of one; `null` for an
   *  attempt no execution has touched (a run's, a step's). */
  readonly executionId: string | null;
  /** Its place among its SESSION's attempts, every mission's together. */
  readonly index: number;
}

/** A session's records segmented: its attempts (every mission's, time
 *  order) and the records that belong to no attempt, by the mission they
 *  name (`null` for none): work whose opening is outside the window, and a
 *  close with nothing opened before it. */
export interface SessionSegments {
  readonly attempts: readonly Attempt[];
  readonly strays: ReadonlyMap<string | null, readonly NormRecord[]>;
}

/** The records that open an attempt even after an earlier one closed. */
const REOPENERS: ReadonlySet<NormAction> = new Set<NormAction>([
  ACTION.RunStart,
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
  const c = endPayloadOf(r)?.exit_code;
  return typeof c === "number" ? c : null;
};

const hasReason = (r: NormRecord): boolean => {
  const reason = payloadOf(r, ACTION.BudgetStop)?.reason;
  return typeof reason === "string" && reason.length > 0;
};

/** Whether an execution's error terminal names the operator's stop. */
const namesAStop = (r: NormRecord): boolean => {
  const reason = payloadOf(r, ACTION.DispatchError)?.stop_reason;
  return typeof reason === "string" && reason.length > 0;
};

/** The edge a closing record implies; `null` for any other record. */
function closeEdgeOf(r: NormRecord): CloseEdge | null {
  switch (r.action) {
    case ACTION.RunComplete:
    case ACTION.DispatchComplete:
    case ACTION.MissionClose:
      return { kind: "complete" };
    case ACTION.StepComplete:
      return payloadOf(r, ACTION.StepComplete)?.later_step_planned === true ? null : { kind: "complete" };
    case ACTION.DispatchError:
      if (namesAStop(r)) return { kind: "operator_stop" };
      return { kind: "error", killed: exitCodeOf(r) === 137, exitCode: exitCodeOf(r) };
    case ACTION.RunError:
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

/** Whether a record closes the attempt it belongs to (rule 2). */
export const isClosing = (r: NormRecord): boolean => closeEdgeOf(r) !== null;

interface Building {
  opening: NormRecord;
  start: NormRecord | null;
  records: NormRecord[];
  close: NormRecord | null;
  skewed: boolean;
  missionId: string | null;
  executionId: string | null;
  index: number;
}

/** The last attempt of execution `x`. */
function latestOfExecution(attempts: readonly Building[], x: string): Building | null {
  for (let i = attempts.length - 1; i >= 0; i--) if (attempts[i].executionId === x) return attempts[i];
  return null;
}

/** The last attempt of mission `m`. */
function latestOf(attempts: readonly Building[], m: string): Building | null {
  for (let i = attempts.length - 1; i >= 0; i--) if (attempts[i].missionId === m) return attempts[i];
  return null;
}

/** The latest attempt still open (no close yet), or `null`. */
function latestOpen(attempts: readonly Building[]): Building | null {
  for (let i = attempts.length - 1; i >= 0; i--) if (attempts[i].close === null) return attempts[i];
  return null;
}

/** The attempt a record joins (rule 1): a record of an execution joins that
 *  execution's latest attempt; otherwise a record naming a mission joins
 *  that mission's latest attempt, or adopts the current attempt when that
 *  one names no mission yet; a record naming none joins the latest attempt
 *  still open at its time, or the latest opened when none is. `null`: it
 *  belongs to no attempt yet. */
function targetFor(attempts: readonly Building[], m: string | null, x: string | null): Building | null {
  const own = x === null ? null : latestOfExecution(attempts, x);
  if (own) return own;
  const cur = attempts.at(-1) ?? null;
  if (!m) return latestOpen(attempts) ?? cur;
  return latestOf(attempts, m) ?? (cur && cur.missionId === null ? cur : null);
}

/** Whether `r` opens a new attempt rather than joining `mine` (rule 1). */
function opensAttempt(mine: Building | null, r: NormRecord): boolean {
  const reopener = r.action !== undefined && REOPENERS.has(r.action);
  if (mine === null) return reopener || (r.action !== undefined && FIRST_OPENERS.has(r.action));
  if (!reopener) return false;
  if (mine.close !== null) return true;
  return isBookendStart(r.action) && mine.start !== null;
}

function add(cur: Building, r: NormRecord, m: string | null, x: string | null): void {
  cur.records.push(r);
  if (cur.missionId === null && m) cur.missionId = m;
  if (cur.executionId === null) cur.executionId = x;
  if (isBookendStart(r.action) && cur.start === null) cur.start = r;
  if (cur.close === null && isClosing(r)) cur.close = r;
}

function pushStray(strays: Map<string | null, NormRecord[]>, m: string | null, r: NormRecord): void {
  const list = strays.get(m);
  if (list) list.push(r);
  else strays.set(m, [r]);
}

/** A stray's attempt, once the segmentation is done: a close with nothing
 *  opened before it closes the first attempt of its mission (any, when it
 *  names none) left with no close of its own, marked `skewed` (rule 2); any
 *  other record joins that mission's first attempt, from before it opened. */
function homeFor(attempts: readonly Building[], r: NormRecord, m: string | null): Building | null {
  const mine = attempts.filter((a) => m === null || a.missionId === m);
  return isClosing(r) ? (mine.find((a) => a.close === null) ?? null) : (mine[0] ?? null);
}

/** Places the strays, in time order across every mission (the order the
 *  daemon's `place_orphans` places them in): a mission-less close does not
 *  take an attempt from an earlier close that names its mission. */
function placeStrays(attempts: Building[], strays: readonly (readonly [string | null, NormRecord])[]): Map<string | null, NormRecord[]> {
  const left = new Map<string | null, NormRecord[]>();
  for (const [m, r] of strays) {
    const home = homeFor(attempts, r, m);
    if (!home) pushStray(left, m, r);
    else if (isClosing(r)) Object.assign(home, { close: r, skewed: true, records: [r, ...home.records] });
    else home.records.unshift(r);
  }
  return left;
}

/** A session's records segmented into attempts (rule 1), time order. */
export function segmentSession(records: readonly NormRecord[]): SessionSegments {
  const attempts: Building[] = [];
  const strays: (readonly [string | null, NormRecord])[] = [];
  for (const r of [...records].sort(byTime)) {
    const m = r.mission_id || null;
    const x = isExecutionAction(r.action) ? (r.execution_id ?? null) : null;
    let target = targetFor(attempts, m, x);
    if (opensAttempt(target, r)) {
      target = { opening: r, start: null, records: [], close: null, skewed: false, missionId: m, executionId: null, index: attempts.length };
      attempts.push(target);
    }
    if (target) add(target, r, m, x);
    else strays.push([m, r]);
  }
  return { attempts, strays: placeStrays(attempts, strays) };
}

/** How a group that never opened recorded its end (rule 2): nothing opened,
 *  so it is not a run, but its session did record how it ended (the crash
 *  shape: the presence reconciler's `session.end`, with the opening records
 *  in an older day or never written). Its dispatch terminal when it has one
 *  (rule 3), else its earliest closing record, as of `asOf`; `null` when it
 *  has none, or when anything opened. */
function recordedEndAsOf(group: RunGroup, asOf: number): Close | null {
  if (group.attempts.length > 0) return null;
  const closes = group.records.filter((r) => isClosing(r) && isAsOf(r, asOf)).sort(byTime);
  const record = closes.find((r) => isBookendTerminal(r.action)) ?? closes[0];
  return record ? { edge: closeEdgeOf(record) ?? { kind: "session_end" }, atMs: record.tMs, skewed: false, record } : null;
}

/** The attempt's close as of `asOf` (rules 2 and 3), or `null`. */
function closeAsOf(a: Attempt, asOf: number): Close | null {
  if (!a.close || !isAsOf(a.close, asOf)) return null;
  const terminal = a.records.find((r) => isBookendTerminal(r.action) && isAsOf(r, asOf));
  const record = terminal ?? a.close;
  return { edge: closeEdgeOf(record) ?? { kind: "session_end" }, atMs: a.close.tMs, skewed: a.skewed, record };
}

const waitMsOf = (r: NormRecord): number => {
  const ms = payloadOf(r, ACTION.BudgetWait)?.wait_ms;
  return typeof ms === "number" && Number.isFinite(ms) ? Math.max(0, ms) : 0;
};

/** When the attempt's open budget wait lapses (rule 4), or `null` when no
 *  wait is open as of `asOf`. */
function openWaitUntil(recs: readonly NormRecord[], policy: LifecyclePolicy): number | null {
  const wait = latestByTime(recs.filter((r) => r.action === ACTION.BudgetWait));
  if (!wait || wait.tMs === null) return null;
  const at = wait.tMs;
  const ended = recs.some((r) => r !== wait && isAtOrAfter(r, at) && (r.action === ACTION.BudgetResume || isClosing(r)));
  return ended ? null : at + waitMsOf(wait) + policy.budgetWaitGraceMs;
}

/** Whether `lastActivityMs` is more than the policy's window before `asOf`.
 *  No activity at all is stale: absence of evidence is not evidence of life.
 *  The one staleness rule; the mission graph's step meter asks it too. */
function isStale(lastActivityMs: number | null, asOf: number, policy: LifecyclePolicy): boolean {
  return lastActivityMs === null || asOf - lastActivityMs > policy.staleAfterMs;
}

const latestTime = (recs: readonly NormRecord[]): number | null => latestByTime(recs)?.tMs ?? null;

function startOf(a: Attempt): number | null {
  return a.start?.tMs ?? a.opening.tMs ?? [...a.records].sort(byTime)[0]?.tMs ?? null;
}

/** The records whose activity keeps an open attempt alive: its own, and
 *  for a mission's whole-run bookend, every other run of its mission (rule
 *  6). */
function activityOf(run: RunRecords, recs: readonly NormRecord[], asOf: number): (readonly NormRecord[])[] {
  const sets: (readonly NormRecord[])[] = [recs];
  for (const g of run.group.siblings) sets.push(recordsAsOf(g.records, asOf));
  return sets;
}

const openedBy = (a: Attempt | null, asOf: number): boolean => a !== null && isAsOf(a.opening, asOf);

/** The phase of an attempt that has opened and not closed (rules 4-6). */
function openPhase(run: RunRecords, recs: readonly NormRecord[], asOf: number, policy: LifecyclePolicy, presence: Presence): Pick<Lifecycle, "phase" | "waitUntilMs"> {
  if (openedBy(run.next, asOf)) return { phase: "stale", waitUntilMs: null };
  if (presence.has(run.ref.sessionId) && !openedBy(run.sessionNext, asOf)) return { phase: "open", waitUntilMs: null };
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
    const close = a ? null : recordedEndAsOf(run.group, asOf);
    return { phase: "not_started", startMs: null, lastActivityMs: latestTime(recordsAsOf(run.group.records, asOf)), close, waitUntilMs: null };
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
      return { status: "complete" };
    case "error":
      return { status: "error" };
    case "session_end":
      return { status: "abandoned", abandonReason: "noterminal" };
    case "budget_stop":
      return { status: "abandoned", abandonReason: edge.byOperator ? "aborted" : "noterminal" };
    case "mission_abort":
    case "operator_stop":
      return { status: "abandoned", abandonReason: "aborted" };
  }
}

/** The run's canonical status: the ONLY lifecycle → `RunStatus` map. */
export function toRunState(l: Lifecycle): RunState {
  switch (l.phase) {
    case "open":
    case "waiting":
      return { status: "running" };
    case "not_started":
      return l.close ? closedState(l.close.edge) : { status: "planned" };
    case "stale":
      return { status: "abandoned", abandonReason: "noterminal" };
    case "closed":
      return closedState(l.close?.edge ?? { kind: "session_end" });
  }
}

/** The `/runs` row whose run IS this session's: a lab run or a dispatch, each
 *  one role execution, its session named by `dispatch_id`. A mission's row is
 *  never one: the session it names is one of its executions, whose status is
 *  that execution's, not the mission's. `missionId`: the mission the session's
 *  records name, which a dispatch row's id must be (or the session itself, for
 *  a dispatch the daemon read from the flow alone). */
export function ownRowOf(rows: readonly Run[], sessionId: string, missionId: string | null): Run | null {
  return rows.find((r) => r.kind !== "mission" && r.dispatch_id === sessionId && (missionId === null || r.id === missionId || r.id === sessionId)) ?? null;
}

/** The run's state as every view shows it. The operator (2026-10-07): "the
 *  color of the status should be a one time status setting so it should not
 *  even have a chance of being different across views." For a run the daemon
 *  lists (`row`, from `ownRowOf`), the status is decided ONCE, on the row (the
 *  runs board, `darkmux run list`), and a view renders that decision: judged
 *  `live` (at the live edge), the row's status is the state; judged at a past
 *  instant (playback), the lifecycle says whether the run was in flight or not
 *  yet ended then, and once it had ended, how it ended is the row's (when the
 *  row has ended too). Without a row, the one lifecycle rule's (`toRunState`),
 *  which the daemon's own executor shares. */
export function shownRunState(l: Lifecycle, row: Run | null, live: boolean): RunState {
  const own = toRunState(l);
  if (!row) return own;
  const decided: RunState = row.abandoned_reason ? { status: row.status, abandonReason: row.abandoned_reason } : { status: row.status };
  if (live) return decided;
  const ended = (s: RunState) => s.status !== "running" && s.status !== "planned";
  return ended(own) && ended(decided) ? decided : own;
}

/** (#2011, #2346) The run's own measured duration: the `wall_ms` its
 *  bookend terminal carries (the runtime's sub-second measure, taken
 *  between its start and terminal writes), or `null` when it closed any
 *  other way or the record carries none. The run page's run-time tile and
 *  playback's elapsed readout both read it, so the two agree. */
export function recordedWallMs(close: Close | null): number | null {
  if (!close || !isBookendTerminal(close.record.action)) return null;
  const w = endPayloadOf(close.record)?.wall_ms;
  return typeof w === "number" && Number.isFinite(w) ? w : null;
}

/** The run's ACTIVE time: `recordedWallMs` minus the `rest_ms` the same terminal
 *  carries (every inter-turn rest; none recorded reads as none), floored at 0.
 *  The one meaning of "active" on every surface: `darkmux run stats` derives
 *  `active_ms = wall_ms - rest_ms` from the same two fields. `null` when there is
 *  no recorded wall. */
export function recordedActiveMs(close: Close | null): number | null {
  const wall = recordedWallMs(close);
  if (wall === null) return null;
  const rest = endPayloadOf(close?.record)?.rest_ms;
  return Math.max(0, wall - (typeof rest === "number" && Number.isFinite(rest) ? rest : 0));
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
 *  `mission.abort`), in whatever record set it is read from: its latest
 *  attempt closed, or, with the mission's start outside the set, the close
 *  itself is there. A close with nothing opened is no RUN (rule 2), but it
 *  still says the mission ended. */
export function missionClosed(groups: readonly RunGroup[]): boolean {
  return groups.some((g) => isMissionLifecycle(g) && (g.attempts.length ? g.attempts[g.attempts.length - 1].close != null : g.records.some(isClosing)));
}
