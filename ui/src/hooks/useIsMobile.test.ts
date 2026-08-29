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

// (#2108 review finding 10, WebKit-proven) The landscape-phone fallback —
// wider than MOBILE_BREAKPOINT but still a phone once rotated (an iPhone
// 14: 844×390). jsdom has no `matchMedia` at all (a bare call throws), so
// these tests stub it per-case rather than relying on a real engine's
// touch emulation, which is exercised separately by
// `sheet-gate-webkit.mjs`'s Playwright/WebKit run against the real app.
describe("useIsMobile — landscape-phone fallback (#2108 review finding 10)", () => {
  function setInnerHeight(height: number) {
    Object.defineProperty(window, "innerHeight", {
      configurable: true,
      value: height,
    });
  }
  function stubMatchMedia(coarse: boolean) {
    const mql = { matches: coarse } as MediaQueryList;
    (window as unknown as { matchMedia: (q: string) => MediaQueryList }).matchMedia = () => mql;
  }
  const ORIGINAL_HEIGHT = window.innerHeight;
  const ORIGINAL_MATCH_MEDIA = (window as unknown as { matchMedia?: unknown }).matchMedia;

  afterEach(() => {
    setInnerHeight(ORIGINAL_HEIGHT);
    if (ORIGINAL_MATCH_MEDIA === undefined) {
      delete (window as unknown as { matchMedia?: unknown }).matchMedia;
    } else {
      (window as unknown as { matchMedia: unknown }).matchMedia = ORIGINAL_MATCH_MEDIA;
    }
  });

  it("a coarse-pointer landscape window at 844x390 (an iPhone 14, rotated) reads mobile even though 844 > MOBILE_BREAKPOINT", () => {
    setInnerWidth(844);
    setInnerHeight(390);
    stubMatchMedia(true);
    const { result } = renderHook(() => useIsMobile());
    expect(result.current).toBe(true);
  });

  it("the SAME 844x390 window with a fine (mouse) pointer stays desktop — width alone is not enough", () => {
    setInnerWidth(844);
    setInnerHeight(390);
    stubMatchMedia(false);
    const { result } = renderHook(() => useIsMobile());
    expect(result.current).toBe(false);
  });

  it("a coarse-pointer PORTRAIT window above the breakpoint stays desktop — landscape is required, not just touch", () => {
    setInnerWidth(1000);
    setInnerHeight(1200);
    stubMatchMedia(true);
    const { result } = renderHook(() => useIsMobile());
    expect(result.current).toBe(false);
  });

  it("a coarse-pointer landscape window ABOVE the 1024px cap stays desktop — a touch-capable desktop/tablet in landscape is not a phone", () => {
    setInnerWidth(1200);
    setInnerHeight(700);
    stubMatchMedia(true);
    const { result } = renderHook(() => useIsMobile());
    expect(result.current).toBe(false);
  });

  it("without a matchMedia implementation at all (jsdom's real default), the landscape branch is a safe no-op, not a throw", () => {
    setInnerWidth(844);
    setInnerHeight(390);
    delete (window as unknown as { matchMedia?: unknown }).matchMedia;
    expect(() => renderHook(() => useIsMobile())).not.toThrow();
    const { result } = renderHook(() => useIsMobile());
    expect(result.current).toBe(false);
  });

  it("updates on a live rotation, in either direction", () => {
    setInnerWidth(390);
    setInnerHeight(844);
    stubMatchMedia(true);
    const { result } = renderHook(() => useIsMobile());
    expect(result.current).toBe(true); // portrait, under the plain breakpoint

    act(() => {
      setInnerWidth(844);
      setInnerHeight(390);
      window.dispatchEvent(new Event("resize"));
    });
    expect(result.current).toBe(true); // rotated to landscape — still mobile

    act(() => {
      setInnerWidth(390);
      setInnerHeight(844);
      window.dispatchEvent(new Event("resize"));
    });
    expect(result.current).toBe(true); // rotated back — still mobile
  });
});
