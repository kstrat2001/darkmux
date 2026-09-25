import { describe, it, expect } from "vitest";
import { buildActivityTimeline } from "./timeline";
import { clkhm } from "../../lib/format";
import type { FlowRecord } from "../../types/handwritten";

function rec(overrides: Partial<FlowRecord>): FlowRecord {
  return { ts: "2026-08-08T00:00:00.000Z", ...overrides };
}

const TMAX = Date.parse("2026-08-08T16:40:59.000Z");
const iso = (offsetMin: number) => new Date(TMAX + offsetMin * 60000).toISOString();

// Expected header/axis text is computed via the SAME `clkrange`/`clkhm`
// helpers `buildActivityTimeline` itself delegates to (`lib/format.ts`),
// rather than a hardcoded literal — `toLocaleTimeString`/`toLocaleDateString`
// read the process's local timezone (the parity harness pins `UTC` inside
// its own Playwright context; this vitest suite has no such pin), so a
// literal "16:40:59" would pass only in a UTC-local CI runner and fail
// everywhere else. This still exercises the real behavior (the exact
// tlMin/tlMax this function derives, the 24h-vs-1h window math, the
// same-day-vs-straddling branch) — it just reads the expected clock text
// from the same clock the implementation uses, instead of assuming a TZ.
describe("buildActivityTimeline — header text", () => {
  // (operator, 2026-09-01) The header no longer carries `· <clkrange>`, so
  // its two date-format tests moved to `lib/format.test.ts` and now exercise
  // `clkrange` directly. They were always format tests using this header as a
  // vehicle; `replayMeta.ts` still renders `clkrange`, so the coverage had to
  // move rather than go with the header.
  it("is the bare title — the axis carries the times, the masthead chip the day", () => {
    const tl = buildActivityTimeline([], new Map(), [], new Set(), TMAX, TMAX, 1440);
    expect(tl.headerText).toBe("recent activity");
  });

  it("the axis has three points: window start, midpoint, window end (via clkhm)", () => {
    const windowMs = 1440 * 60000;
    const tlMin = TMAX - windowMs;
    const span = TMAX - tlMin;
    const tl = buildActivityTimeline([], new Map(), [], new Set(), TMAX, TMAX, 1440);
    expect(tl.axis).toEqual([clkhm(tlMin), clkhm(tlMin + span / 2), clkhm(TMAX)]);
  });
});

describe("buildActivityTimeline — lanes and bars", () => {
  const uids = ["m1"];
  const liveSet = new Set(["s1"]);
  const data: FlowRecord[] = [
    // s1: still running (∈ liveSet) — open-ended bar.
    rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.start", ts: iso(-30), handle: "coder" }),
    // s2: cleanly completed, not live — "done".
    rec({ machine_uid: "m1", session_id: "s2", action: "dispatch.start", ts: iso(-50) }),
    rec({ machine_uid: "m1", session_id: "s2", action: "dispatch.complete", ts: iso(-40) }),
    // s3: watchdog-killed, not live — "err".
    rec({ machine_uid: "m1", session_id: "s3", action: "dispatch.start", ts: iso(-45) }),
    rec({ machine_uid: "m1", session_id: "s3", action: "dispatch.error", ts: iso(-35), payload: { exit_code: 137 } }),
    // s4: abandoned (only a session.end close-edge, no dispatch terminal), not live — "canceled".
    rec({ machine_uid: "m1", session_id: "s4", action: "dispatch.start", ts: iso(-25) }),
    rec({ machine_uid: "m1", session_id: "s4", action: "session.end", ts: iso(-20) }),
    // s5: entirely before the 1h window — dropped, not a bar at all.
    rec({ machine_uid: "m1", session_id: "s5", action: "dispatch.start", ts: iso(-1440) }),
    rec({ machine_uid: "m1", session_id: "s5", action: "dispatch.complete", ts: iso(-1430) }),
  ];

  it("builds one lane per machine uid, named via nameOf", () => {
    const tl = buildActivityTimeline(data, new Map(), uids, liveSet, TMAX, TMAX, 60);
    expect(tl.lanes).toHaveLength(1);
    expect(tl.lanes[0].uid).toBe("m1");
    expect(tl.lanes[0].name).toBe("m1"); // no machine_id record and no beat -> falls back to the uid itself
  });

  it("classifies a still-live session as 'run', open-ended to the window's right edge", () => {
    const tl = buildActivityTimeline(data, new Map(), uids, liveSet, TMAX, TMAX, 60);
    const bar = tl.lanes[0].bars.find((b) => b.sid === "s1")!;
    expect(bar.cls).toBe("run");
    expect(bar.title).toContain("running");
  });

  it("classifies a clean dispatch.complete as 'done'", () => {
    const tl = buildActivityTimeline(data, new Map(), uids, liveSet, TMAX, TMAX, 60);
    const bar = tl.lanes[0].bars.find((b) => b.sid === "s2")!;
    expect(bar.cls).toBe("done");
    expect(bar.title).toContain("complete");
  });

  it("classifies a watchdog-killed dispatch.error (exit 137) as 'err'/killed", () => {
    const tl = buildActivityTimeline(data, new Map(), uids, liveSet, TMAX, TMAX, 60);
    const bar = tl.lanes[0].bars.find((b) => b.sid === "s3")!;
    expect(bar.cls).toBe("err");
    expect(bar.title).toContain("killed");
  });

  it("classifies an abandoned session (session.end, no dispatch terminal) as 'canceled'", () => {
    const tl = buildActivityTimeline(data, new Map(), uids, liveSet, TMAX, TMAX, 60);
    const bar = tl.lanes[0].bars.find((b) => b.sid === "s4")!;
    expect(bar.cls).toBe("canceled");
  });

  it("drops a session that ended entirely before the window — no bar at all", () => {
    const tl = buildActivityTimeline(data, new Map(), uids, liveSet, TMAX, TMAX, 60);
    expect(tl.lanes[0].bars.find((b) => b.sid === "s5")).toBeUndefined();
  });

  it("clips a window-straddling bar's start to the window's left edge (never negative)", () => {
    const straddling: FlowRecord[] = [
      // Started well before the 1h window, still running.
      rec({ machine_uid: "m1", session_id: "s6", action: "dispatch.start", ts: iso(-300) }),
    ];
    const tl = buildActivityTimeline(straddling, new Map(), uids, new Set(["s6"]), TMAX, TMAX, 60);
    const bar = tl.lanes[0].bars[0];
    expect(bar.leftPct).toBe(0);
  });
});

/**
 * (#1800 P2) The REPLAY arm. Until this packet `buildActivityTimeline` had
 * only the live one, because `/next` had no historical route that reached it
 * — and `Math.max(tMax, nowMs)` on a recorded day is `nowMs` by definition,
 * so a 2026-08-07 page drew today's axis with every bar filtered out for
 * falling before `tlMin`.
 */
// (Playback parity, Change A, finding #8 — 2026-09-24, operator decision)
// This describe block used to be "replay (liveMode = false)" and asserted
// the OLD divergent behavior: replay drew the recorded day's own fixed span
// with no window control, headed "activity" (no "recent"), while live drew a
// rolling window ending at `now`. That is a parity defect, not a feature —
// a replay now draws the SAME rolling window as live, anchored at the
// playhead (`tlMax = playheadT` unconditionally), same header, same window
// control; `liveMode`/`tMin`/`nowMs` no longer affect any of it (kept as
// dead parameters — see `timeline.ts`'s own doc — so existing positional
// call sites don't need a rewrite). This block is rewritten to prove that.
describe("buildActivityTimeline — parity: liveMode no longer changes the window, header, or verdicts", () => {
  const TMIN = Date.parse("2026-08-08T02:09:42.000Z");
  const uids = ["m1"];
  const day: FlowRecord[] = [
    rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.start", ts: new Date(TMIN).toISOString(), handle: "coder" }),
    rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.complete", ts: new Date(TMAX).toISOString() }),
  ];

  it("the rolling window is anchored at the playhead — identical axis/header/bars whichever way `liveMode` is passed", () => {
    // `TMAX - TMIN` is ~14.5h, inside the 24h (1440min) window, so the
    // window ending AT the playhead (TMAX, the default) covers the whole
    // recorded span here — same as it would live, probed at the same
    // instant. `_nowMs`/`_liveMode`/`_tMin` (positions 6/8/9) are passed as
    // their old values on purpose, to prove they are now inert.
    const replay = buildActivityTimeline(day, new Map(), uids, new Set(), TMAX, TMAX, 1440, false, TMIN);
    const live = buildActivityTimeline(day, new Map(), uids, new Set(), TMAX, TMAX, 1440, true, TMIN);
    expect(replay.headerText).toBe("recent activity");
    expect(live.headerText).toBe("recent activity");
    expect(replay.axis).toEqual(live.axis);
    expect(replay.lanes).toEqual(live.lanes);
    expect(replay.lanes[0].bars.map((b) => b.sid)).toEqual(["s1"]);
  });

  it("a closed session reads 'done', not 'run', regardless of the liveMode flag", () => {
    const tl = buildActivityTimeline(day, new Map(), uids, new Set(), TMAX, TMAX, 1440, false, TMIN);
    expect(tl.lanes[0].bars[0].cls).toBe("done");
  });

  it("an UNCLOSED session, fresh as of the playhead, reads 'run' regardless of the liveMode flag", () => {
    const open: FlowRecord[] = [
      rec({ machine_uid: "m1", session_id: "s9", action: "dispatch.start", ts: new Date(TMIN).toISOString() }),
      rec({ machine_uid: "m1", session_id: "s9", action: "dispatch.turn", ts: new Date(TMAX).toISOString() }),
    ];
    const replay = buildActivityTimeline(open, new Map(), uids, new Set(), TMAX, TMAX, 1440, false, TMIN);
    expect(replay.lanes[0].bars[0].cls).toBe("run");
    // Same data, same empty presence set, the OTHER value of the now-inert
    // `liveMode` flag: the SAME verdict, which is the parity fix (this used
    // to be the one test proving `liveMode` was "genuinely load-bearing" —
    // it no longer is, by design).
    const live = buildActivityTimeline(open, new Map(), uids, new Set(), TMAX, TMAX, 1440, true, TMIN);
    expect(live.lanes[0].bars[0].cls).toBe("run");
  });

  // (#1869) Omitting the 10th argument (`playheadT`) defaults it to `tMax`
  // — the exact pre-transport behavior, where a session start could never
  // be after the day's true max, so legacy's own
  // `if(!s||T(s.ts)>state.t)return"";` guard was dropped as an
  // unconditional no-op. A scrubbable playhead makes it reachable: a
  // session starting AFTER `tMax` (here, one minute past the day's true
  // ceiling — a stand-in for "hasn't happened yet as of the playhead")
  // must draw no bar at all, not a phantom sliver at the track's right edge
  // (which is what `sessionRunning` finding no close-edge — because
  // there's nothing to close yet — would otherwise produce).
  it("a session that hasn't started yet as of the playhead draws no bar at all", () => {
    const notYetStarted: FlowRecord[] = [
      rec({ machine_uid: "m1", session_id: "s-future", action: "dispatch.start", ts: new Date(TMAX + 60000).toISOString() }),
    ];
    const tl = buildActivityTimeline(notYetStarted, new Map(), uids, new Set(), TMAX, TMAX, 1440, false, TMIN);
    expect(tl.lanes[0].bars).toHaveLength(0);
  });

  // (Playback parity, Change A, finding #8) This used to be "scrubbing the
  // playhead back to tMin does NOT collapse the axis — it stays the day's
  // whole span", the regression test for the OLD "replay draws a fixed
  // day-span" behavior. Under the parity fix that premise is gone BY
  // DESIGN: the window now follows the playhead exactly like live's does,
  // so scrubbing back MOVES the whole window left with it — the same
  // behavior a live viewer gets from watching a session recede out of a
  // rolling window as time passes. The axis is expected to be centered on
  // `TMIN`, not pinned to the day's original span.
  it("scrubbing the playhead back to tMin MOVES the rolling window with it (parity — not the old fixed day-span)", () => {
    const tl = buildActivityTimeline(day, new Map(), uids, new Set(), TMAX, TMAX, 1440, false, TMIN, TMIN);
    const winMs = 1440 * 60000;
    expect(tl.axis).toEqual([clkhm(TMIN - winMs), clkhm(TMIN - winMs / 2), clkhm(TMIN)]);
    // The marker sits at the window's own right edge now — this IS the live
    // edge behavior, applied uniformly.
    expect(tl.playheadPct).toBe(100);
  });
});

/**
 * (#2125) A review mission's per-step session id is a FIXED string
 * (`task-review-probe-low-task` etc, `session_id::task(&step.task_id)` —
 * `crates/darkmux-crew/src/step_kinds/builtins.rs::DispatchMapStepKind`),
 * reused verbatim by every review run. The operator's real report: an
 * OLDER review mission ran that exact step to completion ~17h before a
 * NEWER one started, and the newer mission's abort got matched against the
 * OLDER mission's `dispatch.start` — one 20-hour "canceled" bar where two
 * short, correctly-bounded ones belonged. `sessionRunsOn` (keyed on
 * (session_id, mission_id), not session_id alone) plus `missionId` threaded
 * into `dispatchRec`/`dispatchEnd`/`sessEnd` is the fix under test.
 */
describe("buildActivityTimeline — reused step session ids across missions (#2125)", () => {
  const uids = ["m1"];
  const REUSED_SID = "task-review-probe-low-task";
  const data: FlowRecord[] = [
    // Older mission (~17h ago): ran the SAME step id to a clean completion.
    rec({
      machine_uid: "m1",
      session_id: REUSED_SID,
      mission_id: "review-older",
      action: "dispatch.start",
      ts: iso(-1020),
      handle: "review-probe-low",
    }),
    rec({ machine_uid: "m1", session_id: REUSED_SID, mission_id: "review-older", action: "dispatch.complete", ts: iso(-1000) }),
    // Newer mission (~23 min ago): the SAME step id, a DIFFERENT mission —
    // started recently and got aborted (dispatch.error) shortly after.
    rec({
      machine_uid: "m1",
      session_id: REUSED_SID,
      mission_id: "review-newer",
      action: "dispatch.start",
      ts: iso(-23),
      handle: "review-probe-low",
    }),
    rec({ machine_uid: "m1", session_id: REUSED_SID, mission_id: "review-newer", action: "dispatch.error", ts: iso(-20) }),
  ];

  it("draws TWO separate bars, not one span stretching across both missions", () => {
    const tl = buildActivityTimeline(data, new Map(), uids, new Set(), TMAX, TMAX, 1440);
    const bars = tl.lanes[0].bars.filter((b) => b.sid === REUSED_SID);
    expect(bars).toHaveLength(2);
  });

  it("gives each bar its OWN mission's start/end, not a cross-mission blend", () => {
    const tl = buildActivityTimeline(data, new Map(), uids, new Set(), TMAX, TMAX, 1440);
    const bars = tl.lanes[0].bars.filter((b) => b.sid === REUSED_SID);
    const widths = bars.map((b) => b.widthPct).sort((a, b) => a - b);
    // Both spans are short (~20 min / 17h-worth would be wildly wider) —
    // the widest of the two must still be a small fraction of the 24h
    // window, not the ~17h span a cross-mission match would draw.
    const windowMs = 1440 * 60000;
    const seventeenHoursPct = ((17 * 60 * 60000) / windowMs) * 100;
    for (const w of widths) {
      expect(w).toBeLessThan(seventeenHoursPct / 2);
    }
  });

  it("each bar carries a distinct React key even though sid is shared", () => {
    const tl = buildActivityTimeline(data, new Map(), uids, new Set(), TMAX, TMAX, 1440);
    const bars = tl.lanes[0].bars.filter((b) => b.sid === REUSED_SID);
    expect(new Set(bars.map((b) => b.key)).size).toBe(2);
  });

  it("the older mission's bar reads 'done' (clean complete), the newer's reads 'err' (aborted) — not both 'canceled' off a blended close-edge", () => {
    const tl = buildActivityTimeline(data, new Map(), uids, new Set(), TMAX, TMAX, 1440);
    const bars = tl.lanes[0].bars.filter((b) => b.sid === REUSED_SID);
    const classes = bars.map((b) => b.cls).sort();
    expect(classes).toEqual(["done", "err"]);
  });

  it("a session with NO mission_id at all keeps its exact prior (unscoped) behavior", () => {
    const bare: FlowRecord[] = [
      rec({ machine_uid: "m1", session_id: "bare-1", action: "dispatch.start", ts: iso(-10) }),
      rec({ machine_uid: "m1", session_id: "bare-1", action: "dispatch.complete", ts: iso(-5) }),
    ];
    const tl = buildActivityTimeline(bare, new Map(), uids, new Set(), TMAX, TMAX, 1440);
    expect(tl.lanes[0].bars).toHaveLength(1);
    expect(tl.lanes[0].bars[0].key).toBe("bare-1");
  });
});

describe("(#2890) a replay's timeline spans the recording", () => {
  const t0 = Date.parse("2026-08-26T10:00:00.000Z");
  const t1 = Date.parse("2026-08-26T10:34:00.000Z");
  const rec = (ts: number, action: string) =>
    ({ ts: new Date(ts).toISOString(), machine_uid: "u1", machine_id: "m5", session_id: "s1", action }) as unknown as FlowRecord;
  it("a fixed range sets the axis to the recording and starts a bar at the left edge", () => {
    const data = [rec(t0, "dispatch.start"), rec(t0 + 10 * 60_000, "dispatch.complete")];
    const tl = buildActivityTimeline(data, new Map(), ["u1"], new Set(), t1, t1, 60, false, t0, t1, [t0, t1]);
    expect(tl.axis).toEqual([clkhm(t0), clkhm(t0 + (t1 - t0) / 2), clkhm(t1)]);
    expect(tl.lanes[0].bars[0].leftPct).toBe(0);
  });
  it("without a range the window still rolls back from the playhead", () => {
    const data = [rec(t0, "dispatch.start"), rec(t0 + 10 * 60_000, "dispatch.complete")];
    const tl = buildActivityTimeline(data, new Map(), ["u1"], new Set(), t1, t1, 60, false, t0, t1);
    expect(tl.axis[0]).toBe(clkhm(t1 - 60 * 60_000));
  });
});

