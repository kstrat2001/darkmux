import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import type { FlowRecord } from "../types/handwritten";
import { recKey } from "../lib/flow";
import { useThrottledValue } from "../hooks/useThrottledValue";
import { LIVE_WINDOW_MS } from "../lib/flow";
import { clk } from "../lib/format";
import { RecordView } from "./RecordView";
import {
  absorbNewFacetValues,
  activityOf,
  computeFacets,
  createFacetSeen,
  defaultFilterState,
  matchesFilters,
  type Facets,
  type FacetSeen,
  type FilterState, activeFilterCount, restoreFilterState, persistFilterState, storedFilterPicks, applyStoredPicks } from "../lib/eventFilters";
import { FiltersDialog, onlyModelFacet } from "./FiltersDialog";
import { ActivityIcon } from "./ActivityIcon";
import { recordDetail } from "../lib/recordDetail";
import { openModalEl } from "../lib/dialogManager";

/** Row cap — `renderLog()`'s `all.slice(-50).reverse()` (viewer.html:2443):
 * newest 50, newest-first. */
const LOG_CAP = 50;
/** (#2068) How long the followed record holds in the detail card before the
 * next one may replace it. Two updates a second is still "live"; faster is
 * unreadable on a phone and reads as flicker. */
const FOLLOW_HOLD_MS = 500;
function sameRecord(a: FlowRecord | null, b: FlowRecord | null): boolean {
  if (a === b) return true;
  if (!a || !b) return false;
  return recKey(a) === recKey(b);
}

/** The live window the counter reports, in hours — the status bar used to
 *  state this and no longer does (it belongs beside the records). */
const WINDOW_HOURS = Math.round(LIVE_WINDOW_MS / 3600000);

const MIN_DETAIL_PCT = 15;
const MAX_DETAIL_PCT = 70;
const DEFAULT_DETAIL_PCT = 38;

/** Enter/Space activates a `role="button"` `<div>` the same way a native
 * `<button>` would (matching `RunsBoard.tsx`'s own `onActivateKeyDown`) —
 * needed because `.eventlog__rec` is a click-only div with no other
 * keyboard path to selecting a record from the log. */
function onActivateKeyDown(onActivate: () => void) {
  return (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      onActivate();
    }
  };
}

/**
 * The event-log column (`.log`, viewer.html:829-849) — the per-record
 * stream, its search box + the full checkbox-per-facet filters modal, the
 * follow-latest toggle, the drag-to-resize `.split` handle, and the
 * `#detail` selected-event panel. Mounted by `App.tsx` for every route
 * EXCEPT `mission` (fleet / a session drill-in / a bare-date playback — see
 * `lib/route.ts`'s `showsEventLog` doc for the verified visibility rule,
 * which corrects a wrong packet-brief claim about `console`), toggled via
 * its `visible` prop rather than conditionally unmounted (see that prop's
 * own doc below).
 *
 * **Second call site (#1868):** `MissionGraphLens.tsx` mounts its OWN
 * instance of this SAME component for the `mission` route — the route
 * `showsEventLog` excludes precisely because the lens owns its own events
 * pane rather than sharing the App-level one (two mounted instances would
 * be two event logs disagreeing about scope; see that component's own
 * doc). `scopeLabel`/`records` are what make one component serve two call
 * sites: the App-level mount feeds it whatever `useRouteRecords` says the
 * ROUTE means, the mission lens feeds it its own mission-scoped fold.
 *
 * **`records` is whatever `useRouteRecords` says this ROUTE means** — the
 * rolling live 2-day window on live routes, and the FETCHED SLICE on
 * `session` (`/flow-session/<id>`) and `playback` (`/flow/<date>`).
 *
 * (#1800 P1) It used to be the live window on every route — named here as a
 * deliberate deferral, which is honest but meant a `#session=` route listed
 * unrelated live traffic beside a stage headed "session replay". Legacy never
 * had that: `boot()` re-scopes `RAW` to the fetched slice before rendering.
 * The fix needed no second pipeline, because this column already took
 * `records` as a prop — only the routing decision was missing.
 *
 * `loading` and `error` exist for the same reason: refusing to fall back to
 * live records on a failed fetch is only honest if the column can say WHY it
 * is empty. A dead daemon, a 500, a typo'd session id and a genuinely quiet
 * day must not render identically.
 *
 * **`#logscope`** moves here from its previous App-level standalone
 * `<span>` (see `App.tsx`'s own doc) — legacy nests it INSIDE `.loglist`'s
 * `<h3>` (`event log · <span id="logscope">`), and rendering it loose
 * above the stage regardless of lens produced the stray uppercase "FLEET"
 * the operator caught on `/next` (this packet's whole reason for existing).
 *
 * **`visible` — this component is ALWAYS MOUNTED, never conditionally
 * unmounted** (a correction mid-packet: an earlier version of this
 * component only rendered when `showsEventLog(route)` was true, which
 * seemed right — "hidden lens, no log column" — until `next-parity.spec.ts`
 * proved it wrong. Legacy's own `#logscope` ALSO exists, with REAL non-empty
 * text, on `runs`/`console`/`machine` — it's just visually hidden (an
 * ANCESTOR gets `display:none`, verified: a Playwright probe showed
 * `#logscope`'s `innerText` on the hidden `machine` lens still reads
 * "MacBook-Pro", because `innerText` on a non-rendered element falls back
 * to `textContent`, not empty string — same finding `showsEventLog`'s own
 * doc cites). `goldens/machine.txt`/`machine-deeplink.txt` capture that
 * REAL, non-empty text, and `next-parity.spec.ts` byte-compares `/next`
 * against those goldens — so unmounting this component on `machine` made
 * `#logscope` genuinely absent, which is MORE different from legacy than
 * the bug this packet exists to fix. The fix: mount unconditionally, and
 * hide the whole column via a CSS class (`eventlog--hidden`,
 * `display:none`) exactly mirroring legacy's own mechanism — `#logscope`
 * keeps real text, `innerText` still finds it (same non-rendered-element
 * fallback), and `#stage` still gets the full row width (a `display:none`
 * flex item doesn't participate in flex layout, so `.app-shell__stage`'s
 * `flex:1` expands on its own — no separate CSS class needed on the row).
 *
 * **The filter MODAL (#1640) — no longer a cut.** `#fbtn`/`data-act="filters"`
 * now opens the real checkbox-per-facet modal (`FiltersDialog.tsx`,
 * `#modalbg`) — matching legacy's own trigger exactly (viewer.html:840,
 * `data-act="filters"`) — rather than the narrower "model activity only"
 * boolean toggle this button stood in for previously (see git history for
 * that interim shape; superseded now that the shared dialog/focus machinery
 * exists to hold the real thing). The former glyph-vs-label mobile-UX note
 * that lived here no longer applies: the button's own visible label is now
 * literally "filters", matching its `title`/`aria-label`, so there is no
 * hover-only sentence left to disagree with a touch user's expectation.
 */
/** (#1066) Session-scoped collapse preference, keyed by MOUNT SITE.
 *
 * This was one global flag, which is wrong because the component has TWO real
 * mount sites: the App-level mainstay and the one `MissionGraphLens` owns.
 * An operator who collapsed the mainstay on any route then opened a mission
 * found the mission's OWN events pane collapsed too — a pane they had never
 * touched, showing a 28px rail with no explanation.
 *
 * That directly contradicted this feature's own framing ("the operator
 * decided and can undo it"), since here the operator decided nothing about
 * that pane. Found by a QA agent that navigated between routes rather than
 * toggling one instance.
 *
 * Keyed by MOUNT SITE (`paneId`), deliberately NOT by `scopeLabel`. The
 * App-level pane's scope label changes with the route — "fleet", "runs",
 * "console" — so keying on it would reset the choice on every tab switch,
 * destroying the one property that makes this a MAINSTAY rather than a
 * per-page toggle. The operator's ask was explicitly "a collapsible mainstay
 * on all tabs".
 *
 * The collision this fixes is between the two SIMULTANEOUS mounts, not
 * between routes: one `paneId` for the App-level chrome, another for the pane
 * `MissionGraphLens` owns. `dmux.` prefix per `lenses/mission/timeline.ts`. */
function collapseKeyFor(pane: string): string {
  return `dmux.eventlog.collapsed.${pane}`;
}

export function EventLogColumn({
  records,
  visible,
  scopeLabel,
  loading = false,
  error = null,
  historical = false,
  serverTruncated = false,
  paneId = "app",
}: {
  records: FlowRecord[];
  visible: boolean;
  /** (#1066 QA) Which MOUNT SITE this is — `"app"` for the App-level
   *  mainstay, something else for a lens that owns its own pane. Keys the
   *  collapse preference, so two simultaneously-mounted panes cannot
   *  overwrite each other's choice. Deliberately separate from `scopeLabel`,
   *  which changes per route and would reset the choice on every tab switch. */
  paneId?: string;
  /** (#1800 P2) True when `records` is a fetched historical slice rather than
   *  the live rolling window — see the header's own note. */
  historical?: boolean;
  /** (#1800 P1) A historical slice still in flight. Without this, "loading"
   *  and "genuinely empty" both render "no events yet". */
  loading?: boolean;
  /** (#1800 P1) Why the slice is empty, when it failed. A dead daemon and a
   *  quiet day must not render identically — see `useRouteRecords`. */
  error?: { status: number | null; message: string } | null;
  /** Not shown — written into a hidden span purely so this port's parity
   *  extraction matches legacy's. See App's `routeChrome` note. */
  scopeLabel: string;
  /** (#1868) True when `records` is itself a slice of a SERVER-capped
   *  response (`/flow-mission/:id`'s own `truncated` flag, capped at
   *  `MAX_CATALOG_RECORDS`) — a cap this component's own `LOG_CAP`
   *  disclosure can't see, because the server already dropped the rest
   *  before `records` ever arrives here. The pre-#1868 standalone
   *  `mission-graph.html` page's own `eventsPanelEls` appended the same "+"
   *  to its total for the same
   *  reason: "50 of 10000" would otherwise restate the server cap as the
   *  mission's whole history. Rendered only where this component ALREADY
   *  reports a real total (not the search-match count, which is a
   *  different metric). */
  serverTruncated?: boolean;
}) {
  // The full facet-filter model (activity/category/tier/telemetry-source +
  // free-text search) — `FiltersDialog` renders the checkbox grid for it,
  // this column applies it to `records`. Facets are DERIVED from `records`
  // every render (`computeFacets`, matching legacy's own `recompute()`).
  //
  // `filters` starts as `defaultFilterState` over WHATEVER facets exist at
  // mount — usually none, since `records` is TanStack Query data and this
  // component mounts before the fetch resolves. That is fine PROVIDED every
  // facet value that shows up afterward gets absorbed into the active
  // filter Sets, which is exactly `absorbNewFacetValues`'s job (viewer.html's
  // `absorbNewFilterValues()` + its `SEEN` ledger — see `eventFilters.ts`'s
  // own doc for the full mechanism and why a ONE-TIME reseed-at-first-
  // population isn't enough on its own: a live-streamed record can
  // introduce a brand-new activity/category/tier/source value at ANY point
  // after the first population too, and that value needs absorbing just as
  // much as the initial batch does). Caught two ways in
  // `tests/parity/next-parity-live.spec.ts`'s SSE record-count assertion: a
  // first draft (seed-once-at-mount) got stuck at a permanent 0 because
  // `records` starts empty; a second draft (reseed once, the first time
  // `facets` has any content) fixed the baseline count but then silently
  // dropped the live-streamed test record itself, because ITS activity
  // (`"note"`) was a facet value that had never been seen before and the
  // one-shot reseed had already run.
  const facets = useMemo(() => computeFacets(records), [records]);

  // (#2018) Seeded from `sessionStorage`, reconciled against the facets this
  // window actually offers. The lazy initializer runs ONCE, which is what
  // makes this a restore rather than a fight with `absorbNewFacetValues`
  // below — that effect owns what happens as new facet values arrive, and it
  // would undo a value re-applied on every render.
  const [filters, setFilters] = useState<FilterState>(() => restoreFilterState(facets));
  // (#2018) Derived, never stored: a count held in state would be one more
  // thing to keep in step with the filters it describes, and would go stale
  // exactly when new facet values arrive.
  const activeFilters = useMemo(() => activeFilterCount(filters, facets), [filters, facets]);

  // Persist on change rather than on close: the operator's picks should
  // survive a refresh taken at any moment, not only one taken after tidily
  // dismissing the dialog.
  useEffect(() => {
    persistFilterState(filters, undefined, scopeLabel);
  }, [filters]);

  const seenRef = useRef<FacetSeen>(createFacetSeen());
  // The absorb runs in the EFFECT BODY, not inside the `setFilters` updater.
  // `absorbNewFacetValues` mutates the `seen` ledger, and React requires
  // updaters to be pure — `<StrictMode>` (on, `main.tsx`) double-invokes them
  // precisely to punish impurity. On React 18.3 the impure version happened
  // to behave, but the failure it risks is the exact bug the ledger exists to
  // prevent: the second invocation sees every value already marked seen and
  // returns the stale filters, silently dropping a live-streamed record whose
  // facet value is brand new. Reading `filters` here costs a dependency on it,
  // and the identity guard below keeps that from looping.
  // (#2027) The operator's STORED picks, held until there are facets to apply
  // them to. `restoreFilterState` in the initializer above cannot do this: the
  // component mounts before its records query resolves, so facets are empty,
  // every intersection is empty, and the picks were silently discarded on
  // every load. The feature looked implemented and did nothing.
  const pendingPicksRef = useRef(storedFilterPicks(undefined, scopeLabel));

  useEffect(() => {
    // First arrival of real facets: apply the stored picks INSTEAD of
    // absorbing. Order matters — `absorbNewFacetValues` selects any value it
    // has not seen before, which is right for live traffic and exactly wrong
    // for a value the operator deliberately deselected last session. Applying
    // first, then letting the ledger mark everything seen, keeps both correct.
    const pending = pendingPicksRef.current;
    if (pending && (facets.act.length || facets.cat.length || facets.tier.length || facets.src.length)) {
      pendingPicksRef.current = null;
      const restored = applyStoredPicks(pending, facets);
      for (const k of ["act", "cat", "tier", "src"] as const) {
        for (const v of facets[k]) seenRef.current[k].add(v);
      }
      setFilters(restored);
      return;
    }
    const next = absorbNewFacetValues(filters, facets, seenRef.current);
    if (next !== filters) setFilters(next);
  }, [facets, filters]);
  const [follow, setFollow] = useState(true);
  // (#1066) Collapsed is the operator's choice and must survive a route
  // change, or every tab switch reopens it and a "mainstay" becomes an
  // irritation. Session scope, matching the filters beside it (#2018): a
  // preference worth keeping across a refresh, not worth resurrecting a week
  // later in a context that no longer resembles the one it was set in.
  const [collapsed, setCollapsed] = useState<boolean>(() => {
    try {
      return window.sessionStorage.getItem(collapseKeyFor(paneId)) === "1";
    } catch {
      return false;
    }
  });
  const toggleCollapsed = () => {
    setCollapsed((c) => {
      try {
        window.sessionStorage.setItem(collapseKeyFor(paneId), c ? "0" : "1");
      } catch {
        // ignore — storage unavailable
      }
      return !c;
    });
  };

  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const [detailPct, setDetailPct] = useState(DEFAULT_DETAIL_PCT);

  const columnRef = useRef<HTMLDivElement | null>(null);
  const dragRef = useRef<{ startY: number; startPct: number } | null>(null);

  const filtered = useMemo(() => records.filter((r) => matchesFilters(r, filters)), [records, filters]);

  function setQuery(q: string) {
    setFilters((f) => ({ ...f, q }));
  }
  function toggleFacet(key: keyof Facets, value: string) {
    setFilters((f) => {
      const next = new Set(f[key]);
      if (next.has(value)) next.delete(value);
      else next.add(value);
      return { ...f, [key]: next };
    });
  }
  function onlyModel() {
    setFilters((f) => ({ ...f, act: onlyModelFacet(facets) }));
  }
  function clearAllFilters() {
    setFilters(defaultFilterState(facets));
  }

  const capped = filtered.length > LOG_CAP;
  const visibleRecs = useMemo(() => filtered.slice(-LOG_CAP).reverse(), [filtered]);

  // (#2068) The followed record is throttled: at playback speed the newest
  // record changed several times a second and the detail card swapped its
  // whole body each time, which on a phone reads as the pane "flickering
  // around". The list below still shows every record as it lands; only the
  // card holds still long enough to be read. Compared by `recKey`, since
  // `visibleRecs` is a fresh array per render around the same records.
  const newest = visibleRecs[0] ?? null;
  const followed = useThrottledValue(newest, FOLLOW_HOLD_MS, sameRecord);
  const selected = useMemo(() => {
    if (follow) return followed;
    return visibleRecs.find((r) => recKey(r) === selectedKey) ?? null;
  }, [follow, followed, visibleRecs, selectedKey]);

  function selectRecord(r: FlowRecord) {
    setSelectedKey(recKey(r));
    setFollow(false);
  }

  function toggleFollow() {
    setFollow((f) => {
      const next = !f;
      if (next) setSelectedKey(null);
      return next;
    });
  }

  // `.split` drag-to-resize — pointer events (not mouse-only) so it works
  // on the phone-width layout too, even though `.eventlog__split` is
  // display:none there (mirrors legacy's own "the row-resize handle isn't
  // useful when stacked" call, viewer.html:679).
  function onSplitPointerDown(e: React.PointerEvent<HTMLDivElement>) {
    dragRef.current = { startY: e.clientY, startPct: detailPct };
    e.currentTarget.setPointerCapture(e.pointerId);
  }
  function onSplitPointerMove(e: React.PointerEvent<HTMLDivElement>) {
    const drag = dragRef.current;
    const col = columnRef.current;
    if (!drag || !col) return;
    const totalH = col.getBoundingClientRect().height || 1;
    const deltaPct = ((e.clientY - drag.startY) / totalH) * 100;
    const next = Math.min(MAX_DETAIL_PCT, Math.max(MIN_DETAIL_PCT, drag.startPct + deltaPct));
    setDetailPct(next);
  }
  function onSplitPointerUp(e: React.PointerEvent<HTMLDivElement>) {
    dragRef.current = null;
    e.currentTarget.releasePointerCapture(e.pointerId);
  }

  const q = filters.q.length > 0;
  const qcountText = q
    ? filtered.length
      ? // (#1891) `serverTruncated` applies here too, and for the same
        // reason the no-search branch below appends it to its own total:
        // `filtered` is computed over `records`, which is itself a slice of
        // a server-capped response when `serverTruncated` is true. A search
        // over that slice can only ever report matches found WITHIN the
        // slice — matches sitting in the part the server already dropped
        // are invisible to it. "134 matches" reads as a complete count;
        // it isn't one. The "+" lands right after the number, same
        // placement as the no-search branch, so the two chips read as one
        // consistent convention rather than two different disclosures.
        `${filtered.length}${serverTruncated ? "+" : ""} match${filtered.length === 1 ? "" : "es"}${capped ? ` · ${LOG_CAP} shown` : ""}`
      : "no match"
    // (operator) "newest 50 of 734" -> "50 of 734". The word carried no
    // information the newest-first ordering does not already show, and this
    // chip sits beside a live stream where every extra word is noise.
    //
    // The CAP itself is legacy's (`all.slice(-50)`, viewer.html:2443) — what
    // this port adds is SAYING so. Legacy hides 684 records in silence; the
    // label is the honest half and worth keeping.
    //
    // `serverTruncated` appends "+" to that total exactly where legacy's own
    // `eventsPanelEls` does — the total this component reports is itself a
    // slice of a server-capped response, so "of 734" would understate the
    // real count without it.
    : capped
      ? `${LOG_CAP} of ${filtered.length}${serverTruncated ? "+" : ""} events`
      : `${filtered.length}${serverTruncated ? "+" : ""} events`;

  return (
    <div
      className={`eventlog${visible ? "" : " eventlog--hidden"}${collapsed ? " eventlog--collapsed" : ""}`}
      ref={columnRef}
    >
      {/* (#1066) The rail is the whole reason "collapsed" differs from
          "hidden". `visible=false` is `display:none` with nothing left to
          click — a route decides for the operator. Collapsed leaves a
          control, so the operator decides and can undo it. The button is
          rendered in BOTH states, never swapped for a different element, so
          keyboard focus survives the toggle instead of being dropped on the
          floor when its host unmounts. */}
      <button
        type="button"
        className="eventlog__collapse"
        data-act="togglelog"
        aria-expanded={!collapsed}
        aria-label={collapsed ? "expand the event log" : "collapse the event log"}
        title={collapsed ? "expand the event log" : "collapse the event log"}
        onClick={toggleCollapsed}
      >
        {collapsed ? "\u2039" : "\u203a"}
      </button>
      {/* (#2068) `following` lets the narrow-viewport stylesheet pin this
          pane's height while it tracks the newest record: content-sized, it
          re-heighted with every followed record's payload and moved the list
          under it on each event. A hand-picked record keeps the content-sized
          pane, since nothing is streaming into it then. */}
      <div className={`eventlog__detail${follow ? " following" : ""}`} id="detail" style={{ flexBasis: `${detailPct}%` }}>
        {/* (operator) No "selected event" title. It was static chrome
            competing with the record's own headline — `RecordView` already
            leads with the action in accent colour, so the label was a second
            heading fighting the real one, and one more thing to read before
            reaching the content. The empty-state line below still explains
            the panel when nothing is selected, which is the only moment a
            title would have earned its place. Free to remove: this panel sits
            outside every extracted golden region. */}
        <div id="detailbody" className="eventlog__detailbody">
          {selected ? <EventDetail record={selected} /> : <div className="eventlog__none">select an event from the log to inspect it</div>}
        </div>
      </div>
      <div
        className="eventlog__split"
        id="split"
        title="drag to resize"
        onPointerDown={onSplitPointerDown}
        onPointerMove={onSplitPointerMove}
        onPointerUp={onSplitPointerUp}
      />
      <div className="eventlog__list">
        <div className="eventlog__head">
          <h3>
            {/* (operator) The header names the WINDOW; the outer UI owns
                context. `#logscope` repeated what the active tab or the crumb
                had already established in six of its eight states — and in
                two of those it was VAGUER than the crumb beside it ("mission"
                against `◆ <mission id>"). Kept in the DOM, empty and hidden,
                so legacy's extraction and this port's agree; the element
                itself is legacy's and dies with it at the flip. */}
            {/* The window suffix is LIVE-ONLY. On a `session`/`playback`
                route `records` is a fetched historical slice, so "last 24h"
                named a window these records were never selected by — the
                same overclaim the savings hero's eyebrow carried until
                #1800 P2. No replacement string is invented here: the route's
                own chrome already says which day or session this is. */}
            <span>
              events{historical ? "" : ` last ${WINDOW_HOURS}h`}
              <span id="logscope" hidden>
                {scopeLabel}
              </span>
            </span>
            <span className="eventlog__headbtns">
              <button
                type="button"
                className={`eventlog__follow${follow ? " on" : ""}`}
                id="follow"
                title={follow ? "following the latest events" : "click to follow latest"}
                aria-label={follow ? "following the latest events" : "click to follow latest"}
                onClick={toggleFollow}
              >
                ⏱
              </button>
              {/* (#2018) The count is the SAFETY property, not decoration.
                  Without it a pane showing three rows out of four hundred is
                  indistinguishable from a quiet system — which is the hazard
                  #1911 names when it refuses to persist a selection ("a
                  filter silently restored ... looks like a quiet system").
                  The danger there is silence, not durability, and silence is
                  already true today with nothing persisted. `aria-label`
                  carries the count too, so the state is not colour- or
                  glyph-only. */}
              <button
                type="button"
                className="eventlog__fbtn"
                id="fbtn"
                data-act="filters"
                data-active={activeFilters > 0 ? "1" : undefined}
                title={activeFilters > 0 ? `${activeFilters} filter${activeFilters === 1 ? "" : "s"} hiding events` : "filters"}
                aria-label={activeFilters > 0 ? `filters, ${activeFilters} active` : "filters"}
                onClick={() => openModalEl("modalbg")}
              >
                filters
                {activeFilters > 0 ? <span className="eventlog__fcount"> · {activeFilters}</span> : null}
              </button>
            </span>
          </h3>
          <div className="eventlog__search">
            <div className="eventlog__searchbox">
              <span className="eventlog__sicon" aria-hidden="true">
                ⌕
              </span>
              <input
                id="logq"
                type="search"
                placeholder="filter events…"
                autoComplete="off"
                spellCheck={false}
                aria-label="search events"
                value={filters.q}
                onChange={(e) => setQuery(e.target.value)}
              />
              {filters.q ? (
                <button
                  className="eventlog__sclear"
                  id="sclear"
                  type="button"
                  aria-label="clear search"
                  title="clear search"
                  onClick={() => setQuery("")}
                >
                  ✕
                </button>
              ) : null}
            </div>
            <span className={`eventlog__qcount${qcountText ? " show" : ""}${q && filtered.length === 0 ? " zero" : ""}`} id="qcount" aria-live="polite">
              {qcountText}
            </span>
          </div>
        </div>
        <div id="logbody" className="eventlog__body">
          {visibleRecs.length ? (
            visibleRecs.map((r) => {
              const key = recKey(r);
              const detail = recordDetail(r);
              return (
                <div
                  key={key}
                  className={`eventlog__rec${selected && recKey(selected) === key ? " sel" : ""}`}
                  data-act="rec"
                  role="button"
                  tabIndex={0}
                  // (#1868) The row's own `handle` — hover provenance, same
                  // convention `.smodel`/`.mn-label` elsewhere in this app
                  // use for a value that's meaningful but too long/noisy for
                  // the row's always-visible text. Doubles as the mission
                  // lens's own parity-extraction hook
                  // (`tests/parity/lib/extract-graph.js`'s port-side events
                  // extractor reads it) — a real, minimal, independently
                  // justified addition to this shared component, not a
                  // fork of it.
                  title={r.handle || undefined}
                  onClick={() => selectRecord(r)}
                  onKeyDown={onActivateKeyDown(() => selectRecord(r))}
                >
                  <span className="eventlog__rectime">{clk(Date.parse(r.ts))}</span>{" "}
                  <ActivityIcon act={activityOf(r)} />
                  <span className="eventlog__ractivity">{activityOf(r)}</span>
                  {r.machine_id ? <span className="eventlog__recmachine"> · {r.machine_id}</span> : null}
                  {r.session_id ? <span className="eventlog__recsession"> · {r.session_id}</span> : null}
                  {/* What the record DID — a tool call's name + arguments +
                      result size, a turn's finish reason, a reasoning
                      excerpt. Without it every tool call in the log read
                      "tool call" and nothing else. */}
                  {detail ? <span className="preview-text"> · {detail}</span> : null}
                </div>
              );
            })
          ) : (
            <div className="eventlog__empty" data-state={error ? "error" : loading ? "loading" : "empty"} role={error ? "alert" : undefined}>
              {error
                ? `couldn't load events${error.status !== null ? ` (HTTP ${error.status})` : ""}: ${error.message}`
                : loading
                  ? "loading…"
                  : // (broadened from "no events match your search") A
                    // filtered-to-empty result can now come from an
                    // unchecked facet, not just the text search — the two
                    // are indistinguishable from here, so the message
                    // covers both honestly rather than naming only search.
                    records.length > 0
                    ? "no events match your filters"
                    : "no events yet"}
            </div>
          )}
        </div>
      </div>
      <FiltersDialog
        facets={facets}
        filters={filters}
        onToggle={toggleFacet}
        onSetQuery={setQuery}
        onOnlyModel={onlyModel}
        onClearAll={clearAllFilters}
      />
    </div>
  );
}

/** `renderDetail()`'s fallback branch (viewer.html:2417-2418,
 * `` `<pre>${pretty(r)}</pre>` ``) — this column doesn't reproduce the
 * structured per-action cards legacy builds for reasoning/tool/compaction/
 * tier-decision records (viewer.html:2370-2415); every selected record
 * renders through this ONE pretty-printed-JSON view instead. A real,
 * working detail pane (the operator can inspect any field of any selected
 * event) — just not the bespoke per-action layout, named as a follow-up
 * rather than partially reproducing four separate card shapes. */
function EventDetail({ record }: { record: FlowRecord }) {
  return (
    <div className="eventlog__detailcard">
      <RecordView record={record as unknown as Record<string, unknown>} />
    </div>
  );
}
