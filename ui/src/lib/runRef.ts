/**
 * Which records belong to a run: the viewer's one answer.
 *
 * A run is a `(session_id, mission_id)` PAIR, never a bare session id. The
 * scheduler's session ids are deterministic (`task-<task_id>`, a review's
 * `task-review-probe-mid-task`), so two missions routinely share one id; a
 * lookup by session alone pairs one mission's start with another's end
 * (#2125). The pair is the `RunGroup`; within it, each relaunch under the
 * same id is an `Attempt` (see `lifecycle.ts`'s `attemptsOf`); a `RunRef`
 * names one attempt of one group.
 *
 * Attribution. A record naming a `mission_id` belongs to that mission's
 * group. A record naming none (a `session.end`, some bookends) belongs to
 * its session's mission when the session has exactly ONE; when it has
 * several, the record is kept apart in a `(session, null)` group of its own
 * rather than guessed into one of them.
 *
 * The index is built once per window ARRAY (a `WeakMap` on its identity):
 * a window array is never mutated after it is first read, and every
 * per-run lookup after the first is a map read.
 */

import { ACTION, isAsOf, type NormRecord } from "./ingest";
import { attemptsOf, type Attempt } from "./lifecycle";

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
  readonly attempts: readonly Attempt[];
}

/** One run's records: what every lifecycle and grain predicate takes. */
export interface RunRecords {
  readonly ref: RunRef;
  readonly group: RunGroup;
  /** The named attempt; `null` when the group has none (nothing opened). */
  readonly attempt: Attempt | null;
  /** The attempt after it, whose opening supersedes this one. */
  readonly next: Attempt | null;
}

/** What kind of unit a group is (contract 8's grains):
 *  - `run`: a mission's whole-run bookend (`dispatch.start` sourced
 *    `mission`, or the retired review launcher's `review`);
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

const RUN_GRAIN_SOURCES: ReadonlySet<string> = new Set([
  "mission",
  // (#2881) The retired review launcher's whole-run bookend source (deleted
  // in #2310 P4d). Archives are append-only (contract 8): readers stay
  // bilingual.
  "review",
]);

const EXECUTION_EVIDENCE: ReadonlySet<unknown> = new Set([
  ACTION.BudgetWait,
  ACTION.DispatchTurnHeartbeat,
  ACTION.DispatchTurn,
  ACTION.DispatchTool,
  ACTION.DispatchRest,
]);

function grainOfRecord(r: NormRecord): Grain {
  if (r.action === ACTION.DispatchStart) return RUN_GRAIN_SOURCES.has(r.source ?? "") ? "run" : "execution";
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

/** Each session's distinct mission ids. */
function missionsBySession(data: readonly NormRecord[]): Map<string, Set<string>> {
  const out = new Map<string, Set<string>>();
  for (const r of data) {
    if (!r.session_id || !r.mission_id) continue;
    let set = out.get(r.session_id);
    if (!set) out.set(r.session_id, (set = new Set()));
    set.add(r.mission_id);
  }
  return out;
}

/** The mission a record is attributed to (the module doc's rule). */
function attributedMission(r: NormRecord, missions: Map<string, Set<string>>): string | null {
  if (r.mission_id) return r.mission_id;
  const only = missions.get(r.session_id ?? "");
  return only && only.size === 1 ? [...only][0] : null;
}

interface Building {
  key: string;
  sessionId: string;
  missionId: string | null;
  records: NormRecord[];
  uids: Set<string>;
}

function collect(data: readonly NormRecord[]): { byKey: Map<string, Building>; keyOf: Map<NormRecord, string> } {
  const missions = missionsBySession(data);
  const byKey = new Map<string, Building>();
  const keyOf = new Map<NormRecord, string>();
  for (const r of data) {
    if (!r || !r.session_id) continue;
    const missionId = attributedMission(r, missions);
    const key = groupKey(r.session_id, missionId);
    let b = byKey.get(key);
    if (!b) byKey.set(key, (b = { key, sessionId: r.session_id, missionId, records: [], uids: new Set() }));
    b.records.push(r);
    b.uids.add(r.machine_uid || "unknown");
    keyOf.set(r, key);
  }
  return { byKey, keyOf };
}

function pushTo<K>(m: Map<K, RunGroup[]>, k: K, g: RunGroup): void {
  const list = m.get(k);
  if (list) list.push(g);
  else m.set(k, [g]);
}

const NO_GROUPS: readonly RunGroup[] = [];

function buildIndex(data: readonly NormRecord[]): RunIndex {
  const { byKey, keyOf } = collect(data);
  const groups: RunGroup[] = [];
  const byGroupKey = new Map<string, RunGroup>();
  const byUid = new Map<string, RunGroup[]>();
  const bySession = new Map<string, RunGroup[]>();
  const byMission = new Map<string, RunGroup[]>();
  for (const b of byKey.values()) {
    const g: RunGroup = { key: b.key, sessionId: b.sessionId, missionId: b.missionId, records: b.records, attempts: attemptsOf(b.records) };
    groups.push(g);
    byGroupKey.set(g.key, g);
    for (const uid of b.uids) pushTo(byUid, uid, g);
    pushTo(bySession, g.sessionId, g);
    if (g.missionId) pushTo(byMission, g.missionId, g);
  }
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
  return { ref, group, attempt: group.attempts[ref.attempt] ?? null, next: group.attempts[ref.attempt + 1] ?? null };
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

/** A session route's run: of the session's groups, the one whose current
 *  attempt opened most recently as of `asOf`. A `#dispatch=<id>` link names
 *  a session only, so when two missions share the id this picks the one that
 *  ran last, never a blend of both. `null` for a session with no records. */
export function sessionRun(data: readonly NormRecord[], sessionId: string, asOf: number): RunRecords | null {
  let best: RunRecords | null = null;
  for (const g of runIndex(data).groupsOfSession(sessionId)) {
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
    const sessionId = first?.session_id ?? "";
    const missionId = first?.mission_id ?? null;
    g = { key: groupKey(sessionId, missionId), sessionId, missionId, records, attempts: attemptsOf(records) };
    loneCache.set(records, g);
  }
  return g;
}
