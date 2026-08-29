import { describe, expect, it } from "vitest";
import { effectiveHostAggregate } from "./machineStatsContent";
import type { HostAggregate } from "../lib/hostStats";
import type { MachineLoad } from "../types/handwritten";

const EMPTY_METRIC = { now: null, avg: null, high: null, p95: null };

const DISPATCH_AGG: HostAggregate = {
  cpu: { now: 10, avg: 20, high: 30, p95: 25 },
  mem: { now: 40, avg: 50, high: 60, p95: 55 },
  gpu: { now: 70, avg: 80, high: 90, p95: 85 },
  count: 3,
};

const LOAD: MachineLoad = {
  now: {
    sampled_at_ms: 4000,
    sampler_cost_ms: 6.3,
    cpu_pct: 11,
    cpu_clusters: null,
    mem_pct: 22,
    gpu_pct: 33,
    gpu_mhz: null,
    gpu_mem_bytes: null,
    thermal: null,
    power_mw: null,
  },
  window: {
    samples: 5,
    interval_ms: 2000,
    span_ms: 8000,
    cpu_pct: { mean: 12.5, p95: 15, max: 20 },
    mem_pct: { mean: 42.5, p95: 45, max: 50 },
    gpu_pct: { mean: 62.5, p95: 65, max: 70 },
    power_mw: null,
    thermal: null,
    energy_mwh: null,
  },
};

describe("effectiveHostAggregate (#2107, #1833)", () => {
  it("with no daemon load at all, falls back to the dispatch aggregate unchanged, on every route", () => {
    expect(effectiveHostAggregate(false, DISPATCH_AGG, null)).toBe(
      DISPATCH_AGG,
    );
    expect(effectiveHostAggregate(true, DISPATCH_AGG, null)).toBe(DISPATCH_AGG);
  });

  it("on a mission/dispatch route, keeps the dispatch's own avg/high/p95 and overrides ONLY `now` with the daemon's reading", () => {
    const agg = effectiveHostAggregate(true, DISPATCH_AGG, LOAD);
    expect(agg.cpu).toEqual({ now: 11, avg: 20, high: 30, p95: 25 });
    expect(agg.mem).toEqual({ now: 22, avg: 50, high: 60, p95: 55 });
    expect(agg.gpu).toEqual({ now: 33, avg: 80, high: 90, p95: 85 });
    // count stays the dispatch's own sample count
    expect(agg.count).toBe(DISPATCH_AGG.count);
  });

  it("on every other route, the daemon's window IS the aggregate — avg/max/p95/now all come from `load`, not the dispatch samples", () => {
    const agg = effectiveHostAggregate(false, DISPATCH_AGG, LOAD);
    expect(agg.cpu).toEqual({ now: 11, avg: 12.5, high: 20, p95: 15 });
    expect(agg.mem).toEqual({ now: 22, avg: 42.5, high: 50, p95: 45 });
    expect(agg.gpu).toEqual({ now: 33, avg: 62.5, high: 70, p95: 65 });
    // count reflects the daemon window's own sample count, not the dispatch's
    expect(agg.count).toBe(5);
  });

  it("a metric the daemon never read (null now) still overrides — absence is a real claim, not silently kept from the dispatch side", () => {
    const partialLoad: MachineLoad = {
      ...LOAD,
      now: { ...LOAD.now, cpu_pct: null },
    };
    const agg = effectiveHostAggregate(true, DISPATCH_AGG, partialLoad);
    expect(agg.cpu.now).toBeNull();
    // avg is untouched — this is a mission/dispatch route
    expect(agg.cpu.avg).toBe(20);
  });

  it("a fully-empty dispatch aggregate on a mission route still picks up the daemon's now", () => {
    const emptyDispatch: HostAggregate = {
      cpu: EMPTY_METRIC,
      mem: EMPTY_METRIC,
      gpu: EMPTY_METRIC,
      count: 0,
    };
    const agg = effectiveHostAggregate(true, emptyDispatch, LOAD);
    expect(agg.cpu).toEqual({ now: 11, avg: null, high: null, p95: null });
    expect(agg.count).toBe(0);
  });
});
