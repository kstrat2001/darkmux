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
import { isPlainObject } from "./guards";

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
  // (#1221) The harness deciding mid-turn whether the model keeps thinking.
  if (a === "dispatch.checkpoint") return "checkpoint";
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
  "checkpoint",
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
 * filter (`window.onlyModelActivity`).
 *
 * `heartbeat` is in here, and it is the reason this filter works at all during
 * the window an operator most wants it. Measured live on a real dispatch: over
 * twelve minutes the session emitted 171 `dispatch.turn.heartbeat` records, 94
 * `telemetry.process`, and exactly ONE `dispatch start` — and zero
 * `dispatch.reasoning`, `dispatch.tool` or `dispatch.turn`, because the model
 * was still inside its first turn. With the original three-element set, "model
 * only" showed an EMPTY list while the model was visibly generating in
 * LMStudio, which reads as "nothing is happening" at the exact moment the most
 * is.
 *
 * A heartbeat IS model activity: it is the runtime's proof-of-work signal for a
 * turn in flight. The original set was written against a multi-turn agentic
 * shape where turns land every ~30s, so per-turn records were always arriving;
 * it does not survive a single long reasoning turn, which is the shape a review
 * or any thinking-family dispatch actually takes.
 *
 * A `checkpoint` is model activity for the same reason, and on the same
 * shape: it is emitted every time a long reasoning turn hits its check-in
 * interval, which on a thinking-family dispatch is the ONLY per-turn record
 * arriving for minutes at a time. (#1221)
 */
export const MODEL_ACTIVITIES = new Set([
  "reasoning",
  "checkpoint",
  "tool call",
  "turn",
  "heartbeat",
]);

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
/** The namespace prefix of a raw activity string — `"mission.start"` →
 * `"mission"`, `"telemetry.process"` → `"telemetry"`, a prefix-less value
 * → itself. Used only to GROUP `sortUnmappedActivities`' output; never
 * shown — the checkbox label stays the full string. */
function activityPrefixOf(a: string): string {
  const i = a.indexOf(".");
  return i === -1 ? a : a.slice(0, i);
}

/** (#2116) `ACT_ORDER` names ~22 activities `activityOf()` recognizes by
 * branch; a busy day's flow can carry `r.action` strings it doesn't (a new
 * `mission.*`/`phase.*`/`step.*`/`dispatch.*`/`hook.*` record kind —
 * `activityOf`'s own fallback, `return a || "other"`, passes the RAW
 * action straight through). Those used to land in `[...acts]`'s bare
 * `Set`-iteration order — first-seen, effectively arbitrary — which is
 * exactly what turned the activity facet into an unscannable ~40-item
 * scramble once #2116's own filters-dialog layout gave it room to be
 * scanned at all. Grouping by namespace prefix (alphabetical within a
 * group, for a deterministic order regardless of which record happened to
 * stream in first) puts every `mission.*` together, every `hook.*`
 * together, and so on — the same grouping `ACT_ORDER` already gives the
 * KNOWN activities, extended to the ones it doesn't yet name. */
export function sortUnmappedActivities(values: string[]): string[] {
  return [...values].sort((a, b) => {
    const pa = activityPrefixOf(a);
    const pb = activityPrefixOf(b);
    if (pa !== pb) return pa.localeCompare(pb);
    return a.localeCompare(b);
  });
}

export function computeFacets(records: FlowRecord[]): Facets {
  const cat = [...new Set(records.map((r) => r.category).filter((v): v is string => v != null))];
  const tier = [...new Set(records.map((r) => r.tier).filter((v): v is string => v != null))];
  const src = [...new Set(records.map((r) => r.source).filter((v): v is string => v != null))];
  const acts = new Set(records.map(activityOf));
  const act = ACT_ORDER.filter((a) => acts.has(a)).concat(
    sortUnmappedActivities([...acts].filter((a) => !ACT_ORDER.includes(a))),
  );
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

/** (#2018) How many of the operator's filters are currently HIDING something.
 *
 * The button reads `filters` whether zero or six are set, so a pane showing
 * three rows out of four hundred looks exactly like a quiet system. That
 * invisibility — not durability — is the real hazard #1911 names when it
 * refuses to persist a selection: "a filter silently restored from last week
 * is a wrong reading that looks like a quiet system." A filter you can SEE is
 * not silent, which is why this lands before any persistence does.
 *
 * A facet counts as active only when it is a STRICT SUBSET of what is on
 * offer. Equal-size means every value is selected, which hides nothing — and
 * a facet whose values have not appeared yet must not read as a filter, or
 * every fresh page would open claiming filters it never applied.
 *
 * The free-text query counts as one, because it hides rows exactly the same
 * way a facet does.
 */
export function activeFilterCount(state: FilterState, facets: Facets): number {
  let n = 0;
  for (const k of FACET_KEYS) {
    const offered = facets[k];
    const selected = state[k];
    if (offered.length > 0 && selected.size < offered.length) n += 1;
  }
  if (state.q.trim() !== "") n += 1;
  return n;
}


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

/** (#2018) `sessionStorage` key for the operator's event-log filter picks.
 *
 * The TIER is the whole decision, and it is `session`, not `local`. #1911
 * refuses `localStorage` for panel selections because "a filter silently
 * restored from last week is a wrong reading that looks like a quiet system"
 * — a correctness argument, and one this must not trade away. Session scope
 * keeps that guarantee (closing the tab clears it) while fixing the actual
 * complaint, which was that every REFRESH reset the picks.
 *
 * Facet sets are stored as arrays and RECONCILED on read against the facets
 * the current window actually offers: a restored value naming a facet no
 * longer present would render a filter for something that cannot appear, and
 * would make `activeFilterCount` report a filter the operator cannot see or
 * clear. Unknown values are dropped rather than trusted.
 */
const FILTERS_STORAGE_KEY = "dmux.eventfilters";

/** (#2027) Per-INSTANCE key. Two `EventLogColumn`s are mounted at once on the
 * `mission` route — the App-level one (always mounted, CSS-hidden) and the
 * one `MissionGraphLens` owns, fed mission-scoped records. They shared this
 * key, so a routine live-poll tick on the HIDDEN pane overwrote the visible
 * pane's picks, including its search text, and a refresh restored the wrong
 * pane's filters. Found by a QA agent that mounted both as siblings.
 *
 * Scoping by the caller's own label keeps each pane's picks to itself. A
 * caller that passes nothing gets the original key, so the App-level pane's
 * existing stored picks survive this change rather than being orphaned. */
function filtersKeyFor(scope?: string): string {
  return scope ? `${FILTERS_STORAGE_KEY}.${scope}` : FILTERS_STORAGE_KEY;
}

type StoredFilters = { act: string[]; cat: string[]; tier: string[]; src: string[]; q: string };

/** Read persisted picks, reconciled against what this window offers.
 *
 * Returns the plain default when nothing is stored, when storage is
 * unavailable, or when the payload is unrecognizable. Storage access can
 * THROW outright (private mode, blocked site data), not merely return null,
 * so every access is wrapped — the pattern `lenses/mission/timeline.ts`
 * already uses. */
export function restoreFilterState(
  facets: Facets,
  storage: Pick<Storage, "getItem"> | null = typeof window === "undefined" ? null : window.sessionStorage,
  scope?: string,
): FilterState {
  const base = defaultFilterState(facets);
  if (!storage) return base;
  let raw: string | null = null;
  try {
    raw = storage.getItem(filtersKeyFor(scope));
  } catch {
    return base;
  }
  if (!raw) return base;
  let parsed: Partial<StoredFilters>;
  try {
    parsed = JSON.parse(raw) as Partial<StoredFilters>;
  } catch {
    return base;
  }
  // (#2027) `JSON.parse("null")` SUCCEEDS and yields `null`; so do `"3"` and
  // `"\"x\""`. The try/catch only covered UNPARSABLE input, so a payload that
  // parsed to a non-object reached `parsed[k]` and threw a TypeError — inside
  // a `useState` lazy initializer, which is a render-phase crash, and with no
  // error boundary in the app at the time, a blank page. The documented
  // contract already promised "the plain default when the payload is
  // unrecognizable"; this makes the code match it.
  if (!isPlainObject(parsed)) return base;
  const out = base;
  for (const k of FACET_KEYS) {
    const stored = parsed[k];
    if (!Array.isArray(stored)) continue;
    const offered = new Set(facets[k]);
    // Intersect, never union: a stored value the current window does not
    // offer is dropped. An EMPTY intersection is left as the default (every
    // value selected) rather than as "nothing selected", because a filter
    // hiding literally everything is indistinguishable from a broken pane.
    const keep = stored.filter((v) => offered.has(v));
    if (keep.length > 0) out[k] = new Set(keep);
  }
  if (typeof parsed.q === "string") out.q = parsed.q;
  return out;
}

/** Persist the current picks. Silent on failure by design — a filter that
 * could not be saved is a lost convenience, never a reason to break the
 * pane. */
export function persistFilterState(
  state: FilterState,
  storage: Pick<Storage, "setItem"> | null = typeof window === "undefined" ? null : window.sessionStorage,
  scope?: string,
): void {
  if (!storage) return;
  try {
    const payload: StoredFilters = {
      act: [...state.act],
      cat: [...state.cat],
      tier: [...state.tier],
      src: [...state.src],
      q: state.q,
    };
    storage.setItem(filtersKeyFor(scope), JSON.stringify(payload));
  } catch {
    // ignore — storage unavailable or full
  }
}

/** (#2027) The stored picks, unreconciled — for a caller that does not yet
 * know what its facets are.
 *
 * `restoreFilterState` intersects against the facets on offer, which is right
 * once they exist and USELESS at mount: `EventLogColumn` mounts before its
 * records query resolves, so `computeFacets([])` is empty, every intersection
 * is empty, and restore returned the plain default. `absorbNewFacetValues`
 * then treated every arriving value as brand new and selected it. The
 * operator's pick was discarded on every single load — the feature looked
 * implemented and did nothing.
 *
 * The tests missed it because they seeded facets and records together, which
 * the real mount order never does. Found by a QA agent that reproduced the
 * actual App mount sequence.
 *
 * Returns null when there is nothing stored, so a caller can distinguish
 * "operator has no saved picks" from "operator saved everything selected".
 */
export function storedFilterPicks(
  storage: Pick<Storage, "getItem"> | null = typeof window === "undefined" ? null : window.sessionStorage,
  scope?: string,
): StoredFilters | null {
  if (!storage) return null;
  try {
    const raw = storage.getItem(filtersKeyFor(scope));
    if (!raw) return null;
    const parsed = JSON.parse(raw) as unknown;
    if (!isPlainObject(parsed)) return null;
    const o = parsed as Partial<StoredFilters>;
    return {
      act: Array.isArray(o.act) ? o.act.filter((v) => typeof v === "string") : [],
      cat: Array.isArray(o.cat) ? o.cat.filter((v) => typeof v === "string") : [],
      tier: Array.isArray(o.tier) ? o.tier.filter((v) => typeof v === "string") : [],
      src: Array.isArray(o.src) ? o.src.filter((v) => typeof v === "string") : [],
      q: typeof o.q === "string" ? o.q : "",
    };
  } catch {
    return null;
  }
}

/** Apply stored picks to freshly-arrived facets: keep what the operator chose,
 * and treat a facet they never had an opinion about as fully selected.
 *
 * This is the piece `absorbNewFacetValues` cannot do — that function's job is
 * "a value never offered before becomes selected", which is correct for live
 * traffic and exactly wrong for a value the operator explicitly DESELECTED in
 * a previous session. */
export function applyStoredPicks(picks: StoredFilters, facets: Facets): FilterState {
  const out = defaultFilterState(facets);
  for (const k of FACET_KEYS) {
    const offered = facets[k];
    if (offered.length === 0) continue;
    const stored = picks[k];
    // Nothing stored for this facet: leave it fully selected rather than
    // reading "absent" as "deselect everything".
    if (!stored || stored.length === 0) continue;
    const keep = stored.filter((v) => offered.includes(v));
    // An empty intersection means the stored picks describe a different world
    // entirely; a filter hiding every row is indistinguishable from a broken
    // pane, so fall back to fully selected.
    if (keep.length > 0) out[k] = new Set(keep);
  }
  out.q = picks.q;
  return out;
}

