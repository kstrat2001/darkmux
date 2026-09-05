/**
 * (#2107) Pure scope resolution for the global machine drawer/modal — which
 * `telemetry.process` samples the CPU/GPU/MEM meters aggregate, and what to
 * call that window.
 *
 * Two windows, per the operator's own scope rule:
 * - On a mission (`#mission=<id>`) or dispatch (`#dispatch=<sid>`) route,
 *   the window is THAT mission's/dispatch's own samples — live while
 *   running, and naturally frozen once it stops (no new samples arrive, so
 *   the aggregate simply stops changing; no separate "frozen" flag needed).
 * - On every other route, the window is a ROLLING last-10-minutes tail of
 *   the live flow window, scoped to the local machine only (a hub serving
 *   a fleet's flow stream carries every peer's `telemetry.process` records
 *   too — the pill reads "machine", singular, so it must not average
 *   another box's GPU into this one's number).
 *
 * Kept separate from the React component so the scope decision is testable
 * without rendering anything.
 */
import type { FlowRecord } from "../types/handwritten";
import type { Route } from "./route";
import { T, uidOf } from "./flow";
import type { ProcSamplePoint } from "./hostStats";

export const DRAWER_ROLLING_WINDOW_MS = 10 * 60 * 1000;
export const DRAWER_ROLLING_SCOPE_LABEL = "last 10 min";

/** How far back `findLastKnownSample` will look for a stray older reading
 * before giving up and reporting "never measured" rather than "measured
 * ages ago" — a genuinely day(s)-old record from a stale flow file
 * shouldn't be reported as if it just happened to be quiet. */
const LAST_KNOWN_LOOKBACK_MS = 24 * 60 * 60 * 1000;

/** (#2413) Accepts BOTH the retired per-dispatch `telemetry.process`
 * (still emitted, at time of writing, by the separate legacy
 * `run_obs::HostTelemetrySampler` mechanism used by mission launches and
 * ACP sessions — see `FLOW_SCHEMA_VERSION` 1.42.0's changelog) and the new
 * machine-scoped `machine.telemetry` (`source: "host"`, no session_id) —
 * so a machine still running an older/partial rollout, or a mission/ACP
 * session whose own telemetry hasn't migrated yet, keeps showing SOMETHING
 * rather than going blank the moment this ships. */
function isHostSampleRecord(r: FlowRecord): boolean {
  return (
    r.action === "telemetry.process" ||
    r.action === "machine.telemetry" ||
    (r.category === "telemetry" && r.source === "process")
  );
}

function toPoint(r: FlowRecord): ProcSamplePoint {
  // Raw route/window records carry `payload`; a normalized render model
  // (`flowToRenderModel`) renames it to `fields` — accept either so this
  // works against whichever shape a caller hands it.
  const f = (r.payload ?? r.fields) as Record<string, unknown> | undefined;
  const num = (v: unknown): number | undefined => {
    const n = Number(v);
    return Number.isFinite(n) ? n : undefined;
  };
  // (#2413) `machine.telemetry` carries the full `host_probe` shape
  // (`cpu_pct`/`mem_pct`/`gpu_pct`); the retired `telemetry.process`
  // carried bare `cpu`/`mem`/`gpu`. Prefer the new keys, fall back to the
  // old ones, so either record shape resolves to the same point.
  return {
    cpu: num(f?.cpu_pct ?? f?.cpu),
    mem: num(f?.mem_pct ?? f?.mem),
    gpu: num(f?.gpu_pct ?? f?.gpu),
  };
}

export interface LastKnownSample {
  point: ProcSamplePoint;
  /** The sample's own timestamp, in ms — formatted by the caller (e.g.
   * `lib/format.ts::relAgoFrom(nowMs, ts)`) rather than pre-rendered here,
   * so this module stays pure data, no locale/wording decisions. */
  ts: number;
}

export interface DrawerScope {
  scopeLabel: string;
  samples: ProcSamplePoint[];
  /** (#2107 phone feedback) Set only when `samples` is empty on the
   * ROLLING branch — a dashboard reading "— avg — max" three times over
   * with no other information is not a state, it is a missing one. `null`
   * on the mission/dispatch branch always (that scope IS the dispatch's
   * own full record set already — there is no separate "last known"
   * outside it) and on the rolling branch when nothing was ever seen for
   * this machine within the lookback window either. */
  lastKnown: LastKnownSample | null;
}

/** The rolling-last-10-minutes slice, scoped to one machine uid (or
 * unfiltered when `uid` is unresolved). Factored out of
 * [[resolveDrawerScope]] so `MachineLens.tsx`'s own live-stats section
 * (#1833) can use the SAME window without faking a `Route` — that lens
 * always wants "this machine, last 10 min" regardless of which app route
 * is current, since IT is what the route names. */
export function rollingWindowSamples(records: FlowRecord[], uid: string | null, nowMs: number): ProcSamplePoint[] {
  const cutoff = nowMs - DRAWER_ROLLING_WINDOW_MS;
  return records
    .filter((r) => isHostSampleRecord(r) && (uid == null || uidOf(r) === uid) && T(r.ts) >= cutoff && T(r.ts) <= nowMs)
    .sort((a, b) => T(a.ts) - T(b.ts))
    .map(toPoint);
}

/** The single most recent `telemetry.process` sample for `uid` anywhere in
 * `records`, regardless of the rolling window's 10-minute cutoff — the
 * sampler only runs DURING a dispatch (#557/#1064's own doc), so an idle
 * machine's rolling window is legitimately empty most of the time, and
 * "no reading in the last 10 min" is a very different claim from "never
 * measured at all". Bounded by `LAST_KNOWN_LOOKBACK_MS` so a genuinely
 * stale record doesn't get reported as if it just happened. */
export function findLastKnownSample(records: FlowRecord[], uid: string | null, nowMs: number): LastKnownSample | null {
  let best: { r: FlowRecord; ts: number } | null = null;
  for (const r of records) {
    if (!isHostSampleRecord(r)) continue;
    if (uid != null && uidOf(r) !== uid) continue;
    const ts = T(r.ts);
    if (!Number.isFinite(ts) || ts > nowMs) continue;
    if (nowMs - ts > LAST_KNOWN_LOOKBACK_MS) continue;
    if (best == null || ts > best.ts) best = { r, ts };
  }
  return best ? { point: toPoint(best.r), ts: best.ts } : null;
}

export function resolveDrawerScope(
  route: Route,
  routeRecords: FlowRecord[],
  rollingWindow: FlowRecord[],
  localUid: string | null,
  nowMs: number,
): DrawerScope {
  if (route.kind === "mission" || route.kind === "dispatch") {
    const scoped = routeRecords.filter(isHostSampleRecord).sort((a, b) => T(a.ts) - T(b.ts));
    return {
      scopeLabel: route.kind === "mission" ? "this mission" : "this dispatch",
      samples: scoped.map(toPoint),
      lastKnown: null,
    };
  }
  const samples = rollingWindowSamples(rollingWindow, localUid, nowMs);
  return {
    scopeLabel: DRAWER_ROLLING_SCOPE_LABEL,
    samples,
    lastKnown: samples.length === 0 ? findLastKnownSample(rollingWindow, localUid, nowMs) : null,
  };
}
