import { describe, it, expect, vi, afterEach } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { usePlaybackTransport } from "./usePlaybackTransport";
import type { FlowRecord } from "../types/handwritten";

const DAY = [
  { ts: "2026-08-07T00:00:00.000Z", action: "dispatch.start" },
  { ts: "2026-08-07T00:30:00.000Z", action: "dispatch.reasoning" },
  { ts: "2026-08-07T01:00:00.000Z", action: "dispatch.complete" },
] as unknown as FlowRecord[];

// (#2346) A day holding two dispatches and one mission, so a focus can be
// scoped to something narrower than the whole day: `sA` runs 08:00–08:30,
// `sB` runs 09:00–20:43 (the day's own last record — the shape of the bug
// report: a run that ends hours before the day's last record), and `mA`
// spans 07:00–07:20 with no dispatches of its own.
const MIXED_DAY = [
  { ts: "2026-08-07T07:00:00.000Z", action: "mission start", mission_id: "mA" },
  { ts: "2026-08-07T07:20:00.000Z", action: "mission close", mission_id: "mA" },
  { ts: "2026-08-07T08:00:00.000Z", action: "dispatch.start", session_id: "sA" },
  { ts: "2026-08-07T08:15:00.000Z", action: "dispatch.reasoning", session_id: "sA" },
  { ts: "2026-08-07T08:30:00.000Z", action: "dispatch.complete", session_id: "sA" },
  { ts: "2026-08-07T09:00:00.000Z", action: "dispatch.start", session_id: "sB" },
  { ts: "2026-08-07T20:43:00.000Z", action: "dispatch.complete", session_id: "sB" },
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
    expect(result.current.scrubbed).toBe(false); // at rest: nothing is cut
    expect(result.current.t).toBe(Date.parse("2026-08-07T01:00:00.000Z"));
    expect(result.current.visibleCount).toBe(3);
    act(() => result.current.rewind());
    expect(result.current.scrubbed).toBe(true);
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
      vi.advanceTimersByTime(500); // half a real second = half a recorded hour at the default 1h/s; the day is one hour
    });
    expect(result.current.t).toBeGreaterThan(result.current.tMin);
    expect(result.current.t).toBeLessThan(result.current.tMax);
    act(() => {
      vi.advanceTimersByTime(60_000);
    });
    expect(result.current.t).toBe(result.current.tMax);
    expect(result.current.playing).toBe(false);
  });

  it("speed is a real multiplier: at 1h/s one real second replays one recorded hour, at 1m/s one minute", () => {
    vi.useFakeTimers();
    const threeHours = [
      { ts: "2026-08-07T00:00:00.000Z", action: "dispatch.start" },
      { ts: "2026-08-07T03:00:00.000Z", action: "dispatch.complete" },
    ] as unknown as FlowRecord[];
    const { result } = renderHook(() => usePlaybackTransport(threeHours));
    expect(result.current.speed).toBe(3600);
    act(() => result.current.togglePlay());
    act(() => {
      vi.advanceTimersByTime(1000);
    });
    expect(result.current.t - result.current.tMin).toBe(3600_000);
    // The step is MEASURED: a throttled interval (one late tick standing in
    // for ten) still replays the labeled amount of recorded time.
    act(() => result.current.rewind());
    act(() => result.current.cycleSpeed()); // 1h/s -> 10m/s
    expect(result.current.speed).toBe(600);
    act(() => result.current.cycleSpeed()); // 10m/s -> 1m/s
    expect(result.current.speed).toBe(60);
    act(() => {
      vi.advanceTimersByTime(1000);
    });
    expect(result.current.t - result.current.tMin).toBe(60_000);
  });

  it("a different day resets the playhead, playing state, and speed", () => {
    const { result, rerender } = renderHook(({ d }) => usePlaybackTransport(d), { initialProps: { d: DAY } });
    act(() => {
      result.current.rewind();
      result.current.cycleSpeed();
    });
    expect(result.current.speed).toBe(600);
    const other = [{ ts: "2026-08-09T00:00:00.000Z", action: "dispatch.start" }, { ts: "2026-08-09T02:00:00.000Z", action: "dispatch.complete" }] as unknown as FlowRecord[];
    rerender({ d: other });
    expect(result.current.t).toBe(Date.parse("2026-08-09T02:00:00.000Z"));
    expect(result.current.speed).toBe(3600);
    expect(result.current.playing).toBe(false);
  });

  // (#2346) The masthead scrubber spans the whole day even when a run or
  // mission is open, so the playhead ends at the day's last record while
  // the open run ended hours earlier — two clocks, two different subjects.
  // A FOCUS narrows tMin/tMax to the open thing's own span. Redesigned
  // after a live-render finding: the focus carries its OWN records (a
  // dispatch/mission's own separate fetch) rather than being derived by
  // filtering the day — see the DISJOINT describe block further down for
  // why that distinction is load-bearing, not cosmetic.
  const sARecords = MIXED_DAY.filter((r) => r.session_id === "sA");
  const sBRecords = MIXED_DAY.filter((r) => r.session_id === "sB");
  const mARecords = MIXED_DAY.filter((r) => r.mission_id === "mA");

  describe("focus (#2346)", () => {
    it("day focus (the default) spans the whole day, same as passing no focus at all", () => {
      const withDefault = renderHook(() => usePlaybackTransport(MIXED_DAY));
      const withExplicit = renderHook(() => usePlaybackTransport(MIXED_DAY, { kind: "day" }));
      expect(withDefault.result.current.tMin).toBe(Date.parse("2026-08-07T07:00:00.000Z"));
      expect(withDefault.result.current.tMax).toBe(Date.parse("2026-08-07T20:43:00.000Z"));
      expect(withExplicit.result.current.tMin).toBe(withDefault.result.current.tMin);
      expect(withExplicit.result.current.tMax).toBe(withDefault.result.current.tMax);
    });

    it("dispatch focus spans that session's own start to its own end, not the day's", () => {
      const { result } = renderHook(() => usePlaybackTransport(MIXED_DAY, { kind: "dispatch", sessionId: "sA", records: sARecords }));
      expect(result.current.tMin).toBe(Date.parse("2026-08-07T08:00:00.000Z"));
      expect(result.current.tMax).toBe(Date.parse("2026-08-07T08:30:00.000Z"));
    });

    it("a later-ending dispatch (the bug report's own shape) ends its OWN wall clock, not the day's 20:43", () => {
      const { result } = renderHook(() => usePlaybackTransport(MIXED_DAY, { kind: "dispatch", sessionId: "sB", records: sBRecords }));
      expect(result.current.tMin).toBe(Date.parse("2026-08-07T09:00:00.000Z"));
      expect(result.current.tMax).toBe(Date.parse("2026-08-07T20:43:00.000Z"));
    });

    it("mission focus spans mission start to mission close, not the day's", () => {
      const { result } = renderHook(() => usePlaybackTransport(MIXED_DAY, { kind: "mission", missionId: "mA", records: mARecords }));
      expect(result.current.tMin).toBe(Date.parse("2026-08-07T07:00:00.000Z"));
      expect(result.current.tMax).toBe(Date.parse("2026-08-07T07:20:00.000Z"));
    });

    it("rewind lands on the focus's own tMin, for a dispatch focus", () => {
      const { result } = renderHook(() => usePlaybackTransport(MIXED_DAY, { kind: "dispatch", sessionId: "sA", records: sARecords }));
      act(() => result.current.rewind());
      expect(result.current.t).toBe(Date.parse("2026-08-07T08:00:00.000Z"));
    });

    it("scrubbing past the focus's range clamps to it, not the day's", () => {
      const { result } = renderHook(() => usePlaybackTransport(MIXED_DAY, { kind: "dispatch", sessionId: "sA", records: sARecords }));
      act(() => result.current.scrub(Date.parse("2026-08-07T20:00:00.000Z"))); // well past sA's own 08:30 end
      expect(result.current.t).toBe(Date.parse("2026-08-07T08:30:00.000Z"));
      act(() => result.current.scrub(Date.parse("2026-08-07T00:00:00.000Z"))); // before sA's own 08:00 start
      expect(result.current.t).toBe(Date.parse("2026-08-07T08:00:00.000Z"));
    });

    it("at rest a dispatch focus is pinned at ITS OWN end, not the day's 20:43", () => {
      const { result } = renderHook(() => usePlaybackTransport(MIXED_DAY, { kind: "dispatch", sessionId: "sA", records: sARecords }));
      expect(result.current.scrubbed).toBe(false);
      expect(result.current.t).toBe(Date.parse("2026-08-07T08:30:00.000Z"));
    });

    it("switching focus on the SAME day keeps the absolute playhead when it still falls in the new range", () => {
      const { result, rerender } = renderHook(({ focus }) => usePlaybackTransport(MIXED_DAY, focus), {
        initialProps: { focus: { kind: "dispatch" as const, sessionId: "sB", records: sBRecords } },
      });
      act(() => result.current.scrub(Date.parse("2026-08-07T10:00:00.000Z"))); // inside sB's 09:00-20:43 span
      // Switch focus to the whole day — 10:00 still falls inside the day's span.
      rerender({ focus: { kind: "day" } as unknown as { kind: "dispatch"; sessionId: string; records: FlowRecord[] } });
      expect(result.current.t).toBe(Date.parse("2026-08-07T10:00:00.000Z"));
    });

    it("switching focus on the SAME day snaps to the new range's end when the old playhead falls outside it", () => {
      const { result, rerender } = renderHook(({ focus }) => usePlaybackTransport(MIXED_DAY, focus), {
        initialProps: { focus: { kind: "dispatch" as const, sessionId: "sB", records: sBRecords } },
      });
      act(() => result.current.scrub(Date.parse("2026-08-07T15:00:00.000Z"))); // inside sB, outside sA
      rerender({ focus: { kind: "dispatch", sessionId: "sA", records: sARecords } });
      // 15:00 is outside sA's 08:00-08:30 span, so the playhead snaps to sA's
      // own end — the same "pinned at end until scrubbed" default a fresh
      // focus gets, not a stale absolute time from the old one.
      expect(result.current.t).toBe(Date.parse("2026-08-07T08:30:00.000Z"));
      expect(result.current.scrubbed).toBe(false);
    });

    it("a genuine day change (not just a focus change) still does the full reset, even mid-focus", () => {
      const { result, rerender } = renderHook(({ d, focus }) => usePlaybackTransport(d, focus), {
        initialProps: { d: MIXED_DAY, focus: { kind: "dispatch" as const, sessionId: "sA", records: sARecords } },
      });
      act(() => {
        result.current.rewind();
        result.current.cycleSpeed();
      });
      expect(result.current.speed).toBe(600);
      const otherDaySc = [
        { ts: "2026-08-09T00:00:00.000Z", action: "dispatch.start", session_id: "sC" },
        { ts: "2026-08-09T02:00:00.000Z", action: "dispatch.complete", session_id: "sC" },
      ] as unknown as FlowRecord[];
      rerender({ d: otherDaySc, focus: { kind: "dispatch", sessionId: "sC", records: otherDaySc } });
      expect(result.current.tMin).toBe(Date.parse("2026-08-09T00:00:00.000Z"));
      expect(result.current.tMax).toBe(Date.parse("2026-08-09T02:00:00.000Z"));
      expect(result.current.t).toBe(Date.parse("2026-08-09T02:00:00.000Z"));
      expect(result.current.speed).toBe(3600);
      expect(result.current.playing).toBe(false);
    });
  });

  // (#2346, redesign after a live-render finding) A REAL daemon's
  // `/flow/<date>` is a CAPPED, TIME-WINDOWED slice — the operator's own
  // run started 00:11:48Z, but the loaded window's floor had drifted to
  // 02:43Z by the time it was inspected, so the run's records were NEVER in
  // `dayRecords` at all. The first cut of this fix derived the focus range
  // by filtering `dayRecords` — every fixture above happens to contain its
  // session, so that version passed every one of the tests above and still
  // reproduced the bug live. These fixtures deliberately put the focus's
  // own records OUTSIDE the day window, matching that shape.
  describe("focus records disjoint from dayRecords (#2346 redesign — the live-render finding)", () => {
    const DAY_WINDOW = [
      { ts: "2026-09-04T02:43:00.000Z", action: "dispatch.start", session_id: "unrelated" },
      { ts: "2026-09-04T04:48:00.000Z", action: "dispatch.complete", session_id: "unrelated" },
    ] as unknown as FlowRecord[];
    // The operator's own run (#2346 evidence): `dispatch start`/`dispatch
    // complete` — the SPACE-separated spelling `darkmux-crew` and the CLI
    // actually emit — wholly before the day window's own floor above.
    const RUN_RECORDS = [
      { ts: "2026-09-04T00:11:48.000Z", action: "dispatch start", session_id: "s-narrow" },
      { ts: "2026-09-04T02:06:37.000Z", action: "dispatch complete", session_id: "s-narrow", payload: { wall_ms: 6_888_067 } },
    ] as unknown as FlowRecord[];

    it("resolves to the run's OWN span even though none of its records are in dayRecords", () => {
      const { result } = renderHook(() =>
        usePlaybackTransport(DAY_WINDOW, { kind: "dispatch", sessionId: "s-narrow", records: RUN_RECORDS }),
      );
      expect(result.current.tMin).toBe(Date.parse("2026-09-04T00:11:48.000Z"));
      expect(result.current.tMax).toBe(Date.parse("2026-09-04T02:06:37.000Z"));
      expect(result.current.scrubbed).toBe(false);
      expect(result.current.t).toBe(result.current.tMax);
    });

    it("falls back to the day's own range while the focus's records are still loading, then SNAPS to the run's own span the moment they arrive", () => {
      const { result, rerender } = renderHook(
        ({ records }: { records: FlowRecord[] }) => usePlaybackTransport(DAY_WINDOW, { kind: "dispatch", sessionId: "s-narrow", records }),
        { initialProps: { records: [] as FlowRecord[] } },
      );
      // Still loading (the session fetch hasn't landed yet): the day's own
      // window is the only thing this can report, per the redesign's own
      // "day range or stay inactive" rule for an empty focus.
      expect(result.current.tMin).toBe(Date.parse("2026-09-04T02:43:00.000Z"));
      expect(result.current.tMax).toBe(Date.parse("2026-09-04T04:48:00.000Z"));
      expect(result.current.scrubbed).toBe(false);

      rerender({ records: RUN_RECORDS });

      // The run's own records have arrived: the range narrows...
      expect(result.current.tMin).toBe(Date.parse("2026-09-04T00:11:48.000Z"));
      expect(result.current.tMax).toBe(Date.parse("2026-09-04T02:06:37.000Z"));
      // ...and the PLAYHEAD follows — pinned at the run's own end, not left
      // behind at the day-fallback's old tMax (which is now well OUTSIDE
      // the new range). This is the preserve-or-snap effect treating
      // "focus records arrived" as a range change, not only an id change —
      // the id (`s-narrow`) never changed across this rerender.
      expect(result.current.scrubbed).toBe(false);
      expect(result.current.t).toBe(Date.parse("2026-09-04T02:06:37.000Z"));
    });

    it("a scrubbed position taken while the range was the day-fallback snaps once the real (disjoint) range arrives", () => {
      const { result, rerender } = renderHook(
        ({ records }: { records: FlowRecord[] }) => usePlaybackTransport(DAY_WINDOW, { kind: "dispatch", sessionId: "s-narrow", records }),
        { initialProps: { records: [] as FlowRecord[] } },
      );
      act(() => result.current.scrub(Date.parse("2026-09-04T03:00:00.000Z"))); // inside the day-fallback window
      expect(result.current.scrubbed).toBe(true);
      rerender({ records: RUN_RECORDS });
      // 03:00Z falls outside the run's real 00:11:48Z-02:06:37Z span, so the
      // playhead snaps to the new range's own end rather than keeping a
      // now-meaningless absolute scrub position.
      expect(result.current.t).toBe(Date.parse("2026-09-04T02:06:37.000Z"));
      expect(result.current.scrubbed).toBe(false);
    });
  });
});
