import { describe, it, expect, vi, afterEach } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { useNowMs, __clockDebug, CLOCK_INTERVAL_MS } from "./clock";

afterEach(() => {
  vi.useRealTimers();
});

describe("clock", () => {
  it("(#1972) ticks a MOUNTED active consumer once a second", () => {
    vi.useFakeTimers();
    vi.setSystemTime(1_000_000);
    const { result } = renderHook(() => useNowMs(true));
    expect(result.current).toBe(1_000_000);

    // NOTE: `advanceTimersByTime` advances the mocked `Date.now()` too, so
    // setting the system time here as well would double-count.
    act(() => {
      vi.advanceTimersByTime(CLOCK_INTERVAL_MS);
    });
    expect(result.current).toBe(1_000_000 + CLOCK_INTERVAL_MS);
  });

  it("(#1972) survives a clock that advances BETWEEN snapshot reads — the React #185 shape", () => {
    // The guard that matters, and the first version of it could not fail.
    //
    // Under frozen fake timers a `getSnapshot` of `() => Date.now()` returns
    // the SAME number on every call, so asserting "two reads are equal"
    // passes against the bug. The real condition is time MOVING between
    // reads, which is what a real browser does — `useSyncExternalStore` then
    // sees a new snapshot after every render and re-renders forever, and
    // React aborts with "Maximum update depth exceeded" (#185). That shipped
    // once as a blank page; see `useHashRoute.ts` and `App.test.tsx`.
    //
    // So: a `Date.now` that advances on EVERY call. With the stored snapshot
    // this is harmless — only the interval writes `nowMs`. With a fresh
    // `Date.now()` snapshot it loops and throws.
    let t = 5_000_000;
    const advancing = vi.fn(() => (t += 17));
    vi.stubGlobal("Date", { ...Date, now: advancing } as unknown as DateConstructor);
    try {
      const { result, rerender, unmount } = renderHook(() => useNowMs(true));
      const first = result.current;
      // Each render re-reads the snapshot. With the value STORED it cannot
      // move (only the interval writes it), so this is stable even though the
      // underlying clock advances on every `Date.now()` call. With a fresh
      // `Date.now()` snapshot React sees a change per render and loops until
      // it throws #185 — which `renderHook` surfaces as a thrown error here.
      rerender();
      rerender();
      rerender();
      expect(result.current).toBe(first);
      unmount();
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("(#1972) runs NO timer when nothing active is mounted, and stops it when the last consumer unmounts", () => {
    // The structural gate. An `enabled` flag can be forgotten; a timer that
    // only exists while a subscriber does cannot leak.
    vi.useFakeTimers();
    expect(__clockDebug()).toEqual({ listeners: 0, running: false });

    const a = renderHook(() => useNowMs(true));
    const b = renderHook(() => useNowMs(true));
    expect(__clockDebug()).toEqual({ listeners: 2, running: true });

    a.unmount();
    expect(__clockDebug().running).toBe(true); // one consumer left

    b.unmount();
    expect(__clockDebug()).toEqual({ listeners: 0, running: false });
  });

  it("(#1972) an INACTIVE consumer subscribes to nothing — a finished run must not drive a timer", () => {
    vi.useFakeTimers();
    const { result, unmount } = renderHook(() => useNowMs(false));
    expect(__clockDebug()).toEqual({ listeners: 0, running: false });
    // ...and returns a stable constant, so it cannot loop either.
    const first = result.current;
    act(() => {
      vi.advanceTimersByTime(CLOCK_INTERVAL_MS * 5);
    });
    expect(result.current).toBe(first);
    unmount();
  });

  it("(#1972) flipping active on starts the timer, and flipping it off stops it", () => {
    vi.useFakeTimers();
    const { rerender, unmount } = renderHook(({ live }) => useNowMs(live), { initialProps: { live: false } });
    expect(__clockDebug().running).toBe(false);
    rerender({ live: true });
    expect(__clockDebug().running).toBe(true);
    rerender({ live: false });
    expect(__clockDebug()).toEqual({ listeners: 0, running: false });
    unmount();
  });

  it("(#1972) refreshes on the FIRST subscribe rather than serving a stale module-load timestamp", () => {
    // A long-lived tab imports this module once. Without the refresh, the
    // first paint after mounting a live run would show whenever the bundle
    // loaded, then jump when the first tick landed.
    vi.useFakeTimers();
    vi.setSystemTime(9_999_000);
    const { result, unmount } = renderHook(() => useNowMs(true));
    expect(result.current).toBe(9_999_000);
    unmount();
  });
});
