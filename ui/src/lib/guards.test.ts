import { describe, it, expect } from "vitest";
import { isPlainObject } from "./guards";

describe("isPlainObject", () => {
  it("returns true for plain objects", () => {
    expect(isPlainObject({})).toBe(true);
    expect(isPlainObject({ a: 1 })).toBe(true);
    expect(isPlainObject(Object.create(null))).toBe(true);
  });

  it("returns false for null, undefined, arrays, strings, numbers, booleans", () => {
    expect(isPlainObject(null)).toBe(false);
    expect(isPlainObject(undefined)).toBe(false);
    expect(isPlainObject([])).toBe(false);
    expect(isPlainObject([1])).toBe(false);
    expect(isPlainObject("")).toBe(false);
    expect(isPlainObject("str")).toBe(false);
    expect(isPlainObject(0)).toBe(false);
    expect(isPlainObject(42)).toBe(false);
    expect(isPlainObject(true)).toBe(false);
  });
});
