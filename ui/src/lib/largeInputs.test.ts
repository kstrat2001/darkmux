// A spread into Math.max/Math.min passes every element as an argument, and
// past roughly 100k arguments the engine throws a RangeError. A day of a busy
// fleet's flow records is well past that, so every extremum over records or
// samples goes through a loop: `latestByTime`/`earliestByTime` for record
// times, `maxOf`/`minOf` for plain numbers. These inputs threw before.
import { describe, expect, it } from "vitest";
import { computeTMax, computeTMin, earliestRecordDate } from "./flow";
import { aggregateHostSamples } from "./hostStats";
import { ACTION, earliestByTime, latestByTime, type NormRecord } from "./ingest";
import { readyParts } from "./metaLine";
import { maxOf, minOf } from "./numbers";
import { perModelScale } from "../lenses/machine/memoryLedgerLines";
import type { MachineResourcesModel } from "../types/handwritten";

const N = 300_000;
const T0 = Date.parse("2026-09-01T00:00:00Z");

/** `N` dispatch starts one second apart, oldest first, built directly as
 *  normalized records so the fixture itself stays cheap. */
function manyStarts(): NormRecord[] {
  const out: NormRecord[] = [];
  for (let i = 0; i < N; i++) {
    out.push({ ts: new Date(T0 + i * 1000).toISOString(), tMs: T0 + i * 1000, action: ACTION.DispatchStart } as NormRecord);
  }
  return out;
}

describe("extrema over 300k inputs do not throw", () => {
  const records = manyStarts();

  it("the record-time picks read the first and last", () => {
    expect(latestByTime(records)?.tMs).toBe(T0 + (N - 1) * 1000);
    expect(earliestByTime(records)?.tMs).toBe(T0);
    expect(computeTMax(records)).toBe(T0 + (N - 1) * 1000);
    expect(computeTMin(records)).toBe(T0);
    expect(earliestRecordDate(records)).toBe("2026-09-01");
  });

  it("the ready line dates the newest dispatch start", () => {
    const live = new Map([["u1", { machine_uid: "u1" }]]) as unknown as Parameters<typeof readyParts>[1];
    const parts = readyParts(records, live, T0 + N * 1000);
    expect(parts?.ago).not.toBe("");
  });

  it("the host aggregate's high is the largest sample", () => {
    const points = Array.from({ length: N }, (_, i) => ({ cpu: i % 97, mem: 1, gpu: 2 }));
    expect(aggregateHostSamples(points).cpu.high).toBe(96);
  });

  it("the per-model scale is the largest footprint", () => {
    const models = Array.from({ length: N }, (_, i) => ({ potential_bytes: i, current_bytes: 0 }) as MachineResourcesModel);
    expect(perModelScale(models)).toBe(N - 1);
  });

  it("maxOf and minOf read the whole array", () => {
    const xs = Array.from({ length: N }, (_, i) => (i * 7919) % N);
    expect(maxOf(xs)).toBe(N - 1);
    expect(minOf(xs)).toBe(0);
  });
});

describe("maxOf and minOf on the edges", () => {
  it("an empty input has no extremum", () => {
    expect(maxOf([])).toBeUndefined();
    expect(minOf([])).toBeUndefined();
  });

  it("negative numbers and a single element", () => {
    expect(maxOf([-3, -1, -2])).toBe(-1);
    expect(minOf([-3, -1, -2])).toBe(-3);
    expect(maxOf([5])).toBe(5);
  });
});

describe("earliestByTime mirrors latestByTime", () => {
  const at = (tMs: number | null) => ({ tMs }) as NormRecord;

  it("an untimed record wins only when nothing is timed", () => {
    const untimed = at(null);
    expect(earliestByTime([untimed, at(5), at(3)])?.tMs).toBe(3);
    expect(earliestByTime([untimed])).toBe(untimed);
    expect(earliestByTime([])).toBeUndefined();
  });

  it("a tie keeps the first in arrival order", () => {
    const a = at(1);
    const b = at(1);
    expect(earliestByTime([a, b])).toBe(a);
  });
});
