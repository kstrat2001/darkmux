import { describe, it, expect, vi, afterEach } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { useArrivalKeys } from "./useArrivalKeys";

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
});
