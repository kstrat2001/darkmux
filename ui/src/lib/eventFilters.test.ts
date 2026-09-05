import { describe, it, expect } from "vitest";
import {
  absorbNewFacetValues,
  activityOf,
  computeFacets,
  createFacetSeen,
  createStoredPicks,
  DEFAULT_ACTIVITIES,
  defaultFilterState,
  matchesFilters,
  MODEL_ACTIVITIES,
  activeFilterCount,
  restoreFilterState,
  persistFilterState,
  storedFilterPicks,
  applyStoredPicks,
  sortUnmappedActivities,
} from "./eventFilters";
import type { Facets, StoredPicks } from "./eventFilters";
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
  it("derives facets from records, and defaultFilterState includes every one of them (a non-act facet)", () => {
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
  it("auto-includes a brand-new NON-act facet value the first time it's ever seen", () => {
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
  //
  // (#2416) The new activity is "dispatch.tool" ("tool call"), not the
  // original "flow.note" ("note") — "note" is outside `DEFAULT_ACTIVITIES`
  // and no longer absorbs on by default (see the dedicated #2416 describe
  // block below for that case). "tool call" IS in the allowlist, so this
  // keeps testing the SAME live-tail absorb-on-first-sight mechanism without
  // needing to pass stored overrides.
  it("a record with a brand-new (allowlisted) activity value is absorbed and then matches", () => {
    const seen = createFacetSeen();
    let filters = defaultFilterState(computeFacets([rec({ action: "dispatch.reasoning" })]));
    seen.act.add("reasoning"); // mark the initial batch as already-seen, matching real usage
    filters = absorbNewFacetValues(filters, computeFacets([rec({ action: "dispatch.reasoning" })]), seen);

    // A new record arrives with an activity that was never in the initial batch.
    const newRecord = rec({ action: "dispatch.tool" });
    const facetsWithNew = computeFacets([rec({ action: "dispatch.reasoning" }), newRecord]);
    filters = absorbNewFacetValues(filters, facetsWithNew, seen);

    expect(filters.act.has("tool call")).toBe(true);
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

describe("MODEL_ACTIVITIES — the model-doing-something vocabulary", () => {
  // (#2416) `heartbeat` LEFT this set. Liveness is now carried by the
  // header's live-status badge and the run lens's own clock/pulse, and the
  // operator was unchecking `heartbeat` by hand on every single run — a busy
  // dispatch turns 171 heartbeats over twelve minutes into pure noise
  // between the turns that actually matter. `MODEL_ACTIVITIES` no longer
  // drives "model only" either — see `DEFAULT_ACTIVITIES` and
  // `onlyModelFacet` below.
  it("no longer includes heartbeat", () => {
    expect(MODEL_ACTIVITIES.has("heartbeat")).toBe(false);
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

  // (#2416) This inverts the pre-fix regression guard: a heartbeat-only
  // session is now CORRECTLY empty under "model only" — heartbeat is no
  // longer treated as model activity because the event log doesn't have to
  // prove liveness with rows any more (the header badge does that).
  it("a heartbeat-only session IS empty under the model-doing-something vocabulary", () => {
    const activities = ["heartbeat", "heartbeat", "host telemetry", "heartbeat"];
    const kept = activities.filter((a) => MODEL_ACTIVITIES.has(a));
    expect(kept).toHaveLength(0);
  });
});

describe("'model only' over the PRODUCTION filter path", () => {
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

  // (#2416) This is the deliberate behavior change the fix accepts: a
  // first-turn session showing only heartbeats now reads as EMPTY under
  // "model only", not as "the model is working". That's fine — the header's
  // live-status badge and the run lens's own clock/pulse carry liveness now,
  // so the event log isn't the only place proving something is happening.
  it("a first-turn session with only heartbeats now shows nothing under 'model only'", () => {
    const records = [heartbeat, hostTelemetry];
    const facets = computeFacets(records);
    const filters = { ...defaultFilterState(facets), act: onlyModelFacet(facets) };
    const shown = records.filter((r) => matchesFilters(r, filters));

    expect(shown).toHaveLength(0);
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
  // `act` uses two values that ARE in `DEFAULT_ACTIVITIES` so "every value
  // selected" is actually achievable via the plain default (#2416 gave `act`
  // a curated allowlist instead of everything-present).
  const facets: Facets = { act: ["reasoning", "turn"], cat: ["work"], tier: ["operator"], src: ["cli"] };

  it("counts nothing when every value is selected", () => {
    expect(activeFilterCount(defaultFilterState(facets), facets)).toBe(0);
  });

  it("counts a facet only when it is a STRICT subset", () => {
    const st = defaultFilterState(facets);
    st.act = new Set(["reasoning"]);
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

describe("filter session persistence — generic mechanics (on `cat`, unaffected by the #2416 act allowlist)", () => {
  // (#2416) `persistFilterState` gained a required `facets` parameter (it
  // now stores INCLUDE/EXCLUDE overrides against each value's default,
  // rather than a flat "currently selected" snapshot — see its own doc) and
  // exercises `act`'s new curated-default behavior extensively in the
  // dedicated describe block below. These tests keep testing the FACET-
  // AGNOSTIC storage mechanics (round-trip, reconciliation against a
  // narrower window, malformed/throwing storage) on `cat`, which still
  // defaults fully-on exactly like every facet used to.
  const facets: Facets = { act: ["reasoning"], cat: ["a", "b"], tier: ["operator"], src: ["cli"] };
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
    st.cat = new Set(["a"]); // excludes "b", which defaults on — an explicit override
    st.q = "err";
    persistFilterState(st, facets, s);
    const back = restoreFilterState(facets, s);
    expect([...back.cat]).toEqual(["a"]);
    expect(back.q).toBe("err");
  });

  it("a stored value this window no longer offers does not leak into the restored state", () => {
    const s = mem();
    const st = defaultFilterState(facets);
    st.cat = new Set(["a"]); // "b" excluded
    persistFilterState(st, facets, s);
    const narrower: Facets = { ...facets, cat: ["a"] }; // "b" no longer offered at all
    expect([...restoreFilterState(narrower, s).cat]).toEqual(["a"]);
  });

  // (#2416) Superseded assumption, rewritten rather than deleted: the old
  // implementation stored a flat "currently selected" array and fell back to
  // "everything selected" whenever the stored/offered intersection was
  // empty, specifically SO a facet could never render as "hiding everything"
  // — see the pre-#2416 comment this test used to carry. That guarantee no
  // longer holds by design: #2416 needs an explicit exclude to survive even
  // when the excluded value becomes the ONLY thing on offer (a busy fleet's
  // `heartbeat` reappearing after being unchecked is exactly this shape),
  // so a facet CAN now legitimately show nothing. The equivalent coverage —
  // an exclude surviving a disappear/reappear cycle — lives in the dedicated
  // #2416 describe block below.
  it("an excluded value stays excluded even if it becomes the only value on offer", () => {
    const s = mem();
    const st = defaultFilterState(facets);
    st.cat = new Set(["a"]); // "b" excluded
    persistFilterState(st, facets, s);
    const onlyExcluded: Facets = { ...facets, cat: ["b"] };
    expect([...restoreFilterState(onlyExcluded, s).cat]).toEqual([]);
  });

  it("returns the default on unparsable stored data rather than throwing", () => {
    const s = { getItem: () => "{not json", setItem: () => {} };
    expect([...restoreFilterState(facets, s).cat].sort()).toEqual(["a", "b"]);
  });

  it("(#2206) returns the default on stored data that PARSES but is not an object", () => {
    // Reaches the isPlainObject guard itself — the "{not json" case above
    // throws inside JSON.parse and never gets there.
    for (const stored of ["42", '"a string"', "null", "true", "[1,2]"]) {
      const s = { getItem: () => stored, setItem: () => {} };
      expect([...restoreFilterState(facets, s).cat].sort()).toEqual(["a", "b"]);
    }
  });

  it("survives storage that throws outright, not merely one that returns null", () => {
    const boom = {
      getItem: () => { throw new Error("blocked"); },
      setItem: () => { throw new Error("blocked"); },
    };
    expect([...restoreFilterState(facets, boom).cat].sort()).toEqual(["a", "b"]);
    expect(() => persistFilterState(defaultFilterState(facets), facets, boom)).not.toThrow();
  });
});

// ── #2416: event filters default to model activity without heartbeat ────
describe("#2416 — act defaults to DEFAULT_ACTIVITIES, new values absorb off, picks stick", () => {
  function mem() {
    const m = new Map<string, string>();
    return {
      getItem: (k: string) => m.get(k) ?? null,
      setItem: (k: string, v: string) => void m.set(k, v),
    };
  }

  // (a)
  it("defaultFilterState turns on only the DEFAULT_ACTIVITIES allowlist for act", () => {
    const facets = computeFacets([rec({ action: "dispatch.tool" }), rec({ action: "dispatch.turn.heartbeat" })]);
    const filters = defaultFilterState(facets);
    expect(filters.act.has("tool call")).toBe(true);
    expect(filters.act.has("heartbeat")).toBe(false);
  });

  // (b)
  it("absorbs a brand-new heartbeat OFF, a brand-new tool call ON, and a stored-include value ON", () => {
    const seen = createFacetSeen();
    const facets = computeFacets([
      rec({ action: "dispatch.turn.heartbeat" }),
      rec({ action: "dispatch.tool" }),
      rec({ action: "flow.note" }), // "note" — outside DEFAULT_ACTIVITIES
    ]);
    const overrides = createStoredPicks();
    overrides.act.include.add("note"); // the operator's own stored pick
    const filters = absorbNewFacetValues(defaultFilterState({ act: [], cat: [], tier: [], src: [] }), facets, seen, overrides);
    expect(filters.act.has("heartbeat")).toBe(false);
    expect(filters.act.has("tool call")).toBe(true);
    expect(filters.act.has("note")).toBe(true);
  });

  // (c)
  it("an exclude survives the value disappearing from facets and reappearing later", () => {
    const s = mem();
    // "reasoning" IS in DEFAULT_ACTIVITIES (default on) — excluding it is the
    // shape that actually needs the override ledger, unlike heartbeat (which
    // is already off by default and needs no override to stay off).
    const initial = computeFacets([rec({ action: "dispatch.reasoning" }), rec({ action: "dispatch.turn" })]);
    const st = defaultFilterState(initial);
    st.act.delete("reasoning"); // operator explicitly excludes it
    persistFilterState(st, initial, s);

    // The fleet goes idle: "reasoning" isn't offered at all for a while.
    const idle = computeFacets([rec({ action: "dispatch.turn" })]);
    expect(restoreFilterState(idle, s).act.has("turn")).toBe(true);

    // "reasoning" reappears — it must come back OFF, honoring the earlier
    // exclude, not silently re-enable itself the way the reported bug did.
    const reappeared = computeFacets([rec({ action: "dispatch.reasoning" }), rec({ action: "dispatch.turn" })]);
    const restored = restoreFilterState(reappeared, s);
    expect(restored.act.has("reasoning")).toBe(false);
    expect(restored.act.has("turn")).toBe(true);
  });

  // (d)
  it("a scope with no override of its own reads the global picks", () => {
    const s = mem();
    const facets = computeFacets([rec({ action: "dispatch.reasoning" }), rec({ action: "flow.note" })]);
    const globalState = defaultFilterState(facets);
    globalState.act.add("note"); // operator explicitly includes "note" globally
    persistFilterState(globalState, facets, s); // no scope => global key

    // A scoped pane that has never had its own picks inherits the global ones.
    expect(restoreFilterState(facets, s, "mission").act.has("note")).toBe(true);
  });

  it("a change inside a scope writes the scope key and leaves the global key untouched", () => {
    const s = mem();
    const facets = computeFacets([rec({ action: "dispatch.reasoning" }), rec({ action: "flow.note" })]);
    const scopedState = defaultFilterState(facets);
    scopedState.act.add("note");
    persistFilterState(scopedState, facets, s, "mission");

    // The global (unscoped) key was never written — restoring unscoped
    // still gets the plain default, not the scoped operator's pick.
    expect(restoreFilterState(facets, s).act.has("note")).toBe(false);
    // The scope itself sees its own pick.
    expect(restoreFilterState(facets, s, "mission").act.has("note")).toBe(true);
  });

  // (e)
  it("an old-format (pre-#2416) stored payload restores as the plain default, without throwing", () => {
    const s = mem();
    // The pre-#2416 flat-array shape, no `version` field.
    s.setItem("dmux.eventfilters", JSON.stringify({ act: ["reasoning"], cat: ["work"], tier: [], src: [], q: "" }));
    const facets = computeFacets([rec({ action: "dispatch.reasoning" }), rec({ action: "dispatch.turn.heartbeat" })]);
    expect(() => restoreFilterState(facets, s)).not.toThrow();
    const restored = restoreFilterState(facets, s);
    expect(restored.act.has("reasoning")).toBe(true); // plain default: reasoning is in DEFAULT_ACTIVITIES
    expect(restored.act.has("heartbeat")).toBe(false); // plain default: heartbeat is not
  });

  // (f)
  it("'model only' (onlyModelFacet) yields exactly DEFAULT_ACTIVITIES intersected with what's offered", () => {
    const facets = computeFacets([
      rec({ action: "dispatch.reasoning" }),
      rec({ action: "dispatch.checkpoint" }),
      rec({ action: "dispatch.tool" }),
      rec({ action: "dispatch.turn" }),
      rec({ action: "dispatch.error" }),
      rec({ action: "dispatch.turn.heartbeat" }),
      rec({ action: "flow.note" }),
    ]);
    const only = onlyModelFacet(facets);
    expect(only).toEqual(new Set(DEFAULT_ACTIVITIES));
  });

  // (g)
  it("activeFilterCount is nonzero by default on a busy facet set (hidden rows never read as a quiet system)", () => {
    const facets = computeFacets([
      rec({ action: "dispatch.reasoning" }),
      rec({ action: "dispatch.turn.heartbeat" }),
      rec({ action: "flow.note" }),
      rec({ category: "telemetry", source: "process" }),
    ]);
    const filters = defaultFilterState(facets);
    expect(activeFilterCount(filters, facets)).toBeGreaterThan(0);
  });

  it("applyStoredPicks: explicit exclude wins over an explicit include for the same value", () => {
    const picks: StoredPicks = createStoredPicks();
    picks.act.exclude.add("reasoning");
    picks.act.include.add("note");
    const facets = computeFacets([rec({ action: "dispatch.reasoning" }), rec({ action: "flow.note" }), rec({ action: "dispatch.turn" })]);
    const state = applyStoredPicks(picks, facets);
    expect(state.act.has("reasoning")).toBe(false); // explicit exclude wins
    expect(state.act.has("note")).toBe(true); // explicit include
    expect(state.act.has("turn")).toBe(true); // no opinion — falls to DEFAULT_ACTIVITIES
  });

  it("storedFilterPicks returns null when nothing recognizable is stored", () => {
    const s = mem();
    expect(storedFilterPicks(s)).toBeNull();
    s.setItem("dmux.eventfilters", "not json");
    expect(storedFilterPicks(s)).toBeNull();
  });
});
