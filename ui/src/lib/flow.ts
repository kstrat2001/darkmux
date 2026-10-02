/**
 * Ports of `viewer.html`'s raw-flow-record derivation pipeline — the
 * machinery behind the machine lens's "runs on <machine>" list and its
 * local-machine identity resolution. Every function here is named at its
 * legacy source line. Validated line-for-line against the recorded corpus
 * (`tests/parity/corpus/flow-{today,yesterday}.json`) by hand-simulating
 * this exact logic in Node and diffing the result against
 * `tests/parity/goldens/machine.txt`'s runs section before this file was
 * written — see the packet report for the transcript. One bug surfaced by
 * that validation and is worth naming so a future port of another lens
 * doesn't repeat it: `loadLiveWindow()` (viewer.html:3497) fetches
 * `[prevDate, today]` IN THAT ORDER and concatenates in that order — a
 * session_id that recurs across the day boundary must resolve its
 * "first-seen" record from the EARLIER day first, which only happens if the
 * merge preserves fetch order. Concatenating `[today, yesterday]` instead
 * (the natural-feeling order) silently reorders which record `Array.find`
 * returns for a reused session id and desyncs the runs list's sort order
 * from rank ~10 onward with NO error — exactly the class of bug this
 * validate-before-port step exists to catch.
 */

import type { PresenceBeat } from "../types/generated/PresenceBeat";
// (#2813) The canonical status axis, generated from the Rust enum. Importing
// it here is the point: the label below is a total function of it, so the two
// cannot drift apart again.
import type { RunStatus } from "../types/generated/RunStatus";
import type { AbandonReason } from "../types/generated/AbandonReason";
import { isPlainObject } from "./guards";
import { missionClosed } from "./lifecycle";
import { runIndex } from "./runRef";
import { findUid, sameUid, uidOf } from "./machineIdentity";
import { ACTION, CATEGORY, SOURCE, byTime, earliestByTime, ingestJsonl, ingestRecord, latestByTime, payloadOf, recKey, recordsAsOf, recordsSince, type NormRecord } from "./ingest";

/** `LIVE_WINDOW_MS` — viewer.html:3374. The rolling live window `RAW` is
 * bounded to; also the "N records · last Nh" meta-line's hour figure. */
export const LIVE_WINDOW_MS = 24 * 60 * 60 * 1000;


/** `todayUTC()` — viewer.html:3369. */
export function todayUTC(): string {
  return new Date().toISOString().slice(0, 10);
}

/** `prevDateUTC()` — viewer.html:3379. */
export function prevDateUTC(d: string): string {
  const dt = new Date(d + "T00:00:00Z");
  dt.setUTCDate(dt.getUTCDate() - 1);
  return dt.toISOString().slice(0, 10);
}

/** GETs a static playback source (`source.ts, the flow file`) and
 * ingests it — the static-build twin of `GET /flow/<date>`, read directly by
 * both `useRouteRecords` (the event log's data) and `PlaybackLens` (the
 * stage's data), each via the SAME query key so they share one fetch and can
 * never disagree about the file's contents (#1801).
 *
 * Returns `[]` rather than throwing on a network failure, a 404, or an
 * empty file — matching legacy's own `catch(e){ RAW=[]; }` around the same
 * fetch. A static build with no committed flow file (or one not yet built)
 * is a valid, if empty, playback — never a crash and never a silent
 * fallback to the live route (see `route.ts`'s own doc for why the ROUTE
 * itself does not depend on this fetch succeeding). */
export async function fetchStaticFlowRecords(src: string): Promise<NormRecord[]> {
  try {
    const res = await fetch(src);
    if (!res.ok) return [];
    const text = await res.text();
    return ingestJsonl(text);
  } catch {
    return [];
  }
}

/** `if(!injectedDate&&RAW.length)date=String(RAW[0].ts||"").slice(0,10)||date;`
 * — viewer.html:3902, the flowSrc branch's own date derivation. Takes the
 * records in FILE order, deliberately `records[0]` rather than the earliest
 * record, matching legacy's un-sorted read, and reads its parsed time as a
 * UTC day. Returns `null` on an empty array or a first record with no usable
 * time, so a caller supplies its OWN placeholder rather than this function
 * inventing one. */
export function firstRecordDate(records: readonly NormRecord[]): string | null {
  const t = records[0]?.tMs;
  return t == null ? null : utcDay(t);
}

const utcDay = (t: number): string => new Date(t).toISOString().slice(0, 10);

/** The earliest record's UTC day in a slice, or null. Records from the daemon
 * are not guaranteed to arrive sorted, so this scans rather than reading the
 * first element. */
export function earliestRecordDate(records: readonly NormRecord[]): string | null {
  const first = earliestByTime(records)?.tMs;
  return first == null ? null : utcDay(first);
}

/** Header owns liveness (operator, 2026-09-03): a mission page is a RECORDING
 * only once the mission has reached a terminal record; until then it is live,
 * whatever day its records carry. The dispatch route decides the same thing
 * from presence; a mission decides it from its own lifecycle records because
 * a mission's work spans many sessions. Returns the replay day, or null while
 * the mission is still running. */
export function missionReplayDate(records: readonly NormRecord[]): string | null {
  return missionClosed(runIndex(records).groups) ? earliestRecordDate(records) : null;
}

/** The `/flow-mission/:id` response body's own `truncated` flag —
 * `crates/darkmux-serve/src/lib.rs`'s `collect_records_by_field`, capped at
 * `MAX_CATALOG_RECORDS` (10,000). A bare array never carries this flag and
 * reads as `false`. `MissionGraphLens.tsx`'s own `srvTruncated` reads the
 * SAME field (`bodies[0].truncated`) for the same reason: a mission past the
 * server cap must say so, or "N of 10000" silently restates the cap as the
 * mission's whole history. */
export function bodyTruncated(body: unknown): boolean {
  if (!isPlainObject(body)) return false;
  return !!(body as { truncated?: boolean }).truncated;
}

/** The tail-cache-side counterpart to `buildFlowWindow`'s own dedup+window
 * filter — used by the live tail (SSE appends and the reconcile backstop,
 * viewer.html's `reconcileLiveWindow`, 3758-3782) so repeated `?since=`
 * polls with a deliberate overlap window (`RECONCILE_OVERLAP_MS`) don't grow
 * the tail cache's stored array by the overlap on every single poll.
 * `buildFlowWindow` itself ALSO dedups the final merged (day-fetch + tail)
 * result, so this isn't required for display correctness — it's what keeps
 * the underlying cache entry bounded across a long-lived tab, the same thing
 * `applyLive()`'s RAW age-out prunes for (viewer.html:3522-3539). */
export function mergeTailRecords(existing: readonly NormRecord[], incoming: readonly NormRecord[], cutMs: number): NormRecord[] {
  const seen = new Set(existing.map(recKey));
  const merged = existing.slice();
  for (const r of incoming) {
    const k = recKey(r);
    if (seen.has(k)) continue;
    seen.add(k);
    merged.push(r);
  }
  return recordsSince(merged, cutMs);
}

/** The view-model half of `flowToRenderModel()` (viewer.html:3195-3234):
 * ingested records plus one synthesized runtime row per session, sorted by
 * time. Deliberately does NOT window or dedup: those belong to the LIVE
 * two-day merge (`buildFlowWindow`, below), not to reading a record set.
 *
 * (#1800 P2) A historical day is shaped the same way and windowed NOT AT
 * ALL — legacy's playback boot is literally `DATA=flowToRenderModel(RAW)`
 * with no window step (viewer.html:3922). Feeding a replayed day through
 * `buildFlowWindow` instead would drop every record older than 24h, i.e. the
 * entire day. */
export function shapeRecords(records: readonly NormRecord[]): NormRecord[] {
  return [...records, ...perSessionRuntimeRecords(records)].sort(byTime);
}

/** The APPEND half of `flowToRenderModel()` — viewer.html:3223-3234. One
 * synthetic `source:"runtime"` telemetry record per session that emitted any
 * `dispatch.turn`, carrying that session's max `turn_seq` as its TURNS
 * metric. The subsystem view reads the metric from here rather than
 * re-scanning, which is why the record exists at all.
 *
 * (#1800) Ported because it is not merely internal: these records are part of
 * `DATA`, so they are COUNTED. `goldens/playback-date.txt`'s meta line reads
 * "2008 records" against a fixture holding 1993 real records and 15 sessions
 * with turns — the 15 are these. The live meta line does not state a count
 * (legacy moved it to the event pane), which is why nothing noticed their
 * absence until a replay had to say the number out loud.
 *
 * Legacy sorts the whole set by ts afterwards because these are appended out
 * of order, and the event log plus follow-latest both assume `DATA` is
 * temporal. `shapeRecords` does the same, so the sort is not this function's
 * own concern. Each row goes through `ingestRecord` like any other record, so
 * its `tMs` obeys the one bad-timestamp policy. */
function perSessionRuntimeRecords(records: readonly NormRecord[]): NormRecord[] {
  const perSession = new Map<string, { turns: number; ts: string; machineId?: string; machineUid?: string }>();
  for (const r of records) {
    if (r.action !== ACTION.DispatchTurn || !r.session_id) continue;
    const seq = Number(payloadOf(r, ACTION.DispatchTurn)?.turn_seq) || 0;
    const prev = perSession.get(r.session_id);
    const entry = prev ?? { turns: 0, ts: r.ts };
    entry.turns = Math.max(entry.turns, seq);
    // `e.ts=r.ts` unconditionally — the LAST turn's timestamp, not the max.
    // Records arrive in ts order, so these agree; keeping legacy's form means
    // they keep agreeing if that ever stops being true.
    entry.ts = r.ts;
    if (r.machine_id) entry.machineId = r.machine_id;
    if (r.machine_uid) entry.machineUid = r.machine_uid;
    perSession.set(r.session_id, entry);
  }
  const out: NormRecord[] = [];
  for (const [sessionId, e] of perSession) {
    const row = ingestRecord({
      ts: e.ts,
      category: CATEGORY.Telemetry,
      source: "runtime",
      machine_id: e.machineId,
      machine_uid: e.machineUid,
      session_id: sessionId,
      fields: { turns: e.turns },
    });
    if (row) out.push(row);
  }
  return out;
}

/** `loadLiveWindow()` + the dispatch-action slice of `flowToRenderModel()` —
 * viewer.html:3497-3512 / 3161-3187. `yesterday`/`today` MUST be passed in
 * that fetch order (see the module doc above for why). */
export function buildFlowWindow(yesterday: readonly NormRecord[], today: readonly NormRecord[], nowMs: number): NormRecord[] {
  const windowed = recordsSince(shapeRecords([...yesterday, ...today]), nowMs - LIVE_WINDOW_MS);
  const seen = new Set<string>();
  return windowed.filter((r) => {
    const k = recKey(r);
    if (seen.has(k)) return false;
    seen.add(k);
    return true;
  });
}

/** `recompute()`'s tMax — viewer.html:1040-1041. */
export function computeTMax(data: readonly NormRecord[]): number {
  return latestByTime(data)?.tMs ?? Date.now();
}

/** `recompute()`'s tMin — viewer.html:1051. Unused while `/next` was
 * live-only (the live timeline anchors on NOW and a fixed window); a REPLAY
 * spans `tMin..tMax`, which is what makes the axis describe the recorded day
 * rather than the last 24 hours of wall-clock. */
export function computeTMin(data: readonly NormRecord[]): number {
  return earliestByTime(data)?.tMs ?? Date.now();
}

/** What a lens is allowed to say about a run: one canonical status, plus the
 * payload fact a LABEL may render it with. */
export interface RunState {
  status: RunStatus;
  /** `abandoned` only. Mirrors `Run.abandoned_reason` on the wire. */
  abandonReason?: AbandonReason;
}

/** `machPresent()` — viewer.html:1321-1327. true=present, false=absent,
 * null=unknown (no evidence either way). */
export function machPresent(
  data: NormRecord[],
  liveMachines: Map<string, PresenceBeat>,
  tMax: number,
  m: string,
): boolean | null {
  if (findUid(liveMachines.keys(), m) !== null) return true;
  const edges = recordsAsOf(data, tMax).filter(
    (r) => sameUid(uidOf(r), m) && (r.action === ACTION.MachineOnline || r.action === ACTION.MachineOffline),
  );
  // The latest edge by time; an untimed one only when no edge is timed.
  const last = latestByTime(edges);
  return last === undefined ? null : last.action === ACTION.MachineOnline;
}

function compKey(sessionId: string | undefined, ts: string | undefined): string {
  return `${sessionId || ""}\x1f${ts || ""}`;
}

/** `flowToRenderModel()` — viewer.html:3171-3242. Shapes an ingested
 * record array (the `/flow-dispatch/<id>` or `/flow-mission/<id>` "replay this
 * thing" payload — the session drill-in's data source, `lenses/session/
 * sessionRun.ts`) into the shape `runRegions()` reads:
 *
 * - `fields` ALIASED from `payload` when a record carries the latter but
 *   not the former (schema 1.6+ carries type-specific data under `fields`
 *   on the wire; older/synthesized records only have `payload`), which the
 *   live window's `shapeRecords` does not do, so a session-view consumer
 *   reading `r.fields` uniformly needs THIS pass
 * - a `dispatch.compaction` record retagged as compaction telemetry, UNLESS
 *   a dedicated `telemetry.compaction` sibling already covers the same
 *   `(session_id, ts)` (the #1122 double-count guard)
 * - a category default (`work` absent a `source`, else `telemetry`)
 * - then `shapeRecords`: the synthesized per-session "runtime" row (the
 *   ONLY source for the session view's TURNS metric) and the time sort. */
export function flowToRenderModel(records: readonly NormRecord[]): NormRecord[] {
  const compTelemetryKeys = new Set<string>();
  for (const r of records) {
    if (r.action === ACTION.TelemetryCompaction) compTelemetryKeys.add(compKey(r.session_id, r.ts));
  }
  const retagged = records.map((r) => {
    const o: NormRecord = { ...r };
    if (o.payload && !o.fields) o.fields = o.payload;
    if (o.action === ACTION.DispatchCompaction && !compTelemetryKeys.has(compKey(o.session_id, o.ts))) {
      const p = payloadOf(o, ACTION.DispatchCompaction);
      o.category = CATEGORY.Telemetry;
      o.source = SOURCE.Compaction;
      o.fields = { from: p?.before_messages || 0, to: p?.after_messages || 0 };
    }
    if (!o.category) o.category = o.source ? CATEGORY.Telemetry : CATEGORY.Work;
    return o;
  });
  return shapeRecords(retagged);
}
