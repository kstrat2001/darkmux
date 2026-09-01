import { describe, it, expect } from "vitest";
import {
  absorbNewFacetValues,
  activityOf,
  computeFacets,
  createFacetSeen,
  defaultFilterState,
  matchesFilters,
  MODEL_ACTIVITIES,
  activeFilterCount,
  restoreFilterState,
  persistFilterState,
  sortUnmappedActivities,
} from "./eventFilters";
import type { Facets } from "./eventFilters";
import { onlyModelFacet } from "../components/FiltersDialog";
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

  // (#2116) On a busy day `activityOf()`'s own fallback — `return a ||
  // "other"` — passes still-unmapped `r.action` strings straight through,
  // and those used to land at the END of the activity facet in bare
  // Set-iteration (first-seen) order. This proves the replacement:
  // grouped by namespace prefix, alphabetical within a group, regardless
  // of arrival order.
  it("groups still-unmapped activity strings by namespace prefix, after the known ACT_ORDER entries", () => {
    const records = [
      rec({ action: "hook.enqueue" }),
      rec({ action: "step.dispatch" }),
      rec({ action: "mission.debrief" }),
      rec({ action: "mission.start" }),
      rec({ action: "phase.begin" }),
      rec({ action: "dispatch.reasoning" }), // a KNOWN activity — "reasoning"
    ];
    const facets = computeFacets(records);
    // "reasoning" is a known ACT_ORDER entry and sorts first regardless of
    // this test's own record order; the four unmapped raw actions follow,
    // grouped by prefix and alphabetical within each group — not the
    // first-seen order the records above were listed in.
    expect(facets.act).toEqual(["reasoning", "hook.enqueue", "mission.debrief", "mission.start", "phase.begin", "step.dispatch"]);
  });

  it("sortUnmappedActivities groups by prefix before falling back to the full string", () => {
    expect(sortUnmappedActivities(["step.b", "mission.z", "step.a", "mission.a"])).toEqual([
      "mission.a",
      "mission.z",
      "step.a",
      "step.b",
    ]);
  });

  it("sortUnmappedActivities leaves a prefix-less value alone (groups with itself)", () => {
    expect(sortUnmappedActivities(["zeta", "mission.a", "alpha"])).toEqual(["alpha", "mission.a", "zeta"]);
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

describe("MODEL_ACTIVITIES — the 'model only' quick filter", () => {
  /** Measured live on a real `dispatch pr-reviewer`: across twelve minutes the
   *  session emitted 171 `dispatch.turn.heartbeat`, 94 `telemetry.process` and
   *  one `dispatch start` — and ZERO reasoning/tool/turn records, because the
   *  model was still inside its first turn. "model only" therefore rendered an
   *  empty list while the model was visibly generating, which reads as
   *  "nothing is happening" at the moment the most is. */
  it("includes heartbeat, the only model signal during a long first turn", () => {
    expect(MODEL_ACTIVITIES.has("heartbeat")).toBe(true);
  });

  it("still includes the per-turn activities it was written for", () => {
    for (const a of ["reasoning", "tool call", "turn"]) {
      expect(MODEL_ACTIVITIES.has(a)).toBe(true);
    }
  });

  it("excludes activity that is not the model working", () => {
    // Host telemetry and lifecycle bookends are about the RUN, not the model.
    for (const a of ["host telemetry", "dispatch start", "dispatch end", "note"]) {
      expect(MODEL_ACTIVITIES.has(a)).toBe(false);
    }
  });

  /** The regression this guards, stated as the real shape: a session whose
   *  only records are heartbeats must not filter down to nothing. */
  it("a heartbeat-only session is not empty under the model-only filter", () => {
    const activities = ["heartbeat", "heartbeat", "host telemetry", "heartbeat"];
    const kept = activities.filter((a) => MODEL_ACTIVITIES.has(a));
    expect(kept).toHaveLength(3);
  });
});

describe("'model only' over the PRODUCTION filter path", () => {
  // The other MODEL_ACTIVITIES tests assert on the Set object and reimplement
  // the filter inline, so they exercise `Set.prototype.has` and can never fail
  // for a production reason. Two mutations that reproduce the exact bug this
  // fixes both left 967/967 green: gating heartbeat out inside
  // `onlyModelFacet`, and renaming the label `activityOf` returns so the set
  // member is dead. This test runs the real chain — activityOf -> computeFacets
  // -> onlyModelFacet -> matchesFilters — and dies under both.
  const heartbeat: FlowRecord = {
    ts: "2026-08-08T12:00:00.000Z",
    category: "work",
    action: "dispatch.turn.heartbeat",
  } as FlowRecord;
  const hostTelemetry: FlowRecord = {
    ts: "2026-08-08T12:00:01.000Z",
    category: "telemetry",
    source: "process",
  } as FlowRecord;

  it("a first-turn session shows its heartbeats instead of reading as idle", () => {
    // The measured shape of a live dispatch still inside turn 1: heartbeats and
    // host telemetry, and NO reasoning/tool/turn records yet.
    const records = [heartbeat, hostTelemetry];
    const facets = computeFacets(records);
    const filters = { ...defaultFilterState(facets), act: onlyModelFacet(facets) };
    const shown = records.filter((r) => matchesFilters(r, filters));

    expect(shown).toHaveLength(1);
    expect(shown[0]).toBe(heartbeat);
  });

  it("'model only' still excludes host telemetry", () => {
    const facets = computeFacets([heartbeat, hostTelemetry]);
    expect(matchesFilters(hostTelemetry, {
      ...defaultFilterState(facets),
      act: onlyModelFacet(facets),
    })).toBe(false);
  });
});

// ── Active-filter count + session persistence (#2018) ────────────────────
describe("activeFilterCount", () => {
  const facets: Facets = { act: ["a", "b"], cat: ["work"], tier: ["operator"], src: ["cli"] };

  it("counts nothing when every value is selected", () => {
    expect(activeFilterCount(defaultFilterState(facets), facets)).toBe(0);
  });

  it("counts a facet only when it is a STRICT subset", () => {
    const st = defaultFilterState(facets);
    st.act = new Set(["a"]);
    expect(activeFilterCount(st, facets)).toBe(1);
  });

  it("does not count a facet with nothing on offer", () => {
    // Otherwise a fresh page opens claiming filters it never applied.
    const empty: Facets = { act: [], cat: [], tier: [], src: [] };
    expect(activeFilterCount(defaultFilterState(empty), empty)).toBe(0);
  });

  it("counts the free-text query, which hides rows the same way a facet does", () => {
    const st = defaultFilterState(facets);
    st.q = "boom";
    expect(activeFilterCount(st, facets)).toBe(1);
    st.q = "   ";
    expect(activeFilterCount(st, facets)).toBe(0);
  });
});

describe("filter session persistence", () => {
  const facets: Facets = { act: ["a", "b"], cat: ["work"], tier: ["operator"], src: ["cli"] };
  function mem() {
    const m = new Map<string, string>();
    return {
      getItem: (k: string) => m.get(k) ?? null,
      setItem: (k: string, v: string) => void m.set(k, v),
    };
  }

  it("round-trips a selection", () => {
    const s = mem();
    const st = defaultFilterState(facets);
    st.act = new Set(["a"]);
    st.q = "err";
    persistFilterState(st, s);
    const back = restoreFilterState(facets, s);
    expect([...back.act]).toEqual(["a"]);
    expect(back.q).toBe("err");
  });

  it("drops stored values this window no longer offers", () => {
    // A restored value naming an absent facet would render a filter for
    // something that cannot appear, and count toward a badge the operator
    // can neither see the effect of nor clear.
    const s = mem();
    const st = defaultFilterState(facets);
    st.act = new Set(["a"]);
    persistFilterState(st, s);
    const narrower: Facets = { ...facets, act: ["b"] };
    expect([...restoreFilterState(narrower, s).act]).toEqual(["b"]);
  });

  it("falls back to the default when the intersection is empty, never to 'nothing selected'", () => {
    // A filter hiding literally everything is indistinguishable from a
    // broken pane.
    const s = mem();
    const st = defaultFilterState(facets);
    st.act = new Set(["a"]);
    persistFilterState(st, s);
    const disjoint: Facets = { ...facets, act: ["z"] };
    expect([...restoreFilterState(disjoint, s).act]).toEqual(["z"]);
  });

  it("returns the default on unparsable stored data rather than throwing", () => {
    const s = { getItem: () => "{not json", setItem: () => {} };
    expect([...restoreFilterState(facets, s).act].sort()).toEqual(["a", "b"]);
  });

  it("(#2206) returns the default on stored data that PARSES but is not an object", () => {
    // Reaches the isPlainObject guard itself — the "{not json" case above
    // throws inside JSON.parse and never gets there.
    for (const stored of ["42", '"a string"', "null", "true", "[1,2]"]) {
      const s = { getItem: () => stored, setItem: () => {} };
      expect([...restoreFilterState(facets, s).act].sort()).toEqual(["a", "b"]);
    }
  });

  it("survives storage that throws outright, not merely one that returns null", () => {
    const boom = {
      getItem: () => { throw new Error("blocked"); },
      setItem: () => { throw new Error("blocked"); },
    };
    expect([...restoreFilterState(facets, boom).act].sort()).toEqual(["a", "b"]);
    expect(() => persistFilterState(defaultFilterState(facets), boom)).not.toThrow();
  });
});
