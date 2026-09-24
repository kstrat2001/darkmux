import { describe, it, expect, vi, afterEach } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { createElement, type ReactNode } from "react";
import { useArrivalKeys } from "./useArrivalKeys";
import { SeekSignalContext } from "../lib/seekSignal";

afterEach(() => {
  vi.useRealTimers();
});

describe("useArrivalKeys (#2878)", () => {
  it("marks nothing as arriving for the page's initial list", () => {
    const { result } = renderHook(({ keys }) => useArrivalKeys(keys, "scope-a"), {
      initialProps: { keys: ["a", "b", "c"] },
    });
    expect(result.current.size).toBe(0);
  });

  it("marks only a genuinely NEW key added later — not the ones already present", () => {
    vi.useFakeTimers();
    const { result, rerender } = renderHook(({ keys }) => useArrivalKeys(keys, "scope-a"), {
      initialProps: { keys: ["a", "b"] },
    });
    expect(result.current.size).toBe(0);
    rerender({ keys: ["a", "b", "c"] });
    expect(result.current.has("c")).toBe(true);
    expect(result.current.has("a")).toBe(false);
    expect(result.current.has("b")).toBe(false);
  });

  it("a key revealed by a filter change (present before, just re-supplied) is never marked arriving — this hook only ever sees the caller's full key set, so a filter toggle in the caller never even reaches it as a change", () => {
    vi.useFakeTimers();
    const { result, rerender } = renderHook(({ keys }) => useArrivalKeys(keys, "scope-a"), {
      initialProps: { keys: ["a", "b", "c"] },
    });
    // Simulating the caller passing the SAME full underlying set again
    // (a filter change never alters what this hook is given).
    rerender({ keys: ["a", "b", "c"] });
    expect(result.current.size).toBe(0);
  });

  it("expires an arrival's highlight after the hold window", () => {
    vi.useFakeTimers();
    const { result, rerender } = renderHook(({ keys }) => useArrivalKeys(keys, "scope-a", 500), {
      initialProps: { keys: ["a"] },
    });
    rerender({ keys: ["a", "b"] });
    expect(result.current.has("b")).toBe(true);
    act(() => {
      vi.advanceTimersByTime(499);
    });
    expect(result.current.has("b")).toBe(true);
    act(() => {
      vi.advanceTimersByTime(1);
    });
    expect(result.current.has("b")).toBe(false);
  });

  it("a resetKey change re-arms as a fresh mount — a whole new data set is never treated as a burst of arrivals", () => {
    vi.useFakeTimers();
    const { result, rerender } = renderHook(({ keys, scope }) => useArrivalKeys(keys, scope), {
      initialProps: { keys: ["a", "b"], scope: "run-1" },
    });
    rerender({ keys: ["x", "y", "z"], scope: "run-2" });
    expect(result.current.size).toBe(0);
  });

  // (Playback parity, Change B, finding #12) A forward SEEK (a scrub that
  // jumps the playhead past several rows at once) must adopt the newly-
  // visible keys silently — not mark every one of them as "arriving", the
  // way a genuine ADVANCE (one row landing at a time) does.
  describe("seek signal (Change B)", () => {
    function withSeekGen(initialGen: number) {
      let gen = initialGen;
      const wrapper = ({ children }: { children: ReactNode }) =>
        createElement(SeekSignalContext.Provider, { value: gen }, children);
      return { wrapper, bump: () => { gen += 1; } };
    }

    it("an ADVANCE (seekGen unchanged) still marks the new key as arriving", () => {
      vi.useFakeTimers();
      const { wrapper } = withSeekGen(0);
      const { result, rerender } = renderHook(({ keys }) => useArrivalKeys(keys, "scope-a"), {
        initialProps: { keys: ["a"] },
        wrapper,
      });
      rerender({ keys: ["a", "b"] });
      expect(result.current.has("b")).toBe(true);
    });

    it("a SEEK (seekGen bumps on the same update several keys appear) adopts them silently — none marked arriving", () => {
      vi.useFakeTimers();
      const { wrapper, bump } = withSeekGen(0);
      const { result, rerender } = renderHook(({ keys }) => useArrivalKeys(keys, "scope-a"), {
        initialProps: { keys: ["a"] },
        wrapper,
      });
      bump();
      rerender({ keys: ["a", "b", "c", "d"] }); // a forward scrub jumped past b, c, d at once
      expect(result.current.size).toBe(0);
      // The seeked-past keys are still adopted as SEEN — a later genuine
      // advance only marks what's new AFTER this seek, not b/c/d again.
      rerender({ keys: ["a", "b", "c", "d", "e"] });
      expect(result.current.has("e")).toBe(true);
      expect(result.current.has("b")).toBe(false);
      expect(result.current.has("c")).toBe(false);
      expect(result.current.has("d")).toBe(false);
    });
  });
});
