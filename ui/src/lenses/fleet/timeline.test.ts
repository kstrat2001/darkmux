import { describe, it, expect } from "vitest";
import { buildActivityTimeline } from "./timeline";
import { clkhm, clkrange } from "../../lib/format";
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
  it("a 24h window straddling a day boundary prefixes each end with its short date", () => {
    const windowMs = 1440 * 60000;
    const tl = buildActivityTimeline([], new Map(), [], new Set(), TMAX, TMAX, 1440);
    expect(tl.headerText).toBe(`recent activity · ${clkrange(TMAX - windowMs, TMAX)}`);
    // A real 24h span crosses a calendar day boundary in EVERY timezone —
    // assert the date-prefixed shape rather than a literal, so this red-
    // proves against `clkrange` silently collapsing to the bare form.
    const range = tl.headerText.split("· ")[1];
    expect(range).toMatch(/–/);
    expect(range.split("–")[0]).toMatch(/^[A-Za-z]{3} \d{1,2} \d{2}:\d{2}:\d{2}$/);
  });

  it("a same-day (1h) window stays bare HH:MM:SS-HH:MM:SS", () => {
    // Anchored at local noon so a 1h window can't straddle local midnight
    // in any real timezone — a TZ-safe way to force the SAME-day branch.
    const localNoon = new Date();
    localNoon.setHours(12, 0, 0, 0);
    const anchor = localNoon.getTime();
    const windowMs = 60 * 60000;
    const tl = buildActivityTimeline([], new Map(), [], new Set(), anchor, anchor, 60);
    expect(tl.headerText).toBe(`recent activity · ${clkrange(anchor - windowMs, anchor)}`);
    // The same-day form has no letters (no month abbreviation) before the
    // en-dash — red-proves against the date-prefixed branch firing instead.
    const range = tl.headerText.split("· ")[1];
    expect(range.split("–")[0]).not.toMatch(/[A-Za-z]/);
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
