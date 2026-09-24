import { describe, it, expect } from "vitest";
import { parseNumericLike } from "./numericLike";

describe("parseNumericLike (#2878)", () => {
  it("parses a bare integer and re-renders a tween value with no grouping", () => {
    const p = parseNumericLike("128");
    expect(p).not.toBeNull();
    expect(p!.n).toBe(128);
    expect(p!.render(64)).toBe("64");
  });

  it("parses a percentage and keeps the % on the tweened value", () => {
    const p = parseNumericLike("72%");
    expect(p!.n).toBe(72);
    expect(p!.render(50)).toBe("50%");
  });

  it("preserves comma grouping on the reproduced string", () => {
    const p = parseNumericLike("4,096");
    expect(p!.n).toBe(4096);
    expect(p!.render(1234)).toBe("1,234");
  });

  it("preserves decimal precision from the original string", () => {
    const p = parseNumericLike("3.20");
    expect(p!.n).toBe(3.2);
    expect(p!.render(1.5)).toBe("1.50");
  });

  it("handles a negative number", () => {
    const p = parseNumericLike("-12");
    expect(p!.n).toBe(-12);
    expect(p!.render(-3)).toBe("-3");
  });

  it("returns null for a model name — not numeric at all", () => {
    expect(parseNumericLike("Qwen3.6-35B-A3B")).toBeNull();
  });

  it("returns null for a multi-part duration string", () => {
    expect(parseNumericLike("4h 20m")).toBeNull();
  });

  it("returns null for an em-dash absence marker", () => {
    expect(parseNumericLike("—")).toBeNull();
  });
});
