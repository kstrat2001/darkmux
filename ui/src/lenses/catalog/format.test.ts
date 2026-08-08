import { describe, it, expect } from "vitest";
import { daySummary, missionSummary, missionsHeader, CATALOG_MISSION_CAP } from "./format";
import type { FlowDay, FlowMissionSummary } from "../../types/handwritten";

function day(overrides: Partial<FlowDay> = {}): FlowDay {
  return { date: "2026-08-08", records: 10, dispatches: 2, missions: [], ...overrides };
}

function mission(overrides: Partial<FlowMissionSummary> = {}): FlowMissionSummary {
  return {
    mission_id: "m1",
    records: 10,
    dispatches: 2,
    machines: [],
    first_ts: "2026-08-08T00:00:00Z",
    last_ts: "2026-08-08T01:00:00Z",
    first_date: "2026-08-08",
    last_date: "2026-08-08",
    ...overrides,
  };
}

describe("daySummary", () => {
  it("pluralizes dispatch/record counts and omits the mission preview when there are none", () => {
    expect(daySummary(day({ dispatches: 1, records: 1, missions: [] }))).toBe("1 dispatch · 1 record");
    expect(daySummary(day({ dispatches: 0, records: 0, missions: [] }))).toBe("0 dispatches · 0 records");
  });

  it("previews a single mission with the singular label", () => {
    expect(daySummary(day({ missions: ["only-one"] }))).toBe("2 dispatches · 10 records · mission only-one");
  });

  it("comma-joins up to 3 missions with the plural label, no +N suffix at exactly 3", () => {
    expect(daySummary(day({ missions: ["a", "b", "c"] }))).toBe("2 dispatches · 10 records · missions a, b, c");
  });

  it("caps the preview at 3 and appends a +N count beyond that — matching viewer.html's toggleCatalog() verbatim", () => {
    expect(daySummary(day({ missions: ["a", "b", "c", "d", "e"] }))).toBe(
      "2 dispatches · 10 records · missions a, b, c +2",
    );
  });
});

describe("missionSummary", () => {
  it("shows a single date when first_date === last_date", () => {
    expect(missionSummary(mission({ first_date: "2026-08-08", last_date: "2026-08-08" }))).toBe(
      "2 dispatches · 10 records · 2026-08-08",
    );
  });

  it("shows a date range when the mission spans days", () => {
    expect(missionSummary(mission({ first_date: "2026-08-06", last_date: "2026-08-08" }))).toBe(
      "2 dispatches · 10 records · 2026-08-06–2026-08-08",
    );
  });

  it("pluralizes dispatch/record counts at 1", () => {
    expect(missionSummary(mission({ dispatches: 1, records: 1 }))).toBe("1 dispatch · 1 record · 2026-08-08");
  });
});

describe("missionsHeader", () => {
  it("names the default cap when the mission count would truncate it", () => {
    expect(missionsHeader(155)).toBe(`missions · newest ${CATALOG_MISSION_CAP} of 155`);
  });

  it("is plain when the count is at or under the cap", () => {
    expect(missionsHeader(CATALOG_MISSION_CAP)).toBe("missions");
    expect(missionsHeader(3)).toBe("missions");
  });
});
