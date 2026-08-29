import { describe, expect, it } from "vitest";
import { aggregateHostSamples, roundPct } from "./hostStats";

describe("aggregateHostSamples (#2107)", () => {
  it("computes now/avg/high/p95 per metric from a chronological sample list", () => {
    const points = [
      { cpu: 50, mem: 60, gpu: 70 },
      { cpu: 90, mem: 65, gpu: 85 },
      { cpu: 95, mem: 68, gpu: 90 },
      { cpu: 40, mem: 62, gpu: 30 },
      { cpu: 85, mem: 78, gpu: 95 },
    ];
    const agg = aggregateHostSamples(points);
    expect(agg.count).toBe(5);
    // now = the LAST sample, not the max — a run that spiked earlier and
    // settled must not report "now" as the spike.
    expect(agg.cpu.now).toBe(85);
    expect(agg.cpu.high).toBe(95);
    expect(agg.cpu.avg).toBeCloseTo(72.0, 5);
    expect(agg.cpu.p95).toBe(95);
    expect(agg.mem.avg).toBeCloseTo(66.6, 5);
    expect(agg.gpu.now).toBe(95);
    expect(agg.gpu.high).toBe(95);
  });

  it("a metric that never reported aggregates to all-null, not zero", () => {
    const agg = aggregateHostSamples([{ cpu: 10 }, { cpu: 20 }]);
    expect(agg.mem).toEqual({ now: null, avg: null, high: null, p95: null });
    expect(agg.gpu).toEqual({ now: null, avg: null, high: null, p95: null });
    expect(agg.cpu.now).toBe(20);
  });

  it("an empty sample list aggregates to all-null with count 0", () => {
    const agg = aggregateHostSamples([]);
    expect(agg.count).toBe(0);
    expect(agg.cpu).toEqual({ now: null, avg: null, high: null, p95: null });
  });

  it("a single sample is its own now/avg/high/p95", () => {
    const agg = aggregateHostSamples([{ cpu: 42, mem: 42, gpu: 42 }]);
    expect(agg.cpu).toEqual({ now: 42, avg: 42, high: 42, p95: 42 });
  });

  it("ignores non-finite / non-numeric fields per-tick without corrupting other metrics", () => {
    const agg = aggregateHostSamples([
      { cpu: 10, mem: Number.NaN as unknown as number },
      { cpu: 20, mem: 30 },
    ]);
    expect(agg.cpu.now).toBe(20);
    expect(agg.mem.now).toBe(30);
    expect(agg.mem.avg).toBe(30);
  });
});

describe("roundPct", () => {
  it("rounds to the nearest integer", () => {
    expect(roundPct(66.6)).toBe(67);
    expect(roundPct(66.4)).toBe(66);
  });

  it("passes null through rather than coercing to 0", () => {
    expect(roundPct(null)).toBeNull();
  });
});
