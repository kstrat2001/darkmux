import { describe, it, expect } from "vitest";
import { isPlainObject } from "./guards";

describe("isPlainObject (#2206/#2207)", () => {
  it("returns true for plain objects", () => {
    expect(isPlainObject({})).toBe(true);
    expect(isPlainObject({ a: 1 })).toBe(true);
    expect(isPlainObject(Object.create(null))).toBe(true);
  });

  it("returns true for NON-plain objects too — the documented naming caveat", () => {
    // Every original inline site accepted these; the shared form must
    // keep doing so. A caller wanting a literal-object check needs a
    // prototype test this function deliberately does not do.
    expect(isPlainObject(new Date())).toBe(true);
    expect(isPlainObject(new Map())).toBe(true);
    expect(isPlainObject(new Set())).toBe(true);
    expect(isPlainObject(/re/)).toBe(true);
    expect(isPlainObject(new (class Boxed {})())).toBe(true);
  });

  it("returns false for null, undefined, arrays, and every primitive", () => {
    expect(isPlainObject(null)).toBe(false);
    expect(isPlainObject(undefined)).toBe(false);
    expect(isPlainObject([])).toBe(false);
    expect(isPlainObject([1])).toBe(false);
    expect(isPlainObject("")).toBe(false);
    expect(isPlainObject("str")).toBe(false);
    expect(isPlainObject(0)).toBe(false);
    expect(isPlainObject(42)).toBe(false);
    expect(isPlainObject(true)).toBe(false);
    expect(isPlainObject(false)).toBe(false);
    expect(isPlainObject(NaN)).toBe(false);
    expect(isPlainObject(Symbol("s"))).toBe(false);
    expect(isPlainObject(0n)).toBe(false);
    expect(isPlainObject(() => {})).toBe(false);
  });

  it("agrees with BOTH original inline spellings over every falsy type and object-ish shape", () => {
    // The committed form of the equivalence evaluation the extraction
    // rested on (#2207). The two spellings replaced across
    // eventFilters.ts/flow.ts differ in their null test (`=== null` vs
    // `!v` falsy) — the claim is they agree everywhere, because of all
    // falsy values only `null` has `typeof === "object"`. Evaluated, not
    // reasoned about.
    const negativeStrict = (v: unknown) => v === null || typeof v !== "object" || Array.isArray(v);
    const negativeFalsy = (v: unknown) => !v || typeof v !== "object" || Array.isArray(v);
    const values: unknown[] = [
      null, undefined, false, true, 0, -0, 1, -1, NaN, "", "x", 0n, 1n,
      Symbol("s"), () => {}, [], [1], {}, { a: 1 }, Object.create(null),
      new Date(), /re/, new Map(), new Set(), new (class C {})(),
    ];
    for (const v of values) {
      expect(negativeStrict(v)).toBe(!isPlainObject(v));
      expect(negativeFalsy(v)).toBe(!isPlainObject(v));
    }
  });
});
