import { describe, it, expect } from "vitest";
import {
  absorbNewFacetValues,
  activityOf,
  computeFacets,
  createFacetSeen,
  defaultFilterState,
  matchesFilters,
} from "./eventFilters";
import type { FlowRecord } from "../types/handwritten";

function rec(overrides: Partial<FlowRecord>): FlowRecord {
  return { ts: "2026-08-08T12:00:00.000Z", category: "work", action: "dispatch.reasoning", ...overrides };
}

describe("activityOf", () => {
  it("maps the full legacy set, including branches the old row-label subset omitted", () => {
    expect(activityOf(rec({ action: "session.end" }))).toBe("session end");
    expect(activityOf(rec({ action: "machine.online" }))).toBe("machine online");
    expect(activityOf(rec({ action: "note" }))).toBe("note");
    expect(activityOf(rec({ action: undefined, source: "orchestrator" }))).toBe("note");
  });
});

describe("computeFacets / defaultFilterState", () => {
  it("derives facets from records, and defaultFilterState includes every one of them", () => {
    const records = [rec({ tier: "local", source: "lms" }), rec({ tier: "cloud", category: "telemetry", source: "lms" })];
    const facets = computeFacets(records);
    expect(facets.tier.sort()).toEqual(["cloud", "local"]);
    const filters = defaultFilterState(facets);
    expect(filters.tier.has("local")).toBe(true);
    expect(filters.tier.has("cloud")).toBe(true);
    expect(filters.q).toBe("");
  });
});

describe("matchesFilters", () => {
  it("excludes a record whose facet value is present but unchecked", () => {
    const facets = computeFacets([rec({ tier: "local" }), rec({ tier: "cloud" })]);
    const filters = defaultFilterState(facets);
    filters.tier.delete("cloud");
    expect(matchesFilters(rec({ tier: "local" }), filters)).toBe(true);
    expect(matchesFilters(rec({ tier: "cloud" }), filters)).toBe(false);
  });

  it("never excludes on a facet the record simply doesn't carry", () => {
    const filters = defaultFilterState({ act: ["reasoning"], cat: [], tier: [], src: [] });
    // `rec()` defaults `category: "work"` — override it too, since the
    // point of this test is a facet the record genuinely lacks (tier, here)
    // being absent from the checkbox model entirely, not category (which
    // this test's `filters.cat` also leaves empty and would otherwise
    // exclude on for an unrelated reason).
    expect(matchesFilters(rec({ category: undefined, tier: undefined }), filters)).toBe(true);
  });

  it("free-text search matches across the whole record", () => {
    const filters = defaultFilterState(computeFacets([rec({ session_id: "s-alpha" })]));
    filters.q = "alpha";
    expect(matchesFilters(rec({ session_id: "s-alpha" }), filters)).toBe(true);
    expect(matchesFilters(rec({ session_id: "s-beta" }), filters)).toBe(false);
  });
});

describe("absorbNewFacetValues — viewer.html's absorbNewFilterValues()/SEEN (#1640 live-tail bug)", () => {
  it("auto-includes a facet value the first time it's ever seen", () => {
    const seen = createFacetSeen();
    let filters = defaultFilterState({ act: [], cat: [], tier: [], src: [] }); // the empty-at-mount snapshot
    const facets = computeFacets([rec({ tier: "local" })]);
    filters = absorbNewFacetValues(filters, facets, seen);
    expect(filters.tier.has("local")).toBe(true);
  });

  // RED-PROVED: this is the exact regression `tests/parity/next-parity-live.spec.ts`
  // caught — a record whose activity is brand new (never seen before) must
  // still be counted once absorbed, or a live-streamed record silently
  // vanishes from the log the instant it introduces a new activity type.
  // Confirmed by temporarily making `absorbNewFacetValues` a no-op (`return
  // filters;` as its first line) and re-running this test, which then
  // failed on the second `matchesFilters` assertion; restored afterward.
  it("a record with a brand-new activity value is absorbed and then matches", () => {
    const seen = createFacetSeen();
    let filters = defaultFilterState(computeFacets([rec({ action: "dispatch.reasoning" })]));
    seen.act.add("reasoning"); // mark the initial batch as already-seen, matching real usage
    filters = absorbNewFacetValues(filters, computeFacets([rec({ action: "dispatch.reasoning" })]), seen);

    // A new record arrives with an activity that was never in the initial batch.
    const newRecord = rec({ action: "flow.note", source: "orchestrator" });
    const facetsWithNew = computeFacets([rec({ action: "dispatch.reasoning" }), newRecord]);
    filters = absorbNewFacetValues(filters, facetsWithNew, seen);

    expect(filters.act.has("note")).toBe(true);
    expect(matchesFilters(newRecord, filters)).toBe(true);
  });

  it("does NOT re-include an already-seen value the operator explicitly unchecked", () => {
    const seen = createFacetSeen();
    let filters = defaultFilterState(computeFacets([rec({ tier: "cloud" })]));
    filters = absorbNewFacetValues(filters, computeFacets([rec({ tier: "cloud" })]), seen);
    filters = { ...filters, tier: new Set(filters.tier) };
    filters.tier.delete("cloud"); // operator unchecks it

    // The SAME value shows up again in a later record set — already seen,
    // so it must stay excluded.
    filters = absorbNewFacetValues(filters, computeFacets([rec({ tier: "cloud" })]), seen);
    expect(filters.tier.has("cloud")).toBe(false);
  });

  it("returns the SAME filters reference when nothing new appeared (no spurious re-render)", () => {
    const seen = createFacetSeen();
    const facets = computeFacets([rec({ tier: "local" })]);
    let filters = defaultFilterState(facets);
    filters = absorbNewFacetValues(filters, facets, seen);
    const again = absorbNewFacetValues(filters, facets, seen);
    expect(again).toBe(filters);
  });
});
