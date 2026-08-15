/**
 * The event-log's full facet-filter model — `activityOf()` (viewer.html:
 * 1014-1042), `ACT_ORDER` (viewer.html:1047), and `recompute()`'s facet
 * derivation (viewer.html:1054-1058), plus the port's own `FilterState` /
 * `matchesFilters` that both `EventLogColumn` (the row list + its `#logq`
 * quick search) and `FiltersDialog` (the full checkbox-per-facet modal,
 * viewer.html:857-865) read from — ONE shared implementation rather than
 * `EventLogColumn`'s previous PARTIAL local `activityOf` (see #1640: before
 * this, `.fbtn` only ever drove a fixed "model only" boolean because the
 * full modal — and the facet lists it needs — didn't exist yet).
 */
import type { FlowRecord } from "../types/handwritten";

/** `activityOf()` — viewer.html:1014-1042, the FULL mapping (every branch,
 * including session end / machine online-offline / note, which the port's
 * previous row-label-only subset omitted because nothing yet needed the
 * facet checkboxes those branches feed). */
export function activityOf(r: FlowRecord): string {
  const a = r.action || "";
  if (a === "dispatch.reasoning") return "reasoning";
  if (a === "dispatch.tool") return "tool call";
  if (a === "dispatch.turn") return "turn";
  if (a === "dispatch.turn.heartbeat") return "heartbeat";
  if (a === "dispatch.start" || a === "dispatch start") return "dispatch start";
  if (a === "dispatch.complete" || a === "dispatch complete") return "dispatch end";
  if (a === "dispatch.error" || a === "dispatch error") return "dispatch error";
  if (a === "dispatch.feedback.injected") return "feedback";
  if (a === "tier-decision") return "routing";
  if (a === "dispatch.compaction" || r.source === "compaction") return "compaction";
  if (a === "flow.note" || a === "note" || r.source === "orchestrator" || r.source === "adjudication") return "note";
  if (a === "machine.online" || a === "machine online") return "machine online";
  if (a === "machine.offline" || a === "machine offline") return "machine offline";
  if (a === "session.end") return "session end";
  if (r.category === "telemetry") {
    if (r.source === "detector") return "detector";
    if (r.source === "tokens") return "tokens";
    if (r.source === "process") return "host telemetry";
    if (r.source === "lms") return "lms";
    if (r.source === "runtime") return "runtime";
    return "telemetry";
  }
  return a || "other";
}

/** `ACT_ORDER` — viewer.html:1047. Preferred display order for the activity
 * facet: model-doing activities first, then dispatch lifecycle, then fleet
 * lifecycle, then telemetry. */
export const ACT_ORDER: string[] = [
  "reasoning",
  "tool call",
  "turn",
  "heartbeat",
  "dispatch start",
  "dispatch end",
  "dispatch error",
  "feedback",
  "routing",
  "compaction",
  "note",
  "machine online",
  "machine offline",
  "session end",
  "detector",
  "runtime",
  "tokens",
  "lms",
  "host telemetry",
  "telemetry",
  "other",
];

/** `ACT_ICON`'s "model only" subset — viewer.html:861's `onlymodel` quick
 * filter (`window.onlyModelActivity`). */
export const MODEL_ACTIVITIES = new Set(["reasoning", "tool call", "turn"]);

export interface Facets {
  act: string[];
  cat: string[];
  tier: string[];
  src: string[];
}

/** `recompute()`'s facet derivation — viewer.html:1054-1058.
 *
 * One DELIBERATE divergence from legacy, named here because the file it
 * diverges from is about to be deleted and would otherwise stop being
 * checkable: legacy's `FCATS` has no `.filter(Boolean)` on category, so a
 * record with no `category` produced an EMPTY-LABEL checkbox that could be
 * unchecked to filter such records out. Here nulls are dropped, and
 * `matchesFilters` null-guards to match — a record with no category is never
 * excluded on that facet. Every current producer emits a category, so this
 * changes nothing observable today; it is a blank checkbox nobody could name
 * being dropped rather than faithfully reproduced. */
export function computeFacets(records: FlowRecord[]): Facets {
  const cat = [...new Set(records.map((r) => r.category).filter((v): v is string => v != null))];
  const tier = [...new Set(records.map((r) => r.tier).filter((v): v is string => v != null))];
  const src = [...new Set(records.map((r) => r.source).filter((v): v is string => v != null))];
  const acts = new Set(records.map(activityOf));
  const act = ACT_ORDER.filter((a) => acts.has(a)).concat([...acts].filter((a) => !ACT_ORDER.includes(a)));
  return { act, cat, tier, src };
}

export interface FilterState {
  act: Set<string>;
  cat: Set<string>;
  tier: Set<string>;
  src: Set<string>;
  q: string;
}

/** `state.filters={cat:new Set(FCATS),tier:new Set(FTIERS),src:new
 * Set(FSRC),act:new Set(FACTS),q:''}` — viewer.html:1068/2877
 * (`clearFilters`'s reset shape, also boot's initial shape). Every facet
 * starts fully included — nothing is filtered out until the operator
 * unchecks something. */
export function defaultFilterState(facets: Facets): FilterState {
  return {
    act: new Set(facets.act),
    cat: new Set(facets.cat),
    tier: new Set(facets.tier),
    src: new Set(facets.src),
    q: "",
  };
}

const FACET_KEYS = ["act", "cat", "tier", "src"] as const;

/** The "have we ever offered this value before" ledger `absorbNewFacetValues`
 * needs — viewer.html's `SEEN` (`SEEN.cat`/`SEEN.tier`/`SEEN.src`/`SEEN.act`,
 * declared alongside `state`). One instance lives for the component's whole
 * mounted lifetime (a `useRef`, not `useState` — this is bookkeeping, not
 * something a render should react to). */
export interface FacetSeen {
  act: Set<string>;
  cat: Set<string>;
  tier: Set<string>;
  src: Set<string>;
}

export function createFacetSeen(): FacetSeen {
  return { act: new Set(), cat: new Set(), tier: new Set(), src: new Set() };
}

/** `absorbNewFilterValues()` — viewer.html:3451-3457, called after every
 * `recompute()` (boot, `applyLive()`'s per-poll live-tail merge, and the
 * date-rollover reload). Mutates `seen` and returns a `filters` object with
 * every BRAND-NEW facet value (one `seen` has never recorded) added to the
 * corresponding active Set — auto-including a value the FIRST time it's
 * ever encountered (a live-streamed record introducing a new activity type,
 * say), while an operator's explicit UNCHECK of an ALREADY-seen value
 * sticks, because `seen` only tracks "processed once ever", never
 * "currently checked".
 *
 * This is the piece that makes the naive "seed `filters` once at mount"
 * approach wrong on its own: `records` (and therefore `facets`) starts
 * empty and populates asynchronously (a TanStack Query fetch, or a live SSE
 * record arriving later) — a single seed-at-mount either locks onto an
 * empty snapshot (nothing ever matches) or, once corrected to seed at first
 * real population, still silently drops every activity/category/tier/
 * source value that shows up for the FIRST time after that (exactly what a
 * live-streamed record's own new `action`/`category` can do). Calling this
 * on every `facets` change is the fix, and it composes with a plain
 * `useState(() => defaultFilterState(EMPTY_FACETS))` initial value cleanly:
 * before any real data exists, `facets` is empty, so this is a no-op; the
 * moment real data exists, every value in it is "new" (nothing has been
 * `seen` yet) and gets absorbed in one pass — no separate one-time-seed
 * codepath needed.
 *
 * Returns the SAME `filters` reference when nothing changed, so a caller
 * (an effect) can call this on every `facets` change without triggering a
 * spurious re-render when nothing was actually new. */
export function absorbNewFacetValues(filters: FilterState, facets: Facets, seen: FacetSeen): FilterState {
  let changed = false;
  const next: FilterState = { ...filters };
  for (const key of FACET_KEYS) {
    for (const v of facets[key]) {
      if (seen[key].has(v)) continue;
      seen[key].add(v);
      if (next[key] === filters[key]) next[key] = new Set(filters[key]);
      next[key].add(v);
      changed = true;
    }
  }
  return changed ? next : filters;
}

/** `render()`'s per-record filter predicate, folding together the four
 * facet checks (viewer.html's `state.filters.act.has(activityOf(r))` etc.,
 * inferred from `renderFilters()`/`toggleFilter()`'s data model — the
 * facet-vs-record predicate itself isn't a single named legacy function,
 * it's inlined into `render()`'s filter chain) plus the free-text search
 * (`JSON.stringify(r).toLowerCase().includes(q)`, viewer.html's `render()`).
 * A record whose cat/tier/src is simply ABSENT (no checkbox represents it)
 * is never excluded on that facet — only a PRESENT value that got
 * unchecked filters the record out. */
export function matchesFilters(r: FlowRecord, filters: FilterState): boolean {
  if (!filters.act.has(activityOf(r))) return false;
  if (r.category != null && !filters.cat.has(r.category)) return false;
  if (r.tier != null && !filters.tier.has(r.tier)) return false;
  if (r.source != null && !filters.src.has(r.source)) return false;
  if (filters.q) {
    const q = filters.q.trim().toLowerCase();
    if (q && !JSON.stringify(r).toLowerCase().includes(q)) return false;
  }
  return true;
}
