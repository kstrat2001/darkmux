import { describe, it, expect, afterEach } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { useIsMobile, MOBILE_BREAKPOINT } from "./useIsMobile";

function setInnerWidth(width: number) {
  Object.defineProperty(window, "innerWidth", {
    configurable: true,
    value: width,
  });
}

const ORIGINAL_WIDTH = window.innerWidth;

afterEach(() => {
  setInnerWidth(ORIGINAL_WIDTH);
});

// (#2108, operator finding — "the breakpoint firing at 974px") The SHARED
// breakpoint every desktop/mobile chrome decision in this app keys off
// (`App.tsx`, `MachineDrawer.tsx`, `MachineLens.tsx`,
// `MachineHealthRegion.tsx`) is this ONE hook, `MOBILE_BREAKPOINT` (768).
// These tests pin that number directly, at the boundary and at 974px —
// the width a real device measured this at — so "desktop renders" at
// 974px is a checked fact, not an assumption a future edit could silently
// invalidate.
describe("useIsMobile (#2108)", () => {
  it("MOBILE_BREAKPOINT is 768 — the one number every caller shares", () => {
    expect(MOBILE_BREAKPOINT).toBe(768);
  });

  it("is false (desktop) at 974px — the width a real device measured the collision at", () => {
    setInnerWidth(974);
    const { result } = renderHook(() => useIsMobile());
    expect(result.current).toBe(false);
  });

  it("is true at exactly the breakpoint (768px, inclusive) and false just above it", () => {
    setInnerWidth(768);
    const atBreakpoint = renderHook(() => useIsMobile());
    expect(atBreakpoint.result.current).toBe(true);

    setInnerWidth(769);
    const justAbove = renderHook(() => useIsMobile());
    expect(justAbove.result.current).toBe(false);
  });

  it("updates on resize, in either direction", () => {
    setInnerWidth(1024);
    const { result } = renderHook(() => useIsMobile());
    expect(result.current).toBe(false);

    act(() => {
      setInnerWidth(500);
      window.dispatchEvent(new Event("resize"));
    });
    expect(result.current).toBe(true);

    act(() => {
      setInnerWidth(974);
      window.dispatchEvent(new Event("resize"));
    });
    expect(result.current).toBe(false);
  });

  it("accepts an explicit breakpoint override, independent of MOBILE_BREAKPOINT", () => {
    setInnerWidth(974);
    // A caller-supplied breakpoint above 974 makes 974 read as "mobile" —
    // proves the override is genuinely honored, not silently ignored in
    // favor of the shared default.
    const { result } = renderHook(() => useIsMobile(1180));
    expect(result.current).toBe(true);
  });
});
