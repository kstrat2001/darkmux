import { describe, expect, it } from "vitest";
import { resolveDrawerScope, DRAWER_ROLLING_SCOPE_LABEL } from "./machineDrawerScope";
import type { FlowRecord } from "../types/handwritten";

const proc = (ts: string, cpu: number, machine_uid?: string): FlowRecord => ({
  ts,
  category: "telemetry",
  source: "process",
  action: "telemetry.process",
  machine_uid,
  payload: { cpu, mem: cpu, gpu: cpu },
});

describe("resolveDrawerScope (#2107)", () => {
  it("on a mission route, scopes to the mission's own route records and labels it", () => {
    const routeRecords = [proc("2026-01-01T00:00:00Z", 10), proc("2026-01-01T00:00:02Z", 20)];
    const s = resolveDrawerScope({ kind: "mission", missionId: "m1" }, routeRecords, [], null, Date.parse("2026-01-01T00:00:02Z"));
    expect(s.scopeLabel).toBe("this mission");
    expect(s.samples.map((p) => p.cpu)).toEqual([10, 20]);
  });

  it("on a dispatch route, scopes to the dispatch's own route records and labels it", () => {
    const routeRecords = [proc("2026-01-01T00:00:00Z", 5)];
    const s = resolveDrawerScope({ kind: "dispatch", dispatchId: "d1" }, routeRecords, [], null, Date.parse("2026-01-01T00:00:00Z"));
    expect(s.scopeLabel).toBe("this dispatch");
    expect(s.samples.map((p) => p.cpu)).toEqual([5]);
  });

  it("mission/dispatch route records are sorted chronologically regardless of input order", () => {
    const routeRecords = [proc("2026-01-01T00:00:04Z", 40), proc("2026-01-01T00:00:00Z", 0)];
    const s = resolveDrawerScope({ kind: "mission", missionId: "m1" }, routeRecords, [], null, 0);
    expect(s.samples.map((p) => p.cpu)).toEqual([0, 40]);
  });

  it("on every other route, rolls a last-10-minute window of the live tail", () => {
    const now = Date.parse("2026-01-01T00:20:00Z");
    const rolling = [
      proc("2026-01-01T00:05:00Z", 1), // 15 min ago — outside the window
      proc("2026-01-01T00:12:00Z", 2), // 8 min ago — inside
      proc("2026-01-01T00:19:00Z", 3), // 1 min ago — inside
    ];
    const s = resolveDrawerScope({ kind: "fleet" }, [], rolling, null, now);
    expect(s.scopeLabel).toBe(DRAWER_ROLLING_SCOPE_LABEL);
    expect(s.samples.map((p) => p.cpu)).toEqual([2, 3]);
  });

  it("the rolling window excludes another machine's samples when a local uid is known", () => {
    const now = Date.parse("2026-01-01T00:10:00Z");
    const rolling = [proc("2026-01-01T00:09:00Z", 99, "peer-machine"), proc("2026-01-01T00:09:30Z", 11, "this-machine")];
    const s = resolveDrawerScope({ kind: "console", panelId: "", opts: {} }, [], rolling, "this-machine", now);
    expect(s.samples.map((p) => p.cpu)).toEqual([11]);
  });

  it("with no known local uid, the rolling window does not filter by machine (best-effort default)", () => {
    const now = Date.parse("2026-01-01T00:10:00Z");
    const rolling = [proc("2026-01-01T00:09:00Z", 99, "peer-machine")];
    const s = resolveDrawerScope({ kind: "runs", runsKind: "all", run: null, machine: null }, [], rolling, null, now);
    expect(s.samples.map((p) => p.cpu)).toEqual([99]);
  });
});
