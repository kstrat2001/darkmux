/**
 * The event-log's full facet-filter model — `activityOf()`, `ACT_ORDER`, and
 * `recompute()`'s facet derivation ported line-for-line from the legacy
 * standalone `viewer.html` (deleted in #1865 once the React port became the
 * only viewer — the line numbers this doc used to cite no longer resolve
 * to anything; the mapping itself is what survived, not the file), plus the
 * port's own `FilterState`/`matchesFilters` that both `EventLogColumn` (the
 * row list + its `#logq` quick search) and `FiltersDialog` (the full
 * checkbox-per-facet modal, also ported from `viewer.html`) read from — ONE
 * shared implementation rather than `EventLogColumn`'s previous PARTIAL
 * local `activityOf` (see #1640: before this, `.fbtn` only ever drove a
 * fixed "model only" boolean because the full modal — and the facet lists
 * it needs — didn't exist yet).
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
  const isNoteEvent = a === "flow.note" || a === "note" || r.source === "orchestrator" || r.source === "adjudication";
  if (isNoteEvent) return "note";
  if (a === "machine.online" || a === "machine online") return "machine online";
  if (a === "machine.offline" || a === "machine offline") return "machine offline";
  if (a === "session.end") return "session end";
  // (#2413) `machine.telemetry` is the machine-scoped replacement for the
  // retired per-dispatch `telemetry.process` — same friendly facet label
  // as the `category: "telemetry", source: "process"` branch below, so a
  // saved "host telemetry" filter keeps working across the schema change
  // (and across the retired mechanism's continuing partial use — see
  // `FLOW_SCHEMA_VERSION` 1.42.0's changelog) instead of fragmenting into
  // a second, differently-named facet.
  if (a === "machine.telemetry") return "host telemetry";
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

/** (#2416) The activity values that default ON — the only facet with a
 * curated allowlist rather than "everything present". `cat`/`tier`/`src`
 * are not the problem the operator reported and keep defaulting to
 * everything-on (see `defaultFilterState` below); `act` on a live fleet
 * carries a long tail of telemetry, lifecycle bookends and detector noise
 * that the operator unchecked on every single load. This set is what's left
 * once that's opted back OUT by default: the per-turn model signal
 * (reasoning / checkpoint / tool call / turn) plus `dispatch error`, because
 * an error hidden by default is the one thing nobody wants to miss.
 *
 * `heartbeat` is deliberately absent — see `MODEL_ACTIVITIES`'s doc for why
 * it no longer needs to carry liveness here. */
export const DEFAULT_ACTIVITIES = new Set(["reasoning", "checkpoint", "tool call", "turn", "dispatch error"]);

/** (silent-miss audit, 2026-09-06) Suffixes that mark an activity value as
 * failure- or abandonment-shaped, checked in ADDITION to `DEFAULT_ACTIVITIES`
 * membership by `isDefaultOn` below. `activityOf`'s literal `return a ||
 * "other"` fallback means any scheduler action with no dedicated branch —
 * `"step error"`, `"phase abandon"` (`darkmux-crew`'s `scheduler.rs`/
 * `lifecycle.rs`) — surfaces here as a value `DEFAULT_ACTIVITIES` was never
 * curated to anticipate, and defaulted OFF: the exact "operator never sees
 * it" failure `DEFAULT_ACTIVITIES`'s own `dispatch error` entry exists to
 * prevent, just reached through a kind this allowlist didn't name instead
 * of a value it forgot. A suffix check closes that off structurally — ANY
 * future action reading as an error or an abandonment (whatever new kind
 * introduces it) defaults on without needing its own allowlist entry. A
 * non-failure activity like `"step start"` matches no suffix and stays off,
 * same as today. */
const DEFAULT_ON_ACTIVITY_SUFFIXES = ["error", "abandon", "abandoned", "failed"];

function looksLikeFailureActivity(value: string): boolean {
  return DEFAULT_ON_ACTIVITY_SUFFIXES.some((suffix) => value.endsWith(suffix));
}

/** Whether facet value `v` under key `k` is ON absent any operator
 * override. `cat`/`tier`/`src` default fully on; `act` defaults to
 * `DEFAULT_ACTIVITIES` PLUS anything failure/abandonment-shaped (see
 * `looksLikeFailureActivity`). This one function is the single place that
 * distinction lives — `defaultFilterState`, `absorbNewFacetValues` and
 * `applyStoredPicks` all defer to it rather than re-deriving it. */
function isDefaultOn(key: keyof Facets, value: string): boolean {
  if (key !== "act") return true;
  return DEFAULT_ACTIVITIES.has(value) || looksLikeFailureActivity(value);
}

/** (#2416) The "model only" quick filter's underlying vocabulary — what
 * counts as the model itself doing something, as opposed to the harness
 * bookkeeping around it.
 *
 * `heartbeat` LEFT this set in #2416. It was added here (and the long
 * comment that used to live on this export told the story) because a real
 * twelve-minute session emitted 171 `dispatch.turn.heartbeat` records and
 * ZERO reasoning/tool/turn records during a single long first turn — without
 * heartbeat, "model only" showed an empty list while the model was visibly
 * generating. That reasoning no longer holds: liveness is now carried by the
 * header's live-status badge and the run lens's own clock/pulse
 * (`LiveStatusBadge`, `LivenessPulse`), so the event log doesn't have to
 * prove "something is happening" with rows of its own. What it was left
 * doing instead was the opposite of useful — the operator unchecked
 * `heartbeat` by hand on every single run, because a busy dispatch turns it
 * into pure noise between the turns that actually matter. Note this set is
 * no longer what the "model only" button applies — see `DEFAULT_ACTIVITIES`
 * and `onlyModelFacet` in `FiltersDialog.tsx`, which now also treats a
 * `dispatch error` as worth surfacing by default. */
export const MODEL_ACTIVITIES = new Set(["reasoning", "checkpoint", "tool call", "turn"]);

export interface Facets {
  act: string[];
  cat: string[];
  tier: string[];
  src: string[];
}

/** (operator, 2026-09-06) The five groupings the activity facet's header
 * toggles are organized under, replacing the modal's old "model only" /
 * "clear all" quick actions (see `FiltersDialog.tsx`) with a header-per-
 * section toggle instead. Order is the render order: the model's own
 * per-turn signal first, then dispatch-lifecycle/harness bookkeeping, then
 * mission/phase/step/hook scheduler activity, then fleet-machine activity,
 * then everything else. */
export type ActivitySectionTitle = "MODEL" | "DISPATCH" | "MISSION" | "MACHINE" | "OTHER";

const MODEL_SECTION_VALUES = new Set(["reasoning", "tool call", "checkpoint", "turn"]);
const DISPATCH_SECTION_VALUES = new Set([
  "dispatch start",
  "dispatch end",
  "dispatch error",
  "heartbeat",
  "detector",
  "runtime",
  "tokens",
  "lms",
  "compaction",
  "feedback",
  "routing",
  "session end",
]);
const MISSION_SECTION_VALUES = new Set(["note"]);
const MACHINE_SECTION_VALUES = new Set(["machine online", "machine offline", "host telemetry", "telemetry"]);

/** Which section a single activity value (a mapped `ACT_ORDER` label, or a
 * raw value `activityOf`'s fallback passed through unmapped) belongs under.
 * Mapped values are matched by exact membership first; a raw/absorbed value
 * that matches none of the curated sets falls through to the PREFIX rules
 * the operator specified (`dispatch.` for DISPATCH; `mission`/`phase`/
 * `step`/`hook.` for MISSION; `machine` for MACHINE), and anything left
 * over — including the literal `"other"` — lands in OTHER. Order matters:
 * a value is checked against each section's curated set/prefix in the same
 * MODEL → DISPATCH → MISSION → MACHINE → OTHER order the sections render
 * in, so no value can match two sections. */
export function activitySectionOf(value: string): ActivitySectionTitle {
  if (MODEL_SECTION_VALUES.has(value)) return "MODEL";
  if (DISPATCH_SECTION_VALUES.has(value) || value.startsWith("dispatch.")) return "DISPATCH";
  if (
    MISSION_SECTION_VALUES.has(value) ||
    value.startsWith("mission") ||
    value.startsWith("phase") ||
    value.startsWith("step") ||
    value.startsWith("hook.")
  ) {
    return "MISSION";
  }
  if (MACHINE_SECTION_VALUES.has(value) || value.startsWith("machine")) return "MACHINE";
  return "OTHER";
}

export interface ActivitySectionGroup {
  title: ActivitySectionTitle;
  values: string[];
}

const ACTIVITY_SECTION_ORDER: ActivitySectionTitle[] = ["MODEL", "DISPATCH", "MISSION", "MACHINE", "OTHER"];

/** Groups an ordered list of PRESENT activity values (`facets.act`, already
 * ordered by `computeFacets`) into the five sections above, preserving each
 * value's relative order within its section. A section with nothing present
 * is omitted entirely, not rendered empty (`FiltersDialog.tsx`'s `FiltersBody`
 * relies on this to skip rendering a header nobody can act on). */
export function groupActivitiesBySections(values: string[]): ActivitySectionGroup[] {
  const buckets: Record<ActivitySectionTitle, string[]> = {
    MODEL: [],
    DISPATCH: [],
    MISSION: [],
    MACHINE: [],
    OTHER: [],
  };
  for (const v of values) buckets[activitySectionOf(v)].push(v);
  return ACTIVITY_SECTION_ORDER.map((title) => ({ title, values: buckets[title] })).filter((g) => g.values.length > 0);
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

const FACET_KEYS = ["act", "cat", "tier", "src"] as const;

/** (#2416) The default filter state over a set of facets, absent any
 * operator override.
 *
 * `cat`/`tier`/`src` start fully included — nothing on those facets is
 * filtered out until the operator unchecks something. They are not the
 * problem the operator reported ("default all on is insane... I always
 * change it to model only, and have to uncheck heartbeat every time") and
 * this fix leaves them alone.
 *
 * `act` is different: it starts with ONLY `DEFAULT_ACTIVITIES` included.
 * Everything else the activity facet can offer — `heartbeat`, every
 * telemetry source, `note`, `routing`, `machine online`/`offline`,
 * `session end`, `dispatch start`/`end`, `compaction`, `feedback`, and any
 * unmapped `other` value — defaults OFF. `activeFilterCount` still counts
 * this as active filtering (see its own doc), so a busy stream with most of
 * its activity hidden never reads as a quiet system. */
export function defaultFilterState(facets: Facets): FilterState {
  // (silent-miss audit, 2026-09-06) Was `facets.act.filter((v) =>
  // DEFAULT_ACTIVITIES.has(v))` — a SECOND, independent re-derivation of
  // exactly what `isDefaultOn` computes, contradicting that function's own
  // doc comment ("`defaultFilterState` ... defer to it rather than
  // re-deriving it"). The two stayed silently in sync only because
  // `isDefaultOn("act", v)` happened to equal `DEFAULT_ACTIVITIES.has(v)`
  // exactly — the moment `isDefaultOn` grew the failure/abandonment suffix
  // check, this call site would have kept the stale, narrower rule with no
  // error anywhere. Routed through `isDefaultOn` for every facet (a no-op
  // change for `cat`/`tier`/`src`, which it already returns `true` for
  // unconditionally) so there is exactly one place this decision is made.
  return {
    act: new Set(facets.act.filter((v) => isDefaultOn("act", v))),
    cat: new Set(facets.cat.filter((v) => isDefaultOn("cat", v))),
    tier: new Set(facets.tier.filter((v) => isDefaultOn("tier", v))),
    src: new Set(facets.src.filter((v) => isDefaultOn("src", v))),
    q: "",
  };
}

/** (#2018, revised #2417 round 2) How many PRESENT values the operator's
 * filters are currently HIDING.
 *
 * The button used to read `filters` whether one value or seventeen were
 * hidden — a strict-subset-per-facet count, capped at one per facet
 * regardless of how many values that facet was hiding. A pane showing three
 * rows out of four hundred because sixteen activity values were unchecked
 * read identically to one showing 399 out of 400. That invisibility — not
 * durability — is the real hazard #1911 names when it refuses to persist a
 * selection: "a filter silently restored from last week is a wrong reading
 * that looks like a quiet system." A number that tracks actual hidden
 * values, not hidden facets, is what makes the button honest at either
 * scale.
 *
 * Each facet contributes `offered.length - selected.size` — the count of
 * values on offer that are NOT currently checked. A facet whose values have
 * not appeared yet contributes nothing, or every fresh page would open
 * claiming filters it never applied.
 *
 * The free-text query still counts as one, because it hides rows exactly the
 * same way a facet does and has no per-value count of its own.
 *
 * (#2416) `act`'s curated default means this is routinely NONZERO on a
 * fresh, busy pane — that is correct and intended. Hiding heartbeat/
 * telemetry/lifecycle noise by default is the whole point of the fix; the
 * count exists precisely so a pane that is hiding rows never looks like one
 * that isn't.
 */
export function activeFilterCount(state: FilterState, facets: Facets): number {
  let n = 0;
  for (const k of FACET_KEYS) {
    const offered = facets[k].length;
    const selected = state[k].size;
    if (offered > 0) n += Math.max(0, offered - selected);
  }
  if (state.q.trim() !== "") n += 1;
  return n;
}

/** The per-facet explicit choices an operator has recorded, as two sets: a
 * value they turned ON that would otherwise default off, and a value they
 * turned OFF that would otherwise default on. Absence from both means "no
 * opinion" — the value follows `isDefaultOn`. This is the shape both
 * `absorbNewFacetValues` (deciding a brand-new value) and `applyStoredPicks`
 * (restoring a facet from storage) resolve against. */
export interface FacetPicks {
  include: Set<string>;
  exclude: Set<string>;
}

export interface StoredPicks {
  act: FacetPicks;
  cat: FacetPicks;
  tier: FacetPicks;
  src: FacetPicks;
  q: string;
}

export function createStoredPicks(): StoredPicks {
  return {
    act: { include: new Set(), exclude: new Set() },
    cat: { include: new Set(), exclude: new Set() },
    tier: { include: new Set(), exclude: new Set() },
    src: { include: new Set(), exclude: new Set() },
    q: "",
  };
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
 * date-rollover reload). Mutates `seen` and returns a `filters` object
 * describing what a BRAND-NEW facet value (one `seen` has never recorded)
 * should default to, while an operator's explicit action on an
 * ALREADY-seen value sticks, because `seen` only tracks "processed once
 * ever", never "currently checked".
 *
 * (#2416) A brand-new value's default is no longer an unconditional "on".
 * For `cat`/`tier`/`src` it still is (unchanged — a new category, tier or
 * source is auto-included exactly like before). For `act`, a new value is
 * absorbed ON only if it is in `DEFAULT_ACTIVITIES`, OR the operator's
 * stored picks (`overrides`, optional — omitted callers get the plain
 * default) explicitly include it; an explicit stored EXCLUDE always wins,
 * even over `DEFAULT_ACTIVITIES` membership. This is the fix for the
 * reported bug: on an idle fleet `heartbeat` isn't offered yet, so it was
 * never in `seen`; the first dispatch introduces it as a "brand new" value,
 * and the old unconditional-on rule switched it on regardless of what the
 * operator had unchecked the run before. Now it stays off (or stays
 * excluded, if the operator had turned it on and then explicitly off again)
 * unless the operator's own stored picks say otherwise.
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
 * spurious re-render when nothing was actually new.
 *
 * (#2417 round 2) `overrides` is REQUIRED, not optional. An omitted-override
 * caller silently fell back to "every new value follows the plain default",
 * which is exactly the bug #2416 fixed for the real component — a caller
 * that forgets to thread the operator's stored picks through would compile
 * clean and regress silently. A test (or a future call site) that has no
 * picks of its own passes `createStoredPicks()` explicitly, which is the
 * empty/no-opinion case and behaves identically to the old default. */
export function absorbNewFacetValues(
  filters: FilterState,
  facets: Facets,
  seen: FacetSeen,
  overrides: StoredPicks,
): FilterState {
  let changed = false;
  const next: FilterState = { ...filters };
  for (const key of FACET_KEYS) {
    for (const v of facets[key]) {
      if (seen[key].has(v)) continue;
      seen[key].add(v);
      const excluded = overrides[key].exclude.has(v);
      const included = overrides[key].include.has(v);
      const on = excluded ? false : included || isDefaultOn(key, v);
      if (!on) continue;
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

/** (#2018, revised #2416) `sessionStorage` key for the operator's event-log
 * filter picks.
 *
 * The TIER is the whole decision, and it is `session`, not `local`. #1911
 * refuses `localStorage` for panel selections because "a filter silently
 * restored from last week is a wrong reading that looks like a quiet system"
 * — a correctness argument, and one this must not trade away. Session scope
 * keeps that guarantee (closing the tab clears it) while fixing the actual
 * complaint, which was that every REFRESH reset the picks.
 */
const FILTERS_STORAGE_KEY = "dmux.eventfilters";

/** (#2027, revised #2416, revised again #2417 round 2) There is exactly ONE
 * key, ONE global filter store — no per-scope forking.
 *
 * #2027's original fix scoped this key by the caller's own label (the
 * mission id, a step id) because two `EventLogColumn`s could be mounted at
 * once on the `mission` route and a routine live-poll tick on a hidden pane
 * was overwriting the visible pane's picks. That fix worked, but it also
 * meant `filtersKeyFor(undefined)` — the plain global key this constant
 * names — was NEVER actually written in production: `EventLogColumn` always
 * has a `scopeLabel` (App.tsx sets one for every route), so every real
 * write landed at a scope-specific key and the global default sat dead. The
 * practical effect: every view forked its own remembered picks instead of
 * "a pick applies everywhere", which is what an operator reasonably expects
 * from "I turned off heartbeat" — they don't expect to turn it off again on
 * the next tab.
 *
 * #2417 round 2 fixes the actual #2027 hazard a different way instead:
 * `persistFilterState` is now called ONLY from an operator gesture (a
 * checkbox toggle, "model only", a query edit, reset) — never from mount,
 * never from the passive `absorbNewFacetValues`/`applyStoredPicks` reconcile
 * effects that run on every `facets` change. A hidden pane that is never
 * touched by the operator never calls `setFilters` from a gesture, so it
 * never writes — there is nothing left for it to clobber. With that
 * guarantee in place, the fork is pure cost (silently dead global key, N
 * divergent per-view defaults) for no remaining benefit, so it's removed:
 * one key, one store, a pick sticks everywhere. */
function filtersKeyFor(): string {
  return FILTERS_STORAGE_KEY;
}

/** On-disk shape of `StoredPicks` — `version: 2` is the #2416 include/exclude
 * shape; anything else (missing, or the pre-#2416 flat `{act:[],cat:[],...}`
 * array shape) is treated as "nothing stored" rather than partially parsed —
 * lenient-on-read, per the schema-versioning contract: an old payload
 * restores as the plain default, never a throw. */
interface StoredPicksJSON {
  version: 2;
  act: { include: string[]; exclude: string[] };
  cat: { include: string[]; exclude: string[] };
  tier: { include: string[]; exclude: string[] };
  src: { include: string[]; exclude: string[] };
  q: string;
}

function readStoredPicksAt(storage: Pick<Storage, "getItem">, key: string): StoredPicks | null {
  let raw: string | null = null;
  try {
    raw = storage.getItem(key);
  } catch {
    return null;
  }
  if (!raw) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return null;
  }
  if (!isPlainObject(parsed)) return null;
  const p = parsed as Partial<StoredPicksJSON>;
  if (p.version !== 2) return null; // pre-#2416 payload (or unversioned garbage): no picks
  const out = createStoredPicks();
  for (const k of FACET_KEYS) {
    const facet = p[k];
    if (isPlainObject(facet)) {
      const inc = (facet as { include?: unknown }).include;
      const exc = (facet as { exclude?: unknown }).exclude;
      if (Array.isArray(inc)) out[k].include = new Set(inc.filter((v): v is string => typeof v === "string"));
      if (Array.isArray(exc)) out[k].exclude = new Set(exc.filter((v): v is string => typeof v === "string"));
    }
  }
  if (typeof p.q === "string") out.q = p.q;
  return out;
}

/** (#2027, revised #2416, revised again #2417 round 2 — one global key, no
 * scope) The stored picks, unreconciled against any particular window's
 * facets — for a caller that does not yet know what its facets are
 * (`EventLogColumn` mounts before its records query resolves), and for
 * `absorbNewFacetValues`, which needs the operator's explicit include/
 * exclude choices to decide a brand-new value rather than a fully-
 * reconciled `FilterState`.
 *
 * Returns null when the global key has no recognizable (`version: 2`)
 * payload, so a caller can distinguish "operator has no saved picks" from
 * "operator saved everything selected". */
export function storedFilterPicks(
  storage: Pick<Storage, "getItem"> | null = typeof window === "undefined" ? null : window.sessionStorage,
): StoredPicks | null {
  if (!storage) return null;
  return readStoredPicksAt(storage, filtersKeyFor());
}

/** Apply stored picks to freshly-arrived facets: a value the operator
 * explicitly excluded stays off, a value they explicitly included stays on,
 * and a value they never had an opinion about follows `isDefaultOn` — which
 * for `act` means `DEFAULT_ACTIVITIES` membership, and for every other
 * facet means fully selected.
 *
 * This is the piece `absorbNewFacetValues` cannot do on its own — that
 * function's job is "a value never offered before", which only fires once
 * per value per mount; this is the FULL reconciliation used both by
 * `restoreFilterState` and by `EventLogColumn`'s first-real-facets effect. */
export function applyStoredPicks(picks: StoredPicks, facets: Facets): FilterState {
  const out: FilterState = { act: new Set(), cat: new Set(), tier: new Set(), src: new Set(), q: picks.q };
  for (const k of FACET_KEYS) {
    for (const v of facets[k]) {
      if (picks[k].exclude.has(v)) continue;
      if (picks[k].include.has(v) || isDefaultOn(k, v)) out[k].add(v);
    }
  }
  return out;
}

/** Read persisted picks, reconciled against what this window offers.
 *
 * Returns the plain default when nothing is stored, when storage is
 * unavailable, or when the payload is unrecognizable. Storage access can
 * THROW outright (private mode, blocked site data), not merely return null —
 * `storedFilterPicks` (and the `readStoredPicksAt` it delegates to) already
 * wraps every access, so this just defers to it. */
export function restoreFilterState(
  facets: Facets,
  storage: Pick<Storage, "getItem"> | null = typeof window === "undefined" ? null : window.sessionStorage,
): FilterState {
  const picks = storedFilterPicks(storage);
  if (!picks) return defaultFilterState(facets);
  return applyStoredPicks(picks, facets);
}

/** Persist the current picks as explicit include/exclude OVERRIDES against
 * `isDefaultOn`, not as a flat "currently selected" snapshot — that's the
 * #2416 fix for the other half of the reported bug: a flat snapshot cannot
 * distinguish "the operator never had an opinion about this value" from
 * "the operator explicitly excluded it", so a value that temporarily
 * disappears from `facets` (and therefore from any snapshot taken while it's
 * gone) silently loses its exclusion the next time it reappears — which is
 * exactly the `heartbeat`-comes-back-every-run bug for any value outside
 * `DEFAULT_ACTIVITIES` too.
 *
 * The fix: read whatever is already stored at the (one, global) key,
 * recompute overrides only for values `facets` currently offers, and carry
 * every other previously-recorded override (for a value not currently
 * offered) forward unchanged. A value matching `isDefaultOn` is removed from
 * both lists (no override needed); a value differing from it is recorded on
 * the matching side.
 *
 * (#2417 round 2) The CALLER decides when this runs, and it must be an
 * operator gesture — a checkbox toggle, "model only", a query edit, reset —
 * never a mount-time seed and never the passive `absorbNewFacetValues`/
 * `applyStoredPicks` reconcile that runs on every `facets` change. Calling
 * this from an effect keyed on `filters` (the pre-round-2 shape) persisted
 * on EVERY state change regardless of source, which is what made the
 * per-scope fork load-bearing in the first place — a hidden pane's own
 * passive reconcile would write and clobber a sibling pane's picks. Gating
 * persistence to gestures only removes the need for that fork; see
 * `filtersKeyFor`'s doc.
 *
 * Silent on failure by design — a filter that could not be saved is a lost
 * convenience, never a reason to break the pane. */
export function persistFilterState(
  filters: FilterState,
  facets: Facets,
  storage: Pick<Storage, "getItem" | "setItem"> | null = typeof window === "undefined" ? null : window.sessionStorage,
): void {
  if (!storage) return;
  try {
    const key = filtersKeyFor();
    const previous = readStoredPicksAt(storage, key) ?? createStoredPicks();
    const next = createStoredPicks();
    next.q = filters.q;
    for (const k of FACET_KEYS) {
      const include = new Set(previous[k].include);
      const exclude = new Set(previous[k].exclude);
      for (const v of facets[k]) {
        include.delete(v);
        exclude.delete(v);
        const on = filters[k].has(v);
        if (on !== isDefaultOn(k, v)) {
          (on ? include : exclude).add(v);
        }
      }
      next[k] = { include, exclude };
    }
    const payload: StoredPicksJSON = {
      version: 2,
      act: { include: [...next.act.include], exclude: [...next.act.exclude] },
      cat: { include: [...next.cat.include], exclude: [...next.cat.exclude] },
      tier: { include: [...next.tier.include], exclude: [...next.tier.exclude] },
      src: { include: [...next.src.include], exclude: [...next.src.exclude] },
      q: next.q,
    };
    storage.setItem(key, JSON.stringify(payload));
  } catch {
    // ignore — storage unavailable or full
  }
}
