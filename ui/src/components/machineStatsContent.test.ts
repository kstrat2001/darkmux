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

  // (#2108 review finding 7) `window.{cpu,gpu,mem}_pct` are typed as
  // always-present `MachineLoadMetric`s, but a pre-#2108 v1-shaped daemon
  // reply — `window` present, its sub-fields keyed the OLD way — can hand
  // this function a `load` where every one of those keys is simply absent.
  // The type only binds the compiler; at runtime this is a plain object
  // literal, so `as unknown as MachineLoad` here is testing exactly the
  // shape a real fetch response can produce, not a contrived impossible
  // value. Must not throw, and must degrade every reading to null/0 (this
  // file's own absence-never-zero convention) rather than crash.
  it("a v1-shaped window (missing cpu_pct/gpu_pct/mem_pct/samples entirely) degrades to null/0 instead of throwing", () => {
    const v1Load = {
      now: LOAD.now,
      window: {
        // v1's own naming — no `cpu_pct`/`gpu_pct`/`mem_pct`/`samples` keys
        // at all, just the old nested `cpu`/`gpu`/`mem` shape.
        cpu: { mean_pct: 10, peak_pct: 20, p95_pct: 15 },
        gpu: { mean_pct: 50, peak_pct: 60, p95_pct: 55 },
        mem: { mean_pct: 30, peak_pct: 40, p95_pct: 35 },
        span_ms: 90_000,
      },
    } as unknown as MachineLoad;

    expect(() => effectiveHostAggregate(false, DISPATCH_AGG, v1Load)).not.toThrow();
    const agg = effectiveHostAggregate(false, DISPATCH_AGG, v1Load);
    expect(agg.cpu).toEqual({ now: 11, avg: null, high: null, p95: null });
    expect(agg.mem).toEqual({ now: 22, avg: null, high: null, p95: null });
    expect(agg.gpu).toEqual({ now: 33, avg: null, high: null, p95: null });
    expect(agg.count).toBe(0);
  });

  // A `window` missing ENTIRELY (an even older/malformed shape) must
  // degrade the same way, not throw on `load.window.cpu_pct` reading off
  // `undefined`.
  it("a `load` with no `window` at all degrades to null/0 instead of throwing", () => {
    const noWindowLoad = { now: LOAD.now } as unknown as MachineLoad;
    expect(() => effectiveHostAggregate(false, DISPATCH_AGG, noWindowLoad)).not.toThrow();
    const agg = effectiveHostAggregate(false, DISPATCH_AGG, noWindowLoad);
    expect(agg.cpu).toEqual({ now: 11, avg: null, high: null, p95: null });
    expect(agg.count).toBe(0);
  });
});
