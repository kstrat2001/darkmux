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

function isTelemetryProcessRecord(r: FlowRecord): boolean {
  return r.action === "telemetry.process" || (r.category === "telemetry" && r.source === "process");
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
  return { cpu: num(f?.cpu), mem: num(f?.mem), gpu: num(f?.gpu) };
}

export interface DrawerScope {
  scopeLabel: string;
  samples: ProcSamplePoint[];
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
    .filter((r) => isTelemetryProcessRecord(r) && (uid == null || uidOf(r) === uid) && T(r.ts) >= cutoff && T(r.ts) <= nowMs)
    .sort((a, b) => T(a.ts) - T(b.ts))
    .map(toPoint);
}

export function resolveDrawerScope(
  route: Route,
  routeRecords: FlowRecord[],
  rollingWindow: FlowRecord[],
  localUid: string | null,
  nowMs: number,
): DrawerScope {
  if (route.kind === "mission" || route.kind === "dispatch") {
    const scoped = routeRecords.filter(isTelemetryProcessRecord).sort((a, b) => T(a.ts) - T(b.ts));
    return {
      scopeLabel: route.kind === "mission" ? "this mission" : "this dispatch",
      samples: scoped.map(toPoint),
    };
  }
  return { scopeLabel: DRAWER_ROLLING_SCOPE_LABEL, samples: rollingWindowSamples(rollingWindow, localUid, nowMs) };
}
