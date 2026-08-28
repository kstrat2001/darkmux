import { describe, it, expect, vi, afterEach } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { usePlaybackTransport } from "./usePlaybackTransport";
import type { FlowRecord } from "../types/handwritten";

const DAY = [
  { ts: "2026-08-07T00:00:00.000Z", action: "dispatch.start" },
  { ts: "2026-08-07T00:30:00.000Z", action: "dispatch.reasoning" },
  { ts: "2026-08-07T01:00:00.000Z", action: "dispatch.complete" },
] as unknown as FlowRecord[];

/** (#2071) The app-shell-owned transport. */
describe("usePlaybackTransport", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("is inactive with no day, and pinned at the day's end once one loads", () => {
    const { result, rerender } = renderHook(({ d }) => usePlaybackTransport(d), { initialProps: { d: null as FlowRecord[] | null } });
    expect(result.current.active).toBe(false);
    rerender({ d: DAY });
    expect(result.current.active).toBe(true);
    expect(result.current.t).toBe(Date.parse("2026-08-07T01:00:00.000Z"));
    expect(result.current.visibleCount).toBe(3);
  });

  it("scrub and rewind move the playhead; visibleCount follows it", () => {
    const { result } = renderHook(() => usePlaybackTransport(DAY));
    act(() => result.current.scrub(Date.parse("2026-08-07T00:30:00.000Z")));
    expect(result.current.visibleCount).toBe(2);
    act(() => result.current.rewind());
    expect(result.current.t).toBe(Date.parse("2026-08-07T00:00:00.000Z"));
    expect(result.current.visibleCount).toBe(1);
  });

  it("play from the end starts over, advances on the tick, and stops at the end", () => {
    vi.useFakeTimers();
    const { result } = renderHook(() => usePlaybackTransport(DAY));
    act(() => result.current.togglePlay());
    expect(result.current.playing).toBe(true);
    expect(result.current.t).toBe(result.current.tMin);
    act(() => {
      vi.advanceTimersByTime(1500);
    });
    expect(result.current.t).toBeGreaterThan(result.current.tMin);
    expect(result.current.t).toBeLessThan(result.current.tMax);
    act(() => {
      vi.advanceTimersByTime(60_000);
    });
    expect(result.current.t).toBe(result.current.tMax);
    expect(result.current.playing).toBe(false);
  });

  it("a different day resets the playhead, playing state, and speed", () => {
    const { result, rerender } = renderHook(({ d }) => usePlaybackTransport(d), { initialProps: { d: DAY } });
    act(() => {
      result.current.rewind();
      result.current.cycleSpeed();
    });
    expect(result.current.speed).toBe(2);
    const other = [{ ts: "2026-08-09T00:00:00.000Z", action: "dispatch.start" }, { ts: "2026-08-09T02:00:00.000Z", action: "dispatch.complete" }] as unknown as FlowRecord[];
    rerender({ d: other });
    expect(result.current.t).toBe(Date.parse("2026-08-09T02:00:00.000Z"));
    expect(result.current.speed).toBe(1);
    expect(result.current.playing).toBe(false);
  });
});
