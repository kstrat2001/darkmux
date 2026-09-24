import { describe, it, expect, vi, afterEach } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { useCountUp } from "./useCountUp";

function stubReducedMotion(matches: boolean) {
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    value: (q: string) => ({
      matches: q.includes("prefers-reduced-motion") ? matches : false,
      media: q,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
    }),
  });
}

const fmt = (n: number | null) => (n === null ? "—" : String(Math.round(n)));

afterEach(() => {
  vi.useRealTimers();
  // @ts-expect-error test-only cleanup of a property tests stub per-case
  delete window.matchMedia;
});

describe("useCountUp (#2878)", () => {
  it("never animates on first mount — the true value renders immediately", () => {
    stubReducedMotion(false);
    const { result } = renderHook(({ v }) => useCountUp(v, fmt), { initialProps: { v: 42 } });
    expect(result.current).toBe("42");
  });

  it("tweens to the new target on a change and lands exactly on it", () => {
    stubReducedMotion(false);
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date", "requestAnimationFrame", "cancelAnimationFrame", "performance"] });
    const { result, rerender } = renderHook(({ v }) => useCountUp(v, fmt), { initialProps: { v: 0 } });
    expect(result.current).toBe("0");
    rerender({ v: 100 });
    // Mid-flight: somewhere between 0 and 100, never the old or new value's
    // exact string on the very first frame this test can observe deep in
    // the animation, but always in range.
    act(() => {
      vi.advanceTimersByTime(300);
    });
    const mid = Number(result.current);
    expect(mid).toBeGreaterThan(0);
    expect(mid).toBeLessThan(100);
    act(() => {
      vi.advanceTimersByTime(1000);
    });
    expect(result.current).toBe("100"); // exact end state, not an approximation
  });

  it("applies the caller's own formatter, never its own", () => {
    stubReducedMotion(false);
    const asDollars = (n: number | null) => (n === null ? "" : `$${n}`);
    const { result } = renderHook(({ v }) => useCountUp(v, asDollars), { initialProps: { v: 7 } });
    expect(result.current).toBe("$7");
  });

  it("prefers-reduced-motion: reduce jumps straight to the target, no intermediate frames", () => {
    stubReducedMotion(true);
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date", "requestAnimationFrame", "cancelAnimationFrame", "performance"] });
    const { result, rerender } = renderHook(({ v }) => useCountUp(v, fmt), { initialProps: { v: 0 } });
    rerender({ v: 100 });
    // No timer advance at all — reduced motion must not need one.
    expect(result.current).toBe("100");
  });

  it("durationMs: 0 is a caller opt-out — a scrub/seek jumps instantly, never tweens between two unrelated instants", () => {
    stubReducedMotion(false);
    const { result, rerender } = renderHook(({ v }) => useCountUp(v, fmt, 0), { initialProps: { v: 10 } });
    rerender({ v: 900 });
    expect(result.current).toBe("900"); // no rAF, no wait, no intermediate frame
  });

  it("absence on either side of a change snaps instantly — never tweens through null", () => {
    stubReducedMotion(false);
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date", "requestAnimationFrame", "cancelAnimationFrame", "performance"] });
    const { result, rerender } = renderHook(({ v }) => useCountUp(v, fmt), { initialProps: { v: 50 as number | null } });
    rerender({ v: null });
    expect(result.current).toBe("—");
    rerender({ v: 80 });
    expect(result.current).toBe("80"); // a first real reading after absence, not a tween from 50
  });
});
