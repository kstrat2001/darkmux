/**
 * (#2915) darkmux's own UTILITY jobs, made visible.
 *
 * A utility job (compaction, radio routing; the list is the Rust enum
 * `darkmux_crew::usage::UtilityJobKind`, exported as `UtilityJobKind`) runs
 * lean: no session of its own, no dispatch bookends, no run, no presence
 * (#2914, the amended contract 2). It writes one `utility.start` flow record
 * when it starts (flow schema 1.61.0+: `job`, `model`, `serves` when it
 * serves an execution, `stall_after_seconds`), and its usage record
 * (`telemetry.tokens`, `purpose: "utility"`, `job`) marks the end. A routing
 * job whose model call failed ends with `utility.error` instead.
 *
 * This module is the ONLY place the viewer spells a job kind or those two
 * actions: `UTILITY_JOB` below is checked against the generated union by
 * `satisfies`, and `utilityJobs.conformance.test.ts` fails when a job-kind
 * literal appears anywhere else in `ui/src`. Job-specific visuals are keyed
 * by the enum (`utilityJobVisual`); a job this build has no visual for (a
 * newer darkmux's) gets the generic utility indicator, so a new job is never
 * silent.
 */

import type { UtilityJobKind } from "../types/generated/UtilityJobKind";
import { uidOf } from "./flow";
import { runIndex } from "./runRef";
import { CALL_KIND, PURPOSE, isUsageRecord, usagePurpose, type UsagePayload } from "./usageRecords";
import { mergeLive } from "./liveChannel";
import { ACTION, isAsOf, isDispatchTerminal, type NormAction, type NormRecord } from "./ingest";

/** Every `UtilityJobKind` variant, by name. A key missing or extra relative
 *  to the generated union is a type error. */
export const UTILITY_JOB = { compaction: "compaction", radio_routing: "radio_routing" } as const satisfies { readonly [K in UtilityJobKind]: K };

/** The bound a `utility.start` that carries none is held to: the runtime's
 *  default inactivity window (`runtime.inactivity_timeout_seconds`, 600 s).
 *  Every 1.61.0 start carries its own `stall_after_seconds`. */
export const UTILITY_JOB_DEFAULT_STALL_MS = 600_000;

const KNOWN: ReadonlySet<string> = new Set(Object.values(UTILITY_JOB));

/** True for a job kind this build knows (a generated variant). */
export function isKnownUtilityJob(v: unknown): v is UtilityJobKind {
  return typeof v === "string" && KNOWN.has(v);
}

/** How a job is drawn. Keyed by the enum; `generic` for any job this build
 *  has no visual for, so a new job still shows. */
export type UtilityJobVisual = "radio" | "compacting" | "generic";

const VISUAL: { readonly [K in UtilityJobKind]: UtilityJobVisual } = {
  compaction: "compacting",
  radio_routing: "radio",
};

export function utilityJobVisual(job: string | null): UtilityJobVisual {
  return isKnownUtilityJob(job) ? VISUAL[job] : "generic";
}

/** The word a job reads as on the fleet card's strip and the machine page.
 *  A job this build does not know reads as its own wire name (spaced), so
 *  the operator still learns what it is. */
const WORD: { readonly [K in UtilityJobKind]: string } = {
  compaction: "compacting",
  radio_routing: "radio routing",
};

export function utilityJobWord(job: string | null): string {
  if (isKnownUtilityJob(job)) return WORD[job];
  return job ? job.replace(/_/g, " ") : "utility job";
}

type Payload = Record<string, unknown>;

function payloadOf(r: NormRecord): Payload {
  const x = r as unknown as { payload?: Payload; fields?: Payload };
  return x.payload ?? x.fields ?? {};
}

export function isUtilityStart(r: NormRecord): boolean {
  return r.action === ACTION.UtilityStart;
}

/** The job a utility record names: its `job`, or, for a compactor call's
 *  usage record from before 1.61.0 (no `job`), the compaction job. `null`
 *  when it names none. */
export function utilityJobOf(r: NormRecord): string | null {
  const p = payloadOf(r);
  if (typeof p.job === "string" && p.job) return p.job;
  if (isUsageRecord(r) && (p as UsagePayload).call_kind === CALL_KIND.compaction) return UTILITY_JOB.compaction;
  return null;
}

/** True when `r` ENDS a utility job: a utility usage record, or a
 *  `utility.error`. */
export function isUtilityEnd(r: NormRecord): boolean {
  if (r.action === ACTION.UtilityError || r.action === ACTION.UtilityEnd) return true;
  return isUsageRecord(r) && usagePurpose(payloadOf(r) as UsagePayload) === PURPOSE.utility;
}

/** A served execution's records that prove it moved past a compaction whose
 *  calls all failed (no usage record comes then): its next turn's opener,
 *  a turn end, a tool, a rest, an installed compaction, or its end. */
const MOVED_ON: ReadonlySet<NormAction> = new Set<NormAction>([
  ACTION.DispatchTurnHeartbeat,
  ACTION.DispatchTurn,
  ACTION.DispatchTool,
  ACTION.DispatchRest,
  ACTION.DispatchCompaction,
  ACTION.DispatchStart,
  ACTION.DispatchComplete,
  ACTION.DispatchError,
]);
function executionMovedOn(action: NormAction | undefined): boolean {
  return action !== undefined && MOVED_ON.has(action);
}

/** A utility job that has started and not ended, as of `nowMs`. */
export interface LiveUtilityJob {
  /** The job's wire name: a `UtilityJobKind`, or an unknown string. */
  job: string;
  known: boolean;
  sinceMs: number;
  /** The job's own bound (`stall_after_seconds`), else the default. */
  stallAfterMs: number;
  /** No end within the bound: reads STALL. */
  stalled: boolean;
  model: string | null;
  /** The session of the execution it serves (a compaction), else `null`. */
  serves: string | null;
}

interface Open {
  job: string;
  jobId: string | null;
  atMs: number;
  session: string | null;
  stallAfterMs: number;
  model: string | null;
}

function msField(p: Payload, key: string): number | null {
  const v = p[key];
  return typeof v === "number" && Number.isFinite(v) && v > 0 ? v : null;
}

/** (#2915 review, C3/C4) When a record happened, at the best precision it
 *  carries. A flow `ts` is whole-second, so a utility marker's own
 *  `started_at_ms` / `ended_at_ms` and a heartbeat's `sampled_at_ms` win. A
 *  whole-second TERMINAL is read as the END of its second: it ends
 *  everything its execution started in that second (a heartbeat without a
 *  sample time keeps its `ts`, so a same-second one does not). */
function whenMs(r: NormRecord, p: Payload): number | null {
  const precise = preciseMs(r, p);
  if (precise !== null || r.tMs === null) return precise;
  return isDispatchTerminal(r.action) ? r.tMs + 999 : r.tMs;
}

function preciseMs(r: NormRecord, p: Payload): number | null {
  return msField(p, isUtilityStart(r) ? "started_at_ms" : isUtilityEnd(r) ? "ended_at_ms" : "sampled_at_ms");
}

/** `records` as of `nowMs`, in time order, each with its payload and best
 *  time. The as-of cut is the shared one (`isAsOf`), tightened by a precise
 *  field when the record carries one. A record with no time at all is kept
 *  and sequenced last (the bad-timestamp policy), with `atMs: null`. */
function sequenced(records: readonly NormRecord[], nowMs: number): { r: NormRecord; p: Payload; atMs: number | null }[] {
  return records
    .map((r) => {
      const p = payloadOf(r);
      return { r, p, atMs: whenMs(r, p), precise: preciseMs(r, p) };
    })
    .filter((x) => (x.precise !== null ? x.precise <= nowMs : isAsOf(x.r, nowMs)))
    .sort((a, b) => (a.atMs === null ? (b.atMs === null ? 0 : 1) : b.atMs === null ? -1 : a.atMs - b.atMs));
}

/** Every job open as of `nowMs` in `records` (one machine's, or one
 *  execution's), oldest first.
 *
 *  - An end that names a `job_id` (flow schema 1.61.0) closes the start with
 *    that id. An end without one (an older writer) closes the most recent
 *    open start of its job in its scope (session, or none for routing).
 *  - Either way, closing a start also DROPS every older open start of the
 *    same job in the same scope: the utility instance serves requests in
 *    order, so a job that started earlier and has not ended by now never
 *    will (a routing call killed mid-flight). One orphan must not read
 *    "radio routing", then "stalled", for the rest of the window.
 *  - A job that serves an execution also ends when that execution moves on
 *    (its next turn's opener, a turn end, a tool, a rest, an installed
 *    compaction) or ends (a terminal, even in the start's own second). */
export function openUtilityJobs(records: readonly NormRecord[], nowMs: number): LiveUtilityJob[] {
  const ordered = sequenced(records, nowMs);
  let open: Open[] = [];
  const closeWithOlder = (i: number) => {
    const closed = open[i];
    open = open.filter((o, k) => k !== i && !(o.job === closed.job && o.session === closed.session && o.atMs <= closed.atMs));
  };
  for (const { r, p, atMs: at } of ordered) {
    // An untimed start opens as of `nowMs`, the only instant it is known open.
    const atMs = at ?? nowMs;
    const session = r.session_id || null;
    if (isUtilityStart(r)) {
      const job = typeof p.job === "string" && p.job ? p.job : "";
      const jobId = typeof p.job_id === "string" && p.job_id ? p.job_id : null;
      const bound = typeof p.stall_after_seconds === "number" && p.stall_after_seconds > 0 ? p.stall_after_seconds * 1000 : UTILITY_JOB_DEFAULT_STALL_MS;
      const model = typeof p.model === "string" && p.model ? p.model : r.model || null;
      open.push({ job, jobId, atMs, session, stallAfterMs: bound, model });
    } else if (isUtilityEnd(r)) {
      const jobId = typeof p.job_id === "string" && p.job_id ? p.job_id : null;
      const job = utilityJobOf(r);
      let i = jobId !== null ? open.findIndex((o) => o.jobId === jobId) : -1;
      if (i < 0) {
        // No id, or its start is outside the window: the most recent open
        // start of this job in this scope.
        for (let k = open.length - 1; k >= 0; k--) {
          if (open[k].session === session && (job === null || open[k].job === job)) {
            i = k;
            break;
          }
        }
      }
      if (i >= 0) closeWithOlder(i);
    } else if (session && (executionMovedOn(r.action))) {
      open = open.filter((o) => o.session !== session);
    }
  }
  return open.map((o) => ({
    job: o.job,
    known: isKnownUtilityJob(o.job),
    sinceMs: o.atMs,
    stallAfterMs: o.stallAfterMs,
    stalled: nowMs - o.atMs > o.stallAfterMs,
    model: o.model,
    serves: o.session,
  }));
}

/** The machine's live utility job as of `nowMs` (`records` already narrowed
 *  to one machine): the most recently started open one, else `null`
 *  (quiet). */
export function machineUtilityJob(records: readonly NormRecord[], nowMs: number): LiveUtilityJob | null {
  const open = openUtilityJobs(records, nowMs);
  return open.length ? open[open.length - 1] : null;
}

/** The latest utility model a machine's records name (a start's `model`, or
 *  a utility usage record's `requested_model`), for a machine whose own
 *  binding the viewer cannot ask (a fleet peer). `null` when none. */
export function lastUtilityModel(records: readonly NormRecord[], nowMs: number): string | null {
  let best: { atMs: number; model: string } | null = null;
  for (const r of records) {
    if (!isAsOf(r, nowMs)) continue;
    // An untimed record wins only when nothing timed does (the bad-timestamp policy).
    const atMs = r.tMs ?? -Infinity;
    let model: unknown = null;
    if (isUtilityStart(r)) model = payloadOf(r).model;
    else if (isUsageRecord(r) && usagePurpose(payloadOf(r) as UsagePayload) === PURPOSE.utility) model = payloadOf(r).requested_model;
    if (typeof model === "string" && model && (!best || atMs >= best.atMs)) best = { atMs, model };
  }
  return best?.model ?? null;
}

/** One job's usage over a window. `job` is `null` for utility records that
 *  name no job (a routing record from before 1.61.0). */
export interface UtilityJobUsage {
  job: string | null;
  known: boolean;
  calls: number;
  tokens: number;
}

function tokensOf(p: Payload): number {
  const n = (v: unknown) => (typeof v === "number" && Number.isFinite(v) && v > 0 ? Math.floor(v) : 0);
  return n(p.total_tokens) || n(p.prompt_tokens) + n(p.completion_tokens);
}

/** Each utility job's calls and tokens, from its usage records: every known
 *  job, in enum order, even at zero (so the list is the same shape whatever
 *  ran), then any job this build does not know, then the unnamed ones. */
export function utilityUsageByJob(records: readonly NormRecord[]): UtilityJobUsage[] {
  const by = new Map<string | null, UtilityJobUsage>();
  for (const job of Object.values(UTILITY_JOB)) by.set(job, { job, known: true, calls: 0, tokens: 0 });
  for (const r of records) {
    if (!isUsageRecord(r)) continue;
    const p = payloadOf(r);
    if (usagePurpose(p as UsagePayload) !== PURPOSE.utility) continue;
    const job = utilityJobOf(r);
    const row = by.get(job) ?? { job, known: isKnownUtilityJob(job), calls: 0, tokens: 0 };
    row.calls += 1;
    row.tokens += tokensOf(p);
    by.set(job, row);
  }
  const rows = [...by.values()];
  const rank = (u: UtilityJobUsage) => (u.known ? 0 : u.job !== null ? 1 : 2);
  return rows.sort((a, b) => rank(a) - rank(b));
}

/** Per data array: each machine's utility records (starts and ends), built
 *  once per array like `sessionRecords`' index, so a fleet card's strip is
 *  not a scan of the whole window per card per tick. */
const machineIndexCache = new WeakMap<readonly NormRecord[], Map<string, NormRecord[]>>();

function machineUtilityRecords(data: readonly NormRecord[], uid: string): NormRecord[] {
  let index = machineIndexCache.get(data);
  if (!index) {
    index = new Map();
    for (const r of data) {
      if (!r || !(isUtilityStart(r) || isUtilityEnd(r))) continue;
      const k = uidOf(r);
      const list = index.get(k);
      if (list) list.push(r);
      else index.set(k, [r]);
    }
    machineIndexCache.set(data, index);
  }
  return index.get(uid) ?? [];
}

/** (#2915) A fleet card's utility strip: the machine's utility model, whether
 *  it is resident (`null`: the viewer cannot tell, a peer), and its live job
 *  with the visual that draws it (`null`: quiet). */
export interface UtilityStrip {
  model: string | null;
  resident: boolean | null;
  job: (LiveUtilityJob & { visual: UtilityJobVisual; word: string }) | null;
}

/** The strip for machine `uid` as of `t`, over the whole window `data`.
 *  `binding` is this machine's own `internal.utility` as `/machine/specs`
 *  reports it, when `uid` is the machine the page is served from. */
export function utilityStrip(
  data: readonly NormRecord[],
  uid: string,
  t: number,
  binding: { id: string; loaded: boolean } | null,
  /** (#2928) This machine's live utility edges (`LiveOverlay.utility`), for
   *  the machine the page is served from only: the live channel is
   *  local-daemon only. A sub-second job shows open while it runs, instead
   *  of arriving start-and-end in one durable delivery. Durable edges win
   *  (`mergeLive`). */
  liveEdges: readonly NormRecord[] = [],
): UtilityStrip {
  const own = mergeLive(machineUtilityRecords(data, uid), liveEdges);
  // A served execution's own records can end its compaction (the execution
  // moved on); pull them in, once each.
  const served = new Set<string>();
  for (const r of own) if (isUtilityStart(r) && r.session_id) served.add(r.session_id);
  let recs: readonly NormRecord[] = own;
  if (served.size) {
    const seen = new Set<NormRecord>(own);
    const extra: NormRecord[] = [];
    const ix = runIndex(data);
    for (const sid of served) for (const g of ix.groupsOfSession(sid)) for (const r of g.records) if (!seen.has(r)) extra.push(r);
    recs = [...own, ...extra];
  }
  const live = machineUtilityJob(recs, t);
  return {
    model: binding?.id ?? lastUtilityModel(own, t),
    resident: binding ? binding.loaded : null,
    job: live ? { ...live, visual: utilityJobVisual(live.job), word: utilityJobWord(live.job) } : null,
  };
}
