// The executable specification for the lab lens's pure logic.
//
// This file used to also cover `labFieldVal`/`labTaskKey`/
// `groupLabRunsByTask`/`labKnobSummary`/`labKnobDiff` — the pure logic
// behind the runs board's `◧ series` knob-diff sub-view, removed in the
// #2860 follow-up (see `../runs/RunsBoard.tsx`'s own module doc and
// `./labSeries.ts`'s own doc for why). `shortModel` alone survives.
import { describe, it, expect } from "vitest";
import { shortModel } from "./labSeries";

describe("shortModel", () => {
  it("strips the darkmux namespace prefix", () => {
    expect(shortModel("darkmux:qwen3.6-35b-a3b")).toBe("qwen3.6-35b-a3b");
  });
  it("leaves a non-namespaced id alone", () => {
    expect(shortModel("qwen3.6-35b-a3b")).toBe("qwen3.6-35b-a3b");
  });
  it("strips only a LEADING prefix, not an embedded one", () => {
    expect(shortModel("vendor/darkmux:x")).toBe("vendor/darkmux:x");
  });
  it("renders an absent model as the empty string, never 'null'/'undefined'", () => {
    expect(shortModel(null)).toBe("");
    expect(shortModel(undefined)).toBe("");
  });
});
