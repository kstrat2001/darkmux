/**
 * Which records belong to a run: the viewer's one answer.
 *
 * A run is a `(session_id, mission_id)` PAIR, never a bare session id. The
 * scheduler's session ids are deterministic (`task-<task_id>`, a review's
 * `task-review-probe-mid-task`), so two missions routinely share one id; a
 * lookup by session alone pairs one mission's start with another's end
 * (#2125). A session's records segment into attempts, every mission's
 * together in time order (`lifecycle.ts`'s `segmentSession`); the attempts
 * of one mission are its `RunGroup`, and a `RunRef` names one attempt of one
 * group.
 *
 * Attribution. A record naming a `mission_id` belongs to that mission's
 * attempt. A record naming none (the presence reconciler's `session.end`,
 * some bookends) belongs to the latest attempt still open at its time, or
 * the latest opened when none is: in a session two missions share, a close
 * with no mission closes the run that was running, never a group of its own.
 * Under two missions running at once on one session (#2125) a mission-less
 * record cannot say which it belongs to, and joins the one opened last.
 *
 * The index is built once per window ARRAY (a `WeakMap` on its identity):
 * a window array is never mutated after it is first read, and every
 * per-run lookup after the first is a map read.
 */

import { ACTION, bookendOf, isAsOf, type NormRecord } from "./ingest";
import { segmentSession, type Attempt, type SessionSegments } from "./lifecycle";

/** One attempt of one run. `attempt` indexes `RunGroup.attempts`. */
export interface RunRef {
  readonly sessionId: string;
  readonly missionId: string | null;
  readonly attempt: number;
}

/** Every record of one `(session, mission)` pair, in window order, and the
 *  attempts they segment into (time order). */
export interface RunGroup {
  readonly key: string;
  readonly sessionId: string;
  readonly missionId: string | null;
  readonly records: readonly NormRecord[];
  /** This mission's attempts, time order. */
  readonly attempts: readonly Attempt[];
  /** Every attempt of the session, every mission's, time order: presence
   *  speaks for the latest only. Missions launched from one config share a
   *  task session and run at once (#2125), so another mission's later
   *  attempt never supersedes this group's. */
  readonly sessionAttempts: readonly Attempt[];
  readonly grain: Grain;
  /** A run-grain group's mission's OTHER runs, whose activity is this run's
   *  (a mission's whole-run bookend never beats; its steps do). Empty for
   *  every other grain. */
  readonly siblings: readonly RunGroup[];
}

/** One run's records: what every lifecycle and grain predicate takes. */
export interface RunRecords {
  readonly ref: RunRef;
  readonly group: RunGroup;
  /** The named attempt; `null` when the group has none (nothing opened). */
  readonly attempt: Attempt | null;
  /** Its mission's attempt after it, whose opening supersedes this one. */
  readonly next: Attempt | null;
  /** The session's attempt after it, any mission's: once that opened,
   *  presence on the session speaks for it, not for this one. */
  readonly sessionNext: Attempt | null;
}

/** What kind of unit a group is (contract 8's grains):
 *  - `run`: a run's own session, opened by `run.start` (a pre-4.0 archive's
 *    whole-run `dispatch.start` reaches the viewer as `run.start`: the
 *    daemon's reader upgrades it);
 *  - `execution`: model work (a dispatch, a budget-held call, or a turn,
 *    heartbeat, tool call or rest);
 *  - `lifecycle`: bookkeeping only (a mission's own lifecycle session, a
 *    scheduler task session with no model work in it). */
export type Grain = "run" | "execution" | "lifecycle";

export interface RunIndex {
  readonly groups: readonly RunGroup[];
  /** Groups with at least one record from machine `uid`. */
  groupsOn(uid: string): readonly RunGroup[];
  groupsOfSession(sessionId: string): readonly RunGroup[];
  groupsOfMission(missionId: string): readonly RunGroup[];
  /** The group a record is attributed to; `null` for a sessionless one. */
  groupOf(r: NormRecord): RunGroup | null;
}

const EXECUTION_EVIDENCE: ReadonlySet<unknown> = new Set([
  ACTION.BudgetWait,
  ACTION.DispatchTurnHeartbeat,
  ACTION.DispatchTurn,
  ACTION.DispatchTool,
  ACTION.DispatchRest,
]);

function grainOfRecord(r: NormRecord): Grain {
  const bookend = bookendOf(r.action);
  if (bookend?.edge === "start") return bookend.grain;
  return EXECUTION_EVIDENCE.has(r.action) ? "execution" : "lifecycle";
}

const GRAIN_RANK: Record<Grain, number> = { lifecycle: 0, execution: 1, run: 2 };

/** A group's grain: the strongest any of its records shows. */
export function grainOf(records: readonly NormRecord[]): Grain {
  let g: Grain = "lifecycle";
  for (const r of records) {
    const rg = grainOfRecord(r);
    if (GRAIN_RANK[rg] > GRAIN_RANK[g]) g = rg;
  }
  return g;
}

const groupKey = (sessionId: string, missionId: string | null): string => `${sessionId}\u0000${missionId ?? ""}`;

function pushTo<K, V>(m: Map<K, V[]>, k: K, v: V): void {
  const list = m.get(k);
  if (list) list.push(v);
  else m.set(k, [v]);
}

const NO_GROUPS: readonly RunGroup[] = [];

/** One session's groups: one per mission its attempts name (`null` for
 *  attempts naming none), plus one per mission whose records belong to no
 *  attempt. Records keep window order. */
function sessionGroups(sessionId: string, recs: readonly NormRecord[], seg: SessionSegments): { groups: RunGroup[]; keyOf: Map<NormRecord, string> } {
  const members = new Map<string | null, { attempts: Attempt[]; recs: Set<NormRecord> }>();
  const memberOf = (m: string | null) => members.get(m) ?? members.set(m, { attempts: [], recs: new Set() }).get(m)!;
  for (const a of seg.attempts) {
    const e = memberOf(a.missionId);
    e.attempts.push(a);
    for (const r of a.records) e.recs.add(r);
  }
  for (const [m, strays] of seg.strays) for (const r of strays) memberOf(m).recs.add(r);
  const keyOf = new Map<NormRecord, string>();
  const groups = [...members].map(([m, e]) => {
    const key = groupKey(sessionId, m);
    for (const r of e.recs) keyOf.set(r, key);
    const records = recs.filter((r) => e.recs.has(r));
    return makeGroup(sessionId, m, records, e.attempts, seg.attempts);
  });
  return { groups, keyOf };
}

function makeGroup(sessionId: string, missionId: string | null, records: readonly NormRecord[], attempts: readonly Attempt[], sessionAttempts: readonly Attempt[]): RunGroup {
  return { key: groupKey(sessionId, missionId), sessionId, missionId, records, attempts, sessionAttempts, grain: grainOf(records), siblings: NO_GROUPS };
}

/** Each run-grain group's siblings: its mission's other groups. */
function linkSiblings(byMission: Map<string, RunGroup[]>): void {
  for (const groups of byMission.values()) {
    for (const g of groups) {
      if (g.grain === "run") (g as { siblings: readonly RunGroup[] }).siblings = groups.filter((o) => o !== g);
    }
  }
}

/** The window's records by session, window order. */
function bySessionId(data: readonly NormRecord[]): Map<string, NormRecord[]> {
  const out = new Map<string, NormRecord[]>();
  for (const r of data) if (r && r.session_id) pushTo(out, r.session_id, r);
  return out;
}

function buildIndex(data: readonly NormRecord[]): RunIndex {
  const groups: RunGroup[] = [];
  const keyOf = new Map<NormRecord, string>();
  for (const [sid, recs] of bySessionId(data)) {
    const built = sessionGroups(sid, recs, segmentSession(recs));
    groups.push(...built.groups);
    for (const [r, k] of built.keyOf) keyOf.set(r, k);
  }
  const byGroupKey = new Map(groups.map((g) => [g.key, g]));
  const byUid = new Map<string, RunGroup[]>();
  const bySession = new Map<string, RunGroup[]>();
  const byMission = new Map<string, RunGroup[]>();
  for (const g of groups) {
    for (const uid of new Set(g.records.map((r) => r.machine_uid || "unknown"))) pushTo(byUid, uid, g);
    pushTo(bySession, g.sessionId, g);
    if (g.missionId) pushTo(byMission, g.missionId, g);
  }
  linkSiblings(byMission);
  return {
    groups,
    groupsOn: (uid) => byUid.get(uid) ?? NO_GROUPS,
    groupsOfSession: (sid) => bySession.get(sid) ?? NO_GROUPS,
    groupsOfMission: (mid) => byMission.get(mid) ?? NO_GROUPS,
    groupOf: (r) => byGroupKey.get(keyOf.get(r) ?? "") ?? null,
  };
}

const indexCache = new WeakMap<readonly NormRecord[], RunIndex>();
let indexBuilds = 0;

/** Test-only: how many run indexes have been built. */
export function __runIndexBuilds(): number {
  return indexBuilds;
}

/** The window's run index, built once per window array. */
export function runIndex(data: readonly NormRecord[]): RunIndex {
  let ix = indexCache.get(data);
  if (!ix) {
    ix = buildIndex(data);
    indexCache.set(data, ix);
    indexBuilds++;
  }
  return ix;
}

/** The attempt of `group` that is current as of `asOf`: the latest whose
 *  opening is at or before it, else the first (not yet started). */
export function refAt(group: RunGroup, asOf: number): RunRef {
  let attempt = 0;
  group.attempts.forEach((a, i) => {
    if (isAsOf(a.opening, asOf)) attempt = i;
  });
  return { sessionId: group.sessionId, missionId: group.missionId, attempt };
}

/** The records of `ref` as a `RunRecords`, from `group` (already in hand). */
export function recordsOfGroup(group: RunGroup, ref: RunRef): RunRecords {
  const attempt = group.attempts[ref.attempt] ?? null;
  return {
    ref,
    group,
    attempt,
    next: group.attempts[ref.attempt + 1] ?? null,
    sessionNext: attempt ? (group.sessionAttempts[attempt.index + 1] ?? null) : null,
  };
}

/** The records of `ref` in window `data`; `null` when the window holds no
 *  record of its `(session, mission)` pair. */
export function runRecords(data: readonly NormRecord[], ref: RunRef): RunRecords | null {
  const g = runIndex(data)
    .groupsOfSession(ref.sessionId)
    .find((x) => x.missionId === ref.missionId);
  return g ? recordsOfGroup(g, ref) : null;
}

/** The group's current attempt as of `asOf`, as `RunRecords`. */
export function currentRun(group: RunGroup, asOf: number): RunRecords {
  return recordsOfGroup(group, refAt(group, asOf));
}

/** The opening time of `run`'s attempt, for picking the most recent one:
 *  one not yet open as of `asOf` ranks below every other, and an untimed
 *  opening above every timed one (the bad-timestamp policy's rule 3). */
function openingRank(run: RunRecords, asOf: number): number {
  if (!run.attempt || !isAsOf(run.attempt.opening, asOf)) return -Infinity;
  return run.attempt.opening.tMs ?? Infinity;
}

/** The groups a session route means: the named mission's alone when the
 *  route names one (`#dispatch=<id>&dispatch.mission=<id>`) its records
 *  carry, else every mission's on the session. A mission the records do not
 *  carry (an older archive, mission-less bookends) reads as a link naming
 *  none: the run, never an empty page. */
function routeGroups(data: readonly NormRecord[], sessionId: string, missionId: string | null): readonly RunGroup[] {
  const groups = runIndex(data).groupsOfSession(sessionId);
  if (missionId === null) return groups;
  const named = groups.filter((g) => g.missionId === missionId);
  return named.length > 0 ? named : groups;
}

/** `data` as a session route naming a mission means it: the session's own
 *  records narrowed to that mission's run, every other session's kept (a
 *  run page reads its mission's other sessions too). Unchanged when the
 *  route names no mission, or one the session's records do not carry. The run page and the event log both read
 *  through this, so they show the run `sessionRun` heads. */
export function sessionRouteRecords<R extends NormRecord>(data: readonly R[], sessionId: string, missionId: string | null): R[] {
  const groups = routeGroups(data, sessionId, missionId);
  if (missionId === null || groups[0]?.missionId !== missionId) return data as R[];
  const own = new Set<NormRecord>(groups[0].records);
  return data.filter((r) => r.session_id !== sessionId || own.has(r));
}

/** A session route's run: of the route's groups (`routeGroups`), the one
 *  whose current attempt opened most recently as of `asOf`. A link naming
 *  a session only picks, when two missions share the id, the one that ran
 *  last, never a blend of both. `null` for a session with no records. */
export function sessionRun(data: readonly NormRecord[], sessionId: string, asOf: number, missionId: string | null = null): RunRecords | null {
  let best: RunRecords | null = null;
  for (const g of routeGroups(data, sessionId, missionId)) {
    const run = currentRun(g, asOf);
    if (best === null || openingRank(run, asOf) > openingRank(best, asOf)) best = run;
  }
  return best;
}

const loneCache = new WeakMap<readonly NormRecord[], RunGroup>();

/** A record set already scoped to ONE run (an execution's records, as the
 *  token-rate scope receives them) as a group of its own. */
export function groupOfRecords(records: readonly NormRecord[]): RunGroup {
  let g = loneCache.get(records);
  if (!g) {
    const first = records.find((r) => r.session_id) ?? records[0];
    const seg = segmentSession(records);
    g = makeGroup(first?.session_id ?? "", first?.mission_id ?? null, records, seg.attempts, seg.attempts);
    loneCache.set(records, g);
  }
  return g;
}
