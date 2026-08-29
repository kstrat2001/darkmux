import { describe, it, expect } from "vitest";
import {
  primaryReplayMission,
  replayMissionLabel,
  replayMetaLines,
  replayMetaParts,
  replayDataSource,
  humanMissionLabel,
  missionTitle,
  resolvedMissionLabel,
  replayPlaybackKvValue,
} from "./replayMeta";
import { normalizeRecords } from "./flow";
import { clk, clkrange, lday } from "./format";
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

/** (#2073) The meta line's PARTS, so a phone can show only what no other
 * chrome carries (the mission and the census) while desktop keeps the full
 * line. The lines must stay the composition of the parts, or the two
 * viewports would disagree about the day. */
describe("replayMetaParts", () => {
  it("composes to exactly the two lines replayMetaLines renders", () => {
    const data = [
      { ts: "2026-08-26T01:00:00Z", machine_uid: "u1", mission_id: "m-one" },
      { ts: "2026-08-26T02:00:00Z", machine_uid: "u2", mission_id: "m-one" },
    ] as never[];
    const parts = replayMetaParts(data, "2026-08-26");
    expect(parts.head).toBe("◆ m-one");
    expect(parts.census).toBe("2 records · 2 machines");
    expect(parts.source).toBe("flow · 2026-08-26");
    expect(`${parts.head} · ${parts.source} · ${parts.span}`).toBe(replayMetaLines(data, "2026-08-26")[0]);
    expect(parts.census).toBe(replayMetaLines(data, "2026-08-26")[1]);
  });
});

/** (#2120) A human label for a mission id, read off the id's own naming
 * convention — never a fetch. Only the `review` convention is recognized
 * today; everything else returns `null` so the caller can fall back (the
 * modal, to the raw id) or omit the label (the transport, per the
 * operator's "raw id lives only in the modal" call). */
describe("humanMissionLabel", () => {
  it("reads a review-config id's trailing slug as a title", () => {
    expect(humanMissionLabel("demo-review-nameof-recency")).toBe("Review · nameof recency");
  });

  it("works with no prefix before the review token", () => {
    expect(humanMissionLabel("review-quarterly-audit")).toBe("Review · quarterly audit");
  });

  it("falls back to a bare 'Review' when the token has no trailing slug", () => {
    expect(humanMissionLabel("demo-review")).toBe("Review");
  });

  it("returns null for an id with no recognizable convention — the caller decides the fallback", () => {
    expect(humanMissionLabel("coder-phase-1786068582-93f404")).toBeNull();
    expect(humanMissionLabel("acp-ephemeral-pr-ship-123")).toBeNull();
  });
});

/** (#2121) A REAL title, read off `mission_title` — the demo's
 * `import_mission.py --title` writer, never a production one today (see
 * `mission_title`'s own doc on `FlowRecord`). */
describe("missionTitle", () => {
  it("finds the title on any correlated record, not just the first", () => {
    const data = [
      rec({ mission_id: "demo-review-nameof-recency", action: "phase start" }),
      rec({ mission_id: "demo-review-nameof-recency", action: "mission start", mission_title: "Review of a merged darkmux PR" }),
      rec({ mission_id: "demo-review-nameof-recency", action: "dispatch start" }),
    ];
    expect(missionTitle(data, "demo-review-nameof-recency")).toBe("Review of a merged darkmux PR");
  });

  it("is null when no correlated record carries a title — every real dispatch today", () => {
    const data = [rec({ mission_id: "coder-phase-1786068582-93f404", action: "mission start" })];
    expect(missionTitle(data, "coder-phase-1786068582-93f404")).toBeNull();
  });

  it("never matches a title stamped on a DIFFERENT mission's record", () => {
    const data = [rec({ mission_id: "other-mission", mission_title: "Wrong Title" })];
    expect(missionTitle(data, "demo-review-nameof-recency")).toBeNull();
  });
});

/** (#2121) The transport's resolved label — title present -> title; title
 * absent -> [[humanMissionLabel]]'s id-derived heuristic; neither -> null
 * (the transport omits the label entirely, same as before #2121). */
describe("resolvedMissionLabel", () => {
  it("prefers the real title over the id-derived heuristic", () => {
    const data = [rec({ mission_id: "demo-review-nameof-recency", mission_title: "Review of a merged darkmux PR" })];
    expect(resolvedMissionLabel(data, "demo-review-nameof-recency")).toBe("Review of a merged darkmux PR");
    // Sanity: the heuristic alone would have produced a DIFFERENT string —
    // proving this test exercises the title branch, not a coincidence.
    expect(humanMissionLabel("demo-review-nameof-recency")).not.toBe("Review of a merged darkmux PR");
  });

  it("falls back to humanMissionLabel when no title is present — live daemon routes, unaffected", () => {
    const data = [rec({ mission_id: "demo-review-nameof-recency" })];
    expect(resolvedMissionLabel(data, "demo-review-nameof-recency")).toBe(humanMissionLabel("demo-review-nameof-recency"));
  });

  it("is null when the id has neither a title nor a recognizable convention", () => {
    const data = [rec({ mission_id: "coder-phase-1786068582-93f404" })];
    expect(resolvedMissionLabel(data, "coder-phase-1786068582-93f404")).toBeNull();
  });
});

/** (#2120) The Machine info modal's `playback` kv row — the day/span/
 * census/raw-id information the sticky row's folded `#meta` summary used
 * to carry, now that the transport shows only a human mission label (or
 * nothing) in its place. */
describe("replayPlaybackKvValue", () => {
  const day: FlowRecord[] = [
    { ts: "2026-08-26T01:08:17.000Z", machine_uid: "u1", mission_id: "demo-review-nameof-recency" } as FlowRecord,
    { ts: "2026-08-26T14:13:01.000Z", machine_uid: "u1", mission_id: "demo-review-nameof-recency" } as FlowRecord,
    { ts: "2026-08-26T09:00:00.000Z", machine_uid: "u2", mission_id: "demo-review-nameof-recency" } as FlowRecord,
  ];

  it("names the day, the bare time span (no repeated date), the census, and the raw mission id", () => {
    const tMin = Date.parse("2026-08-26T01:08:17.000Z");
    const tMax = Date.parse("2026-08-26T14:13:01.000Z");
    expect(replayPlaybackKvValue(day, "2026-08-26")).toBe(
      `flow 2026-08-26 · ${clkrange(tMin, tMax)} · 3 records · 2 machines · mission demo-review-nameof-recency`,
    );
  });

  it("omits the mission clause entirely when the day has no mission ids", () => {
    const noMission: FlowRecord[] = [{ ts: "2026-08-26T01:08:17.000Z", machine_uid: "u1" } as FlowRecord];
    expect(replayPlaybackKvValue(noMission, "2026-08-26")).toBe(`flow 2026-08-26 · ${clkrange(Date.parse("2026-08-26T01:08:17.000Z"), Date.parse("2026-08-26T01:08:17.000Z"))} · 1 records · 1 machines`);
    expect(replayPlaybackKvValue(noMission, "2026-08-26")).not.toContain("mission");
  });

  // (#2121) "Ids remain in detail panes ... the Machine info playback row" —
  // this row is the one exception `resolvedMissionLabel` deliberately does
  // NOT reach: even when the mission carries a real `mission_title`, this
  // kv value still names the RAW id, unchanged. The transport is the only
  // surface that swaps to the title.
  it("still names the raw id even when the mission carries a mission_title", () => {
    const titled: FlowRecord[] = day.map((r) => ({ ...r, mission_title: "Review of a merged darkmux PR" }));
    const value = replayPlaybackKvValue(titled, "2026-08-26");
    expect(value).toContain("mission demo-review-nameof-recency");
    expect(value).not.toContain("Review of a merged darkmux PR");
  });

  // (#2121) `mission_reviewed` — additional detail-pane content, appended
  // after the raw id, never replacing it.
  it("appends the reviewed PR reference when a correlated record carries mission_reviewed", () => {
    const reviewed: FlowRecord[] = day.map((r, i) => (i === 1 ? { ...r, mission_reviewed: "kstrat2001/darkmux#2030" } : r));
    const value = replayPlaybackKvValue(reviewed, "2026-08-26");
    expect(value.endsWith("· mission demo-review-nameof-recency · reviewed kstrat2001/darkmux#2030")).toBe(true);
  });

  it("omits the reviewed clause when no correlated record carries it — every real dispatch today", () => {
    expect(replayPlaybackKvValue(day, "2026-08-26")).not.toContain("reviewed");
  });
});
