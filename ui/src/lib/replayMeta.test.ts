import { describe, it, expect } from "vitest";
import { primaryReplayMission, replayMissionLabel, replayMetaLines, replayDataSource } from "./replayMeta";
import { normalizeRecords } from "./flow";
import { clk, lday } from "./format";
import type { FlowRecord } from "../types/handwritten";

function rec(overrides: Partial<FlowRecord>): FlowRecord {
  return { ts: "2026-08-07T02:09:42.000Z", ...overrides };
}

/**
 * (#1800) The replay arm of `renderMeta()`. `goldens/playback-date.txt` is the
 * byte-level spec and the parity suite enforces it against a real browser;
 * these cover the branches one recorded day cannot reach (no missions, one
 * mission, exactly the cap) plus the census arithmetic that made the
 * render-model gap visible in the first place.
 */
describe("replayMissionLabel", () => {
  it("names up to two missions in full", () => {
    const data = [rec({ mission_id: "m-a" }), rec({ mission_id: "m-b" })];
    expect(replayMissionLabel(data)).toBe("m-a, m-b");
  });

  it("caps at two and counts the rest — the shape the golden shows", () => {
    const data = ["a", "b", "c", "d", "e"].map((m) => rec({ mission_id: m }));
    expect(replayMissionLabel(data)).toBe("a, b +3 more");
  });

  it("a day with no mission ids reads '—', not an empty label", () => {
    // Real state: a day of unscoped `dispatch` calls has records and no
    // missions. An empty string here would render a bare "◆ " in the meta bar.
    expect(replayMissionLabel([rec({ session_id: "s1" })])).toBe("—");
  });

  it("dedups and keeps first-seen (record) order, which is timestamp order", () => {
    const data = [
      rec({ mission_id: "second", ts: "2026-08-07T05:00:00.000Z" }),
      rec({ mission_id: "first", ts: "2026-08-07T06:00:00.000Z" }),
      rec({ mission_id: "second", ts: "2026-08-07T07:00:00.000Z" }),
    ];
    expect(replayMissionLabel(data)).toBe("second, first");
    expect(primaryReplayMission(data)).toBe("second");
  });
});

describe("primaryReplayMission", () => {
  it("is null when the day has no missions, so the crumb renders empty", () => {
    expect(primaryReplayMission([rec({ session_id: "s1" })])).toBeNull();
  });
});

describe("replayDataSource", () => {
  it("is lowercase 'flow · <date>' — NOT the topbar's 'Flow · <date>'", () => {
    // Two strings, two places, deliberately not derived from each other:
    // `DATA_SOURCE` (viewer.html:3465) goes inside the meta line, `#srcbadge`
    // (3472) is the topbar chip, and legacy capitalizes them differently.
    expect(replayDataSource("2026-08-07")).toBe("flow · 2026-08-07");
  });
});

describe("replayMetaLines", () => {
  const day: FlowRecord[] = [
    rec({ mission_id: "m-a", machine_uid: "u1", session_id: "s1", action: "dispatch.start", ts: "2026-08-07T02:09:42.000Z" }),
    rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.complete", ts: "2026-08-07T18:28:15.000Z" }),
  ];

  it("states the day's span from its OWN records, not the clock", () => {
    const tMin = Date.parse("2026-08-07T02:09:42.000Z");
    const tMax = Date.parse("2026-08-07T18:28:15.000Z");
    const [head] = replayMetaLines(day, "2026-08-07");
    expect(head).toBe(`◆ m-a · flow · 2026-08-07 · ${lday(tMin)} ${clk(tMin)}–${clk(tMax)}`);
  });

  it("the census counts records and DISTINCT machines", () => {
    const [, census] = replayMetaLines(day, "2026-08-07");
    expect(census).toBe("2 records · 1 machines");
  });

  // The bug this line exposed. `useRouteRecords` handed out RAW records, so
  // the schema header — which has no `machine_uid` — counted as a second
  // machine. Nothing else on the page says a machine COUNT out loud, which is
  // why a phantom machine could ride along unnoticed until the meta line had
  // to state it.
  it("the schema header is not a machine (it is dropped before counting)", () => {
    const withHeader = [{ _type: "schema", ts: "" } as unknown as FlowRecord, ...day];
    const [, census] = replayMetaLines(normalizeRecords(withHeader), "2026-08-07");
    expect(census).toBe("2 records · 1 machines");
    // …and read UNSHAPED it really does miscount, which is what shipped.
    const [, wrong] = replayMetaLines(withHeader, "2026-08-07");
    expect(wrong).toBe("3 records · 2 machines");
  });
});

describe("normalizeRecords — the per-session runtime aggregate", () => {
  // `flowToRenderModel` APPENDS one synthetic runtime telemetry record per
  // session that emitted any `dispatch.turn` (viewer.html:3223-3234). These
  // are counted in `DATA.length`, which is why the golden reads 2008 records
  // against a fixture holding 1993 real ones plus 15 such sessions.
  const turns: FlowRecord[] = [
    rec({ session_id: "s1", action: "dispatch.turn", machine_uid: "u1", machine_id: "mac", payload: { turn_seq: 1 }, ts: "2026-08-07T01:00:00.000Z" }),
    rec({ session_id: "s1", action: "dispatch.turn", machine_uid: "u1", machine_id: "mac", payload: { turn_seq: 7 }, ts: "2026-08-07T02:00:00.000Z" }),
    rec({ session_id: "s2", action: "dispatch.turn", machine_uid: "u1", payload: { turn_seq: 3 }, ts: "2026-08-07T03:00:00.000Z" }),
  ];

  it("appends exactly one record per session with turns", () => {
    const out = normalizeRecords(turns);
    expect(out).toHaveLength(turns.length + 2);
    const synthetic = out.filter((r) => r.source === "runtime");
    expect(synthetic).toHaveLength(2);
  });

  it("carries each session's MAX turn_seq", () => {
    const bySession = new Map(
      normalizeRecords(turns)
        .filter((r) => r.source === "runtime")
        .map((r) => [r.session_id, (r.fields as { turns: number }).turns]),
    );
    expect(bySession.get("s1")).toBe(7);
    expect(bySession.get("s2")).toBe(3);
  });

  it("appends NOTHING for a session with no turns — the inverted case", () => {
    // Without this, an implementation that emitted one record per SESSION
    // (rather than per session-with-turns) would pass every test above.
    const noTurns = [rec({ session_id: "s9", action: "dispatch.start" })];
    expect(normalizeRecords(noTurns).filter((r) => r.source === "runtime")).toHaveLength(0);
    expect(normalizeRecords(noTurns)).toHaveLength(1);
  });

  it("returns the whole set in timestamp order — the aggregates are appended out of order", () => {
    const out = normalizeRecords(turns);
    const ts = out.map((r) => Date.parse(r.ts));
    expect(ts).toEqual([...ts].sort((a, b) => a - b));
    // The event log reverses this set and follow-latest takes the topmost row,
    // so both assume temporal order; an unsorted append breaks both quietly.
  });
});
