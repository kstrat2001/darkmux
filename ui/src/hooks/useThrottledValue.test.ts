import { describe, it, expect, vi, afterEach } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { useThrottledValue } from "./useThrottledValue";

/** (#2068) The event inspector's follow throttle. */
describe("useThrottledValue", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("lands the first change immediately, coalesces changes inside the hold, and lands the latest when the hold closes", () => {
    vi.useFakeTimers();
    vi.setSystemTime(10_000);
    const { result, rerender } = renderHook(({ v }) => useThrottledValue(v, 400), { initialProps: { v: "a" } });
    expect(result.current).toBe("a");
    rerender({ v: "b" });
    expect(result.current).toBe("b"); // leading edge: enough time since the last commit
    rerender({ v: "c" });
    rerender({ v: "d" });
    expect(result.current).toBe("b"); // inside the hold: c and d are coalesced, nothing rendered yet
    act(() => {
      vi.advanceTimersByTime(399);
    });
    expect(result.current).toBe("b");
    act(() => {
      vi.advanceTimersByTime(1);
    });
    expect(result.current).toBe("d"); // trailing edge lands the LATEST, not c
  });

  it("never settles on a value that is no longer the input: a revert during the hold cancels the pending commit", () => {
    vi.useFakeTimers();
    vi.setSystemTime(10_000);
    const { result, rerender } = renderHook(({ v }) => useThrottledValue(v, 400), { initialProps: { v: "a" } });
    rerender({ v: "b" }); // leading edge
    expect(result.current).toBe("b");
    rerender({ v: "c" }); // pending c, trailing timer armed
    rerender({ v: "b" }); // the input reverts to what is shown (a filter excluded c)
    act(() => {
      vi.advanceTimersByTime(1000);
    });
    expect(result.current).toBe("b"); // c must not land
    rerender({ v: "d" }); // a later change is still honored (trailing, inside the hold since b's commit)
    act(() => {
      vi.advanceTimersByTime(1000);
    });
    expect(result.current).toBe("d");
  });

  it("uses the caller's equality so a re-created object for the same record does not restart the hold", () => {
    vi.useFakeTimers();
    vi.setSystemTime(10_000);
    const same = (a: { k: string }, b: { k: string }) => a.k === b.k;
    const { result, rerender } = renderHook(({ v }) => useThrottledValue(v, 400, same), { initialProps: { v: { k: "x" } } });
    const first = result.current;
    rerender({ v: { k: "x" } });
    expect(result.current).toBe(first); // same key: no commit, same object kept
  });
});
