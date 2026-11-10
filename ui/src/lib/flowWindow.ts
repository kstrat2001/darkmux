/**
 * The live flow window, kept up to date by INSERTION while only the live tail
 * grows. `buildFlowWindow` (`lib/flow.ts`) copies, shapes, sorts and dedups
 * the whole 24h window; run on every streamed record it cost about 20-40 ms
 * per record at 30k records. When the only change is records appended to
 * today's tail, each new record is placed into the window already sorted, at
 * the position the full build would give it, and only the per-session
 * runtime row its turn moved is replaced. Anything else (a day fetch, the
 * window's edge moving, a pruned tail) is a full build. The two must agree
 * exactly: `flowWindow.test.ts` checks every step of random streams against
 * `buildFlowWindow`.
 *
 * The order the full build gives, which insertion reproduces: by time
 * (untimed last), and among equal times in concat order (yesterday's day,
 * its tail, today's day, its tail) with every synthetic runtime row after
 * every real record, the synthetic rows in their sessions' first-seen order.
 * A duplicate (same `recKey`) keeps its first occurrence in that order; a
 * duplicate shares its original's time, so it is always in the same run of
 * equal times.
 */

import { foldSessionTurn, LIVE_WINDOW_MS, sessionRuntimeRow, type SessionTurns } from "./flow";
import { byTime, isAtOrAfter, recKey, recordsSince, type NormRecord } from "./ingest";

export interface FlowWindowInputs {
  yDay: readonly NormRecord[];
  yTail: readonly NormRecord[];
  tDay: readonly NormRecord[];
  tTail: readonly NormRecord[];
  /** The window's trailing edge (`flowWindowEdgeMs`); the window keeps what
   *  is at or after `edgeMs - LIVE_WINDOW_MS`. */
  edgeMs: number;
}

export interface FlowWindowState {
  inputs: FlowWindowInputs;
  data: NormRecord[];
  /** Every session's turn fold, over every record (in the window or not). */
  sessions: Map<string, SessionTurns>;
  /** Each session's synthetic row, as last built. */
  rows: Map<string, NormRecord>;
  /** Whether this state came from a full build (false: from insertion). */
  rebuilt: boolean;
}

/** The synthetic rows' session order, by row. Rows are fresh objects, so a
 *  module-wide map is safe to share across states. */
const syntheticOrder = new WeakMap<NormRecord, number>();

/** The window for `inputs`, inserting into `prev` when the only change is
 *  records appended to today's tail. Pure over `prev`: it is never mutated,
 *  so a discarded render cannot corrupt the next one. */
export function nextFlowWindow(prev: FlowWindowState | null, inputs: FlowWindowInputs): FlowWindowState {
  if (prev && sameInputs(prev.inputs, inputs)) return prev;
  if (prev && appendsOnly(prev.inputs, inputs)) return insertAppended(prev, inputs);
  return fullBuild(inputs);
}

function sameInputs(a: FlowWindowInputs, b: FlowWindowInputs): boolean {
  return a.yDay === b.yDay && a.yTail === b.yTail && a.tDay === b.tDay && a.tTail === b.tTail && a.edgeMs === b.edgeMs;
}

/** Whether `next` only appended to today's tail: every other input is the
 *  same array, and the old tail is the new tail's prefix (the live tail
 *  writes `[...prev, ...batch]`, so its records keep their identity). */
function appendsOnly(prev: FlowWindowInputs, next: FlowWindowInputs): boolean {
  if (prev.yDay !== next.yDay || prev.yTail !== next.yTail || prev.tDay !== next.tDay || prev.edgeMs !== next.edgeMs) return false;
  const n = prev.tTail.length;
  if (next.tTail.length < n) return false;
  return n === 0 || (next.tTail[0] === prev.tTail[0] && next.tTail[n - 1] === prev.tTail[n - 1]);
}

function fullBuild(inputs: FlowWindowInputs): FlowWindowState {
  const all = [...inputs.yDay, ...inputs.yTail, ...inputs.tDay, ...inputs.tTail];
  const sessions = new Map<string, SessionTurns>();
  for (const r of all) foldSessionTurn(sessions, r);
  const rows = new Map<string, NormRecord>();
  for (const [sid, e] of sessions) {
    const row = sessionRuntimeRow(sid, e);
    if (!row) continue;
    rows.set(sid, row);
    syntheticOrder.set(row, e.order);
  }
  const windowed = recordsSince([...all, ...rows.values()].sort(byTime), cutOf(inputs));
  const seen = new Set<string>();
  const data = windowed.filter((r) => {
    const k = recKey(r);
    if (seen.has(k)) return false;
    seen.add(k);
    return true;
  });
  return { inputs, data, sessions, rows, rebuilt: true };
}

function cutOf(inputs: FlowWindowInputs): number {
  return inputs.edgeMs - LIVE_WINDOW_MS;
}

function insertAppended(prev: FlowWindowState, inputs: FlowWindowInputs): FlowWindowState {
  const data = prev.data.slice();
  const sessions = new Map(prev.sessions);
  const rows = new Map(prev.rows);
  const cut = cutOf(inputs);
  for (const r of inputs.tTail.slice(prev.inputs.tTail.length)) {
    if (isAtOrAfter(r, cut)) placeReal(data, r);
    const sid = foldSessionTurn(sessions, r);
    if (sid !== null) replaceRuntimeRow(data, rows, sid, sessions.get(sid)!, cut);
  }
  return { inputs, data, sessions, rows, rebuilt: false };
}

/** A time as the sort sees it: untimed records sort last. */
const timeOf = (r: NormRecord): number => r.tMs ?? Infinity;

/** The run of records at `t`: `[lo, hi)`. */
function runAt(data: readonly NormRecord[], t: number): [number, number] {
  let lo = 0;
  let hi = data.length;
  while (lo < hi) {
    const mid = (lo + hi) >>> 1;
    if (timeOf(data[mid]) < t) lo = mid + 1;
    else hi = mid;
  }
  let end = lo;
  while (end < data.length && timeOf(data[end]) === t) end++;
  return [lo, end];
}

/** The index of a record in `data[lo, hi)` sharing `r`'s dedup key, or -1. */
function duplicateIn(data: readonly NormRecord[], lo: number, hi: number, r: NormRecord): number {
  const k = recKey(r);
  for (let i = lo; i < hi; i++) if (recKey(data[i]) === k) return i;
  return -1;
}

/** Places a real record after every real record of its time and before that
 *  time's synthetic rows. A duplicate already placed wins, unless it is a
 *  synthetic row: a real record comes first in the full build's order. */
function placeReal(data: NormRecord[], r: NormRecord): void {
  const [lo, hi] = runAt(data, timeOf(r));
  const dup = duplicateIn(data, lo, hi, r);
  if (dup >= 0 && !syntheticOrder.has(data[dup])) return;
  if (dup >= 0) data.splice(dup, 1);
  let at = lo;
  while (at < data.length && timeOf(data[at]) === timeOf(r) && !syntheticOrder.has(data[at])) at++;
  data.splice(at, 0, r);
}

/** Swaps a session's synthetic row for one built from its new fold, placed
 *  among its time's synthetic rows by session order. */
function replaceRuntimeRow(data: NormRecord[], rows: Map<string, NormRecord>, sid: string, e: SessionTurns, cut: number): void {
  const old = rows.get(sid);
  if (old) {
    const [lo, hi] = runAt(data, timeOf(old));
    const i = data.indexOf(old, lo);
    if (i >= 0 && i < hi) data.splice(i, 1);
    rows.delete(sid);
  }
  const row = sessionRuntimeRow(sid, e);
  if (!row) return;
  rows.set(sid, row);
  syntheticOrder.set(row, e.order);
  if (!isAtOrAfter(row, cut)) return;
  const [lo, hi] = runAt(data, timeOf(row));
  if (duplicateIn(data, lo, hi, row) >= 0) return;
  let at = lo;
  while (at < hi && (!syntheticOrder.has(data[at]) || syntheticOrder.get(data[at])! < e.order)) at++;
  data.splice(at, 0, row);
}
