import { describe, it, expect, vi, afterEach } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { createElement, type ReactNode } from "react";
import { useCountUp } from "./useCountUp";
import { SeekSignalContext } from "../lib/seekSignal";

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

  it("upOnly: a decrease snaps with no intermediate frames; an increase still tweens", () => {
    // The fleet hero's "last 24h" total SHRINKS on every poll as the window
    // slides past old records, with nothing running. Tweening that read as
    // live activity on an idle fleet; only new work (an increase) animates.
    stubReducedMotion(false);
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date", "requestAnimationFrame", "cancelAnimationFrame", "performance"] });
    const { result, rerender } = renderHook(({ v }) => useCountUp(v, fmt, undefined, { upOnly: true }), {
      initialProps: { v: 1000 },
    });
    rerender({ v: 900 });
    expect(result.current).toBe("900"); // landed on the render itself, no frame advanced
    rerender({ v: 1000 });
    act(() => {
      vi.advanceTimersByTime(300);
    });
    const mid = Number(result.current);
    expect(mid).toBeGreaterThan(900);
    expect(mid).toBeLessThan(1000);
    act(() => {
      vi.advanceTimersByTime(1000);
    });
    expect(result.current).toBe("1000");
  });

  it("a change mid-tween continues from the number on screen, never jumps back to the old target", () => {
    stubReducedMotion(false);
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date", "requestAnimationFrame", "cancelAnimationFrame", "performance"] });
    const { result, rerender } = renderHook(({ v }) => useCountUp(v, fmt), { initialProps: { v: 0 } });
    rerender({ v: 100 });
    act(() => {
      vi.advanceTimersByTime(300);
    });
    const shown = Number(result.current);
    rerender({ v: 200 });
    act(() => {
      vi.advanceTimersByTime(16);
    });
    // Restarting from the previous TARGET (100) would jump forward past what
    // was shown; the next frame must stay near the displayed value.
    expect(Number(result.current)).toBeGreaterThanOrEqual(shown);
    expect(Number(result.current)).toBeLessThan(100);
  });

  it("a target going null never renders the stale number, not even for one render", () => {
    // SessionReplay's metric tile formats through a parser that exists only
    // while the value is numeric; handing it the old number on the render
    // where the value turned "—" crashed the whole run page.
    stubReducedMotion(false);
    let live: number | null = 5;
    const strict = (n: number | null) => {
      if (live === null && n !== null) throw new Error(`stale number ${n} formatted after the target went null`);
      return fmt(n);
    };
    const { result, rerender } = renderHook(({ v }) => useCountUp(v, strict), { initialProps: { v: 5 as number | null } });
    live = null;
    rerender({ v: null });
    expect(result.current).toBe("—");
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

  // (Playback parity, Change B) The seek signal is the ONLY thing that
  // suppresses a tween now — not a per-caller `durationMs` gate keyed on
  // `liveMode`. An ADVANCE (the target changes, `seekGen` does not) still
  // tweens; a SEEK (the target changes AND `seekGen` bumps in the same
  // update) snaps instantly, in both modes.
  describe("seek signal (Change B)", () => {
    function withSeekGen(initialGen: number) {
      let gen = initialGen;
      const wrapper = ({ children }: { children: ReactNode }) =>
        createElement(SeekSignalContext.Provider, { value: gen }, children);
      return { wrapper, bump: () => { gen += 1; } };
    }

    it("an ADVANCE (seekGen unchanged) still tweens — mid-flight value is neither the old nor the new target", () => {
      stubReducedMotion(false);
      vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date", "requestAnimationFrame", "cancelAnimationFrame", "performance"] });
      const { wrapper } = withSeekGen(0);
      const { result, rerender } = renderHook(({ v }) => useCountUp(v, fmt), { initialProps: { v: 0 }, wrapper });
      rerender({ v: 100 });
      act(() => { vi.advanceTimersByTime(350); }); // mid-tween (DEFAULT_DURATION_MS=700)
      const mid = Number(result.current);
      expect(mid).toBeGreaterThan(0);
      expect(mid).toBeLessThan(100);
      act(() => { vi.advanceTimersByTime(400); });
      expect(result.current).toBe("100");
    });

    it("a SEEK (seekGen bumps on the same update the target changes) snaps instantly — no intermediate frame", () => {
      stubReducedMotion(false);
      vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date", "requestAnimationFrame", "cancelAnimationFrame", "performance"] });
      const { wrapper, bump } = withSeekGen(0);
      const { result, rerender } = renderHook(({ v }) => useCountUp(v, fmt), { initialProps: { v: 0 }, wrapper });
      bump();
      rerender({ v: 100 });
      // No rAF/timer advance at all — a seek must land on the first render,
      // exactly like the `durationMs: 0` caller opt-out above.
      expect(result.current).toBe("100");
    });

    it("live mode's context default (seekGen never provided, always 0) never counts as a seek — advance keeps animating", () => {
      // No wrapper at all: `useSeekGeneration()` reads the context's
      // default value (0), which never changes across renders — this is
      // what makes live mode's behavior exactly what it always was.
      stubReducedMotion(false);
      vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date", "requestAnimationFrame", "cancelAnimationFrame", "performance"] });
      const { result, rerender } = renderHook(({ v }) => useCountUp(v, fmt), { initialProps: { v: 0 } });
      rerender({ v: 100 });
      act(() => { vi.advanceTimersByTime(350); });
      const mid = Number(result.current);
      expect(mid).toBeGreaterThan(0);
      expect(mid).toBeLessThan(100);
    });
  });
});
